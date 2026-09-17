# Ferroma architecture

**Who should read this:** anyone about to add a crate, a protocol surface or a
background task to Ferroma. It is the map that the other documents in this
directory take for granted.

Ferroma is a single Rust process that speaks SMTP, IMAP and HTTPS, stores
message metadata in PostgreSQL and message bytes in a Maildir on the same host.
Everything that is not a protocol parser goes through one mail core and one set
of repositories. This document describes the products, the crate graph, the
layering rule and what happens to a message from the moment a remote MX opens a
TCP connection to the moment a client's socket receives a `mail.received` frame.

> **Status:** architectural description of the repository as it stands. The
> `ferroma-core`, `ferroma-mail`, `ferroma-storage`, `ferroma-auth` and
> `ferroma-events` crates are implemented. `ferroma-smtp`, `ferroma-imap`,
> `ferroma-sync`, `ferroma-api`, `server` and `client` are crate skeletons with a
> documented interface and no implementation yet; every statement about their
> internals below is marked _(planned)_ and is a design specification, not an
> observation. The HTTP and FCP wire contracts are frozen separately in
> [api.md](api.md) and [fcp.md](fcp.md) — this document never restates them.

---

## 1. The four products

| Product | Where it lives | What it is | Status |
|---|---|---|---|
| **Ferroma Server** | `server/` (binary `ferroma`), `crates/*` | The daemon: SMTP, IMAP, HTTP API, queue workers, sync service, event bus | binary is a stub; libraries partially implemented |
| **Ferroma Webmail** | `web/` | Browser mail client, a static SPA served by the API | SPA sources present; not served yet _(planned)_ |
| **Ferroma Admin** | `admin/` | Domain/user/queue/DNS/storage administration SPA | SPA sources present; not served yet _(planned)_ |
| **Ferroma Client** | `client/` (binary `ferroma-client`) | Official desktop client (Windows, Linux, macOS), shared core + UI shell | skeleton |

Webmail and Admin are not separate processes. They are static assets served by
`ferroma-api` under the same origin as `/api/v1`, gated by `api.serve_frontend`
and by `is_admin` for the Admin routes. Both consume the **Management API**; the
official client consumes the **Client API (FCP)** and nothing else.

Webmail and the official client deliberately share four subsystems — mail core,
API, event bus and authentication — so there is exactly one implementation of
"mark this message read" on the server. Specification §35.

---

## 2. Crate graph

Members of the workspace, from `Cargo.toml`:

```text
                                 ┌───────────────┐
                                 │ ferroma-core  │  config, error, ids,
                                 │               │  address, limits, logging
                                 └───────┬───────┘
                    ┌────────────────────┼────────────────────┬──────────────┐
                    │                    │                    │              │
                    ▼                    ▼                    ▼              ▼
            ┌───────────────┐   ┌────────────────┐   ┌──────────────┐  ┌───────────┐
            │ ferroma-mail  │   │ferroma-storage │   │ferroma-auth  │  │ferroma-   │
            │ RFC 5322+MIME │   │ PG + Maildir + │   │ Argon2id,    │  │events     │
            │               │   │ blob store     │   │ tokens,      │  │ bus +     │
            └───────┬───────┘   └───────┬────────┘   │ devices      │  │ envelopes │
                    │                   │            └──────┬───────┘  └─────┬─────┘
                    │                   │                   │                │
      ┌─────────────┴──────────┬────────┴───────────┬───────┘                │
      │                        │                    │                        │
      ▼                        ▼                    ▼                        │
┌───────────┐           ┌─────────────┐      ┌──────────────┐                │
│ferroma-   │           │ferroma-imap │      │ferroma-sync  │◄───────────────┘
│smtp       │           │             │      │ changelog,   │
│server+    │           │ IMAP4rev1   │      │ cursors,     │
│client+mx+ │           │ server      │      │ operations   │
│dkim/spf/  │           │             │      │              │
│dmarc      │           │             │      │              │
└─────┬─────┘           └──────┬──────┘      └──────┬───────┘
      │                        │                    │
      └────────────┬───────────┴────────────────────┘
                   ▼
            ┌─────────────┐
            │ ferroma-api │  REST + FCP + WebSocket + frontends
            └──────┬──────┘
                   │
        ┌──────────┴──────────┐
        ▼                     ▼
  ┌───────────┐        ┌────────────┐
  │  server/  │        │  client/   │
  │  ferroma  │        │ ferroma-   │
  │  binary   │        │ client     │
  └───────────┘        └────────────┘
```

