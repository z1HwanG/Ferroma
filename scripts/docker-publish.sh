#!/bin/sh
# =============================================================================
# Ferroma — build the release image and publish it to Docker Hub
# =============================================================================
#   ./scripts/docker-publish.sh                    # publish 0.1.0, 0.1, latest
#   ./scripts/docker-publish.sh --dry-run          # print the buildx command, push nothing
#   ./scripts/docker-publish.sh --load             # single-arch, into the local daemon
#   ./scripts/docker-publish.sh --platforms linux/amd64
#   ./scripts/docker-publish.sh --version 0.2.0    # release a version other than Cargo.toml's
#   ./scripts/docker-publish.sh --repo me/ferroma  # another namespace or registry
#   ./scripts/docker-publish.sh --no-latest        # tags 0.1.0 and 0.1 only
#
# Run `docker login` first. This script never handles credentials, reads no token
# and stores none: every push is authenticated by the credential the Docker CLI
# already holds for the target registry.
#
# The same build is what `.github/workflows/docker-publish.yml` runs on a `v*` tag;
# this script exists so a maintainer can cut a release without waiting on CI, and
# so the CI path is not the only one that has ever been exercised.
# =============================================================================
set -eu

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
DOCKERFILE="Dockerfile"

# Where releases live. Overridable per invocation (--repo) or per environment, so
# a fork can publish to its own namespace without editing this file.
DEFAULT_REPO="wesukilaye/ferroma"
# Both release architectures. Ferroma is a self-hosted server: amd64 covers the
# usual VPS, arm64 the growing share of ARM hosts (Hetzner CAX, Ampere, Apple
# silicon development boxes).
DEFAULT_PLATFORMS="linux/amd64,linux/arm64"

REPO="${FERROMA_REPO:-$DEFAULT_REPO}"
PLATFORMS="$DEFAULT_PLATFORMS"
PLATFORMS_GIVEN=0
VERSION_ARG=""
PUSH_LATEST=1
DRY_RUN=0
LOAD=0
ALLOW_DIRTY=0
NO_CACHE=0

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
warn() { printf '%s[warn]%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die()  { printf '%s[error]%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }
need_value() {
    [ "$#" -ge 2 ] || die "option $1 needs a value"
}

usage() {
    sed -n '3,18p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --repo)        need_value "$@"; REPO="$2"; shift 2 ;;
        --repo=*)      REPO="${1#*=}"; shift ;;
        --version)     need_value "$@"; VERSION_ARG="$2"; shift 2 ;;
        --version=*)   VERSION_ARG="${1#*=}"; shift ;;
        --platforms)   need_value "$@"; PLATFORMS="$2"; PLATFORMS_GIVEN=1; shift 2 ;;
        --platforms=*) PLATFORMS="${1#*=}"; PLATFORMS_GIVEN=1; shift ;;
        --no-latest)   PUSH_LATEST=0; shift ;;
        --no-cache)    NO_CACHE=1; shift ;;
        --allow-dirty) ALLOW_DIRTY=1; shift ;;
        --dry-run)     DRY_RUN=1; shift ;;
        --load)        LOAD=1; shift ;;
        -h|--help|help) usage ;;
        *) die "unknown option '$1' (--help for the list)" ;;
    esac
done

cd "$ROOT_DIR"

