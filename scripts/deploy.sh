#!/bin/sh
# =============================================================================
# Ferroma — one-command deployment onto a host that already runs PostgreSQL
# =============================================================================
# Target: a Linux server with Docker (Engine 24+, Compose v2), an existing
# PostgreSQL 14+ and an existing reverse proxy that terminates HTTPS.
#
#   ./scripts/deploy.sh                 # first deployment, or re-apply config
#   ./scripts/deploy.sh status          # containers, health, database
#   ./scripts/deploy.sh upgrade         # rebuild the image and restart
#   ./scripts/deploy.sh dkim            # DKIM key + the TXT record to publish
#   ./scripts/deploy.sh doctor          # ferroma doctor, inside the container
#   ./scripts/deploy.sh down [--volumes]
#
# Backups are not part of this deployment: back up PostgreSQL and the
# ferroma-data volume with the host's own tooling. The two halves belong
# together, and docs/deployment.md §8 has the commands and what they must
# contain.
#
# Everything it writes lands in `.env`, which is the only configuration this
# deployment has. It never edits your PostgreSQL server's configuration: it
# connects over 127.0.0.1, which every distribution already allows.
#
# Non-interactive use (cloud-init, CI) — every prompt has a flag:
#   ./scripts/deploy.sh --yes --domain example.com --admin me@example.com \
#                       --db-password "$DB_PW" --tls-cert … --tls-key …
#
# Requires nothing but docker and a POSIX shell. `psql` and `openssl` are used
# when present and worked around when not.
# =============================================================================
set -eu

# -----------------------------------------------------------------------------
# Locations and defaults
# -----------------------------------------------------------------------------
ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
COMPOSE_FILE="docker-compose.external-db.yml"
ENV_FILE="$ROOT_DIR/.env"

DEFAULT_API_HOST="127.0.0.1"
DEFAULT_API_PORT="18080"
# The HTTPS port the reverse proxy serves the mail hostname on. 443 is the
# default every client assumes; anything else has to be spelled out in URLs.
DEFAULT_PUBLIC_PORT="443"
DEFAULT_DB_HOST="127.0.0.1"
DEFAULT_DB_PORT="5432"
DEFAULT_DB_USER="ferroma"
DEFAULT_DB_NAME="ferroma"
DEFAULT_IMAGE="ferroma:latest"
DEFAULT_PG_IMAGE="postgres:16-alpine"

# The container name is fixed by the compose file, and both `status` and the
# health check wait on it.
APP_CONTAINER="ferroma"

cd "$ROOT_DIR"

TMP_FILES=""
cleanup() {
    for _f in $TMP_FILES; do rm -f "$_f" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

# -----------------------------------------------------------------------------
# Output
# -----------------------------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RESET=$(printf '\033[0m')
    C_BOLD=$(printf '\033[1m')
    C_GREEN=$(printf '\033[32m')
    C_YELLOW=$(printf '\033[33m')
    C_RED=$(printf '\033[31m')
else
    C_RESET=''; C_BOLD=''; C_GREEN=''; C_YELLOW=''; C_RED=''
fi

step() { printf '%s==>%s %s%s%s\n' "$C_GREEN" "$C_RESET" "$C_BOLD" "$*" "$C_RESET"; }
info() { printf '    %s\n' "$*"; }
raw()  { printf '%s\n' "$*"; }
warn() { printf '%s[warn]%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die()  { printf '%s[error]%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# -----------------------------------------------------------------------------
# Prompts, secrets, .env, docker
# -----------------------------------------------------------------------------
# Read a value from the terminal, falling back to the default when there is no
# terminal (a piped or cron invocation) so the script stays usable unattended.
ask() {
    _prompt="$1"
    _default="${2:-}"
    if [ ! -t 0 ]; then
        printf '%s\n' "$_default"
        return 0
    fi
    if [ -n "$_default" ]; then
        printf '%s %s[%s]%s: ' "$_prompt" "$C_BOLD" "$_default" "$C_RESET" >&2
    else
        printf '%s: ' "$_prompt" >&2
    fi
    IFS= read -r _answer || _answer=""
    printf '%s\n' "${_answer:-$_default}"
}

confirm() {
    [ "$ASSUME_YES" = 1 ] && return 0
    [ -t 0 ] || return 1
    printf '%s [y/N]: ' "$1" >&2
    IFS= read -r _answer || _answer=""
    case "$_answer" in
        y|Y|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

rand_hex() {
    if have openssl; then
        openssl rand -hex "$1"
    else
        head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
    fi
}

# 24 alphanumerics: strong enough to be generated rather than chosen, short
# enough to be typed into a mail client.
rand_password() {
    if have openssl; then
        openssl rand -base64 32 | tr -d '/+=\n' | cut -c1-24
    else
        rand_hex 16 | cut -c1-24
    fi
}

env_get() {
    [ -f "$ENV_FILE" ] || return 0
    sed -n "s/^$1=//p" "$ENV_FILE" | tail -n 1
}

env_set() {
    _key="$1"
    _value="$2"
    # `|` delimits the substitution; `&` and `\` in the value must not be read as
    # sed metacharacters (a password may contain either).
    _escaped=$(printf '%s' "$_value" | sed 's/[&|\\]/\\&/g')
    _tmp="${ENV_FILE}.tmp.$$"
    TMP_FILES="$TMP_FILES $_tmp"
    if [ -f "$ENV_FILE" ] && grep -q "^${_key}=" "$ENV_FILE"; then
        sed "s|^${_key}=.*|${_key}=${_escaped}|" "$ENV_FILE" > "$_tmp"
    else
        if [ -f "$ENV_FILE" ]; then cat "$ENV_FILE" > "$_tmp"; else : > "$_tmp"; fi
        printf '%s=%s\n' "$_key" "$_escaped" >> "$_tmp"
    fi
    chmod 600 "$_tmp"
    mv "$_tmp" "$ENV_FILE"
}

env_del() {
    [ -f "$ENV_FILE" ] || return 0
    _tmp="${ENV_FILE}.tmp.$$"
    TMP_FILES="$TMP_FILES $_tmp"
    grep -v "^$1=" "$ENV_FILE" > "$_tmp" || true
    chmod 600 "$_tmp"
    mv "$_tmp" "$ENV_FILE"
}

compose() { docker compose -f "$COMPOSE_FILE" "$@"; }

image_exists() { docker image inspect "$1" >/dev/null 2>&1; }
container_running() {
    [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || echo false)" = "true" ]
}

listening_on() {
    if have ss; then
        ss -ltn 2>/dev/null | tail -n +2 | awk '{print $4}' | grep -Eq "[:.]$1$"
    elif have netstat; then
        netstat -ltn 2>/dev/null | tail -n +2 | awk '{print $4}' | grep -Eq "[:.]$1$"
    else
        return 1
    fi
}

# The host's own address on the default Docker bridge: the one place a container
# can reach a service that listens on the host, and what a containerised reverse
# proxy has to be pointed at (`host-gateway` resolves here too).
docker_bridge_gateway() {
    _gw=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null || true)
    case "$_gw" in
        ''|'<no value>') printf '172.17.0.1' ;;
        *) printf '%s' "$_gw" ;;
    esac
}

# Can a TCP connection be opened at all? A *filtered* port — a firewall dropping
# packets, or a public hostname that points back at this machine where the NAT
# refuses to hairpin — makes libpq wait for the kernel's SYN timeout of a couple
# of minutes. That is what "it hangs at Checking PostgreSQL" looks like; this
# answers in three seconds so the script can say something useful instead.
port_reachable() {
    if have timeout && have bash; then
        timeout 3 bash -c "exec 3<>/dev/tcp/$1/$2" >/dev/null 2>&1 && return 0
        return 1
    fi
    if have nc; then
        nc -w 3 "$1" "$2" </dev/null >/dev/null 2>&1 && return 0
        return 1
    fi
    return 0
}

# Is the configured database this machine? A public hostname that resolves to one
# of our own addresses counts: 1Panel and similar panels run PostgreSQL in a
# container and hand out the panel's hostname.
db_host_is_local() {
    case "$DB_HOST" in
        127.0.0.1|localhost|::1) return 0 ;;
    esac
    _addr=$(getent hosts "$DB_HOST" 2>/dev/null | awk '{print $1}' | head -n 1)
    [ -n "$_addr" ] || return 1
    for _ip in $(hostname -I 2>/dev/null); do
        [ "$_ip" = "$_addr" ] && return 0
    done
    return 1
}

# Running containers that look like a PostgreSQL *server* — where peer/trust auth
# already works, and where the host has no `postgres` system user to become.
local_postgres_containers() {
    docker ps --format '{{.Names}}\t{{.Image}}' 2>/dev/null \
        | grep -i 'postgres' \
        | grep -iv '^ferroma' \
        | awk '{print $1}'
}

