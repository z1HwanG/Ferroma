# AGENTS.md — working in this repository

Ferroma is a from-scratch, Rust-native self-hosted mail platform: its own SMTP and IMAP
servers, its own MIME and mail core, its own storage. It does not wrap Postfix, Dovecot
or Stalwart. The desktop client left in 0.1.8; this repository is the server, the
Webmail, the Admin console, and the Ferroma Client Protocol (FCP) a client talks to.
The licence is AGPL-3.0-only.

The design record is `docs/`: [`architecture.md`](docs/architecture.md) for the shape of
the system, [`api.md`](docs/api.md) and [`fcp.md`](docs/fcp.md) for the frozen wire
contracts, and [`security.md`](docs/security.md) for the threat model. The conventions
in §4 below are not negotiable. [`CONTRIBUTING.md`](CONTRIBUTING.md) is the shorter
version of the same agreement, including the checks a change has to pass.

The 64-section project book this repository was built from is no longer kept in the
tree. Documents and doc comments that cite it by section number — `specification §N`,
`项目书 §N` — are quoting a text that now lives only in git history, so treat the code
and the documents under `docs/` as what a change is measured against.

`AGENTS.md` has no Chinese mirror, and nothing in the repository requires one.

---

## 1. This machine is unusual — read this first

The scripts under `scripts/*.ps1` and the notes in this section describe **one Windows
development host**, not Ferroma, and not every checkout. On a machine where `cargo`
reaches crates.io and Docker is available, ignore §1.1–§1.2 and use the commands in
`README.md` and `docs/deployment.md`. Two quirks of that Windows host shape every build
done on it. Ignore them there and nothing compiles.

### 1.1 The Windows TLS stack is broken

`schannel` fails with `SEC_E_NO_CREDENTIALS`, so **anything that does HTTPS through
the OS fails**: `cargo`'s registry client, `curl.exe`, `git`, and .NET's
`SslStream`. Node.js ships its own OpenSSL and *does* work.

Two consequences:

* **crates.io access goes through a local proxy.** `tools/crates-proxy.mjs` re-serves
  the crates.io sparse index and crate archives over plain HTTP on
  `http://127.0.0.1:8931`, fetching upstream with Node. A gitignored, machine-local
  `.cargo/config.toml` replaces the `crates-io` source with it — committing that file
  would break `cargo build` everywhere else, which is why `.gitignore` and
  `.dockerignore` both exclude `.cargo/`. Start the proxy with
  `node tools/crates-proxy.mjs` (background) or let `scripts/cargo.ps1` start it.
* **Use rustls, never native-tls.** Every dependency that can choose a TLS backend is
  pinned to rustls in the workspace `Cargo.toml` (`runtime-tokio-rustls`,
  `reqwest` with `default-features = false` and `rustls-tls`). Do not add a crate that
  pulls in `native-tls`, `openssl`, or `schannel`. This is not just an environment
  workaround: rustls is the right choice for a server that terminates SMTP/IMAP TLS
  anyway.

For one-off HTTPS downloads, use `node tools/fetch.mjs <url> <dest>`.

### 1.2 `CARGO_HOME` lives inside the repository

That host's session file sandbox only permits writes inside the project tree, so
`CARGO_HOME` is redirected to `.cargo-home/` (and `CARGO_TARGET_DIR` to `target/`).
Source the environment first:

```powershell
. .\scripts\env.ps1          # or: .\scripts\cargo.ps1 <args>
cargo test --workspace
```

Concurrent agents share `target/` and will block on cargo's build lock. If you are
one of several agents working in parallel, give yourself a private target directory
(`target-<name>/`, which `.gitignore` already excludes):

```powershell
$env:CARGO_TARGET_DIR = Join-Path (Get-Location) 'target-<yourname>'
```

Do not hard-code another checkout's path. A private target directory is several
gigabytes; delete it when the work is done.

---

## 2. PostgreSQL for development and tests

On the Windows host there is no Docker. A minimal PostgreSQL 16 distribution lives in
`.cache/pgsql/` and a cluster in `.cache/pgdata/`, started by:

```powershell
.\scripts\dev-postgres.ps1 start     # listens on 127.0.0.1:5433, trust auth
.\scripts\dev-postgres.ps1 status
.\scripts\dev-postgres.ps1 stop
```

`pg_ctl` cannot be used on that host (it fails to create a restricted token); the
script runs `postgres.exe` directly. `pg_ctl stop` would not help either, because
stopping a server means signalling it, and that is denied too.

### 2.1 The cluster is fragile in one specific way — read this before killing anything

That sandbox forbids cross-process signalling. Two consequences, both of which have
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

