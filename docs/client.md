# The official Ferroma client

**Who should read this:** anyone implementing `client/`, anyone deciding whether a
feature belongs in the desktop client or on the server, and anyone testing
multi-device behaviour.

Ferroma Client is the official desktop application for Windows, Linux and macOS
(Android and iOS are second-phase). It is organised as a platform-independent
**shared core** plus a thin UI shell, because the same account, sync engine,
cache, Outbox and search have to behave identically on three platforms and,
later, on two more. This document specifies the shared-core architecture of
specification §24 and §25, the directory responsibilities of §25, the SQLite cache
schema of §26 and the `Server = Source of Truth` rule, the sync engine and its
pending-operations queue, offline mode (§27), the Outbox state machine (§28) with
its eight states, attachment caching and streaming (§29), local search with
server fallback (§30), multi-account (§31), autodiscovery via `.well-known/ferroma`
(§32), device management (§33), notifications (§34) including the APNs/FCM
reservation, the settings surface (§52) and the UI plan (§51).

> **Status:** design specification. The `ferroma-client` crate is a skeleton —
> `client/src/lib.rs` declares the module list and `client/src/main.rs` is a stub,
> and `Cargo.toml` already pulls in the dependencies the design needs (`sqlx` with
> the `sqlite` feature, `reqwest`, `tokio-tungstenite`, `clap`, `dirs`). **Every
> module below is therefore _(planned)_**, and the SQLite schema in §3 is a
> specification, not an existing migration. The protocols it speaks are frozen in
> [fcp.md](fcp.md) and [api.md](api.md), and both are implemented on the server
> side of the contract only insofar as `ferroma-api` is implemented — it is not
> yet either.

---

## 1. Scope and relationship to IMAP

Specification §53 draws the line, and [fcp.md](fcp.md) §1 repeats it:

```text
                    Ferroma
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
      Client API      IMAP         SMTP
          │            │            │
          ▼            ▼            ▼
     官方客户端    Thunderbird   Outlook
                   Apple Mail
```

| Client | Protocol | Why |
|---|---|---|
| **Official Ferroma Client** | FCP over HTTPS + WebSocket | needs incremental sync with a cursor, server-side drafts, device management, chunked attachments and push — none of which IMAP can express |
| Thunderbird, Apple Mail, Outlook, iPhone Mail, Android | IMAP4rev1 + SMTP | they exist, they work, and Ferroma must not break them ([imap.md](imap.md) §11) |

The official client **never speaks IMAP or SMTP**. It has no IMAP parser, it does
not open port 143, and it does not implement SMTP submission. Everything goes
through `/api/v1/client`.

That is a deliberate narrowing: it means the desktop client needs exactly one
transport, one authentication scheme, one error vocabulary and one sync model, and
it means a server-side bug in the IMAP layer cannot affect it. The cost is that
the FCP surface must be complete — a feature that only exists over IMAP is a
feature the official client cannot have.

---

## 2. Architecture: shared core + UI

Specification §24.2:

```text
                    Ferroma Client
                          │
             ┌────────────┴────────────┐
             │                         │
        Shared Core                    UI
             │                         │
      ┌──────┼──────┐          ┌───────┼───────┐
      ▼      ▼      ▼          ▼       ▼       ▼
    Sync    API     DB        Windows  Linux   macOS
```

The rule that makes this worth stating: **the UI is a consumer of the core, never
a participant in it.** The UI does not open sockets, does not hold a cursor and
does not write the cache. It calls the core and reacts to events the core emits.

```text
   ┌──────────────────────────── UI shell (per platform) ─────────────────────┐
   │  window, folder pane, list, reader, composer, notifications, settings    │
   └────────────────────────────────┬─────────────────────────────────────────┘
                                    │  core API + a stream of state changes
   ┌────────────────────────────────▼─────────────────────────────────────────┐
   │                        ferroma-client core (Rust)                        │
   │                                                                          │
   │   account ── api ── sync ── database ── mail ── draft ── outbox          │
   │                    │                     │                               │
   │                    └── attachment ── search ── notification ── device    │
   │                                                                          │
   │             settings (local + server-mirrored)                           │
   └────────────────────────────────┬─────────────────────────────────────────┘
                                    │
                       ┌────────────┴────────────┐
                       ▼                         ▼
                SQLite cache               HTTPS + WebSocket
                       │                         │
                       └────────────┬────────────┘
                                    ▼
                          Ferroma Server (FCP)
```

Why a Rust core rather than platform-native code per platform:

| Property | Consequence |
|---|---|
| One implementation of sync, cache and Outbox | the hardest logic in the client is written and tested once, not three times |
| Same SQLite schema everywhere | a bug in the cache is one bug, and the migration path is one path |
| Same FCP client everywhere | a protocol change is one change |
| Platform UI can be whatever fits | Windows, Linux and macOS get a native shell; a mobile shell can be added without touching the core |
| `ferroma-core` is shared with the server | `FerromaError`, typed ids, `Cursor` and address parsing are literally the same types ([architecture.md](architecture.md) §2) |

The core is a library (`client/src/lib.rs`); the platform shell is a separate
binary or process that links it. Specification §6.3 suggests **Slint** if the
project emphasises a Rust-native client and **Tauri + web UI** otherwise; the
choice is the UI's, not the core's, and the core must not depend on it. Note the
current manifests: `client/Cargo.toml` depends on `ferroma-core` only — not on
`ferroma-storage`, not on `sqlx`'s PostgreSQL feature. A desktop client must not
link a PostgreSQL driver or a Maildir.

**Status: the core is built, the shell is not.** Everything in §2 and §3–§13 ships
and is tested — 330 test functions over sync, the SQLite cache, the Outbox, offline
queues, local search, attachments, multi-account, autodiscovery and settings, driven
by a real CLI (`ferroma-client`) that runs against a real server in the acceptance
run. What does not exist is the three-pane window (§14). The seam it would be built
on is `ui.rs`: `ClientHandle` (`open`, `folders`, `messages`, `open_message`, `send`,
`sync`, `search`, `outbox_counts`, `load_settings`/`save_settings`) plus a
`subscribe()` broadcast of `ClientEvent` for sync progress and new mail — a shell
drives those and owns nothing else.

