# SMTP in Ferroma

**Who should read this:** anyone implementing, testing or troubleshooting
`ferroma-smtp`, and any operator trying to work out why a remote server refused
their mail.

This document covers both directions of SMTP. Inbound is the listener that
accepts mail from other MTAs, and the submission listener that accepting mail
from your own users' mail clients. Outbound is the queue worker that resolves MX
records and delivers your users' mail to remote hosts. It specifies the command
set, the session state machine, the reply codes for every failure, the limits and
where each one is enforced, the `Received:` header Ferroma prepends, the retry
schedule, the 4xx-versus-5xx classification rule, and bounce generation.

> **Status:** implemented, and exercised by `cargo test --workspace` plus the
> acceptance suite over real sockets. The command set, session state machine,
> reply codes, limits, `Received:` header, retry schedule and classification rule
> below describe what `ferroma-smtp` does today; a row that says "not
> implemented" describes behaviour that is specified but not shipped. The
> configuration keys, limit values and error variants it refers to exist —
> `[smtp]` and `[limits]` in [`config/ferroma.toml`](../config/ferroma.toml),
> `Limits` in `crates/ferroma-core/src/limits.rs`, `FerromaError` in
> `crates/ferroma-core/src/error.rs`, and the `mail_queue` /
> `delivery_attempts` tables in `migrations/0001_initial.sql`.

---

## 1. Ports and listener roles

| Port | Config key | Role | TLS |
|---|---|---|---|
| 25 | `smtp.port` | Inbound MX. Accepts mail from other MTAs. Unauthenticated peers may deliver **only** to local domains. | plaintext, `STARTTLS` offered |
| 587 | `smtp.submission_port` | Submission. For your users' mail clients. Authentication required before `MAIL FROM`. | plaintext, `STARTTLS` required by policy |
| 465 | `smtp.smtps_port` | Implicit TLS. Same policy as 587; the handshake comes first. `0` disables the listener. | TLS from the first byte |

`Config::validate()` refuses to start when `smtp.smtps_port != 0` and
`tls.enabled = false`, when two active SMTP ports collide, or when `smtp.port` is
`0`. See `crates/ferroma-core/src/config.rs`.

Which port a connection arrived on is part of the session, not a separate code
path: it selects the *policy profile* (whether authentication is mandatory, and
whether an unauthenticated transaction may address a non-local domain).

---

## 2. Command set (§9)

Specification §9.1 splits the command set into two phases.

| Phase | Commands | Status |
|---|---|---|
| **MVP** | `EHLO`, `HELO`, `MAIL FROM`, `RCPT TO`, `DATA`, `RSET`, `NOOP`, `QUIT` | first release |
| **Second phase** | `AUTH`, `STARTTLS` | first release for submission; `STARTTLS` on port 25 as soon as `tls.enabled` |

Additional commands the implementation accepts, with the RFC that defines them:

| Command | RFC | Notes |
|---|---|---|
| `VRFY` | 5321 §4.1.1.6 | answered `252 2.1.5 Cannot VRFY user, but will accept message` — never confirms whether an address exists |
| `HELP` | 5321 §4.1.1.4 | `214 2.0.0` plus a one-line summary |
| `EXPN` | 5321 §4.1.1.7 | not accepted; it falls through to `500 5.5.2 Command unrecognized` |
| `STARTTLS` | 3207 | available only when `tls.enabled` and not already encrypted |
| `AUTH` | 4954 | `PLAIN` and `LOGIN` only |
| `BDAT` | 3030 | recognised so the refusal is a precise `502 5.5.1` — `CHUNKING` is not advertised |

Anything else is `500 5.5.2 Command unrecognized`.

### Command lines

* Maximum command line: 512 octets including `CRLF` (RFC 5321 §4.5.3.1.4). Longer
  ⇒ `500 5.5.2 Line too long`. The limit is checked before parsing, so an
  attacker cannot make the parser allocate.
* Commands are case-insensitive (`mail from:` is valid).
* `MAIL FROM`, `RCPT TO` and `AUTH` may carry ESMTP parameters
  (`SIZE=`, `BODY=8BITMIME`, `SMTPUTF8`, `AUTH=`) separated by spaces. Unknown
  parameters are ignored, per RFC 5321 §4.1.1.11.
* Unknown verbs are rejected; unknown *parameters* are not. That asymmetry is
  what the RFC asks for and it is what keeps a new client working against an old
  server.

---

## 3. The state machine

Specification §9.2 gives the enum; the shipped implementation uses it verbatim,
extended with the encrypted flag that `STARTTLS` needs.

```rust
/// crates/ferroma-smtp/src/server/state.rs
pub enum SmtpState {
    Connected,      // greeting sent, nothing received yet
    Greeted,        // EHLO/HELO accepted
    MailFrom,       // MAIL FROM accepted, no recipient yet
    RcptTo,         // at least one RCPT TO accepted
    Data,           // 354 sent, reading the dot-terminated body
    Authenticated,  // SASL succeeded (orthogonal to the five above)
}
```

Transitions:

```text
                    ┌──────────────┐
   connect ────────►│  Connected   │ 220 <smtp.banner>
                    └──────┬───────┘
        EHLO/HELO ─────────┤          250-… / 250 SIZE n
                           ▼
                    ┌──────────────┐
                    │   Greeted    │◄──── RSET (from any state)
                    └──────┬───────┘
      MAIL FROM:<…> ───────┤          250 2.1.0
                           ▼
                    ┌──────────────┐
                    │   MailFrom   │
                    └──────┬───────┘
      RCPT TO:<…> ─────────┤          250 2.1.5  (repeatable)
                           ▼
                    ┌──────────────┐
                    │    RcptTo    │
                    └──────┬───────┘
         DATA ─────────────┤          354 End data with <CR><LF>.<CR><LF>
                           ▼
                    ┌──────────────┐
                    │     Data     │  (body with limits.data_timeout_secs)
                    └──────┬───────┘
        end-of-data ───────┴─────────► 250 2.0.0 Ok: queued as <id>  → Greeted
                                   └──► 4xx / 5xx                    → Greeted
```

`Authenticated` is not a position in that sequence; it is a flag that survives
`RSET` and changes what the session is allowed to do. AUTH is legal in
`Greeted` and later, never in `Connected` (`503 5.5.1 Send HELO/EHLO first`) and
never during `Data`.

Guards, and the reply when a guard fires:

| Guard | Fires when | Reply |
|---|---|---|
| `helo_required` | `MAIL FROM` arrives without `EHLO`/`HELO` | `503 5.5.1 Send HELO/EHLO first` |
| Sequence | `RCPT TO` before `MAIL FROM` | `503 5.5.1 Need MAIL FROM before RCPT TO` |
| Sequence | `DATA` with no accepted recipient | `503 5.5.1 Need RCPT TO before DATA` |
| `require_auth_on_submission` | `MAIL FROM` on the submission port without AUTH | `530 5.7.0 Authentication required` |
| `require_tls_for_auth` | `AUTH` on an unencrypted connection | `538 5.7.11 Encryption required for requested authentication mechanism` |
| Nested `MAIL` | a second `MAIL FROM` during a transaction | `503 5.5.1 Sender already specified` |
| Nested `DATA` | `DATA` while already in `Data` | impossible: the body reader consumes to the terminator |

---

## 4. The session struct

Specification §9.3:

```rust
/// crates/ferroma-smtp/src/server/session.rs
pub struct SmtpSession {
    /// Unique per accepted connection; also the `connection_id` log field.
    pub connection_id: Uuid,
    /// TCP peer address. Taken from `X-Forwarded-For` only when
    /// `api.trust_proxy_headers` is set and the peer is a trusted proxy.
    pub remote_addr: SocketAddr,
    /// The handler the connection is talking to.
    pub port: SmtpPort,          // Mx | Submission | Smtps
    pub state: SmtpState,
    /// The name the peer announced. Used in `Received:`, never trusted.
    pub helo: Option<String>,
    /// `true` once STARTTLS completed. Gates `require_tls_for_auth`.
    pub encrypted: bool,
    /// Set by a successful `AUTH`; the account whose quota and rate limits apply.
    pub authenticated_user: Option<UserId>,
    /// The session row created by `AuthService::open_session(SessionKind::Smtp)`.
    pub session_id: Option<SessionId>,
    /// `MAIL FROM` reverse-path, empty for a null sender (`<>`, a bounce).
    pub envelope_from: Option<String>,
    /// Accepted recipients, in order, after aliases and catch-all expansion.
    pub recipients: Vec<String>,
    /// `SIZE=` announced on `MAIL FROM`, when the client sent one.
    pub declared_size: Option<u64>,
    /// `BODY=` / `SMTPUTF8` parameters seen on `MAIL FROM`.
    pub body_8bit: bool,
    pub smtp_utf8: bool,
}
```

Every field that can be logged appears in the structured fields listed in
specification §40 (`connection_id`, `remote_ip`, `helo`, `authenticated_user`,
`sender`, `recipient`, `message_id`, `result`, `duration`). `ferroma-core`'s
`logging` module installs the subscriber that carries them.

The session is per connection and lives on one Tokio task. Nothing about it is
shared, so the state machine needs no locking; the counters that *are* shared
(connection totals, per-IP rate windows) live in the listener.

---

## 5. `EHLO` extensions advertised

`EHLO` is answered with one line per capability, and the whole set is only sent
when `smtp.advertise_extensions = true`; when it is `false` the session replies
with `250-<hostname>` and a bare `250 OK`, which is what an ancient client that
chokes on extensions needs.

| Line | Advertised when | Meaning |
|---|---|---|
| `250-<server.hostname>` | always | greeting line |
| `250-PIPELINING` | `smtp.advertise_extensions` | client may batch commands without waiting for replies |
| `250-SIZE <limits.max_message_size>` | `smtp.advertise_size` | maximum `DATA` size accepted, in octets |
| `250-8BITMIME` | `smtp.advertise_extensions` | `BODY=8BITMIME` is accepted |
| `250-ENHANCEDSTATUSCODES` | `smtp.advertise_extensions` | replies carry `x.y.z` status codes (see §11) |
| `250-SMTPUTF8` | `smtp.advertise_extensions` | UTF-8 local parts are accepted |
| `250-DSN` | never | `RET`/`ENVID` are not honoured; DSN (RFC 3461) is not implemented |
| `250-STARTTLS` | `tls.enabled` and not yet encrypted | the client may upgrade in place |
| `250-AUTH PLAIN LOGIN` | `tls.enabled` or `smtp.require_tls_for_auth = false` | SASL mechanisms |
| `250-HELP` | always | `HELP` is implemented |
| `250 CHUNKING` | never | `BDAT` is not implemented; do not advertise it |

`AUTH` is withheld entirely on a connection that is not encrypted when
`smtp.require_tls_for_auth = true`. Advertising a mechanism the session will
refuse is worse than not advertising it: it makes a client fail after sending
credentials it should never have sent in the clear.

---

## 6. Open-relay policy, and why it is the default

Specification §9.4 and §54:

```text
recipient domain is a local domain   →  accept (subject to quota and limits)
recipient domain is anything else    →  require a successful AUTH first
```

Concretely, in `RCPT TO` handling:

| Connection | Recipient domain | Outcome |
|---|---|---|
| unauthenticated, port 25 | local (`domains.name` matches, `domains.enabled`) | accepted, delivers locally |
| unauthenticated, port 25 | not local | `550 5.7.1 Relaying denied` |
| authenticated, any port | local | accepted |
| authenticated, port 587/465 | not local | accepted, queued to `mail_queue` |
| authenticated, port 25 | not local | accepted, queued — the account is authenticated, so this is submission, not relaying |

**Why the default is what it is.** An open relay is not a configuration
inconvenience; it is a machine for laundering spam that will be blacklisted
within hours and will take the operator's legitimate mail down with it. Every
mail administrator has seen it happen, and the damage is measured in months of
deliverability, not minutes of downtime. Making the safe behaviour the default
and the unsafe behaviour unreachable-by-typo is therefore worth the small amount
of extra configuration: there is no `allow_relay = true` key to find and set by
accident, and `[../AGENTS.md](../AGENTS.md)` §4.6 states the rule as
non-negotiable.

Two adjacent policy decisions that fall out of the same reasoning:

* **Local delivery does not require authentication.** Otherwise no other MTA
  could deliver mail to your users at all, which is the whole point of running an
  MX.
* **AUTH does not make you trustworthy for arbitrary `MAIL FROM`.** The From
  header must name an address the user owns — a `mailboxes` row with their
  `user_id`. That check lives in the API's send path
  (`resolve_sender` in `crates/ferroma-api/src/routes/mail/store.rs`), which is
  the only way a client of this server sends; the SMTP submission listener
  itself does not re-verify `MAIL FROM` against the authenticated user today.