Read the edges off the manifests, not off the diagram:

| Crate | Depends on |
|---|---|
| `ferroma-core` | nothing internal |
| `ferroma-mail` | `ferroma-core` |
| `ferroma-storage` | `ferroma-core` |
| `ferroma-auth` | `ferroma-core`, `ferroma-storage` |
| `ferroma-events` | `ferroma-core` |
| `ferroma-smtp` | core, mail, storage, auth, events |
| `ferroma-imap` | core, mail, storage, auth, events |
| `ferroma-sync` | core, mail, storage, events |
| `ferroma-api` | core, mail, storage, auth, events, sync, smtp |
| `server` | every crate above |
| `client` | `ferroma-core` only |

Two consequences worth knowing before you touch a manifest:

1. **`ferroma-core` never depends on a database driver.** `ferroma-storage`
   translates `sqlx::Error` into its own `StorageError` and only then into
   `FerromaError` (`crates/ferroma-storage/src/error.rs`), keeping `sqlx` out of
   the public API of everything above it.
2. **`client` shares `ferroma-core` and nothing else.** Cargo would happily let
   the client link `ferroma-storage`, but the desktop client must not drag a
   PostgreSQL pool or a Maildir into a shipped binary; its own SQLite cache lives
   in `client/src/database/` _(planned)_.

---

## 3. The layering rule

Non-negotiable, from [../AGENTS.md](../AGENTS.md) §4.5 and specification §8:

```text
    Protocol layer            ferroma-smtp, ferroma-imap, ferroma-api, client
    (parse, authenticate, marshal, reply)
              │
              ▼
    Mail core + repositories  ferroma-mail, ferroma-storage,
                              ferroma-auth, ferroma-sync
              │
              ▼
    Storage / Queue /         PostgreSQL, Maildir, attachment blob store
    Delivery
```

**Protocol layers contain no business logic.** An SMTP session parser does not
decide whether an address exists; it hands the envelope to the mail core and
turns the resulting `FerromaError` into a reply code. An IMAP `STORE` handler
does not edit a flag string; it calls `MessagesRepository::set_flags` and the
maildir's `set_flags`. A Webmail route does not build MIME; it calls
`ferroma-mail::MessageBuilder`.

The reason is not purity. It is that SMTP, IMAP, Webmail and the client API each
have a different idea of what "read", "delete" and "size" mean, and the only way
to keep them consistent — and to keep the sync change log honest — is to have one
implementation that all four call.

Where the boundary is enforced today:

| Rule | Enforced by |
|---|---|
| Errors are `ferroma_core::FerromaError` everywhere | `FerromaError` in `crates/ferroma-core/src/error.rs`; `From<StorageError> for FerromaError` |
| No `sqlx::query!` macros (compile-time DB) | convention in [../AGENTS.md](../AGENTS.md) §4.3; `sqlx::query_as` + `#[derive(FromRow)]` in `ferroma-storage::models` |
| No `unwrap()` on peer input | convention in [../AGENTS.md](../AGENTS.md) §4.4 |
| The server is never an open relay | `SmtpConfig::require_auth_on_submission`, `FerromaError::Forbidden` — [smtp.md](smtp.md) §6 |
| Timestamps are `TIMESTAMPTZ`, UTC, everywhere | `migrations/0001_initial.sql`; `chrono::DateTime<Utc>` in the models |

