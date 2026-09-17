# Ferroma Client Protocol (FCP) v1

FCP is the contract between Ferroma Server and the official Ferroma clients.

It does **not** replace IMAP or SMTP. Third-party clients — Thunderbird, Apple Mail,
Outlook, phones — keep speaking IMAP and SMTP and are first-class citizens
(specification §53). FCP exists because those protocols cannot express the things an
official client needs: incremental sync with a resumable cursor, server-side drafts,
device management, chunked attachment transfer and realtime push.

```text
                    Ferroma
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
     Client API       IMAP         SMTP
          │            │            │
          ▼            ▼            ▼
   Official client  Thunderbird   Outlook
                    Apple Mail
```

* Base path: `/api/v1/client`
* Transport: HTTPS (or plain HTTP behind a trusted proxy during development)
* Media type: `application/json; charset=utf-8`
* Realtime: `GET /api/v1/client/events` upgraded to WebSocket
* Reference implementation: `ferroma-api::client` (server), `ferroma-client::api` (client)

---

## 1. Version negotiation

A client announces itself on every request:

```http
X-Ferroma-Client: FerromaClient/0.7.0
X-Ferroma-Protocol: 1
X-Ferroma-Platform: windows
```

The server replies with the protocol it used:

```http
X-Ferroma-Protocol: 1
X-Ferroma-Server: 0.1.0
```

| Situation | Server behaviour |
|---|---|
| `X-Ferroma-Protocol` ≥ `client.min_protocol_version` | normal operation |
| below the minimum | `426 Upgrade Required`, body `{"error":{"code":"unsupported",…}}` |
| header absent | treated as protocol `1` (a courtesy for `curl` and monitoring) |
| higher than `client.protocol_version` | served, with `X-Ferroma-Protocol` set to the server's version; the client must degrade gracefully |

`GET /api/v1/client/account` returns the negotiated values so a client can record
them:

```json
{
  "user": { "id": 7, "email": "alice@example.com", "display_name": "Alice" },
  "mailboxes": [ { "id": 3, "address": "alice@example.com", "is_primary": true } ],
  "protocol_version": 1,
  "min_protocol_version": 1,
  "server_version": "0.1.0",
  "server_hostname": "mail.example.com",
  "limits": { "max_message_size": 26214400, "max_recipients": 100, "attachment_chunk_size": 1048576, "sync_page_size": 500 },
  "features": ["sync", "events", "drafts", "attachments", "devices", "search"]
}
```

`features` is how a client discovers optional capability without a version bump.

---

## 2. Authentication

```text
POST /api/v1/client/auth/login     { email, password, device }
POST /api/v1/client/auth/refresh   { refresh_token, device_uid }
POST /api/v1/client/auth/logout    { refresh_token? }
GET  /api/v1/client/account
```

`device` identifies the installation and registers it:

```json
{
  "device_uid": "3f2c…",           // client-generated, stable for the install
  "name": "Alice's laptop",
  "platform": "windows",           // windows | linux | macos | android | ios
  "client_version": "0.7.0"
}
```

Login returns the same token pair shape as the management API plus the device id:

```json
{
  "access_token": "eyJ…",
  "refresh_token": "rt_…",
  "token_type": "Bearer",
  "expires_in": 3600,
  "device_id": 12,
  "user": { "id": 7, "email": "alice@example.com" }
}
```

Rules:

* Access tokens live one hour. The client refreshes on `401`, once, then retries.
* **Refresh tokens rotate.** Each refresh returns a new one and invalidates the old.
  Presenting an already-used refresh token revokes the entire token family and
  returns `401` — that is the signal that a token was stolen.
* Every login records a `devices` row and publishes nothing; device *revocation*
  publishes `device.revoked` so the other sessions of that device drop their tokens.
* A revoked or expired device gets `401` on every endpoint, including sync.

Logout revokes the session. The client discards both tokens and, if the user asked
for it, wipes the local cache.

---

## 3. The sync cursor

This is the heart of FCP.