# -----------------------------------------------------------------------------
# Preflight
# -----------------------------------------------------------------------------
# Checked before anything slow happens: a buildx that cannot push multi-arch is a
# failure worth discovering now, not after a 20-minute Rust release build.
preflight() {
    have docker || die "docker is not on PATH"
    docker buildx version >/dev/null 2>&1 \
        || die "docker buildx is unavailable — Docker Engine 24+ / Docker Desktop ships it"

    [ -f "$DOCKERFILE" ] || die "$DOCKERFILE not found (expected in $ROOT_DIR)"

    # The active builder's driver decides whether a multi-platform manifest can be
    # pushed. Two things here are easy to get wrong, and both were measured on the
    # machine this was written on:
    #
    #   * `docker buildx inspect --format '{{.Driver}}'` is NOT portable. Docker
    #     29.8 / BuildKit v0.33 answers "unknown flag: --format" and exits 125, so a
    #     preflight built on it silently degrades to no check at all. Parse the plain
    #     output instead.
    #   * The `docker` driver is not automatically disqualifying any more. It cannot
    #     push a multi-platform manifest against the *classic* image store, but with
    #     containerd — the default in current Docker Desktop — it accepts
    #     `--platform linux/amd64,linux/arm64` and starts building. BuildKit rejects
    #     an unsupported platform list before it compiles anything, so refusing here
    #     would block a working configuration without saving any time. This warns and
    #     names the remedy; the build's own error is already early.
    _driver=$(docker buildx inspect 2>/dev/null | sed -n 's/^Driver:[[:space:]]*//p' | head -n 1)
    case "${_driver:-}" in
        docker)
            warn "the active buildx builder uses the 'docker' driver."
            warn "with the containerd image store that is fine; against the classic store a"
            warn "multi-platform build stops with \"multiple platforms feature is currently"
            warn "not supported\". If you see that, create a containerised builder once:"
            warn "  docker buildx create --name ferroma --driver docker-container --use --bootstrap"
            ;;
        '')
            warn "could not read the buildx driver; continuing and letting buildx decide"
            ;;
    esac

    # Whether the context path is a POSIX one that MSYS will rewrite. Only changes
    # what the dry run prints — see build().
    case "$(uname -s 2>/dev/null || echo unknown)" in
        MINGW*|MSYS*|CYGWIN*) MSYS_SHELL=1 ;;
        *) MSYS_SHELL=0 ;;
    esac
}

# -----------------------------------------------------------------------------
# Version, revision, tags
# -----------------------------------------------------------------------------
# The version comes from Cargo.toml — the same string `ferroma version` reports and
# the tag the release workflow derives from the git tag. `--version` exists for the
# case where the two have to disagree, and it is the only way they can: nothing
# here rewrites the source tree.
resolve_version() {
    if [ -n "$VERSION_ARG" ]; then
        VERSION="$VERSION_ARG"
    else
        VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT_DIR/Cargo.toml" | head -n 1)
        [ -n "$VERSION" ] || die "no version found in Cargo.toml — pass --version"
    fi

    case "$VERSION" in
        [0-9]*.[0-9]*.[0-9]*) : ;;
        *) die "'$VERSION' is not a x.y.z version" ;;
    esac

    MINOR=$(printf '%s' "$VERSION" | sed 's/^\([0-9]*\.[0-9]*\)\..*/\1/')

    # A pre-release moves no rolling tag: `latest` pointing at 0.2.0-rc.1 is exactly
    # the surprise the tag is supposed to prevent.
    PRERELEASE=0
    case "$VERSION" in *-*) PRERELEASE=1 ;; esac

    TAGS="$REPO:$VERSION"
    if [ "$PRERELEASE" = 0 ]; then
        if [ "$PUSH_LATEST" = 1 ]; then
            TAGS="$TAGS $REPO:$MINOR $REPO:latest"
        else
            TAGS="$TAGS $REPO:$MINOR"
        fi
    fi
}

# The revision label has to be true. An image published from a dirty tree claims a
# commit that does not contain its own source — so publishing one takes an explicit
# flag, and even then it is announced.
resolve_revision() {
    if have git && git -C "$ROOT_DIR" rev-parse HEAD >/dev/null 2>&1; then
        REVISION=$(git -C "$ROOT_DIR" rev-parse HEAD)
        if [ -n "$(git -C "$ROOT_DIR" status --porcelain)" ]; then
            if [ "$ALLOW_DIRTY" = 0 ] || [ "$DRY_RUN" = 1 ]; then
                [ "$DRY_RUN" = 1 ] || die "the working tree has uncommitted changes, so the
      revision label would name a commit that does not match this source. Commit
      them, or publish anyway with --allow-dirty (the label will not be
      reproducible)."
                warn "working tree is dirty — a real publish would need --allow-dirty"
            else
                warn "publishing from a dirty tree at $REVISION (--allow-dirty)"
            fi
        fi
    else
        REVISION=unknown
        [ "$DRY_RUN" = 1 ] || warn "not a git checkout: the revision label will say 'unknown'"
    fi
    CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
}