---

## 4. Request lifecycle — an inbound SMTP message

The path a message takes from a stranger's MX to a stored row and a `new/` file.
Steps marked _(planned)_ describe `ferroma-smtp` and `ferroma-api` as specified;
the storage and event steps are implemented.

```text
 remote MX ──TCP:25──► ferroma-smtp listener
                            │  accept, spawn session task
                            ▼
                       SmtpSession { state: Connected, … }        (spec §9.2, §9.3)
                            │  220 banner (smtp.banner)
                            ▼
                       EHLO ──► 250-… capabilities, 250 SIZE <limits.max_message_size>
                            │  state = Greeted
                            ▼
                       MAIL FROM:<bob@example.net>
                            │  parse reverse-path (ferroma-core::EmailAddress)
                            │  state = MailFrom
                            ▼
                       RCPT TO:<alice@example.com>
                            │  resolve domain → domains.name
                            │  resolve local part → mailboxes(domain_id, local_part)
                            │  unknown domain or address ⇒ 550 5.1.1 / 5.1.2
                            │  recipient count > limits.max_recipients ⇒ 452 4.5.3
                            ▼
                       DATA ──► 354, read dot-terminated body with limits
                            │  abort with 552 5.3.4 when SIZE is exceeded
                            ▼
                 ┌──────────────────────────────────────────┐
                 │  Mail Core                               │
                 │  1. parse: ferroma_mail::ParsedMessage   │
                 │  2. auth verdicts: SPF/DKIM/DMARC        │
                 │  3. prepend Received: (server.hostname)  │
                 │  4. quota: MailboxesRepository::         │
                 │             check_quota(mailbox, size)   │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  Maildir::store(domain, local_part,      │
                 │                 "INBOX", bytes, flags)   │
                 │  tmp/ → fsync → rename into new/         │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  MessagesRepository::insert(NewMessage)  │
                 │  allocates the IMAP UID from             │
                 │  folders.uid_next under the folder's     │
                 │  row lock (one statement, no race)       │
                 │  then MessagesRepository::insert_        │
                 │  recipients() and AttachmentsRepository  │
                 │  ::insert() per part, blobs via          │
                 │  AttachmentStore::store()                │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  ChangeLogRepository::append(NewChange { │
                 │      kind: "message_created", … })       │
                 │  → one change_log row per affected       │
                 │    mailbox, seq is the client cursor     │
                 └──────────────┬───────────────────────────┘
                                ▼
                 ┌──────────────────────────────────────────┐
                 │  EventBus::publish(                      │
                 │      EventScope::User(user_id),          │
                 │      Event::mail_received(…))            │
                 └──────────────┬───────────────────────────┘
                                ▼
                    250 2.0.0 Ok: queued as <id>

  Event subscribers:  WebSocket /api/v1/client/events  → official client
                      Webmail session (SSE or WS)      → unread badge
                      notification service             → desktop toast
                      webhook consumer                 → HTTP POST
```

Ordering rules that matter:

* The Maildir write happens **before** the row is inserted. A crash between the
  two leaves an orphan file (swept later by `Maildir::sweep_tmp`); the reverse
  order would leave a row whose body does not exist, which surfaces to users as
  `StorageError::BodyMissing`.
* `messages.id` and `messages.uid` are allocated by the database. A UID is only
  ever allocated inside the `INSERT` that uses it — see the
  `WITH next_uid AS (UPDATE folders SET uid_next = uid_next + 1 …)` statement in
  `crates/ferroma-storage/src/repository/messages.rs`.
* The change-log row is appended in the same transaction as the message row. A
  committed message with no change-log entry would be invisible to every synced
  client — the exact failure the cursor model exists to prevent.

The reply codes for each failure are tabulated in [smtp.md](smtp.md) §12.

---

## 5. Request lifecycle — an API call

