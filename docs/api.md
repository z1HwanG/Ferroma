# Ferroma HTTP API

Two audiences share one router:

| Surface | Base path | Who uses it |
|---|---|---|
| **Management API** | `/api/v1` | Webmail, the Admin console, scripts |
| **Client API (FCP)** | `/api/v1/client` | the official desktop clients |

Both are plain JSON over HTTP/1.1 and HTTP/2. TLS is terminated by Ferroma itself
(`api.tls_port`) or by a reverse proxy in front of it.

Everything on this page is implemented by `ferroma-api`; the protocol-level details
of the client surface (versions, cursors, real-time framing) live in
[`fcp.md`](fcp.md).

---

## 1. Conventions

### 1.1 Content type and encoding

Requests and responses are `application/json; charset=utf-8` unless stated
otherwise. Attachments are streamed as `application/octet-stream` with their real
`Content-Type` in the metadata. Field names are `snake_case`.

### 1.2 Authentication

| Surface | Mechanism |
|---|---|
| Management API | `Authorization: Bearer <access_token>` **or** a `ferroma_session` cookie (Webmail) |
| Client API | `Authorization: Bearer <access_token>` only |

Access tokens are HS256 JWTs issued by `POST /auth/login`. They are short-lived
(`api.access_token_ttl_secs`, one hour by default); the refresh token obtained at
the same time mints new ones. Refresh tokens are single-use: refreshing rotates them
and invalidates the previous value.

Admin endpoints additionally require `is_admin` on the user.

### 1.3 Errors

Every failure returns the same envelope:

```json
{
  "error": {
    "code": "invalid_input",
    "message": "recipient address has no domain: bob",
    "details": { "field": "to" }
  }
}
```

`code` is stable and machine-readable (it is literally
`FerromaError::code()`); `message` is human-readable and may change. `details` is
optional and only present when there is something structured to say.

`message` follows `Accept-Language` (RFC 9110): `zh` of any region selects the
Simplified Chinese catalog, anything else — including an absent header — answers in
English, and a message with no translation yet is sent in English rather than
dropped. `code` never changes with the locale, so a client's branching is
language-independent:

```http
GET /api/v1/domains/9999 HTTP/1.1
Authorization: Bearer …
Accept-Language: zh-CN,zh;q=0.9,en;q=0.8
```

```json
{ "error": { "code": "not_found", "message": "未找到：域名 9999" } }
```

A request whose path matches no route, or a route whose method is not allowed,
answers with this same envelope rather than an empty body.

| HTTP | `code` | Meaning |
|---|---|---|
| 400 | `invalid_input`, `parse_error`, `protocol_error` | the request is malformed |
| 401 | `unauthorized` | missing, expired or revoked credentials |
| 403 | `forbidden` | authenticated but not allowed (e.g. not an admin) |
| 404 | `not_found` | no such message, mailbox, draft, device |
| 409 | `conflict` | uniqueness or state violation; an operation whose original request is **still in flight**; a sync cursor older than the retained history |
| 413 | `limit_exceeded`, `mailbox_full` | message too large, too many recipients, or the recipient's mailbox is full |
| 426 | `unsupported` | the client's `X-Ferroma-Protocol` is below `client.min_protocol_version`; upgrade required |
| 429 | `rate_limited` | slow down; honour `Retry-After` |
| 500 | `storage_error`, `internal_error` | a bug or a database failure |
| 501 | `unsupported` | a specified-but-unimplemented capability, where no protocol upgrade would help |
| 502 | `dns_error`, `network_error`, `timeout` | an upstream failure |

`409` is **not** what a client sees for an operation it already completed. See §1.5.

### 1.4 Pagination

List endpoints accept `limit` (default 50, maximum 500) and `offset` and return:

```json
{ "items": [ … ], "total": 1234, "limit": 50, "offset": 0 }
```

Incremental sync does **not** use pagination; it uses a cursor (§5).

### 1.5 Idempotency

State-changing requests accept an idempotency key: the `Idempotency-Key` header on the
management surface, or `operation_id` in the body on the Client API. `POST`, `PATCH`
and `DELETE` all accept it.

The server records the operation and **replays the original response** for a repeat —
same status, same body — so a client that retries after a timeout never double-sends
or double-deletes. Keys are kept for `client.tombstone_retention_days`.

```text
first  POST /api/v1/client/messages  {"operation_id":"op_9f2c…", …}  -> 201 {"message_id":4821,…}
retry  POST /api/v1/client/messages  {"operation_id":"op_9f2c…", …}  -> 201 {"message_id":4821,…}
```

Three outcomes are worth distinguishing, because a client must react to them
differently:

| Situation | Response | What the client does |
|---|---|---|
| The key is new | the operation runs | normal handling |
| The key was already **completed** | the recorded response, replayed verbatim | treat as success; do **not** retry |
| The key's original request is **still running** (or its process died mid-request) | `409 conflict`, `"operation … has not finished; retry later"` | retry later with the same key |