Both candidate toolchains were **measured to compile on the development host** before
deferring the work, so the choice can be made on merit rather than on what the
machine supports:

| Toolkit | Probe result |
|---|---|
| **Tauri 2.11** (`tauri` + `tauri-build`, WebView2 backend) | `cargo check` clean in ~2m25s; `wry`, `tao`, `webview2-com` all build |
| **Slint 1.18** (`slint`, software and femtovg renderers) | `cargo check` clean in ~2m18s; needs no system webview |

The Windows toolchain has the C++ build tools and the Windows SDK it needs, and
WebView2 is present on this OS — so Tauri would work, but Slint avoids depending on a
system webview entirely, which is the more Rust-native answer §6.3 leans towards.

---

## 3. The SQLite cache (§26)

### 3.1 Tables

Specification §26 lists the tables. Each has a server-side counterpart, and the
client's copy is a *cache*, which is what determines which columns exist and which
do not:

| Table | Server counterpart | Cached? |
|---|---|---|
| `accounts` | — (client-only) | the local list of configured accounts; the server has no equivalent because it is the server |
| `mailboxes` | `mailboxes` + `folders` | flattened: the client wants "folder id → name, counts, uid_validity" in one row |
| `messages` | `messages` | metadata only: ids, flags, subject, sender, dates, size, `has_attachments` |
| `message_headers` | `message_recipients` + parsed headers | the full header list for the reader, fetched on demand |
| `attachments` | `attachments` | metadata plus a local cache path when the bytes have been downloaded |
| `drafts` | `drafts` | full content: a draft is the one thing that must survive being offline |
| `outbox` | `mail_queue` (read-only mirror) | pending operations and sends, with their `operation_id` |
| `sync_state` | `client_sync_states` | the cursor per account/mailbox/folder |
| `devices` | `devices` | the device list for the settings screen |
| `settings` | `settings` | local UI settings, plus a mirror of server-side values |

Plus one table specification §26 does not list but the design needs:

| Table | Purpose |
|---|---|
| `search_index` | the local search index of §30; an FTS5 virtual table over subject, sender, recipients and body text |

### 3.2 A concrete schema

_(planned)_ — this is the shape the cache should have, so that the sync engine's
operations are all single-row upserts and the UI's queries are all index-backed.

```sql
-- One row per configured account.
CREATE TABLE accounts (
    id              INTEGER PRIMARY KEY,
    server_id       INTEGER NOT NULL,              -- users.id on the server
    email           TEXT    NOT NULL UNIQUE,
    display_name    TEXT,
    api_base        TEXT    NOT NULL,              -- https://mail.example.com/api/v1
    access_token    TEXT,                          -- see §3.4
    refresh_token   TEXT,
    token_expires_at INTEGER,
    device_uid      TEXT    NOT NULL,              -- stable per installation
    device_id       INTEGER,                       -- server devices.id
    paused          INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    last_sync_at    INTEGER
);

-- One row per folder, per account. Mirrors folders + the counts from
-- GET /api/v1/client/mailboxes.
CREATE TABLE mailboxes (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,              -- folders.id
    address_id      INTEGER NOT NULL,              -- mailboxes.id on the server
    name            TEXT    NOT NULL,              -- "INBOX", "Archive/2026"
    special_use     TEXT,                          -- "\Sent", "\Drafts", …
    message_count   INTEGER NOT NULL DEFAULT 0,
    unseen_count    INTEGER NOT NULL DEFAULT 0,
    uid_validity    INTEGER NOT NULL DEFAULT 0,
    uid_next        INTEGER NOT NULL DEFAULT 0,
    subscribed      INTEGER NOT NULL DEFAULT 1,
    UNIQUE (account_id, server_id)
);

-- Message metadata. NOT the body: bodies are fetched on demand (§5).
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,              -- messages.id, the API's message_id
    folder_id       INTEGER NOT NULL,              -- mailboxes.id above
    uid             INTEGER NOT NULL,
    uid_validity    INTEGER NOT NULL,              -- the generation the uid belongs to
    rfc_message_id  TEXT,
    thread_id       TEXT,
    subject         TEXT,
    sender          TEXT,
    sender_name     TEXT,
    snippet         TEXT,
    flags           TEXT NOT NULL DEFAULT '',      -- "seen,flagged", Flags::to_db_string form
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    attachment_count INTEGER NOT NULL DEFAULT 0,
    internal_date   INTEGER NOT NULL,
    sent_at         INTEGER,
    body_cached     INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, server_id)
);

CREATE INDEX messages_folder_date_idx ON messages (folder_id, internal_date DESC);
CREATE INDEX messages_account_idx     ON messages (account_id, internal_date DESC);
CREATE INDEX messages_rfc_id_idx      ON messages (account_id, rfc_message_id)
    WHERE rfc_message_id IS NOT NULL;
CREATE UNIQUE INDEX messages_folder_uid_key ON messages (folder_id, uid);

-- The full header list, for the reader. Fetched with the body, not with the list.
CREATE TABLE message_headers (
    id          INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL DEFAULT 0,
    name        TEXT    NOT NULL,
    value       TEXT    NOT NULL
);
CREATE INDEX message_headers_message_idx ON message_headers (message_id, ordinal);

-- Attachment metadata plus the local cache location.
CREATE TABLE attachments (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,              -- attachments.id
    message_id      INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename        TEXT,
    content_type    TEXT,
    size_bytes      INTEGER NOT NULL DEFAULT 0,
    sha256          TEXT,                          -- the server's content address
    content_id      TEXT,
    is_inline       INTEGER NOT NULL DEFAULT 0,
    cached_path     TEXT,                          -- NULL = not downloaded
    cached_bytes    INTEGER NOT NULL DEFAULT 0,
    cached_at       INTEGER
);
CREATE INDEX attachments_message_idx ON attachments (message_id);
CREATE INDEX attachments_sha_idx     ON attachments (account_id, sha256);

-- Drafts: full content, because a draft must survive being offline.
CREATE TABLE drafts (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER,                       -- NULL until first synced
    subject         TEXT,
    body_text       TEXT,
    body_html       TEXT,
    recipients_json TEXT NOT NULL DEFAULT '[]',
    attachments_json TEXT NOT NULL DEFAULT '[]',
    in_reply_to     TEXT,
    references_json TEXT NOT NULL DEFAULT '[]',
    updated_at      INTEGER NOT NULL,
    server_updated_at INTEGER,
    dirty           INTEGER NOT NULL DEFAULT 0    -- 1 = local edit not yet pushed
);

-- The Outbox: pending operations AND pending sends (§6, §8).
CREATE TABLE outbox (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    operation_id    TEXT    NOT NULL UNIQUE,       -- generated when the user acted
    kind            TEXT    NOT NULL,              -- send | mark_read | move | flag | delete | draft_save …
    payload_json    TEXT    NOT NULL,
    state           TEXT    NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX outbox_due_idx ON outbox (account_id, next_attempt_at)
    WHERE state IN ('pending', 'retrying');

-- The cursor. One row per account/mailbox/folder; folder_id = 0 is the
-- account level, mirroring client_sync_states' COALESCE(folder_id, 0).
CREATE TABLE sync_state (
    id          INTEGER PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id  INTEGER NOT NULL,
    folder_id   INTEGER NOT NULL DEFAULT 0,
    cursor      INTEGER NOT NULL DEFAULT 0,
    last_full_sync_at INTEGER,
    UNIQUE (account_id, mailbox_id, folder_id)
);

CREATE TABLE devices (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id       INTEGER NOT NULL,
    device_uid      TEXT    NOT NULL,
    name            TEXT,
    platform        TEXT,
    client_version  TEXT,
    last_seen_at    INTEGER,
    revoked         INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, server_id)
);

CREATE TABLE settings (
    account_id  INTEGER NOT NULL DEFAULT 0,         -- 0 = application-wide
    key         TEXT    NOT NULL,
    value       TEXT    NOT NULL,
    PRIMARY KEY (account_id, key)
);

-- Local search (§9). FTS5 over the fields a user searches.
CREATE VIRTUAL TABLE search_index USING fts5(
    subject, sender, recipients, body,
    content='',                                     -- contentless: we store the text ourselves
    tokenize='unicode61'
);
```

