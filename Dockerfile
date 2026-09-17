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

# `touch` so cargo notices the placeholder sources changed.
RUN set -eux; \
    find crates server client -name '*.rs' -exec touch {} +; \
    cargo build --release --bin ferroma; \
    strip target/release/ferroma

# -----------------------------------------------------------------------------
# Stage 2 — runtime
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="Ferroma" \
      org.opencontainers.image.description="A Rust-native self-hosted mail platform" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

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

# State: Maildir, attachments, TLS material, backups.
RUN set -eux; \
    mkdir -p /var/lib/ferroma/{mail,attachments,tls,backups}; \
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
# These two are not optional: the router resolves `api.webmail_dir` / `api.admin_dir`
# from the configuration, and falls back to *relative* candidates (`web/dist`, `web`)
# resolved against the process working directory — which here is `/var/lib/ferroma`,
# not `/usr/share/ferroma`. Without them the container starts healthily and serves a
# 404 at `/`, which looks like a broken build rather than a missing path.
ENV FERROMA__API__WEBMAIL_DIR=/usr/share/ferroma/web \
    FERROMA__API__ADMIN_DIR=/usr/share/ferroma/admin

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
