# Synchronisation

**Who should read this:** anyone implementing or debugging the sync path — the
server side in `ferroma-sync` / `ferroma-storage::repository::sync`, or the
official client, which lives in a separate repository.

This document explains how a Ferroma client stays in step with the server: why
FCP exists next to IMAP, what "the server is the source of truth" constrains, how
`change_log.seq` becomes the cursor a client stores, the apply-then-advance
contract that makes replay safe, how tombstones let a client learn about
deletions it missed, what to do when a cursor is too old, why `uid_validity`
invalidates a folder cache, how `operations` makes a retried mutation harmless,
how conflicts are resolved, what the offline Outbox is, and what happens on each
class of failure. The **wire format** — request and response shapes, headers,
WebSocket framing — is frozen in [fcp.md](fcp.md) and is not repeated here.

> **Status:** implemented. The server-side primitives are `change_log`,
> `client_sync_states` and `operations` in `migrations/0001_initial.sql` and
> `ChangeLogRepository`, `SyncStatesRepository`, `OperationsRepository` and
> `OperationOutcome` in `crates/ferroma-storage/src/repository/sync.rs`;
> `SyncService` in `crates/ferroma-sync/src/service.rs` turns them into
> `GET /api/v1/client/sync` and the idempotent `with_operation` wrapper.

---

## 1. Why FCP exists alongside IMAP

IMAP already synchronises mail. Specification §20 and §53 are explicit that FCP
does not replace it, and [fcp.md](fcp.md) §1 states the split:

| Requirement | IMAP | FCP |
|---|---|---|
| Fetch a folder the first time | `SELECT` + `FETCH 1:*` — fine | `GET /sync?cursor=0`, paged |
| Fetch only what changed since last time | needs `CONDSTORE`/`QRESYNC` or a full `FETCH`, and `EXPUNGE`-based diffing that most servers implement differently | one cursor, `seq > last` |
| Know that a message was **deleted** while offline | `EXPUNGE` responses are session-scoped; a client that was not connected never sees them | `message_deleted` tombstone in the change stream |
| Server-side drafts shared across devices | `APPEND` to `Drafts` plus guesswork | `drafts` rows, mirrored to `Drafts` ([fcp.md](fcp.md) §7) |
| Device inventory and remote revocation | not expressible | `devices` + `device.revoked` |
| Chunked, resumable attachment upload | not expressible | `/attachments/init`, `/chunk`, `/complete` ([fcp.md](fcp.md) §6) |
| Realtime push | `IDLE`, one folder per connection, re-established every 29 min ([imap.md](imap.md) §9) | one WebSocket for the whole account |
| Idempotent mutation replay | `UID` makes some operations repeatable, not all | `operation_id` on every mutation |

The last row is the one that makes the difference for a desktop client. IMAP has
no way to say "I already asked you to mark 4821 read and my request timed out —
tell me what happened", so a client that retries either double-applies or has to
re-derive state with a `FETCH`. FCP's `operations` table answers that question
exactly.

The cost of running both is real: every mutation has to be visible through both
surfaces, which is why the layering rule in [architecture.md](architecture.md) §3
exists. A flag change made over IMAP must produce the same `change_log` row as one
made over FCP, or the two views diverge. The IMAP listener records message
creation (APPEND/COPY), updates (STORE and implicit `\\Seen` on FETCH), moves,
and expunge tombstones through the same `SyncService` as the HTTP entry points.
Folder CREATE/DELETE/RENAME/SUBSCRIBE changes are recorded as well. The event
bus is an in-memory push channel, **not** the durable cursor journal.

IMAP and HTTP COPY/MOVE now share `ferroma-storage::relocate_message`: it
stages the destination Maildir body before changing the row, updates folder/UID
and path in one SQL transaction, and removes the old file only after a successful
MOVE. That transaction also inserts the FCP change entry; COPY checks quota and
repairs cached usage within it, including under concurrent requests. A failed
body write, database update or change-log insert keeps the source intact;
failure-injection tests guard those cases. IMAP and REST folder renames now share a
folder-tree coordinator: it stages destination directories while old bodies
remain readable, then updates folder names, descendants, message paths and
folder cursor entries in one SQL transaction. A failed SQL commit removes the
staged directories. Concurrent mailbox writers and renames still require
serialization; without it a file inserted after the staging snapshot can be
missed.

