# Storage

**Who should read this:** anyone touching `ferroma-storage`, anyone writing SQL
against a Ferroma database, and any operator reasoning about quota, disk usage,
garbage collection or restore.

Ferroma keeps two stores. PostgreSQL holds every fact the application queries —
who exists, which message is in which folder, what state a delivery is in.
The filesystem holds the bytes: RFC 5322 messages in a Maildir and attachments in
a content-addressed blob store. This document states which store is authoritative
for what, walks the schema table by table with the index that serves each query,
specifies the Maildir delivery algorithm and its durability guarantee, explains
quota accounting and where it is enforced, describes attachment
content-addressing and GC, and gives the backup model and the integrity-check
story.

> **Status:** describes implemented code. `ferroma-storage` is complete:
> `crates/ferroma-storage/src/{database,maildir,attachment,error,models}.rs` and
> `crates/ferroma-storage/src/repository/*.rs`. The schema is
> `migrations/0001_initial.sql` plus the three migrations after it. 0001 creates
> every table named below; 0002 comments the columns, 0003 lets a folder tombstone
> outlive its row, and 0004 widens `sessions.kind` to admit `jmap`. 0001's bytes
> are a checksum every existing database already recorded, so a later change to
> the schema is a new file, never an edit of that one.
> `ferroma storage stats|verify|gc` and the API's `GET /api/v1/storage` and
> `POST /api/v1/storage/gc` exist today. Where a `pg_dump`/`psql` command is given
> instead, it is a real command you can run today.

---

## 1. Two stores, one truth

```text
                        ┌──────────────────────────────┐
                        │        PostgreSQL            │
   authoritative for:   │  users, domains, mailboxes,  │
   *what exists*        │  folders, messages,          │
                        │  message_recipients,         │
                        │  attachments, mail_queue,    │
                        │  delivery_attempts, devices, │
                        │  sessions, client_sync_      │
                        │  states, drafts, operations, │
                        │  change_log, audit_logs,     │
                        │  login_attempts, settings    │
                        └──────────────┬───────────────┘
                                       │  messages.storage_path
                                       │  attachments.storage_path
                                       ▼
                        ┌──────────────────────────────┐
   authoritative for:   │        Filesystem            │
   *the bytes*          │                              │
                        │  Maildir:                    │
                        │    <root>/<domain>/<local>/  │
                        │      Maildir/{cur,new,tmp}   │
                        │      Maildir/.Folder/{…}     │
                        │                              │
                        │  Blob store:                 │
                        │    <root>/ab/cd/<sha256>     │
                        └──────────────────────────────┘
```

The division is stated in `migrations/0001_initial.sql`'s header and implemented
in `crates/ferroma-storage/src/lib.rs`:

> The database is authoritative for *what exists*; the filesystem is authoritative
> for *the bytes*. A row without its file is a `StorageError::BodyMissing`; a file
> without its row is garbage collected by `AttachmentStore::gc` and
> `Maildir::sweep_tmp`.

| Question | Answer comes from |
|---|---|
| Does `alice@example.com` exist? | `mailboxes` (+ `domains.enabled`) |
| Which folders does she have? | `folders` |
| How many unread? | `folders.unseen_count` |
| What is message 4821's subject? | `messages.subject` |
| What is message 4821's UID? | `messages.uid` |
| What are message 4821's bytes? | the file at `messages.storage_path` under the Maildir root |
| What is attachment 9's content? | the file at `attachments.storage_path` under the blob root |
| Was it delivered? | `mail_queue.status` |

Roots come from the config, not from the database:
`Config::maildir_root()` is `storage.maildir_root` or `<server.data_dir>/mail`,
and `Config::attachment_root()` is `storage.attachment_root` or
`<server.data_dir>/attachments`.

**Every path column is relative.** `messages.storage_path` looks like
`example.com/alice/Maildir/cur/1758012751.M4821_P3210.mail:2,S` — relative to the
Maildir root, and in a fixed form: forward slashes, always (`Maildir::relative`
joins with `/`). That is what makes a data directory relocatable: move the root,
update `server.data_dir`, and every path still resolves.

---

## 2. The schema, table by table

Four migrations, applied in order at startup when `database.run_migrations = true`.
`migrations/0001_initial.sql` (440 lines) creates the schema; `0002` adds the column
comments, `0003` drops the foreign key that kept a deleted folder's tombstone from
being written, and `0004` widens `sessions.kind` so a JMAP token is a session like
the others. They target PostgreSQL 14+ and use only core `gen_random_uuid()`-era
features — no extensions to install.

**An applied migration is frozen.** sqlx records a SHA-384 of each file and refuses
to start when the file and the record disagree (`VersionMismatch`). Editing 0001 to
add `jmap` would stop every database that had already applied it, which is why the
kind is added by 0004 instead. A fresh database runs both and ends at the same
constraint.

### 2.1 Identity

#### `users`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `email` | `TEXT NOT NULL` | unique, lower-cased |
| `password_hash` | `TEXT NOT NULL` | an Argon2id PHC string — see [security.md](security.md) §2 |
| `display_name` | `TEXT` | |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | a disabled account cannot log in |
| `is_admin` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `quota_bytes` | `BIGINT NOT NULL DEFAULT 1073741824` | 1 GiB |
| `used_bytes` | `BIGINT NOT NULL DEFAULT 0` | denormalised cache of the Maildir total |
| `failed_logins` | `INTEGER NOT NULL DEFAULT 0` | consecutive failures |
| `locked_until` | `TIMESTAMPTZ` | set by `record_login_failure` |
| `last_login_at` | `TIMESTAMPTZ` | |
| `created_at`, `updated_at` | `TIMESTAMPTZ NOT NULL DEFAULT NOW()` | |

Constraints: `users_email_lowercase CHECK (email = lower(email))`,
`users_email_not_blank CHECK (length(btrim(email)) > 3)`,
`users_quota_sane CHECK (quota_bytes >= 0)`, `users_used_sane CHECK (used_bytes >= 0)`.

Type: `User` in `crates/ferroma-storage/src/models.rs`; helper
`User::is_login_allowed(now)` reads `enabled` and `locked_until`.

#### `domains`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `name` | `TEXT NOT NULL` | unique, lower-cased |
| `description` | `TEXT` | |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | a disabled domain receives no mail |
| `catch_all` | `TEXT` | local part that receives mail addressed to a non-existent mailbox in this domain |
| `dkim_selector` | `TEXT` | per-domain selector |
| `dkim_private_key`, `dkim_public_key` | `TEXT` | PEM; see [security.md](security.md) §9 |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

`domains_name_lowercase`, `domains_name_not_blank` mirror the `users` checks.

**DKIM private keys live in the database here, and in `[dkim] private_key_path` in
the config.** Both are supported; the database column is what `GET
/api/v1/domains/:id/dkim` reads, and the file is what the signer prefers when
`dkim.enabled` is set. Back up both — see §8.

### 2.2 Addresses, aliases, folders