**The server is the source of truth** (specification §55). The client holds a cache
and a cursor per folder; it never re-downloads a mailbox.

A cursor is an opaque string. Today it is the decimal sequence number of the
`change_log` row, but the client must not parse it — it is only ever echoed back.

### `GET /api/v1/client/sync?mailbox_id=3&folder_id=5&cursor=0&limit=500`

| Parameter | Meaning |
|---|---|
| `mailbox_id` | which address (required) |
| `folder_id` | one folder; omit for account-level changes (folder list, drafts, settings) |
| `cursor` | last cursor the client successfully applied; `0` (or absent) on first sync |
| `limit` | maximum changes to return, capped at `client.sync_page_size` |

Response:

```json
{
  "next_cursor": "1841",
  "has_more": true,
  "changes": [
    { "type": "message_created", "seq": 1836, "message_id": 4821, "uid": 117 },
    { "type": "message_updated", "seq": 1837, "message_id": 4821, "flags": "seen" },
    { "type": "message_deleted", "seq": 1838, "message_id": 4712 },
    { "type": "message_moved",   "seq": 1839, "message_id": 4700, "from_folder_id": 5, "to_folder_id": 6 },
    { "type": "folder_created",  "seq": 1840, "folder_id": 6, "name": "Archive/2026" }
  ]
}
```

### The contract

1. **Apply, then advance.** The client applies every change in order, durably, and
   only then stores `next_cursor`. Crash before storing ⇒ the same page is fetched
   again; every change is therefore idempotent by construction.
2. **Page until `has_more` is false.** Never assume one response is the whole delta.
3. **Ordering is by `seq`, ascending and gapless per user.** If a client ever sees a
   gap it must discard its cursor and resync from `0`.
4. **Metadata first, bodies on demand.** `message_created` carries ids and flags, not
   the message. The client fetches the body lazily with
   `GET /api/v1/client/messages/:id` (or `…/raw`).
5. **Deletions are tombstones.** A `message_deleted` change is emitted even though
   the row may be gone; tombstones outlive messages for
   `client.tombstone_retention_days`.
6. **A stale cursor is recoverable.** A cursor older than the retained window gets
   `409 conflict` with `{"error":{"code":"conflict","message":"cursor too old; full resync required"}}`.
   The client responds by dropping its cache for that folder and syncing from `0`.
7. **First sync is a normal sync** from cursor `0`. To show progress, the client
   compares the number of changes applied against the folder's `message_count`
   reported by `GET /api/v1/client/mailboxes`.

### Ordering guarantee across folders

`seq` is global per user, so a client syncing several folders can use one cursor per
folder and still see a consistent relative order. Merging a `message_moved` that
arrives in the target folder's stream with a `message_deleted` from the source
folder's stream is safe in either order, because both are keyed by `message_id`.

---

## 4. Mailboxes and folders

```text
GET /api/v1/client/mailboxes
```

```json
{
  "mailboxes": [
    {
      "id": 3, "address": "alice@example.com", "display_name": "Alice", "is_primary": true,
      "folders": [
        { "id": 5, "name": "INBOX",   "special_use": null,      "message_count": 412, "unseen_count": 3, "uid_validity": 1, "uid_next": 118 },
        { "id": 6, "name": "Sent",    "special_use": "\\Sent",  "message_count": 152, "unseen_count": 0, "uid_validity": 1, "uid_next": 153 },
        { "id": 7, "name": "Drafts",  "special_use": "\\Drafts","message_count": 2,   "unseen_count": 0, "uid_validity": 1, "uid_next": 3 },
        { "id": 8, "name": "Trash",   "special_use": "\\Trash", "message_count": 9,   "unseen_count": 0, "uid_validity": 1, "uid_next": 10 },
        { "id": 9, "name": "Junk",    "special_use": "\\Junk",  "message_count": 1,   "unseen_count": 1, "uid_validity": 1, "uid_next": 2 },
        { "id": 10, "name": "Archive","special_use": "\\Archive","message_count": 39, "unseen_count": 0, "uid_validity": 1, "uid_next": 40 }
      ]
    }
  ]
}
```

