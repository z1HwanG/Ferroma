# Ferroma

**A Rust-native, self-hosted mail platform.**

Ferroma is a complete mail system built from the protocols up: its own SMTP and IMAP
servers, its own MIME and mail core, its own storage engine — plus a Webmail client,
an admin console and a set of official cross-platform clients that talk to it over a
purpose-built synchronisation protocol.

It does **not** wrap Postfix, Dovecot, Stalwart or any other mail server. The point is
to own the whole stack.

```text
                              FERROMA
                                 │
      ┌──────────────────────────┼──────────────────────────┐
      │                          │                          │
   Webmail                Official Clients               Admin
      │                          │                          │
      │                    ┌─────┼─────┐                    │
      │                    ▼     ▼     ▼                    │
      │                  Win   Linux  macOS                 │
      │                                                      │
      └──────────────────────────┼──────────────────────────┘
                                 │
                        Client API (FCP) / HTTP API
                                 │
                        ┌────────▼────────┐
                        │  Ferroma Core   │
                        └────────┬────────┘
                                 │
       ┌───────────┬─────────────┼─────────────┬───────────┐
       ▼           ▼             ▼             ▼           ▼
     SMTP        IMAP         Storage        Queue        DNS
       │           │             │             │           │
       └───────────┴─────────────┼─────────────┴───────────┘
                                 │
                            Event Bus ── WebSocket ── Push
                                 │
                            PostgreSQL
```

---

## Quick start

The supported deployment is Docker Compose. You need a host with a public IP, a domain
whose `MX` record points at it, and port 25 open.

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
cp .env.example .env          # set POSTGRES_PASSWORD, FERROMA_HOSTNAME, FERROMA_JWT_SECRET
docker compose up -d
docker compose logs -f ferroma
```

Then open `http://localhost:8080` and walk through the first-run wizard. For a real MX
— TLS, DKIM, deliverability — use `docker-compose.prod.yml` and follow
[`docs/deployment.md`](docs/deployment.md); the DNS section is not optional.

### Prefer a published image over a local build?

Every release is published to Docker Hub as `wesukilaye/ferroma`, for `linux/amd64` and
`linux/arm64`. The first `docker compose up -d` above compiles the whole Rust workspace
inside the container — 10–30 minutes, and several gigabytes of build cache — so pulling
is the faster path onto a server:

```bash
docker pull wesukilaye/ferroma:0.1.0
```

`docker-compose.prod.yml` and `docker-compose.external-db.yml` already default to that
repository; pin the release you want in `.env`:

```bash
FERROMA_VERSION=0.1.0                      # docker-compose.prod.yml: the tag to pull
# FERROMA_IMAGE=wesukilaye/ferroma:0.1.0   # docker-compose.external-db.yml: the whole reference
```

Available tags are `0.1.0` (exact release), `0.1` (latest patch of that minor) and
`latest` (newest release). For reproducible deployments pin the exact release, never
`latest`.

### Already have PostgreSQL and a reverse proxy?

Then there is nothing to assemble by hand. One script does the whole first run, and it
never edits your database server's configuration: the container shares the host's network
namespace, so the PostgreSQL you already run is reachable as `127.0.0.1:5432` and your
proxy reaches the API on `127.0.0.1:18080`.

```bash
git clone https://github.com/z1HwanG/Ferroma && cd Ferroma
./scripts/deploy.sh
```

It writes `.env` with generated secrets, creates the role and database, builds the image,
applies the migrations, installs your certificate for the SMTP/IMAP TLS listeners, starts
the stack, creates the first administrator, generates a DKIM key, and prints the DNS
records still to publish plus the reverse-proxy block to paste. After that:
`./scripts/deploy.sh status | logs | backup | upgrade | restore | dkim | certs | doctor | down`.
Every step, and the reasoning behind it, is in
[`docs/deployment.md`](docs/deployment.md) §3.1.

### Running it directly, without Docker

One command creates the database, applies the migrations and prints what to do next —
the whole first-run path, with no `psql` step:

```bash
cargo build --release --bin ferroma

# Point it at a PostgreSQL you can reach. Everything else has a default.
export DATABASE_URL=postgres://ferroma:secret@localhost:5432/ferroma

./target/release/ferroma database init        # creates the database, then migrates
./target/release/ferroma domain create example.com
./target/release/ferroma user create you@example.com --admin
./target/release/ferroma serve
```