#### `mailboxes` — an address, not a folder

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `user_id` | `BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE` | |
| `domain_id` | `BIGINT NOT NULL REFERENCES domains(id) ON DELETE CASCADE` | |
| `local_part` | `TEXT NOT NULL` | lower-cased |
| `display_name` | `TEXT` | |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | |
| `is_primary` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `quota_bytes` | `BIGINT` | `NULL` = inherit the owner's |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

This is the SMTP `RCPT TO` target and the unit of quota accounting. It is *not*
an IMAP folder; see [architecture.md](architecture.md) §9.1 for why the
specification's §37 sketch was split.

#### `aliases`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `domain_id` | `BIGINT NOT NULL REFERENCES domains(id) ON DELETE CASCADE` | |
| `local_part` | `TEXT NOT NULL` | lower-cased |
| `target` | `TEXT NOT NULL` | full destination address; a bare local part means "same domain" |
| `enabled` | `BOOLEAN NOT NULL DEFAULT TRUE` | |
| `created_at` | `TIMESTAMPTZ` | |

#### `folders` — the IMAP folder

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `mailbox_id` | `BIGINT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE` | the owning address |
| `name` | `TEXT NOT NULL` | IMAP name, e.g. `Archive/2026` |
| `parent_id` | `BIGINT REFERENCES folders(id) ON DELETE CASCADE` | hierarchy |
| `special_use` | `TEXT` | `\Sent`, `\Drafts`, `\Trash`, `\Junk`, `\Archive`, `\All`, `\Flagged`, or `NULL` |
| `subscribed` | `BOOLEAN NOT NULL DEFAULT TRUE` | `LSUB` |
| `uid_validity` | `BIGINT NOT NULL DEFAULT 1` | IMAP UID generation |
| `uid_next` | `BIGINT NOT NULL DEFAULT 1` | the UID allocator |
| `highest_modseq` | `BIGINT NOT NULL DEFAULT 1` | reserved for `CONDSTORE` |
| `message_count` | `INTEGER NOT NULL DEFAULT 0` | counter, kept by `recount` |
| `unseen_count` | `INTEGER NOT NULL DEFAULT 0` | counter |
| `total_bytes` | `BIGINT NOT NULL DEFAULT 0` | counter |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

Constraints: `folders_name_not_blank`, and `folders_special_use_known` restricting
`special_use` to the seven values above.

### 2.3 Messages

#### `messages`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | typed `MessageId`; returned by the API as `message_id` |
| `folder_id` | `BIGINT NOT NULL REFERENCES folders(id) ON DELETE CASCADE` | **the authoritative parent** |
| `mailbox_id` | `BIGINT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE` | denormalised copy of `folders.mailbox_id` |
| `uid` | `BIGINT NOT NULL` | IMAP UID, unique per folder |
| `rfc_message_id` | `TEXT` | the RFC 5322 `Message-ID` header — [architecture.md](architecture.md) §9.2 |
| `thread_id` | `TEXT` | root `Message-ID` of the `References` chain |
| `subject` | `TEXT` | decoded |
| `sender` | `TEXT` | the `From` address |
| `sender_name` | `TEXT` | the `From` display name |
| `snippet` | `TEXT` | short, body-free preview for list views and notifications |
| `size_bytes` | `BIGINT NOT NULL` | |
| `storage_path` | `TEXT NOT NULL` | relative to the Maildir root |
| `checksum_sha256` | `TEXT` | lower-case hex, when `storage.checksum = true` |
| `flags` | `TEXT NOT NULL DEFAULT ''` | `Flags::to_db_string()` form, e.g. `seen,flagged` |
| `internal_date` | `TIMESTAMPTZ NOT NULL DEFAULT NOW()` | IMAP `INTERNALDATE` |
| `received_at` | `TIMESTAMPTZ NOT NULL DEFAULT NOW()` | when Ferroma accepted it |
| `sent_at` | `TIMESTAMPTZ` | the `Date` header, when parseable |
| `has_attachments`, `attachment_count` | `BOOLEAN` / `INTEGER` | denormalised for list views |
| `is_draft` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `modseq` | `BIGINT NOT NULL DEFAULT 1` | reserved for `CONDSTORE` |
| `deleted_at` | `TIMESTAMPTZ` | soft delete (`\Deleted`), before expunge |
| `expunged_at` | `TIMESTAMPTZ` | gone from the client's view; the row and file may still exist |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

Constraints: `messages_size_sane CHECK (size_bytes >= 0)`,
`messages_uid_sane CHECK (uid > 0)`.

The two-stage deletion is worth spelling out, because it is the difference
between "the user marked it deleted" and "the message is gone":

```text
live            expunged_at IS NULL AND deleted_at IS NULL
\Deleted        deleted_at  IS NOT NULL   (IMAP STORE +FLAGS \Deleted, API DELETE)
expunged        expunged_at IS NOT NULL   (IMAP EXPUNGE, API DELETE ?permanent=true)
```

Every read path filters `expunged_at IS NULL` — `find_by_uid`, `list_by_uids`,
`list_by_folder`, `list_unexpunged`, `count_by_folder`, `newest`, `search`.
Rows are removed by `hard_delete`, and only after the Maildir file has been
unlinked.

#### `message_recipients`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | |
| `message_id` | `BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE` | |
| `kind` | `TEXT NOT NULL` | `to`, `cc`, `bcc`, `reply-to`, `sender` |
| `address` | `TEXT NOT NULL` | |
| `display_name` | `TEXT` | |
| `ordinal` | `INTEGER NOT NULL DEFAULT 0` | preserves the header's order |

`message_recipients_kind_known` restricts `kind`. This table exists so that
`IMAP SEARCH TO/CC/BCC`, the API's recipient filter and the Admin search are one
indexed query instead of a header re-parse per message.

#### `attachments`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | typed `AttachmentId` |
| `message_id` | `BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE` | |
| `filename` | `TEXT` | |
| `content_type` | `TEXT NOT NULL DEFAULT 'application/octet-stream'` | |
| `size_bytes` | `BIGINT NOT NULL` | |
| `storage_path` | `TEXT NOT NULL` | `ab/cd/<sha256>`, relative to the blob root |
| `content_id` | `TEXT` | `Content-ID` for inline parts |
| `is_inline` | `BOOLEAN NOT NULL DEFAULT FALSE` | |
| `checksum_sha256` | `TEXT` | the same digest as the path |
| `created_at` | `TIMESTAMPTZ` | |

**Many rows, one blob.** An attachment row is (message, part, metadata); the file
it points at is shared with every other row holding the same bytes. Deleting a
message removes its rows but not necessarily the blob — §6 explains why.

### 2.4 Outbound queue

#### `mail_queue`