### 3.3 `Server = Source of Truth`

Specification §26 and §55, elaborated in [sync.md](sync.md) §2. Applied to the
schema above, it produces four rules:

1. **Every row is a cache of something the server owns.** `server_id` is the
   identity; the local `id` is an implementation detail and must never be sent to
   the server as if it were a server id.
2. **`sync_state.cursor` is the only thing the client owns authoritatively.** It
   records how far *this installation* has read, and it is meaningless to any
   other device.
3. **`outbox` rows are intents, not facts.** Until the server acknowledges an
   operation, the row is a wish. The UI may show the optimistic result; it must
   not present it as confirmed.
4. **`drafts.dirty` is the only local write that can win a conflict** — §7.

What this means in practice: the client can always rebuild its entire database
from the server. There is no client-side data whose loss is unacceptable, with
exactly two exceptions — the cursor, whose loss costs a resync, and the Outbox,
whose loss costs the user typed work. That is why `outbox` is the table that gets
the most care.

### 3.4 Token storage

Specification §38 puts "Token 安全存储" (secure token storage) in the client-layer
controls. The rules:

| Platform | Where |
|---|---|
| Windows | Credential Manager (`windows-credentials` / DPAPI) |
| macOS | Keychain (`security` framework) |
| Linux | Secret Service (libsecret / gnome-keyring) via the platform's keyring crate |

**Not in SQLite in plaintext, and not in a config file.** The `accounts` table
above shows `access_token` / `refresh_token` columns because the design needs a
place to reference them; on a platform with a keyring they hold an opaque handle,
and on a platform without one the design must refuse to store the refresh token at
all rather than write it to disk unprotected.

`device_uid` is NOT a secret. It is a stable identifier for the installation,
generated once and kept, and it is what makes device revocation target the right
installation ([fcp.md](fcp.md) §9).

---

## 4. Directory responsibilities (§25)

Specification §25 gives the directory list and the core responsibilities.

| Module | Responsibility | What it must not do |
|---|---|---|
| `app/` | process lifecycle: single-instance lock, startup, shutdown, the top-level event loop that ties the core to the UI | contain business logic |
| `account/` | add / remove / pause / re-authenticate an account; hold the per-account core context | talk HTTP directly — that is `api/` |
| `api/` | the FCP HTTP client: request building, the `X-Ferroma-*` headers, token refresh on `401`, WebSocket connect and reconnect, retry policy | know what a message is |
| `sync/` | the sync engine: read `sync_state.cursor`, page `GET /client/sync`, apply changes in a transaction, advance the cursor; detect gaps and `uid_validity` changes | parse protocol payloads into anything other than the local model |
| `mail/` | the mail model and the operations on cached mail: list queries, flag changes, moves, threading | implement sync or HTTP |
| `draft/` | draft lifecycle: create, edit, autosave, delete; mirror to the server and to the Drafts folder | send |
| `outbox/` | the Outbox state machine (§8): enqueue, attempt, back off, retry, fail, and reconcile with `mail_queue` state from the server | upload attachments — that is `attachment/` |
| `attachment/` | content-addressed local cache, streaming download with progress, chunked resumable upload (§7) | decide when a file is needed — that is `mail/` or the UI |
| `search/` | the local FTS index and query parsing; the server fallback (§9) | be the only search path — the fallback exists |
| `notification/` | desktop toasts and badge counts; the APNs/FCM reservation (§11) | render a message |
| `database/` | the SQLite connection pool, migrations, and the typed accessors | contain queries that belong to a feature module |
| `device/` | the device list, revocation, and "sign out this device" (§12) | — |
| `settings/` | the settings surface of §13, reading and writing `settings` | — |
| `ui/` | the platform shell (§14): windows, panes, composer, dialogs | open a socket, hold a cursor, or write the cache directly |

Two invariants that follow from the table:

* **`api/` is the only module that knows about HTTP.** Everything else calls
  typed functions on it. That is what makes the retry policy, the token refresh
  and the version headers exist in exactly one place.