Clients map `special_use` rather than guessing names, so an account whose Sent folder
is called `Sent Items` still lines up.

`uid_validity` changes when the server renumbers a folder (a rebuild, a migration).
A client that sees a different `uid_validity` for the same folder id must discard the
folder's cache and resync — its UID cache is meaningless.

---

## 5. Messages

```text
GET    /api/v1/client/messages?mailbox_id=3&folder_id=5&limit=50&offset=0
GET    /api/v1/client/messages/:id            # metadata + headers + snippets
GET    /api/v1/client/messages/:id/raw        # the RFC 5322 bytes
POST   /api/v1/client/messages                # send
PATCH  /api/v1/client/messages/:id            # { seen?, flagged?, answered?, deleted? }
DELETE /api/v1/client/messages/:id            # ?permanent=true
POST   /api/v1/client/messages/:id/read
POST   /api/v1/client/messages/:id/unread
POST   /api/v1/client/messages/:id/star
POST   /api/v1/client/messages/:id/archive
POST   /api/v1/client/messages/:id/move       # { folder_id }
POST   /api/v1/client/messages/:id/trash
```

List item shape — headers and flags only, no bodies:

```json
{
  "items": [
    {
      "id": 4821, "uid": 117, "folder_id": 5,
      "subject": "Invoice for September",
      "from": { "address": "bob@example.net", "name": "Bob" },
      "to": [ { "address": "alice@example.com", "name": null } ],
      "snippet": "Hi Alice, attached is the invoice for September…",
      "flags": "seen",
      "size_bytes": 24831,
      "has_attachments": true,
      "attachment_count": 1,
      "internal_date": "2026-09-16T09:12:44Z",
      "sent_at": "2026-09-16T09:12:31Z",
      "rfc_message_id": "<20260916091231.7f3a@example.net>"
    }
  ],
  "total": 412, "limit": 50, "offset": 0
}
```

Full message adds `text_body`, `html_body`, `attachments[]` and the raw header list.
`html_body` is sanitised server-side when `security.sanitize_html` is on.

### Offline operations

Every mutating request carries a client-generated operation id:

```json
{ "operation_id": "op_9f2c41…", "type": "mark_read", "message_id": 4821 }
```

The server records the operation and replays the original response if it sees the id
again, so a retry after a timeout — or after the client crashed mid-request — never
acts twice. This is what makes the Outbox safe (specification §55).

Clients should generate ids before enqueuing locally, never at send time.

---

## 6. Attachments

Metadata and small files:

```text
POST   /api/v1/client/attachments        multipart/form-data
GET    /api/v1/client/attachments/:id    streams bytes; supports Range and ETag
```

For large files, or to resume an interrupted upload:

```text
POST   /api/v1/client/attachments/init       { filename, content_type, size_bytes }
   -> { attachment_id, chunk_size, upload_token }
PUT    /api/v1/client/attachments/:id/chunk?index=N   (raw bytes, exactly chunk_size except the last)
POST   /api/v1/client/attachments/:id/complete        { sha256 }
   -> { id, filename, content_type, size_bytes, sha256 }
```

`complete` verifies the digest; a mismatch returns `409 conflict` and discards the
upload. Chunks may arrive out of order and may be retried; the server keeps a
bitmap of received chunks. `GET /api/v1/client/attachments/:id/status` reports
which chunks it holds, so a client resuming after a crash uploads only the gap.

Downloads are content-addressed, so `ETag` is the blob's SHA-256 and repeated
downloads of the same file never re-transfer bytes.

---

## 7. Drafts

```text
POST   /api/v1/client/drafts        { subject?, text?, html?, to[], cc[], bcc[], in_reply_to?, references[], attachment_ids[] }
GET    /api/v1/client/drafts
PATCH  /api/v1/client/drafts/:id
DELETE /api/v1/client/drafts/:id
```