Maildir and PostgreSQL cannot share a crash-atomic transaction. A crash after
staging but before SQL commit can leave an orphan file; run `ferroma storage
verify` after an interrupted write. IMAP STORE/FETCH `\\Seen` and HTTP flag
updates commit their row, path and cursor entry together; IMAP EXPUNGE/CLOSE
commits each row deletion with its cursor tombstone. IMAP APPEND and IMAP
CREATE/DELETE/RENAME/SUBSCRIBE also commit their row, usage where applicable,
and cursor entry together. Other protocol paths may still record changes after
their database writes; a crash there can leave a missing cursor entry. Those
paths need the same treatment before all cross-protocol mutations can claim
crash atomicity.

---

## 2. `Server = Source of Truth`

Specification §26 and §55 both state the rule:

```text
Server = Source of Truth
Client SQLite = Local Cache
```

What it constrains, concretely:

| The client may | The client must not |
|---|---|
| Show mail, folders, drafts and flags from its cache while offline | Present cached state as authoritative when the server disagrees |
| Queue mutations locally and apply them optimistically to the cache | Send a mutation that the server has not accepted and treat it as done |
| Keep a message body after the server deleted the message | Keep serving a deleted message after it has seen the tombstone |
| Choose its own cache eviction policy | Decide that a message does not exist because its cache was evicted |
| Hold a cursor and trust it | Invent state the server never sent |

Three consequences that show up in the code:

1. **The client never writes a UID it did not receive.** UIDs come from
   `messages.uid`; a client that assigns its own will disagree with the server
   after any resync.
2. **A local cache miss is not a deletion.** The client fetches the body
   (`GET /api/v1/client/messages/:id`) rather than concluding the message is gone.
   Only a tombstone deletes.
3. **Conflicts resolve server-ward, except for drafts.** §9.

---

## 3. `change_log` is the cursor source

### 3.1 The table