* **`sync/` is the only module that advances a cursor.** If any other module can
  write `sync_state.cursor`, a page can be skipped and the loss is silent.

---

## 5. The sync engine

The mechanics and the reasoning are in [sync.md](sync.md); this is the client-side
shape.

```text
   ┌────────────────────── sync engine (one task per account) ─────────────────┐
   │                                                                           │
   │   loop:                                                                   │
   │     for each folder of the account:                                       │
   │        cursor = SELECT cursor FROM sync_state WHERE …                     │
   │        page   = api.get_sync(mailbox_id, folder_id, cursor, limit)        │
   │        BEGIN;                                                             │
   │          for change in page.changes: apply(change)     # idempotent       │
   │          UPDATE sync_state SET cursor = page.next_cursor                  │
   │        COMMIT;                                                            │
   │     until not page.has_more                                               │
   │                                                                           │
   │   then: drain the outbox (§8)                                             │
   │   then: wait for a WebSocket frame, a timer, or a UI request to sync      │
   └───────────────────────────────────────────────────────────────────────────┘
```

Client obligations, restated from [sync.md](sync.md) §4 because they are easy to
get wrong:

| Obligation | Failure if ignored |
|---|---|
| Apply every change in order, then store the cursor, in one transaction | a crash re-fetches the page; harmless *because* handlers are idempotent |
| Every handler is idempotent (`INSERT … ON CONFLICT`, absolute flag values, not toggles) | a replayed page produces wrong state |
| Page until `has_more` is false | a large account syncs partially and looks truncated |
| Detect a `seq` gap and resync from `0` | silent permanent data loss |
| Detect a `uid_validity` change and drop the folder's UID index | the wrong message opens after a server-side rebuild |
| Never advance the cursor past a change that failed to apply | that change is never delivered again |
| On `409 conflict` from `/sync`, discard the folder cache and sync from `0` | the folder never recovers |

**Message bodies are not in the change stream.** `message_created` carries ids and
flags ([fcp.md](fcp.md) §3, item 4); the body is fetched on demand with
`GET /api/v1/client/messages/:id` or `…/raw`. That is what keeps the first sync of
a 50 000-message mailbox a metadata operation rather than a download of gigabytes.

**First sync shows progress** by comparing the changes applied against the
folder's `message_count` from `GET /api/v1/client/mailboxes` — the only way for
the UI to render a meaningful progress bar, since the server does not report the
total number of changes up front.

### 5.1 Pending operations

Offline actions do not queue in the sync engine; they go to `outbox` and the sync
engine drains it after each successful sync pass ([sync.md](sync.md) §10). The
separation matters:

* The **sync engine** consumes server truth.
* The **Outbox** holds client intent.

A single queue that mixed both would make it ambiguous whether an entry is "the
server told me this" or "I want the server to do this", and the two have opposite
retry policies.

---

## 6. Offline mode (§27)

Specification §27 lists what works offline:

```text
查看已同步邮件       view synced mail
本地搜索             local search
查看已缓存附件       view cached attachments
写邮件               compose
保存草稿             save a draft
回复                 reply
删除                 delete
标记已读             mark read
```

| Capability | Offline behaviour | On reconnect |
|---|---|---|
| View synced mail | served entirely from `messages` + `message_headers` + `body_cached` | nothing to do |
| View an uncached message | "this message is not available offline" — the client must say so rather than show an empty reader | fetched on demand |
| Local search | `search_index`, no server round trip | unchanged |
| View a cached attachment | from `attachment.cached_path` | unchanged |
| View an uncached attachment | not offered; the download is queued | downloaded |
| Compose / reply | fully local; attachments reference local files | sent by the Outbox |
| Save a draft | written to `drafts` with `dirty = 1` | pushed, and the conflict policy of §7 applies |
| Delete / mark read / flag / move | applied optimistically to the cache, and one `outbox` row per action with its `operation_id` | applied on the server; the resulting change arrives and confirms it |
| Send | the message is a `send` row in `outbox`; it is visible in the Outbox view, not in Sent | uploaded, accepted, queued |

Rules that are not negotiable:

* **An optimistic local change is marked as unconfirmed.** The UI must be able to
  show "this will be sent when you are back online" — a user who believes a delete
  happened and later finds the message is a bug report, not a UX nit.
* **Nothing the user typed is ever lost.** Drafts, Outbox rows and attachment
  references live in SQLite until the server acknowledges them
  ([fcp.md](fcp.md) §11).
* **Cached bodies survive a failed send.** A send that fails on `413` keeps the
  draft and the attachment references, so the user can shrink the attachment and
  retry.
* **The cache is not the truth.** A message deleted on the server while this
  machine was offline is deleted locally as soon as the tombstone is applied,
  even if the body was cached. A cached body is never a reason to keep a message
  the server says is gone.

---

## 7. Conflicts

Full treatment in [sync.md](sync.md) §9. The client-side view:

| Entity | Policy | What the client does |
|---|---|---|
| Flags | last write wins | applies the incoming `message_updated` and drops its own pending flag change if it was already acknowledged |
| Folder membership | last write wins | applies `message_moved` in either direction |
| Message existence | server wins absolutely | a tombstone deletes, even a cached body |
| Folder list | server wins | `folder_created` / `folder_deleted` applied to the cache |
| **Draft content** | last write wins, **and the client is told** | on `conflict.detected`, keep the local version as a copy or prompt; never silently retry |
| Local settings | never synchronised | window size, theme, cache budget stay local |
| Server settings | server wins | resynced from `GET /client/account` |

The draft case is the only one where a user can lose typed text, which is why
[fcp.md](fcp.md) §7 specifies that the server reports what it overwrote:

```json
{ "id": 44, "updated_at": "2026-09-16T12:00:01Z",
  "conflict": { "detected": true, "server_updated_at": "2026-09-16T11:59:58Z" } }
```

A client that ignores that field is silently discarding a user's paragraph. The
minimum acceptable behaviour is to keep the losing version as a separate local
draft and tell the user it happened; the better behaviour is to show both and let
them pick.

---

## 8. The Outbox (§28)

Specification §28 gives the flow and the eight states.

