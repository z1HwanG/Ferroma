# =============================================================================
# Ferroma — production image
# =============================================================================
# Multi-stage build: a full Rust toolchain compiles the workspace, and only the
# resulting binary plus a minimal Debian userland ship in the final image.
#
# Note for this repository: the host's `.cargo/config.toml` redirects crates.io to
# a local HTTP proxy that only exists on the development machine. The build context
# therefore excludes `.cargo/` (see .dockerignore) so the container uses the real
# registry, and TLS inside the container is plain OpenSSL — no schannel involved.
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — build
# -----------------------------------------------------------------------------
# The toolchain is pinned, so it has to be pinned correctly: the dependency graph
# requires Rust 1.88 (`encoding_rs`, `home`, the ICU crates, `time`), and a lower pin
# fails the build here — on the server, after a full dependency download.
# `node tools/msrv.mjs` recomputes the floor from Cargo.lock and fails if this drifts.
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Dependencies first: this layer is cached until a manifest changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/ferroma-core/Cargo.toml      crates/ferroma-core/
COPY crates/ferroma-mail/Cargo.toml      crates/ferroma-mail/
COPY crates/ferroma-storage/Cargo.toml   crates/ferroma-storage/
COPY crates/ferroma-auth/Cargo.toml      crates/ferroma-auth/
COPY crates/ferroma-events/Cargo.toml    crates/ferroma-events/
COPY crates/ferroma-smtp/Cargo.toml      crates/ferroma-smtp/
COPY crates/ferroma-imap/Cargo.toml      crates/ferroma-imap/
COPY crates/ferroma-sync/Cargo.toml      crates/ferroma-sync/
COPY crates/ferroma-api/Cargo.toml       crates/ferroma-api/
COPY server/Cargo.toml                   server/
COPY client/Cargo.toml                   client/

# Placeholder sources so the dependency graph can be compiled and cached on its own.
RUN set -eux; \
    for crate in ferroma-core ferroma-mail ferroma-storage ferroma-auth ferroma-events \
                 ferroma-smtp ferroma-imap ferroma-sync ferroma-api; do \
        mkdir -p "crates/$crate/src"; \
        echo '' > "crates/$crate/src/lib.rs"; \
    done; \
    mkdir -p server/src client/src; \
    echo 'fn main() {}' > server/src/main.rs; \
    echo 'fn main() {}' > client/src/main.rs; \
    mkdir -p migrations; \
    echo '-- placeholder' > migrations/0001_initial.sql; \
    mkdir -p config; \
    echo '' > config/ferroma.toml; \
    cargo build --release --bin ferroma --offline 2>/dev/null || \
    cargo build --release --bin ferroma

# Real sources.
COPY crates ./crates
COPY server ./server
COPY client ./client
COPY migrations ./migrations
COPY config ./config
COPY web ./web
COPY admin ./admin
COPY shared ./shared

# `touch` so cargo notices the placeholder sources changed.
RUN set -eux; \
    find crates server client -name '*.rs' -exec touch {} +; \
    cargo build --release --bin ferroma; \
    strip target/release/ferroma

# -----------------------------------------------------------------------------
# Stage 2 — runtime
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Provenance. `scripts/docker-publish.sh` and the release workflow pass these as
# build arguments, so every published image says which release, commit and date
# produced it — and `org.opencontainers.image.source` is what links the Docker Hub
# repository back to the source tree. The defaults keep a plain `docker build .`
# self-describing instead of labelling the image with empty strings.
# TODO(0.1.7): these three reach the OCI labels below and never reach the compiler.
# `ferroma-core/src/version.rs` reads FERROMA_BUILD_TIMESTAMP and FERROMA_GIT_SHA with
# `option_env!` at compile time, so `ferroma version` in the image prints
# `built: unknown` / `revision: unknown` even though the label carries the commit.
# Re-declare the ARGs in the `builder` stage and set
#   ENV FERROMA_GIT_SHA=$FERROMA_REVISION FERROMA_BUILD_TIMESTAMP=$FERROMA_CREATED
# immediately BEFORE the final `cargo build` — after the dependency-cache layer, which
# a per-release value would otherwise invalidate every time. See README "Carried into
# 0.1.7".
ARG FERROMA_VERSION=dev
ARG FERROMA_REVISION=unknown
ARG FERROMA_CREATED=unknown

LABEL org.opencontainers.image.title="Ferroma" \
      org.opencontainers.image.description="A Rust-native self-hosted mail platform" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.url="https://github.com/z1HwanG/Ferroma" \
      org.opencontainers.image.source="https://github.com/z1HwanG/Ferroma" \
      org.opencontainers.image.documentation="https://github.com/z1HwanG/Ferroma/blob/main/docs/deployment.md" \
      org.opencontainers.image.version="${FERROMA_VERSION}" \
      org.opencontainers.image.revision="${FERROMA_REVISION}" \
      org.opencontainers.image.created="${FERROMA_CREATED}"