`GET /api/v1/client/messages/4821`, the shape every authenticated request shares.

```text
 HTTP/1.1 or HTTP/2 (axum) behind rustls or a proxy
      │
      ▼
 1. version gate                     X-Ferroma-Protocol < client.min_protocol_version
      │                              ⇒ 426 { "code": "unsupported" }
      ▼
 2. authentication                   Authorization: Bearer <access token>
      │                              AuthService::authenticate(&token)
      │                                TokenService::verify_access → AccessClaims
      │                                sessions row must exist and not be revoked
      │                              failure ⇒ FerromaError::Unauthorized ⇒ 401
      ▼
 3. authorisation                    the message's mailbox_id must belong to
      │                              claims.user_id, else FerromaError::Forbidden
      │                              ⇒ 403
      ▼
 4. idempotency (mutating verbs)     OperationsRepository::begin(operation_id, …)
      │                                Fresh  ⇒ do the work, then complete(result)
      │                                Replay ⇒ return the cached result verbatim
      ▼
 5. handler                          protocol layer only: read path params,
      │                              build a `MessageSearch` / `NewMessage`, call
      ▼                              the repository or the mail core
 6. repository / mail core           sqlx over the PgPool; Maildir or
      │                              AttachmentStore for bytes
      ▼
 7. response mapping                 FerromaError::code() + http_status()
      │                              ⇒ the error envelope in api.md §1.3
      ▼
 8. side effects                     change_log append, change_log-driven sync,
                                     EventBus::publish for the realtime socket
```

`FerromaError::code()` is literally the `code` field of every API error body and
`FerromaError::http_status()` is the status — see `crates/ferroma-core/src/error.rs`.
There is no second mapping table anywhere, and an API route that invents a code
string is a bug.

---

## 6. The event bus

`ferroma-events` is implemented, and it is one process-wide object. Specification
§22 and §23.

```text
  producer (Mail Core, queue worker, auth service)
      │ EventBus::publish(scope, event)          or publish_nowait(…)
      ▼
  ┌─ one mutex ──────────────────────────────────────────┐
  │ last_seq += 1        seq is gap-free and monotonic   │
  │ ring: VecDeque<EventEnvelope>  (history_capacity)    │
  └───────────────────────┬──────────────────────────────┘
                          │ tokio::sync::broadcast::send
                          ▼
        per-subscriber Subscription { EventFilter, … }
              EventFilter::matches(&scope)   ← the authorisation boundary
                          │
        ┌─────────────────┴──────────────────┐
        ▼                                    ▼
  live WebSocket frames              EventBus::replay_since(after)
  EventEnvelope::to_wire()           for a reconnecting client
```

Vocabulary — `Event` in `crates/ferroma-events/src/event.rs`:

| Rust variant | `Event::name()` | Wire `type` | Payload struct |
|---|---|---|---|
| `Event::MailReceived` | `mail.received` | `mail.received` | `MailReceived` |
| `Event::MailSent` | `mail.sent` | `mail.sent` | `MailSent` |
| `Event::MailDeleted` | `mail.deleted` | `mail.deleted` | `MailDeleted` |
| `Event::MailRead` | `mail.read` | `mail.read` | `MailRead` |
| `Event::MailFlagChanged` | `mail.flag_changed` | `mail.flag_changed` | `MailFlagChanged` |
| `Event::MailMoved` | `mail.moved` | `mail.moved` | `MailMoved` |
| `Event::DraftCreated` | `draft.created` | `draft.created` | `DraftCreated` |
| `Event::DraftUpdated` | `draft.updated` | `draft.updated` | `DraftUpdated` |
| `Event::DeliveryUpdated` | `delivery.updated` | `delivery.updated` | `DeliveryUpdated` |
| `Event::DeviceRevoked` | `device.revoked` | `device.revoked` | `DeviceRevoked` |