# -----------------------------------------------------------------------------
# Usage
# -----------------------------------------------------------------------------
usage() {
    cat <<'USAGE'
Ferroma deployment — for a host with an existing PostgreSQL and an existing proxy.

  scripts/deploy.sh [command] [options]

Commands
  deploy     first deployment, or re-apply .env and restart (default)
  status     containers, health and database status
  logs       follow the Ferroma log
  upgrade    rebuild the image, restart, wait for healthy
  dkim       generate a DKIM key and print the TXT record; --enable signs with it
  certs      install the current certificate and reload the mail listeners; this is
             what a certbot deploy hook calls after a renewal
  doctor     run `ferroma doctor` inside the container
  down       stop the stack (--volumes also deletes mail and users)
  help       this text

Backups
  None ship with this deployment. Back up PostgreSQL and the ferroma-data
  volume with whatever the host already uses; docs/deployment.md §8 lists what
  a backup must contain and why the two halves belong together.

Options
  --wizard               ask only for the web port and let the first-run wizard collect
                         the mail domain, hostname, administrator and TLS
  --domain DOMAIN        mail domain, e.g. example.com
  --hostname FQDN        MX hostname, e.g. mail.example.com  (default mail.DOMAIN)
  --admin EMAIL          first administrator address        (default admin@DOMAIN)
  --admin-password PW    its password; generated when omitted
  --api-host ADDR        where the HTTP API listens         (default 127.0.0.1)
  --api-port PORT        the port your reverse proxy proxies to (default 18080)
  --public-port PORT     the HTTPS port the proxy listens on, when it is not 443.
                         It goes into FERROMA_PUBLIC_URL, the links in notification
                         mail, and the URLs printed at the end. Note that a port
                         other than 443 costs two things, because both are defined
                         to live on 443: client autodiscovery
                         (/.well-known/ferroma) and MTA-STS.
  --db-host HOST         PostgreSQL host                    (default 127.0.0.1)
  --db-port PORT         PostgreSQL port                    (default 5432)
  --db-user USER         role Ferroma connects as           (default ferroma)
  --db-name NAME         database                           (default ferroma)
  --db-password PW       its password; generated when omitted
  --pg-superuser USER    superuser that creates role and database (default postgres)
  --pg-password PW       that superuser's password, when password auth is used
  --image REF            image tag: built here unless it names a registry
  --tls-cert PATH        certificate bundle (leaf first, then intermediates)
  --tls-key PATH         its private key
  --no-tls               deploy without TLS: mail clients cannot authenticate
  --enable               with `dkim`: turn signing on after publishing the record
  --rebuild              rebuild the image even when it is already present
  --volumes              with `down`: also delete mail and users
  -y, --yes              assume yes: never prompt, never confirm
  -h, --help             this text
USAGE
}

# -----------------------------------------------------------------------------
# Argument parsing
# -----------------------------------------------------------------------------
COMMAND=""
# Set by --wizard: this run publishes the web port and nothing else, and the mail identity
# is collected by the first-run wizard instead of here.
WIZARD_MODE=0
DOMAIN_ARG=""
HOSTNAME_ARG=""
ADMIN_ARG=""
ADMIN_PASSWORD=""
API_HOST_ARG=""
API_PORT_ARG=""
PUBLIC_PORT_ARG=""
DB_HOST_ARG=""
DB_PORT_ARG=""
DB_USER_ARG=""
DB_NAME_ARG=""
DB_PASSWORD_ARG=""
PG_SUPERUSER_ARG=""
PG_SUPER_PASSWORD=""
IMAGE_ARG=""
TLS_CERT_ARG=""
TLS_KEY_ARG=""
ASSUME_YES=0
NO_TLS=0
ENABLE_DKIM=0
REBUILD=0
DOWN_VOLUMES=0
# 1 when `database init` could not create the schema itself, so the stack has to
# prove it afterwards that the server applied the migrations on startup.
MIGRATE_DEFERRED=0

need_value() {
    [ "$#" -ge 2 ] || die "option $1 needs a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        deploy|status|logs|upgrade|dkim|certs|doctor|down|help)
            [ -z "$COMMAND" ] || die "two commands given: $COMMAND and $1"
            COMMAND="$1"; shift ;;
        --wizard)         WIZARD_MODE=1; shift ;;
        --domain)         need_value "$@"; DOMAIN_ARG="$2"; shift 2 ;;
        --hostname)       need_value "$@"; HOSTNAME_ARG="$2"; shift 2 ;;
        --admin)          need_value "$@"; ADMIN_ARG="$2"; shift 2 ;;
        --admin-password) need_value "$@"; ADMIN_PASSWORD="$2"; shift 2 ;;
        --api-host)       need_value "$@"; API_HOST_ARG="$2"; shift 2 ;;
        --api-port)       need_value "$@"; API_PORT_ARG="$2"; shift 2 ;;
        --public-port)    need_value "$@"; PUBLIC_PORT_ARG="$2"; shift 2 ;;
        --db-host)        need_value "$@"; DB_HOST_ARG="$2"; shift 2 ;;
        --db-port)        need_value "$@"; DB_PORT_ARG="$2"; shift 2 ;;
        --db-user)        need_value "$@"; DB_USER_ARG="$2"; shift 2 ;;
        --db-name)        need_value "$@"; DB_NAME_ARG="$2"; shift 2 ;;
        --db-password)    need_value "$@"; DB_PASSWORD_ARG="$2"; shift 2 ;;
        --pg-superuser)   need_value "$@"; PG_SUPERUSER_ARG="$2"; shift 2 ;;
        --pg-password)    need_value "$@"; PG_SUPER_PASSWORD="$2"; shift 2 ;;
        --image)          need_value "$@"; IMAGE_ARG="$2"; shift 2 ;;
        --tls-cert)       need_value "$@"; TLS_CERT_ARG="$2"; shift 2 ;;
        --tls-key)        need_value "$@"; TLS_KEY_ARG="$2"; shift 2 ;;
        --no-tls)         NO_TLS=1; shift ;;
        --enable|--enable-dkim) ENABLE_DKIM=1; shift ;;
        --rebuild)        REBUILD=1; shift ;;
        --volumes)        DOWN_VOLUMES=1; shift ;;
        -y|--yes)         ASSUME_YES=1; shift ;;
        -h|--help)        COMMAND="help"; shift ;;
        -*)               die "unknown option: $1 (try --help)" ;;
        *)                die "unexpected argument: $1 (try --help)" ;;
    esac
done
[ -n "$COMMAND" ] || COMMAND="deploy"