A `DELETE` that is retried after the resource is already gone answers `404`; a client
flushing a queued delete must treat that as success, since the desired end state
holds. That is what makes `DELETE` safe without a separate tombstone API.

### 1.6 Rate limits

`429` responses carry `Retry-After` in seconds. Submission and login are limited per
account; the API is limited per token.

### 1.7 Timestamps

RFC 3339 / ISO 8601 in UTC, e.g. `2026-09-16T12:00:00Z`. IDs are integers except
`operation_id` (`op_…`) and `device_uid` (client-generated string).

---

## 2. Health and discovery

### `GET /api/v1/health`

No authentication. Drives the container health check.

```json
{
  "status": "ok",
  "version": "0.1.0",
  "protocol_version": 1,
  "uptime_secs": 84213,
  "database": { "ok": true, "server_version": "PostgreSQL 16.15", "pool": { "size": 4, "idle": 3, "max": 20 } },
  "smtp": { "enabled": true, "connections": 3 },
  "imap": { "enabled": true, "connections": 1 },
  "clients": { "active_sessions": 4, "active_devices": 2 },
  "queue": { "pending": 0, "delivering": 0, "retry": 2, "failed": 1, "cancelled": 0, "bounce_pending": 1, "bounce_processing": 0, "received_today": 128, "sent_today": 41 }
}
```

`clients.active_sessions` counts live Webmail/API/client sessions (rows in `sessions`
that are neither revoked nor expired), and `active_devices` counts non-revoked
`devices` rows. `queue.received_today` and `sent_today` cover the current UTC day.
`queue.bounce_pending` and `bounce_processing` count delivery reports still owed to a
sender: a failed delivery whose report has not gone out is otherwise
indistinguishable from an ordinary failure, and it is the one queue state an operator
has to act on. All of these feed the Admin dashboard, which must render an absent
figure as `—` rather than a fabricated zero.

Returns `503` with `"status": "degraded"` when the database is unreachable; the
`queue` and `clients` blocks are then omitted rather than reported as zero.

### `GET /api/v1/version`

`{ "version": "0.1.0", "protocol_version": 1, "git_sha": "abc1234", "built": "…" }`

### `GET /.well-known/ferroma`

Unauthenticated autodiscovery (specification §32). Clients fetch this from the
domain of the address the user typed.

```json
{
  "api": "https://mail.example.com/api/v1",
  "imap": { "host": "mail.example.com", "port": 993, "tls": true, "security": "implicit" },
  "smtp": { "host": "mail.example.com", "port": 587, "tls": true, "security": "starttls" },
  "web": "https://mail.example.com",
  "protocol_version": 1
}
```

`tls` says whether the connection is encrypted. `security` says how: `implicit` on
465 and 993 (TLS from the first byte), `starttls` on 587 and 143 (SMTP or IMAP first,
then the upgrade). A client that treats every `tls: true` as implicit TLS opens 587
that way and the handshake fails. When the implicit port is not listening, the
document names the plaintext port and `starttls` instead of advertising a closed 465
or 993. `security` is omitted when no TLS is configured.

### `GET /.well-known/mta-sts.txt` and `GET /api/v1/domains/:id/dns`

See §4.4.

---

## 3. Authentication (management surface)

### `POST /api/v1/auth/login`

```json
{ "email": "alice@example.com", "password": "…", "device_name": "Firefox on Linux", "totp": "123456" }
```

```json
{
  "access_token": "eyJ…",
  "refresh_token": "rt_…",
  "token_type": "Bearer",
  "expires_in": 3600,
  "user": { "id": 7, "email": "alice@example.com", "display_name": "Alice", "is_admin": false, "quota_bytes": 1073741824, "used_bytes": 52428800 }
}
```

Sets the `ferroma_session` cookie when `device_name` is absent (browser flow).
`401 unauthorized` for bad credentials; `429 rate_limited` after
`limits.max_failed_logins` failures, with the account locked for
`limits.login_lockout_secs`.

`totp` is a six-digit code or a one-time recovery code, and is required only when
the account enforces a second factor. Omitting it then answers
`401 totp_required` — deliberately **not** `unauthorized`, because it means "send
the code" rather than "the password was wrong", and a client that conflated the two
would retry the password forever. A wrong code counts as a failed login, so the
lockout covers code guessing too.

`password` may also be an **application password** (`ap_…`) on any login surface,
including the client and JMAP ones. That is what lets a mail client — which cannot
be asked for a code — keep working while the second factor is enforced.

### `POST /api/v1/auth/refresh`

`{ "refresh_token": "rt_…" }` → a brand-new token pair. The presented refresh token
is invalidated. A reused refresh token revokes the whole family and returns `401`.

### `POST /api/v1/auth/logout`