---

## 7. Limits and where each one is enforced

Each limit is enforced at the protocol edge **and** re-checked in the mail core,
so talking to the API instead of SMTP cannot bypass it. `crates/ferroma-core/src/limits.rs`
is the single definition; `[limits]` in `config/ferroma.toml` is the single
source of values.

| Limit | Config key | Default | Enforced at | Reply or error |
|---|---|---|---|---|
| Message size | `limits.max_message_size` | 26214400 (25 MiB) | `DATA` body reader, from the `SIZE` extension and by counting octets; re-checked by the mail core before the Maildir write | `552 5.3.4 Message size exceeds fixed maximum message size` |
| Announced size, early | `MAIL FROM SIZE=` | — | `MAIL FROM`, before `354` | `552 5.3.4 Message size exceeds fixed maximum message size` |
| Recipients per transaction | `limits.max_recipients` | 100 | `RCPT TO` counter | `452 4.5.3 Too many recipients` |
| Simultaneous connections, global | `limits.max_connections` | 100 | listener accept loop | `421 4.3.2 Too many connections, try again later`, then close |
| Simultaneous connections per IP | `limits.max_connections_per_ip` | 10 | listener accept loop, per source address | `421 4.3.2 Too many connections from your address` |
| Inbound commands per minute per IP | `limits.smtp_rate_limit` | 100 | command loop, sliding window per IP | `421 4.7.0 Too many commands, slow down` |
| Submission messages per hour per account | `limits.submission_rate_limit` | 50 | on acceptance of an authenticated transaction | `452 4.7.0 Submission rate limit exceeded` |
| Messages per account per day | `limits.daily_send_limit` | 500 | on acceptance, counted from `mail_queue`, not from memory | `452 4.7.0 Daily send limit exceeded` |
| Mailbox quota | `users.quota_bytes`, `mailboxes.quota_bytes`, `limits.mailbox_quota` | 1 GiB | advisory precheck plus live usage and whole-batch quota check under user locks in the inbound SQL transaction | `452 4.2.2 Mailbox full` — temporary, so the sender retries after the user frees space |
| Command timeout | `smtp.command_timeout_secs` | 300 | command loop, per read | `421 4.4.2 Timeout waiting for command`, then close |
| `DATA` timeout | `smtp.data_timeout_secs` | 600 | body reader | `421 4.4.2 Timeout waiting for data`, then close |
| MIME nesting depth | `limits.max_mime_depth` | 20 | MIME parser (`ParseLimits` in `ferroma-mail`) | `552 5.3.4 MIME nesting deeper than 20 levels` — the message is refused, not filed into `Junk` |
| Attachments per message | `limits.max_attachments` | 50 | the API send path (`crates/ferroma-api/src/service.rs`); inbound delivery is bounded by `max_message_size` and the part budget | `422`/`LimitExceeded` on the API; the part budget maps to `552 5.3.4` |
| Single attachment size | `limits.max_attachment_size` | 26214400 | attachment extraction | `552 5.3.4` via `LimitExceeded` |

`Limits::validate()` refuses to boot when `max_connections_per_ip >
max_connections`, when `max_attachment_size > max_message_size`, when
`max_message_size` is `0`, or when `max_mime_depth` is outside 1–100. A typo in
`[limits]` therefore stops the server at boot instead of silently disabling a
limit.

The per-account limits are the ones that matter for abuse: an authenticated
account that has been compromised can otherwise be used as a spam cannon at
whatever rate the host's uplink allows. `submission_rate_limit` and
`daily_send_limit` are checked on *acceptance*, and the daily count is derived
from `mail_queue` rows rather than an in-memory counter, so a restart does not
reset it.

---

## 8. `AUTH PLAIN` and `AUTH LOGIN`

Specification §15 specifies Argon2id for password storage and `AUTH PLAIN` /
`AUTH LOGIN` for SMTP.

| Mechanism | Wire form | Notes |
|---|---|---|
| `PLAIN` | `AUTH PLAIN <base64(\0authcid\0passwd)>` or `AUTH PLAIN` then a `334` continuation carrying the same blob | one round trip; the continuation form is what a client uses after a `334` |
| `LOGIN` | `AUTH LOGIN` → `334 VXNlcm5hbWU6` → base64 username → `334 UGFzc3dvcmQ6` → base64 password | two round trips; the prompts are the conventional base64 of `Username:` and `Password:` |

An initial-response `=` (RFC 4954 §4) means "empty", which is a parse error for
both mechanisms: `501 5.5.4 Invalid base64 data`.

Successful authentication:

1. The session decodes the credentials and calls
   `AuthService::login(email, password, SessionKind::Smtp, ip, user_agent, None)`.
2. `AuthService` verifies against the stored Argon2id PHC string
   (`PasswordHasher::verify`), transparently re-hashes when
   `PasswordHasher::needs_rehash` says the stored parameters are stale, and
   records the attempt through `LoginAttemptsRepository`.
3. On success the session gets an `authenticated_user` and a `SessionId` from
   `sessions` with `kind = 'smtp'`.
4. The reply is `235 2.7.0 Authentication successful`.

Failure replies — deliberately indistinguishable between "no such user" and
"wrong password", exactly as `AuthService::login` makes them:

| Condition | Reply |
|---|---|
| Wrong password, unknown account, disabled account | `535 5.7.8 Authentication credentials invalid` |
| `limits.max_failed_logins` reached, or the account is locked | `454 4.7.0 Temporary authentication failure` |
| More than `3 × limits.max_failed_logins` failures from one source IP in the window | `454 4.7.0 Temporary authentication failure` |
| Malformed base64, unknown mechanism | `501 5.5.4 Invalid base64 data` / `504 5.5.4 Unrecognized authentication type` |
| `AUTH` after a successful `AUTH` | `503 5.5.1 Already authenticated` |
| `AUTH` on cleartext with `smtp.require_tls_for_auth = true` | `538 5.7.11 Encryption required for requested authentication mechanism` |

### `require_tls_for_auth`

Default `false` in `config/ferroma.toml` so a bare `cargo run` works without
certificates. `docker-compose.yml` sets
`FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH: 'true'`, and a production MX should keep it
there: `AUTH PLAIN` and `AUTH LOGIN` both send the password in base64, which is
encoding, not encryption. Over plaintext port 587 a passive observer reads the
password in the clear. With the flag on, `AUTH` is neither advertised nor
accepted until `STARTTLS` completes.