Drafts are server-side so the same draft appears on every device. The server stores
the content as JSON and mirrors it into the mailbox's Drafts folder as a real
message (`\Draft` flag) so IMAP clients see it too. Deleting a draft from either
surface removes it from both.

Conflict policy: last write wins, and the client is told what it overwrote:

```json
{ "id": 44, "updated_at": "2026-09-16T12:00:01Z", "conflict": { "detected": true, "server_updated_at": "2026-09-16T11:59:58Z" } }
```

---

## 8. Realtime events

```text
GET /api/v1/client/events?cursor=1841     Upgrade: websocket
```

The socket authenticates with the same bearer token. On connect the client may pass
`cursor` to replay what it missed:

```json
{ "type": "hello", "protocol_version": 1, "heartbeat_secs": 30, "last_seq": 1841 }
```

Then a stream of frames, one event each:

```json
{ "seq": 1842, "id": "0f4c…", "at": "2026-09-16T12:00:00Z", "scope": "user:7",
  "type": "mail.received", "mailbox_id": 3, "message_id": 4822, "from": "bob@example.net",
  "subject": "Re: Invoice", "snippet": "Thanks, got it." }
```

Event names: `mail.received`, `mail.sent`, `mail.deleted`, `mail.read`,
`mail.flag_changed`, `mail.moved`, `draft.created`, `draft.updated`,
`delivery.updated`, `device.revoked`.

Client obligations:

* Reply to a `ping` frame with `pong`, and send `ping` when nothing has arrived for
  `heartbeat_secs` (30 s by default).
* On reconnect, sync from the stored cursor before trusting the socket again. The
  socket is an optimisation; **the cursor is the source of truth.** Events may be
  missed during an outage and will be replayed by sync.
* On `{"replay_gap": true}` the bus dropped events for this subscriber; run a sync
  immediately.

---

## 9. Devices

```text
GET    /api/v1/client/devices
DELETE /api/v1/client/devices/:id
POST   /api/v1/client/devices/:id/revoke
```

```json
{
  "devices": [
    { "id": 12, "device_uid": "3f2c…", "name": "Alice's laptop", "platform": "windows",
      "client_version": "0.7.0", "protocol_version": 1,
      "last_seen_at": "2026-09-16T12:00:00Z", "last_ip": "203.0.113.44", "created_at": "2026-08-01T10:00:00Z", "revoked": false }
  ]
}
```

Revoking a device:

1. marks the device revoked,
2. revokes every session belonging to it,
3. publishes `device.revoked` so a live socket for that device disconnects.

The revoked client must clear its tokens and its cache; its next request gets `401`.
Revoking the device you are calling from is allowed and takes effect immediately.

---

## 10. Server-side search fallback

Local search is the client's job (specification §30) and must work offline. When the
local index has no answer, the client asks the server, which searches the same
fields:

```text
GET /api/v1/client/search?q=from:bob+subject:invoice+has:attachment+after:2026-01-01&mailbox_id=3&folder_id=5&limit=50
```

The operator set is `from:`, `to:`, `subject:`, `body:`, `has:attachment`,
`is:unread`, `is:flagged`, `before:`, `after:`, `folder:`. Results are message list
items, newest first.

---

## 11. Error handling rules for clients

| Status | `code` | What the client must do |
|---|---|---|
| 401 | `unauthorized` | refresh once, retry once; on a second 401, log out and keep the cache |
| 403 | `forbidden` | surface it; do not retry |
| 409 | `conflict` | if it came from `sync`, full resync that folder; else surface it |
| 413 | `limit_exceeded` | tell the user what is too big; keep the draft |
| 426 | `unsupported` | refuse to run; prompt for an upgrade |
| 429 | `rate_limited` | honour `Retry-After`; queue locally and retry |
| 5xx / network | — | treat as temporary, keep pending operations, exponential backoff with jitter, and never lose them |

**Nothing the user typed may be lost to a network error.** Pending operations and
drafts live in the local database until the server acknowledges them.