Revokes the current session. `204 No Content`.

### `GET /api/v1/auth/me`

The authenticated user, plus their addresses:

```json
{
  "id": 7, "email": "alice@example.com", "display_name": "Alice",
  "is_admin": false, "quota_bytes": 1073741824, "used_bytes": 52428800,
  "mailboxes": [
    { "id": 3, "address": "alice@example.com", "user_id": 7, "display_name": "Alice",
      "is_primary": true, "enabled": true, "quota_bytes": null,
      "used_bytes": 4096, "created_at": "2026-01-01T00:00:00Z" }
  ]
}
```

### `POST /api/v1/auth/password`

`{ "current_password": "…", "new_password": "…" }`. Revokes every other session.

### Second factors and application passwords

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/v1/auth/totp` | `{ "status": "disabled\|pending\|enabled", "recovery_codes_left": 0 }` |
| `POST` | `/api/v1/auth/totp/enroll` | issue `{ "secret", "uri" }` — a `pending` enrollment, **not** enforced. The Webmail draws `uri` as a QR code in the page |
| `POST` | `/api/v1/auth/totp/confirm` | `{ "code": "123456" }` proves the authenticator holds the secret; answers `{ "enabled": true, "recovery_codes": ["…"] }` |
| `POST` | `/api/v1/auth/totp/disable` | `{ "password": "…" }` |
| `GET` | `/api/v1/auth/app-passwords` | every application password, revoked ones included |
| `POST` | `/api/v1/auth/app-passwords` | `{ "label": "Thunderbird" }` → the secret, returned **once** |
| `DELETE` | `/api/v1/auth/app-passwords/:id` | revoke one; `404` on a second attempt |

Three properties are deliberate. An enrollment is **not enforced until confirmed**,
so a mis-scanned QR code cannot lock an account out. `recovery_codes` are returned
in plaintext exactly once — only digests are stored — and each works once.
Disabling the factor **costs the account password**, not merely a live session: a
stolen session is the case the factor exists for, so it must not be able to remove
it.

---

## 4. Administration

Admin-only. `403 forbidden` for ordinary users.

### 4.1 Users

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/users` | `?query=&limit=&offset=` |
| `POST` | `/api/v1/users` | `{email, password, display_name?, is_admin?, quota_bytes?}` — also creates the primary address when the email's domain already exists (`mailboxes` in the response; empty when it does not) |
| `GET` | `/api/v1/users/:id` | |
| `PATCH` | `/api/v1/users/:id` | any of `display_name`, `enabled`, `is_admin`, `quota_bytes`, `password` |
| `DELETE` | `/api/v1/users/:id` | cascades: addresses, folders, messages, queue rows |
| `GET` | `/api/v1/users/:id/mailboxes` | addresses owned by the user |
| `POST` | `/api/v1/users/:id/mailboxes` | `{domain, local_part, is_primary?, quota_bytes?}` — creates the Maildir and the standard folders |
| `PATCH` | `/api/v1/users/:id/mailboxes/:mailbox_id` | `{is_primary?, quota_bytes?}`. `quota_bytes: 0` inherits the account quota. The address itself is not editable. |
| `GET` | `/api/v1/users/:id/security` | `{totp_status, recovery_codes_left, app_passwords[]}` |
| `DELETE` | `/api/v1/users/:id/app-passwords/:app_id` | revoke one application password |

`GET /users/:id/security` is **read-only**. There is deliberately no endpoint that
clears a second factor: an administrator's session that could do that would be a
bypass for every account on the server, and the operator path for a user who has lost
both their authenticator and their recovery codes is `ferroma user totp-disable` on
the host ([deployment.md](deployment.md) §6.6). Revoking an application password *is*
offered, because a lost device has to be able to stop working, and it weakens
nothing — the user keeps their second factor.