# ca-certificates: outbound TLS to remote MX hosts and webhooks.
# tzdata: correct `Received:` timestamps and log localisation.
# openssl + bind9-dnsutils: two Admin-panel features shell out to them. Generating a
#   DKIM key runs `openssl genpkey`, and the DNS diagnostics run `nslookup`. Without
#   them the container starts fine and those two buttons fail — so they are a runtime
#   dependency, not a build-time convenience. (`ferroma dkim generate` on the CLI uses
#   the Rust RSA implementation and needs neither.)
# libcap2-bin: supplies `setcap`, used below so that the unprivileged service user
#   can bind the privileged mail ports.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        ca-certificates tzdata openssl bind9-dnsutils libcap2-bin; \
    rm -rf /var/lib/apt/lists/*; \
    groupadd --system --gid 10001 ferroma; \
    useradd --system --uid 10001 --gid ferroma --home-dir /var/lib/ferroma --create-home ferroma

COPY --from=builder /build/target/release/ferroma /usr/local/bin/ferroma

# SMTP (25), submission (587) and IMAP (143) are below 1024. Inside a bridge
# network the kernel's `net.ipv4.ip_unprivileged_port_start` is 0, so uid 10001
# may bind them; under `network_mode: host` the container shares the host's
# namespace, where those ports are still privileged. A file capability grants
# exactly the one needed, at exec, without running the server as root.
RUN set -eux; \
    setcap 'cap_net_bind_service=+ep' /usr/local/bin/ferroma; \
    getcap /usr/local/bin/ferroma

# Configuration and static frontends are baked in; operators override the config by
# mounting their own at /etc/ferroma/ferroma.toml.
COPY --from=builder /build/config/ferroma.toml /etc/ferroma/ferroma.toml
COPY --from=builder /build/web /usr/share/ferroma/web
COPY --from=builder /build/admin /usr/share/ferroma/admin
COPY --from=builder /build/shared /usr/share/ferroma/shared

# `COPY` preserves the mode of the file it copies, and a checkout can legitimately hold a
# source file that is not world-readable — `0600` is what some editors and agent tools
# write, and git does not track the difference (only the exec bit). The service runs as
# uid 10001 and reads these through the static file server, so such a file is not
# "slightly private": it answers 404, the ES module graph fails to load, and the Webmail
# and Admin render a blank page — a symptom with nothing in the server log to connect it
# to a file mode. The image therefore normalises what it ships instead of trusting the
# umask of whichever machine built it. `X` grants execute only to directories.
#
# State: the Maildir, the attachment blobs, the DKIM private key and database.json.
# No backups directory: nothing in the image writes backups, and the volume is the
# operator's to back up (docs/deployment.md §8).
RUN set -eux; \
    chmod -R a+rX /usr/share/ferroma /etc/ferroma; \
    mkdir -p /var/lib/ferroma/{mail,attachments,tls}; \
    chown -R ferroma:ferroma /var/lib/ferroma /etc/ferroma

USER ferroma
WORKDIR /var/lib/ferroma

ENV FERROMA_CONFIG=/etc/ferroma/ferroma.toml \
    FERROMA_DATA_DIR=/var/lib/ferroma \
    FERROMA_API_HOST=0.0.0.0 \
    FERROMA_SMTP_HOST=0.0.0.0 \
    RUST_BACKTRACE=1

# Point the API at the baked-in frontends.
#
# These are not optional: the router resolves `api.webmail_dir` / `api.admin_dir` /
# `api.shared_dir` from the configuration, and falls back to *relative* candidates
# (`web/dist`, `web`, `shared`) resolved against the process working directory —
# which here is `/var/lib/ferroma`, not `/usr/share/ferroma`. Without them the
# container starts healthily and serves a 404 at `/`, which looks like a broken
# build rather than a missing path. `shared_dir` is what keeps `/shared/api.js`
# reachable: both front-ends import their common modules from there.
ENV FERROMA__API__WEBMAIL_DIR=/usr/share/ferroma/web \
    FERROMA__API__ADMIN_DIR=/usr/share/ferroma/admin \
    FERROMA__API__SHARED_DIR=/usr/share/ferroma/shared

# 25   SMTP (inbound MX)
# 587  Submission (authenticated, STARTTLS)
# 465  SMTPS (implicit TLS)
# 143  IMAP + STARTTLS
# 993  IMAPS
# 8080 HTTP API, Webmail, Admin
EXPOSE 25 587 465 143 993 8080

# The binary talks to its own HTTP API, so the image needs no curl.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["ferroma", "healthcheck", "--url", "http://127.0.0.1:8080/api/v1/health"]

# SIGTERM triggers a graceful shutdown: listeners stop accepting, in-flight SMTP
# transactions and queue deliveries finish inside `server.shutdown_timeout_secs`.
STOPSIGNAL SIGTERM

ENTRYPOINT ["ferroma"]
CMD ["serve", "--config", "/etc/ferroma/ferroma.toml"]