```text
Compose
   ↓
Local Outbox
   ↓
Uploading
   ↓
Server
   ↓
Mail Queue
   ↓
SMTP Delivery
```

### 8.1 The eight states

```text
Draft   Pending   Uploading   Queued   Sending   Sent   Failed   Retrying
```

| State | Set by | Meaning | Transitions to |
|---|---|---|---|
| `Draft` | the composer | composed, not yet queued for sending | `Pending` (user hits send), or the draft is deleted |
| `Pending` | the user | queued locally, waiting for the sync engine | `Uploading`, `Failed` |
| `Uploading` | the Outbox | attachments are being transferred (chunked, resumable) | `Queued`, `Retrying`, `Failed` |
| `Queued` | the server's `202`/`200` from `POST /client/messages` | accepted: one `mail_queue` row per recipient | `Sending`, `Retrying`, `Failed` |
| `Sending` | server state | the server is delivering to the remote MX (`mail_queue.status = 'delivering'`) | `Sent`, `Retrying`, `Failed` |
| `Sent` | server state | every recipient delivered (`delivered`) | terminal |
| `Retrying` | server state | a temporary failure; the server will try again at `next_attempt_at` | `Sending`, `Failed` |
| `Failed` | server state, or local validation | permanent failure or attempts exhausted (`failed`), or the request was rejected (`413`, `403`) | terminal (the user may edit and re-send, which creates a new Outbox row) |

The client drives the first four; the server drives the last four. The client
reaches `Sent`, `Retrying` and `Failed` by applying `delivery.updated` changes from
sync — **and the corresponding WebSocket frames when the socket is up**. Because
the socket is an optimisation and the cursor is the source of truth
([fcp.md](fcp.md) §8), a client that only listened to the socket would strand a
message in `Sending` forever. Both paths must feed the same state machine.

### 8.2 The transition rules

```text
  Draft ──send──► Pending ──pick──► Uploading ──accepted──► Queued
                     │                  │                     │
                     │ reject           │ reject              │ server
                     ▼                  ▼                     ▼
                  Failed            Retrying ◄──────────── Sending
                                       │                     │
                                       └──attempt──────────► │
                                                             ▼
                                                           Sent
                                       │
                                       └──exhausted──► Failed
```

| Rule | Reason |
|---|---|
| `operation_id` is generated when the row is created, never at send time | a retry must present the same id, or it protects nothing ([sync.md](sync.md) §8.1) |
| Attachments upload **before** the send | a `send` that references an attachment id the server does not have fails |
| Uploaded attachment ids are stored in the Outbox row | a restart resumes rather than re-uploading ([fcp.md](fcp.md) §6) |
| A `4xx` other than `429` is terminal for that row | `413 limit_exceeded` will not stop being true |
| `429` honours `Retry-After` exactly | the server knows its own limits |
| `5xx` and network errors back off exponentially with jitter | and never drop the row |
| A timeout is never an acknowledgement | the request may have succeeded; the retry carries the same `operation_id` and gets the cached response |
| The row is removed only on a confirmed `Sent`, or on the user deleting it | "probably sent" must not become "gone" |

### 8.3 The Outbox view

Specification §28 shows what the user sees:

```text
发件箱

正在发送    2
发送失败    1
已发送    152
```

Which maps to `Sending`/`Uploading`, `Failed`, and `Sent`. `Failed` rows must be
actionable — show the reason (`last_error`, and the server's enhanced status code
when there is one), offer "edit and resend", and never silently expire. A message
the user believes was sent and which silently vanished is the worst failure this
client can have.

---

## 9. Attachments (§29)

Specification §29 lists the capabilities:

```text
流式上传       streaming upload
分块上传       chunked upload
断点续传       resumable transfer
流式下载       streaming download
下载进度       download progress
本地缓存       local cache
缓存清理       cache eviction
文件大小限制   size limit
MIME 类型      MIME type
文件校验       integrity check
```

### 9.1 Upload

Small files use the simple endpoint; large files and resumable transfers use the
chunked one. Both are specified in [fcp.md](fcp.md) §6.