### 4.2 Domains

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/domains` | |
| `POST` | `/api/v1/domains` | `{name, description?}` |
| `GET` | `/api/v1/domains/:id` | |
| `PATCH` | `/api/v1/domains/:id` | `enabled`, `description`, `catch_all` |
| `DELETE` | `/api/v1/domains/:id` | refuses while addresses exist unless `?force=true` |

### 4.3 Aliases

| Method | Path |
|---|---|
| `GET` | `/api/v1/domains/:id/aliases` |
| `POST` | `/api/v1/domains/:id/aliases` — `{local_part, target}` |
| `PATCH` | `/api/v1/aliases/:id` |
| `DELETE` | `/api/v1/aliases/:id` |

### 4.4 DNS diagnostics

`GET /api/v1/domains/:id/dns` runs the live checks behind the Admin "DNS Health"
panel (specification §16):

```json
{
  "domain": "example.com",
  "checked_at": "2026-09-16T12:00:00Z",
  "records": [
    { "kind": "MX",    "status": "ok",   "expected": "mail.example.com", "found": ["10 mail.example.com."] },
    { "kind": "A",     "status": "ok",   "expected": "203.0.113.10",     "found": ["203.0.113.10"] },
    { "kind": "AAAA",  "status": "skip", "found": [] },
    { "kind": "PTR",   "status": "ok",   "expected": "mail.example.com", "found": ["mail.example.com."] },
    { "kind": "SPF",   "status": "ok",   "found": ["v=spf1 mx -all"] },
    { "kind": "DKIM",  "status": "warn", "expected": "default._domainkey.example.com",
      "found": [], "hint": "publish the TXT record shown by GET /api/v1/domains/:id/dkim" },
    { "kind": "DMARC", "status": "ok",   "found": ["v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com"] }
  ],
  "score": 5,
  "max_score": 7
}
```

`status` is `ok`, `warn`, `fail` or `skip`.

Two rows depend on how this instance sends mail. `PTR` is what a *direct* sender needs — the
address on the wire is the one receivers reverse-resolve — so when `[queue] relay_host` is set,
that row is reported as `skip` with what it found and a hint saying so, rather than a warning
about a record no receiver of its mail will ever look up. `SPF` is the same shape: the `expected`
value is the record a direct sender publishes, and a relayed instance has to `include` its
provider's own domain instead — a name only that provider knows — so a published record that
delegates sending is accepted, with a hint naming the condition, rather than warned about.

`GET /api/v1/domains/:id/dkim` returns the DNS record to publish:

```json
{ "selector": "default", "record_name": "default._domainkey.example.com", "record_type": "TXT", "record_value": "v=DKIM1; k=rsa; p=MIIBIjANBg…" }
```

`POST /api/v1/domains/:id/dkim` generates a key pair when none exists.

### 4.5 Mail queue and delivery logs

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/queue` | `?status=pending\|delivering\|delivered\|retry\|failed\|cancelled&limit=&offset=` |
| `GET` | `/api/v1/queue/:id` | one entry plus its attempt history |
| `POST` | `/api/v1/queue/:id/retry` | requeue a failed entry now |
| `DELETE` | `/api/v1/queue/:id` | cancel |
| `GET` | `/api/v1/queue/stats` | counts per status, the outstanding delivery-report tasks, plus `next_due_at` |

