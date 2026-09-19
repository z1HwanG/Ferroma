# AGENTS.md — working in this repository

Ferroma is a from-scratch, Rust-native self-hosted mail platform. Its design record is
`docs/`: [`architecture.md`](docs/architecture.md) for the shape of the system,
[`api.md`](docs/api.md) and [`fcp.md`](docs/fcp.md) for the frozen wire contracts, and
[`security.md`](docs/security.md) for the threat model. The conventions in §4 below are
not negotiable.

The 64-section project book this repository was built from is no longer kept in the
tree. Documents and doc comments that cite it by section number — `specification §N`,
`项目书 §N` — are quoting a text that now lives only in git history, so treat the code
and the documents under `docs/` as what a change is measured against.

---

## 1. This machine is unusual — read this first

Two environment quirks shape every build here. Ignore them and nothing compiles.

### 1.1 The Windows TLS stack is broken

`schannel` fails with `SEC_E_NO_CREDENTIALS`, so **anything that does HTTPS through
the OS fails**: `cargo`'s registry client, `curl.exe`, `git`, and .NET's
`SslStream`. Node.js ships its own OpenSSL and *does* work.

Two consequences:

* **crates.io access goes through a local proxy.** `tools/crates-proxy.mjs` re-serves
  the crates.io sparse index and crate archives over plain HTTP on
  `http://127.0.0.1:8931`, fetching upstream with Node. The workspace
  `.cargo/config.toml` replaces the `crates-io` source with it. Start it with
  `node tools/crates-proxy.mjs` (background) or let `scripts/cargo.ps1` start it.
* **Use rustls, never native-tls.** Every dependency that can choose a TLS backend is
  pinned to rustls in the workspace `Cargo.toml`. Do not add a crate that pulls in
  `native-tls`, `openssl`, or `schannel`. This is not just an environment workaround:
  rustls is the right choice for a server that terminates SMTP/IMAP TLS anyway.

For one-off HTTPS downloads, use `node tools/fetch.mjs <url> <dest>`.

### 1.2 `CARGO_HOME` lives inside the repository

The session file sandbox only permits writes inside the project tree, so
`CARGO_HOME` is redirected to `.cargo-home/` (and `CARGO_TARGET_DIR` to `target/`).
Always source the environment first:

```powershell
. .\scripts\env.ps1          # or: .\scripts\cargo.ps1 <args>
cargo test --workspace
```

Concurrent agents share `target/` and will block on cargo's build lock. If you are
one of several agents working in parallel, give yourself a private target directory:

```powershell
$env:CARGO_TARGET_DIR="C:\Users\25688\Documents\DSH\Ferroma\target-<yourname>"
```

---

## 2. PostgreSQL for development and tests

There is no Docker on this machine. A minimal PostgreSQL 16 distribution lives in
`.cache/pgsql/` and a cluster in `.cache/pgdata/`, started by:

```powershell
.\scripts\dev-postgres.ps1 start     # listens on 127.0.0.1:5433, trust auth
.\scripts\dev-postgres.ps1 status
.\scripts\dev-postgres.ps1 stop
```

`pg_ctl` cannot be used on this host (it fails to create a restricted token); the
script runs `postgres.exe` directly. `pg_ctl stop` would not help either, because
stopping a server means signalling it, and that is denied too.

### 2.1 The cluster is fragile in one specific way — read this before killing anything

This sandbox forbids cross-process signalling. Two consequences, both of which have
already cost real time:

1. **Never force-kill the cluster, and never run anything that makes PostgreSQL
   terminate backends.** Forbidden: `pg_ctl stop`, `pg_terminate_backend`,
   `DROP DATABASE … WITH (FORCE)`, and `Stop-Process` on `postgres.exe`.
