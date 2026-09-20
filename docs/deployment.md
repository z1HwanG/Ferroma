# Deployment

**Who should read this:** the operator standing up a Ferroma server, and the one
who gets paged when it stops delivering mail.

This document is the complete operational path: the DNS records to create before
the first boot, which compose file to use and when, the environment variables that
must be set, the port table, the TLS options (Ferroma-terminated versus a reverse
proxy) and how to get a Let's Encrypt certificate, first-run setup, creating
domains and users, generating and publishing a DKIM key, the backup and restore
procedure you run with the host's own tooling and why the two halves belong
together, monitoring and health checks, upgrades and
rollback, and a troubleshooting section keyed by symptom with the command that
diagnoses each one.

> **Status:** the deployment artefacts are real and complete —
> `Dockerfile`, `docker-compose.yml`, `docker-compose.prod.yml`,
> `docker-compose.external-db.yml`, `.env.example`, `config/ferroma.toml`,
> `scripts/deploy.sh`.
> The `ferroma` binary implements every subcommand this document uses: `serve`,
> `config check|show|default`, `database init|status`, `migrate`, `user`, `domain`,
> `dkim`, `storage`, `sync`, `healthcheck`, `doctor`, `version`.
>
> **There is exactly one exception: the HTTPS listener.** `api.tls_port` (default
> 8443) is read by the configuration but is **never bound** — the HTTP API serves
> plaintext only (`api.port`, default 8080). Webmail / Admin / API HTTPS must be
> terminated by a reverse proxy, see §5.3 and §5.4. SMTP and IMAP implicit TLS
> (465 / 993) *is* terminated by Ferroma itself, but note that `smtps_port` /
> `imaps_port` default to `0` (off) and must be enabled explicitly.

---

## 1. What a deployment consists of

```text
                       Internet
                           │
        ┌──────────────────┼───────────────────┬──────────────┐
        │                  │                   │              │
      :25                :587/:465           :143/:993      :443
    inbound MX         submission           IMAP           HTTPS (API,
        │                  │                   │            Webmail, Admin)
        └──────────────────┴───────────────────┘              │
                           │                                  │
                  ┌────────▼──────────────────────────────────▼────────┐
                  │                  ferroma container                 │
                  │  one process: SMTP, IMAP, HTTP API, queue workers, │
                  │  sync service, event bus                          │
                  └────────┬───────────────────────────────┬──────────┘
                           │                               │
                  ┌────────▼────────┐            ┌─────────▼──────────┐
                  │ postgres:16     │            │ volume ferroma-data│
                  │ (internal only) │            │  mail/ attachments/│
                  └─────────────────┘            │  dkim/ private key │
                                                 │  database.json     │
                                                 └────────────────────┘
```

Two containers minimum. PostgreSQL is never published to the host: it is on
`ferroma-internal` and reachable only by the `ferroma` service.

> **If this host already runs PostgreSQL** (and a reverse proxy for 443), do not
> start a second database container: use `docker-compose.external-db.yml` and
> `./scripts/deploy.sh` from §3.1 — that is Ferroma itself and the database you
> already have; backing it up is your own job, see §8.

Requirements before you start:

| Requirement | Why |
|---|---|
| A host with a static public IPv4 address | an MX needs a stable address, and the PTR record must match it |
| Port 25 reachable **inbound** | receiving mail from other servers. Many VPS providers block it by default — ask them to unblock it before you begin |
| Port 25 reachable **outbound** | delivering mail. Some providers block outbound 25 to force you through a relay |
| A domain you control | `example.com` below |
| Docker Engine 24+ with the Compose plugin | `docker compose`, not `docker-compose` |
| ~4 GB RAM, 2 vCPU, 20 GB disk | enough for a small deployment; the mail store grows |

---

## 2. DNS records

**Do this first.** Specification §42 and §54: a perfectly configured Ferroma with
no DNS records will have all of its outbound mail rejected and will receive
nothing. Create every record below before the first boot.

Throughout, the example zone is `example.com` and the server is
`mail.example.com` at `203.0.113.10`. Replace both.

### 2.1 The records, as a table

| Type | Name | Value | TTL | Purpose |
|---|---|---|---|---|
| `A` | `mail.example.com` | `203.0.113.10` | 3600 | the server's address |
| `AAAA` | `mail.example.com` | `2001:db8::10` | 3600 | optional; omit if you have no working IPv6, a broken AAAA breaks delivery |
| `MX` | `example.com` | `10 mail.example.com.` | 3600 | where mail for the domain goes |
| `PTR` | `10.113.0.203.in-addr.arpa` | `mail.example.com.` | 3600 | reverse DNS — set at your hosting provider, not in your own zone |
| `TXT` | `example.com` | `"v=spf1 mx -all"` | 3600 | SPF: only this host may send |
| `TXT` | `default._domainkey.example.com` | `"v=DKIM1; k=rsa; p=…"` | 3600 | DKIM public key, from §7 |
| `TXT` | `_dmarc.example.com` | `"v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com; adkim=r; aspf=r"` | 3600 | DMARC policy and reporting |
| `TXT` | `_mta-sts.example.com` | `"v=STSv1; id=20260916000000"` | 3600 | MTA-STS policy version |
| `CNAME` | `mta-sts.example.com` | `mail.example.com.` | 3600 | where the policy file is served |
| `TXT` | `_smtp._tls.example.com` | `"v=TLSRPTv1; rua=mailto:tlsrpt@example.com"` | 3600 | TLS reporting (optional but useful) |
| `CAA` | `example.com` | `0 issue "letsencrypt.org"` | 3600 | only this CA may issue certificates |

### 2.2 A BIND-style zone snippet

```bind
$TTL 3600
$ORIGIN example.com.

; --- address ---
@               IN  A       203.0.113.10
mail            IN  A       203.0.113.10
; Only publish AAAA if IPv6 genuinely works end to end. A host that
; advertises AAAA and cannot answer on it loses mail from dual-stack senders.
; mail          IN  AAAA    2001:db8::10

; --- mail routing ---
@               IN  MX  10  mail.example.com.

; A "null MX" says this domain accepts no mail. Do not publish it on a
; domain that has users:
; @             IN  MX  0   .

; --- sender authentication ---
@               IN  TXT     "v=spf1 mx -all"

; DKIM: paste the p= value from `ferroma dkim generate` / the Admin console.
default._domainkey IN TXT  "v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA..."

; DMARC. Start at p=none to collect reports, then tighten.
_dmarc          IN  TXT     "v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com; ruf=mailto:dmarc@example.com; adkim=r; aspf=r; pct=100"

; --- transport security ---
_mta-sts        IN  TXT     "v=STSv1; id=20260916000000"
mta-sts         IN  CNAME   mail.example.com.
_smtp._tls      IN  TXT     "v=TLSRPTv1; rua=mailto:tlsrpt@example.com"

; --- issuance control ---
@               IN  CAA     0 issue "letsencrypt.org"
@               IN  CAA     0 iodef "mailto:security@example.com"

; --- optional service hostnames (specification §42) ---
; webmail       IN  CNAME   mail.example.com.
; admin         IN  CNAME   mail.example.com.
```

### 2.3 The PTR record

Reverse DNS is set at whoever owns the IP block — your VPS provider's control
panel, usually. An MX with no PTR, or a PTR that does not match the name in the
`EHLO`, is the single most common reason legitimate mail is rejected or junked.

```bash
# What the world sees for your address. It must equal FERROMA_HOSTNAME.
dig +short -x 203.0.113.10
# expected: mail.example.com.
```

Set `FERROMA_HOSTNAME=mail.example.com` and make the PTR match it exactly,
including that the forward `A` record for that name points back at the same
address. The round trip is what a receiver checks:

```bash
dig +short mail.example.com        # -> 203.0.113.10
dig +short -x 203.0.113.10         # -> mail.example.com.
```

### 2.4 MX priority and a backup MX

A single MX with preference 10 is correct for one server. If you ever add a
second, give it a higher preference number — lower is preferred:

```bind
@   IN  MX  10  mail.example.com.
@   IN  MX  20  mail2.example.com.
```

Do not add a second MX that does not have the same mailbox data. A backup MX that
accepts mail it cannot deliver is worse than no backup MX: the sender believes the
message was accepted.

### 2.5 SPF: getting it right

| Record | Meaning |
|---|---|
| `v=spf1 mx -all` | hosts in this domain's MX records may send. The usual choice for a mail server that is its own MX |
| `v=spf1 a:mail.example.com -all` | an explicit host, when the MX is elsewhere |
| `v=spf1 mx ip4:203.0.113.10 -all` | belt and braces |
| `v=spf1 mx ~all` | softfail: mark as suspicious rather than reject. Use while migrating |
| `v=spf1 mx ?all` | neutral: SPF proves nothing. Do not ship this |
| `v=spf1 mx include:_spf.other.example -all` | add a third party that also sends for the domain |

Rules that matter:

* **One SPF record, ever.** Two `v=spf1` TXT records on the same name is a
  permanent error and receivers treat it as `permerror`. If you need to merge,
  merge into one string.
* **`-all` at the end.** Without an `all` mechanism, every host that is not
  matched gets a neutral result and SPF stops protecting you.
* **Ten DNS lookups maximum** (RFC 7208 §4.6.4), which is also
  `policy.spf_max_lookups`. Over that, receivers return `permerror`. Count your
  `include:` and `mx` mechanisms.
* **Subdomains do not inherit.** If you send from `news.example.com`, it needs
  its own record or a `redirect=`/`include:` from the parent.

```bash
# Check what receivers will see.
dig +short TXT example.com | grep spf1
```

### 2.6 MTA-STS

MTA-STS tells senders to require TLS to your domain and to validate your
certificate, which closes the opportunistic-downgrade hole described in
[security.md](security.md) §7.4. It needs both a TXT record and an HTTPS file.

```bind
_mta-sts    IN  TXT     "v=STSv1; id=20260916000000"
mta-sts     IN  CNAME   mail.example.com.
```

```text
# Served at https://mta-sts.example.com/.well-known/mta-sts.txt
version: STSv1
mode: enforce
mx: mail.example.com
max_age: 604800
```

| Field | Values | Notes |
|---|---|---|
| `mode` | `none`, `testing`, `enforce` | start at `testing` for a week and read your TLS reports, then `enforce` |
| `mx` | one line per MX host | must list every MX, or a sender will refuse the ones you omitted |
| `max_age` | seconds | how long a sender may cache the policy; 604800 is a week |

Changing the policy means changing the `id` in the TXT record. A sender caches by
`id`, so an unchanged `id` with a changed file is ignored.

`GET /.well-known/mta-sts.txt` is served by `ferroma-api` ([api.md](api.md) §2).
Point the `CNAME` for `mta-sts.<domain>` at this host and let the reverse proxy
forward that path to Ferroma: the nginx snippet in §5.4 proxies everything, so no
extra rule is needed. The certificate must cover `mta-sts.<domain>` (§5.5).