```sql
CREATE TABLE change_log (
    seq        BIGSERIAL   PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mailbox_id BIGINT      REFERENCES mailboxes(id) ON DELETE CASCADE,
    folder_id  BIGINT,
    message_id BIGINT,
    kind       TEXT        NOT NULL,
    payload    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Four indexes: `change_log_cursor_idx (user_id, seq)` — the sync query;
`change_log_mailbox_idx (mailbox_id, seq)` — per-address sync;
`change_log_folder_idx (folder_id, seq)` — per-folder sync;
`change_log_created_idx (created_at)` — retention pruning.

`message_id` and `folder_id` are **not** foreign keys. The schema says why in a
comment: *"tombstones must outlive rows"*. Those two columns are the ones a
`*_deleted` change names, so a constraint on either would break the journal exactly
when it matters — a `folder_deleted` entry is written *after* the folder row is gone,
and before `migrations/0003_change_log_folder_tombstones.sql` that insert answered
`500` with `change_log_folder_id_fkey`. `mailbox_id` keeps its cascade: no
`mailbox_deleted` kind exists, so nothing writes a mailbox tombstone. This is the
single most important schema decision in the sync design and §5 explains it.

### 3.2 Why `seq` is the cursor

A cursor has to be four things: monotonic, comparable, cheap to store, and opaque
to the client.

| Property | How `seq` satisfies it |
|---|---|
| Monotonic | `BIGSERIAL`, reallocated after a per-user transaction lock in migration 0007 so one user's visible cursors follow commit order; gaps remain possible |
| Comparable | `seq > $2` is the entire query |
| Cheap | one `BIGINT` per (device, mailbox, folder) in `client_sync_states` |
| Opaque | the client treats it as a string and never parses it ([fcp.md](fcp.md) §3) |

The last one is deliberate: `ferroma_core::Cursor` is a newtype over `i64` today,
and the API contract says a client must not depend on that. If the mechanism ever
has to change — a per-user sequence, a timestamp-plus-id, a vector clock for
multi-node — clients that treated it as opaque keep working.

**`seq` is global per user, not per folder.** A client syncing five folders can
hold five cursors and still see a consistent relative order across them, because
all five values come from one sequence. That is what makes merging a
`message_moved` that arrives in the target folder's stream with a
`message_deleted` from the source folder's stream safe in either order: both are
keyed by `message_id`. [fcp.md](fcp.md) §3 states the same guarantee.

### 3.3 The query

```rust
// crates/ferroma-storage/src/repository/sync.rs
pub async fn changes_since(
    &self,
    user_id: UserId,
    after: Cursor,
    limit: i64,
) -> Result<Vec<ChangeLogEntry>> {
    // SELECT * FROM change_log
    //  WHERE user_id = $1 AND seq > $2
    //  ORDER BY seq ASC LIMIT $3
}
```

`after` is **exclusive**: passing the `seq` of the last entry the client already
has never repeats it. `changes_since_in_mailbox` is the same query with
`AND mailbox_id = $2`.

`limit` is `client.sync_page_size` (500), and `limit_of(limit)` clamps it so a
client cannot ask for a million changes. A client that fills the page asks again
with the new last `seq` — the page boundary is not a transaction boundary, and it
does not need to be, because every change is independently applicable.

`max_seq(user_id)` returns `COALESCE(MAX(seq), 0)`, which is what a
`has_more: false` response and the WebSocket `hello` frame report as `last_seq`.

### 3.4 `client_sync_states`

```sql
CREATE TABLE client_sync_states (
    id         BIGSERIAL PRIMARY KEY,
    device_id  BIGINT NOT NULL REFERENCES devices(id)   ON DELETE CASCADE,
    mailbox_id BIGINT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    folder_id  BIGINT REFERENCES folders(id) ON DELETE CASCADE,   -- NULL = account level
    cursor     BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX client_sync_states_key
    ON client_sync_states (device_id, mailbox_id, COALESCE(folder_id, 0));
```

The row is a server-side record of how far each *device* has read. It is not what
the client uses to decide what to fetch — the client stores its own cursor and
sends it. The server-side row exists so that:

* the Admin device view can show sync progress,
* a device that loses its local state can be reset with
  `SyncStatesRepository::reset_for_device`, and
* the retention sweep for `change_log` knows whether it is safe to prune (§6).

`SyncStatesRepository::get` returns `0` for a missing row — "nothing read yet" and
"never synced" are the same state.

The `COALESCE(folder_id, 0)` in the unique index is load-bearing twice over: it is
the conflict target of the upsert in `SyncStatesRepository::set`, and without it
PostgreSQL's NULL-distinctness would let one device accumulate unlimited
account-level rows. SQLite's equivalent on the client is a `UNIQUE` index over
`(account_id, folder_id)` with a sentinel `0` for the account level.

---

## 4. The apply-then-advance contract

From [fcp.md](fcp.md) §3, restated as a client obligation with the reasoning:

```text
loop:
    page = GET /sync?mailbox_id=A&folder_id=F&cursor=<stored>&limit=500
    BEGIN LOCAL TRANSACTION
        for change in page.changes:      # in order
            apply(change)                # insert / update / delete locally
        store_cursor(page.next_cursor)   # in the same transaction
    COMMIT
    if not page.has_more: break
```

The ordering inside the transaction is the whole point:

| Crash point | Result |
|---|---|
| Before `COMMIT` | nothing was applied and the cursor did not move. The next sync fetches the same page |
| After `COMMIT`, before the socket closes | the page was applied and the cursor moved. The next sync fetches the *following* page |
| During `apply`, mid-page | the transaction rolls back, so no partial page is visible and the cursor did not move |

Therefore **every change handler must be idempotent**, because a page can
legitimately be delivered twice. Concretely:

| Change | Idempotent implementation |
|---|---|
| `message_created` | `INSERT … ON CONFLICT (server_id) DO UPDATE` |
| `message_updated` | set the flags to the absolute value in the payload, never "toggle" |
| `message_deleted` | `DELETE WHERE server_id = ?` — deleting nothing is fine |
| `message_moved` | move to the named folder; a no-op if it is already there |
| `folder_created` | `INSERT … ON CONFLICT (name) DO NOTHING` |

The rule that falls out: **changes carry absolute state, never deltas.** A
`message_updated` payload carries the resulting flag string, not "toggle
`\Seen`", precisely so that replaying it is harmless. This is why
`Event::mail_flag_changed` carries `flags: String` (the full database flag
string) rather than a diff.

A client that applies a page and *then* stores the cursor outside a transaction
has a window in which it has applied changes but not recorded them; after a crash
it re-fetches the page, which is fine because the handlers are idempotent. The
transaction is an optimisation, not a correctness requirement — which is the
property the contract is designed to have.

**Never advance the cursor past a change you did not apply.** A skipped change is
permanently invisible: `seq > cursor` will never return it again. If the client
cannot apply a change (a malformed payload, a local constraint violation), the
correct response is to stop, report it, and resync the folder from `0` — not to
skip and advance.

---

## 5. Tombstones

### 5.1 The problem

A client that was offline for two days reconnects and syncs. It has 400 messages
in its cache for a folder. The server has 395. Which five are gone?

It cannot answer by comparing, because it does not know whether a locally present
message is missing because it was deleted or because the client's cache is
partial. It cannot answer from `messages`, because the rows are gone.

### 5.2 The mechanism

Every deletion appends a `message_deleted` change to `change_log`, and that row
**stays** after the `messages` row is removed. That is what `change_log.message_id`
having no foreign key buys:

```text
   messages                    change_log
   ────────                    ──────────
   id 4821  "Invoice"          seq 1836  kind message_created  message_id 4821
   …                           seq 1839  kind message_moved    message_id 4821
                               seq 1841  kind message_deleted  message_id 4821
   (row deleted)                     ▲
                                     └─ still here, forever (or until retention)
```

The payload carries what the client needs to apply it without another round trip:

```json
{ "type": "message_deleted", "seq": 1841, "message_id": 4821, "folder_id": 5, "permanent": false }
```

`permanent: false` means "moved to Trash"; `permanent: true` means the row is
gone. The distinction matters because the client should keep the body for the
first and may drop it for the second.

Two deletions produce tombstones, matching `messages.deleted_at` and
`messages.expunged_at` ([storage.md](storage.md) §2.3):

| Event | Change `kind` | Tombstone? |
|---|---|---|
| `\Deleted` set / `DELETE` without `permanent` | `message_updated` (flags) + `message_moved` if it went to Trash | no — the message still exists |
| Expunge / `DELETE ?permanent=true` | `message_deleted` | **yes** |

### 5.3 Retention

Tombstones do not live forever. `client.tombstone_retention_days` (30) is what
`operations.created_at` and `change_log.created_at` are compared against by the
retention sweep, and [fcp.md](fcp.md) §1 documents the same value for idempotency
keys.

```rust
// ChangeLogRepository / OperationsRepository
pub async fn prune_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64>
pub async fn purge_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64>
```

`ChangeLogRepository::prune_older_than` carries a warning in its documentation
that the sweep is responsible for knowing when it is safe: **only prune changes
that every device is already past.** Pruning a change a device has not read turns
that device's next sync into a gap, and a gap is treated as corruption (§7). The
safe cutoff is the minimum cursor across all `client_sync_states` rows for that
user, floored by the retention window.

Thirty days is the design target: a desktop client that has been switched off for
a month comes back to a resync rather than a silent hole.

---

## 6. Cursor-too-old recovery

A cursor is too old when the change it points at has been pruned, or when the
user's `change_log` no longer contains anything between the cursor and the
present. There is no way to reconstruct the missing delta, and the server must
not pretend otherwise.

The contract ([fcp.md](fcp.md) §3, item 6):

```http
GET /api/v1/client/sync?mailbox_id=3&folder_id=5&cursor=9
```

```http
HTTP/1.1 409 Conflict
Content-Type: application/json

{ "error": { "code": "conflict", "message": "cursor too old; full resync required" } }
```

The client's response:

```text
1. stop the incremental sync for that folder
2. mark the folder's cache invalid            (drop message rows for the folder)
3. set the folder's cursor to 0
4. sync from 0                              (first sync, §3)
5. show sync progress using folders.message_count from GET /client/mailboxes
```

`409 conflict` is the same status the API uses for a replayed `operation_id`
([api.md](api.md) §1.3), so a client must distinguish by context: a `409` from
`/sync` means resync, anything else means surface it.

### 6.1 Gaps

`seq` is gap-free per user under normal operation, but there is one legitimate
source of gaps: **pruning**. A client that sees a gap — `1836` followed by
`1851` — must not try to reason about the missing range. [fcp.md](fcp.md) §3
item 3 states the rule: discard the cursor and resync from `0`.

Why not ask for the gap explicitly? Because the server may have pruned precisely
those rows, so the request would fail again, and because a client that has fallen
that far behind needs a resync anyway. The check is cheap:

```text
if any(change.seq != previous_seq + 1 for consecutive changes): full resync
```

A client that does not check will silently miss the changes in the gap; the only
reason the check is not mandatory is that the cursor-too-old `409` catches the
common case.

---

## 7. `uid_validity` invalidation

UIDs are folder-scoped and opaque ([imap.md](imap.md) §5). `folders.uid_validity`
identifies the *generation* of a folder's UID space, and a client that caches a
UID → message mapping must key that cache by `(folder_id, uid_validity)`, not by
`folder_id` alone.

```text
   cached: folder 5, uid_validity 1, uid 117 -> server message_id 4821
   server: SELECT returns "OK [UIDVALIDITY 2]"
        ⇒ every cached UID for folder 5 is meaningless
        ⇒ drop the folder's UID index (message bodies keyed by message_id may stay)
        ⇒ cursor = 0, resync
```

`GET /api/v1/client/mailboxes` returns `uid_validity` per folder
([fcp.md](fcp.md) §4), so a client can detect the change before it even selects
the folder.

| Event | `uid_validity` | Client action |
|---|---|---|
| Normal operation | unchanged | nothing |
| Folder renamed | unchanged | nothing — same UID space |
| Messages expunged | unchanged | nothing |
| Message moved in or out | unchanged (the destination folder's `uid_next` advances) | apply the `message_moved` change |
| A rebuild after data loss | bumped | drop the folder's cache, resync |
| `ferroma storage verify --repair` renumbers a folder | bumped | drop the folder's cache, resync |
| Restore from a backup older than a rebuild | bumped | drop the folder's cache, resync |

The client rule is the same as the IMAP one and for the same reason: a UID is only
meaningful together with the `uid_validity` under which it was issued. A client
that omits this gets the classic IMAP bug — opening the wrong message after a
server-side rebuild — and it is silent, which is what makes it worth stating
twice.

**Server-side:** `FoldersRepository::set_uid_validity` must be called whenever an
operation could make an old UID → message mapping wrong, and never otherwise. A
spurious bump costs every client a folder resync.

---

## 8. Idempotency through `operations`

### 8.1 The rule

Every mutating client request carries a client-generated `operation_id`
(specification §55). The server records the operation and replays the original
response if it sees the id again.

From [fcp.md](fcp.md) §5:

```json
{ "operation_id": "op_9f2c41…", "type": "mark_read", "message_id": 4821 }
```

> Clients should generate ids before enqueuing locally, never at send time.

That last sentence is the whole design. An id generated at send time is a
different id on every retry, so it protects nothing. An id generated when the
user acted — and stored with the pending operation — is stable across every
retry, every reconnect and every process restart.

`ferroma_core::OperationId::generate()` produces `op_` plus a UUID v4 with
hyphens removed.

### 8.2 The mechanism

```rust
// crates/ferroma-storage/src/repository/sync.rs
pub enum OperationOutcome {
    Fresh,          // new id: the caller owns the work
    Replay(Operation),  // seen before: the cached row is returned
}

pub async fn begin(&self, operation_id: &str, user_id: Option<UserId>, kind: &str)
    -> Result<OperationOutcome>
```

`begin` is one statement:

```sql
INSERT INTO operations (operation_id, user_id, kind, status)
VALUES ($1, $2, $3, 'applied')
ON CONFLICT (operation_id) DO NOTHING
RETURNING *
```

The atomicity is the point: of two concurrent retries of the same request exactly
one gets `Fresh` and the other gets `Replay`. There is no read-then-write window
in which both could believe they are first. If `RETURNING *` yields nothing, the
row exists and `find(operation_id)` reads it back; the code retries that read
three times before giving up with
`StorageError::Conflict("operation … was claimed but disappeared")`, which can
only happen if a purge ran between the two statements.

A replay is scoped to its owner and kind: the stored `user_id` and `kind` must
match the retry, or `begin` refuses with
`StorageError::Conflict("operation_id is already used by another user or
operation")`. A cross-user collision then fails loudly instead of replaying
another account's cached response, and the same user reusing a key for a
different mutation fails instead of swallowing the second mutation as a replay.

The full server-side sequence for a mutating request:

```text
1. OperationsRepository::begin(op_id, user_id, kind)
       Fresh  -> 2
       Replay -> return operation.result verbatim, with the original status
2. do the work (repository call, mail core call, Maildir write)
3. success: OperationsRepository::complete(op_id, result_json)
   failure: OperationsRepository::fail(op_id, error_json)
4. append the change_log row, publish the event
```

`complete` and `fail` both store into `operations.result`, and both set
`completed_at`. A failed operation is cached too: replaying it returns the same
error rather than re-running a call that will fail the same way.

### 8.3 Where the id comes from in HTTP

| Surface | Carrier |
|---|---|
| Client API (FCP) | `operation_id` in the request body ([fcp.md](fcp.md) §5) |
| Management API | `Idempotency-Key` header ([api.md](api.md) §1.5) |

Both end up in the same column. The header form exists because the browser
Webmail client cannot always put a field in a body (a `DELETE` has none).

### 8.4 What idempotency does not cover

* **Non-mutating requests do not need an id**, and a `GET` with one is ignored.
* **The id is not a lock.** Two *different* ids for the same logical action both
  execute; idempotency protects against retries, not against a user clicking
  twice. The client's job is to not generate two ids for one user action.
* **The window is `client.tombstone_retention_days`** (30). An operation id older
  than that has been purged (`OperationsRepository::purge_older_than`) and will be
  treated as fresh. A client retrying a month-old request is not retrying, it is
  re-issuing, and re-issuing is what the user asked for.
* **Operations are per user, and that is enforced.** `operations.user_id` and
  `kind` are part of the claim: a replay whose owner or operation kind differs
  is a `409 conflict`, not a replay. The primary key is still the id string
  alone, so a client should namespace its ids per account — `op_<uuid4>` does —
  and a collision now fails loudly instead of answering with someone else's
  cached result.

---

## 9. Conflict resolution

### 9.1 Flags and folders: last write wins, server clock

Two devices mark the same message read and unread at the same time. The server
applies whichever request arrived last; the other device learns the result from
its next sync, because every mutation appends a change. There is no merge and no
vector clock, and that is the right answer for a boolean flag: the user's most
recent intent is the only thing that matters, and a "conflict" is not worth
telling them about.

The client's obligation is to **not fight the server**: having applied an
optimistic local change, it must not re-send it when the server's state disagrees
— the pending operation has already been acknowledged, so it is removed from the
queue, and the incoming change wins.

### 9.2 Drafts: last write wins, and the client is told

Drafts are the one place a bad merge loses user-typed text, so the policy is
explicit ([fcp.md](fcp.md) §7):

```json
{ "id": 44, "updated_at": "2026-09-16T12:00:01Z",
  "conflict": { "detected": true, "server_updated_at": "2026-09-16T11:59:58Z" } }
```

| Situation | Behaviour |
|---|---|
| Draft edited on one device, no other edit | normal update |
| Draft edited on two devices, second edit arrives with an older `updated_at` than the stored row | the stored row wins, and the response says so |
| The client sees `conflict.detected` | it must surface it: keep the user's version locally as a copy, or prompt. It must not silently retry |
| Draft deleted on the server, edited on the client | the `PATCH` is `404 not_found`; the client should offer to re-create from its local copy |

Last-write-wins is chosen over a merge because two concurrent edits of a rich-text
body have no correct automatic merge. The `conflict` object is what makes it
acceptable: the losing writer is not silently discarded, it is informed.

Detection uses `updated_at` from the row, so the comparison is server-clock to
server-clock and no device's clock skew is involved.

### 9.3 Sending

A message sent while offline goes to the Outbox and is sent when the network
returns. If the same draft was also sent from another device, both are sent —
the server has no way to know they are "the same" message, and suppressing one
would be worse than a duplicate. The `operation_id` prevents a *retry* of one
send from producing two, which is the failure that actually happens.

### 9.4 What is never merged

| Entity | Policy |
|---|---|
| Message existence | server wins absolutely. A tombstone deletes |
| Message flags | last write wins |
| Folder membership | last write wins; a `message_moved` in either direction is applied |
| Folder existence and name | server wins; `folder_deleted` removes it locally |
| Draft content | last write wins, with the conflict reported |
| Account settings | server wins, resynced from `GET /client/account` |
| Local-only settings (window size, theme, cache budget) | never synchronised |

---

## 10. The offline Outbox

Specification §27 and §28. The Outbox holds *pending operations*, not pending
messages — a send is just one kind of operation.

```text
   Compose ──► local Outbox row (state = draft or pending)
                    │
                    │ network available
                    ▼
              Sync Engine ──► FCP request with operation_id
                    │
                    ├── 2xx  ──► remove the row (or mark sent)
                    ├── 4xx  ──► mark failed, keep the row, surface it
                    └── 5xx / offline ──► retry with backoff, keep the row
```

The **eight states** of specification §28:

```text
Draft   Pending   Uploading   Queued   Sending   Sent   Failed   Retrying
```

The two vocabularies — the client's Outbox states and the server's
`mail_queue.status` — are related but not identical, because "Uploading" and
"Retrying" are client-side facts:

| Client state | Meaning | Server-side counterpart |
|---|---|---|
| `Draft` | composed, not yet queued for sending | none (a `drafts` row, if saved) |
| `Pending` | queued locally, waiting for the sync engine to pick it up | none |
| `Uploading` | attachments are being transferred (chunked) | `/attachments/chunk` progress |
| `Queued` | server accepted it: `POST /client/messages` returned `{message_id, queued}` | `mail_queue.status = 'pending'`, one row per recipient |
| `Sending` | the server is delivering to the remote MX | `mail_queue.status = 'delivering'` |
| `Sent` | every recipient was delivered | `mail_queue.status = 'delivered'` |
| `Retrying` | a delivery attempt failed temporarily; the server will try again | `mail_queue.status = 'retry'`, `next_attempt_at` |
| `Failed` | permanent failure, or attempts exhausted | `mail_queue.status = 'failed'` |

The transitions the client drives versus the ones the server drives:

```text
   client-driven:  Draft → Pending → Uploading → Queued
                   Pending/Uploading → Failed        (local validation, 413)
                   Retrying → Sending → Sent         (after a sync)

   server-driven:  Queued → Sending → Sent
                   Queued/Sending → Retrying → Sending → …
                   Queued/Sending/Retrying → Failed
```

The client learns about the server-driven transitions from two sources, and must
treat them as equivalent:

1. **Sync changes** — `delivery.updated` in the change stream, with
   `queue_id`, `recipient`, `status`, `attempts`, `last_error`
   (`DeliveryUpdated` in `crates/ferroma-events/src/event.rs`).
2. **Realtime** — the same event over the WebSocket ([fcp.md](fcp.md) §8).

Because the socket is an optimisation and the cursor is the source of truth, a
client that missed a `delivery.updated` frame still finds out on its next sync.
A client that only listened to the socket would show a message as `Sending`
forever.

Outbox rules:

* **A pending operation is only removed when the server acknowledges it**, with a
  `2xx` or a replayed cached response for its `operation_id`. A timeout is never
  an acknowledgement.
* **Nothing the user typed may be lost to a network error** ([fcp.md](fcp.md)
  §11). Attachments are uploaded before the send, and their ids are stored in the
  pending row so a restart resumes rather than re-uploads.
* **Backoff is exponential with jitter** on `5xx` and network errors, capped by
  the client's own policy. The server's `Retry-After` wins when present.
* **A `4xx` is terminal for that operation** (except `429`, which is a throttle).
  `413 limit_exceeded` means the message is too big and retrying will not help —
  the client keeps the draft and tells the user.
* **Ordering is not guaranteed.** Two queued operations may be applied out of
  order after a retry. Every operation is written to be independently applicable,
  which is why changes carry absolute state (§4).

---

## 11. The failure matrix

What happens to the client and to the queued work, per error class. The statuses
and codes are the ones in [api.md](api.md) §1.3 and [fcp.md](fcp.md) §11.

| Class | Server response | Client must | Outbox | Cursor |
|---|---|---|---|---|
| **Offline / DNS failure / connection reset** | none | queue locally, retry with backoff + jitter, never discard | retained | unchanged |
| **`401 unauthorized`** | `401` | refresh the access token once, retry once. A second `401` ⇒ log out, keep the cache and the Outbox | retained across logout | unchanged |
| **`403 forbidden`** | `403` | surface it; do not retry | marked failed | unchanged |
| **`404 not_found`** | `404` | the target is gone. Drop the operation, apply the server's state on the next sync | removed | unchanged |
| **`409 conflict` from `/sync`** | `409` | cursor too old ⇒ discard the folder cache, sync from `0` (§6) | unaffected | reset to `0` |
| **`409 conflict` from a mutation** | `409` | a replayed `operation_id` (the cached response should have been replayed instead) or a state violation ⇒ surface it | marked failed | unchanged |
| **`413 limit_exceeded`** | `413` | tell the user what is too big; keep the draft | removed from the Outbox, draft retained | unchanged |
| **`426 unsupported`** | `426` | refuse to run; prompt for an upgrade. A client below `client.min_protocol_version` cannot be served | frozen | frozen |
| **`429 rate_limited`** | `429` + `Retry-After` | honour `Retry-After` exactly; queue locally | retained | unchanged |
| **`500 storage_error` / `internal_error`** | `500` | treat as temporary; back off; never lose the operation | retained | unchanged |
| **`502 dns_error` / `network_error` / `timeout`** | `502` | treat as temporary; back off | retained | unchanged |
| **WebSocket disconnect** | — | reconnect, then **sync from the stored cursor before trusting the socket** | unaffected | unchanged |
| **`{"replay_gap": true}`** | WS frame | the bus dropped events for this subscriber ⇒ run a sync immediately | unaffected | unchanged |
| **`device.revoked` / `401` after revocation** | WS frame + `401` | clear tokens, stop syncing, keep the cache, show "this device was signed out" | frozen | frozen |
| **App killed mid-send** | — | on restart, resume from the Outbox. Attachments already uploaded are referenced by id, so they are not re-sent | resumed | unchanged |
| **App killed mid-sync** | — | the page transaction either committed (cursor advanced) or did not (page re-fetched). Idempotent handlers make both safe | unaffected | consistent |
| **Gap detected in `seq`** | — | discard the cursor, resync from `0` (§6.1) | unaffected | reset to `0` |
| **`uid_validity` changed** | — | discard the folder's UID index, resync the folder (§7) | unaffected | reset to `0` for that folder |
| **Clock skew between devices** | — | irrelevant: conflicts are resolved on server clocks (§9.2) | unaffected | unchanged |

The row that matters most is the first: **nothing the user typed may be lost to a
network error.** Every other policy in this table is subordinate to it, and it is
why pending operations and drafts live in the client's SQLite database until the
server acknowledges them, and why an idempotency key is generated when the user
acts rather than when the request is sent.

---

## 12. Related documents

| Topic | Document |
|---|---|
| The FCP wire format: endpoints, cursors, framing, chunked upload | [fcp.md](fcp.md) |
| The HTTP management surface | [api.md](api.md) |
| Event names, scopes, replay, the in-process limitation | [architecture.md](architecture.md) §6 |
| Schema: `change_log`, `operations`, `client_sync_states` | [storage.md](storage.md) §2.7 |
| UID and UIDVALIDITY semantics on the IMAP side | [imap.md](imap.md) §5 |
| Queue states, retry schedule, bounce | [smtp.md](smtp.md) §11 |