| Path | When | Endpoint |
|---|---|---|
| Simple | file ≤ `client.attachment_chunk_size` (1 MiB by default, as reported in `GET /client/account`'s `limits`) | `POST /api/v1/client/attachments` (multipart) |
| Chunked | larger, or to resume an interrupted upload | `POST …/attachments/init` → `PUT …/attachments/:id/chunk?index=N` → `POST …/attachments/:id/complete` |

Client obligations for the chunked path:

* **Chunk size comes from the server**, in the `init` response (`chunk_size`), not
  from a constant in the client. The server may change `client.attachment_chunk_size`.
* **Every chunk except the last is exactly `chunk_size`.** The server enforces it.
* **Chunks may be retried and may arrive out of order.** The server keeps a bitmap.
* **Resume by asking.** `GET …/attachments/:id/status` reports which chunks the
  server holds, so a client that crashed uploads only the gap. Guessing costs a
  full re-upload and, worse, can produce a hole the client believes it filled.
* **`complete` must send the SHA-256.** A mismatch is `409 conflict` and the
  upload is discarded — the client should re-upload rather than retry `complete`
  with the same digest.

Attachments are content-addressed server-side, so two messages sharing the same
file share a blob ([storage.md](storage.md) §6). The client can use the same idea
locally: cache by `sha256`, so the same PDF attached to three drafts is stored
once on disk.

### 9.2 Download and cache

| Aspect | Behaviour |
|---|---|
| Streaming | `GET /api/v1/client/attachments/:id` streams; `Range` is supported, so an interrupted download resumes |
| Validation | the `ETag` is the blob's SHA-256 ([fcp.md](fcp.md) §6); the client verifies the content and re-downloads on a mismatch |
| Progress | from `Content-Length` and the bytes received; a `Range` resume starts from the known offset |
| Cache key | `(account_id, sha256)`, so identical attachments deduplicate locally |
| `cached_path` | a file under the client's data directory, never the user's Downloads folder unless they exported it |
| Eviction | LRU by `cached_at`, bounded by the maximum-cache-size setting (§13). Never evict a `dirty` draft's attachment, or an Outbox row's |
| Offline | a cached attachment opens; an uncached one is offered as "download when online" |
| Inline images | subject to the same cache, and subject to the "block remote content" rule in [security.md](security.md) §10.2 — an inline part with a remote URL is not fetched |

The size limit is `limits.max_attachment_size` (25 MiB) and
`limits.max_attachments` (50) per message, both reported by the server in the
`GET /client/account` limits block. The composer must refuse locally, before
uploading, so the user finds out while they are still in the message.

---

## 10. Local search (§30)

Specification §30:

```text
from:alice@example.com
subject:invoice
attachment:pdf
after:2026-01-01
```

Strategy:

```text
先搜索本地缓存      search the local cache first
       ↓
没有结果            no results
       ↓
请求服务器搜索      ask the server
```

### 10.1 The operator set

The client and the server must agree on the query language, or the same query
returns different results depending on whether the cache had an answer. The
server's set is fixed in [fcp.md](fcp.md) §10:
`from:`, `to:`, `subject:`, `body:`, `has:attachment`, `is:unread`, `is:flagged`,
`before:`, `after:`, `folder:`.

| Operator | Local support | Notes |
|---|---|---|
| `from:` | yes | `messages.sender` |
| `to:` | yes | needs recipients in the index; the cache stores them for search |
| `subject:` | yes | FTS column |
| `body:` | yes for cached bodies only | a message whose body is not cached cannot match locally, which is the main reason the fallback exists |
| `has:attachment` | yes | `messages.has_attachments` |
| `is:unread` | yes | `flags` does not contain `seen` |
| `is:flagged` | yes | `flags` contains `flagged` |
| `before:` / `after:` | yes | `internal_date` |
| `folder:` | yes | `folder_id` |

### 10.2 The fallback

```text
   user types a query
        │
        ▼
   parse locally (reject an unknown operator with a clear message)
        │
        ▼
   search_index over the cached rows
        │
        ├── results ──► show them, marked "from this device's cache"
        │
        └── no results ──► GET /api/v1/client/search?q=…&mailbox_id=…&limit=50
                                │
                                ├── results ──► show them, marked "from the server"
                                │                 (and offer to cache the bodies)
                                └── offline ──► say so explicitly
```

Two rules that make the fallback honest:

* **Never present a local result set as complete.** A local search that matches
  nothing may be matching nothing because the bodies are not cached. The UI must
  distinguish "no results" from "no results in the local cache" — otherwise the
  user concludes the message does not exist.
* **Never silently fall back when offline.** "You are offline; showing results
  from this device only" is the correct message.

Why local-first at all, given the server can search? Because local search works
offline, and because it is instant on a mailbox the user has already synced. The
server search exists for the case local search cannot serve: bodies that were
never downloaded, and messages outside the synced window.

The client's index is contentless FTS5, so the client must also maintain the
source text (subject, sender, recipients, cached body) alongside it. Adding a
body to the index when it is fetched — and removing it when the body cache is
evicted — is what keeps the index consistent with what the user can actually read
offline.

---

## 11. Multi-account (§31)

Specification §31:

```text
Accounts
├── Personal   → alice@example.com
├── Work       → alice@company.com
└── Other      → test@example.org
```

| Capability | Implementation |
|---|---|
| Add an account | `account/`, driven by autodiscovery (§12); one `accounts` row |
| Remove an account | delete the row; cascade deletes its mail, drafts, Outbox rows and cached attachments. Confirm first — it is irreversible and the cache may hold the only copy of a `failed` send |
| Pause sync | `accounts.paused = 1`; the sync task stops paging. **The Outbox keeps draining**, because a user pausing sync does not mean "do not send the mail I already told you to send" |
| Re-authenticate | a `401` that survives a refresh; prompt for the password, and keep the cache and the Outbox |
| Edit an account | display name, server URL, device name |
| View sync status | `sync_state.last_full_sync_at` per folder, plus the Outbox counts |
| Per-account isolation | every table carries `account_id`, and every query filters on it. One account must never see another's mail |

Two design rules the multi-account model forces:

* **One sync task per account**, not one global task. Accounts can be on different
  servers with different availability, and a slow or unreachable one must not
  stall the others.
* **The UI is account-scoped and offers a unified view.** A unified inbox is a
  query across accounts (`messages WHERE account_id IN (…) ORDER BY internal_date
  DESC`), not a merged table. Merging would make every per-account operation —
  delete, move, flag — ambiguous.

---

## 12. Autodiscovery (§32)

Specification §32: `https://example.com/.well-known/ferroma`. The response shape
is frozen in [api.md](api.md) §2:

```json
{
  "api": "https://mail.example.com/api/v1",
  "imap": { "host": "mail.example.com", "port": 993, "tls": true },
  "smtp": { "host": "mail.example.com", "port": 587, "tls": true },
  "web": "https://mail.example.com",
  "protocol_version": 1
}
```

The client's flow when the user types an address:

```text
   user types alice@example.com
        │
        ▼
   1. GET https://example.com/.well-known/ferroma      (the domain from the address
        │                                                only; port 443, HTTPS only)
        ├── 200 + JSON ──► use `api` as the FCP base, record `protocol_version`
        │
        ├── 404 ──► Discovery::guessed — the conventional hostnames:
        │             api  = https://mail.example.com/api/v1
        │             imap = imap.example.com:993 (tls)
        │             smtp = mail.example.com:587 (tls)
        │           marked as a *guess*, for the UI to confirm — never treated as a
        │           discovery result.
        │
        └── anything else (5xx, transport/TLS failure, a body that is not JSON, a
            document without a usable `api`) ──► an error, and never a guess:
            guessing there points the account at somebody else's server. The manual
            pane takes over (`Discovery::candidate_hosts` offers mail.<domain>,
            imap.<domain>, <domain> in that order).
        ▼
   2. manual configuration: ask for the server URL and validate it with
      GET <base>/health or GET <base>/client/account
      (CLI: `account add … --server <URL>`, which skips discovery entirely).
```

Only step 1 makes a request, and it asks the **bare domain** exactly once: the core
never then retries `mail.<domain>` by itself. To run the API on a port other than 443
(the containerised-proxy case at the end of deployment.md §5.4), have whatever holds
443 for the bare domain serve this document — its `api` field is what points clients
at the real port.

Rules:

| Rule | Reason |
|---|---|
| Fetch from the **domain of the address**, not from the region | that is where the record lives and it is the only part the user typed |
| HTTPS only; a plain-HTTP discovery response is ignored | an attacker on the network would otherwise redirect every account to their own server |
| Verify the certificate normally | same reason |
| Unknown fields are ignored, missing fields fall back | the response is versioned and will grow |
| The official client uses only `api` | the `imap`/`smtp` entries exist so the same record serves third-party clients and future import flows |
| The client records `protocol_version` from `GET /client/account` | it is the server that decides the negotiated version ([fcp.md](fcp.md) §1) |
| A mismatch between the address domain and the discovered server is shown, not hidden | "you typed example.com but this server is mail.other.example" is worth a confirmation |

Specification §32 also reserves **autoconfig** and **autodiscover** (the
Thunderbird and Microsoft conventions) for future compatibility _(planned)_;
neither is served today.

---

## 13. Devices (§33)

Specification §33 lists the API and the fields. The endpoints are frozen in
[fcp.md](fcp.md) §9.

| Action | Endpoint | Effect |
|---|---|---|
| List | `GET /api/v1/client/devices` | every installation of this account, with `last_seen_at`, `last_ip`, `platform`, `client_version`, `protocol_version`, `revoked` |
| Revoke | `POST /api/v1/client/devices/:id/revoke` | marks revoked, revokes its sessions, publishes `device.revoked` |
| Delete | `DELETE /api/v1/client/devices/:id` | removes the record |

Client behaviour:

* **Register on login.** Send `device` in the FCP login body
  (`{ device_uid, name, platform, client_version }`), with `device_uid` stable per
  installation and generated once ([fcp.md](fcp.md) §2).
* **Update on start.** A refresh on every launch keeps `last_seen_at` and
  `client_version` current, which is what makes the device list useful for
  spotting an installation the user does not recognise.
* **React to `device.revoked`.** The WebSocket frame for *this* device means: clear
  the tokens, stop syncing, keep the cache, and show "this device was signed out".
  The next request gets `401` ([fcp.md](fcp.md) §9).
* **Revoking your own device is allowed** and takes effect immediately. A user on
  a laptop that is about to be sold must be able to cut it off from a phone.
* **Signing out is not the same as revoking.** Logout discards the local tokens;
  revocation invalidates them server-side. The settings screen must offer both and
  say which is which.

---

## 14. Notifications (§34)

Specification §34:

```text
Mail Received
      ↓
 Event Bus
      ↓
Notification Service
      ↓
┌─────┴─────┐
▼           ▼
APNs       FCM
```

### 14.1 Desktop

| Platform | Mechanism |
|---|---|
| Windows | toast notifications via the WinRT notification API |
| Linux | the freedesktop notification spec (libnotify / DBus) |
| macOS | `UNUserNotificationCenter` |

Triggered by `Event::mail_received` (`mail.received`), which carries exactly what a
banner needs and nothing more:

```json
{ "seq": 1842, "type": "mail.received", "mailbox_id": 3, "message_id": 4822,
  "from": "bob@example.net", "subject": "Re: Invoice", "snippet": "Thanks, got it." }
```

**The event carries a snippet, never a body** ([architecture.md](architecture.md)
§6), so a notification cannot leak message content into the OS notification
store — which on Windows and macOS is persisted and searchable.

Rules:

* **Notify once per message.** The same event can arrive twice (a socket frame and
  a sync change); the client deduplicates by `message_id`.
* **Suppress when the user is looking at the mailbox.** A notification for the
  folder that is open and focused is noise.
* **Respect a per-account and per-folder toggle**, and a do-not-disturb window.
* **Badge counts come from sync, not from the socket.** The socket can miss
  events; `folders.unseen_count` from `GET /client/mailboxes` is the number to
  show.
* **Never include the body in the snippet.** The server truncates
  `MailReceived.snippet` for this reason; the client must not fetch the body to
  make a richer banner.

### 14.2 Mobile reservation: APNs and FCM

Specification §34 puts APNs and FCM under "移动端预留" — reserved for mobile. The
design reservations, so that adding mobile later is not a schema change:

| Reservation | Where |
|---|---|
| `client.push_enabled` | config key, default `false` — the server advertises push capability without enabling it |
| A device token column | `devices` gains `push_token TEXT` and `push_platform TEXT` when mobile ships; the table already exists and is keyed by `(user_id, device_uid)` |
| The push payload | the same fields as `MailReceived`: ids, `from`, `subject`, `snippet`, counts. No body |
| The delivery path | `Event::MailReceived` → notification service → APNs/FCM. The event is already the integration point, so the notification service is the only new component |
| The privacy rule | a lock-screen notification shows sender and subject. The body requires unlocking the app and fetching it over FCP |

None of this is implemented. The point of writing it down is that
`Event::MailReceived` already carries the right fields and `devices` already has a
stable identity, so no protocol change is needed to add push — only a server-side
sender and a mobile client.

---

## 15. Settings (§52)

Specification §52 lists the sections and, for sync, the options.

| Section | Content |
|---|---|
| **Accounts** | add / remove / pause / re-authenticate; per-account server URL and device name |
| **Sync** | what to sync and how much (below) |
| **Notifications** | per-account and per-folder toggles; do-not-disturb; sound |
| **Appearance** | theme (light / dark / system), density, font size |
| **Reading** | HTML or plain text by default; block remote content; mark-as-read delay; conversation view |
| **Composing** | signature per account; reply quoting style; send delay / undo window |
| **Attachments** | auto-download policy; download directory; maximum attachment size to auto-download |
| **Search** | whether to index bodies; whether to fall back to the server |
| **Storage** | the cache budget and where the cache lives (below) |
| **Security** | token storage status; "sign out all devices"; auto-lock |
| **Devices** | the device list of §13 with revoke |
| **About** | version, `protocol_version`, server version, build, log location |

Sync settings, from specification §52:

```text
同步全部邮件             sync all mail
仅同步最近 30 天         sync the last 30 days only
仅同步最近 90 天         sync the last 90 days only
附件自动下载             auto-download attachments
仅 Wi-Fi 下载            download over Wi-Fi only
最大缓存大小             maximum cache size
```

How each maps onto the design:

| Setting | Implementation |
|---|---|
| Sync window (all / 30 / 90 days) | a predicate applied when paging `GET /client/sync`: changes whose `internal_date` is outside the window are recorded as seen (so the cursor advances) but not stored as messages. The bodies for skipped messages are never fetched |
| Auto-download attachments | overrides the default of "download on open" |
| Wi-Fi only | desktop: whether the connection is metered; the platform API reports it |
| Maximum cache size | the eviction budget for `attachments.cached_path` and cached bodies (§9.2), LRU by `cached_at` |

Settings storage: `settings` with `account_id = 0` for application-wide values and
a real id for per-account ones. Values that exist on the server too
(`client.tombstone_retention_days` affects the client's behaviour but is the
server's policy) are read from the server and shown read-only; a client that
invented its own retention would silently diverge.

---

## 16. UI plan (§51)

Specification §51:

```text
┌─────────────────────────────────────────────────────┐
│ Ferroma                         🔍   ⚙   👤         │
├───────────────┬─────────────────────┬───────────────┤
│               │                     │               │
│ 收件箱        │ 邮件列表            │ 邮件阅读      │
│ 已发送        │                     │               │
│ 草稿          │ ┌─────────────────┐ │               │
│ 垃圾箱        │ │ Alice           │ │               │
│ 回收站        │ │ Invoice         │ │               │
│               │ ├─────────────────┤ │               │
│ 文件夹        │ │ Bob             │ │               │
│               │ │ Meeting         │ │               │
│               │ └─────────────────┘ │               │
│               │                     │               │
└───────────────┴─────────────────────┴───────────────┘
```

A three-pane shell: an account and folder rail, a message list, a reader, with a
toolbar (search, settings, account) across the top.

| Pane | Contents | Data source |
|---|---|---|
| Rail | accounts; per account its folders, `INBOX` first, then by name; `special_use` markers drive the icons so a folder named `Sent Items` still reads as Sent | `mailboxes` table ([imap.md](imap.md) §4.3) |
| List | one row per message: sender, subject, snippet, date, attachment clip, flag; virtualised, because a folder can hold 50 000 rows | `messages`, paged with `internal_date DESC` |
| Reader | headers, text or sanitised HTML body, attachments | `message_headers` + the fetched body; HTML goes through the sanitiser ([security.md](security.md) §10.2) |
| Composer | a separate window or a full-width overlay; recipient fields with local recipient autocompletion, subject, body, attachment chips, send/save-draft | `drafts`, `attachments` |
| Outbox | the §8 state machine, with the counts from specification §28 | `outbox` |
| Search | an overlay over the list, with the operator syntax of §10 | `search_index`, then the server |
| Settings | the twelve sections of §15 | `settings` |

UI rules that follow from the core's design:

* **The UI never blocks on the network.** Every pane reads from SQLite. A sync in
  progress is a status line, not a spinner over the list.
* **Offline is a visible state, not an error dialog.** A status indicator, plus
  per-item markers for operations that have not been confirmed.
* **The Outbox is always reachable.** A user must be able to see that a message is
  stuck, and why.
* **Unread counts come from the folder rows**, refreshed by sync — never computed
  by counting the loaded list, which is paged and therefore wrong.
* **A message whose body is not cached says so.** Never an empty reader.

Platform targets and phases, from specification §24.1 and §50:

| Phase | Platforms |
|---|---|
| First (v0.7–v0.9) | Windows, Linux, macOS |
| Second | Android, iOS |

Specification §50's client milestones: v0.7 the MVP (login, accounts, inbox,
reading, composing, reply, forward, delete, read/unread, attachments, basic sync),
v0.8 offline, local cache, incremental sync, local search, WebSocket, v0.9
multi-account, device management, push notification, draft sync, Outbox.

---

## 17. Dependencies the design already implies

`client/Cargo.toml` already declares them, which is the strongest available
evidence of the intended shape:

| Crate | Used for |
|---|---|
| `ferroma-core` | `FerromaError`, typed ids, `Cursor`, address parsing — the same types the server uses |
| `tokio` | the async runtime; one sync task per account |
| `sqlx` (features `sqlite`, `runtime-tokio-rustls`, `migrate`, `chrono`) | the local cache, with real migrations |
| `reqwest` (`rustls-tls`, `json`, `stream`, `multipart`) | the FCP HTTP client, including streaming downloads and multipart uploads |
| `tokio-tungstenite` | the real-time WebSocket (`GET /api/v1/client/events`) |
| `serde` / `serde_json` | the wire types, matching [fcp.md](fcp.md) |
| `chrono` | timestamps; everything is UTC |
| `clap` | the client binary's own CLI (account management, a headless sync mode for testing) |
| `dirs` | the per-platform data directory for the cache and the attachment store |
| `sha2` | verifying attachment digests and deduplicating the local cache |
| `uuid` | `device_uid` and `operation_id` generation |
| `tracing` / `tracing-subscriber` | the same structured logging as the server, so a bug report can include a client log |
| `base64`, `hmac` | token handling if a platform needs to build a request signature |

Note what is **not** there: no `ferroma-storage`, no `ferroma-imap`, no
`ferroma-smtp`, no PostgreSQL feature in `sqlx`. The client links the shared core
crate and nothing else from the server side ([architecture.md](architecture.md)
§2).

---

## 18. Related documents

| Topic | Document |
|---|---|
| FCP wire format: endpoints, cursor, real-time framing, chunked upload | [fcp.md](fcp.md) |
| Management API, autodiscovery, health, error envelope | [api.md](api.md) |
| Sync model: change log, apply-then-advance, tombstones, failure matrix | [sync.md](sync.md) |
| IMAP behaviour for third-party clients on the same accounts | [imap.md](imap.md) |
| Attachment content addressing, quota, cache eviction on the server | [storage.md](storage.md) §6 |
| Token theft detection, HTML sanitiser, known gaps | [security.md](security.md) |
| Deploying the server the client talks to | [deployment.md](deployment.md) |
| Crate graph and layering rule | [architecture.md](architecture.md) |