The deployment is `docker-compose.yml`: Ferroma only, on the host's network, and the
setup page asks for the PostgreSQL the host already runs. `docker-compose.demo.yml`
is a demonstration — it builds the checkout and starts its own database, in plaintext.
`scripts/deploy.sh` drives the deployment file. See `docs/deployment.md` §3.1.

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
migrations/          PostgreSQL DDL, embedded into the binary at compile time
config/              ferroma.toml — also embedded as the default configuration
web/, admin/         Webmail and Admin single-page apps (no build step)
shared/              the ES modules both apps import (served at /shared)
docs/                architecture, protocol and operations documentation
  zh/                the Chinese half of every document under docs/
scripts/             development helpers (PowerShell, for the host in §1) and deploy.sh
tools/               doc checks, the crates proxy, the HTTPS fetcher, probes
```

There is no `client/` directory. FCP stays, in `docs/fcp.md` and `ferroma-api`; a
desktop client is a separate project.

---

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
   API all go through `ferroma-mail` and the repositories; none of them implements
   its own message handling.
6. **The server is never an open relay.** Unauthenticated peers may deliver only to
   local domains. There is no `allow_relay` key to discover and set.
7. **Never log** passwords, tokens, private keys, or full message bodies.
8. **Timestamps are `TIMESTAMPTZ` and UTC** at every layer.
9. **Migrations are forward-only, and an applied file is frozen.** `migrations/` is an
   ordered list applied at startup when `database.run_migrations = true`. sqlx records
   a SHA-384 of each file and refuses to start (`VersionMismatch`) if one changes —
   a comment is enough. A schema change is a new file, never an edit of
   `0001_initial.sql` or anything after it. There is no down migration, so an upgrade
   that applies one cannot be rolled back by swapping the binary; the database has to
   be restored from the pre-upgrade dump (`docs/deployment.md` §9).

---

## 5. Testing

* Unit tests live in `#[cfg(test)] mod tests` next to the code and are expected to be
  thorough — protocol parsers especially. Every crate has real fixtures.
* Integration tests live in `crates/*/tests/` and use that crate's
  `tests/common/mod.rs` helper for a disposable, migrated database.
* The end-to-end acceptance run is `server/tests/e2e.rs`. It drives a real server
  process over real sockets: SMTP submission, IMAP retrieval, the HTTP API, sync and
  WebSocket. It is part of `cargo test --workspace`, not a separate harness.
* The whole suite is `cargo test --workspace`. `CONTRIBUTING.md` runs it `--offline`,
  which is right once the registry cache is warm and wrong on a checkout that still
  has crates to fetch.

---

## 6. Definition of done

A change is done when: it compiles without new warnings, its tests pass, it is
documented, and the behaviour it claims is covered by a test that would fail if the
behaviour regressed. "It compiles" is not done.

```bash
cargo test --workspace
node tools/check-docs.mjs       # every relative link and anchor, and the en/zh pairs
node tools/check-zh.mjs         # terminology, pairing, and the language of each reference
node tools/check-diagrams.mjs   # box-drawing rows share a last column
node tools/check-deploy.mjs     # deployment artefacts, the Dockerfile included
node tools/check-web.mjs        # module graph, ids, i18n coverage of web/ and admin/
node tools/site-check.mjs       # the generated site publishes both languages
```

The front-end check fails on an element id that no view defines, on a `t('…')` string
the Chinese catalog does not cover, and on a `fetch()` outside `shared/api.js`. Each of
those has already shipped as a bug once.

A **documentation** change is done when `node tools/check-docs.mjs` and
`node tools/check-zh.mjs` pass: every relative link and anchor resolves, a Chinese
document links to its Chinese sibling rather than to the English file it mirrors, each
pair keeps the same headings and code fences, and `docs/dockerhub.md` still carries both
languages with English first. Both checks run in CI before a release image is built, so
a reference that breaks in a later edit fails the release instead of the reader.

A **page** is done the same way, and the coverage is checked rather than remembered: every document
under `docs/` is a pair, and the generated site publishes both languages or the check fails.
`node tools/site-check.mjs` reports a page that exists in only one language, and one that does not
link to its counterpart; `node tools/check-zh.mjs` reports a document with no Chinese version and a
Chinese document whose English original is gone. A document that is *meant* to be English-only goes
in `check-zh.mjs`'s `enOnly` set with a reason — a missing pair is invisible to every reader who
reads only one language, which is why it fails rather than warns.

A **figure** is done when `node tools/check-diagrams.mjs` passes: inside a fenced block, every row
carrying the same number of box-drawing characters must put its last one in the same display column.
Display columns, not character indices — a CJK character is two columns wide, so the same picture
needs different arithmetic in English and in Chinese, and measuring the wrong one turns an aligned
figure into a false finding. Rows from boxes side by side or nested carry different counts and are
excluded by construction; that is what keeps the rule from firing on every crate graph in
`architecture.md`.

A release tag has to equal `Cargo.toml`'s `version`. The image workflow refuses a tag
that disagrees with the manifest, so a tag cannot label a tree it did not build.