### 4.6 Storage, audit and settings

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/storage` | see the shape below |
| `POST` | `/api/v1/storage/gc` | drops unreferenced attachment blobs and stale `tmp/` files |
| `GET` | `/api/v1/storage/destinations` | the archive destinations the server remembers |
| `PUT` | `/api/v1/storage/destinations` | `{items: [{id, to}]}` replaces the list |
| `GET` | `/api/v1/storage/transfer-settings` | S3 endpoint, region and presence flags; never returns keys |
| `PUT` | `/api/v1/storage/transfer-settings` | `{endpoint, region, access_key?, secret_key?}`; blank keys retain saved values, `clear_credentials: true` removes both |
| `POST` | `/api/v1/storage/export` | `{to, live?}` writes one archive while the server stays up |
| `GET` | `/api/v1/audit` | `?actor_user_id=&action=&since=&limit=&offset=` |
| `GET` | `/api/v1/settings` | DB-backed settings |
| `PUT` | `/api/v1/settings/:key` | `{ "value": … }` |
| `GET` | `/api/v1/services` | live SMTP and IMAP listener state |
| `PUT` | `/api/v1/services/:service` | `service` is `smtp`, `imap` or `jmap`; `{ "enabled": true\|false }` |

`GET /api/v1/services` returns `{ "smtp": { "available": true, "enabled": true },
"imap": { "available": true, "enabled": true }, "jmap": { "available": true, "enabled": true } }`.  `available` is false when the
process was started without that subsystem (`--only` or its static configuration); such a
listener cannot be enabled through the API. `PUT` is Admin-only, persists the choice as a
dedicated runtime setting, and applies it before returning. Disabling SMTP stops its MX,
submission and configured SMTPS listeners; disabling IMAP stops its plaintext and configured
IMAPS listeners. Idle sessions receive the protocol shutdown response and disconnect, while
mail already stored remains untouched. Disabling JMAP withdraws `/.well-known/jmap` and the
JMAP API, uploads and downloads while leaving Webmail and the management API online. TLS ports are still controlled by `smtps_port`,
`imaps_port`, and `tls.enabled` — this endpoint adds no TLS-specific switch.

`GET /api/v1/storage` answers the Admin "Storage" screen and the dashboard cards:

```json
{
  "maildir_bytes": 8123456789,
  "attachment_bytes": 1234567890,
  "database_bytes": 234567890,
  "mailboxes": 42,
  "messages": 128431,
  "users": 17,
  "domains": 3,
  "disk_total_bytes": 107374182400,
  "disk_free_bytes": 64424509440
}
```

`users`, `domains`, `mailboxes` and `messages` are counts, not sizes — and
`mailboxes` counts **addresses**, matching the schema. A figure a deployment cannot
report is omitted from the object rather than sent as `0`.

### 4.7 First-run setup

`GET /api/v1/setup` → `{ "required": true, "hostname": "mail.example.com",
"public_url": "https://mail.example.com" }` while no admin exists. `hostname` and
`public_url` are the values the running configuration advertises, so a client can
present them for confirmation instead of guessing.

`POST /api/v1/setup` creates the first admin, the domain and its primary address, then
returns a normal token pair (flattened at the top level) plus an `applied` object:

```json
{
  "access_token": "…", "refresh_token": "…", "token_type": "Bearer", "expires_in": 3600,
  "user": { "…": "…" },
  "applied": {
    "hostname": "mail.example.com",
    "public_url": "https://mail.example.com",
    "api_host": "0.0.0.0",
    "api_port": 8080,
    "tls_enabled": true,
    "tls_cert": "/etc/ferroma/tls/fullchain.pem",
    "tls_key": "/etc/ferroma/tls/privkey.pem",
    "restart_required": true
  }
}
```

The body is `{email, password, domain, domain_description?, hostname?, public_url?,
api_host?, api_port?, tls_enabled?, tls_cert?, tls_key?}`. `GET /api/v1/setup` reports
the running values for the same fields, so the wizard can prefill itself.

Every field except the administrator and the domain is optional, and every one of them is
*stored* rather than applied on the spot: a running process cannot move its own socket or
re-read a PEM file, so the values are written to `settings` and read back when the server
starts (see the server's `apply_stored_settings`). `applied` lists exactly what was stored,
and `restart_required` says whether that means a restart.

A submission that needs one is not left to the operator: the server replaces its own process
image as soon as this response is on the wire — a container keeps its ports, its volumes and
the same PID, which matters because in a container that process is PID 1 — and the console
waits for it to answer again and reloads into it. `ferroma.toml` and the environment still win
over a stored row, so a deployment that states its hostname explicitly is never overridden by
a stale wizard submission. A `tls_cert`/`tls_key` that is not a file *on the server* is
refused with `400`, because the path is read by the process, not by the browser.

`POST /api/v1/setup` answers `409 conflict` once an admin exists. Both endpoints
answer `404 not_found` when `api.enable_setup_wizard = false`: a disabled wizard is
an endpoint that is not there, which is how a client tells "disabled" apart from
"already completed".

### 4.8 System logs and devices

The Admin console's "System Logs" and "Devices" screens (specification §36) need a
management-side view; the client-API device routes are bearer-only and scoped to one
account.

`GET /api/v1/logs`

```json
{
  "items": [
    { "at": "2026-09-16T12:00:00Z", "level": "warn", "target": "ferroma_smtp::client",
      "message": "delivery deferred: 421 too many connections", "fields": { "queue_id": 91, "remote_mx": "mx1.example.net" } }
  ],
  "total": 1, "limit": 100, "offset": 0
}
```

Backed by a bounded in-process ring buffer that a `tracing` layer fills — see
`server/src/logring.rs`, which the server composes into its subscriber next to the
stdout layer. The ring **captures** at the level the operator configured
(`server.log_level`): a deployment logging at `info` records `INFO` and above, so the
panel shows the same story as `docker compose logs` rather than sitting empty on a
healthy host. It holds the most recent 1000 entries and is **lost on restart** — that
is the honest trade, and the response says so with `"buffer_entries"`,
`"buffer_capacity"` and `"oldest_at"`. Filters: `?level=` (a floor *within* what was
captured), `?target=`, `?query=`, `?since=`, `?limit=`, `?offset=`.

Message bodies, credentials and tokens are never written to this buffer; the log
layer scrubs values that look like opaque tokens (`rt_…`, `st_…`) before storing them.

`GET /api/v1/devices` — every device on the server, newest activity first:

```json
{
  "items": [
    { "id": 12, "user_id": 7, "email": "alice@example.com", "device_uid": "3f2c…",
      "name": "Alice's laptop", "platform": "windows", "client_version": "0.7.0",
      "protocol_version": 1, "last_seen_at": "2026-09-16T12:00:00Z",
      "last_ip": "203.0.113.44", "created_at": "2026-08-01T10:00:00Z", "revoked": false }
  ],
  "total": 1, "limit": 50, "offset": 0
}
```

Filters: `?user_id=`, `?include_revoked=`, `?platform=`.
`POST /api/v1/devices/:id/revoke` and `DELETE /api/v1/devices/:id` behave exactly as
the client-API equivalents: the device is marked revoked, every session it holds is
revoked, and `device.revoked` is published.

### 4.9 TLS

`GET /api/v1/tls` answers the Admin "TLS" screen: what TLS is configured, whether the
process can actually read the PEM files, and which ports offer it.

```json
{
  "enabled": true,
  "min_version": "1.2",
  "self_signed_fallback": false,
  "use_platform_roots": true,
  "allow_insecure_dev_mode": false,
  "certificate": {
    "path": "/etc/ferroma/tls/fullchain.pem",
    "present": true, "readable": true, "size_bytes": 4312,
    "modified_at": "2026-09-01T09:12:44Z",
    "sha256": "9f2c…", "error": null
  },
  "private_key": {
    "path": "/etc/ferroma/tls/privkey.pem",
    "present": true, "readable": false, "size_bytes": 2412,
    "modified_at": "2026-09-01T09:12:44Z",
    "sha256": null, "error": "Permission denied (os error 13)"
  },
  "listeners": {
    "smtps_port": 465, "imaps_port": 993, "https_port": 0,
    "public_url": "https://mail.example.com", "public_url_is_tls": true
  }
}
```

Two deliberate omissions:

* **The private key is never fingerprinted** — a hash of a secret is still a durable
  fact about it. Presence, size, mtime and `readable` are enough to catch the failure
  this screen exists for, which is a key the server's own user cannot open.
* **The certificate's `notBefore` / `notAfter` are not parsed.** This build links no
  X.509 parser, and an expiry derived from a file mtime would be a worse answer than
  no answer. Compare `sha256` against `openssl x509 -fingerprint -sha256 -noout`
  on the host, and watch expiry there.

`readable` is the useful field: `enabled: true` with `readable: false` is a server
that will fail its TLS handshakes while the configuration looks correct.

---

## 5. Mailboxes, messages and attachments

### 5.1 Mailboxes and folders

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/mailboxes` | the caller's addresses |
| `GET` | `/api/v1/mailboxes/:id/folders` | IMAP folders with `message_count`, `unseen_count`, `special_use`. Each folder is recounted from its live rows before the response, so a badge a move or a delete left behind is repaired by opening the list |
| `POST` | `/api/v1/mailboxes/:id/folders` | `{name, parent?}` |
| `PATCH` | `/api/v1/folders/:id` | `{name?, parent_id?, subscribed?}`. `parent_id: null` moves the folder to the top level, a number moves it inside that folder, and omitting the field leaves the parent alone. A move re-paths the folder and its descendants — the name is the path — and is refused when it would put a folder inside itself |
| `DELETE` | `/api/v1/folders/:id` | refuses `INBOX` |