### 2.7 DMARC: roll it out gradually

| Stage | Record | What you learn |
|---|---|---|
| 1. Monitor | `v=DMARC1; p=none; rua=mailto:dmarc@example.com` | who sends as your domain, including relays you forgot about |
| 2. Quarantine | `v=DMARC1; p=quarantine; pct=25; rua=…` | tightens slowly, so a missed sender only affects a quarter of its mail |
| 3. Enforce | `v=DMARC1; p=reject; rua=…` | the target state |

Ferroma's own inbound policy mirrors this: `policy.dmarc_failure_action` defaults
to `"quarantine"`, not `"reject"`, because a forwarded message routinely fails
SPF and DKIM and is still legitimate ([security.md](security.md) §8.1).

Read the `rua` reports. A DMARC report you never open is `p=none` forever, which
is the same as having no policy and thinking you have one.

### 2.8 Verifying the zone

```bash
# Everything at once.
dig +short MX  example.com
dig +short A   mail.example.com
dig +short -x  203.0.113.10
dig +short TXT example.com              | grep spf1
dig +short TXT default._domainkey.example.com
dig +short TXT _dmarc.example.com
dig +short TXT _mta-sts.example.com
curl -s https://mta-sts.example.com/.well-known/mta-sts.txt

# Ask a DNSBL whether your IP is already listed (use a real one, this is illustrative).
# 203.0.113.10 is documentation-reserved and will never be listed.
dig +short 10.113.0.203.zen.spamhaus.org
```

The Admin console's DNS Health screen (`GET /api/v1/domains/:id/dns`,
[api.md](api.md) §4.4) runs exactly these checks and returns a score out of 7.

---

## 3. The three compose files

| File | Use it for | TLS | Postgres | Images | Extra |
|---|---|---|---|---|---|
| `docker-compose.external-db.yml` | **a host that already runs PostgreSQL and a reverse proxy** (recommended, see §3.1) | the reverse proxy terminates HTTPS; Ferroma terminates 465/993 | **none** — it uses the host's Postgres | `wesukilaye/ferroma:<tag>`, `docker pull`ed or built here — the whole reference is `FERROMA_IMAGE` | `network_mode: host`, `.env` is the whole configuration; backups stay yours, see §8 |
| `docker-compose.yml` | development, a single host, a first look | off; ports 25/587/143/8080 plaintext | `postgres:16-alpine`, defaults | built locally from `Dockerfile`, tagged `wesukilaye/ferroma:dev` | — |
| `docker-compose.prod.yml` | a real MX | terminated by Ferroma on 465/993 (HTTPS goes to the reverse proxy) | tuned (`shared_buffers=512MB`, `wal_compression=on`, …) | `wesukilaye/ferroma:${FERROMA_VERSION}` — a released tag, never built | resource limits, `restart: always`, bounded logs, `ulimit nofile 65536`; backups stay yours, see §8 |

Both reference the published repository through `FERROMA_REPO`, which defaults to
`wesukilaye/ferroma` and can point at a mirror or a private registry instead. The
repository and the tag are two separate variables on purpose: Compose does **not**
interpolate a `${…}` nested inside another, so a nested default such as
`${FERROMA_IMAGE:-${FERROMA_REPO}:${FERROMA_VERSION}}` interpolates to a bare
`:`-terminated string rather than to an image reference. `tools/check-deploy.mjs`
fails the build if one reappears.

Do not use `docker-compose.yml` in production. It serves IMAP and the API in
plaintext, has no resource limits, and it binds port 143 to the host unencrypted.

### 3.1 A host that already runs PostgreSQL: one command (recommended)

If this Linux host **already runs PostgreSQL** and **already has a reverse proxy**
(nginx, Caddy, …) owning 443, do not start a second database container: use
`docker-compose.external-db.yml`, driven by `scripts/deploy.sh`. It is the route
with the fewest steps and the smallest change to the environment you already have.

```bash
git clone … && cd Ferroma
./scripts/deploy.sh
```

It does the following, in order. If any step fails it prints the exact command that
fixes it rather than leaving you to guess:

| Step | What it does |
|---|---|
| 1. Preflight | are `docker`, the compose plugin and the compose files all present |
| 2. Collect configuration | asks interactively: mail domain, MX hostname, admin address, database address, API port (default `127.0.0.1:18080`) |
| 3. Write `.env` | generates a random database password and `FERROMA_JWT_SECRET`, mode 600; **it is the only configuration file** |
| 4. Create the role and the database | tries, in order: `sudo -u postgres` (peer auth), the `psql` **inside a PostgreSQL container on this host** (how 1Panel and similar panels run it, through `docker exec`), and the superuser named by `--pg-password`; if none works it prints SQL you can paste — in the `docker exec` form when the database is a container |
| 5. Build the image | a local `docker build` (10–30 minutes the first time). Pass `--image wesukilaye/ferroma:0.1.5` to pull the release instead — the same command skips the build entirely |
| 6. Create the schema | runs `ferroma database init` in the container (which also creates the database when it is missing) |
| 7. Install the certificate | installs the certificate into `./tls` as uid 10001 for 465/993, and checks that the SAN covers the MX hostname |
| 8. Start | `docker compose up -d`, waiting up to 3 minutes for the health check and printing the log on timeout |
| 9. First-run initialisation | creates the domain and the admin account (the password is printed once), generates the DKIM key and prints the TXT record to publish |
| 10. Summary | the reverse-proxy snippet, the DNS records still missing, and the everyday commands |

The trade-offs that matter:

* **It does not touch your PostgreSQL configuration.** The container uses
  `network_mode: host`, so the local database is simply `127.0.0.1:5432`: no
  `listen_addresses` change, no Docker subnet to add to `pg_hba.conf`, no
  `host-gateway`. The side effect is a good one — SMTP and IMAP see the client's
  real source IP, which the login throttle and the logs both rely on.
* **The low ports need one privilege.** 25, 587 and 143 are below 1024. In a bridge
  network the kernel sets `net.ipv4.ip_unprivileged_port_start` to 0, so uid 10001
  may bind them; `network_mode: host` shares the *host's* namespace, where it is
  usually 1024. The image therefore grants the binary `NET_BIND_SERVICE` as a file
  capability (the `setcap` step in `Dockerfile`). If that capability does not
  survive your build, the script notices before starting and tells you the one line
  that fixes it: `sudo sysctl -w net.ipv4.ip_unprivileged_port_start=0`.
* **`.env` is the whole configuration.** This stack mounts no `ferroma.toml`, so
  every setting is an environment override (the double-underscore form, like
  `FERROMA__SMTP__PORT=2525`); `docker compose … up -d` applies it.
* **HTTPS is still your reverse proxy's job.** Ferroma serves its plaintext API on
  `127.0.0.1:18080` only, and the public-facing TLS is terminated by the proxy —
  exactly what §5.3 and §5.4 describe. When the proxy itself runs in a container, or
  the public port is not 443 (container on 80/443, host publishing 180/1443), read
  the last part of §5.4: the API has to be bound to the Docker bridge address and
  `--public-port` has to be passed with it.
* **Backups are yours.** Nothing in this deployment backs anything up: no sidecar,
  no timer, no script, and `scripts/deploy.sh` has no `backup` or `restore`
  subcommand. Dump PostgreSQL and copy the `ferroma-data` volume with the host's own
  tooling, and keep the two halves together — §8 has the commands and says what a
  backup must contain.

The sub-commands you will use:

```bash
./scripts/deploy.sh                     # first deployment, or re-apply .env and restart
./scripts/deploy.sh status              # containers / health / database
./scripts/deploy.sh logs                # follow the Ferroma log
./scripts/deploy.sh upgrade             # rebuild the image → restart → wait for health
./scripts/deploy.sh dkim --enable       # turn signing on after the TXT record is published
./scripts/deploy.sh certs               # re-install a renewed certificate and restart (used by the certbot hook)
./scripts/deploy.sh doctor              # run `ferroma doctor` inside the container
./scripts/deploy.sh down [--volumes]    # stop the stack; --volumes also deletes ferroma-data
./scripts/deploy.sh help                # the usage text
```

An unattended run (cloud-init, CI) has a flag for every prompt:

```bash
./scripts/deploy.sh --yes --domain example.com --admin admin@example.com \
  --db-password "$DB_PW" \
  --tls-cert /etc/letsencrypt/live/mail.example.com/fullchain.pem \
  --tls-key  /etc/letsencrypt/live/mail.example.com/privkey.pem
```

With no certificate the script turns TLS off and warns explicitly: there is then no
STARTTLS on 587, mail clients cannot authenticate, and it is only good for getting
the database and the API up.

### 3.2 Development / single host

```bash
cp .env.example .env
# Edit .env: at minimum POSTGRES_PASSWORD and FERROMA_JWT_SECRET.
docker compose up -d
docker compose logs -f ferroma
```

What it brings up: `postgres` (internal network only, `expose: 5432`) and
`ferroma` (ports 25, 587, 143, 8080; volumes `ferroma-data`, `./config/ferroma.toml`
mounted read-only, `./tls` mounted read-only).

### 3.3 Production (with its own database)

```bash
cp .env.example .env
# Edit .env and set every REQUIRED variable: see §4.
docker compose -f docker-compose.prod.yml pull
docker compose -f docker-compose.prod.yml up -d
docker compose -f docker-compose.prod.yml ps
docker compose -f docker-compose.prod.yml logs -f ferroma
```

Differences that matter operationally:

```yaml
FERROMA_TLS_ENABLED: 'true'
FERROMA_TLS_CERT: /etc/ferroma/tls/fullchain.pem
FERROMA_TLS_KEY: /etc/ferroma/tls/privkey.pem
FERROMA__API__SECURE_COOKIES: 'true'
FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH: 'true'
FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN: 'true'
# The implicit-TLS listeners are off by default (0). Without these two lines the
# 465/993 published by compose map to a port nobody listens on — the connection
# is refused rather than reporting any error.
FERROMA__SMTP__SMTPS_PORT: '465'
FERROMA__IMAP__IMAPS_PORT: '993'
FERROMA_DKIM_ENABLED: ${FERROMA_DKIM_ENABLED:-false}
FERROMA_DKIM_KEY: /etc/ferroma/dkim/${FERROMA_DKIM_SELECTOR:-default}.private
```

It publishes no HTTPS port at all: Ferroma has no HTTPS listener (see the status
note at the top of this file), and Webmail / Admin / API are reached by the reverse
proxy at `127.0.0.1:8080` — add `127.0.0.1:8080:8080` to `ports` to do that, see
§5.3.

Nothing backs this stack up for you, and there is no `backup` service to run. The
mail store lives in the `ferroma-data` volume and the relational state in the
`ferroma-postgres-data` volume owned by the `postgres` container; §8 has the
commands that dump the one and archive the other, and both halves are needed
together.

### 3.4 Operating commands you will actually type