`ferroma serve` binds SMTP on 25 and 587, IMAP on 143, and the API, Webmail and Admin
on 8080 — and prints exactly what it bound, so a port you did not expect to be taken
is visible immediately:

```text
smtp      0.0.0.0:25, 0.0.0.0:587
imap      0.0.0.0:143 (starttls), 0.0.0.0:0 (tls)
http      http://0.0.0.0:8080/api/v1
webmail   http://0.0.0.0:8080/
admin     http://0.0.0.0:8080/admin
```

Verify it from another terminal:

```bash
ferroma healthcheck            # exactly what the container HEALTHCHECK runs
curl -s localhost:8080/api/v1/health
```

### Run the preflight

`ferroma doctor` checks everything `serve` needs and says what would go wrong, before
you find out the hard way. It exits non-zero when something would stop the server
working:

```text
data-dir    ok    /var/lib/ferroma is writable
database    ok    ferroma, 2 migration(s) applied
ports       warn  reachable on loopback by another process: http 0.0.0.0:8080
                  binding 0.0.0.0 succeeds while another process holds 127.0.0.1 on
                  the same port, so a client on this host reaches that process instead.
tls         skip  disabled: STARTTLS, SMTPS, IMAPS and HTTPS are unavailable
jwt-secret  warn  api.jwt_secret is unset
                  every session is invalidated when the process restarts.

no blocking problems, 2 warning(s) worth reading.
```

Each of those checks exists because it has already gone wrong once. The port one in
particular catches a failure that is otherwise invisible: binding `0.0.0.0:8080`
succeeds even when another process holds `127.0.0.1:8080`, so the server reports a
healthy start while everything on the same host talks to the other program.

### Check DNS before you blame the server

Worth thirty seconds, because getting it wrong looks like a bug in Ferroma rather than
a fact about your resolver.
The inbound SPF/DKIM/DMARC step runs **before** the SMTP reply, so its DNS lookups are
on the critical path of every message. A resolver that answers costs ~7 ms and you
never notice. A resolver that **does not answer at all** — which is what happens for a
domain with no records yet, for an internal-only domain, or under a reserved TLD like
`.test` — is paid for in full timeout, once per lookup, before the sending MTA gets its
`250`:

```bash
ferroma config check --dns-domain example.com
# dns          ok (3 MX host(s) for example.com in 8 ms)
# ...or...
# dns          FAILED for example.com: no answer within 5s
```

If it fails, either point `[dns] resolvers` at a resolver that answers, lower
`dns.timeout_secs`, or accept that inbound mail will be slow until the domain's records
exist. `dns.timeout_secs × (attempts + 1)` is what the first lookup of every message
costs, which makes it the most latency-sensitive setting on the server.

One more trap worth knowing: `example.com`, `example.net` and `example.org` are **real
domains with real, hostile DNS** — IANA publishes `v=spf1 -all` and `p=reject` for
them. Hosting one locally means your own users' mail fails SPF and DMARC and is
quarantined into Junk. Use a domain you control, or a reserved TLD that genuinely has
no records (`.test`, `.invalid`, `.localhost`).

---

## What works

| Component | State |
|---|---|
| `ferroma-core` — configuration, errors, typed ids, addresses, limits, logging | done |
| `ferroma-mail` — RFC 5322 + MIME parsing and building, headers, flags, envelopes | done |
| `ferroma-storage` — PostgreSQL schema, repositories, Maildir, content-addressed attachments | done |
| `ferroma-events` — typed event bus with replay for reconnecting clients | done |
| `ferroma-auth` — Argon2id, HS256 access tokens, rotating refresh tokens, devices, throttling | done |
| `ferroma-smtp` — server, outbound client, MX resolution, DKIM/SPF/DMARC, inbound policy | done |
| `ferroma-imap` — IMAP4rev1 server, including IDLE, APPEND, MOVE and EXPUNGE | done |
| `ferroma-sync` — change log, cursors, idempotent client operations | done |
| `ferroma-api` — REST API, Ferroma Client Protocol, WebSocket, frontend hosting | done |
| `server` — the `ferroma` binary and its operator commands | done |
| Webmail, Admin | done |
| `client` — official desktop client **core** (sync, cache, outbox, search, multi-account) | done |
| `client` — desktop **graphical shell** | not built |