`Config::allows_plaintext_auth()` exists so callers can ask the question without
re-deriving it from the TLS flags.

---

## 9. `STARTTLS`, SMTPS and the submission role

| Port | What happens | Policy |
|---|---|---|
| 25 | `EHLO` advertises `STARTTLS`; the client sends `STARTTLS`, gets `220 2.0.0 Ready to start TLS`, and both sides renegotiate. The session returns to `Connected` and the client must send a fresh `EHLO`. | Mail from other MTAs is accepted opportunistically. Refusing plaintext inbound would lose mail from every host that does not do TLS. |
| 587 | Same upgrade path, but submission policy applies: `require_auth_on_submission` means `MAIL FROM` is refused until AUTH succeeds, and `require_tls_for_auth` means AUTH is refused until TLS succeeds. | Submission. A client that cannot do `STARTTLS` cannot send. |
| 465 | TLS from the first octet (SMTPS). No `STARTTLS` is advertised — there is nothing to upgrade. | Submission. |

Details that are easy to get wrong and are therefore specified:

* **State after `STARTTLS` is reset.** `helo`, `envelope_from`, `recipients`,
  `declared_size` and the AUTH flag are all cleared; only `connection_id` and
  `remote_addr` survive. A client that authenticated before `STARTTLS` must
  authenticate again — otherwise an attacker could inject commands into the
  plaintext session and have them apply to the encrypted one.
* **No command may be pipelined across `STARTTLS`.** A `STARTTLS` command line
  followed by anything in the same TCP segment is an error, per RFC 3207 §6.
  Reply: `554 5.5.1 Pipelining violated` then close.
* **`STARTTLS` when `tls.enabled = false`** is answered
  `502 5.5.1 Command not implemented`, and it is not advertised.
* **`STARTTLS` when already encrypted** is `503 5.5.1 TLS already active`.
* **A failed TLS handshake closes the connection** without a reply; there is no
  plaintext fallback, which is what prevents a downgrade attack.
