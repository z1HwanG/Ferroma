# Changelog

All notable changes to Ferroma, newest first.

Every released version is a `vX.Y.Z` tag whose value must equal `Cargo.toml`'s, because
`.github/workflows/docker-publish.yml` refuses a tag that disagrees with the manifest —
so a tag cannot label a tree it did not build. Until 1.0 a minor bump may change
anything and a patch bump fixes it.

The headings follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) in spirit:
a section a release has nothing for is left out rather than written empty.

## [Unreleased]

### Known issues carried into 0.1.4

Both were found while cutting 0.1.3. Neither is a regression in it, and neither is
fixed by a mechanical edit.

- **A published image cannot say which build it is.** `ferroma version` inside the
  container and the Admin sidebar both report `built: unknown` / `revision: unknown`.
  `ferroma-core/src/version.rs` reads `FERROMA_BUILD_TIMESTAMP` and `FERROMA_GIT_SHA`
  with `option_env!`, while the `Dockerfile` passes its build identity only to the OCI
  labels — so `docker inspect` names the commit and the binary itself cannot. The
  label is correct; the fix and its one caveat are in
  [README — Carried into 0.1.4](README.md#carried-into-014).
- **Eight documents still describe skeletons.** They carry 80 `_(planned)_` claims
  written before those crates existed, and five of them (`client.md`, `imap.md`,
  `security.md`, `smtp.md`, `sync.md`) still open with a status banner calling
  implemented crates unimplemented. Each claim is an assertion about behaviour that has
  to be checked against the code, so this is a read rather than a search-and-replace.

## [0.1.3] — 2026-09-19

### Added

- **Simplified Chinese, next to English, in both front-ends.** English is the source
  language and its text *is* the lookup key, so a string with no translation renders in
  English instead of breaking; `shared/locales/zh-CN.js` holds 881 entries. The
  language picker is in Settings, the choice is remembered per browser, and the API is
  asked for error messages in it.
- **The API localises its error messages** from `Accept-Language` (RFC 9110 q-values).
  `code` stays language-neutral. The message is translated in two halves — the error
  kind and its detail — which is what covers 126 `format!` call sites without threading
  a message key through each of them.
- **`shared/`, one copy of the modules both front-ends use.** The server mounts it at
  `/shared`, so `../shared/api.js` resolves to the same URL from `/main.js` and from
  `/admin/main.js`; `api.shared_dir` names the directory and the image bakes it in.
- `tools/serve-frontends.mjs` — serves `web/`, `admin/` and `shared/` with the same
  three mounts the router creates, for front-end work with no server and no database.
- `ferroma user create --disabled`, matching `POST /users` with `enabled: false`.

### Changed

- `/auth/me` returns full mailbox records rather than the reduced address brief, so a
  client can read each address's quota and current size.
- `GET /users` carries each account's addresses; `GET /audit` resolves the acting
  account's address instead of sending only its id.
- `GET /setup` answers `404` when the wizard is disabled, rather than `200` with
  `required: false` — a client could not tell "disabled" from "an administrator already
  exists". A submitted `hostname` that disagrees with `server.hostname` is `400` naming
  the setting, and is validated before anything is written.
- An unknown `/api/v1` path, and a known path with the wrong method, answer the error
  envelope `docs/api.md` §1.3 promises. An unknown path previously fell through to the
  Webmail's SPA fallback and returned `index.html` with a `200`.
- The Admin console gained a filter bar, a sortable and selectable table, a detail
  drawer and a real section layout; `hidden` now hides what it marks.
- The Webmail's compose dialog can send and save again.

### Fixed

- **DKIM signing runs on outbound mail.** `[dkim]` was fully implemented — key parsing,
  canonicalisation, signing — with no production caller, so a deployment with DKIM
  enabled, a published selector and a matching key still sent every message unsigned.
- The Webmail showed every message as unread and hid every star: the API sends the
  canonical lower-case flag string (`seen flagged`, frozen by `docs/fcp.md` §4) and the
  client matched `\Seen`. The flags are also sent as booleans now.
- `INBOX` was never recognised, because it carries no `special_use` — RFC 6154 defines
  no `\Inbox`. It is identified by name, and standard folders render under a display
  name while `folder.name` stays IMAP data and is never translated.
- `POST /users` silently dropped `enabled`, so the console's "Account enabled" box did
  nothing; clearing a display name reported success and kept the old name.
- Four of the Admin dashboard's five cards rendered the string `undefined`.
- A wrong current password was retried as an expired session; `304 Not Modified` was
  treated as a failure by attachment downloads; badge colours were lost when their
  label was translated before the colour was derived from it.
- `doctor`'s loopback-shadow test asserted Windows' `SO_REUSEADDR` semantics and failed
  on every Linux host.
- `ferroma-api`'s integration harness created its shared test database with a
  check-then-create that raced against every other test in the binary.
- `scripts/docker-publish.sh` is executable, which its own usage examples assume.

### Removed

- `backup/` and `.probe/` — local scratch that was never tracked: a bundle of a commit
  already in history, and a previous audit's throwaway proofs of concept.

## [0.1.2] — 2026-09-17

### Added

- The Admin app is served at `/admin/`, with the directory redirect that makes its
  relative asset URLs resolve inside the Admin directory rather than the Webmail's.

### Fixed

- Both front-ends read the field names the API actually sends, and fill the sections
  that had been shipping empty.
- The front-end static checks run where they are documented to run.

## [0.1.1] — 2026-09-17

### Fixed

- An undefined `renderChrome` reference that stopped the Webmail from starting at all.

## [0.1.0] — 2026-09-17

The first release: SMTP, IMAP and the HTTP API in one process, over PostgreSQL
metadata, a Maildir for message bytes and a content-addressed store for attachments,
with the Webmail and Admin apps and the official client's core and CLI.

### Added

- An outbound relay, for hosts whose IP has no PTR record.
- Docker Hub publishing, driven by a `v*` tag.

### Changed

- The operator scripts are executable.

### Fixed

- `scripts/deploy.sh` fails fast when PostgreSQL is unreachable, can provision a
  containerised PostgreSQL, survives a cluster with no `postgres` database, and asks
  the container who its superuser is instead of assuming.
- `ferroma database init` creates a missing database without assuming a `postgres`
  database exists.
- TLS settings are carried into the IMAP configuration, and `FERROMA_PUBLIC_URL`
  reaches the server under the name the server actually reads.