| Column | Type | Notes |
|---|---|---|
| `id` | `BIGSERIAL PK` | typed `QueueId` |
| `message_id` | `BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE` | |
| `user_id` | `BIGINT REFERENCES users(id) ON DELETE SET NULL` | `NULL` for system mail |
| `sender` | `TEXT NOT NULL` | envelope reverse-path |
| `recipient` | `TEXT NOT NULL` | **one row per recipient** |
| `status` | `TEXT NOT NULL DEFAULT 'pending'` | `pending`, `delivering`, `delivered`, `retry`, `failed`, `cancelled` |
| `attempts` | `INTEGER NOT NULL DEFAULT 0` | |
| `max_attempts` | `INTEGER NOT NULL DEFAULT 12` | `queue.max_attempts` |
| `next_attempt_at` | `TIMESTAMPTZ` | when the dispatcher may retry |
| `last_attempt_at` | `TIMESTAMPTZ` | |
| `delivered_at` | `TIMESTAMPTZ` | |
| `last_error` | `TEXT` | |
| `last_status_code` | `INTEGER` | the remote SMTP code |
| `last_status_text` | `TEXT` | the remote's text |
| `remote_mx` | `TEXT` | the host that was tried |
| `created_at`, `updated_at` | `TIMESTAMPTZ` | |

`mail_queue_status_known` restricts `status` to the six values above, and
`mail_queue_attempts_sane CHECK (attempts >= 0)`. Retry timing is
[smtp.md](smtp.md) §11.3.

#### `delivery_attempts`

One row per attempt: `queue_id` (FK, cascade), `attempt`, `remote_mx`,
`status_code`, `status_text`, `error`, `duration_ms`, `created_at`. This is the
history the Admin "Delivery Logs" screen reads. It grows with every retry, which
is why `queue.retention_days` (30) exists and why the index is
`(queue_id, attempt)` rather than `(created_at)`.

### 2.5 Sessions, devices, client sync state

#### `devices`

`id`, `user_id` (FK, cascade), `device_uid TEXT` (client-generated, stable per
installation), `name`, `platform`, `client_version`, `protocol_version`,
`last_seen_at`, `last_ip`, `created_at`, `revoked_at`.

`devices_uid_key UNIQUE (user_id, device_uid)` is what makes device registration
idempotent: `DevicesRepository::upsert` can be called on every client start.

#### `sessions`

`id`, `user_id`, `kind` (`web`, `api`, `client`, `imap`, `smtp`, and `jmap` once
`0004_sessions_allow_jmap.sql` has widened `sessions_kind_known`), `token_hash TEXT`,
`device_id BIGINT REFERENCES devices(id) ON DELETE SET NULL`, `ip`, `user_agent`,
`created_at`, `last_seen_at`, `expires_at`, `revoked_at`. 0001 creates the check
without `jmap`; 0004 drops it and adds it back with the extra kind, so an existing
database and a fresh one accept the same set.

**The raw token is never stored.** `sessions.token_hash` is the SHA-256 of an
opaque refresh token (`TokenService::hash`), so a database dump does not hand an
attacker working sessions. Access tokens are stateless JWTs and are not in this
table at all.

#### `client_sync_states`

`id`, `device_id` (FK, cascade), `mailbox_id` (FK, cascade), `folder_id` (FK,
cascade; `NULL` = account level), `cursor BIGINT NOT NULL DEFAULT 0`,
`updated_at`.

`client_sync_states_key UNIQUE (device_id, mailbox_id, COALESCE(folder_id, 0))`.
The `COALESCE` is load-bearing: PostgreSQL treats `NULL`s as distinct in a unique
index, so without it one device could accumulate unlimited account-level rows.
`SyncStatesRepository::get` returns `0` for a missing row, which is exactly "has
never synced".

### 2.6 Drafts

#### `drafts`

`id`, `user_id`, `mailbox_id`, `folder_id`, `message_id` (the mirrored copy in the
Drafts folder), `subject`, `body_text`, `body_html`, `recipients JSONB DEFAULT
'[]'`, `attachments JSONB DEFAULT '[]'`, `in_reply_to`, `reference_ids JSONB
DEFAULT '[]'`, `created_at`, `updated_at`.

Drafts are JSON in the row *and* a real message in the Drafts folder, so an IMAP
client and the official client see the same draft — [fcp.md](fcp.md) §7.

### 2.7 Operations, change log, audit

#### `operations` — idempotency

`operation_id TEXT PRIMARY KEY`, `user_id`, `kind`, `status` (`applied` or
`failed`), `result JSONB` (the cached response), `created_at`, `completed_at`.

The primary key *is* the `op_…` string the client generated. `begin()` is a single
`INSERT … ON CONFLICT DO NOTHING RETURNING *`, so the claim is atomic.

#### `change_log` — the sync journal

`seq BIGSERIAL PK`, `user_id`, `mailbox_id`, `folder_id`, `message_id BIGINT`,
`kind`, `payload JSONB`, `created_at`.

**`message_id` and `folder_id` have no foreign key, deliberately.** The schema
comment says why: *"tombstones must outlive rows"*. A `message_deleted` entry has to
remain readable after the `messages` row is gone, and a `folder_deleted` one after the
`folders` row is gone, or an offline client would never learn that either
disappeared. `folder_id` lost its constraint in
`migrations/0003_change_log_folder_tombstones.sql`: while it had one, the insert that
records a folder deletion violated it and `DELETE /api/v1/folders/:id` answered `500`.
`mailbox_id` keeps its cascade — no `mailbox_deleted` kind exists.
`seq` is the sync cursor — see [sync.md](sync.md).

#### `audit_logs`

`id`, `actor_user_id` (FK `ON DELETE SET NULL` — an audit row outlives the account
it names), `action`, `target_type`, `target_id`, `ip`, `user_agent`,
`details JSONB DEFAULT '{}'`, `created_at`.

### 2.8 Login throttling and settings

#### `login_attempts`

`id`, `email`, `ip`, `kind TEXT NOT NULL DEFAULT 'password'`, `success BOOLEAN
NOT NULL`, `created_at`. Every login attempt writes a row, success or failure;
`AuthService::login` reads them through
`LoginAttemptsRepository::count_failures_for_ip(ip, window)` *before* doing any
Argon2 work, so a credential flood cannot burn the CPU.

#### `settings`

`key TEXT PRIMARY KEY`, `value JSONB NOT NULL`, `updated_at`. DB-backed settings
that the Admin console can change without a restart. They do **not** override
`ferroma.toml`: a value here is a runtime knob, not configuration, and nothing in
`Config` reads this table.

---

## 3. Indexes, and the query each one serves

An index that no query uses is a write cost for nothing. This is the full list
from `migrations/0001_initial.sql`, with the query it exists for.