### 5.2 Messages

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/messages` | `?mailbox_id=&folder_id=&query=&unread=&flagged=&has_attachments=&since=&before=&limit=&offset=` |
| `GET` | `/api/v1/messages/:id` | full message: headers, text body, html body, attachment metadata |
| `GET` | `/api/v1/messages/:id/raw` | the RFC 5322 bytes, `message/rfc822` |
| `POST` | `/api/v1/messages` | send. `{from, to[], cc[]?, bcc[]?, subject, text?, html?, attachments[]?, in_reply_to?, references[], draft_id?}` |
| `PATCH` | `/api/v1/messages/:id` | `{seen?, flagged?, answered?, deleted?}` |
| `POST` | `/api/v1/messages/:id/move` | `{folder_id}` |
| `POST` | `/api/v1/messages/:id/copy` | `{folder_id}` |
| `DELETE` | `/api/v1/messages/:id` | move to Trash; `?permanent=true` removes it |
| `POST` | `/api/v1/messages/batch` | `{operation: "read"\|"unread"\|"flag"\|"unflag"\|"move"\|"delete", ids: [ … ], folder_id?}` |

`query=` is a **word** search over the subject, the sender and the whole body, with
the same substring match over the subject, sender and snippet kept beside it — so
`invoi` still finds an `invoice`, and a word buried past the stored preview finds the
message it is in. Quotes and `or`/`-` are accepted as `websearch_to_tsquery` reads
them, and nothing a user can type makes the query fail. Bodies are indexed as mail
arrives; a message stored before body indexing existed is matched by its subject,
sender and snippet until `ferroma storage reindex-search` fills in its text
([deployment.md](deployment.md) §6.7).

Sending queues one `mail_queue` row per recipient and returns immediately:

```json
{ "message_id": 4821, "queued": 2, "recipients": ["bob@example.net", "carol@example.org"] }
```

`413 limit_exceeded` when the message exceeds `limits.max_message_size`,
`429 rate_limited` beyond `submission_rate_limit` per hour or `daily_send_limit`
per day.

`POST /api/v1/messages` also accepts `draft: true`, which files the message in the
sender's Drafts folder instead of queueing it. A draft needs no `to` — that is the
normal case for "save and come back to it".

#### The single-message shape

`GET /api/v1/messages/:id` returns everything a reader needs, including the headers
a reply must carry:

```json
{
  "id": 4821,
  "uid": 117,
  "folder_id": 5,
  "mailbox_id": 3,
  "subject": "Invoice for September",
  "from": { "address": "bob@example.net", "name": "Bob" },
  "to": [ { "address": "alice@example.com", "name": "Alice" } ],
  "cc": [],
  "reply_to": [],
  "flags": "seen",
  "size_bytes": 24831,
  "snippet": "Hi Alice, attached is the invoice…",
  "text_body": "Hi Alice,\n\nattached is the invoice for September.\n",
  "html_body": "<p>Hi Alice,</p><p>attached is the invoice for September.</p>",
  "message_id_header": "<20260916091231.7f3a@example.net>",
  "in_reply_to": "<20260915101100.4b21@example.com>",
  "references": ["<20260910120000.11ab@example.com>", "<20260915101100.4b21@example.com>"],
  "internal_date": "2026-09-16T09:12:44Z",
  "sent_at": "2026-09-16T09:12:31Z",
  "is_draft": false,
  "has_attachments": true,
  "attachment_count": 1,
  "attachments": [
    { "id": 991, "filename": "invoice-2026-09.pdf", "content_type": "application/pdf", "size_bytes": 24831, "is_inline": false, "content_id": null }
  ]
}
```

* `message_id_header` is the RFC 5322 `Message-ID` of this message. A reply needs it
  as `in_reply_to`, and `references` is this message's `references` with
  `message_id_header` appended. A client that does not receive
  `message_id_header` must send the reply **without** threading headers rather than
  inventing a value.
* `html_body` is sanitised server-side, **unconditionally**: `<script>`, `<style>` and
  `<iframe>` bodies, every `on*` handler, `javascript:`/`vbscript:`/`file:` URLs and
  non-image `data:` URLs are removed before the body is returned. There is
  deliberately no configuration switch — a setting that turns off HTML sanitisation
  is a setting that turns a mail client into a remote code execution vector. The
  sanitiser only ever *removes*, never rewrites, so it cannot introduce markup; it is
  a filter rather than a full parser, which is why a client must still render the
  result inside a sandbox.
* A message the caller does not own is a `404`, never a `403` — the API does not
  confirm that someone else's message exists.

### 5.3 Drafts

Drafts are server-side so the same draft appears on every device the user signs in
from. The management surface mirrors the client one (`fcp.md` §7) and both address
the same records.

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/drafts` | `?limit=&offset=` |
| `POST` | `/api/v1/drafts` | `{mailbox_id?, subject?, text?, html?, to?, cc?, bcc?, in_reply_to?, references?, attachment_ids?}` |
| `GET` | `/api/v1/drafts/:id` | |
| `PATCH` | `/api/v1/drafts/:id` | any subset of the create fields |
| `DELETE` | `/api/v1/drafts/:id` | |