```bash
# Follow the log of one service.
docker compose -f docker-compose.prod.yml logs -f --tail=200 ferroma

# Restart just Ferroma (PostgreSQL keeps running).
docker compose -f docker-compose.prod.yml restart ferroma

# A shell inside the container, as the ferroma user.
docker compose -f docker-compose.prod.yml exec ferroma sh

# A psql session against the database.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma

# Disk usage of the two volumes.
docker system df -v | grep -E 'ferroma-data|ferroma-postgres-data'

# Stop everything, keeping the volumes.
docker compose -f docker-compose.prod.yml down

# Stop everything and DESTROY the volumes. This deletes all mail and users.
# docker compose -f docker-compose.prod.yml down -v
```

---

### 3.5 Choosing the database in the browser

A server that cannot reach PostgreSQL has nothing to serve — no repositories, no session,
not even the page that would let you fix it. So it does not exit: when no connection was
*stated* (no `FERROMA_DATABASE__URL`, nothing in `ferroma.toml`, nothing in
`<data_dir>/database.json`), it binds the web port anyway and serves one page, at
`http://<host>:<port>/`, that asks for the connection.

That address is also where the rest of setting up happens. While an instance still owes an
administrator — no database, or a database with no administrator in it — `/` serves the Admin
console, and the console shows whichever step is outstanding: the form that asks for the
connection and the code, then the first-run wizard. So opening the mail hostname always lands
on the step that is owed rather than on a sign-in box for an account that does not exist yet,
and initialisation is one address from beginning to end. `/admin/` serves the same console
throughout (releases before 0.1.5 printed that URL, and it is worth keeping working); once an
administrator exists, `/` is the Webmail, which is what a mail hostname is for.

```
No database is connected yet. Open http://0.0.0.0:8080/ and enter:

    address   postgres://user:password@host:5432/ferroma
    code      7JVTQAHO
```

* **The code is required.** It is generated at every start and printed to the log, because
  otherwise anyone who can reach the published web port could point this instance at a
  database of their choosing. Read it with `docker compose logs ferroma`.
* **The database must already exist.** Ferroma connects and applies its schema; it never
  runs `CREATE DATABASE`, so the role you name only needs the rights to use that database.
* **There is no restart.** The connection is tested, the schema applied, and the boot
  continues in the same process on the same port — the page reloads straight into the
  first-run wizard.
* **The address is remembered** in `<data_dir>/database.json` (mode 0600: it holds a
  password) and used on every later start. Leave `DATABASE_URL` out of `.env` to get this
  behaviour; state it and the deployment wins, as everywhere else.

A remembered address that stops working brings the page back, with the server's own
explanation of why it was refused — that is a wrong password you can fix from the browser.
An address stated by the deployment is still a hard, loud failure at startup, because that
one is a mistake in a file you can see.


## 4. Environment variables

Copy `.env.example` to `.env`. Compose interpolates it, and the compose files
refuse to start without the required ones.

### 4.1 Required in production

| Variable | Example | Required by | Notes |
|---|---|---|---|
| `POSTGRES_PASSWORD` | `openssl rand -base64 32` | all | `${POSTGRES_PASSWORD:?…}` — compose fails without it |
| `FERROMA_JWT_SECRET` | `openssl rand -base64 48` | optional | signs access/refresh tokens. Leave it unset and the server generates one into the data volume and reuses it; set it only to share the secret across instances |
| `FERROMA_HOSTNAME` | `mail.example.com` | optional | must equal the PTR record. Absent, the first-run wizard asks for it and the stored value is adopted at the next start |
| `FERROMA_PUBLIC_URL` | `https://mail.example.com` | optional | used in `.well-known/ferroma` and in links. Same arrangement as the hostname: the wizard owns it unless the deployment states it |

Stating any of these in the environment wins over the wizard, which is the point: a
deployment that knows its identity sets it once, and an instance being set up by hand gets
asked. `scripts/deploy.sh --wizard` writes none of them, so a fresh container needs only
the web port published — plus `POSTGRES_PASSWORD`, which the script generates.
| `FERROMA_VERSION` | `0.1.5` | prod (`:?`) | a released image tag; prod never builds |

### 4.2 Commonly set

| Variable | Default | Maps to |
|---|---|---|
| `POSTGRES_USER` | `ferroma` | the database role |
| `POSTGRES_DB` | `ferroma` | the database name |
| `FERROMA_REPO` | `wesukilaye/ferroma` | the repository releases are pulled from — another Docker Hub namespace, a mirror, or a private registry |
| `FERROMA_IMAGE` | `wesukilaye/ferroma:latest` (external-db only) | the whole image reference. A value containing `/` is pulled; one without is built on this host |
| `FERROMA_LOG_LEVEL` | `info` | `server.log_level` |
| `FERROMA_LOG_FORMAT` | `text` (dev) / `json` (prod) | `server.log_format` |
| `FERROMA_TLS_ENABLED` | `false` | `tls.enabled` |
| `FERROMA_TLS_CERT` | — | `tls.cert_path` |
| `FERROMA_TLS_KEY` | — | `tls.key_path` |
| `FERROMA_DKIM_ENABLED` | `false` | `dkim.enabled` |
| `FERROMA_DKIM_SELECTOR` | `default` | `dkim.selector` |
| `FERROMA_DKIM_KEY` | — | `dkim.private_key_path` |
| `TRUST_PROXY_HEADERS` | `false` | `api.trust_proxy_headers` |
| `SMTP_PORT`, `SUBMISSION_PORT`, `IMAP_PORT`, `HTTP_PORT` | 25, 587, 143, 8080 | **dev only** — the host side of the published ports |
| `FERROMA_API_PORT` | 8080 (18080 in the external-db stack) | `api.port` — the host side of the plaintext HTTP API |

### 4.3 The generic override form

Any setting can be overridden without editing `ferroma.toml`, using a double
underscore as the path separator:

```bash
FERROMA__SMTP__PORT=2525
FERROMA__TLS__ENABLED=true
FERROMA__API__SECURE_COOKIES=true
FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH=true
FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN=true
FERROMA__API__TRUST_PROXY_HEADERS=true
FERROMA__LIMITS__MAX_MESSAGE_SIZE=52428800
```

Precedence, from `Config::load`: embedded defaults < `ferroma.toml` < environment.
Values are type-coerced against the default at that path, so `2525` becomes an
integer and `true` becomes a boolean. **Unknown keys are rejected** — a typo stops
the server at boot rather than silently leaving a limit disabled, which is the
behaviour you want.

The shorthand aliases (`config.rs`, the `ALIASES` table):

```text
DATABASE_URL                  FERROMA_HOSTNAME        FERROMA_DATA_DIR
FERROMA_LOG_LEVEL             FERROMA_LOG_FORMAT      FERROMA_SMTP_HOST
FERROMA_SMTP_PORT             FERROMA_SMTP_SUBMISSION_PORT
FERROMA_IMAP_PORT             FERROMA_API_HOST        FERROMA_API_PORT
FERROMA_API_PUBLIC_URL        FERROMA_JWT_SECRET      FERROMA_TLS_ENABLED
FERROMA_TLS_CERT              FERROMA_TLS_KEY         FERROMA_DKIM_ENABLED
FERROMA_DKIM_KEY              FERROMA_DKIM_SELECTOR
```

`FERROMA_CONFIG` points at the config file; the image sets it to
`/etc/ferroma/ferroma.toml`.

### 4.4 Secrets

```bash
# Generate both, then put them in .env. Never commit .env.
openssl rand -base64 32      # POSTGRES_PASSWORD
openssl rand -base64 48      # FERROMA_JWT_SECRET
```

`.env` is gitignored. Anything that copies the `ferroma-data` volume is
**secret-bearing** and deserves the same care: it holds the DKIM private key at
`/var/lib/ferroma/dkim/<selector>.private` and, on a deployment that connected the
database through the browser (§3.5), the database address and password inside
`<data_dir>/database.json` (mode 0600). Encrypt the archive or snapshot at rest,
restrict who can read it, and keep `*.env` and `credentials*` out of any copy of the
configuration. See [security.md](security.md) §13.

---

## 5. Ports and TLS

### 5.1 Port table

| Port | Service | Config key | Dev | Prod | Notes |
|---|---|---|---|---|---|
| 25 | SMTP inbound (MX) | `smtp.port` | published | published | **must** be reachable from the internet |
| 587 | Submission, `STARTTLS` | `smtp.submission_port` | published | published | for your users' mail clients |
| 465 | SMTPS (implicit TLS) | `smtp.smtps_port` | not published | published | requires `tls.enabled` **and** `smtps_port = 465` (the default is 0, i.e. off) |
| 143 | IMAP, `STARTTLS` | `imap.port` | published | published | |
| 993 | IMAPS (implicit TLS) | `imap.imaps_port` | not published | published | requires `tls.enabled` **and** `imaps_port = 993` (the default is 0, i.e. off) |
| 8080 | HTTP API + Webmail + Admin | `api.port` | published | loopback only | the health check runs against it; the reverse proxy terminates HTTPS on the public side |
| 8443 | *_(not implemented)_* | `api.tls_port` | — | — | the key exists in the configuration but there is **no listener**: HTTPS for the API / Webmail / Admin belongs to the reverse proxy, see §5.3 |
| 5432 | PostgreSQL | — | `expose` only | `expose` only | never publish this |

`EXPOSE 25 587 465 143 993 8080` in the `Dockerfile`. `Config::validate()` refuses
to start when two active SMTP ports collide, when `smtp.port` is `0`, or when a
TLS port is configured while `tls.enabled = false`.

### 5.2 Option A — Ferroma terminates SMTP/IMAP TLS

What `docker-compose.prod.yml` does: 465 and 993 are served by rustls directly
(the `network_mode: host` of `docker-compose.external-db.yml` is the same), and
the PEM bundle and key are mounted read-only. HTTPS is not part of it — see the
status note at the top of this file. Both implicit-TLS listeners are off by
default, so turn them on explicitly:

```bash
FERROMA__SMTP__SMTPS_PORT=465
FERROMA__IMAP__IMAPS_PORT=993
```

```bash
mkdir -p tls dkim
# Certificate and key, however you obtained them.
ls -l tls/fullchain.pem tls/privkey.pem
```

```bash
# In .env
FERROMA_TLS_ENABLED=true
FERROMA_TLS_CERT=/etc/ferroma/tls/fullchain.pem
FERROMA_TLS_KEY=/etc/ferroma/tls/privkey.pem
```

`tls.cert_path` is a bundle: the leaf certificate first, then intermediates.
`tls.key_path` is PKCS#8 or PKCS#1. If you set one and not the other,
`Config::validate()` refuses to boot:

```text
tls.cert_path and tls.key_path must be set together (or enable self_signed_fallback)
```

`tls.self_signed_fallback = true` generates a certificate with `rcgen` at boot
when no PEM is configured. It is for local development and CI only, and it is
gated behind `tls.allow_insecure_dev_mode = true` — an MX with a self-signed
certificate cannot be validated by any sending server, so all its outbound TLS
fails.