2. **A crash can be unrecoverable.** The checkpointer's own timer-driven checkpoints
   are fine — no signal involved. But any checkpoint *requested* by another process,
   including the end-of-recovery checkpoint the startup process performs after crash
   recovery, needs `kill(2)`, fails with

   ```text
   FATAL:  could not signal for checkpoint: Operation not permitted
   LOG:    startup process (PID …) exited with exit code 1
   LOG:    shutting down due to startup process failure
   ```

   and the server exits. On restart it replays the same WAL, hits the same wall, and
   loops forever — 35 restarts in a row is what that looked like.

   **Escape hatch: `.\scripts\dev-postgres.ps1 reinit`.** It moves the data directory
   aside and runs `initdb` again. A cluster that shut down *cleanly* has no WAL to
   replay, so it never requests a checkpoint. Losing a test cluster costs nothing.
   `scripts/pg-supervisor.ps1` keeps the cluster alive across ordinary failures but
   cannot rescue a recovery loop.

Because the supervisor may be mid-restart when a suite starts, the integration-test
harnesses retry their first connection for a few seconds before giving up.

Connection string for tests:

```text
postgres://ferroma@127.0.0.1:5433/postgres
```

Integration tests read `FERROMA_TEST_DATABASE_URL`, defaulting to the above. They do
**not** skip silently when the database is unreachable: a skipped test that the
harness counts as a pass is a hollow green, and this repository has already been
burned by one (a crashed cluster produced "56 passed, 0 failed" in 12 seconds with
every test skipped). An unreachable database therefore **fails** the suite, and the
operator opts into skipping explicitly:

```powershell
$env:FERROMA_TEST_SKIP_WITHOUT_DATABASE = "1"   # only when you really have no database
cargo test --workspace
```

The target deployment is Docker Compose, in one of two shapes. A host with nothing but
Docker uses `docker-compose.prod.yml`, which brings its own PostgreSQL container. A host
that **already runs PostgreSQL** (the common case for the server this is deployed to)
uses `docker-compose.external-db.yml` — `scripts/deploy.sh` drives it, the container runs
with `network_mode: host` so the existing database is reached over `127.0.0.1`, and HTTPS
is left to the reverse proxy that is already there. See `docs/deployment.md` §3.1.

---

## 3. Repository layout

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
  ferroma-api/       REST API, Ferroma Client Protocol (FCP), WebSocket, frontends
server/              the `ferroma` binary: wires everything together
client/              the official desktop client (core + UI shell)
migrations/          PostgreSQL DDL, embedded into the binary at compile time
config/              ferroma.toml — also embedded as the default configuration
web/, admin/         Webmail and Admin single-page apps
shared/              the ES modules both apps import (served at /shared)
docs/                architecture, protocol and operations documentation
scripts/             development and deployment helpers
tools/               the crates proxy and the HTTPS fetcher
```

## 4. Conventions that are not negotiable

1. **Every public item is documented.** Crates carry `#![warn(missing_docs)]`.
2. **Errors are `ferroma_core::FerromaError`.** Storage maps `sqlx::Error` into
   `StorageError` first and converts at the boundary, so `ferroma-core` never
   depends on a database driver.
3. **No `sqlx::query!` macros.** They require a live database at *compile* time.
   Use `sqlx::query_as` with `#[derive(FromRow)]` row structs from
   `ferroma-storage::models`.
4. **No `unwrap()` on untrusted input.** Peers control SMTP commands, IMAP literals,
   MIME structures and HTTP bodies. `unwrap()` belongs in tests only.
5. **Protocol layers contain no business logic.** SMTP, IMAP, Webmail and the Client
   API all go through the mail core and the repositories; none of them implements
   its own message handling.
6. **The server is never an open relay.** Unauthenticated peers may deliver only to
   local domains.
7. **Never log** passwords, tokens, private keys, or full message bodies.
8. **Timestamps are `TIMESTAMPTZ` and UTC** at every layer.

## 5. Testing

* Unit tests live in `#[cfg(test)] mod tests` next to the code and are expected to be
  thorough — protocol parsers especially. Every crate has real fixtures.
* Integration tests live in `crates/*/tests/` and use the shared
  `tests/common/mod.rs` helper for a disposable, migrated database.
* The end-to-end acceptance run (`tests/e2e/`) drives a real server process over
  real sockets: SMTP submission, IMAP retrieval, HTTP API, sync and WebSocket.
* Everything is run with `cargo test --workspace`.

## 6. Definition of done

A change is done when: it compiles without new warnings, its tests pass, it is
documented, and the behaviour it claims is covered by a test that would fail if the
behaviour regressed. "It compiles" is not done.