A draft is also mirrored into the mailbox's `Drafts` folder as a real message
carrying `\Draft`, so an IMAP client sees it too; deleting it from either surface
removes it from both.

### 5.4 Contacts

An address is remembered when the account sends to it or receives mail that names it.
The owner edits everything except the address.

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/v1/contacts` | `?q=` matches the address, the name or the note. Favorites first. |
| `POST` | `/api/v1/contacts` | `{address, display_name?}` |
| `PATCH` | `/api/v1/contacts/:id` | `{display_name?, note?, favorite?, blocked?}`. An empty string clears the name or the note. |
| `DELETE` | `/api/v1/contacts/:id` | forgets it. The next message that names the address remembers it again, unblocked. |

Mail from a blocked address is delivered to Junk.

### 5.5 Attachments

| Method | Path | Notes |
|---|---|---|
| `POST` | `/api/v1/attachments` | `multipart/form-data`, field name **`file`**; streams to the blob store, returns `{id, filename, content_type, size_bytes, sha256}` |
| `GET` | `/api/v1/attachments/:id` | streams the bytes, supports `Range` and `ETag` |
| `DELETE` | `/api/v1/attachments/:id` | only while still unreferenced |
| `GET` | `/api/v1/attachments/:id/meta` | metadata without the bytes |

Uploads above `client.attachment_chunk_size` should use the chunked endpoints in
[`fcp.md`](fcp.md) §6. `GET /api/v1/client/attachments/:id/status` — the endpoint a
resuming client polls — answers:

```json
{ "attachment_id": 991, "size_bytes": 4194304, "chunk_size": 1048576,
  "chunk_count": 4, "received": [0, 1, 3], "complete": false }