### 5.3 Option B — a reverse proxy terminates HTTPS

Use this when something else already owns 443 and manages certificates, or when
you want one place for HTTP security headers. SMTP and IMAP are **not** proxied;
Ferroma still terminates those itself. `docker-compose.external-db.yml` from §3.1
is exactly this shape: it uses `network_mode: host`, so there are no port mappings
at all and the API listens on `127.0.0.1:18080` directly (`FERROMA_API_HOST` /
`FERROMA_API_PORT`).

```yaml
# Add to docker-compose.prod.yml's ferroma service.
    ports:
      - '25:25'
      - '587:587'
      - '465:465'
      - '143:143'
      - '993:993'
      - '127.0.0.1:8080:8080'      # HTTP, bound to loopback: the proxy reaches it
    environment:
      FERROMA_TLS_ENABLED: 'true'  # still needed for 465 and 993
      FERROMA__SMTP__SMTPS_PORT: '465'
      FERROMA__IMAP__IMAPS_PORT: '993'
      FERROMA__API__TRUST_PROXY_HEADERS: 'true'
      FERROMA__API__SECURE_COOKIES: 'true'
```

`api.trust_proxy_headers = true` makes Ferroma believe `X-Forwarded-For` and
`X-Real-IP`. **Turn it on only when a proxy you control sets them**: with it on and
no proxy, a client can forge its own source IP and defeat the per-IP login
throttle and connection limits. The proxy must overwrite the header, not append
to it. Binding the API to a loopback address (`FERROMA_API_HOST=127.0.0.1`) is the
other half of the same trust: nothing outside this machine can forge those headers
directly.

### 5.4 nginx in front

```nginx
server {
    listen 443 ssl http2;
    server_name mail.example.com mta-sts.example.com;

    ssl_certificate     /etc/letsencrypt/live/mail.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/mail.example.com/privkey.pem;
    ssl_protocols       TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers off;

    add_header Strict-Transport-Security "max-age=31536000" always;
    add_header X-Content-Type-Options nosniff always;
    add_header X-Frame-Options DENY always;
    add_header Referrer-Policy no-referrer always;

    client_max_body_size 30m;      # >= api.max_request_size and limits.max_message_size

    # The MTA-STS policy is served by ferroma-api, so forward it (point the CNAME
    # for mta-sts.<domain> at this host, and cover it in the certificate too).
    location /.well-known/mta-sts.txt {
        proxy_pass http://127.0.0.1:18080;
        proxy_set_header Host $host;
    }

    location / {
        proxy_pass http://127.0.0.1:18080;   # the prod stack uses 8080; §3.1's stack defaults to 18080
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $remote_addr;   # overwrite, never append
        proxy_set_header X-Forwarded-Proto $scheme;

        # The realtime socket needs the upgrade dance and a long read timeout.
        proxy_set_header Upgrade    $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
    }
}
```

`X-Forwarded-For $remote_addr` rather than `$proxy_add_x_forwarded_for` is
deliberate: appending lets a client prepend its own value, and Ferroma would read
the first one.

#### When the proxy is itself a container, or the public port is not 443

The two usually arrive together: nginx runs in a container, still listening on 80
and 443 *inside* it, and the host publishes those as 180 and 1443 because the
host's own 80 and 443 belong to something else. Three things differ from above.

1. **A container cannot reach `127.0.0.1`.** Each container has its own network
   namespace, so the host's loopback address is not visible to it. Bind the API to
   the host's address on the Docker bridge (usually `172.17.0.1`; ask
   `docker network inspect bridge`, or let the script compute it) and point the
   proxy at that:

   ```bash
   ./scripts/deploy.sh --api-host 172.17.0.1 --api-port 18080 --public-port 1443
   ```

   ```nginx
   # Inside the nginx container: keep listening on 80 / 443 — those are container ports.
   location / { proxy_pass http://172.17.0.1:18080; … }
   # Or give the nginx container --add-host=host.docker.internal:host-gateway
   # and proxy_pass http://host.docker.internal:18080;
   ```

   On the host side, publish only nginx's ports: `-p 180:80 -p 1443:443`.

2. **`FERROMA_PUBLIC_URL` has to carry the port** — that is what `--public-port 1443`
   is for. Without it, links in notification mail and the address handed to clients
   point at the host's 443, which is somebody else's service.

3. **The certificate can only come from DNS-01.** HTTP-01 always connects to port
   80 and TLS-ALPN-01 to port 443, and neither belongs to nginx here. Issue through
   your DNS provider's API (a certbot DNS plugin, or `acme.sh --dns`) and install it
   as in §5.5; renew through the deploy hook there too.

Two things stop working by default, because both are defined to live on 443:

* **Client autodiscovery** — the client fetches
  `https://<the domain in the address>/.well-known/ferroma` on **443** only
  (`alice@example.com` asks the **bare domain** `example.com`, not
  `mail.example.com`), and both the path and the port are fixed. The document itself
  is a small JSON that *anyone* can serve, and its `api` field is
  `FERROMA_PUBLIC_URL + /api/v1` — so it is the document that tells the client the
  real API lives on 1443:

  ```bash
  # 1. Take a copy from Ferroma (its content follows FERROMA_PUBLIC_URL)
  curl -s https://mail.example.com:1443/.well-known/ferroma
  curl -s http://172.17.0.1:18080/.well-known/ferroma   # or from the host

  # 2. Have whatever holds 443 serve it: as a static file, or by proxying that one
  #    path to Ferroma (mapped to the bare domain example.com)
  sudo mkdir -p /var/www/html/.well-known
  curl -s http://127.0.0.1:18080/.well-known/ferroma | sudo tee /var/www/html/.well-known/ferroma

  # 3. Check: expect 200, with api pointing at 1443
  curl -s https://example.com/.well-known/ferroma
  ```

  The document also carries the `imap` (993) and `smtp` (465/587) endpoints, and
  those ports are bound directly by Ferroma — unrelated to the HTTPS port — so a
  static copy is fully usable.

  When that is not possible, or the document answers `404`, the client does not
  silently guess somebody else's server: it falls back to guessing `mail.<domain>`
  (443/993/587) and marks the result as a guess, or reports the error. Configure the
  account by hand instead: `https://mail.example.com:1443/api/v1` for the API,
  `mail.example.com:993` for IMAP.

* **MTA-STS** (RFC 8461 fetches the policy over 443) — skip it, or have whatever
  holds 443 serve that policy file (a static file is enough there too).

Both come back if you can forward ports upstream (public 443 → this machine's
1443): then `FERROMA_PUBLIC_URL` is the portless `https://mail.example.com` and you
do not pass `--public-port` at all.

### 5.5 Let's Encrypt

The certificate must cover the names your users and peers connect to: at minimum
`mail.example.com` (SMTP, IMAP and the API), plus `mta-sts.example.com` if you
serve the policy from it.

```bash
# certbot, HTTP-01. Port 80 must be reachable.
sudo certbot certonly --standalone \
  -d mail.example.com -d mta-sts.example.com \
  --agree-tos -m admin@example.com --no-eff-email

# Files land here; symlink or copy them into ./tls/.
sudo ls -l /etc/letsencrypt/live/mail.example.com/
#   fullchain.pem  -> tls/fullchain.pem
#   privkey.pem    -> tls/privkey.pem
```

```bash
# Copy into the mounted directory with the ownership the container expects
# (the image runs as uid 10001).
sudo install -o 10001 -g 10001 -m 0640 \
  /etc/letsencrypt/live/mail.example.com/fullchain.pem tls/fullchain.pem
sudo install -o 10001 -g 10001 -m 0600 \
  /etc/letsencrypt/live/mail.example.com/privkey.pem   tls/privkey.pem
docker compose -f docker-compose.prod.yml restart ferroma
```

Notes:

* **Port 80 must be free and reachable** for HTTP-01. If a reverse proxy already
  listens on 80 — the common case — use webroot rather than `--standalone`: point
  the proxy's `/.well-known/acme-challenge/` at a directory and issue with
  `certbot certonly --webroot -w /var/www/html -d mail.example.com`.
* **`acme.sh` with DNS-01** avoids the port question entirely and covers
  wildcards; it is the better choice behind a proxy or on a host whose port 80 is
  otherwise occupied.
* **Renewal is a reload, not a re-issue.** Ferroma reads the PEM at start and does
  not watch the file, so a renewal has to be re-installed and the listeners
  restarted. On the §3.1 stack that is one command, which makes a certbot deploy
  hook enough to make the whole thing automatic:

  ```bash
  # /etc/letsencrypt/renewal-hooks/deploy/ferroma.sh  (chmod +x)
  #!/bin/sh
  cd /path/to/ferroma && ./scripts/deploy.sh certs >> /var/log/ferroma-certs.log 2>&1
  ```

  Without the hook, run `sudo ./scripts/deploy.sh certs` after a renewal: it
  installs the PEM into `./tls` as uid 10001, updates `.env` when it has to, and
  restarts the listeners, waiting for the health check. Your proxy reloads its own
  copy of the same certificate and the two never interfere.
* **On this development machine**, remember `AGENTS.md` §1.1: the Windows TLS
  stack is broken (`schannel` → `SEC_E_NO_CREDENTIALS`), so `curl.exe`, `git` and
  .NET cannot do HTTPS. Use `node tools/fetch.mjs <url> <dest>` for a one-off
  download and never add a `native-tls` dependency.

```bash
# A renewal rehearsal: certbot really does issue (against staging) and fires the
# deploy hook above.
sudo certbot renew --dry-run
```

---

## 6. First-run setup

### 6.1 Boot

```bash
docker compose -f docker-compose.prod.yml up -d
docker compose -f docker-compose.prod.yml ps
docker compose -f docker-compose.prod.yml logs ferroma | tail -50
```

A healthy start logs the resolved configuration summary, an `info` line per
listener, and the migration outcome when `database.run_migrations = true`.

### 6.2 The first-run wizard