Note the one naming divergence from specification §23, which lists
`mail.updated`: the implementation splits that into `mail.read` and
`mail.flag_changed`, and adds `mail.moved`, `draft.created` and
`device.revoked`. The frozen wire list is [fcp.md](fcp.md) §8.

Scope is the authorisation boundary: `EventScope::User(UserId)`,
`EventScope::Mailbox(MailboxId)` or `EventScope::System`. A subscriber filtered
to `User(7)` cannot observe another user's frames even though all sessions share
one bus.

Guarantees and their limits:

* `seq` is strictly monotonic and gap-free even under concurrency; the counter and
  the replay ring mutate under one mutex and the broadcast happens while it is
  held, so subscribers also observe events in `seq` order.
* Defaults: `history_capacity = 1024`, `channel_capacity = 512`,
  `drop_on_lag = true`. A subscriber that stops reading gets
  `SubscriptionError::Lagged` rather than stalling `Mail Core`. `drop_on_lag =
  false` is accepted but behaves as `true` — the bus has no publisher
  back-pressure mode.
* **No message bodies cross the bus.** `MailReceived` carries a `snippet`, a
  subject and a size, never the bytes.

**The bus is in-process only.** There is no Redis or NATS backend and no
cross-process fan-out _(planned)_. Two `ferroma` processes sharing one database
have two independent event streams; a client connected to process A will not see
a change made through process B until it runs a sync. This is a real constraint
on horizontal scaling and is listed again in [security.md](security.md) and
[deployment.md](deployment.md).

---

## 7. The sync model

Full treatment in [sync.md](sync.md); this is the shape.

```text
   Server (source of truth)                         Client (cache)
   ─────────────────────────                        ──────────────
   messages, folders, drafts                        SQLite cache
        │                                                │
        │  every mutation appends one change_log row     │
        ▼                                                │
   change_log(seq BIGSERIAL, user_id,                    │
              mailbox_id, folder_id,                     │
              message_id, kind, payload)                 │
        │                                                │
        │  GET /api/v1/client/sync?cursor=N ────────────►│
        │◄──── { next_cursor, has_more, changes[] } ─────│
        │                                                │
        │                                     apply all changes durably
        │                                     then store next_cursor
        │                                                │
        │  POST /… { operation_id: "op_…" } ◄────────────│  Outbox
        │  OperationsRepository::begin()                 │
        │    Fresh  → execute, record result             │
        │    Replay → return the recorded response       │
```

Three properties that hold by construction:

1. **The cursor is `change_log.seq`.** `ChangeLogRepository::changes_since(user_id,
   after, limit)` selects `seq > after` ascending, so passing the last applied
   `seq` never repeats an entry. The client treats it as opaque.
2. **Deletions are append-only tombstones.** `change_log.message_id` and
   `change_log.folder_id` are deliberately *not* foreign keys, so a `message_deleted`
   or `folder_deleted` row outlives the row it describes. See the comments in
   `migrations/0001_initial.sql` and
   `migrations/0003_change_log_folder_tombstones.sql`.
3. **Replay is idempotent.** `OperationsRepository::begin` is a single
   `INSERT … ON CONFLICT DO NOTHING RETURNING *`; of two concurrent retries of the
   same `operation_id` exactly one gets `OperationOutcome::Fresh`.

---

## 8. Why these choices

**Why PostgreSQL for metadata and the filesystem for bytes.** Message bodies are
append-only large blobs; metadata is small, hot and relational. Specification §6.2
says exactly this ("邮件正文和附件不建议全部直接存入数据库"). Keeping bodies out of
PostgreSQL keeps `pg_dump` small enough to run nightly, keeps `VACUUM` cheap, and
lets you back up the two halves with tools that suit each.

**Why Maildir rather than a database blob column or an object store.** A Maildir
is a directory tree any mail administrator can inspect with `ls`, restore with
`tar`, and hand to Dovecot if Ferroma is ever abandoned. It also gives delivery
its atomicity for free: write to `tmp/`, `rename(2)` into `new/` and a reader can
never observe a partial message. Specification §13.