| Index | Table | Serves |
|---|---|---|
| `users_email_key` | `users` | login: `UsersRepository::find_by_email` |
| `users_admin_idx` … `WHERE is_admin` | `users` | partial: the (small) admin list |
| `domains_name_key` | `domains` | `RCPT TO` domain resolution, `DomainsRepository::find_by_name` |
| `mailboxes_address_key` | `mailboxes` | **the SMTP delivery lookup**: `(domain_id, local_part)` — unique, so an address cannot be duplicated |
| `mailboxes_user_idx` | `mailboxes` | `list_by_user`, the caller's address list |
| `mailboxes_primary_key` … `WHERE is_primary` | `mailboxes` | partial unique: at most one primary address per user |
| `aliases_key` | `aliases` | `(domain_id, local_part)` — the alias lookup on `RCPT TO` |
| `folders_name_key` | `folders` | `(mailbox_id, name)` unique — `find_by_name`, and the guard that stops duplicate folders |
| `folders_mailbox_idx` | `folders` | `list(mailbox_id)` |
| `folders_special_use_key` … `WHERE special_use IS NOT NULL` | `folders` | partial unique: at most one `\Sent` (etc.) per address |
| `messages_folder_uid_key` | `messages` | **`(folder_id, uid)` unique** — `find_by_uid`, `list_by_uids`, `FETCH`/`STORE` by UID |
| `messages_live_idx` | `messages` | **`(folder_id, internal_date DESC) WHERE expunged_at IS NULL`** — the IMAP `SELECT` view and the folder message list |
| `messages_mailbox_date_idx` | `messages` | `(mailbox_id, internal_date DESC)` — "all mail of this address", the Webmail All-Mail view |
| `messages_folder_date_idx` | `messages` | `(folder_id, internal_date DESC)` — folder listing including expunged rows (integrity checks, Admin) |
| `messages_rfc_id_idx` … `WHERE rfc_message_id IS NOT NULL` | `messages` | partial, **not unique**: `find_by_rfc_message_id` for threading and dedup |
| `messages_thread_idx` … `WHERE thread_id IS NOT NULL` | `messages` | partial: conversation grouping |
| `messages_sender_idx` | `messages` | `SEARCH FROM`, the API's sender filter |
| `messages_subject_fts_idx` | `messages` | GIN over `to_tsvector('simple', coalesce(subject, ''))` — `SEARCH SUBJECT`, the API's `?query=` |
| `message_recipients_message_idx` | `message_recipients` | recipients of one message |
| `message_recipients_address_idx` | `message_recipients` | `SEARCH TO/CC/BCC`, "mail to this address" |
| `attachments_message_idx` | `attachments` | attachments of one message |
| `mail_queue_due_idx` | `mail_queue` | **`(next_attempt_at) WHERE status IN ('pending','retry')`** — the dispatcher's "what is due now?" hot path |
| `mail_queue_message_idx` | `mail_queue` | queue rows of one message |
| `mail_queue_status_idx` | `mail_queue` | Admin queue filtering by status |
| `mail_queue_user_idx` | `mail_queue` | `(user_id, created_at DESC) WHERE user_id IS NOT NULL` — a user's own Outbox view |
| `delivery_attempts_queue_idx` | `delivery_attempts` | `(queue_id, attempt)` — the attempt history of one queue row |
| `devices_uid_key` | `devices` | `(user_id, device_uid)` unique — idempotent device upsert |
| `devices_user_idx` | `devices` | a user's device list |
| `sessions_token_key` | `sessions` | `(token_hash)` unique — refresh-token lookup |
| `sessions_user_idx` | `sessions` | "active sessions" in Admin |
| `sessions_expiry_idx` … `WHERE revoked_at IS NULL` | `sessions` | partial: the expiry sweeper |
| `client_sync_states_key` | `client_sync_states` | `(device_id, mailbox_id, COALESCE(folder_id, 0))` unique — cursor read/write |
| `client_sync_states_device_idx` | `client_sync_states` | `(device_id, updated_at DESC)` — "what is this device syncing?" |
| `drafts_user_idx` | `drafts` | `(user_id, updated_at DESC)` — the draft list |
| `operations_user_idx` | `operations` | `(user_id, created_at DESC)` — operation history |
| `operations_created_idx` | `operations` | `(created_at)` — `purge_older_than` |
| `change_log_cursor_idx` | `change_log` | **`(user_id, seq)`** — `changes_since(user_id, after, limit)`, the sync query |
| `change_log_mailbox_idx` | `change_log` | `(mailbox_id, seq)` — per-address sync |
| `change_log_folder_idx` | `change_log` | `(folder_id, seq)` — per-folder sync |
| `change_log_created_idx` | `change_log` | `(created_at)` — retention pruning |
| `audit_logs_actor_idx`, `audit_logs_action_idx`, `audit_logs_created_idx` | `audit_logs` | the three Admin audit filters |
| `login_attempts_email_idx`, `login_attempts_ip_idx` | `login_attempts` | `(email, created_at DESC)` and `(ip, created_at DESC)` — throttling windows |
| `login_attempts_created_idx` | `login_attempts` | `(created_at)` — the retention sweep |

The five bolded ones are on the critical path. If a query plan shows a sequential
scan on `messages_live_idx` or `mail_queue_due_idx`, something is wrong with the
query, not the index.

Two things to know before adding an index: there is no index on
`messages.flags`, so `SEARCH UNSEEN` is a filtered scan within a folder — fine for
mailbox-sized folders, not fine for a million-row folder; and there is no body
index, which is why `SEARCH BODY` is not supported ([imap.md](imap.md) §8).

---

## 4. Maildir layout and the delivery algorithm

### 4.1 Layout

`storage.layout = "maildir"` (the default) produces the layout from specification
§13:

```text
<maildir_root>/
└── example.com/                       <- domains.name, lower-cased, sanitised
    └── alice/                         <- mailboxes.local_part, lower-cased, sanitised
        └── Maildir/                   <- INBOX
            ├── cur/                   seen by a mail client; flags in the file name
            ├── new/                   delivered but not yet seen by a client
            ├── tmp/                   half-written files; never read
            ├── .Sent/{cur,new,tmp}
            ├── .Drafts/{cur,new,tmp}
            ├── .Trash/{cur,new,tmp}
            ├── .Junk/{cur,new,tmp}
            ├── .Archive/{cur,new,tmp}
            └── .Archive.2026/{cur,new,tmp}   <- IMAP "Archive/2026"
```

`storage.layout = "maildirperfolder"` produces one Maildir per folder instead:
`<root>/example.com/alice/INBOX/{cur,new,tmp}`, `…/Sent/{cur,new,tmp}`, with no
dotted directory names. Choose it on filesystems with unusual name rules — see
[imap.md](imap.md) §10.

`Maildir::SUBDIRS` is the literal `["cur", "new", "tmp"]`; `Maildir::INBOX` is
`"INBOX"`.

### 4.2 The file name

```text
1758012751.M4821_P3210.mail:2,S
└───┬────┘ └─┬──┘└─┬─┘ └┬─┘ │ │
unix secs   pid        host │ └── flags: S = \Seen
                counter     └──── version marker "2,"
```

`Maildir::unique_filename` builds `<secs>.<pid>_<counter>.<hostname>` and appends
`{sep}2,{flags}` through `with_info`. Uniqueness is guaranteed in-process by the
`AtomicU64` counter and across processes by the pid; the hostname is
`sanitize_component`d first, falling back to `ferroma`.

The separator is `:` on Unix and `;` on Windows, and reads accept both — the full
explanation is in [imap.md](imap.md) §10.

### 4.3 The delivery algorithm

`Maildir::store(domain, local_part, folder, bytes, flags)`:

```text
 1. dir = folder_dir(domain, local_part, folder)
       └─ sanitize_component on the domain and the local part,
          maildir_folder_name on the folder  (path-traversal defence)
 2. create dir/{cur,new,tmp} if missing
 3. maildir_flags = flags_to_maildir(flags)      // "seen" -> "S"
 4. filename = unique_filename(maildir_flags)
 5. tmp_path   = dir/tmp/<filename>.tmp
 6. final_sub  = if maildir_flags.is_empty() { "new" } else { "cur" }
    final_path = dir/<final_sub>/<filename>
 7. std::fs::write(tmp_path, bytes)                  ← the whole message
 8. if storage.fsync_on_write: sync_all() on tmp_path
 9. std::fs::rename(tmp_path, final_path)            ← the atomic step
10. if storage.fsync_on_write: sync_all() on dir/<final_sub>
11. return StoredMessage { path (relative), size, sha256 }
```

Three properties, and why each matters:

**Atomicity.** `rename(2)` within one filesystem is atomic: a reader sees either
no file or the complete file. A message is therefore never observed half-written.
This is the entire reason for the `tmp/` step and it is why a reader must never
look in `tmp/` — `Maildir::iter_messages` skips anything ending in `.tmp`, and
`Maildir::sweep_tmp(older_than_secs)` deletes leftovers.

**Durability.** Steps 8 and 10 flush the data *and* the directory entry, so a
power loss cannot leave a rename pointing at unflushed blocks. `fsync` costs
throughput; `storage.fsync_on_write = true` is the default because a mail server
that acknowledges `DATA` and then loses the message has broken its only promise.
An operator with battery-backed storage can turn it off.

**Idempotence of the name.** `store` never overwrites: every call allocates a new
counter value. `set_flags` is the idempotent one — it computes the target name and
returns the original path unchanged when nothing needs to move.

### 4.4 The rest of the Maildir API

| Function | Behaviour |
|---|---|
| `read(relative_path)` | the whole message; a missing file is `StorageError::BodyMissing`, not a generic IO error |
| `read_prefix(relative_path, limit)` | the first `limit` bytes, for header-only `FETCH` |
| `delete(relative_path)` | unlink; already-missing is `Ok(())`, so it is retry-safe |
| `set_flags(relative_path, flags)` | rename the file and move it between `new/` and `cur/` as the flag set becomes non-empty or empty; returns the possibly-new path |
| `move_message(relative_path, domain, local_part, to_folder, flags)` | read, store in the destination, delete the source |
| `iter_messages(domain, local_part, folder)` | every file in `cur/` and `new/`, skipping `.tmp`; returns `MaildirEntry { path, size, maildir_flags, modified_secs }` |
| `usage(domain, local_part)` | total bytes under the mailbox root, for quota reconciliation |
| `sweep_tmp(older_than_secs)` | delete abandoned `tmp/` files older than the threshold |
| `ensure_mailbox` / `create_folder` / `delete_folder` / `rename_folder` / `list_folders` / `folder_exists` | folder lifecycle; `INBOX` is protected from deletion and renaming |
| `absolute(relative_path)` | the path-traversal gate — see §7 |

Note the asymmetry in `move_message`: it is a copy-then-delete, not a `rename`.
A cross-folder move may cross a filesystem boundary, and the copy path is also
what lets the destination get a freshly encoded file name for the new flag set.
The window in which both copies exist is harmless — the database row is updated
in one statement afterwards, so nothing points at the source once the transaction
commits.

---

## 5. Quota accounting