While no admin exists, `GET /api/v1/setup` returns `{ "required": true }`
([api.md](api.md) §4.7), and `/` serves the Admin console rather than the Webmail — that is the
wizard ([§3.5](#35-choosing-the-database-in-the-browser) explains why the root is the console
until an instance is set up). Filling it in creates the first administrator and signs you in.

The hostname, the public URL, the listen address and the TLS material it collects are read when
the server starts, so the server comes back up by itself to adopt them: the page says it is
restarting, waits for the server to answer again, and reloads into the console. Nothing to do by
hand — and if the process cannot replace itself (a platform without `exec`), the page says so and
the container's restart policy, or `docker compose restart ferroma`, finishes the job.

```bash
# Or drive it from the shell.
curl -s https://mail.example.com/api/v1/setup
curl -s -X POST https://mail.example.com/api/v1/setup \
  -H 'Content-Type: application/json' \
  -d '{"email":"admin@example.com","password":"…","hostname":"mail.example.com","domain":"example.com"}'
```

`POST /setup` creates the first admin, the domain and its primary address, and
returns a normal token pair. Both endpoints return `409 conflict` afterwards.
`restart_required` in the response says whether the server is restarting to adopt what was
stored. The wizard is disabled entirely by `api.enable_setup_wizard = false` — set that
in `ferroma.toml` if you would rather create the first admin out of band, and
remember that it means the endpoints 404 rather than fail.

### 6.3 Creating a domain without the wizard

```bash
curl -s -X POST https://mail.example.com/api/v1/domains \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"example.com","description":"primary"}'
```

Or directly in the database, if the API is not up yet:

```sql
-- Domains are lower-cased; the schema enforces it.
INSERT INTO domains (name, description) VALUES ('example.com', 'primary')
RETURNING id, name, enabled;
```

### 6.4 Creating users and addresses

```bash
# The password is hashed with Argon2id by the server; never insert a hash by hand.
curl -s -X POST https://mail.example.com/api/v1/users \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"email":"alice@example.com","password":"…","display_name":"Alice","quota_bytes":1073741824}'

# Attach an address to that user. This creates the Maildir and the standard folders.
curl -s -X POST https://mail.example.com/api/v1/users/7/mailboxes \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"domain":"example.com","local_part":"alice","is_primary":true}'
```

The Maildir and folder rows are created by `Maildir::ensure_mailbox` and
`FoldersRepository::ensure_standard`, which produce
`INBOX`, `Sent`, `Drafts`, `Trash`, `Junk`, `Archive` with their `special_use`
markers ([imap.md](imap.md) §4.3).

```bash
# Confirm on disk, inside the container.
docker compose -f docker-compose.prod.yml exec ferroma \
  ls -la /var/lib/ferroma/mail/example.com/alice/Maildir
```

### 6.5 A first end-to-end check

```bash
# Local delivery to a local address, by hand. Use a real From for a real test.
printf 'EHLO test\r\nMAIL FROM:<admin@example.com>\r\nRCPT TO:<alice@example.com>\r\nDATA\r\nSubject: hello\r\n\r\nfirst\r\n.\r\nQUIT\r\n' \
  | nc 127.0.0.1 25

# Did it land?
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT id, uid, subject, sender, size_bytes, storage_path FROM messages ORDER BY id DESC LIMIT 5;"
```

---

## 7. DKIM: generating and publishing a key

DKIM signing is what keeps your outbound mail out of spam folders and makes DMARC
alignment possible.

### 7.1 Generate a key pair

```bash
# Preferred: the CLI generates a 2048-bit key and writes it into the domain record
# (or --out to a file).
docker compose exec ferroma ferroma dkim generate --domain example.com

# Print the TXT record to publish:
docker compose exec ferroma ferroma dkim show --domain example.com
```

```bash
# Or with OpenSSL. 2048-bit RSA is the size receivers
# expect; 1024 is too weak and 4096 is slow to verify.
openssl genrsa -out dkim/default.private 2048
openssl rsa -in dkim/default.private -pubout -out dkim/default.public

# The TXT record value, on one line:
printf 'v=DKIM1; k=rsa; p=%s\n' \
  "$(openssl rsa -in dkim/default.private -pubout 2>/dev/null \
     | grep -v '^-----' | tr -d '\n')"

# The private key must be readable only by the service user (uid 10001).
sudo chown 10001:10001 dkim/default.private
chmod 0600 dkim/default.private
```

The `docker-compose.external-db.yml` of §3.1 spares you the manual steps above:
`./scripts/deploy.sh` generates the key inside the `ferroma-data` volume at
`/var/lib/ferroma/dkim/<selector>.private` (already owned by uid 10001) and prints
the TXT record to publish; `./scripts/deploy.sh dkim --enable` turns signing on
once the record is out.

The `p=` value is base64 and may be long. Most DNS providers accept a TXT record
of 255 characters and some require you to split longer values into quoted chunks;
`dig` reassembles them. If your provider rejects the whole string, split it:

```bind
default._domainkey IN TXT (
    "v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA"
    "…the rest of the base64…"
)
```

### 7.2 Publish it

Add the record from §2.2, wait for the TTL, and verify:

```bash
dig +short TXT default._domainkey.example.com
# expected (illustrative): "v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A..."
```

### 7.3 Enable signing

```toml
# config/ferroma.toml
[dkim]
enabled = true
selector = "default"
private_key_path = "/etc/ferroma/dkim/default.private"
canonicalization = "relaxed"
verify_inbound = true
add_auth_results = true
```

```bash
# Or through the environment, which is how docker-compose.prod.yml does it.
FERROMA_DKIM_ENABLED=true
FERROMA_DKIM_SELECTOR=default
FERROMA_DKIM_KEY=/etc/ferroma/dkim/default.private
docker compose -f docker-compose.prod.yml restart ferroma
```

`Config::validate()` refuses to boot when `dkim.enabled = true` and
`dkim.private_key_path` is unset, when `dkim.selector` is empty, or when
`dkim.canonicalization` is neither `relaxed` nor `simple`.

The signing domain is per-domain by default and can be pinned with
`dkim.domain`. The private key can also live in `domains.dkim_private_key`
(`DomainsRepository::set_dkim`) — pick one place and stay there, and back up
whichever you chose.

### 7.4 Verify a signature end to end

```bash
# Send a message to a checker that reports the DKIM result.
swaks --server mail.example.com --port 587 --tls \
      --auth PLAIN --auth-user alice@example.com --auth-password '…' \
      --from alice@example.com --to check-auth@verifier.port25.com \
      --body "dkim test"

# Or read the Authentication-Results header in a message you sent to yourself.
```

Because `policy.add_auth_results = true`, a message delivered to one of your own
mailboxes carries the verdict, which is the quickest way to confirm the signer is
running:

```bash
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -Atc \
  "SELECT storage_path FROM messages ORDER BY id DESC LIMIT 1"
```

### 7.5 Rotating a DKIM key

1. Generate a new key with a **new selector** (`default2`).
2. Publish `default2._domainkey` and wait for the TTL to pass everywhere.
3. Switch `dkim.selector = "default2"` and restart.
4. Keep the old record published for at least the DMARC report window — a month
   is comfortable. Mail signed with the old key is still in flight and still being
   verified.
5. Only then remove the old record and the old key file.

---

## 8. Backup and restore

**Ferroma ships no backup tooling.** There is no `scripts/backup.sh`, no
`scripts/restore.sh`, no `backup` or `restore` service in either compose file, and
no `ferroma-backups` volume. `scripts/deploy.sh` has no `backup` and no `restore`
subcommand either. Backing this deployment up is the operator's job, done with the
host's own tools: `pg_dump`, `tar` or `rsync`, `restic`/`borg`, a filesystem or
hypervisor snapshot, your existing backup product.

### 8.1 What a backup must contain

The deployment keeps its state in two places, and they are **one** backup:

| Half | Where | What it holds |
|---|---|---|
| Database | the `postgres` container's volume (`ferroma-postgres-data`), or the PostgreSQL server you already run | domains, users, mailboxes, message metadata, the queue, the sync log, and the per-domain key in `domains.dkim_private_key` |
| Volume `ferroma-data` | mounted at `/var/lib/ferroma` (`server.data_dir`) | the Maildir, the attachment blobs, the DKIM private key at `/var/lib/ferroma/dkim/<selector>.private`, and `<data_dir>/database.json` |

The rule that governs everything else:

> A backup that contains only one of the two is not a backup: the database says a
> message exists and the Maildir holds its bytes, and restoring either alone gives
> you a mailbox full of dangling rows or a directory of orphaned files.

| Restored alone | What the user sees |
|---|---|
| Database only | an inbox of subjects with no bodies — every read is `StorageError::BodyMissing` |
| Maildir only | empty folders; the bytes are on disk and nothing knows about them |

`<data_dir>/database.json` is the address the server remembered when the database
was chosen in the browser (§3.5): mode 0600, because it holds the password in
clear. The volume is therefore **secret-bearing** exactly like the DKIM key beside
it, and whatever stores a copy of it — archive, snapshot or backup repository — has
to be at least as protected as `.env`: encrypted at rest, access-controlled, and
kept out of anything world-readable.

### 8.2 Taking a backup

Give every run one timestamped directory and put both halves in it:

```bash
# DATABASE_URL and POSTGRES_PASSWORD live in .env on the external-db stack.
set -a; . ./.env; set +a
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "/backups/$STAMP"
```

**The database half.** Use a `pg_dump` whose major version matches the server's.

```bash
# The external-db stack, or any host that already runs PostgreSQL with a psql of
# its own. DATABASE_URL is the address Ferroma connects with.
pg_dump --format=custom --compress=6 -d "$DATABASE_URL" \
  > "/backups/$STAMP/ferroma.dump"

# No host psql? Use the client image scripts/deploy.sh pulls for exactly this. It
# is a client: no server is ever started from it. --network host lets it reach the
# database at 127.0.0.1, which is how the external-db stack connects.
docker run --rm --network host -i \
  -e PGPASSWORD="$POSTGRES_PASSWORD" -e PGUSER="$POSTGRES_USER" -e PGDATABASE="$POSTGRES_DB" \
  postgres:16-alpine \
  pg_dump -h 127.0.0.1 --format=custom --compress=6 \
  > "/backups/$STAMP/ferroma.dump"

# Roles and other cluster-wide objects live in the cluster, not in the database.
docker run --rm --network host -i -e PGPASSWORD="$POSTGRES_PASSWORD" \
  postgres:16-alpine \
  pg_dumpall -h 127.0.0.1 -U postgres --globals-only \
  > "/backups/$STAMP/globals.sql"

# The prod stack, where PostgreSQL is the `postgres` container. The container's own
# POSTGRES_USER/POSTGRES_DB are used, so a deployment that renamed either still
# dumps the right database.
docker compose -f docker-compose.prod.yml exec -T postgres \
  sh -c 'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom --compress=6' \
  > "/backups/$STAMP/ferroma.dump"
```

**The volume half** — the Maildir, the attachments, the DKIM key and the
remembered database address. A throwaway container reads it without installing
anything on the host:

```bash
# One tar beside the dump. The mount is read-only: nothing here writes to the store.
docker run --rm \
  -v ferroma-data:/data:ro \
  -v "/backups/$STAMP:/out" \
  alpine tar -czf /out/ferroma-data.tar.gz -C /data .

# rsync, when the store is large and you want an incremental copy instead. Alpine
# ships tar but not rsync, so install it inside the throwaway container.
docker run --rm \
  -v ferroma-data:/data:ro \
  -v /backups/ferroma-data:/out \
  alpine sh -c 'apk add --no-cache rsync >/dev/null && rsync -a --delete /data/ /out/'
```

**Or a snapshot**, which is the same idea at the filesystem or hypervisor level and
much cheaper on a large store. Snapshots have to be taken for **both** halves at
the same moment — the volume's mountpoint and PostgreSQL's data directory, or the
whole VM at once:

```bash
docker volume inspect -f '{{.Mountpoint}}' ferroma-data   # where the volume really is
```

**Get the copies off the machine.** A backup on the same disk as the mail store is
not a backup, and neither is one in the same cloud account without versioning. Point
whatever you already run — `restic`, `borg`, `rclone`, an object store with
versioning — at the timestamped directory:

```bash
restic -r s3:s3.example.com/ferroma-offsite backup "/backups/$STAMP"
restic -r s3:s3.example.com/ferroma-offsite forget --keep-daily 14 --prune
```

Retention is now a policy you own rather than a variable in `.env`: pick a window
(14 days is reasonable) and enforce it in the tool that holds the copies. Whatever
you choose, test the restore (§8.6) — an untested backup is a hypothesis.

### 8.3 Consistency while live

* The **database half is a single `pg_dump`** — a consistent snapshot of one
  instant.
* The **Maildir half is a live `tar` or `rsync`.** Maildir writes are atomic
  renames ([storage.md](storage.md) §4.3), so the copy can miss an in-flight
  delivery but can never contain a half-written message.
* The bad direction cannot happen: the Maildir file is written *before* the row,
  so a live backup can produce an orphan file (benign, sweepable) but not a row
  without bytes.

They are still two snapshots of two systems taken at two moments. For a perfectly
consistent pair, stop the service for the duration:

```bash
docker compose -f docker-compose.prod.yml stop ferroma
# take the pg_dump and the volume copy now (§8.2)
docker compose -f docker-compose.prod.yml start ferroma
```

That is the only way to guarantee no transaction is split across the two halves,
and it takes seconds on a small store.

### 8.4 Restore

Stop Ferroma first: a restore **writes** both halves underneath a running server,
and a server pointed at a half-restored store is worse than a stopped one. Restore
from the **same** timestamp on both sides — a database from one day and a volume
from another is exactly the mismatch §8.1 warns about.

The order still matters, and it is the reverse of taking the backup: the database
first, because it defines what should exist; the volume second, so every row has
its file before the server starts. There is no configuration archive any more, but
the files that are in neither store still matter: `.env`, `config/ferroma.toml`,
and — on the prod stack, which bind-mounts them — `./tls` and `./dkim` live on the
host and belong in whatever keeps the deployment's definition. On the external-db
stack the DKIM key is inside `ferroma-data`, so the volume covers it.

```bash
docker compose -f docker-compose.prod.yml stop ferroma

# 1. The database. Recreate it empty first: pg_restore into a populated database
#    merges rather than replaces, and a silently merged mail store is how a week of
#    mail disappears.
docker compose -f docker-compose.prod.yml exec -T postgres \
  sh -c 'dropdb -U "$POSTGRES_USER" "$POSTGRES_DB"'
docker compose -f docker-compose.prod.yml exec -T postgres \
  sh -c 'createdb -U "$POSTGRES_USER" "$POSTGRES_DB"'
docker compose -f docker-compose.prod.yml exec -T postgres \
  sh -c 'pg_restore --no-owner --no-privileges -U "$POSTGRES_USER" -d "$POSTGRES_DB"' \
  < "/backups/$STAMP/ferroma.dump"

# 2. The volume. Clear it, then unpack the archive from the same stamp. tar keeps
#    the stored ownership, which the image needs: it runs as uid 10001.
docker volume create ferroma-data     # only when the volume is gone entirely
docker run --rm \
  -v ferroma-data:/data \
  -v "/backups/$STAMP:/in:ro" \
  alpine sh -c 'rm -rf /data/* /data/.[!.]*; tar -xzf /in/ferroma-data.tar.gz -C /data'

# 3. Start it and check that the two halves agree.
docker compose -f docker-compose.prod.yml up -d ferroma
docker compose -f docker-compose.prod.yml exec ferroma ferroma storage verify
```

Both halves are plain standard formats, so nothing needs the deployment to be
running: a bare-metal recovery on a new host is the same three steps with the same
files.

On the external-db stack the database half goes through the host's `psql` or the
client container instead of `docker compose exec postgres`. There is no `postgres`
service and no `--profile tools` restore service to run:

```bash
docker compose -f docker-compose.external-db.yml stop ferroma
docker run --rm --network host -i \
  -e PGPASSWORD="$POSTGRES_PASSWORD" -e PGUSER="$POSTGRES_USER" -e PGDATABASE="$POSTGRES_DB" \
  postgres:16-alpine \
  pg_restore --no-owner --no-privileges --clean --if-exists -h 127.0.0.1 \
  < "/backups/$STAMP/ferroma.dump"
docker compose -f docker-compose.external-db.yml up -d ferroma
```

Restoring one half alone is sometimes the right call — a migration the old binary
cannot read needs only the database back (§9.2) — but do it deliberately: the rows
and the files stop describing the same moment, and every message received since is
either a row with no bytes or bytes with no row. Run `ferroma storage verify`
afterwards in either case.

### 8.5 After a restore

```bash
# 1. A first sanity check: how many live messages the restored rows describe.
#    The full procedure is docs/storage.md §9.
docker compose -f docker-compose.prod.yml exec ferroma sh -c '
  psql "$DATABASE_URL" -Atc "SELECT COUNT(*) FROM messages WHERE expunged_at IS NULL"'

# 2. The integrity check, from the binary itself: rows without bodies, files
#    without rows, counters, uid_next.
docker compose -f docker-compose.prod.yml exec ferroma \
  ferroma storage verify --details

# 3. Reconcile the counters that are allowed to drift.
#    FoldersRepository::recount and MailboxesRepository::recompute_usage, triggered
#    through POST /api/v1/storage/gc (an admin token is required) and the Admin
#    storage screen.
#    curl -s -X POST https://mail.example.com/api/v1/storage/gc \
#      -H "Authorization: Bearer $TOKEN"

# 4. Start Ferroma and watch the log for the first minute.
docker compose -f docker-compose.prod.yml up -d ferroma
docker compose -f docker-compose.prod.yml logs -f --tail=100 ferroma
```

The full integrity procedure — the orphan query, the checksum loop, the counter
comparison, the `uid_next`/`uid_validity` rule — is in
[storage.md](storage.md) §9. Run it after every restore; a restore is the one
operation guaranteed to produce inconsistencies if anything went wrong.

### 8.6 A restore drill

**Test the restore, not the backup.** A backup that has never been restored is a
hypothesis (specification §46: *"必须实际测试恢复"* — recovery must actually be
tested).

```bash
# Restore the dump into a scratch database on the same host, without touching
# production. The volume half is restored the same way, into a throwaway volume.
docker compose -f docker-compose.prod.yml exec postgres createdb -U ferroma ferroma_drill
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_restore --no-owner --no-privileges --dbname=ferroma_drill \
  < "/backups/$STAMP/ferroma.dump"
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma_drill -c 'SELECT COUNT(*) FROM messages;'
docker compose -f docker-compose.prod.yml exec postgres dropdb -U ferroma ferroma_drill
```

Do this quarterly, and after every schema migration.

---

## 9. Upgrades and rollback

### 9.1 Upgrade

```bash
# 1. Back up first. Always. An upgrade is the second-most-likely time to need it.
#    Take both halves as in §8.2 — a database dump and a copy of the ferroma-data
#    volume — and note the stamp; §9.2 needs it.
# On the external-db stack ./scripts/deploy.sh upgrade rebuilds the image, restarts
# and waits for health. It does not back anything up for you.

# 2. Read the release notes for migration and configuration changes.
#    A new required key, or a removed one, stops the new version at boot
#    (unknown keys are rejected).

# 3. Update the image tag. Prod never builds from source.
sed -i 's/^FERROMA_VERSION=.*/FERROMA_VERSION=0.2.0/' .env

# 4. Pull and recreate only the ferroma service.
docker compose -f docker-compose.prod.yml pull ferroma
docker compose -f docker-compose.prod.yml up -d ferroma

# 5. Watch it come up.
docker compose -f docker-compose.prod.yml logs -f --tail=100 ferroma
```

**Coming from a release that shipped the backup sidecar.** `./scripts/deploy.sh
upgrade` starts the stack with `--remove-orphans`, which retires the container of a
service the compose file no longer defines — for 0.1.3 and earlier that is the
`ferroma-backup` container. The `ferroma-backups` volume is not removed with it
(volumes outlive services), so take your own backup first and drop it when the old
archives are no longer wanted:

```bash
docker volume rm ferroma-backups
```

Migrations run at startup when `database.run_migrations = true`. They are
forward-only: `migrations/` is an ordered list (currently one file,
`0001_initial.sql`) applied in order, and there is no down migration. That is why
step 1 is step 1.

### 9.2 Rollback

```bash
# Roll the image back.
sed -i 's/^FERROMA_VERSION=.*/FERROMA_VERSION=0.1.5/' .env
docker compose -f docker-compose.prod.yml pull ferroma
docker compose -f docker-compose.prod.yml up -d ferroma
```

An image rollback works **only if the schema is compatible**. If the new version
applied a migration that the old version cannot read, rolling back the binary is
not enough and you must restore the database from the pre-upgrade dump. On the
external-db stack that is the client-image form from §8.4:

```bash
docker compose -f docker-compose.external-db.yml stop ferroma
docker run --rm --network host -i -e PGPASSWORD="$POSTGRES_PASSWORD" \
  postgres:16-alpine \
  pg_restore --no-owner --no-privileges --clean --if-exists \
  -h 127.0.0.1 -U ferroma -d ferroma \
  < /backups/<pre-upgrade-stamp>/ferroma.dump
docker compose -f docker-compose.external-db.yml up -d ferroma
```

Rolling back the mail store is unnecessary here, because the dump and the volume
are separate archives: restore only the database, and the messages received since
the backup stay where they are. Do not unpack a pre-upgrade `ferroma-data`
archive on top of a store that has been running since — that would *lose* every
message received in between.

### 9.3 Zero-downtime is not supported

One process, one event bus ([security.md](security.md) §15.4). Running two
replicas behind a load balancer gives each user a real-time experience that depends
on which replica they hit. Scale the database and the storage before you consider
a second Ferroma process; both are likelier bottlenecks.

A restart costs the duration of `server.shutdown_timeout_secs` (30 s) plus the
boot: listeners stop accepting, in-flight SMTP transactions and queue deliveries
finish, then the process exits. `stop_grace_period: 60s` in
`docker-compose.prod.yml` gives it room. Inbound mail during the gap is retried by
the sending MTA, because SMTP is store-and-forward by design.

### 9.4 Publishing a release to Docker Hub

The image is published to `wesukilaye/ferroma` for `linux/amd64` and `linux/arm64`,
so an operator never has to spend 10–30 minutes compiling Rust on a mail server. Two
paths produce the *same* build; both are tag-driven and both refuse to publish an
image whose labels would lie about their source.

**By tag (the normal path).** Merging a version bump into `main` and tagging it is
the whole release:

```bash
# Cargo.toml [workspace.package] version = "0.2.0"
git commit -am 'release 0.2.0'
git tag v0.2.0
git push origin main v0.2.0
```

`.github/workflows/docker-publish.yml` then checks that the tag and `Cargo.toml`
agree — a `v0.2.0` tag on a tree that says `0.1.5` fails before anything is built —
runs `node tools/check-deploy.mjs`, builds both architectures with a GitHub Actions
layer cache, and pushes `0.2.0` and `latest`. A release publishes those two tags and
nothing else: there is deliberately no rolling minor tag (`0.2`, `0.3`, …), and no
`buildcache` tag. A pre-release (`0.2.0-rc.1`) pushes only its exact tag and never
moves `latest`: a `latest` pointing at an rc is exactly the surprise that tag exists
to avoid. The `0.1` and `buildcache` tags an earlier pipeline left on Docker Hub have
been deleted, and nothing in this repository produces either of them again.

It needs two repository secrets, set once:

| Secret | Value |
|---|---|
| `DOCKERHUB_USERNAME` | the Docker Hub account that owns the repository |
| `DOCKERHUB_TOKEN` | a Docker Hub **access token** with Read & Write scope — not the account password, so it can be revoked and is scoped to pushing |

`DOCKERHUB_REPO` (a repository *variable*, not a secret) overrides the target
repository for a fork.

**Locally.** The same build without CI, for a maintainer who wants it now:

```bash
docker login
./scripts/docker-publish.sh --dry-run       # the exact buildx command, nothing pushed
./scripts/docker-publish.sh                 # publish 0.2.0 and latest
./scripts/docker-publish.sh --no-latest     # publish 0.2.0 only
./scripts/docker-publish.sh --load          # this machine's architecture only, no push
```

On Windows the reliable form is `sh`, which Git for Windows puts on `PATH`:

```powershell
sh scripts/docker-publish.sh --dry-run
```

Plain PowerShell does not execute a `.sh` file at all: Windows has no association for
the extension, so the path is treated as a *document* to open. Standalone it prints
nothing and runs nothing; inside a pipeline it fails with "Cannot run a document in the
middle of a pipeline". The failure mode is a no-op that looks like it worked.

`scripts/docker-publish.ps1` is the entry point for a machine that allows script
execution. It is a wrapper, not a second implementation: it locates Git for Windows
(never `bash` from `PATH`, which on a machine carrying the WSL launcher is a different
distribution entirely), converts the script path to POSIX form, and forwards every
argument verbatim. `tools/check-deploy.mjs` fails the build if it ever stops delegating.
Where script execution is disabled — the Windows default, which `Get-ExecutionPolicy
-List` reports as `Undefined` at every scope — it has to be named explicitly:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/docker-publish.ps1 --dry-run
```

It takes the version from `Cargo.toml`, refuses to publish from a dirty working tree
(the revision label would name a commit that does not contain the source —
`--allow-dirty` overrides it and says so), warns when the active buildx builder cannot
push a multi-platform manifest, and keeps its layer cache **locally** in
`<repo>/.cache/buildx` — gitignored, so the second build on the same machine is not
another cold Rust build without publishing anything. `--no-cache` skips even that.
A registry cache would mean a published `buildcache` tag in the release repository,
which is why there is none; CI uses GitHub's own cache instead (above).

One environment trap is worth knowing, because it fails *after* a successful publish:
BuildKit passes the cache destination through a gRPC header, and a header value has to
be printable ASCII. A checkout under a path with non-ASCII characters (`~/项目/Ferroma`)
therefore cannot export the cache, and the run ends with
`header key "buildkit-attachable-store-id" contains value with non-printable ASCII
characters` even though the image is already on Docker Hub. The script moves the cache
to `$HOME/.cache/ferroma-buildx` when the checkout path is not plain ASCII, so the
status it reports matches what happened; `FERROMA_BUILDX_CACHE` overrides the location.

Two things about the build itself are worth knowing before starting one, because
neither is visible until it fails:

* **It pulls its base images first.** `rust:1.88-bookworm` and `debian:bookworm-slim`
  are ~1.5 GB, and they are fetched inside the build — where a flaky route to Docker
  Hub costs you the whole run. Pull them ahead of time so the download is a step of
  its own and can be retried:
  ```bash
  docker pull rust:1.88-bookworm && docker pull debian:bookworm-slim
  ```
* **arm64 is emulated on an amd64 machine.** QEMU runs the whole Rust release build
  for the second architecture, which turns a 10–30 minute native build into a much
  longer one. For a first publish, `--platforms linux/amd64` gets an image out in the
  native time; a tag pushed through CI (`§9.4` above) produces both architectures.

For a **single-host private registry** instead of Docker Hub, point
`FERROMA_REPO` at it and use `--repo`:

```bash
./scripts/docker-publish.sh --repo registry.example.com/ferroma
# then, in .env:  FERROMA_REPO=registry.example.com/ferroma
```

---

## 10. Monitoring and health checks

### 10.1 The health endpoint

`GET /api/v1/health` — no authentication, drives the container health check
([api.md](api.md) §2):

```json
{
  "status": "ok",
  "version": "0.1.5",
  "protocol_version": 1,
  "uptime_secs": 84213,
  "database": { "ok": true, "server_version": "PostgreSQL 16.15",
                "pool": { "size": 4, "idle": 3, "max": 20 } },
  "smtp": { "enabled": true, "connections": 3 },
  "imap": { "enabled": true, "connections": 1 },
  "queue": { "pending": 0, "delivering": 0, "retry": 2, "failed": 1 }
}
```

`503` with `"status": "degraded"` when the database is unreachable.

```bash
docker compose -f docker-compose.prod.yml exec ferroma \
  ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health

# Or without the CLI.
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'wget -qO- http://127.0.0.1:8080/api/v1/health || echo unreachable'
```

The Docker health check in the compose files and the `Dockerfile` runs
`ferroma healthcheck --url http://127.0.0.1:8080/api/v1/health` every 30 s with a
20–30 s start period and 3 retries. The address in
`docker-compose.external-db.yml` follows `FERROMA_API_HOST` **and**
`FERROMA_API_PORT` (default `127.0.0.1:18080`) — the probe follows wherever the API
is bound. When a containerised proxy forces the API onto the Docker bridge address
(the end of §5.4), the probe moves with it instead of reporting a healthy server as
unhealthy.

### 10.2 What to watch

| Signal | Where | Healthy | Act when |
|---|---|---|---|
| Container health | `docker compose ps` | `healthy` | `unhealthy` twice in a row |
| Restart count | `docker inspect -f '{{.RestartCount}}' ferroma` | stable | it climbs |
| `mail_queue.status = 'failed'` | `GET /api/v1/queue/stats` | near zero | any sustained non-zero |
| `mail_queue.status = 'retry'` | same | small | it grows for hours |
| Disk free | `df -h` on the host | > 20 % | < 15 %: mail stops being accepted |
| `mail_queue_due_idx` backlog | `GET /api/v1/queue/stats` → `next_due_at` | in the past or now | it lags more than a minute behind |
| IMAP/SMTP connections | `/api/v1/health` | well under `limits.max_connections` | pinned at the cap |
| Failed logins | `login_attempts` | a trickle | a burst, or one email/IP repeatedly |
| Certificate expiry | `openssl s_client`, `certbot certificates` | > 21 days | < 21 days: renew before it lapses |
| Backup freshness | your own backup job — `restic snapshots`, the timestamp on the newest dump | < 26 h old | older than 48 h |

```bash
# The queue, grouped.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT status, COUNT(*) FROM mail_queue GROUP BY status ORDER BY 2 DESC;"

# The oldest thing waiting to be retried.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT id, recipient, attempts, next_attempt_at, last_status_code, left(last_error,60)
     FROM mail_queue WHERE status IN ('pending','retry')
    ORDER BY next_attempt_at LIMIT 20;"

# Storage.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT pg_size_pretty(pg_database_size('ferroma')) AS db,
          (SELECT COUNT(*) FROM messages WHERE expunged_at IS NULL) AS live_messages,
          (SELECT COUNT(*) FROM users) AS users,
          (SELECT COUNT(*) FROM domains) AS domains;"
du -sh "$(docker volume inspect -f '{{.Mountpoint}}' ferroma-data)"

# Failed logins in the last day.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT email, ip, COUNT(*) FROM login_attempts
    WHERE NOT success AND created_at > NOW() - INTERVAL '1 day'
    GROUP BY email, ip ORDER BY 3 DESC LIMIT 20;"
```

### 10.3 Logs

`server.log_format = "json"` in production (`FERROMA_LOG_FORMAT=json`), which is
what Loki/ELK want. Every SMTP session carries `connection_id`, `remote_ip`,
`helo`, `authenticated_user`, `sender`, `recipient`, `message_id`, `result`,
`duration` ([security.md](security.md) §12.2), and `connection_id` also appears in
the `Received:` header Ferroma prepends — so a log line and a message header can be
joined.

```bash
# Just the errors.
docker compose -f docker-compose.prod.yml logs ferroma | grep -i '"level":"ERROR"'

# Everything about one message id.
docker compose -f docker-compose.prod.yml logs ferroma | grep '4821'

# A delivery that failed.
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'delivery'

# Fresh log lines as they happen, filtered.
docker compose -f docker-compose.prod.yml logs -f ferroma | grep -E 'WARN|ERROR'
```

Log rotation is bounded in the compose files (`max-size: 20m`, `max-file: 10` in
prod), so a log flood cannot fill the disk. Do not raise
`database.log_statements` in production: it prints message subjects.

### 10.4 Metrics and alerting are _(planned)_

Specification §40 lists Prometheus metrics (`smtp_connections_total`,
`smtp_messages_received_total`, `queue_pending_messages`, `mail_storage_bytes`,
`sync_operations_total`, …) and §41 places Prometheus and Grafana in a later
version. Neither exists yet: there is no `/metrics` endpoint. Until there is,
monitor with the health endpoint and the `psql` queries above, and alert on:

* container health failing,
* `mail_queue.status = 'failed'` above a threshold you choose,
* disk above 85 %,
* certificate expiry inside 21 days,
* the newest database dump or volume snapshot older than 48 h — this is your job
  now, nothing in the deployment checks it for you.

---

## 11. Troubleshooting, by symptom

### 11.1 "Mail is rejected as spam" / lands in the recipient's Junk

Almost always DNS, not Ferroma.

```bash
# 1. Does the PTR match FERROMA_HOSTNAME, and does it point back?
dig +short -x 203.0.113.10            # must equal FERROMA_HOSTNAME
dig +short mail.example.com           # must be the same address
grep FERROMA_HOSTNAME .env

# 2. Is SPF present, single, and terminating in -all?
dig +short TXT example.com | grep spf1

# 3. Is the DKIM record published and does it match the key you sign with?
dig +short TXT default._domainkey.example.com
openssl rsa -in dkim/default.private -pubout 2>/dev/null | grep -v '^-----' | tr -d '\n'

# 4. Is DMARC present?
dig +short TXT _dmarc.example.com

# 5. Is the IP on a blocklist?
dig +short 10.113.0.203.zen.spamhaus.org

# 6. Is the queue reporting failures with a remote status code?
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT recipient, last_status_code, last_status_text FROM mail_queue
    WHERE status = 'failed' ORDER BY updated_at DESC LIMIT 10;"
```

The usual causes, in order of frequency: no PTR or a mismatched one; a missing or
duplicated SPF record; more than ten SPF DNS lookups; a DKIM record published but
`dkim.enabled = false` so nothing is signed; a DMARC `p=reject` with no
alignment; a brand-new IP with no sending history (which only time and volume
fix); and a shared IP whose reputation you inherited.

### 11.2 "Cannot receive mail from Gmail" (or another large provider)

Large providers require TLS and behave strictly.

```bash
# 1. Is a connection from the internet reaching port 25 at all?
#    From a machine outside your network:
nc -vz mail.example.com 25

# 2. Does the MX record resolve, and is it the host you think?
dig +short MX example.com
dig +short A mail.example.com

# 3. Is Ferroma listening on 25 inside the container?
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'netstat -tlnp 2>/dev/null || ss -tlnp'

# 4. Did the connection even arrive? If there is no log line, it never got here.
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'connection\|reject\|550\|554'

# 5. Is the certificate valid from outside?
openssl s_client -starttls smtp -connect mail.example.com:25 -crlf < /dev/null 2>&1 | head -30

# 6. Does the greeting name match the PTR?
printf 'EHLO test\r\nQUIT\r\n' | nc mail.example.com 25
```

Common causes: the host firewall or the provider blocking port 25 (very common on
new VPSs — ask them to open it); a NAT/port-forward that maps 25 to the wrong
host; an AAAA record that advertises IPv6 the host cannot serve, which makes
dual-stack senders time out; and a certificate that expired, which makes senders
that require TLS (MTA-STS, or a provider policy) defer the mail with a `4xx`.

### 11.3 "Mail is accepted but never arrives" / "the queue is growing"

```bash
# 1. Is the queue actually growing, and is the first attempt recent?
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT status, COUNT(*), MIN(next_attempt_at), MAX(created_at)
     FROM mail_queue GROUP BY status;"

# 2. What are the top failures saying?
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT recipient, attempts, last_status_code, last_error
     FROM mail_queue WHERE status IN ('retry','failed')
    ORDER BY attempts DESC LIMIT 20;"

# 3. The attempt history of one stuck delivery.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT a.attempt, a.remote_mx, a.status_code, a.status_text, a.duration_ms, a.created_at
     FROM delivery_attempts a JOIN mail_queue q ON q.id = a.queue_id
    WHERE q.id = 1234 ORDER BY a.attempt;"

# 4. Can this host reach the remote MX on port 25 at all?
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'nc -vz gmail-smtp-in.l.google.com 25'

# 5. Is outbound 25 blocked by the provider? Test from the host.
nc -vz alt1.gmail-smtp-in.l.google.com 25

# 6. Is the dispatcher running at all?
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'queue\|dispatch'
grep -A6 '^\[queue\]' config/ferroma.toml
```

Read the diagnostics in the reply codes:

| Symptom | Meaning | Action |
|---|---|---|
| `421 4.7.0 Try again later` from a big provider | rate-limited or IP reputation | slow down, request delisting, check for a compromised account |
| `450`/`451` repeatedly | the remote is greylisting | normal; the retry schedule handles it |
| `550 5.7.1` from the remote | their policy rejects you | SPF/DKIM/DMARC/PTR, or a blocklist |
| `550 5.1.1` | the recipient does not exist | the message will bounce; correct the address |
| Nothing in `delivery_attempts` | the worker never claimed the row | check `queue.enabled`, `queue.workers`, and `mail_queue_due_idx` |
| Attempts climbing to `max_attempts` (12) | a persistent failure | read `last_status_text` |
| Everything stuck at `pending` with `next_attempt_at` in the past | dispatcher not running | check the log for panics; restart |

A burst of outbound mail that is not the user's suggests a compromised account:
check `login_attempts`, `sessions` (look at `ip`), and `mail_queue.user_id` for one
account dominating.

```sql
-- Who is sending the most?
SELECT user_id, COUNT(*) FROM mail_queue
 WHERE created_at > NOW() - INTERVAL '1 hour'
 GROUP BY user_id ORDER BY 2 DESC LIMIT 10;
```

Then revoke the account's sessions (`POST /api/v1/client/devices/:id/revoke`) and
change the password.

### 11.4 "IMAP login fails"

```bash
# 1. Is IMAP listening?
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'netstat -tlnp 2>/dev/null | grep -E "143|993"'

# 2. What does the greeting and capability list say?
openssl s_client -crlf -connect mail.example.com:143 < /dev/null 2>&1 | head -20
# for implicit TLS:
openssl s_client -connect mail.example.com:993 < /dev/null 2>&1 | head -20

# 3. Try a real login.
printf 'a LOGIN alice@example.com "…"\r\nb LOGOUT\r\n' | \
  openssl s_client -quiet -crlf -connect mail.example.com:143

# 4. Is TLS required, and is your client doing it?
grep -E 'require_tls_for_login|imaps_port' config/ferroma.toml
grep FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN .env

# 5. Is the account locked by the login throttle?
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT id, email, enabled, failed_logins, locked_until FROM users WHERE email='alice@example.com';"

# 6. What do the recent attempts say?
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT email, ip, success, created_at FROM login_attempts
    ORDER BY created_at DESC LIMIT 20;"

# 7. The server's view of the failure.
docker compose -f docker-compose.prod.yml logs ferroma | grep -i 'imap\|login'
```

| Response | Cause | Fix |
|---|---|---|
| `NO [PRIVACYREQUIRED]` | `imap.require_tls_for_login = true` and the client is on cleartext | configure `STARTTLS` (port 143) or implicit TLS (993) in the client |
| `NO [AUTHENTICATIONFAILED]` | wrong password, or the address is not the login name | the login is the full address, lower-cased: `alice@example.com` |
| Connection refused | the listener is down, or the port is not published | `grep -A4 '^\[imap\]' config/ferroma.toml`; check `docker compose ps` ports |
| TLS handshake error | expired or mismatched certificate | `openssl s_client -connect mail.example.com:993 -servername mail.example.com` |
| `* BYE Autologout` immediately | `imap.idle_timeout_secs` too low, or a clock problem | raise it; check the host clock |
| Works locally, not remotely | a firewall, or the client configured with the wrong host | `nc -vz mail.example.com 993` from outside |

A user who cannot send but can receive has a **submission** problem, not an IMAP
one: check port 587, `smtp.require_auth_on_submission`, and that the address they
are sending as is one they own ([security.md](security.md) §6.3).

### 11.5 "The API is down" / Webmail will not load

```bash
# 1. Health, from inside the container.
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'wget -qO- http://127.0.0.1:8080/api/v1/health || echo unreachable'

# 2. Is the container healthy?
docker compose -f docker-compose.prod.yml ps
docker inspect -f '{{.State.Health.Status}} restarts={{.RestartCount}}' ferroma

# 3. Why did it exit?
docker compose -f docker-compose.prod.yml logs --tail=200 ferroma | grep -iE 'error|panic|config'

# 4. A configuration typo stops the server at boot. That is by design.
docker compose -f docker-compose.prod.yml run --rm ferroma \
  ferroma serve --config /etc/ferroma/ferroma.toml
```

`500 storage_error` from every endpoint means the database is unreachable: check
`docker compose ps postgres`, the `DATABASE_URL`, and the pool size against
`database.max_connections`. A `503` from `/health` with
`"status": "degraded"` is the same condition reported gracefully.

### 11.6 "Quota says full but the mailbox looks empty"

```bash
# What the database believes.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT m.id, d.name||'@'||m.local_part AS addr, u.used_bytes, u.quota_bytes
     FROM mailboxes m JOIN domains d ON d.id=m.domain_id JOIN users u ON u.id=m.user_id
    ORDER BY u.used_bytes DESC LIMIT 10;"

# What is actually on disk.
docker compose -f docker-compose.prod.yml exec ferroma \
  du -sb /var/lib/ferroma/mail/example.com/alice

# The files that make up the total.
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'find /var/lib/ferroma/mail/example.com/alice -type f -printf "%s %p\n" | sort -rn | head -20'
```

`used_bytes` is a cache and `MailboxesRepository::recompute_usage` corrects it
([storage.md](storage.md) §5). A large discrepancy in either direction is worth
investigating rather than just recomputing: files deleted behind Ferroma's back
mean someone else is writing to the mail root.

### 11.7 "Disk is filling up"

```bash
# Where.
du -sh /var/lib/docker/volumes/ferroma-data/_data/*
du -sh /var/lib/docker/volumes/ferroma-data/_data/mail/*

# Garbage from interrupted writes.
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'find /var/lib/ferroma/mail -type d -name tmp -exec du -sh {} +'

# Unreferenced attachment blobs.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -Atc 'SELECT DISTINCT storage_path FROM attachments' | wc -l
docker compose -f docker-compose.prod.yml exec ferroma \
  sh -c 'find /var/lib/ferroma/attachments -type f ! -name "*.tmp" | wc -l'

# Expunged messages still holding their files.
docker compose -f docker-compose.prod.yml exec postgres \
  psql -U ferroma -d ferroma -c \
  "SELECT COUNT(*), pg_size_pretty(SUM(size_bytes)) FROM messages WHERE expunged_at IS NOT NULL;"
```

Levers, in order: run the sweeps (`Maildir::sweep_tmp`, `AttachmentStore::gc`);
prune `delivery_attempts` and `login_attempts` by age; lower
`queue.retention_days`; and only then look at quotas. The blob/file count mismatch
is expected — identical attachments share a blob — so compare *sizes*, not counts.

---

## 12. Related documents

| Topic | Document |
|---|---|
| Endpoints used in every command above | [api.md](api.md) |
| SMTP reply codes, retry schedule, bounce format | [smtp.md](smtp.md) |
| IMAP capability list, folder names, client compatibility | [imap.md](imap.md) |
| Schema, Maildir layout, quota, GC, integrity procedure | [storage.md](storage.md) |
| Threat model, relay defence, TLS decision, known gaps | [security.md](security.md) |
| Sync model and the client's failure matrix | [sync.md](sync.md) |
| The official desktop client | [client.md](client.md) |
| Crate graph and request lifecycle | [architecture.md](architecture.md) |
| The text of the published Docker Hub repository page | [dockerhub.md](dockerhub.md) |
| Every term, and the form to write it in | [GLOSSARY.md](GLOSSARY.md) |
| What is still not done | [../TODO.md](../TODO.md) |
| Build quirks on this machine (proxy, `CARGO_HOME`, PostgreSQL) | [../AGENTS.md](../AGENTS.md) |