**Why rustls and never native-tls.** Two reasons that happen to agree. On this
development host the Windows TLS stack is broken (`schannel` fails with
`SEC_E_NO_CREDENTIALS`), so `native-tls` would not even build or connect; and for
a server that terminates SMTP, IMAP and HTTPS itself, a memory-safe TLS
implementation with an explicit cipher suite list is the right default. Every
TLS-capable dependency is pinned to rustls in the workspace `Cargo.toml`, and
`AGENTS.md` §1.1 forbids adding a crate that pulls in `native-tls`, `openssl` or
`schannel`.

**Why a bespoke FCP alongside IMAP.** IMAP cannot express a resumable cursor,
server-side drafts, device management, chunked attachment transfer or realtime
push. Third-party clients keep speaking IMAP and SMTP and stay first-class
(specification §53); official clients get a protocol that fits the job. Rationale
and wire format: [fcp.md](fcp.md) §1.

**Why typed ids instead of `i64`.** `UserId`, `MailboxId`, `MessageId` and the
rest are `#[repr(transparent)]` newtypes in `crates/ferroma-core/src/ids.rs`. They
are free at runtime and map to `BIGINT` columns directly, but a function that
wants a `MailboxId` cannot be handed a `MessageId` by accident — a class of bug
that is otherwise invisible in review and catastrophic in production.

**Why one mutex around the event bus.** Publishing must be cheap and must never
block on a slow consumer. A single mutex held briefly while incrementing a counter
and pushing to a bounded ring is fast enough for a single-process server, and it
is the simplest thing that makes `seq` gap-free — which is what lets a client use
`seq` as a reconnect cursor without a separate ordering mechanism.

**Why the client compiles `ferroma-core` only.** The desktop client must not link
a PostgreSQL driver or a Maildir. Sharing `ferroma-core` gives it the same
`FerromaError`, the same typed ids and the same `Cursor` without dragging the
server's storage layer along.

---

## 9. Two deliberate deviations from the specification's §37 schema sketch

`migrations/0001_initial.sql` opens with these two notes. They are the only
intentional divergences from the specification's schema, and both were made
because the §37 sketch is ambiguous in a way that breaks queries.

### 9.1 `mailboxes` is an address; IMAP folders live in `folders`

Specification §37 defines:

```sql
CREATE TABLE mailboxes (
    id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users(id),
    domain_id BIGINT NOT NULL REFERENCES domains(id),
    local_part TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(domain_id, local_part)
);
```

and then gives `messages` a single `mailbox_id`:

```sql
CREATE TABLE messages (
    id BIGSERIAL PRIMARY KEY,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id),
    ...
);
```

The sketch calls the table `mailboxes` but gives it the shape of an *address*:
`(domain_id, local_part)` with `UNIQUE(domain_id, local_part)` is
`alice@example.com`, not "the Inbox folder". With one such column on `messages`
there is no way to express INBOX versus Sent versus Archive — every message of a
user would land in the same bucket, and IMAP `SELECT INBOX` would be
indistinguishable from `SELECT Sent`.

The shipped schema keeps the word *mailbox* for the address and adds a table for
folders:

| Table | Meaning | Key columns |
|---|---|---|
| `mailboxes` | an address owned by a user in a domain — the SMTP `RCPT TO` target | `domain_id`, `local_part`, `user_id`, `is_primary`, `quota_bytes` |
| `folders` | one IMAP folder of one address | `mailbox_id`, `name`, `parent_id`, `special_use`, `uid_validity`, `uid_next`, `highest_modseq`, `message_count`, `unseen_count`, `total_bytes` |
| `messages` | a stored message | `folder_id` (**authoritative parent**), `mailbox_id` (denormalised copy), `uid`, `storage_path` |