# -----------------------------------------------------------------------------
# Build
# -----------------------------------------------------------------------------
build() {
    # `--load` writes into the local daemon, which holds one architecture only.
    if [ "$LOAD" = 1 ]; then
        if [ "$PLATFORMS_GIVEN" = 1 ]; then
            case "$PLATFORMS" in
                *,*) die "--load cannot load a multi-platform build into the local daemon;
      drop --load, or narrow it: --platforms linux/amd64" ;;
            esac
        else
            _arch=$(docker version --format '{{.Server.Arch}}' 2>/dev/null || echo amd64)
            case "$_arch" in aarch64|arm64) PLATFORMS="linux/arm64" ;; *) PLATFORMS="linux/amd64" ;; esac
        fi
    fi

    set -- buildx build \
        --file "$DOCKERFILE" \
        --platform "$PLATFORMS" \
        --build-arg "FERROMA_VERSION=$VERSION" \
        --build-arg "FERROMA_REVISION=$REVISION" \
        --build-arg "FERROMA_CREATED=$CREATED"

    for _tag in $TAGS; do
        set -- "$@" --tag "$_tag"
    done

    # Registry-backed cache: the Rust release build is 10–30 minutes cold, and a
    # cache that lives only on the publishing machine helps nobody else. It costs
    # one extra tag (`buildcache`) in the repository.
    if [ "$NO_CACHE" = 0 ] && [ "$DRY_RUN" = 0 ]; then
        set -- "$@" --cache-from "type=registry,ref=$REPO:buildcache"
        [ "$LOAD" = 1 ] || set -- "$@" --cache-to "type=registry,ref=$REPO:buildcache,mode=max"
    fi

    if [ "$LOAD" = 1 ]; then
        set -- "$@" --load
    else
        # Provenance attestation: the build records how it was made. Cheap, and it
        # is what makes "which source is this image?" answerable later.
        set -- "$@" --push --provenance=true
    fi

    set -- "$@" "$ROOT_DIR"

    step "Building $VERSION for $PLATFORMS"
    info "tags: $TAGS"
    info "revision: $REVISION"
    if [ "$DRY_RUN" = 1 ]; then
        info "target: ${REPO} (dry run: nothing is built, tagged or pushed)"
        printf '\n'
        info "docker login   # if you have not already, for the registry above"
        printf '    docker'; for _a in "$@"; do printf ' %s' "$_a"; done; printf '\n'
        if [ "$MSYS_SHELL" = 1 ]; then
            printf '\n'
            info "the build context above is a Git Bash path. MSYS rewrites it to a"
            info "Windows path on the way to docker.exe, so the line is correct as run —"
            info "but it will not paste into PowerShell as written."
        fi
        return 0
    fi

    docker "$@"
}

# -----------------------------------------------------------------------------
# Report
# -----------------------------------------------------------------------------
report() {
    [ "$DRY_RUN" = 1 ] && return 0
    printf '\n'
    if [ "$LOAD" = 1 ]; then
        step "Loaded into the local daemon"
        for _tag in $TAGS; do info "$_tag"; done
        return 0
    fi

    step "Published"
    for _tag in $TAGS; do info "$_tag"; done
    printf '\n'
    info "Deploy it by pinning the version in .env:"
    info "  FERROMA_VERSION=$VERSION"
    printf '\n'
    info "The digest is what a reproducible deployment should pin:"
    info "  docker buildx imagetools inspect $REPO:$VERSION"
}

preflight
resolve_version
resolve_revision
build
report