# -----------------------------------------------------------------------------
# Preflight
# -----------------------------------------------------------------------------
preflight() {
    have docker || die "docker is not on PATH. This script deploys Ferroma as containers."
    docker info >/dev/null 2>&1 || die "cannot talk to the Docker daemon (is it running? is this user in the docker group?)"
    docker compose version >/dev/null 2>&1 || die "the 'docker compose' plugin is missing (Docker 24+ ships it)."
    [ -f "$ROOT_DIR/$COMPOSE_FILE" ] || die "$COMPOSE_FILE is missing; run this from a Ferroma checkout."

    # `sudo -n` (no password prompt) is the only form that can be used from a
    # script without hijacking the terminal.
    USE_SUDO=0
    if have sudo && sudo -n true >/dev/null 2>&1; then USE_SUDO=1; fi
}

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------
# Values already in .env beat the built-in defaults; explicit flags beat both.
# That makes a re-run a no-op that only applies what changed.
load_defaults() {
    FERROMA_HOSTNAME=$(env_get FERROMA_HOSTNAME)
    FERROMA_DOMAIN=$(env_get FERROMA_DEPLOY_DOMAIN)
    ADMIN_EMAIL=$(env_get FERROMA_DEPLOY_ADMIN)
    API_HOST=$(env_get FERROMA_API_HOST)
    API_PORT=$(env_get FERROMA_API_PORT)
    PUBLIC_PORT=$(env_get FERROMA_PUBLIC_PORT)
    DB_HOST=$(env_get DB_HOST)
    DB_PORT=$(env_get DB_PORT)
    DB_USER=$(env_get POSTGRES_USER)
    DB_NAME=$(env_get POSTGRES_DB)
    DB_PASSWORD=$(env_get POSTGRES_PASSWORD)
    JWT_SECRET=$(env_get FERROMA_JWT_SECRET)
    FERROMA_IMAGE=$(env_get FERROMA_IMAGE)
    POSTGRES_IMAGE=$(env_get POSTGRES_IMAGE)
    DKIM_SELECTOR=$(env_get FERROMA_DKIM_SELECTOR)
    INSTALLED=$(env_get FERROMA_DEPLOY_SETUP_DONE)

    _host_domain=$(hostname -d 2>/dev/null || true)
    FERROMA_DOMAIN="${DOMAIN_ARG:-${FERROMA_DOMAIN:-${_host_domain:-example.com}}}"
    FERROMA_HOSTNAME="${HOSTNAME_ARG:-${FERROMA_HOSTNAME:-mail.$FERROMA_DOMAIN}}"
    ADMIN_EMAIL="${ADMIN_ARG:-${ADMIN_EMAIL:-admin@$FERROMA_DOMAIN}}"
    API_HOST="${API_HOST_ARG:-${API_HOST:-$DEFAULT_API_HOST}}"
    API_PORT="${API_PORT_ARG:-${API_PORT:-$DEFAULT_API_PORT}}"
    PUBLIC_PORT="${PUBLIC_PORT_ARG:-$PUBLIC_PORT}"
    [ -n "$PUBLIC_PORT" ] || PUBLIC_PORT="$DEFAULT_PUBLIC_PORT"
    DB_HOST="${DB_HOST_ARG:-${DB_HOST:-$DEFAULT_DB_HOST}}"
    DB_PORT="${DB_PORT_ARG:-${DB_PORT:-$DEFAULT_DB_PORT}}"
    DB_USER="${DB_USER_ARG:-${DB_USER:-$DEFAULT_DB_USER}}"
    DB_NAME="${DB_NAME_ARG:-${DB_NAME:-$DEFAULT_DB_NAME}}"
    DB_PASSWORD="${DB_PASSWORD_ARG:-$DB_PASSWORD}"
    # A hand-edited DATABASE_URL survives a re-run: the component flags are the
    # only thing that asks the script to rebuild it. (See `database_url`.)
    DB_COMPONENTS_GIVEN=0
    if [ -n "$DB_HOST_ARG$DB_PORT_ARG$DB_USER_ARG$DB_NAME_ARG$DB_PASSWORD_ARG" ]; then
        DB_COMPONENTS_GIVEN=1
    fi
    FERROMA_IMAGE="${IMAGE_ARG:-${FERROMA_IMAGE:-$DEFAULT_IMAGE}}"
    POSTGRES_IMAGE="${POSTGRES_IMAGE:-$DEFAULT_PG_IMAGE}"
    DKIM_SELECTOR="${DKIM_SELECTOR:-default}"
    PG_SUPERUSER="${PG_SUPERUSER_ARG:-postgres}"

    # A tag containing a slash names a registry: that one is pulled, everything
    # else is a local tag this script builds.
    case "$FERROMA_IMAGE" in
        */*) PULL_IMAGE=1 ;;
        *)   PULL_IMAGE=0 ;;
    esac
}

interview() {
    step "Configuration"
    info "Answers are kept in .env; re-running with the same answers changes nothing."
    printf '\n'

    if [ "$WIZARD_MODE" = 1 ]; then
        # The whole point of the mode: the domain, the MX hostname, the first administrator
        # and the public URL are asked on the wizard page, once the stack is up — so this
        # run only needs to know which port to publish.
        info "wizard mode: the mail domain, hostname, administrator and TLS are collected at"
        info "             http://<this host>:${WEB_PORT:-8080}/admin/ after the stack starts."
        printf '\n'
        [ -n "$PUBLIC_PORT_ARG" ] || WEB_PORT=$(ask "Web port (wizard, Webmail and API)" "${WEB_PORT:-8080}")
        printf '\n'
        return 0
    fi

    [ -n "$DOMAIN_ARG" ]   || FERROMA_DOMAIN=$(ask "Mail domain (the part after @)" "$FERROMA_DOMAIN")
    [ -n "$HOSTNAME_ARG" ] || FERROMA_HOSTNAME=$(ask "MX hostname (must match this host's PTR record)" "mail.$FERROMA_DOMAIN")
    [ -n "$ADMIN_ARG" ]    || ADMIN_EMAIL=$(ask "First administrator address" "admin@$FERROMA_DOMAIN")
    [ -n "$API_PORT_ARG" ] || API_PORT=$(ask "Port your reverse proxy proxies to (bound to loopback)" "$API_PORT")
    [ -n "$PUBLIC_PORT_ARG" ] || PUBLIC_PORT=$(ask "HTTPS port your proxy serves the mail hostname on" "$PUBLIC_PORT")
    [ -n "$DB_HOST_ARG" ]  || DB_HOST=$(ask "PostgreSQL host" "$DB_HOST")
    [ -n "$DB_PORT_ARG" ]  || DB_PORT=$(ask "PostgreSQL port" "$DB_PORT")
    [ -n "$DB_USER_ARG" ]  || DB_USER=$(ask "PostgreSQL role for Ferroma" "$DB_USER")
    [ -n "$DB_NAME_ARG" ]  || DB_NAME=$(ask "PostgreSQL database" "$DB_NAME")
    printf '\n'
}

# Secrets are read back from .env when they are already there, so a re-run never
# invalidates sessions and never moves the database password out from under a
# role that already has it.
resolve_secrets() {
    if [ -z "$DB_PASSWORD" ]; then
        DB_PASSWORD=$(rand_hex 24)
        info "generated a PostgreSQL password for role '$DB_USER' (stored in .env)"
    fi
    if [ -z "$JWT_SECRET" ]; then
        JWT_SECRET=$(rand_hex 32)
        info "generated FERROMA_JWT_SECRET (stored in .env)"
    fi
    if [ -z "$ADMIN_PASSWORD" ]; then
        ADMIN_PASSWORD=$(rand_password)
    fi
}

# -----------------------------------------------------------------------------
# .env — the whole configuration of this deployment
# -----------------------------------------------------------------------------
# The externally reachable base URL. Port 443 is left implicit — every client
# assumes it — and anything else is spelled out, because the links in
# notification mail and `.well-known/ferroma` both have to be followable.
public_url() {
    if [ -n "$PUBLIC_PORT" ] && [ "$PUBLIC_PORT" != "443" ]; then
        printf 'https://%s:%s' "$FERROMA_HOSTNAME" "$PUBLIC_PORT"
    else
        printf 'https://%s' "$FERROMA_HOSTNAME"
    fi
}

# The connection string. A hand-edited `DATABASE_URL` is left alone — somebody who
# appended `?sslmode=require`, used `postgresql://` rather than `postgres://`, or
# pointed it at a differently named database meant it — and only a `--db-*` flag
# on the command line, or a missing value, makes the script write its own.
database_url() {
    _existing=$(env_get DATABASE_URL)
    if [ -n "$_existing" ] && [ "$DB_COMPONENTS_GIVEN" = 0 ]; then
        printf '%s' "$_existing"
    else
        printf 'postgres://%s:%s@%s:%s/%s' "$DB_USER" "$DB_PASSWORD" "$DB_HOST" "$DB_PORT" "$DB_NAME"
    fi
}

seed_env_file() {
    if [ ! -f "$ENV_FILE" ]; then
        {
            printf '# Ferroma — written by scripts/deploy.sh on %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
            printf '#\n'
            printf '# Every line here is passed to the container; .env.example lists the same\n'
            printf '# settings with commentary, and docs/deployment.md §3.1 explains what the\n'
            printf '# script did. This file holds secrets: keep it mode 600, never commit it.\n'
            printf '#\n'
            printf '# Re-run ./scripts/deploy.sh after editing it — or docker compose up -d,\n'
            printf '# which is all a changed value needs.\n'
        } > "$ENV_FILE"
        chmod 600 "$ENV_FILE"
    fi
}

write_env() {
    step "Writing .env"

    env_set DATABASE_URL "$(database_url)"
    if [ -n "$(env_get DATABASE_URL)" ] && [ "$DB_COMPONENTS_GIVEN" = 0 ] \
        && [ "$(env_get DATABASE_URL)" != "postgres://$DB_USER:$DB_PASSWORD@$DB_HOST:$DB_PORT/$DB_NAME" ]; then
        info "kept the DATABASE_URL already in .env (a --db-* flag rewrites it)"
    fi
    env_set POSTGRES_USER "$DB_USER"
    env_set POSTGRES_PASSWORD "$DB_PASSWORD"
    env_set POSTGRES_DB "$DB_NAME"
    env_set DB_HOST "$DB_HOST"
    env_set DB_PORT "$DB_PORT"
    env_set POSTGRES_IMAGE "$POSTGRES_IMAGE"
    env_set FERROMA_IMAGE "$FERROMA_IMAGE"

    if [ "$WIZARD_MODE" = 1 ]; then
        # Stated values would beat the wizard's: `apply_stored_settings` only adopts what
        # the configuration left at its default. So in this mode the hostname, the public
        # URL and the JWT secret are simply absent — the wizard stores the first two, and
        # the server generates the third once into the data volume and reuses it.
        env_set WEB_PORT "${WEB_PORT:-8080}"
        info "wizard mode: hostname, public URL and TLS are left unset for the wizard"
    else
        env_set FERROMA_HOSTNAME "$FERROMA_HOSTNAME"
        env_set FERROMA_PUBLIC_URL "$(public_url)"
        env_set FERROMA_PUBLIC_PORT "$PUBLIC_PORT"
        env_set FERROMA_JWT_SECRET "$JWT_SECRET"
    fi
    env_set FERROMA_DATA_DIR "/var/lib/ferroma"
    [ -n "$(env_get FERROMA_LOG_LEVEL)" ]  || env_set FERROMA_LOG_LEVEL "info"
    [ -n "$(env_get FERROMA_LOG_FORMAT)" ] || env_set FERROMA_LOG_FORMAT "text"

    # The API is reachable only from this host; the reverse proxy owns the public
    # face of it, so it is the proxy's headers that must be believed.
    env_set FERROMA_API_HOST "$API_HOST"
    env_set FERROMA_API_PORT "$API_PORT"
    if [ "$NO_TLS" = 0 ] && [ "$WIZARD_MODE" = 1 ]; then
        # Certificates are in place under ./tls, so the wizard only has to tick the box; the
        # switch itself is a stored setting, and stating it here would override that.
        env_set FERROMA__API__TRUST_PROXY_HEADERS "true"
        env_set FERROMA__API__SECURE_COOKIES "true"
        env_set FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH "true"
        env_set FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN "true"
    elif [ "$NO_TLS" = 0 ]; then
        env_set FERROMA__API__TRUST_PROXY_HEADERS "true"
        env_set FERROMA__API__SECURE_COOKIES "true"
        env_set FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH "true"
        env_set FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN "true"
    else
        env_set FERROMA__API__TRUST_PROXY_HEADERS "false"
        env_set FERROMA__API__SECURE_COOKIES "false"
        env_set FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH "false"
        env_set FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN "false"
    fi

    env_set FERROMA_DKIM_SELECTOR "$DKIM_SELECTOR"
    # The key lives inside the data volume: the container writes it as uid 10001
    # with no host-directory ownership dance, and a backup of that volume takes
    # it along. Losing it invalidates every DKIM signature this host has made.
    env_set FERROMA_DKIM_KEY "/var/lib/ferroma/dkim/$DKIM_SELECTOR.private"
    [ -n "$(env_get FERROMA_DKIM_ENABLED)" ] || env_set FERROMA_DKIM_ENABLED "false"

    # Bookkeeping for this script. `Config::load` ignores names that are neither
    # `FERROMA__*` nor a documented alias, so these reach nothing but deploy.sh.
    env_set FERROMA_DEPLOY_DOMAIN "$FERROMA_DOMAIN"
    env_set FERROMA_DEPLOY_ADMIN "$ADMIN_EMAIL"
    [ -n "$(env_get FERROMA_DEPLOY_SETUP_DONE)" ] || env_set FERROMA_DEPLOY_SETUP_DONE "0"

    info "wrote $ENV_FILE (mode 600)"
}

# -----------------------------------------------------------------------------
# PostgreSQL on this host
# -----------------------------------------------------------------------------
# This image is a *client*, not a database: it supplies `psql` for the checks
# below when the host has none. This stack starts no PostgreSQL server of its
# own — yours is the only one.
pull_pg_image() {
    # Only when the host has no `psql` of its own: `psql_app` and the provisioning
    # fallback both prefer the host's client, and this image is what stands in when
    # there is none. Skipping the pull is what keeps a redundant ~100 MB image (and
    # its unpacked ~400 MB) off a host that already has the client.
    if have psql; then
        return 0
    fi
    if ! image_exists "$POSTGRES_IMAGE"; then
        step "Pulling $POSTGRES_IMAGE"
        info "the host has no psql, so this client image stands in for the checks below"
        info "(it is not a database server: nothing here starts a second PostgreSQL)"
        docker pull "$POSTGRES_IMAGE" 2>&1 | tail -n 3
        image_exists "$POSTGRES_IMAGE" \
            || die "cannot pull $POSTGRES_IMAGE — check the registry mirror or the network, or set POSTGRES_IMAGE in .env"
    fi
}

# Talk to the database the way Ferroma will. Any psql will do, so the postgres
# image stands in when the host has none.
psql_app() {
    # `PGCONNECT_TIMEOUT` and `-w`: a filtered port must fail in seconds rather
    # than wait for the kernel's SYN timeout, and a script must never sit at a
    # password prompt.
    if have psql; then
        PGPASSWORD="$DB_PASSWORD" PGCONNECT_TIMEOUT=5 \
            psql -w -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d "$DB_NAME" "$@" 2>&1
    else
        docker run --rm --network host -e PGPASSWORD="$DB_PASSWORD" -e PGCONNECT_TIMEOUT=5 "$POSTGRES_IMAGE" \
            psql -w -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d "$DB_NAME" "$@" 2>&1
    fi
}

probe_db() {
    DB_PROBE_OUT=$(psql_app -tAc 'select 1') && return 0
    return 1
}

print_db_help() {
    # Where would the operator have to type the SQL? A containerised PostgreSQL
    # has no `postgres` system user on the host to `sudo -u` into.
    _pgc=""
    if db_host_is_local; then
        _pgc=$(local_postgres_containers | head -n 1)
    fi

    printf '\n' >&2
    warn "PostgreSQL refused the connection Ferroma needs:"
    printf '      %s\n\n' "$(printf '%s' "${DB_PROBE_OUT:-}" | head -n 3)" >&2
    case "${DB_PROBE_OUT:-}" in
        *"password authentication failed"*)
            warn "the server rejected that role and password."
            info "Under scram-sha-256 this same message is returned whether the password"
            info "is wrong *or* the role does not exist at all, so check which it is:"
            info "  docker exec -i <postgres container> psql -U $PG_SUPERUSER -c '\\du'"
            info "Then either reset the password, or create the role — see below." ;;
        *"role"*"does not exist"*)
            warn "the role does not exist and could not be created automatically." ;;
        *"database"*"does not exist"*)
            warn "the database does not exist and could not be created automatically." ;;
        *"no pg_hba.conf entry"*|*"ident authentication"*)
            warn "pg_hba.conf does not allow a password login from 127.0.0.1 (common on RHEL)."
            info "Add this to pg_hba.conf, then run SELECT pg_reload_conf();"
            info "  host    $DB_NAME    $DB_USER    127.0.0.1/32    scram-sha-256" ;;
        *"could not connect"*|*"Connection refused"*|*"timeout expired"*|*"could not translate host"*|*"timed out"*)
            warn "nothing answered at $DB_HOST:$DB_PORT."
            if [ "$DB_HOST" != "127.0.0.1" ] && [ "$DB_HOST" != "localhost" ]; then
                info "If PostgreSQL runs on this machine, use its loopback address instead — a"
                info "public hostname that points back here often cannot be reached from here:"
                info "  ./scripts/deploy.sh --db-host 127.0.0.1"
            fi
            info "If it is genuinely elsewhere, check that it listens on that address and that"
            info "the firewall between here and it allows $DB_PORT." ;;
    esac
    printf '\n' >&2
    info "Run this on this host (skip whichever object already exists), then start"
    info "deploy.sh again — it reuses the password already in .env:"
    printf '\n' >&2
    if [ -n "${_pgc:-}" ]; then
        # The database is a container (1Panel and friends): there is no `postgres`
        # system user on the host, so the commands go through `docker exec`. `-d
        # template1` and `--maintenance-db=template1` because the cluster may have
        # no database called postgres to be the default one.
        _su=$(docker exec -i "$_pgc" sh -c 'printf "%s" "$POSTGRES_USER"' 2>/dev/null || true)
        [ -n "$_su" ] || _su="$PG_SUPERUSER"
        printf "    docker exec -i %s psql -d template1 -U %s -c \"ALTER ROLE %s WITH PASSWORD '<the password in .env>';\"\n" \
            "$_pgc" "$_su" "$DB_USER" >&2
        printf '    docker exec -i %s createdb --maintenance-db=template1 -U %s -O %s --encoding=UTF8 --locale=C %s\n' \
            "$_pgc" "$_su" "$DB_USER" "$DB_NAME" >&2
        info "this container's configured superuser is '$_su' — the app role may itself be it"
    else
        printf "    sudo -u postgres psql -c \"CREATE ROLE %s LOGIN PASSWORD '<the password in .env>';\"\n" "$DB_USER" >&2
        printf '    sudo -u postgres createdb -O %s --encoding=UTF8 --locale=C %s\n' "$DB_USER" "$DB_NAME" >&2
    fi
    printf '\n' >&2
}

# Create the role and the database, in the order that asks least of the
# operator: peer auth as the postgres system user first, then TCP with a
# superuser password (from the host's psql, or from a container when there is
# no psql on the host).
#
# The statements go in on stdin rather than through `-f`: a file that only the
# invoking user can read is unreadable to the `postgres` system user this
# deliberately runs as.
run_superuser_sql() {
    _file="$1"
    # `-d template1`, not the default database: psql defaults to a database named
    # after the user (`postgres` for the postgres role), and a cluster — a managed
    # or panel-provisioned one especially — does not always have it. Every cluster
    # has template1.
    if [ "$USE_SUDO" = 1 ]; then
        sudo -n -u postgres psql -d template1 -v ON_ERROR_STOP=1 -tA < "$_file" >/dev/null 2>&1 && return 0
    fi
    if [ "$(id -u)" = 0 ]; then
        su -s /bin/sh postgres -c 'psql -d template1 -v ON_ERROR_STOP=1 -tA' < "$_file" >/dev/null 2>&1 && return 0
    fi
    # PostgreSQL in a container on this host (1Panel and friends): its own psql
    # has peer/trust access to its server, which is the only way in when the host
    # has no `postgres` system user. Only when the configured database *is* this
    # machine, so a remote database is never provisioned by accident.
    #
    # The superuser is not necessarily `postgres`: the image creates the role named
    # by POSTGRES_USER, and a panel-provisioned cluster often makes that the only
    # one — there may be no role called postgres at all, and no database called
    # postgres either. The container knows what it was told, so ask it.
    if db_host_is_local; then
        for _c in $(local_postgres_containers); do
            _configured=$(docker exec -i "$_c" sh -c 'printf "%s" "$POSTGRES_USER"' 2>/dev/null || true)
            for _role in "$_configured" "$PG_SUPERUSER" "$DB_USER"; do
                [ -n "$_role" ] || continue
                docker exec -i "$_c" psql -d template1 -U "$_role" -v ON_ERROR_STOP=1 -tA < "$_file" >/dev/null 2>&1 && return 0
            done
        done
    fi
    if [ -n "$PG_SUPER_PASSWORD" ] && have psql; then
        PGPASSWORD="$PG_SUPER_PASSWORD" PGCONNECT_TIMEOUT=5 psql -w -d template1 -h "$DB_HOST" -p "$DB_PORT" -U "$PG_SUPERUSER" \
            -v ON_ERROR_STOP=1 -tA < "$_file" >/dev/null 2>&1 && return 0
    fi
    if [ -n "$PG_SUPER_PASSWORD" ]; then
        docker run --rm --network host -v "$_file:/ferroma-provision.sql:ro" \
            -e PGPASSWORD="$PG_SUPER_PASSWORD" -e PGCONNECT_TIMEOUT=5 "$POSTGRES_IMAGE" \
            psql -w -d template1 -h "$DB_HOST" -p "$DB_PORT" -U "$PG_SUPERUSER" \
            -v ON_ERROR_STOP=1 -tAf /ferroma-provision.sql >/dev/null 2>&1 && return 0
    fi
    return 1
}

# The provisioning script. `\gexec` executes the generated CREATE DATABASE only
# when the database is missing, which keeps the whole thing idempotent — the
# normal case on a second run, where both objects already exist and both
# statements must simply do nothing.
#
# Uses `_pw` (the escaped password) from the caller.
write_provision_sql() {
    _target="$1"
    _locale="$2"
    {
        printf "DO \$ferroma\$\n"
        printf 'BEGIN\n'
        printf '    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = %s) THEN\n' "'$DB_USER'"
        printf '        CREATE ROLE "%s" LOGIN PASSWORD %s;\n' "$DB_USER" "'$_pw'"
        printf '    END IF;\n'
        printf 'END\n'
        printf '%s\n' '$ferroma$;'
        # Deterministic collation, as in the bundled stack: a mail server must
        # sort identically on every host. TEMPLATE template0 permits the locale.
        printf "SELECT format('CREATE DATABASE %%I OWNER %%I ENCODING ''UTF8''%s TEMPLATE template0', %s, %s)\n" \
            "$_locale" "'$DB_NAME'" "'$DB_USER'"
        printf 'WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = %s);\n' "'$DB_NAME'"
        printf '%s\n' '\gexec'
    } > "$_target"
    chmod 600 "$_target"
}

provision_database() {
    _pw=$(printf '%s' "$DB_PASSWORD" | sed "s/'/''/g")

    _sql=$(mktemp "${TMPDIR:-/tmp}/ferroma-provision.XXXXXX.sql")
    TMP_FILES="$TMP_FILES $_sql"
    write_provision_sql "$_sql" " LC_COLLATE ''C'' LC_CTYPE ''C''"

    step "Creating role '$DB_USER' and database '$DB_NAME'"
    if run_superuser_sql "$_sql"; then
        info "created (or already present)"
        return 0
    fi

    # A cluster whose default collation is not `C` can refuse the locale
    # clauses; a database without them is still far better than no database.
    _retry=$(mktemp "${TMPDIR:-/tmp}/ferroma-provision-retry.XXXXXX.sql")
    TMP_FILES="$TMP_FILES $_retry"
    write_provision_sql "$_retry" ""
    if run_superuser_sql "$_retry"; then
        info "created, with the cluster's default collation"
        warn "this database does not sort like the bundled stack; create it by hand if that matters"
        return 0
    fi
    return 1
}

ensure_database() {
    step "Checking PostgreSQL at $DB_HOST:$DB_PORT"
    info "as $DB_USER, database $DB_NAME"
    pull_pg_image

    # Fail in seconds on a filtered port, with a diagnosis, instead of letting
    # libpq wait out the kernel's SYN timeout and look like a hang.
    if ! port_reachable "$DB_HOST" "$DB_PORT"; then
        warn "no TCP connection to $DB_HOST:$DB_PORT within 3 seconds."
        if [ "$DB_HOST" != "127.0.0.1" ] && [ "$DB_HOST" != "localhost" ]; then
            info "If PostgreSQL is on this machine, point the script at its loopback address:"
            info "  a public hostname that resolves back to this host frequently cannot be reached"
            info "  from this host (hairpin NAT), and a cloud firewall may block $DB_PORT outright."
            printf '\n' >&2
            info "  ./scripts/deploy.sh --db-host 127.0.0.1 …   # keeps everything already in .env"
        else
            info "Check that PostgreSQL is running and listening on $DB_PORT:"
            info "  ss -ltnp | grep $DB_PORT"
        fi
        die "no usable database — nothing was started."
    fi

    if probe_db; then
        info "$DB_USER@$DB_NAME reachable"
    else
        if provision_database && probe_db; then
            info "$DB_USER@$DB_NAME reachable"
        else
            print_db_help
            die "no usable database — nothing was started."
        fi
    fi

    # Keep the client image near the server's major version: a much older psql
    # warns on every connect and cannot always read newer catalog columns. The
    # image supplies clients only — no server is ever started.
    _num=$(psql_app -tAc 'show server_version_num' | tr -d ' \r\n' || true)
    case "$_num" in
        ''|*[!0-9]*) : ;;
        *)
            _major=$(awk -v v="$_num" 'BEGIN { printf "%d", v / 10000 }')
            if [ -n "$_major" ] && [ "$_major" -ge 10 ]; then
                if [ "$POSTGRES_IMAGE" != "postgres:$_major-alpine" ]; then
                    POSTGRES_IMAGE="postgres:$_major-alpine"
                    env_set POSTGRES_IMAGE "$POSTGRES_IMAGE"
                    info "PostgreSQL $_major detected: using $POSTGRES_IMAGE as the client image"
                fi
            fi
            ;;
    esac
}

# -----------------------------------------------------------------------------
# Image
# -----------------------------------------------------------------------------
build_image() {
    if [ "$PULL_IMAGE" = 1 ]; then
        step "Pulling $FERROMA_IMAGE"
        docker pull "$FERROMA_IMAGE"
        return 0
    fi
    if image_exists "$FERROMA_IMAGE" && [ "$REBUILD" = 0 ]; then
        info "image $FERROMA_IMAGE is already present (--rebuild to rebuild it)"
        return 0
    fi

    _version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -n 1)
    step "Building the image — a full Rust release build, 10–30 minutes the first time"
    if [ -n "$_version" ]; then
        docker build -t "$FERROMA_IMAGE" -t "ferroma:$_version" "$ROOT_DIR"
        info "tagged $FERROMA_IMAGE and ferroma:$_version"
    else
        docker build -t "$FERROMA_IMAGE" "$ROOT_DIR"
    fi
}

# SMTP (25), submission (587) and IMAP (143) are privileged ports. Docker gives a
# bridge-network container a namespace whose `net.ipv4.ip_unprivileged_port_start`
# is 0, so uid 10001 may bind them; under `network_mode: host` the container shares
# the *host's* namespace, where the limit is usually 1024. The image grants the
# binary NET_BIND_SERVICE for exactly this case — and if that capability did not
# survive the build, the host sysctl is the other way through. Better to say so now
# than to watch the container exit a second after `up -d`.
check_privileged_ports() {
    _start=$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start 2>/dev/null || echo 1024)
    case "$_start" in ''|*[!0-9]*) _start=1024 ;; esac
    if [ "$_start" -le 143 ]; then
        return 0
    fi

    _caps=$(docker run --rm --entrypoint getcap "$FERROMA_IMAGE" /usr/local/bin/ferroma 2>/dev/null || true)
    case "$_caps" in
        *cap_net_bind_service*) return 0 ;;
    esac

    warn "this kernel reserves ports below $_start for root, and the image's file"
    warn "capability is missing, so uid 10001 cannot bind 25, 587 or 143."
    info "Fix either one:"
    info "  sudo sysctl -w net.ipv4.ip_unprivileged_port_start=0"
    info "  echo 'net.ipv4.ip_unprivileged_port_start=0' | sudo tee /etc/sysctl.d/99-ferroma.conf"
    if [ "$(id -u)" = 0 ] && confirm "Lower it now (this boot)?"; then
        sysctl -w net.ipv4.ip_unprivileged_port_start=0 >/dev/null 2>&1 \
            && info "applied — make it survive reboots with the /etc/sysctl.d line above"
    fi
}

# -----------------------------------------------------------------------------
# TLS for the SMTP and IMAP listeners
# -----------------------------------------------------------------------------
discover_tls() {
    TLS_CERT_SRC=""
    TLS_KEY_SRC=""
    if [ -n "$TLS_CERT_ARG" ]; then
        TLS_CERT_SRC="$TLS_CERT_ARG"
        TLS_KEY_SRC="$TLS_KEY_ARG"
        [ -n "$TLS_KEY_SRC" ] || die "--tls-cert needs --tls-key"
        return 0
    fi
    for _dir in "/etc/letsencrypt/live/$FERROMA_HOSTNAME" "/etc/letsencrypt/live/$FERROMA_DOMAIN"; do
        if [ -f "$_dir/fullchain.pem" ] && [ -f "$_dir/privkey.pem" ]; then
            if [ "$ASSUME_YES" = 1 ] || { [ -t 0 ] && confirm "Found $_dir. Install it for Ferroma's SMTP/IMAP listeners?"; }; then
                TLS_CERT_SRC="$_dir/fullchain.pem"
                TLS_KEY_SRC="$_dir/privkey.pem"
            fi
            return 0
        fi
    done
    return 0
}

install_tls() {
    step "TLS"
    if [ "$NO_TLS" = 1 ]; then
        warn "deploying without TLS: mail clients cannot authenticate without STARTTLS."
        env_set FERROMA_TLS_ENABLED "false"
        env_del FERROMA__SMTP__SMTPS_PORT
        env_del FERROMA__IMAP__IMAPS_PORT
        return 0
    fi

    # A certificate already installed by an earlier run stays in use, so
    # re-running the script cannot silently downgrade a TLS deployment.
    if [ -z "$TLS_CERT_SRC" ] && [ -f "$ROOT_DIR/tls/fullchain.pem" ] && [ -f "$ROOT_DIR/tls/privkey.pem" ]; then
        info "keeping the certificate already installed in $ROOT_DIR/tls"
        TLS_CERT_SRC="$ROOT_DIR/tls/fullchain.pem"
        TLS_KEY_SRC="$ROOT_DIR/tls/privkey.pem"
        TLS_ALREADY_INSTALLED=1
    fi

    if [ -z "$TLS_CERT_SRC" ]; then
        warn "no certificate, so TLS stays off: nothing can submit or read mail yet."
        info "Point the script at a bundle and a key and run it again:"
        info "  ./scripts/deploy.sh --tls-cert /etc/letsencrypt/live/$FERROMA_HOSTNAME/fullchain.pem \\"
        info "                     --tls-key  /etc/letsencrypt/live/$FERROMA_HOSTNAME/privkey.pem"
        info "The certificate must cover $FERROMA_HOSTNAME, not only $FERROMA_DOMAIN."
        env_set FERROMA_TLS_ENABLED "false"
        env_del FERROMA__SMTP__SMTPS_PORT
        env_del FERROMA__IMAP__IMAPS_PORT
        return 0
    fi

    [ -f "$TLS_CERT_SRC" ] || die "no such certificate: $TLS_CERT_SRC"
    [ -f "$TLS_KEY_SRC" ] || die "no such private key: $TLS_KEY_SRC"

    # The image runs as uid 10001 and reads these files itself, so they have to
    # be owned by it — not merely readable by root.
    if [ "${TLS_ALREADY_INSTALLED:-0}" = 1 ] || [ "$(id -u)" = 0 ]; then
        if [ "${TLS_ALREADY_INSTALLED:-0}" != 1 ]; then
            install -d -o 10001 -g 10001 -m 0750 "$ROOT_DIR/tls"
            install -o 10001 -g 10001 -m 0640 "$TLS_CERT_SRC" "$ROOT_DIR/tls/fullchain.pem"
            install -o 10001 -g 10001 -m 0600 "$TLS_KEY_SRC"  "$ROOT_DIR/tls/privkey.pem"
            info "installed tls/fullchain.pem and tls/privkey.pem (owner uid 10001)"
        fi
        env_set FERROMA_TLS_ENABLED "true"
        env_set FERROMA_TLS_CERT "/etc/ferroma/tls/fullchain.pem"
        env_set FERROMA_TLS_KEY "/etc/ferroma/tls/privkey.pem"
        env_set FERROMA__SMTP__SMTPS_PORT "465"
        env_set FERROMA__IMAP__IMAPS_PORT "993"
    else
        warn "installing the certificate needs root (the container reads it as uid 10001):"
        printf '      sudo %s --tls-cert %s --tls-key %s\n' "$0" "$TLS_CERT_SRC" "$TLS_KEY_SRC" >&2
        env_set FERROMA_TLS_ENABLED "false"
        env_del FERROMA__SMTP__SMTPS_PORT
        env_del FERROMA__IMAP__IMAPS_PORT
    fi

    if have openssl; then
        # One name per line, each ending in a comma, so the test below cannot
        # match a prefix of a longer name.
        _sans=$(openssl x509 -in "$TLS_CERT_SRC" -noout -text 2>/dev/null \
            | sed -n '/Subject Alternative Name/,+1p' | tr -d ' ' | tr ',' '\n' \
            | sed -n 's/^DNS://p' | tr '\n' ',' || true)
        case "$_sans" in
            *"$FERROMA_HOSTNAME,"*) : ;;
            *) warn "this certificate does not cover $FERROMA_HOSTNAME"
               warn "mail clients will reject it; reissue it with -d $FERROMA_HOSTNAME"
               warn "certificate covers: ${_sans%,}" ;;
        esac
        _expiry=$(openssl x509 -in "$TLS_CERT_SRC" -noout -enddate 2>/dev/null || true)
        [ -n "$_expiry" ] && info "$_expiry"
    fi
}

# -----------------------------------------------------------------------------
# The stack
# -----------------------------------------------------------------------------
# A named volume is initialised from whichever image mounts it *first*: mount one
# without /var/lib/ferroma and the volume comes up empty and owned by root, so
# Ferroma (uid 10001) cannot write to it. Populating it here, from the image that
# owns the directory, settles the ownership before the stack — or a hand-run
# `docker run -v ferroma-data:…` — can get it wrong.
prepare_data_volume() {
    step "Preparing the data volume"
    docker run --rm -v ferroma-data:/var/lib/ferroma "$FERROMA_IMAGE" version >/dev/null
    info "ferroma-data is ready (owned by uid 10001)"
}

migrate_database() {
    step "Creating the schema"

    # `database init` also *creates* the database when it is missing, and to do
    # that it connects to a maintenance database named `postgres` — which a
    # managed or panel-provisioned cluster does not always have. The server applies
    # the migrations itself on startup (`database.run_migrations` defaults to true),
    # so on such a cluster the step is skipped rather than left to fail.
    if _maint=$(psql_app -tAc "select 1 from pg_database where datname = 'postgres'"); then
        if [ "$(printf '%s' "$_maint" | tr -d ' \r\n')" != "1" ]; then
            info "this cluster has no maintenance database named postgres"
            info "the migrations are applied by the server at startup — skipping database init"
            MIGRATE_DEFERRED=1
            return 0
        fi
    fi

    if compose run --rm -T ferroma database init; then
        return 0
    fi

    warn "ferroma database init could not complete — most often because the role may not"
    warn "create databases, or the target database is not the one it connected to."
    info "the server applies the migrations itself when it starts, so this is not fatal:"
    info "starting the stack now, and checking the schema afterwards."
    MIGRATE_DEFERRED=1
    return 0
}

start_stack() {
    step "Starting the containers"
    # `--remove-orphans` retires containers whose service this compose file no
    # longer defines. The backup sidecar of earlier releases is the case that
    # matters: without it that container keeps running under `restart: always`
    # for as long as the host lives, next to a stack that no longer has one.
    compose up -d --remove-orphans
}

wait_healthy() {
    step "Waiting for the health check"
    _tries=0
    while [ "$_tries" -lt 90 ]; do
        _state=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$APP_CONTAINER" 2>/dev/null || echo missing)
        case "$_state" in
            healthy)
                info "healthy"
                return 0 ;;
            unhealthy|exited|dead|missing)
                printf '\n'
                _logs=$(compose logs --tail=60 ferroma 2>&1 || true)
                printf '%s\n' "$_logs"
                explain_startup_failure "$_logs"
                die "the container is $_state — the log above says why. docs/deployment.md §11 maps symptom to cause." ;;
        esac
        _tries=$((_tries + 1))
        sleep 2
    done
    _logs=$(compose logs --tail=60 ferroma 2>&1 || true)
    printf '%s\n' "$_logs"
    explain_startup_failure "$_logs"
    die "the container did not become healthy within 3 minutes."
}

# Two startup failures have a single-line fix each, and both look like a wall of
# log to someone who has not met them before.
explain_startup_failure() {
    case "$1" in
        *"Permission denied"*|*"permission denied"*|*"EACCES"*)
            warn "that is the privileged-port problem (25/587/143 belong to root here):"
            info "  sudo sysctl -w net.ipv4.ip_unprivileged_port_start=0" ;;
        *"Address already in use"*|*"address already in use"*|*"EADDRINUSE"*)
            warn "something else holds one of the mail ports:"
            info "  ss -ltnp | grep -E ':(25|587|465|143|993)\$'" ;;
        *"database"*"does not exist"*|*"password authentication"*)
            warn "the database is unreachable from inside the container."
            info "  docker compose -f $COMPOSE_FILE run --rm ferroma database status" ;;
    esac
}

# -----------------------------------------------------------------------------
# First run
# -----------------------------------------------------------------------------
create_admin() {
    # The password is piped, never passed as an argument: `ferroma user create`
    # reads a secret from stdin precisely so it stays out of `ps`. `run` is tried
    # first because it always attaches stdin; `exec` covers the case where the
    # service container already exists under a fixed name.
    if printf '%s\n' "$ADMIN_PASSWORD" \
        | compose run --rm -T ferroma user create "$ADMIN_EMAIL" --admin >/dev/null 2>&1; then
        return 0
    fi
    if printf '%s\n' "$ADMIN_PASSWORD" \
        | compose exec -T ferroma ferroma user create "$ADMIN_EMAIL" --admin >/dev/null 2>&1; then
        return 0
    fi
    return 1
}

first_run() {
    if [ "$(env_get FERROMA_DEPLOY_SETUP_DONE)" = "1" ]; then
        return 0
    fi
    if [ "$WIZARD_MODE" = 1 ]; then
        step "First-run wizard"
        info "no administrator yet: open http://<this host>:${WEB_PORT:-8080}/admin/ and fill"
        info "in the domain, the hostname, the first administrator and TLS."
        return 0
    fi

    step "Creating the first administrator: $ADMIN_EMAIL"
    if create_admin; then
        info "created, with the domain $FERROMA_DOMAIN and its standard folders"
        env_set FERROMA_DEPLOY_SETUP_DONE "1"
        ADMIN_CREATED=1
    else
        warn "could not create $ADMIN_EMAIL automatically; run it yourself:"
        printf '      docker compose -f %s run --rm ferroma user create %s --admin\n' "$COMPOSE_FILE" "$ADMIN_EMAIL" >&2
        printf '      (it reads the password from stdin, and creates the domain automatically)\n' >&2
        ADMIN_CREATED=0
    fi
}

# -----------------------------------------------------------------------------
# DKIM
# -----------------------------------------------------------------------------
dkim_key() {
    step "DKIM"
    compose exec -T ferroma mkdir -p /var/lib/ferroma/dkim >/dev/null 2>&1 || true
    _out=$(compose run --rm -T ferroma dkim generate --domain "$FERROMA_DOMAIN" \
        --selector "$DKIM_SELECTOR" \
        --out "/var/lib/ferroma/dkim/$DKIM_SELECTOR.private" 2>&1) && _ok=1 || _ok=0
    if [ "$_ok" = 1 ]; then
        info "generated a 2048-bit key for $FERROMA_DOMAIN (selector $DKIM_SELECTOR)"
    else
        case "$_out" in
            *"already has a DKIM key"*)
                info "a key already exists for $FERROMA_DOMAIN (selector $DKIM_SELECTOR)" ;;
            *"no such domain"*)
                warn "the domain $FERROMA_DOMAIN does not exist yet:"
                warn "  create it, then run ./scripts/deploy.sh dkim" ;;
            *)
                warn "dkim generate failed:"
                printf '      %s\n' "$_out" >&2 ;;
        esac
    fi
    DKIM_RECORD=$(compose run --rm -T ferroma dkim show --domain "$FERROMA_DOMAIN" 2>/dev/null | tail -n 1 || true)
    if [ -n "$DKIM_RECORD" ]; then
        raw ""
        raw "  Publish this TXT record:"
        raw "    $DKIM_RECORD"
        raw ""
    else
        warn "could not read the public key back; run: docker compose -f $COMPOSE_FILE run --rm ferroma dkim show --domain $FERROMA_DOMAIN"
    fi
}

dkim_enable_signing() {
    env_set FERROMA_DKIM_ENABLED "true"
    compose up -d ferroma >/dev/null
    info "outbound signing enabled"
}

# -----------------------------------------------------------------------------
# Summary
# -----------------------------------------------------------------------------
summary() {
    _tls=$(env_get FERROMA_TLS_ENABLED)
    printf '\n'
    printf '%s────────────────────────────────────────────────────────────────%s\n' "$C_BOLD" "$C_RESET"
    printf '%s Ferroma is deployed%s\n' "$C_BOLD" "$C_RESET"
    printf '%s────────────────────────────────────────────────────────────────%s\n' "$C_BOLD" "$C_RESET"
    printf '\n'
    printf '  Webmail     %s/            (through your proxy)\n' "$(public_url)"
    printf '  Admin       %s/admin\n' "$(public_url)"
    printf '  API         http://%s:%s/api/v1   (loopback: the proxy reaches it)\n' "$API_HOST" "$API_PORT"
    if [ "$_tls" = "true" ]; then
        printf '  Mail        smtp 25, submission 587, smtps 465, imap 143, imaps 993\n'
    else
        printf '  Mail        smtp 25, submission 587, imap 143   %s(no TLS yet)%s\n' "$C_YELLOW" "$C_RESET"
    fi
    printf '  Database    postgres://%s@%s:%s/%s\n' "$DB_USER" "$DB_HOST" "$DB_PORT" "$DB_NAME"
    printf '  Data        volume ferroma-data — mail, attachments, DKIM keys\n'
    printf '              no backup service ships: back this volume up together with\n'
    printf '              the database — docs/deployment.md §8\n'
    printf '\n'

    if [ "${ADMIN_CREATED:-0}" = 1 ]; then
        printf '  %sAdministrator%s\n' "$C_BOLD" "$C_RESET"
        printf '    %s\n' "$ADMIN_EMAIL"
        printf '    %s\n' "$ADMIN_PASSWORD"
        printf '    shown once: change it from the Admin panel, or with\n'
        printf '      docker compose -f %s run --rm ferroma user password %s\n' "$COMPOSE_FILE" "$ADMIN_EMAIL"
        printf '\n'
    fi

    if [ "$WIZARD_MODE" = 1 ]; then
        printf '  %sFirst-run wizard%s\n' "$C_BOLD" "$C_RESET"
        printf '    http://<this host>:%s/admin/\n' "${WEB_PORT:-8080}"
        printf '    the mail domain, hostname, first administrator and TLS are set there;\n'
        printf '    DNS (MX/A/PTR/SPF/DKIM/DMARC) has to be in place before mail flows.\n'
        printf '\n'
    fi

    printf '  %sYour reverse proxy%s\n' "$C_BOLD" "$C_RESET"
    printf '    server_name %s;\n' "${FERROMA_HOSTNAME:-mail.example.com}"
    printf '    listen %s ssl http2;\n' "$PUBLIC_PORT"
    printf '    location / {\n'
    printf '        proxy_pass http://%s:%s;\n' "$API_HOST" "$API_PORT"
    printf '        proxy_http_version 1.1;\n'
    printf '        proxy_set_header Host $host;\n'
    printf '        proxy_set_header X-Real-IP $remote_addr;\n'
    printf '        proxy_set_header X-Forwarded-For $remote_addr;   # overwrite, never append\n'
    printf '        proxy_set_header X-Forwarded-Proto $scheme;\n'
    printf '        proxy_set_header Upgrade $http_upgrade;\n'
    printf '        proxy_set_header Connection "upgrade";\n'
    printf '        proxy_read_timeout 3600s;   # the sync WebSocket\n'
    printf '    }\n'
    printf '    client_max_body_size 30m;       # >= limits.max_message_size\n'
    printf '    /.well-known/mta-sts.txt is served by Ferroma: proxy it like everything else.\n'
    printf '    The full block, with the security headers: docs/deployment.md §5.4.\n'
    if [ "$PUBLIC_PORT" != "443" ]; then
        printf '\n'
        raw "  Note: HTTPS is on $PUBLIC_PORT, and two things are defined to live on 443:"
        raw "    * client autodiscovery (https://$FERROMA_DOMAIN/.well-known/ferroma) —"
        raw "      configure clients with $FERROMA_HOSTNAME:$PUBLIC_PORT explicitly"
        raw "    * MTA-STS (RFC 8461 fetches the policy over 443) — skip it, or serve it"
        raw "      from whatever holds 443 for this name"
        raw "    Forwarding 443 -> $PUBLIC_PORT at your NAT keeps both of them working."
    fi
    case "$API_HOST" in
        127.0.0.1|localhost|::1)
            printf '\n'
            raw "  If that proxy runs in a *container*, it cannot reach 127.0.0.1: bind the API"
            raw "  to the host's address on the Docker bridge and point the proxy there instead:"
            raw "    ./scripts/deploy.sh --api-host $(docker_bridge_gateway)"
            raw "    proxy_pass http://$(docker_bridge_gateway):$API_PORT;"
            ;;
    esac
    printf '\n'

    printf '  %sDNS still to publish%s\n' "$C_BOLD" "$C_RESET"
    printf '    A      %-28s -> this host\n' "$FERROMA_HOSTNAME"
    printf '    PTR    this host IP                 -> %s\n' "$FERROMA_HOSTNAME"
    printf '    MX     %-28s 10 %s.\n' "$FERROMA_DOMAIN" "$FERROMA_HOSTNAME"
    printf '    TXT    %-28s "v=spf1 mx -all"\n' "$FERROMA_DOMAIN"
    printf '    TXT    %-28s "v=DMARC1; p=none; rua=mailto:dmarc@%s"\n' "_dmarc.$FERROMA_DOMAIN" "$FERROMA_DOMAIN"
    printf '    Why each one matters, and the rest: docs/deployment.md §2.\n'
    printf '\n'

    printf '  %sDay to day%s\n' "$C_BOLD" "$C_RESET"
    printf '    ./scripts/deploy.sh status      containers, health, database\n'
    printf '    ./scripts/deploy.sh logs        follow the log\n'
    printf '    ./scripts/deploy.sh upgrade     rebuild the image and restart\n'
    printf '    ./scripts/deploy.sh doctor      check every listener and dependency\n'
    printf '    ./scripts/deploy.sh dkim        reprint the DKIM record\n'
    printf '\n'
}

# -----------------------------------------------------------------------------
# Commands
# -----------------------------------------------------------------------------
cmd_deploy() {
    preflight
    load_defaults
    # An unattended first run has nothing to default to: guessing a domain would
    # deploy a mail server for a name nobody owns.
    if [ "$ASSUME_YES" = 1 ] && [ "$INSTALLED" != "1" ] && [ -z "$DOMAIN_ARG" ]; then
        die "--yes on a first run also needs --domain (and probably --admin): nothing can be guessed."
    fi
    if [ "$INSTALLED" != "1" ] || [ "$ASSUME_YES" = 0 ]; then
        interview
    fi
    resolve_secrets
    discover_tls
    seed_env_file
    write_env
    ensure_database
    build_image
    check_privileged_ports
    prepare_data_volume
    migrate_database
    install_tls
    start_stack
    wait_healthy
    if [ "$MIGRATE_DEFERRED" = 1 ]; then
        step "Confirming the schema the server just applied"
        compose exec -T ferroma ferroma database status \
            || warn "that failed too — read the container log for the migration error"
    fi
    first_run
    dkim_key
    if [ "$ENABLE_DKIM" = 1 ]; then
        dkim_enable_signing
    elif [ -t 0 ] && [ "$(env_get FERROMA_DKIM_ENABLED)" != "true" ] && confirm "Sign outbound mail with this key now?"; then
        dkim_enable_signing
    else
        info "signing is off until the record is live: ./scripts/deploy.sh dkim --enable"
    fi
    summary
}

cmd_status() {
    preflight
    load_defaults
    step "Containers"
    compose ps
    printf '\n'
    step "Health"
    info "ferroma    $(docker inspect -f '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}no-healthcheck{{end}} restarts={{.RestartCount}}' "$APP_CONTAINER" 2>/dev/null || echo 'not created')"
    printf '\n'
    step "Database"
    if container_running "$APP_CONTAINER"; then
        compose exec -T ferroma ferroma database status || true
    else
        warn "ferroma is not running; skipping"
    fi
    printf '\n'
    step "Listening"
    for _port in 25 587 465 143 993 "$API_PORT"; do
        if listening_on "$_port"; then info "$_port  listening"; else info "$_port  -"; fi
    done
}

cmd_logs() {
    preflight
    compose logs -f --tail=200 ferroma
}

cmd_upgrade() {
    preflight
    load_defaults
    [ -f "$ENV_FILE" ] || die "no .env yet: run ./scripts/deploy.sh first"
    REBUILD=1
    build_image
    check_privileged_ports
    start_stack
    wait_healthy
    step "Done"
    compose ps
}

cmd_dkim() {
    preflight
    load_defaults
    [ -n "$FERROMA_DOMAIN" ] || die "no domain known yet; run ./scripts/deploy.sh first"
    container_running "$APP_CONTAINER" || die "ferroma is not running"
    dkim_key
    if [ "$ENABLE_DKIM" = 1 ]; then
        dkim_enable_signing
    fi
}

cmd_doctor() {
    preflight
    load_defaults
    container_running "$APP_CONTAINER" || die "ferroma is not running"
    compose exec -T ferroma ferroma doctor || true
}

# A renewed certificate has to be re-installed *and* re-read: the server loads the
# PEM once, when it starts, and never watches the file. This is the command a
# certbot deploy hook calls, so renewal needs no human.
cmd_certs() {
    preflight
    load_defaults
    [ -n "$FERROMA_HOSTNAME" ] || die "no hostname known yet; run ./scripts/deploy.sh first"

    TLS_CERT_SRC=""
    TLS_KEY_SRC=""
    if [ -n "$TLS_CERT_ARG" ]; then
        TLS_CERT_SRC="$TLS_CERT_ARG"
        TLS_KEY_SRC="$TLS_KEY_ARG"
        [ -n "$TLS_KEY_SRC" ] || die "--tls-cert needs --tls-key"
    elif [ -f "/etc/letsencrypt/live/$FERROMA_HOSTNAME/fullchain.pem" ]; then
        TLS_CERT_SRC="/etc/letsencrypt/live/$FERROMA_HOSTNAME/fullchain.pem"
        TLS_KEY_SRC="/etc/letsencrypt/live/$FERROMA_HOSTNAME/privkey.pem"
    fi
    # With no source found, install_tls keeps whatever ./tls already holds — still
    # worth doing, because the restart below is what picks up a replaced file.

    NO_TLS=0
    install_tls

    step "Reloading the listeners"
    # `up -d` applies an .env change (TLS switched on, paths moved); the restart is
    # what actually re-reads the PEM.
    compose up -d ferroma
    compose restart ferroma
    wait_healthy
    info "the new certificate is in use; SMTP and IMAP clients will see it on their next connection"
}

cmd_down() {
    preflight
    if [ "$DOWN_VOLUMES" = 1 ]; then
        warn "--volumes deletes the mail store and the users."
        confirm "Really delete all of it?" || die "aborted"
        compose down --volumes
        info "volume deleted: ferroma-data"
    else
        compose down
        info "volume kept: ferroma-data"
    fi
}

case "$COMMAND" in
    deploy)  cmd_deploy ;;
    status)  cmd_status ;;
    logs)    cmd_logs ;;
    upgrade) cmd_upgrade ;;
    dkim)    cmd_dkim ;;
    certs)   cmd_certs ;;
    doctor)  cmd_doctor ;;
    down)    cmd_down ;;
    help)    usage ;;
    *)       usage; exit 2 ;;
esac