* **Certificate material** comes from `tls.cert_path` / `tls.key_path` (a PEM
  bundle: leaf followed by intermediates, and a PKCS#8 or PKCS#1 private key).
  `tls.self_signed_fallback` generates a certificate with `rcgen` at boot for
  development only — `tls.allow_insecure_dev_mode` has to be true as well, and
  `Config::validate()` enforces the pairing.
* **Minimum protocol version** is `tls.min_version`, `"1.2"` by default; `"1.3"`
  is accepted. Anything else refuses to boot.
* **rustls only.** No `native-tls`, no OpenSSL, no schannel — see
  [architecture.md](architecture.md) §8 and [../AGENTS.md](../AGENTS.md) §1.1.

---

## 10. The `Received:` header

Inbound mail gets one `Received:` header prepended when
`smtp.add_received_header = true` (the default). It is prepended, not appended:
`Received:` headers accumulate newest-first, and the top one is the first hop
that Ferroma knows about.

```text
Received: from <helo> (<reverse-dns> [<remote-ip>])
        by <server.hostname> (Ferroma <version>)
        with ESMTPS id <connection_id>
        for <recipient>
        ; <date>
```

Rendered example (illustrative output):

```text
Received: from mail.example.net (mail.example.net [203.0.113.25])
        by mail.example.com (Ferroma 0.1.0)
        with ESMTPS id 0f4c9a12-6b1e-4d3f-9a77-2c1f0e5b8d41
        for <alice@example.com>
        ; Tue, 16 Sep 2026 09:12:31 +0000
```

Field-by-field rules:

| Field | Source | Rule |
|---|---|---|
| `from <helo>` | `SmtpSession::helo` | the name the peer announced — untrusted, displayed, never used for a decision |
| `(<reverse-dns> [<remote-ip>])` | PTR lookup of `remote_addr` | omitted entirely when the lookup fails; brackets always carry the literal IP |
| `by <host>` | `server.hostname` | must be a valid DNS name or `Config::validate()` refuses to boot |
| `with <protocol>` | session | `SMTP` (plaintext), `ESMTP` (EHLO, plaintext), `ESMTPS` (EHLO + TLS), `ESMTPSA` (EHLO + TLS + AUTH), `ESMTPA` (EHLO + AUTH, no TLS) |
| `id <connection_id>` | `SmtpSession::connection_id` | the same UUID that appears in the logs, so a `Received:` line and a log line can be joined |
| `for <recipient>` | first accepted recipient | omitted when there are several recipients (it would leak the other recipients), which is RFC 5321 §4.4 practice for multi-recipient mail |
| `; <date>` | clock at acceptance | RFC 5322 `date-time`, always UTC with a `+0000` zone |

Two things the header never contains: the peer's IP is taken from the socket, not
from any header the peer sent, and no header the peer sent is ever copied into
the generated line.

`Authentication-Results` is added separately when `policy.add_auth_results` or
`policy.add_auth_results` is set; see [security.md](security.md) §8.

---

## 11. Outbound delivery

Specification §10 gives the pipeline; specification §11 the queue states and the
retry schedule.

```text
 SMTP submission / Webmail / Client API
                │
                ▼
            Mail Core                renders RFC 5322, signs DKIM,
                │                    writes the Sent copy
                ▼
            mail_queue                one row per recipient
                │
                ▼
        QueueRepository::claim_due()  status pending|retry → delivering
                │
                ▼
            DNS MX                    per recipient domain
                │
                ▼
       remote MX hosts, in preference order, :25
                │
                ▼
        SMTP client (EHLO → STARTTLS if offered → MAIL → RCPT → DATA)
                │
                ├── success  ──► delivered   + delivery_attempts row
                └── failure  ──► retry | failed, per §12
```

### 11.1 Queue states and the columns that hold them

`mail_queue.status` carries a `CHECK` constraint listing exactly these values:
`pending`, `delivering`, `delivered`, `retry`, `failed`, `cancelled`.

```text
  pending ──► delivering ──┬──► delivered
     ▲                     │
     │                     ├──► retry ──► delivering ──► …
     └─────────────────────┘
                           └──► failed   (attempts exhausted, or a 5xx)
```

| Column | Meaning |
|---|---|
| `mail_queue.message_id` | the stored copy the delivery is for; deleting its row is refused while a queue entry is pending, retrying or delivering (a database guard also covers folder/account cascades) |
| `mail_queue.sender` | envelope reverse-path |
| `mail_queue.recipient` | envelope forward-path — one row per recipient, so one bad recipient does not delay the others |
| `mail_queue.attempts` / `max_attempts` | attempts so far / the cap (`queue.max_attempts`, 12) |
| `mail_queue.next_attempt_at` | when the dispatcher may pick the row up; `mail_queue_due_idx` indexes exactly `WHERE status IN ('pending','retry')` |
| `mail_queue.last_error`, `last_status_code`, `last_status_text` | the most recent failure, for the Admin queue screen |
| `mail_queue.remote_mx` | the host that was tried |
| `mail_queue.delivered_at` | when it finally worked |

Cancellation succeeds only for `pending` or `retry`: once a worker claims a row,
remote SMTP may already have accepted it, so `delivering` cannot honestly be
reported as cancelled. The sender must wait for the outcome before retrying a
cancel request.

Every attempt also writes a `delivery_attempts` row (`queue_id`, `attempt`,
`remote_mx`, `status_code`, `status_text`, `error`, `duration_ms`), which is what
the Admin "Delivery Logs" screen reads. Attempts are history; the queue row is
state. On worker startup, a single-process Ferroma deployment returns claims
left in `delivering` by a previous process to `retry` before polling. While it
runs, claims older than one day are requeued on the next poll to recover from a
failed status update without stealing a merely slow attempt. A crash after a
remote MX accepted DATA but before the delivered state was recorded can still
cause duplicate delivery on retry: SMTP cannot provide exactly-once delivery.

### 11.2 MX resolution

`ferroma-smtp::mx::MxResolver` uses `hickory-resolver` (configured
from `[dns]`).

1. Look up `MX` for the recipient domain.
2. Sort by preference ascending, with a stable random tie-break inside equal
   preferences so that repeated attempts do not always hit the same host first —
   which is also what RFC 5321 §5.1 recommends and what stops one dead MX from
   absorbing every retry.
3. **Try hosts in order.** A connection failure, a `4xx` greeting, or a TLS
   failure moves to the next host in the same attempt. Only when every host has
   failed is the attempt recorded as failed.
4. **Null MX.** A single `MX 0 .` means the domain accepts no mail. That is a
   permanent failure (`failed`, and a bounce): `556 5.1.10` equivalent locally,
   `FerromaError::Invalid`.
5. **No MX, but an `A`/`AAAA` record.** Fall back to the address itself, per RFC
   5321 §5.1. Only when the fallback also fails is the domain undeliverable.
6. **No MX and no address.** `FerromaError::Dns` — classified per §12.5, which
   makes it temporary: a domain can be mid-registration.
7. **CNAME chains** are followed by the resolver, bounded by `dns.attempts`.
8. Timeouts come from `[dns]` (`timeout_secs`, `attempts`, `tcp_fallback`).

Connection concurrency to one host is capped by `queue.max_connections_per_host`
(4): hammering a single remote MX with four hundred parallel connections is how
a mail server gets itself blocked.

### 11.3 The retry schedule (§11)

`queue.retry_schedule_secs = [60, 300, 900, 3600, 21600, 86400]` — one minute,
five minutes, fifteen minutes, one hour, six hours, twenty-four hours. The last
entry repeats until `attempts` reaches `queue.max_attempts` (12).

`QueueConfig::backoff_for_attempt(attempt)` is the implementation of that rule: it
clamps the index to the end of the schedule, so attempt 7, 8, … all wait
86 400 seconds, and returns 60 when the schedule is empty.

| Attempt | Delay before it | Cumulative elapsed (approximately) |
|---|---|---|
| 1 | immediate (on acceptance) | 0 |
| 2 | 60 s | 1 min |
| 3 | 300 s | 6 min |
| 4 | 900 s | 21 min |
| 5 | 3600 s | 1 h 21 min |
| 6 | 21600 s | 7 h 21 min |
| 7 | 86400 s | 31 h 21 min |
| 8–12 | 86400 s each | up to ~5 days 7 h |

Four and a half days of trying is the conventional MTA posture: long enough that
a remote server's weekend outage does not bounce your user's mail, short enough
that the sender eventually learns the message did not arrive. `queue.retention_days`
(30) governs how long a `delivered` row is kept afterwards; it is independent of
the retry window.

The dispatcher polls with `queue.poll_interval_secs` (10) and runs
`queue.workers` (4) deliveries concurrently.

### 11.4 Outbound relay (smarthost)

Some hosts cannot deliver at all: their public IP has no PTR record, or the provider
will not set one — a ticket may come back saying it is unsupported. Mail from such
an IP is junked by Gmail and refused outright by Microsoft's properties, while
**receiving is unaffected**; only outbound needs the detour.

```toml
[queue]
# A hosting provider's submission service, a transactional mail API, or another
# host with a proper reverse record.
relay_host = "smtp.example-relay.com"
relay_port = 587
# starttls (587) | implicit (465) | none (an internal relay; never with credentials)
relay_tls = "starttls"
relay_username = "…"
relay_password = "…"
# Empty means everything goes through the relay; a list limits it to those domains.
relay_from_domains = ["example.com"]
```

What it does and does not change:

* The queue worker **skips MX resolution** and hands the envelope to the relay
  instead; domains outside `relay_from_domains` still resolve MX as in §11.2. The
  relay's own name is resolved with `A`/`AAAA` (`MxResolver::addresses`).
* **Local delivery never went through the queue**, so mail between local users is
  unaffected either way.
* **DKIM signing happens before the hand-off**, so the signature still carries your
  own domain and DMARC alignment is untouched; SPF needs the relay's sending domain
  `include:`d in your record ([security.md](security.md) §8).
* When the relay wants `AUTH`, credentials are sent
  **only over an encrypted channel**: `relay_tls = "none"` together with credentials
  is rejected by the boot check. `AUTH PLAIN` with an initial response is preferred,
  falling back to `AUTH LOGIN` when that is all the relay offers.
* **A rejected `AUTH` is a temporary failure, never a bounce**: a wrong password is a
  configuration mistake, and bouncing the queue over it destroys mail a corrected
  password would have delivered.
* Bounces (a null return path) go through the relay too — they are the messages a
  host with no reverse record gets refused for most readily.
* The startup banner says so out loud:
  `queue     4 worker(s), outbound via relay smtp.example-relay.com`.

---

## 12. `4xx` versus `5xx`, and `FerromaError::is_temporary()`

The classification is not decided in the SMTP layer. It is
`FerromaError::is_temporary()` — one function, in
`crates/ferroma-core/src/error.rs` — and it is the single source of truth behind
both SMTP reply classes and the queue's retry-versus-fail decision.

```rust
pub fn is_temporary(&self) -> bool {
    matches!(
        self,
        FerromaError::Io(_)
            | FerromaError::Network(_)
            | FerromaError::Dns(_)
            | FerromaError::RateLimited
            | FerromaError::Timeout(_)
            | FerromaError::Storage(_)
            | FerromaError::Internal(_)
    )
}
```

| `FerromaError` variant | `code()` | Temporary? | Meaning for a delivery |
|---|---|---|---|
| `Io` | `io_error` | yes | local filesystem/socket failure — try again |
| `Network` | `network_error` | yes | outbound connection failed |
| `Dns` | `dns_error` | yes | MX/A lookup failed or timed out |
| `RateLimited` | `rate_limited` | yes | throttle, back off |
| `Timeout` | `timeout` | yes | deadline exceeded |
| `Storage` | `storage_error` | yes | database or mail store failure — **the message is not at fault** |
| `Internal` | `internal_error` | yes | a bug; retrying is the conservative choice, and the failure is logged loudly |
| `Config` | `config_error` | no | unusable configuration — never a per-message condition |
| `Parse` | `parse_error` | no | malformed input |
| `NotFound` | `not_found` | no | referenced entity does not exist |
| `Conflict` | `conflict` | no | uniqueness or state violation |
| `Invalid` | `invalid_input` | no | validation failed |
| `Unauthorized` | `unauthorized` | no | credentials missing or wrong |
| `Forbidden` | `forbidden` | no | policy refusal, e.g. relaying denied |
| `LimitExceeded` | `limit_exceeded` | no | size, recipients or quota |
| `Protocol` | `protocol_error` | no | peer violated the protocol |
| `Tls` | `tls_error` | no | certificate validation failed |
| `Unsupported` | `unsupported` | no | specified but not implemented |

The rule in one sentence: **`is_temporary() == true` ⇒ `4xx` and requeue;
`is_temporary() == false` ⇒ `5xx` and fail.** A protocol layer that classifies
errors itself by string-matching a message is a bug; add a variant or fix
`is_temporary()`.

### 12.1 Inbound: what Ferroma replies to a peer

| Condition | Reply | Class |
|---|---|---|
| Message stored | `250 2.0.0 Ok: queued as <id>` | 2xx |
| `DATA` body exceeds `limits.max_message_size` | `552 5.3.4 Message size exceeds fixed maximum message size` | 5xx, permanent — retrying will not shrink it |
| Recipient count over `limits.max_recipients` | `452 4.5.3 Too many recipients` | 4xx — RFC 5321 §4.5.3.1.10 makes this a transient reply |
| Unknown local domain | `550 5.1.2 Relay access denied` | 5xx |
| Unknown local address | `550 5.1.1 No such user here` | 5xx |
| Relay to a non-local domain, unauthenticated | `550 5.7.1 Relaying denied` | 5xx |
| Mailbox over quota | `452 4.2.2 Mailbox full` | 4xx — the user can free space |
| Database or Maildir write failure | `451 4.3.0 Temporary local problem` | 4xx — `FerromaError::Storage`, retry |
| Disk full on the mail root | `452 4.3.1 Insufficient system storage` | 4xx |
| Internal error while storing | `451 4.3.0 Temporary local problem` | 4xx — `FerromaError::Internal` |
| Rate limit exceeded | `421 4.7.0 Too many commands, slow down` | 4xx, then close |
| SPF hard fail (`-all`) | no rejection on its own — the verdict goes into `Authentication-Results` and feeds DMARC | — |
| DMARC failure with `p=reject` | `550 5.7.1 Message rejected by the DMARC policy of <domain>` | 5xx |
| DKIM verification failure | no rejection on its own — the verdict feeds DMARC alignment | — |

For unauthenticated inbound mail, all previously accepted local recipients
share one DATA acceptance decision: Ferroma stages their Maildir bodies, then
commits every message row, recipient row, quota update and sync change in a
single PostgreSQL transaction. If any mailbox is full, DATA returns `452` and
no recipient receives a partial copy; a storage failure likewise rolls the
whole transaction back and returns a temporary error. This matters because
SMTP provides only one reply after DATA: acknowledging one successful mailbox
with `250` while silently losing another previously accepted RCPT is unsafe.
Authenticated outbound submission still has a separate per-recipient queueing
path; this atomic inbound guarantee does not extend to it yet.

Two corrections to the intuition above, both deliberate:

* **Over-quota is `452`, not `550`.** A full mailbox is a temporary condition of
  the *recipient*, not a permanent property of the address. `550` would cause the
  sender's MTA to bounce immediately, and the user would lose the mail that
  arrived while they were over quota. RFC 3463 codes `4.2.2` as "mailbox full"
  for exactly this reason. The storage layer's `StorageError::QuotaExceeded`
  therefore maps to the temporary class only in the SMTP reply — it remains
  `is_temporary() == false` at the queue level, where the fault is genuinely the
  message's.
* **`451 4.3.0` for any storage failure**, including a database outage. A remote
  server that gets `451` will retry for days; a `550` would bounce the mail
  permanently because our disk was full for ten seconds.

### 12.2 Outbound: what a remote reply means for the queue

| Remote reply | Queue decision | Rationale |
|---|---|---|
| `2xx` after the final dot | `delivered`, write `delivered_at`, `delivery_attempts` | done |
| `4xx` at any point | `retry`, `next_attempt_at = now + backoff_for_attempt(attempts)`, record `last_status_code`/`last_status_text` | the remote asked us to come back |
| `5xx` at any point | `failed` immediately, no further attempts, bounce if `queue.bounce_on_failure` | the remote refused permanently; retrying is abuse |
| Connection refused / timeout / reset | `retry` (`FerromaError::Network` / `Timeout`) | try the next MX, then back off |
| TLS handshake failure | `retry` | often a certificate rotation on the far side |
| MX lookup failure | `retry` (`FerromaError::Dns`) | DNS glitches are transient |
| All MX hosts tried and all failed with 4xx | `retry` | one attempt covers every host; the backoff applies to the whole domain |
| `attempts` reaches `max_attempts` | `failed`, bounce if `queue.bounce_on_failure` | the retry window has elapsed |
| Remote `5xx` on one recipient of a multi-recipient message | that `mail_queue` row fails; the others are unaffected | one row per recipient exists precisely so this is per-recipient |

### 12.3 Enhanced status codes

`ENHANCEDSTATUSCODES` (RFC 3463) is advertised, so every reply carries a
`x.y.z` class after the basic code. The classes Ferroma emits:

| Enhanced code | Meaning | Where it comes from |
|---|---|---|
| `2.0.0` | other/undefined status, success | message accepted |
| `2.1.0` | sender ok | `MAIL FROM` accepted |
| `2.1.5` | recipient ok | `RCPT TO` accepted |
| `2.5.2` | cannot `VRFY` user | `VRFY` |
| `2.7.0` | security policy ok | `AUTH` succeeded |
| `4.2.2` | mailbox full | quota exceeded |
| `4.3.0` | other mail system status | local storage/database failure |
| `4.3.1` | mail system full | disk full |
| `4.3.2` | system not accepting network messages | connection caps |
| `4.4.2` | bad connection | timeout |
| `4.5.3` | too many recipients | `limits.max_recipients` |
| `4.7.0` | security policy, temporary | rate limits, throttled AUTH |
| `5.1.1` | bad destination mailbox address | unknown local address |
| `5.1.2` | bad destination system address | unknown local domain |
| `5.3.4` | message too big for system | size limit |
| `5.5.1` | invalid command | sequence violation |
| `5.5.2` | syntax error | unparseable command line |
| `5.5.4` | invalid command arguments | bad base64, bad parameter |
| `5.7.0` | security policy, permanent | authentication required |
| `5.7.1` | delivery not authorised | relaying denied, DMARC policy refusal |
| `5.7.8` | authentication credentials invalid | bad password |
| `5.7.11` | encryption required | cleartext `AUTH` refused |

When `smtp.advertise_extensions = false` the enhanced code is omitted and the
reply is the bare three-digit code plus the class-less text.

### 12.4 DSN

`DSN` (RFC 3461) is **not** implemented: it is not advertised, and the parameters
on `MAIL FROM` (`RET=FULL|HDRS`, `ENVID=`) and `RCPT TO` (`NOTIFY=`, `ORCPT=`)
are ignored, which is the behaviour RFC 5321 §4.1.1.11 requires of a server that
does not support them.

### 12.5 The complete inbound reply table

| Reply | Text | Trigger |
|---|---|---|
| `220` | `<smtp.banner>` | connection accepted |
| `220` | `2.0.0 Ready to start TLS` | `STARTTLS` |
| `221` | `2.0.0 Bye` | `QUIT` |
| `235` | `2.7.0 Authentication successful` | `AUTH` |
| `250` | `2.0.0 Ok` | `EHLO`/`HELO` final line, `NOOP`, `RSET` |
| `250` | `2.1.0 Ok` | `MAIL FROM` |
| `250` | `2.1.5 Ok` | `RCPT TO` |
| `250` | `2.0.0 Ok: queued as <id>` | end of `DATA` |
| `252` | `2.5.2 Cannot VRFY user, but will accept message and attempt delivery` | `VRFY` |
| `334` | `<base64 prompt>` | `AUTH` continuation |
| `354` | `End data with <CR><LF>.<CR><LF>` | `DATA` |
| `421` | `4.3.2 Too many connections, try again later` | `limits.max_connections` |
| `421` | `4.3.2 Too many connections from your address` | `limits.max_connections_per_ip` |
| `421` | `4.4.2 Timeout waiting for command` | `smtp.command_timeout_secs` |
| `421` | `4.4.2 Timeout waiting for data` | `smtp.data_timeout_secs` |
| `421` | `4.7.0 Too many commands, slow down` | `limits.smtp_rate_limit` |
| `450` | `4.2.0 Mailbox busy, try again later` | not implemented — lock contention surfaces as a storage failure `451 4.3.0` instead |
| `451` | `4.3.0 Temporary local problem` | `FerromaError::Storage` / `Internal` |
| `452` | `4.2.2 Mailbox full` | quota |
| `452` | `4.3.1 Insufficient system storage` | disk full |
| `452` | `4.5.3 Too many recipients` | `limits.max_recipients` |
| `452` | `4.7.0 Submission rate limit exceeded` | `limits.submission_rate_limit` |
| `452` | `4.7.0 Daily send limit exceeded` | `limits.daily_send_limit` |
| `454` | `4.7.0 Temporary authentication failure` | lockout, or per-IP failure flood |
| `500` | `5.5.2 Command unrecognized` | unknown verb |
| `500` | `5.5.2 Line too long` | command over 512 octets |
| `501` | `5.5.4 Invalid base64 data` | malformed SASL blob |
| `501` | `5.5.4 Syntax error in parameters` | unparseable `MAIL`/`RCPT` |
| `502` | `5.5.1 Command not implemented` | `EXPN`, `BDAT`, `STARTTLS` without TLS |
| `503` | `5.5.1 Send HELO/EHLO first` | `helo_required` |
| `503` | `5.5.1 Need MAIL FROM before RCPT TO` | sequence |
| `503` | `5.5.1 Need RCPT TO before DATA` | sequence |
| `503` | `5.5.1 Sender already specified` | second `MAIL FROM` |
| `503` | `5.5.1 Already authenticated` | second `AUTH` |
| `503` | `5.5.1 TLS already active` | `STARTTLS` twice |
| `504` | `5.5.4 Unrecognized authentication type` | `AUTH CRAM-MD5` |
| `530` | `5.7.0 Authentication required` | `require_auth_on_submission` |
| `535` | `5.7.8 Authentication credentials invalid` | bad credentials |
| `538` | `5.7.11 Encryption required for requested authentication mechanism` | `require_tls_for_auth` |
| `550` | `5.1.1 No such user here` | unknown local address |
| `550` | `5.1.2 Relay access denied` | unknown local domain |
| `550` | `5.7.1 Relaying denied` | unauthenticated relay attempt |
| `550` | `5.7.1 Message rejected by the DMARC policy of <domain>` | DMARC, effective policy `reject` — SPF and DKIM verdicts feed DMARC instead of rejecting on their own |
| `552` | `5.3.4 Message size exceeds fixed maximum message size` | size limit |
| `552` | `5.3.4 <reason>` (`LimitExceeded`) | MIME depth / part budget over the limits |
| `554` | `5.5.1 Pipelining violated` | command pipelined across `STARTTLS` |
| `554` | `5.7.1 <reason>` (`Forbidden`) | delivery policy refusal |

### 12.6 Greylisting

`[policy.greylist]` is **off by default**. When it is on, an unauthenticated peer
whose `(address, sender, recipient)` triplet has never been seen is deferred once
with `451 4.7.1`. A real MTA queues the message and comes back; most bulk senders
do not, which is the whole value. The delay is measured from the **first**
sighting and that timestamp is never moved, so a peer that retries early cannot
push its own deadline forward and wait forever.

What is never deferred:

| Peer | Why |
|---|---|
| an authenticated session | it is a known user, not an unknown peer |
| a null reverse-path (`<>`) | a bounce has no address to retry from, so deferring it discards the report |
| an address or block in `whitelist` | a relay or partner that queues nothing would otherwise eat one deferral per new triplet |

The check runs at `RCPT TO`, and only after the address resolves to a real
mailbox: deferring mail to an address that does not exist is a delay with nothing
behind it. It **fails open** — a database error accepts the message and logs a
warning. Greylisting exists to filter bulk senders, never to become the reason a
peer cannot deliver mail.

Triplets are pruned by `ferroma storage gc` after `retention_days` (default 30).
Forgetting one costs a single extra deferral, never a message.

---

## 13. Bounce generation

When a delivery ends as `failed` and `queue.bounce_on_failure = true`, Ferroma
sends a Delivery Status Notification back to the envelope sender. Specification
§11 does not spell out the format, so this section is the specification.

### 13.1 Who gets bounced to

| Envelope sender | Behaviour |
|---|---|
| A local address that exists | bounce delivered into that mailbox's `INBOX`, with `From: MAILER-DAEMON@<server.hostname>` |
| A local address that no longer exists | bounce discarded, logged at `warn` |
| A remote address | a new `mail_queue` row with a null sender (`MAIL FROM:<>`), subject to the same retry policy; a bounce that itself bounces is dropped |
| Empty (`<>`) | never bounce — this is already a bounce, and bouncing it is how mail loops start |

The null-sender rule matters: RFC 5321 §6.1 and RFC 3464 both require that a
notification have a null reverse-path, and a server that bounces a bounce will
happily generate an infinite loop between two misconfigured MTAs.

### 13.2 The bounce is a DSN

`multipart/report; report-type=delivery-status` (RFC 3462/3464), containing:

| Part | Content type | Content |
|---|---|---|
| 1 | `text/plain; charset=utf-8` | human-readable explanation: which recipient, why, and how long it was tried |
| 2 | `message/delivery-status` | per-message fields (`Reporting-MTA`, `Arrival-Date`) and per-recipient fields (`Final-Recipient`, `Action: failed`, `Status: 5.1.1`, `Diagnostic-Code: smtp; 550 5.1.1 No such user here`) |
| 3 | `message/rfc822` (or `text/rfc822-headers`) | the original message's headers, or the whole message when it is small |

Built with `ferroma_mail::MessageBuilder`; the original bytes come from the
Maildir by `messages.storage_path`. The `Status:` field carries the *enhanced*
code from §12.3, so the sender's client can act on the class rather than the
prose.

### 13.3 When a bounce is generated

| Condition | Bounce? |
|---|---|
| Remote `5xx` on the final dot | yes, immediately |
| Remote `5xx` on `RCPT TO` | yes, for that recipient |
| `attempts` reached `queue.max_attempts` | yes |
| Null MX | yes |
| Recipient is a local address that does not exist | generated at `RCPT TO` time, not by the queue |
| `queue.bounce_on_failure = false` | no bounce; the failure is recorded in `mail_queue` and `delivery_attempts` and shown in Admin |
| The message came from a local submission and the sender is still connected | the submission already returned `250`; the bounce is the only feedback path |

The bounce carries the original `Message-ID` in `In-Reply-To` and `References`, so
a client can thread "Undelivered Mail Returned to Sender" with the message the
user actually sent.

---

## 14. Diagnosing SMTP by hand

Specification §44 names the tools. All of these work against a locally running
server; port 25 is the inbound listener, 587 the submission listener.

```bash
# Greeting, capabilities and a full transaction, unencrypted.
swaks --server 127.0.0.1 --port 25 --from bob@example.net --to alice@example.com --body "test"

# The same, forcing STARTTLS.
swaks --server 127.0.0.1 --port 587 --tls --auth PLAIN --auth-user alice@example.com --auth-password '…'

# By hand: type EHLO, MAIL FROM, RCPT TO, DATA.
nc 127.0.0.1 25

# Which capabilities does the submission port advertise, and does it offer STARTTLS?
openssl s_client -starttls smtp -connect 127.0.0.1:587 -crlf

# Implicit TLS on 465.
openssl s_client -connect 127.0.0.1:465

# Is the MX record the one Ferroma will use?
dig +short MX example.com
```

Illustrative output of the greeting and capability exchange:

```text
220 mail.example.com Ferroma ESMTP ready
EHLO client.example.net
250-mail.example.com
250-PIPELINING
250-SIZE 26214400
250-8BITMIME
250-ENHANCEDSTATUSCODES
250-SMTPUTF8
250-STARTTLS
250-AUTH PLAIN LOGIN
250 HELP
MAIL FROM:<bob@example.net>
250 2.1.0 Ok
RCPT TO:<alice@example.com>
250 2.1.5 Ok
DATA
354 End data with <CR><LF>.<CR><LF>
Subject: test

hello
.
250 2.0.0 Ok: queued as 4821
QUIT
221 2.0.0 Bye
```

For "mail is not arriving" and "the queue is growing", see the symptom-keyed
command list in [deployment.md](deployment.md) §11.

---

## 15. Related documents

| Topic | Document |
|---|---|
| Ports, DNS records, TLS termination, first-run setup | [deployment.md](deployment.md) |
| SPF, DKIM, DMARC, HTML sanitisation, relay defence rationale | [security.md](security.md) |
| Where the bytes end up, and the `messages` / `mail_queue` schema | [storage.md](storage.md) |
| Retry state machine, Outbox | [sync.md](sync.md) |
| The API that queues mail: `POST /api/v1/messages` | [api.md](api.md) §5.2 |
| Crate layering and the request lifecycle | [architecture.md](architecture.md) |