Everything above the last line is verified by the suite below, including an acceptance
run that starts the real server, delivers a message over SMTP, reads it back over IMAP,
finds it through the API, and syncs it with the real desktop client.

```text
cargo test --workspace    →  2376 passed, 0 failed, 0 skipped
cargo clippy --workspace  →  0 warnings, 0 errors
```

The official client ships as a tested shared core plus a CLI. The three-pane GUI
described in specification §51 is the one piece that is not built.

The full specification lives in [`Ferroma-完整项目书.md`](Ferroma-完整项目书.md) (Chinese,
64 sections). [`AGENTS.md`](AGENTS.md) explains the repository conventions.
A Chinese translation of this file is at [`README_zh.md`](README_zh.md).

---

## Documentation

| Document | What it covers |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | the system, the crate graph, and why it is shaped this way |
| [`docs/smtp.md`](docs/smtp.md) | inbound and outbound SMTP, reply codes, the open-relay policy |
| [`docs/imap.md`](docs/imap.md) | IMAP4rev1, folders, UIDs, flags, client compatibility |
| [`docs/storage.md`](docs/storage.md) | the schema, the Maildir, quotas, attachments, integrity |
| [`docs/api.md`](docs/api.md) | every HTTP endpoint, with examples |
| [`docs/fcp.md`](docs/fcp.md) | the Ferroma Client Protocol: sync cursor, realtime, devices |
| [`docs/sync.md`](docs/sync.md) | the synchronisation model in depth |
| [`docs/security.md`](docs/security.md) | the threat model and each control, plus known gaps |
| [`docs/deployment.md`](docs/deployment.md) | DNS, TLS, backups, upgrades, troubleshooting |
| [`docs/client.md`](docs/client.md) | the official client's architecture and features |

---

## Building from source

Ferroma targets stable Rust and PostgreSQL 14+.

```bash
cargo build --release --bin ferroma
cargo test --workspace
```

### Contributing

Development-environment details — including the two quirks of the machine this was
built on — live in [`AGENTS.md`](AGENTS.md), because they are about one checkout, not
about Ferroma.

One testing rule is worth repeating here, because it is about the code rather than the
machine. Integration tests read `FERROMA_TEST_DATABASE_URL` and do **not** skip
silently when it is unreachable: a skipped test that the harness counts as a pass is a
hollow green, and this repository has been burned by one. An unreachable database fails
the suite; pass `FERROMA_TEST_SKIP_WITHOUT_DATABASE=1` to skip explicitly.

---

## Repository layout

```text
crates/
  ferroma-core/      configuration, errors, typed ids, addresses, limits, logging
  ferroma-mail/      RFC 5322 + MIME: parsing, building, headers, flags, envelopes
  ferroma-storage/   PostgreSQL repositories + Maildir + attachment blob store
  ferroma-auth/      Argon2id, sessions, tokens, devices, login throttling
  ferroma-events/    the event bus (mail.received, mail.updated, …)
  ferroma-smtp/      SMTP server, SMTP client, MX resolution, DKIM/SPF/DMARC
  ferroma-imap/      IMAP4rev1 server
  ferroma-sync/      change log, cursors, idempotent client operations
  ferroma-api/       REST API, Ferroma Client Protocol, WebSocket, frontends
server/              the `ferroma` binary
client/              the official desktop client
web/, admin/         Webmail and Admin single-page apps (no build step)
migrations/          PostgreSQL DDL, embedded into the binary
config/              ferroma.toml — also embedded as the default configuration
docs/                architecture, protocol and operations documentation
scripts/             development and deployment helpers
tools/               the crates proxy and the HTTPS fetcher
```

---

## Design rules

1. **Protocol correctness before features.** SMTP and IMAP behaviour is tested against
   real command sequences, not assumed.
2. **One mail core.** SMTP, IMAP, Webmail and the Client API never implement their own
   message handling; they all go through `ferroma-mail` and the repositories.
3. **The server is never an open relay.** Unauthenticated peers can deliver only to
   local domains. This is enforced at the protocol edge, not configured hopefully.
4. **Server is the source of truth.** Clients keep a cache and a cursor; the server
   keeps the truth.
5. **Nothing a user typed is lost to a network error.** Client operations carry an
   idempotency key and survive a crash mid-request.
6. **rustls everywhere.** No OpenSSL, no schannel — one TLS implementation, in Rust.
7. **Never log secrets or message bodies.**

## Licence

MIT OR Apache-2.0.