Two numbers: `users.quota_bytes` (default
`limits.mailbox_quota` = 1073741824 = 1 GiB) and `mailboxes.quota_bytes`
(`NULL` = inherit the owner's). `FoldersRepository` does not participate; quota is
per **address**, because that is the unit SMTP delivers to.

| Step | Where |
|---|---|
| Read the effective limit | `MailboxesRepository::quota(mailbox_id)` |
| Read current usage | `MailboxesRepository::used_bytes(mailbox_id)` — `SELECT used_bytes FROM users` |
| Decide | `MailboxesRepository::check_quota(mailbox_id, needed)` — returns `StorageError::QuotaExceeded { mailbox_id, used, needed, limit }` |
| Adjust after a write | `MailboxesRepository::add_usage(mailbox_id, delta_bytes)` |
| Reconcile from disk | `MailboxesRepository::recompute_usage(mailbox_id)` — walks the Maildir with `Maildir::usage` and writes the total back |

Where it is enforced, and what the enforcement looks like from outside:

| Path | Checked at | Result |
|---|---|---|
| Inbound SMTP | before the Maildir write, after `DATA` is complete | `452 4.2.2 Mailbox full` — **temporary**, so the sender retries ([smtp.md](smtp.md) §12.1) |
| `POST /api/v1/messages` (send) | before the Sent copy is written and before queueing | `413 limit_exceeded` |
| `POST /api/v1/attachments` | against the target mailbox when the attachment is attached | `413 limit_exceeded` |
| IMAP `APPEND` | before the literal is stored | `NO [OVERQUOTA]` |
| IMAP/API flag changes and moves | not checked — they do not change the total | — |

`used_bytes` is a cache, and `recompute_usage` is how it is corrected. It can
drift after a crash between the file write and the counter update, or after an
operator adds files to the Maildir by hand. The reconciliation is:

```sql
-- What the database believes, per address.
SELECT m.id, d.name || '@' || m.local_part AS address, u.used_bytes
  FROM mailboxes m
  JOIN domains d ON d.id = m.domain_id
  JOIN users   u ON u.id = m.user_id
 ORDER BY u.used_bytes DESC;
```

```bash
# What is actually on disk for one address.
du -sb /var/lib/ferroma/mail/example.com/alice
```

If they disagree, `recompute_usage` is the repair, and the discrepancy is worth
investigating: a large positive difference means files were deleted behind
Ferroma's back, a large negative one means an interrupted write.

`storage.enforce_quota = false` turns the check off entirely. It exists for
migrations and for operators who would rather deliver everything and sort it out
later; a server in that mode will fill its disk.

---

## 6. Attachments: content addressing and GC

`AttachmentStore` in `crates/ferroma-storage/src/attachment.rs`.

### 6.1 Addressing

```text
<attachment_root>/ab/cd/abcdef0123…      <- SHA-256 of the content, hex, lower-case
                  └┬┘└┬┘└─────┬─────┘
              byte 0-1  2-3   the full digest
```

`AttachmentStore::path_for_digest(digest_hex)` builds that path and rejects a
digest shorter than four characters or containing a non-hex character with
`StorageError::Invalid`. Two levels of sharding keep any one directory from
holding hundreds of thousands of entries, which is what makes `readdir` on the
blob root cheap on ext4 and NTFS alike.

Consequences of addressing by content:

| Property | Effect |
|---|---|
| Identical content is stored once | a PDF forwarded around an organisation occupies one blob, however many `attachments` rows point at it |
| `store` is idempotent | storing the same bytes twice returns `StoredBlob { deduplicated: true }` without writing |
| `ETag` is the SHA-256 | `GET /api/v1/attachments/:id` can serve a strong validator with no extra work, and repeated downloads never re-transfer ([fcp.md](fcp.md) §6) |
| A blob cannot be corrupted silently | the file name *is* the checksum — `sha256sum` over the tree verifies the whole store |

### 6.2 Writing a blob

```text
1. digest   = sha256(data)
2. relative = path_for_digest(digest)          // "ab/cd/<sha256>"
3. if <root>/<relative> is already a file: return { deduplicated: true }
4. create_dir_all(parent)
5. tmp = parent/".{digest[4..16]}.{pid}.tmp"
6. write(tmp, data)
7. if storage.fsync_on_write: sync_all(tmp)
8. rename(tmp, final)      // on Unix: atomic overwrite with identical content
                           // on Windows: the first writer wins
9. return { path, size, sha256, deduplicated: false }
```

Step 8 is a race with a benign outcome: if two concurrent writers produce the same
digest, `rename` either overwrites with identical bytes (Unix) or fails because
the destination exists (Windows), and the `Err(_) if final_path.is_file()` arm
cleans up the temporary file and reports success. Either way the bytes match the
name.

### 6.3 Garbage collection

**Deleting an `attachments` row does not delete the blob.** It cannot: another
message may reference the same digest, and the store has no refcount. That is why
`AttachmentStore::gc(keep)` takes a set and does the job in one pass:

```text
for every file under the blob root:
    if the name ends in .tmp          -> remove  (an interrupted write)
    if keep contains the file name    -> keep
    if keep contains the relative path-> keep    (both forms are accepted)
    otherwise                         -> remove
```

The `keep` set is produced by `AttachmentsRepository::referenced_paths()`
(`SELECT DISTINCT storage_path FROM attachments`), and the caller is
`POST /api/v1/storage/gc` (or `ferroma storage gc` on the host). Two rules for
anyone re-implementing it:

* **Collect the keep-set before scanning, not during.** Reading `attachments`
  while walking the tree races with a concurrent upload, and the failure mode is
  deleting a blob that was attached a millisecond ago.
* **`gc` also sweeps `.tmp` files**, so a crash mid-upload does not leak scratch
  files forever. `AttachmentStore::total_size` excludes them for the same reason
  the sweeper removes them.

`Maildir::sweep_tmp(older_than_secs)` is the equivalent for messages, but with a
threshold: a `tmp/` file younger than the threshold might belong to a delivery
that is still in progress on another task, so sweeping with a zero threshold is
only safe on a stopped server.

### 6.4 Verifying the blob store

```bash
# Every blob's file name must equal the SHA-256 of its content.
cd /var/lib/ferroma/attachments
find . -type f ! -name '*.tmp' -printf '%f %p\n' | while read digest path; do
    actual=$(sha256sum "$path" | cut -d' ' -f1)
    [ "$actual" = "$digest" ] || echo "CORRUPT: $path (name $digest, content $actual)"
done
```

Illustrative output when everything is fine — the command prints nothing.
Attachments are **not** covered by `messages.checksum_sha256`; the file name is
the checksum, which is why this check needs no database at all.

---

## 7. Path-traversal defences

Three places take a string from outside and turn it into a filesystem path:
domain names, local parts, folder names, message `storage_path`s and attachment
`storage_path`s. All three go through one of two gates.

### 7.1 `sanitize_component`

```rust
/// Reject anything that could be used to escape the mail root.
pub fn sanitize_component(component: &str) -> Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty() {
        return Err(StorageError::Invalid("empty path component".into()));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(StorageError::Invalid(format!("invalid path component: {trimmed}")));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(StorageError::Invalid(format!(
            "path component contains a separator: {trimmed}"
        )));
    }
    if trimmed.contains(':') {
        return Err(StorageError::Invalid(format!(
            "path component contains a colon: {trimmed}"
        )));
    }
    Ok(trimmed.to_string())
}
```

Called on every component of a folder path before it is joined
(`maildir_folder_name`), on the domain and local part in `Maildir::mailbox_dir`,
and on the hostname in `Maildir::new`. Its test covers the interesting inputs:

```rust
for bad in ["..", ".", "a/b", "a\\b", "", "  ", "a:b", "x\0y"] {
    assert!(sanitize_component(bad).is_err(), "should reject {bad:?}");
}
```

The `:` rejection is deliberate even though `:` is legal on Unix: it is the
Maildir info separator, and a folder named `a:b` would produce a directory whose
name is ambiguous with a flagged file. On Windows it is also an NTFS alternate
data stream.

A name is validated **per segment**, so an IMAP folder `Archive/2026` is fine
(each segment is clean) while `Archive/../../etc` is rejected on its third
segment rather than by pattern-matching the whole string.

### 7.2 `absolute` — the relative-path gate

Both stores have one, and both do the same thing:

```rust
/// Turn a relative path into an absolute one, refusing to escape the root.
pub fn absolute(&self, relative_path: &str) -> Result<PathBuf> {
    let candidate = Path::new(relative_path);
    if candidate.is_absolute() {
        return Err(StorageError::Invalid(format!(
            "storage path must be relative: {relative_path}"
        )));
    }
    for component in candidate.components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(StorageError::Invalid(format!(
                    "storage path escapes the mail root: {relative_path}"
                )));
            }
            _ => {}
        }
    }
    Ok(self.root.join(candidate))
}
```

`Maildir::absolute` (mailroot) and `AttachmentStore::absolute` (blob root) are
the only functions that turn a stored `storage_path` into a real path. `read`,
`read_prefix`, `delete`, `set_flags` and `exists` all call it first, so no code
path can open a file outside the root even if the database is compromised:

```rust
assert!(m.absolute("../../etc/passwd").is_err());
assert!(m.absolute("/etc/passwd").is_err());
assert!(s.absolute("../../secret").is_err());
```

`Component::Prefix(_)` matters on Windows: without it, `C:\Windows\...` would
be treated as a relative path and joined onto the root.

The complementary function is `Maildir::relative`, which strips the root and
falls back to `with_info`-style normalisation; a path that does not start with
the root is `StorageError::Invalid("path outside the mail root")` rather than a
silently stored absolute path.

---

## 8. Backup and restore

**Ferroma ships one command for both halves, and nothing else.** `ferroma storage
export` writes one archive — the database dump and the volume, taken together —
and `ferroma storage import` puts that archive back. There is still no `backup`
service in either compose file, no `ferroma-backups` volume, and no `backup` or
`restore` subcommand in `scripts/deploy.sh`: scheduling and off-site retention stay
the operator's job. What belongs here is what the archive contains, why the two
halves cannot be separated, and the order a restore follows; the step-by-step
operator walkthrough is in [deployment.md](deployment.md) §8.

### 8.1 The two halves, and why neither alone is a backup

Ferroma keeps two stores (§1), so a backup is two halves, and they are kept
together:

| Half | What it holds |
|---|---|
| The PostgreSQL database | every fact the application queries: `mailboxes`, `folders`, `messages`, `attachments`, `mail_queue`, the change log |
| The `ferroma-data` volume | the bytes: the Maildir, the attachment blob store, the DKIM private key, and the data directory's own files — `<data_dir>/database.json` (the database address this instance remembers) and `<data_dir>/jwt_secret` when the secret was generated rather than configured |

| Restored alone | Result |
|---|---|
| Database only | every `messages.storage_path` points at a file that is not there. Every read is `StorageError::BodyMissing`; users see an inbox of subjects with no bodies |
| Maildir only | the files exist and nothing knows about them. Folders appear empty; the bytes are invisible until an operator re-imports them by hand |

The database is authoritative for *what exists* and the filesystem for *the bytes*,
and neither can be reconstructed from the other — which is why a backup that
contains only one of the two is not a backup. Whatever backs up the volume must
treat it as secret-bearing: it carries the DKIM private key, and usually
`<data_dir>/jwt_secret` as well. See [security.md](security.md) §13.

### 8.2 Consistency of a live backup

* **The database half is a single `pg_dump`**, so it is a consistent snapshot of
  one point in time.
* **The volume half is a live copy.** Maildir writes are atomic renames, so a
  `tar` of the volume can miss a delivery that was in flight but can never contain
  a half-written file: a message delivered during the copy may be present as a
  file and absent from the database (an orphan), or — much less likely — absent
  from both, in which case the SMTP transaction that delivered it had not yet been
  acknowledged and the sender will retry.
* **The two halves are not the same instant.** The order decides which way the
  mismatch points: take the database dump **first** and copy the volume **second** —
  a message delivered in between is then an extra file with no row, the safe
  direction, and the same one the delivery algorithm takes when it writes
  the file before the row. Reverse the order and the window instead leaves rows
  whose files are missing: `BodyMissing` for the user, with no bytes to recover
  from. Do not pair a filesystem snapshot of the volume with a live `pg_dump` and
  assume the two agree.

If you need the halves to be exactly one instant, stop the application for the
whole window:

```bash
docker compose stop ferroma
docker compose exec -T postgres sh -c \
  'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom --compress=6' > ferroma.dump
docker run --rm -v ferroma-data:/data -v "$PWD":/backup alpine \
  tar -czf /backup/ferroma-data.tar.gz -C /data .
docker compose start ferroma
```

`docker compose stop` stops only the application: mail already accepted into the
queue is in the database, and a peer MTA whose connection drops retries, so the
window costs a delivery delay rather than a message.

The external-database shape has no `postgres` service to `exec` into, so take the
database half with a throwaway client container instead — same order:

```bash
docker run --rm --network host -e PGPASSWORD postgres:16-alpine \
  pg_dump -h 127.0.0.1 -U ferroma -d ferroma --format=custom --compress=6 > ferroma.dump
```

`PGPASSWORD` comes from the environment (`.env`), and `-U`/`-d` are that file's
`POSTGRES_USER`/`POSTGRES_DB` (both `ferroma` by default); everything about the
volume half is unchanged.

For most deployments the live pair is correct enough, because an orphan file is
benign and a missed message is retried by the sending MTA — but it is a pair to
verify rather than assume (see §8.3).

### 8.3 Restore ordering, and verifying afterwards

Restore in the order the dependency graph demands:

```text
1. database   pg_restore --no-owner --no-privileges --exit-on-error, into an empty database
2. volume     tar -xzf, over the ferroma-data volume
3. secrets    .env, TLS material, and the rest of §8.4
```

The database goes first because it is the half that defines what should exist; the
volume second so that by the time the server starts, every row already has its
file. Secrets and configuration last, so a restore that fails halfway does not
leave a running server pointed at the wrong TLS certificate.

```bash
docker compose stop ferroma

# 1. the database, into an empty database of its own
docker compose exec -T postgres sh -c \
  'pg_restore -U "$POSTGRES_USER" -d "$POSTGRES_DB" --no-owner --no-privileges --exit-on-error' < ferroma.dump

# 2. the volume; the tar was written as root, so a root extraction restores the
#    uid 10001 ownership the service needs
docker run --rm -v ferroma-data:/data -v "$PWD":/backup alpine \
  sh -c 'tar -xzf /backup/ferroma-data.tar.gz -C /data && chown -R 10001:10001 /data'

docker compose start ferroma
```

`pg_restore` is for a `--format=custom` dump; a `pg_dumpall` or plain-SQL dump is
fed to `psql` instead. Restore into a database that is empty: `pg_restore` reports
every object that already exists as an error, and silently merging two mail stores
is how an operator loses a week of mail — no tool can tell "restore on top" from
"oops, wrong database". If the target is not empty, stop and check it first.

Then verify the pair against §9's invariants, from inside the application
container:

```bash
docker compose exec ferroma ferroma storage verify --details
```

It reports how many message rows it checked, how many are missing their file, and
any size mismatch; `--details` prints the first 50 of each, and the run also
sweeps abandoned `tmp/` files older than an hour. A non-zero exit means the mail
store is not consistent. Two outcomes matter:

* **A missing body** means the two halves do not belong together — the backup was
  inconsistent, or the volume half was not restored. Do not serve mail until it is
  resolved; no row can be turned back into RFC 5322 bytes.
* **Orphan files** are the expected residue of a live backup (files delivered
  after the dump) and of an interrupted delivery. `ferroma storage verify` does
  not enumerate them; §9.2's `comm` listing is how to see them. Do not delete one
  before you are sure the restore was complete. `ferroma storage gc --dry-run` is
  a different inventory — unreferenced attachment blobs, not Maildir files.

Checksums, counters and `uid_next` are not part of `verify`; §9.3 and §9.4 cover
them for the cases where a matching size is not enough.

### 8.4 What else you must keep

| Item | Why | Where it lives |
|---|---|---|
| `ferroma.toml` | limits, ports, TLS paths, `api.public_url`, `server.hostname` | `config/ferroma.toml`, mounted read-only |
| `[api] jwt_secret` / `FERROMA_JWT_SECRET` | without it every session is invalidated on restart | the environment, or `<data_dir>/jwt_secret` inside the volume when it was generated rather than configured |
| DKIM private keys | losing them means every signature breaks and DMARC starts failing | `domains.dkim_private_key` **and** the file at `dkim.private_key_path` — `/var/lib/ferroma/dkim/<selector>.private` in the volume, or the `./dkim` read-only mount |
| TLS certificate and key | losing them means a TLS outage until reissue | `/etc/ferroma/tls/`, a `./tls` bind mount — not the data volume |
| `.env` | `POSTGRES_PASSWORD` for the database service, and often `FERROMA_JWT_SECRET` | gitignored, on the host; no shipped tooling excludes it for you |
| PostgreSQL role and database | a restore needs somewhere to restore into | the `postgres` service, or the host's own server |

An archive of the `ferroma-data` volume is secret-bearing — the DKIM private key
and usually `<data_dir>/jwt_secret` are inside it — and no script excludes
anything on your behalf any more. Encrypt the archive, keep it where you keep
secrets, and treat a copy of the volume with the same care as the database. See
[security.md](security.md) §13 and [deployment.md](deployment.md) §4.

---

## 9. Integrity checks and repair

The invariant to verify: **every live `messages` row has a readable file at its
`storage_path`, and every file under the mail root belongs to a row.**

### 9.1 Finding rows without bodies

```sql
-- The candidate set: live messages. Compare against the filesystem.
SELECT m.id, m.mailbox_id, m.uid, m.size_bytes, m.storage_path
  FROM messages m
 WHERE m.expunged_at IS NULL
 ORDER BY m.id;
```

```bash
# Rows whose file is missing.
psql "$DATABASE_URL" -Atc \
  "SELECT storage_path FROM messages WHERE expunged_at IS NULL" |
while read -r p; do
    [ -f "/var/lib/ferroma/mail/$p" ] || echo "MISSING: $p"
done
```

A missing body surfaces in production as `StorageError::BodyMissing`, which
becomes `NO [SERVERBUG] message body missing` over IMAP and a `404 not_found`
from the API. It means the filesystem was modified behind Ferroma, or a restore
put back a database newer than the Maildir.

### 9.2 Finding files without rows

```bash
# Orphan candidates under the mail root: files whose relative path is in no row.
psql "$DATABASE_URL" -Atc \
  "SELECT storage_path FROM messages" | sort > /tmp/known.txt
cd /var/lib/ferroma/mail
find . -type f ! -path '*/tmp/*' | sed 's|^\./||' | sort > /tmp/on_disk.txt
comm -13 /tmp/known.txt /tmp/on_disk.txt        # on disk, not in the database
```

Orphans are the expected residue of an interrupted delivery and of
`hard_delete` after a crash, and they are the safe failure direction to have.
They are not automatically deleted: a file that looks orphaned because a restore
was half-applied is the only copy of somebody's mail.

### 9.3 Checking the checksums

`messages.checksum_sha256` is written when `storage.checksum = true`
(`StoredMessage.sha256`, hex lower-case):

```bash
psql "$DATABASE_URL" -Atc \
  "SELECT storage_path, checksum_sha256 FROM messages
    WHERE expunged_at IS NULL AND checksum_sha256 IS NOT NULL" |
while IFS='|' read -r p want; do
    got=$(sha256sum "/var/lib/ferroma/mail/$p" | cut -d' ' -f1)
    [ "$got" = "$want" ] || echo "CORRUPT: $p (want $want, got $got)"
done
```

### 9.4 Checking the counters

`folders.message_count`, `unseen_count` and `total_bytes`, and `users.used_bytes`,
are caches. They are correctable from the data they summarise:

```sql
-- What the counters should be, per folder.
SELECT f.id, f.name, f.message_count,
       COUNT(m.id) FILTER (WHERE m.expunged_at IS NULL) AS actual,
       COUNT(m.id) FILTER (WHERE m.expunged_at IS NULL
                            AND m.flags NOT LIKE '%seen%') AS actual_unseen,
       COALESCE(SUM(m.size_bytes) FILTER (WHERE m.expunged_at IS NULL), 0) AS actual_bytes
  FROM folders f
  LEFT JOIN messages m ON m.folder_id = f.id
 GROUP BY f.id, f.name, f.message_count
HAVING f.message_count <> COUNT(m.id) FILTER (WHERE m.expunged_at IS NULL)
 ORDER BY f.id;
```

`FoldersRepository::recount(folder_id)` is the repair: it recomputes all three
counters and returns the updated `Folder`. `MailboxesRepository::recompute_usage`
does the same for `users.used_bytes`. Move, copy, a hard delete and a change to
`\Seen` also shift the three folder columns in the same transaction as the row,
so a folder does not keep counting mail that has already left. A folder that was
already behind is still repaired by the next `recount`, and by opening the folder
list: `GET /api/v1/mailboxes/:id/folders` recomputes each folder before it answers.

A wrong `message_count` is what the folder list shows until that recount runs.
A wrong `uid_next` is not: it is the UID allocator, and `MessagesRepository::max_uid`
(`SELECT COALESCE(MAX(uid), 0) FROM messages WHERE folder_id = $1`) is the value it
must be at least. If `uid_next` is ever behind `max_uid`, the next delivery
allocates a UID that already exists and the unique index on `(folder_id, uid)`
rejects it — which is a loud failure, not a silent overwrite, because of
`messages_folder_uid_key`.

### 9.5 What to do about each finding

| Finding | Action |
|---|---|
| Missing body | restore from the backup that contains it, or hard-delete the row and log it. There is no way to reconstruct RFC 5322 bytes from metadata |
| Orphan file | leave it, or move it to a quarantine directory. Do not delete before confirming the restore was complete |
| Checksum mismatch | the file changed on disk. Restore it; do not "fix" the database |
| Counter drift | `FoldersRepository::recount` / `MailboxesRepository::recompute_usage` |
| `uid_next` behind `max_uid` | set `uid_next = max_uid + 1` **and bump `uid_validity`** — the UID space has been tampered with ([imap.md](imap.md) §5.3) |
| Unreferenced blob | `AttachmentStore::gc` with a freshly collected keep-set |
| Leftover `tmp/` file | `Maildir::sweep_tmp(3600)` on a live server, `sweep_tmp(0)` on a stopped one |

`ferroma storage verify` covers the missing-body and size findings in the table
above and sweeps abandoned `tmp/` files older than an hour; it does not enumerate
the orphan files of §9.2. `POST /api/v1/storage/gc` runs the blob collector and
sweeps the `tmp/` areas of both the blob and Maildir roots. The remaining checks —
orphans, checksums, counter drift and `uid_next` — are still the `psql` and shell
snippets in this section.

---

## 10. Configuration that shapes storage

| Key | Default | Effect |
|---|---|---|
| `server.data_dir` | `./data` | base for both roots |
| `storage.maildir_root` | `<data_dir>/mail` | Maildir root |
| `storage.attachment_root` | `<data_dir>/attachments` | blob root |
| `storage.fsync_on_write` | `true` | `fsync` the message and its directory entry before acknowledging `DATA` |
| `storage.layout` | `"maildir"` | `"maildir"` (Maildir++ dotted folders) or `"maildirperfolder"` |
| `storage.checksum` | `true` | store the SHA-256 of every message and attachment |
| `storage.enforce_quota` | `true` | refuse writes past the quota |
| `storage.soft_delete` | `true` | move to `Trash` instead of unlinking immediately |
| `database.url` | `postgres://ferroma:ferroma@localhost:5432/ferroma` | connection string |
| `database.max_connections` / `min_connections` | 20 / 2 | pool bounds |
| `database.run_migrations` | `true` | apply `migrations/*.sql` at startup |
| `database.log_statements` | `false` | **never enable in production — it prints message subjects** |
| `limits.mailbox_quota` | 1073741824 | default quota for a new user |

`Config::validate()` refuses to boot if `storage.maildir_root` is set to an empty
path, if `database.url` is not a `postgres://`/`postgresql://` URL, or if
`database.min_connections > database.max_connections`.

---

## 11. Related documents

| Topic | Document |
|---|---|
| Why `mailboxes` and `folders` are separate, and `rfc_message_id` | [architecture.md](architecture.md) §9 |
| Maildir++ folder naming, UID/UIDVALIDITY, flag mapping | [imap.md](imap.md) §4, §5, §6 |
| The change log, cursors, tombstones | [sync.md](sync.md) |
| Quota replies over SMTP, retry scheduling, bounce | [smtp.md](smtp.md) §7, §11 |
| Backup commands, DNS, TLS, restore drill | [deployment.md](deployment.md) §8 |
| What is never logged, secrets handling | [security.md](security.md) §10 |