`messages.folder_id` is what every folder-scoped query uses.
`messages.mailbox_id` is a denormalised copy of `folders.mailbox_id`, kept so that
account-scoped queries and quota accounting stay single-index fast:

* `messages_live_idx ON messages (folder_id, internal_date DESC) WHERE expunged_at IS NULL`
  serves the IMAP `SELECT` view of one folder.
* `messages_mailbox_date_idx ON messages (mailbox_id, internal_date DESC)`
  serves "all mail of this address" — the Webmail "All mail" view and the API's
  `?mailbox_id=` filter.

`INBOX` is a real `folders` row for every address, created by
`FoldersRepository::ensure_standard`, which also creates `Sent`, `Drafts`,
`Trash`, `Junk` and `Archive` with their `special_use` markers. `INBOX` carries
`special_use = NULL` on purpose: RFC 6154 reserves `\Inbox` for a different
purpose and clients must treat `INBOX` by name. Details in [imap.md](imap.md) §3.

### 9.2 `messages.rfc_message_id` holds the RFC 5322 `Message-ID` header

Specification §37 calls the header column `message_id`:

```sql
CREATE TABLE messages (
    id BIGSERIAL PRIMARY KEY,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id),
    message_id TEXT,
    ...
```

while the row's own identity is `id`. Every query, join and ad-hoc `psql` session
then has to disambiguate two things both called "message id": the `BIGSERIAL`
primary key that the API returns as `message_id`, and the RFC 5322 header value
that clients thread on. In the shipped schema the header column is named for what
it is:

| Specification §37 | Shipped | Meaning |
|---|---|---|
| `messages.id` | `messages.id` | the row's own identity; `BIGSERIAL PRIMARY KEY`; typed as `MessageId`; returned as `message_id` by the API |
| `messages.message_id TEXT` | `messages.rfc_message_id TEXT` | the RFC 5322 `Message-ID` header value, e.g. `<20260916091231.7f3a@example.net>`; typed as `RfcMessageId`; returned as `rfc_message_id` by the API |

The two are related but independent. `messages.id` is allocated by the database
and is what IMAP UIDs aside, every internal reference uses. `rfc_message_id` is
what the *sender* chose, is nullable (not every message has one, and malformed
ones are dropped rather than guessed), is only unique in the index
`messages_rfc_id_idx ON messages (rfc_message_id) WHERE rfc_message_id IS NOT NULL`
— deliberately *not* unique, because the same `Message-ID` legitimately appears
several times when a message is copied into multiple folders or delivered to
several recipients. `MessagesRepository::find_by_rfc_message_id(mailbox_id, …)`
is the lookup that uses it.

`RfcMessageId` in `crates/ferroma-core/src/ids.rs` is the Rust type; its
`RfcMessageId::generate(domain)` builds a value of the form
`<{timestamp}.{random:016x}.{pid:08x}@{domain}>`.

`thread_id` is separate again: it holds the root `Message-ID` of the `References`
chain, for conversation grouping.

---

## 10. Where each concern is documented

| Concern | Document |
|---|---|
| HTTP endpoints and error envelope | [api.md](api.md) |
| FCP wire format, cursors, realtime framing, chunked upload | [fcp.md](fcp.md) |
| SMTP commands, state machine, reply codes, outbound delivery | [smtp.md](smtp.md) |
| IMAP commands, states, UIDs, flags, folder naming | [imap.md](imap.md) |
| Schema, indexes, Maildir, quota, GC, backup | [storage.md](storage.md) |
| Sync model, tombstones, conflicts, Outbox, failure matrix | [sync.md](sync.md) |
| Threat model, controls, known gaps | [security.md](security.md) |
| DNS, compose files, TLS, backup/restore, troubleshooting | [deployment.md](deployment.md) |
| Official desktop client | [client.md](client.md) |
| Build quirks on this machine | [../AGENTS.md](../AGENTS.md) |