```

`received` is the list of chunk indexes the server holds, so a client that crashed
mid-upload sends only the gap.

---

## 6. Client API (FCP)

The official clients use this surface exclusively. Full protocol semantics —
including the sync cursor, real-time framing and chunked uploads — are in
[`fcp.md`](fcp.md); this is the endpoint index from specification §19.

| Method | Path |
|---|---|
| `POST` | `/api/v1/client/auth/login` |
| `POST` | `/api/v1/client/auth/refresh` |
| `POST` | `/api/v1/client/auth/logout` |
| `GET` | `/api/v1/client/account` |
| `GET` | `/api/v1/client/mailboxes` |
| `GET` | `/api/v1/client/sync` |
| `GET` | `/api/v1/client/messages` |
| `GET` | `/api/v1/client/messages/:id` |
| `POST` | `/api/v1/client/messages` |
| `PATCH` | `/api/v1/client/messages/:id` |
| `DELETE` | `/api/v1/client/messages/:id` |
| `POST` | `/api/v1/client/messages/:id/read` |
| `POST` | `/api/v1/client/messages/:id/unread` |
| `POST` | `/api/v1/client/messages/:id/star` |
| `POST` | `/api/v1/client/messages/:id/archive` |
| `POST` | `/api/v1/client/messages/:id/move` |
| `POST` | `/api/v1/client/messages/:id/trash` |
| `GET`/`POST` | `/api/v1/client/drafts` |
| `PATCH`/`DELETE` | `/api/v1/client/drafts/:id` |
| `GET`/`POST` | `/api/v1/client/attachments`, `/api/v1/client/attachments/:id` |
| `GET` | `/api/v1/client/devices` |
| `DELETE` | `/api/v1/client/devices/:id` |
| `POST` | `/api/v1/client/devices/:id/revoke` |
| `GET` | `/api/v1/client/events` (WebSocket upgrade) |

Client requests declare themselves, and the server uses this to gate compatibility
(specification §56):

```http
X-Ferroma-Client: FerromaClient/0.7.0
X-Ferroma-Protocol: 1
X-Ferroma-Platform: windows
User-Agent: FerromaClient/0.7.0 (Windows 11; x86_64)
```

A client whose `X-Ferroma-Protocol` is below `client.min_protocol_version` receives
`426 Upgrade Required` with `{ "error": { "code": "unsupported", "message": "client protocol 0 is no longer supported; upgrade to FCP/1" } }`.

---

## 7. Sending an email, end to end

The flow the Webmail UI and the official client both follow:

1. `POST /api/v1/auth/login` (or the client equivalent) → tokens.
2. `GET /api/v1/mailboxes` → the addresses the user may send from.
3. `POST /api/v1/attachments` for each file; collect the ids.
4. `POST /api/v1/messages` with `from`, `to`, `subject`, `text`/`html` and the
   attachment ids. The server writes the message into the sender's Sent folder,
   enqueues one `mail_queue` row per recipient, publishes `mail.sent`, and returns
   `{message_id, queued, recipients}`.
5. `GET /api/v1/queue?status=retry,failed` (or the client's own Outbox view) to
   watch delivery; the server pushes `delivery.updated` events over the socket as
   each attempt completes.

---

## 8. JMAP

Ferroma also exposes the initial JMAP mail surface from RFC 8620 and RFC 8621. It
uses the existing mailboxes, Maildir files, repositories, events and change log; it
never creates a second mailbox or message store. JMAP is alongside IMAP and FCP, not
a replacement for either.

A standard client sends `Authorization: Basic` — the full mailbox address and its
password — to `GET /.well-known/jmap` and receives the RFC 8620 Session object. A
`401` names `Basic` in `WWW-Authenticate`, which is what tells the client to send
that password. That `401` is `application/problem+json` and carries a `type`
(RFC 7807), which is what a JMAP client library needs before it will treat the
status as an authentication failure and try Basic. The Ferroma error envelope is
still in the same body. The same request also accepts a JMAP bearer token minted by
`POST /api/jmap/auth/token` (`email`, `password`, `device_name`). That token is
accepted only by JMAP endpoints; browser cookies and ordinary REST or FCP bearer
sessions are rejected. A Basic login reuses the JMAP session already open for that
address instead of writing one row per request.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/.well-known/jmap` | authenticated JMAP Session discovery |
| `POST` | `/api/jmap/` | JMAP method-call endpoint |
| `POST` | `/api/jmap/auth/token` | create a dedicated JMAP token session |
| `POST` | `/api/jmap/upload/:accountId` | upload one raw blob |
| `GET` | `/api/jmap/download/:accountId/:blobId` | download an owned blob |

The method endpoint currently supports `Mailbox/get`, `Email/query`, `Email/get`,
`Email/set`, and `Email/import`. `Email/set` changes standard seen/flagged keywords
or destroys an email through the shared message service, so its changes are visible to
IMAP and FCP cursor sync. `Email/import` consumes a blob returned by the upload
endpoint and files the parsed RFC 5322 message into a selected existing folder.

This first cut deliberately does not advertise push, calendars, contacts, Sieve,
Mailbox mutations, arbitrary custom keyword patches, multi-mailbox Email membership,
or the full MIME-body property set. Clients must poll the JMAP state returned by the
Session and method responses. Those unsupported pieces are not claimed as RFC 8621
complete support.
