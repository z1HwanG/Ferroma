# Security

**Who should read this:** anyone reviewing Ferroma before exposing it to the
internet, anyone changing authentication, TLS, mail policy or the storage layer,
and any operator who needs to know what a Ferroma deployment does and does not
defend against.

This document is the threat model and the control inventory. It states the
assumptions, then walks each control — password hashing, tokens and sessions,
login throttling, relay prevention, sender and recipient validation, SPF/DKIM/
DMARC, TLS, rate and size limits, HTML handling, path traversal, log hygiene and
secrets management — with the real type, function and config key that implements
it. It ends with a table mapping every control to its implementation site, and a
"known gaps" section listing what Ferroma v1 deliberately does not defend
against.

> **Status:** implemented. Everything described here — passwords, tokens and
> sessions, login throttling, relay prevention, sender and recipient validation,
> SPF/DKIM/DMARC, TLS, rate and size limits, HTML sanitisation, path traversal
> guards and log hygiene — exists in `ferroma-core`, `ferroma-auth`,
> `ferroma-storage`, `ferroma-mail`, `ferroma-smtp` and `ferroma-api`, and is
> exercised by `cargo test --workspace` and the acceptance suite. The controls
> listed in [../AGENTS.md](../AGENTS.md) §1 that Ferroma v1 deliberately does
> **not** have are stated in §14, not marked as pending here.

---

## 1. Threat model

### 1.1 What Ferroma is protecting

| Asset | Where it lives | Consequence of compromise |
|---|---|---|
| Mail content | Maildir under `storage.maildir_root`, blobs under `storage.attachment_root` | full read of every user's correspondence |
| Credentials | `users.password_hash` (Argon2id PHC strings) | offline crack, then account takeover |
| Sessions and tokens | `sessions.token_hash`, in-memory access tokens | live account takeover without the password |
| DKIM private keys | `domains.dkim_private_key`, `[dkim] private_key_path` | forge signed mail as the operator's domains |
| TLS private key | `tls.key_path` | impersonate the server, decrypt recorded traffic |
| `api.jwt_secret` | environment (`FERROMA_JWT_SECRET`) | mint valid access tokens for any user |
| The server's sending reputation | the IP address and the domains | the machine becomes a spam source and gets blocklisted |

### 1.2 Who the adversaries are

| Adversary | Capability | Primary controls |
|---|---|---|
| **A remote SMTP peer** | sends arbitrary commands, `MAIL FROM`, recipients, `DATA`, MIME | relay policy (§5), sender/recipient validation (§6), size and rate limits (§11), parse limits (§10) |
| **A remote IMAP peer** | arbitrary commands, literals, huge sequence sets | authentication (§2–§4), `imap.require_tls_for_login`, `limits.max_fetch_messages`, `imap.max_append_size` |
| **An unauthenticated HTTP client** | arbitrary JSON bodies, headers, URLs | token verification (§3), error envelope that leaks nothing ([api.md](api.md) §1.3), `api.trust_proxy_headers` off by default |
| **A legitimate user abusing their account** | can authenticate, send, store | per-account rate limits (`submission_rate_limit`, `daily_send_limit`), quota, `mailboxes.user_id` ownership checks on `From` |
| **A compromised client device** | holds a refresh token | rotation-based theft detection (§3), device revocation (§4), token hashing at rest |
| **A local attacker with filesystem access** | reads files | password hashes are Argon2id, tokens are only stored hashed, no plaintext secrets in the mail store |
| **A local attacker with database access** | reads and writes rows | path-traversal gates (§12) mean a compromised `storage_path` still cannot escape the mail root |
| **A malformed-message author** | deeply nested MIME, enormous headers, bad encodings | `ParseLimits` (20 levels, 200 parts, `limits.max_message_size`), total parsing that never panics |

### 1.3 Explicit non-assumptions

* **The network is hostile.** SMTP and IMAP on the public internet are plaintext
  by default and TLS is opportunistic, so message content is assumed readable by
  a passive observer on port 25. TLS on submission (587) and IMAP (993) is what
  protects credentials and content that matter.
* **`Received:`, `From:`, `HELO` and every display name are attacker-controlled.**
  They are parsed, stored and displayed, never used to make an authorisation
  decision.
* **The database is trusted; the filesystem paths in it are not.** Both stores
  re-validate every relative path they are handed — §12.
* **One process, one host.** Multi-node deployments are not supported and the
  event bus does not cross processes ([architecture.md](architecture.md) §6).

---

## 2. Passwords

### 2.1 Argon2id parameters

`crates/ferroma-auth/src/password.rs`:

```rust
impl Default for Argon2Params {
    fn default() -> Self {
        // OWASP 2024: m=19456 KiB (19 MiB), t=2, p=1.
        Argon2Params { memory_kib: 19_456, iterations: 2, parallelism: 1 }
    }
}
```

The stored form is a PHC string, which is why the parameters travel with the hash:

```text
$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>
```

| Parameter | Value | Why |
|---|---|---|
| Algorithm | Argon2id (`Algorithm::Argon2id`) | the hybrid: data-independent first pass resists side-channel attacks, data-dependent later passes resist time-memory tradeoffs. It is the OWASP first choice for new applications |
| Memory | 19 456 KiB (19 MiB) | memory-hardness is what makes GPU and ASIC cracking expensive. 19 MiB is OWASP's 2024 recommendation and fits comfortably in a per-connection budget |
| Iterations | 2 | the second OWASP-recommended axis; with 19 MiB, more passes buy less than more memory |
| Parallelism | 1 | one lane per hash, so a login flood cannot be amplified by the device's core count |
| Salt | 16 random bytes from `OsRng` (`SaltString::generate`) | unique per password, so a rainbow table is useless and two users with the same password have different hashes |
| Version | `Version::V0x13` | current Argon2 version |

The requirement this satisfies is *offline* resistance: `users.password_hash` may
end up in a backup, a database dump or a SQL injection result, and 19 MiB × 2
passes makes a dictionary attack cost real money per guess.

### 2.2 Verification

```rust
pub fn verify(&self, password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}
```

Two deliberate properties:

* **A wrong password and an unparseable stored hash both return `false`.** A
  corrupted row cannot be probed for information by comparing error messages.
* **Verification is constant-time with respect to the hash**, which is the
  `argon2` crate's guarantee. The comparison of the derived key uses the
  constant-time `subtle` path inside the crate.

### 2.3 Transparent rehashing

`PasswordHasher::needs_rehash(stored)` compares the parameters recorded in the PHC
string with the current ones, and `AuthService::login` re-hashes the password
while it holds the plaintext:

```rust
if self.hasher.needs_rehash(&user.password_hash) {
    match self.hash_password(password).await { … }
}
```

A failed upgrade is logged at `warn` and **does not fail the login**. So the
parameter set can be raised later and the whole database upgrades itself over
time, without a migration and without locking anyone out. `Argon2Params::stored_params`
reads the parameters out of an existing hash, for auditing.

### 2.4 Password policy

`validate_password` enforces the length bounds, and `strength(password) -> u8`
provides a score for the UI:

| Constant | Value |
|---|---|
| `MIN_PASSWORD_LENGTH` | 8 |
| `MAX_PASSWORD_LENGTH` | 1024 — Argon2 itself has no limit; this bounds request size |

There is **no** composition rule (no "must contain a digit") because length is
the property that matters and composition rules push users toward
`Password1!`. There is **no** breach-corpus check — see §14.

`AuthService::change_password` revokes every other session: changing a password
is the gesture a user makes when they suspect compromise, and leaving other
sessions alive would defeat it.

### 2.5 Login failure accounting

| Column / config | Effect |
|---|---|
| `users.failed_logins` | consecutive failures, reset by `record_login_success` |
| `users.locked_until` | set by `record_login_failure(user_id, now, lockout_secs, max_failed_logins)` |
| `limits.max_failed_logins` | 10 — the threshold |
| `limits.login_lockout_secs` | 900 — 15 minutes |

---

## 3. Access tokens, refresh tokens and rotation-based theft detection

### 3.1 Two token types

| | Access token | Refresh token |
|---|---|---|
| Format | JWT, HS256 | opaque: `rt_` + base64url(32 random bytes) |
| Constant | — | `REFRESH_PREFIX = "rt_"`, `OPAQUE_TOKEN_BYTES = 32` |
| Lifetime | `api.access_token_ttl_secs`, 3600 s | `api.refresh_token_ttl_secs`, 2 592 000 s (30 days) |
| Stored server-side? | **no** — stateless, verified by signature | **yes, hashed**: `sessions.token_hash` |
| Sent | `Authorization: Bearer …` on every request | only to `/auth/refresh` |
| Revocable | not directly; revoked by revoking the session or rotating the secret | yes, `sessions.revoked_at` |

Why both: a stateless access token keeps the hot path free of a database round
trip, and a short lifetime bounds the damage of a leaked one. An opaque refresh
token is one the server can actually revoke, which a JWT fundamentally is not.

### 3.2 The access token's claims and their checks

`Claims` / `AccessClaims` in `crates/ferroma-auth/src/token.rs`:

```json
{ "sub": "7", "sid": 12, "typ": "access", "iss": "mail.example.com",
  "iat": 1789000000, "exp": 1789003600, "jti": "9f2c41…" }
```

`TokenService::verify_access_at` checks, in this order:

| Check | Failure |
|---|---|
| exactly three dot-separated parts | `unauthorized("malformed token")` |
| header decodes and parses | `unauthorized("malformed token header")` |
| **`alg == "HS256"`, exactly** | `unauthorized("unsupported token algorithm")` — this is the `alg: none` / algorithm-confusion defence; there is no negotiation |
| signature verifies, **before** the payload is parsed | `unauthorized("invalid token signature")` |
| `typ == "access"` | `unauthorized("not an access token")` — a refresh token cannot be used as an access token |
| `iss == self.issuer` | `unauthorized("token issued for a different server")` |
| `exp > now` | `unauthorized("token expired")` |
| `iat <= now + 5 minutes` | `unauthorized("token issued in the future")` — a forged or clock-broken claim is refused rather than trusted indefinitely |

The ordering is the point: **the signature is verified before any
attacker-controlled claim is deserialised into a struct**, and the HMAC
comparison is constant-time (`mac.verify_slice`, backed by `subtle`). There is no
`alg`-driven dispatch to confuse.

`jti` is a per-token UUID, so an individual token can be identified in a log
without logging the token.

### 3.3 Rotation-based theft detection

Every refresh rotates: the presented session is revoked and a replacement of the
same kind is opened.

```rust
// crates/ferroma-auth/src/service.rs
if session.revoked_at.is_some() {
    // Reuse of a revoked token: assume compromise and burn the family.
    let revoked = self.repos.sessions.revoke_all_for_user(user_id).await?;
    tracing::warn!(user_id = …, session_id = session.id, revoked,
                   "revoked refresh token reused; all sessions revoked");
    return Err(FerromaError::Unauthorized(
        "refresh token was already used; all sessions have been revoked".into(),
    ));
}
```

The reasoning: an honest client presents a refresh token exactly once and throws
it away. If a token is presented twice, either the client is buggy or two parties
hold it — and the server cannot tell which. Revoking the whole family is the
conservative answer, and it is the standard refresh-token-rotation pattern.

```text
  login      ──► refresh_1 (stored as a hash in sessions)
  refresh    ──► refresh_1 revoked, refresh_2 issued
  refresh_2  ──► fine
  attacker presents refresh_1
             ──► "already used" ⇒ revoke every session of the user
             ──► the legitimate client's refresh_2 stops working too
             ──► the user is forced to log in again, and the theft is visible in the log
```

`AuthService::refresh` also refuses an expired session
(`unauthorized("refresh token expired")`) and a disabled account
(`unauthorized("account disabled")`).

### 3.4 Why the hash, not the token

`sessions.token_hash` holds `hash_token(raw)` — SHA-256, lower-case hex, from
`crates/ferroma-auth/src/token.rs`:

> SHA-256 of a raw token, lower-case hex. The only form ever written to the database.

A plain SHA-256 rather than Argon2 is correct here and not a shortcut: the input
is 32 bytes of CSPRNG output, so there is no dictionary to attack and no
work-factor to add. What matters is that a database dump does not contain a
directly usable refresh token.

`looks_like_opaque_token(value)` exists so log scrubbing can recognise a token by
its prefix (`rt_` or `st_`) without knowing its value.

### 3.5 `api.jwt_secret`

`TokenService::from_config` uses `api.jwt_secret` when it is set and non-blank.
When it is not:

```rust
tracing::warn!(
    "api.jwt_secret is not configured: generating an ephemeral secret. \
     Every restart will invalidate all sessions. Set FERROMA_JWT_SECRET in production."
);
```

and `has_ephemeral_secret()` returns `true` so a caller can refuse to serve in
that state. `docker-compose.yml` and `docker-compose.prod.yml` both require it
(`${FERROMA_JWT_SECRET:?set FERROMA_JWT_SECRET in .env}`), and `.env.example`
gives the generation command:

```bash
openssl rand -base64 48
```

HS256 is a symmetric signature, so the secret is as sensitive as every session it
signs. See §13.

---

## 4. Sessions and device revocation

### 4.1 The `sessions` row

`kind` is one of `web`, `api`, `client`, `imap`, `smtp` (enforced by
`sessions_kind_known`), so an IMAP session and a Webmail session are
distinguishable and independently revocable. `SessionKind::is_refreshable()` marks
the kinds that may use `/auth/refresh`.

Lifecycle:

| Operation | Method | Effect |
|---|---|---|
| Open | `AuthService::open_session(user, kind, device_id, ip, user_agent)` | inserts the row, returns `(Session, TokenPair)` |
| Authenticate | `AuthService::authenticate(bearer)` | verifies the access token, then requires the session to exist and not be revoked |
| Revoke one | `AuthService::logout(session_id)` | sets `revoked_at` |
| Revoke all for a user | `AuthService::logout_all(user_id)` / `SessionsRepository::revoke_all_for_user` | an administrative lockout |
| Purge | `AuthService::purge_expired_sessions()` | deletes rows past `expires_at`, indexed by `sessions_expiry_idx … WHERE revoked_at IS NULL` |

**A valid signature is not enough.** `authenticate` checks the session row after
verifying the token, which is what makes revocation take effect immediately
rather than when the access token expires. `sessions_expiry_idx` is partial on
`revoked_at IS NULL` so the sweeper does not scan dead rows.

### 4.2 Devices

`devices` is per installation, keyed `(user_id, device_uid)` where `device_uid`
is client-generated and stable. `device_uid` is validated: non-empty and at most
128 characters (`AuthService::register_device`).

Revoking a device is the remote-wipe gesture from specification §33:

```rust
pub async fn revoke_device(&self, device_id: DeviceId) -> Result<u64>
```

1. marks the device revoked (`devices.revoked_at`),
2. revokes every session belonging to it,
3. publishes `Event::device_revoked(device_id, user_id)` so a live WebSocket for
   that device disconnects ([fcp.md](fcp.md) §9).

The revoked client's next request is `401`. Revoking the device you are calling
from is allowed and takes effect immediately — a user who has lost a laptop must
be able to cut it off from any other device, including one that looks like it.

### 4.3 Idle sessions

`client.session_idle_days` (90) is the policy for how long a device session may go
unused before it is revoked. It is enforced by a periodic sweep, not by the
`expires_at` column: the refresh token's own `api.refresh_token_ttl_secs` (30
days) is the hard bound, and the idle policy catches a device that refreshes
forever but is never actually used.

---

## 5. Login throttling and lockout

Two independent mechanisms, checked in this order in `AuthService::login`.

### 5.1 Per-source-IP, checked before any expensive work

```rust
if let Some(ref ip_text) = ip_str {
    let failures = self.repos.login_attempts
        .count_failures_for_ip(ip_text, now - self.failure_window).await?;
    if failures >= i64::from(self.limits.max_failed_logins) * 3 {
        tracing::warn!(ip = %ip_text, failures, "login throttled by source address");
        self.record_attempt(&email, ip_str.as_deref(), "password", false).await;
        return Err(FerromaError::RateLimited);
    }
}
```

The threshold is **three times** the per-account threshold, because one NAT or
office egress address can legitimately contain more than ten users. The check
happens **before** the user lookup and before Argon2, so a credential flood
cannot spend 19 MiB and two passes per request from a single source. That
ordering is the whole reason this block is first.

`failure_window` is set by `AuthService::with_failure_window`, so tests can pin it.

### 5.2 Per-account lockout

| Step | Effect |
|---|---|
| Wrong password | `UsersRepository::record_login_failure(user_id, now, lockout_secs, max_failed_logins)` increments `failed_logins` and sets `locked_until` once the threshold is hit |
| Locked | `User::is_login_allowed(now)` is false ⇒ `FerromaError::RateLimited` |
| Success | `record_login_success` clears `failed_logins` and `locked_until`, stamps `last_login_at` |
| Both | a `login_attempts` row is written either way, for the audit trail |

Both are `429 rate_limited` over HTTP ([api.md](api.md) §1.3) and `454 4.7.0`
over SMTP ([smtp.md](smtp.md) §8).

### 5.3 What is deliberately identical

| Situation | Response |
|---|---|
| Unknown account | `invalid_credentials()` |
| Wrong password | `invalid_credentials()` |
| Disabled account | `invalid_credentials()` **and** a `warn` log naming the user id |

The comment in the code is explicit: *"Unknown account: same message and same
cost profile as a wrong password."* An attacker cannot enumerate accounts through
the login endpoint, and each attempt costs one Argon2 verification either way —
which is also why the throttle above has to exist.

The one place the server **does** distinguish is the audit log, where a disabled
account produces `"login refused: account disabled"`. That is for the operator,
not for the caller.

### 5.4 Retention

`login_attempts` grows with every attempt. `login_attempts_created_idx` on
`(created_at)` exists for the retention sweep that deletes by age. Without it the
table is the largest in the schema on a busy server.

---

## 6. Open-relay prevention, sender and recipient validation

### 6.1 Relay policy

Stated in [../AGENTS.md](../AGENTS.md) §4.6 and specification §9.4, and specified
in full in [smtp.md](smtp.md) §6:

```text
  recipient domain is local  →  accept (quota and limits apply)
  recipient domain is remote →  a successful AUTH is required first
```

| Connection | Recipient | Result |
|---|---|---|
| unauthenticated, port 25 | local | accepted, delivered |
| unauthenticated, port 25 | remote | `550 5.7.1 Relaying denied` |
| authenticated | local or remote | accepted, queued if remote |

There is no configuration key that turns relaying on. The safe behaviour is
unreachable-by-typo, which is the difference between a policy and a suggestion.

### 6.2 Recipient validation

`RCPT TO` is resolved through the database, not through a filesystem guess:

1. Domain: `DomainsRepository::find_by_name(normalise_domain(domain))`, and
   `domains.enabled` must be true. Unknown or disabled ⇒ `550 5.1.2`.
2. Local part: `MailboxesRepository::find_by_address(domain, local_part)` —
   `mailboxes_address_key (domain_id, local_part)` is the index. Missing ⇒ try
   `AliasesRepository`, then `domains.catch_all`; still missing ⇒ `550 5.1.1`.
3. `mailboxes.enabled` must be true; a disabled address is `550 5.1.1`.
4. Recipient count against `limits.max_recipients` ⇒ `452 4.5.3` when exceeded.

Both lookups use the lower-cased form (`EmailAddress::to_lowercase`,
`normalise_domain`), and the schema enforces it:
`mailboxes_local_lowercase CHECK (local_part = lower(local_part))`,
`domains_name_lowercase CHECK (name = lower(name))`,
`users_email_lowercase CHECK (email = lower(email))`. Case-insensitivity is a
correctness requirement and a security one: two rows differing only in case would
make "who is this address" ambiguous.

### 6.3 Sender validation and address syntax

`MAIL FROM` is parsed by `ferroma_core::EmailAddress::parse`, which is strict on
purpose. `validate_local_part` and `validate_domain` reject, among others:

| Rejected | Why |
|---|---|
| `alice` (no domain) | not an address |
| `alice@` , `@example.com` | empty halves |
| `alice@@example.com` | two separators |
| `.alice@…` , `alice.@…` , `al.ice..x@…` | invalid dot placement |
| `Alice <alice@example.com>` | a display name is not an address; the parser does not guess |
| `ali ce@…` | whitespace |
| `alice@-example.com` , `alice@example-.com` | a label may not start or end with `-` |
| `alice@example..com` | empty label |
| `alice@[192.0.2.1]` | a domain literal is legal SMTP but never a local mailbox domain, so accepting one would create a mailbox nothing can reach |
| a quoted local part containing `\r`, `\n` or `\0` | header injection |

Length bounds: `MAX_LOCAL_PART_LEN` 64, `MAX_DOMAIN_LABEL_LEN` 63,
`MAX_DOMAIN_LEN` 255.

**A local `From` must be owned by the authenticated user.** An authenticated
submission with a `MAIL FROM` in a local domain may only name a `mailboxes` row
whose `user_id` is the session's; otherwise
`550 5.7.1 Sender address rejected: not owned by user`. Without that check, any
user could send as any other user in the domain, which is the sender-forgery
problem that SPF and DMARC exist to detect at the *receiving* end.

### 6.4 Aliases and catch-all

`aliases.target` is a full address, or a bare local part meaning "same domain".
`domains.catch_all` is a local part that receives mail addressed to a
non-existent mailbox. Both are admin-controlled, never user-controlled, and both
are resolved *after* the direct mailbox lookup so that a catch-all can never
shadow a real address.

The catch-all is a spam-amplification surface by nature: it accepts mail for
arbitrary local parts. It is off by default (`catch_all` is `NULL`) and should be
turned on only with a reason.

---

## 7. TLS

### 7.1 Policy

| Listener | Config | Policy |
|---|---|---|
| SMTP 25 | `smtp.port` | plaintext with `STARTTLS` offered — opportunistic, because refusing plaintext inbound loses mail |
| Submission 587 | `smtp.submission_port` | `STARTTLS`; `smtp.require_auth_on_submission` forces AUTH, `smtp.require_tls_for_auth` forces TLS before AUTH |
| SMTPS 465 | `smtp.smtps_port` | implicit TLS from the first octet |
| IMAP 143 | `imap.port` | plaintext with `STARTTLS` |
| IMAPS 993 | `imap.imaps_port` | implicit TLS |
| HTTPS | `api.tls_port` | `0` means the reverse proxy terminates it |

`tls.enabled` gates all of it. `Config::validate()` refuses to start when
`smtp.smtps_port != 0` or `imap.imaps_port != 0` or `api.tls_port != 0` while
`tls.enabled = false` — a configured TLS port with TLS off is a listener that
would serve plaintext on a port clients believe is encrypted.

### 7.2 rustls only

Every TLS-capable dependency in the workspace is pinned to rustls:
`rustls`, `tokio-rustls`, `rustls-pemfile`, `rustls-pki-types`, `webpki-roots`,
and `reqwest` with `default-features = false, features = ["rustls-tls", …]`.
`AGENTS.md` §1.1 forbids adding a crate that pulls in `native-tls`, `openssl` or
`schannel`, and gives two reasons that agree: the Windows `schannel` stack on this
development host fails with `SEC_E_NO_CREDENTIALS`, and a memory-safe TLS
implementation with an explicit cipher-suite policy is the right choice for a
server that terminates SMTP, IMAP and HTTPS itself.

### 7.3 Certificate handling

| Key | Meaning |
|---|---|
| `tls.cert_path` | PEM bundle: leaf certificate followed by intermediates |
| `tls.key_path` | PEM private key, PKCS#8 or PKCS#1 |
| `tls.self_signed_fallback` | generate a certificate at boot when no PEM is configured |
| `tls.allow_insecure_dev_mode` | required for the fallback |
| `tls.min_version` | `"1.2"` or `"1.3"`; anything else refuses to boot |
| `tls.use_platform_roots` | also trust OS-installed roots for outbound verification |

`self_signed_fallback` is explicitly local-development and CI only. It is
generated with `rcgen`, and it is gated twice: `tls.allow_insecure_dev_mode` must
be true, and `Config::validate()` refuses the combination otherwise:

```text
tls.self_signed_fallback requires tls.allow_insecure_dev_mode = true
```

An MX with a self-signed certificate cannot be validated by any sending server, so
every outbound TLS handshake fails and — worse — the operator may be tempted to
turn verification off somewhere else.

### 7.4 What TLS does and does not buy

* **Submission (587/465) and IMAP (993):** credentials and content are protected
  on the wire. This is the security boundary for user data.
* **Inbound port 25:** opportunistic. A sending MTA that does not do TLS sends
  plaintext, and Ferroma must accept it or lose the mail. Message content on port
  25 should be assumed readable by a network observer. End-to-end encryption is
  the only defence, and Ferroma does not implement it.
* **MTA-STS** raises the bar for senders that honour it; the DNS record and the
  policy file are specified in [deployment.md](deployment.md) §2.

### 7.5 `require_tls_for_auth` and `require_tls_for_login`

Both default to `false` so a bare `cargo run` works without certificates, and
both are set to `true` in `docker-compose.prod.yml`:

```yaml
FERROMA__SMTP__REQUIRE_TLS_FOR_AUTH: 'true'
FERROMA__IMAP__REQUIRE_TLS_FOR_LOGIN: 'true'
```

`AUTH PLAIN`, `AUTH LOGIN` and IMAP `LOGIN` all send the password in base64 or
plaintext. Over an unencrypted socket, a passive observer reads it. A production
deployment that leaves these false is one `tcpdump` away from an account
takeover, and the refusal is explicit rather than silent:
`538 5.7.11 Encryption required for requested authentication mechanism` over SMTP,
`NO [PRIVACYREQUIRED]` over IMAP.

`Config::allows_plaintext_auth()` answers the question for the whole config
without re-deriving it.

---

## 8. SPF, DKIM and DMARC

> Implemented. `ferroma-smtp`'s `spf`, `dkim` and `dmarc` modules
> (`crates/ferroma-smtp/src/spf.rs`, `dkim.rs`, `dmarc.rs`, wired together in
> `inbound.rs` and evaluated after `DATA` in `server.rs`) carry this out. The
> configuration keys live in `[dkim]` and `[policy]` of `config/ferroma.toml`
> and in `DkimConfig` / `PolicyConfig` in `crates/ferroma-core/src/config.rs`.

### 8.1 Inbound

| Check | Config | On failure |
|---|---|---|
| SPF (RFC 7208) | `policy.spf_enabled`, `policy.spf_max_lookups` (10) | no direct rejection — the verdict goes into `Authentication-Results` and feeds DMARC |
| DKIM verification (RFC 6376) | `dkim.verify_inbound` | no direct rejection — the verdict feeds DMARC alignment |
| DMARC (RFC 7489) | `policy.dmarc_enabled`, `policy.dmarc_failure_action` | `policy.dmarc_failure_action` is `none`, `quarantine` or `reject`; the default is `"quarantine"` — file to `Junk`; a published or local `reject` is `550 5.7.1 Message rejected by the DMARC policy of <domain>` |
| `Authentication-Results` | `policy.add_auth_results` | the header is prepended with the verdicts |

`dmarc_failure_action` defaults to `quarantine` rather than `reject` for a
specific reason: a DMARC `p=reject` evaluated against a *forwarded* message —
a mailing list, an alumni forwarder — routinely fails SPF and DKIM and is
legitimate. Quarantine puts it in `Junk` where the user can find it; reject loses
it. An operator who has measured their forwarding tolerance can raise it.

`policy.spf_max_lookups` (10) is the RFC 7208 §4.6.4 limit; it exists because an
SPF record can be built to force an unbounded number of DNS lookups, which is a
denial-of-service vector against both Ferroma and the resolver.

### 8.2 Outbound

| Control | Config |
|---|---|
| Which domains are signed | `dkim.domain` (one domain) or every local domain when unset |
| Selector | `dkim.selector` (default `default`), published at `<selector>._domainkey.<domain>` |
| Signing key | `dkim.private_key_path`, or `domains.dkim_private_key` |
| Canonicalisation | `dkim.canonicalization`, `"relaxed"` or `"simple"` |
| Signed headers | `dkim.headers_to_sign`: `From`, `To`, `Cc`, `Subject`, `Date`, `Message-ID`, `MIME-Version`, `Content-Type`, `Content-Transfer-Encoding`, `Reply-To`, `In-Reply-To`, `References` |

`From` being signed is not optional: a DKIM signature that does not cover `From`
can be replayed with a different sender, which is exactly what DMARC alignment
checks. `headers_to_sign` includes it first, and the list matches the specification
§16 flow (canonicalise → header hash → body hash → sign → `DKIM-Signature`).

The private key must never be logged, exported in an API response beyond the
public record, or included in a backup that is less protected than the database.
`GET /api/v1/domains/:id/dkim` returns only the **public** record:

```json
{ "selector": "default", "record_name": "default._domainkey.example.com", "record_type": "TXT", "record_value": "v=DKIM1; k=rsa; p=MIIBIjANBg…" }
```

### 8.3 DNS security relevant to mail

| Record | Security role |
|---|---|
| **PTR** | a missing or mismatched PTR is the single most common reason a legitimate server is rejected. It is a deliverability control, not an authentication one, which is why it is in [deployment.md](deployment.md) §2 |
| **SPF** | authorises sending hosts; `-all` is the strict form |
| **DKIM** | proves the message was signed by the domain and was not modified |
| **DMARC** | tells receivers what to do when SPF and DKIM both fail *and* how to report it (`rua`) |
| **MTA-STS** | requires TLS for inbound mail to the domain, defeating a downgrade |
| **CAA** | restricts which CAs may issue for the domain |
| **DNSSEC** | not implemented or required by Ferroma; it protects the records above where the zone supports it |

`[dns]` configures the resolver Ferroma itself uses: explicit `resolvers`,
`timeout_secs` (5), `attempts` (3), `cache_ttl_secs` (300),
`negative_ttl_secs` (60), `tcp_fallback`. Use a resolver you trust: an attacker
who controls the resolver can forge the MX record for a recipient domain and
receive the mail you are delivering.

---

## 9. Limits and the request surface

Full per-limit tables are in [smtp.md](smtp.md) §7, [imap.md](imap.md) §10 and
[api.md](api.md) §1.6. Summary of the security-relevant ones:

| Limit | Key | Default | Threat it bounds |
|---|---|---|---|
| Message size | `limits.max_message_size` | 25 MiB | disk exhaustion, memory per connection |
| Recipients per transaction | `limits.max_recipients` | 100 | amplification: one connection, many victims |
| Simultaneous connections | `limits.max_connections` | 100 | resource exhaustion |
| Connections per IP | `limits.max_connections_per_ip` | 10 | a single source monopolising the listener |
| Inbound commands/minute/IP | `limits.smtp_rate_limit` | 100 | command floods |
| Submissions/hour/account | `limits.submission_rate_limit` | 50 | a compromised account as a spam cannon |
| Messages/day/account | `limits.daily_send_limit` | 500 | the same, at a slower tempo |
| Mailbox quota | `limits.mailbox_quota` | 1 GiB | one user filling the disk |
| Failed logins | `limits.max_failed_logins` | 10 | password guessing |
| Lockout window | `limits.login_lockout_secs` | 900 | — |
| MIME nesting depth | `limits.max_mime_depth` | 20 | parser recursion |
| Parts per message | `ParseLimits::max_parts` | 200 | parser fan-out |
| IMAP fetch per command | `limits.max_fetch_messages` | 5000 | one `FETCH 1:*` on a huge folder |
| IMAP `APPEND` literal | `imap.max_append_size` | 25 MiB | — |
| HTTP request body | `api.max_request_size` | 25 MiB | — |
| Authentication commands | `limits.idle_timeout_secs`, `limits.data_timeout_secs` | 300 / 600 | slowloris |

Malformed input must be a **limit** failure, not a panic. `ParseLimits`:

```rust
pub struct ParseLimits { pub max_depth: usize, pub max_parts: usize,
                        pub max_part_size: usize, pub max_message_size: usize }
// defaults: 20 / 200 / 26214400 / 26214400
```

`ParsedMessage::parse_with_limits` returns `FerromaError::LimitExceeded` rather
than recursing without bound, and `ParseLimits::from_limits(&Limits)` derives the
values from the platform configuration so there is one place to change them.
`AGENTS.md` §4.4 forbids `unwrap()` on peer input, and the parsers are the reason.

---

## 10. Message handling: MIME, HTML and escape hatches

### 10.1 Total parsing

> Parsing is *total*: any byte string produces a `ParsedMessage`. The only errors
> are the explicit resource limits in `ParseLimits`, because a mail server that
> rejects a message it cannot render drops real mail.

— `crates/ferroma-mail/src/message.rs`

This is a security property as much as a usability one. A parser that can fail on
a class of input creates a class of mail that is silently dropped, which an
attacker can use to suppress a message (a password reset, say) by appending a
byte sequence. MIME decoding is likewise permissive:
`decode_base64`, `decode_quoted_printable` and `decode_charset` return best-effort
results with explicit errors only for genuinely undecodable input.

### 10.2 HTML sanitisation

Implemented. `sanitize_html` in `crates/ferroma-api/src/routes/mail/store.rs`
runs when an HTML body is stored, gated by `security.sanitize_html` in
`config/ferroma.toml`. The rules:

* **Never render raw `html_body` into a privileged origin.** The Webmail and
  Admin SPAs are served from the same origin as the API, so an unsanitised HTML
  body is a stored XSS against a session that can call the admin API.
* **Render in a sandboxed iframe** with
  `sandbox="allow-popups allow-popups-to-escape-sandbox"` and no
  `allow-scripts`/`allow-same-origin`, and block remote content by default so a
  tracking pixel cannot confirm that a message was read. The frame adds
  `<base target="_blank">`, so following a link opens it in a new tab instead of
  replacing the message; `allow-popups-to-escape-sandbox` is what lets that tab be an
  ordinary page rather than another sandboxed document. The framed message itself still
  cannot run script, read this origin, or navigate its parent.
* **The official client is not a browser.** Its reader should not execute
  anything from a message; HTML rendering goes through the same sanitiser, and
  when the sanitiser is absent it falls back to the text part.
* **Attachments are never rendered inline as HTML.** `Content-Type` is
  attacker-controlled; `text/html` on an attachment is not a reason to run it.
  `attachments.is_inline` and `content_id` are display hints, not trust.

The sanitiser, when it lands, must strip `<script>`, `<style>` with `expression`,
`<iframe>`, `<object>`, `<embed>`, `<form>`, `<base>`, `<meta http-equiv>`, all
`on*` attributes and all `javascript:`/`data:` URLs, and it must run server-side
(the client cannot be trusted to do it and the API feeds several clients).

### 10.3 What Ferroma does not do with message content

* It does not execute anything from a message.
* It does not follow a link from a message, ever — no prefetching, no link
  unfurling, no image proxying that fetches remote content.
* It does not deserialise a message into a type that can construct a path, a SQL
  fragment or a shell argument. Message content reaches the database as bound
  parameters (`sqlx::query_as`, never `sqlx::query!` and never string
  interpolation) and the filesystem only through `sanitize_component` (§12).
* It does not store attachments under a name the sender chose. The blob path is
  the SHA-256 of the content; `attachments.filename` is metadata for display and
  is sanitised before it reaches a `Content-Disposition` header.

---

## 11. Path-traversal defences in the Maildir and the blob store

They exist and they are the reason a compromised database row cannot read
`/etc/passwd`. Full detail in [storage.md](storage.md) §7.

### 11.1 `sanitize_component`

```rust
// crates/ferroma-storage/src/maildir.rs
pub fn sanitize_component(component: &str) -> Result<String> {
    let trimmed = component.trim();
    if trimmed.is_empty() { return Err(StorageError::Invalid("empty path component".into())); }
    if trimmed == "." || trimmed == ".." {
        return Err(StorageError::Invalid(format!("invalid path component: {trimmed}")));
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(StorageError::Invalid(format!("path component contains a separator: {trimmed}")));
    }
    if trimmed.contains(':') {
        return Err(StorageError::Invalid(format!("path component contains a colon: {trimmed}")));
    }
    Ok(trimmed.to_string())
}
```

Applied to the domain, the local part (in `Maildir::mailbox_dir`), the hostname
(in `Maildir::new`) and every segment of a folder name (in
`maildir_folder_name`). A test enumerates the interesting inputs:
`["..", ".", "a/b", "a\\b", "", "  ", "a:b", "x\0y"]` must all be rejected.

The `:` rejection is not cosmetic: it is the Maildir info separator, and on
Windows it introduces an NTFS alternate data stream ([imap.md](imap.md) §10).

### 11.2 `absolute`

Both stores expose one, and it is the only function that turns a stored
`storage_path` into a real path:

```rust
// Maildir::absolute and AttachmentStore::absolute
if candidate.is_absolute() {
    return Err(StorageError::Invalid(format!("storage path must be relative: {relative_path}")));
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
```

Every read, write, delete, `set_flags` and `exists` calls it first. Tested:

```rust
assert!(m.absolute("../../etc/passwd").is_err());
assert!(m.absolute("/etc/passwd").is_err());
assert!(s.absolute("../../secret").is_err());
assert!(!s.exists("../../secret"));
```

`Component::Prefix(_)` is what stops `C:\Windows\...` on Windows from being
treated as relative and joined onto the root.

### 11.3 The residual risk

Both gates validate the *string*. A symlink placed inside the mail root by
another process is not detected, because `std::fs` follows symlinks. Ferroma does
not create symlinks anywhere, and the mail root should be owned by the service
user with no other writer — `Dockerfile` runs as uid 10001 (`ferroma`) and
`chown -R ferroma:ferroma /var/lib/ferroma`. An attacker who can write into the
mail root has already won by other means, but it is worth stating that this
control is about a compromised *database*, not a compromised *host*.

---

## 12. Logging: what is and is never recorded

### 12.1 Never logged

`AGENTS.md` §4.7 and specification §40:

| Never logged | Where the rule is honoured |
|---|---|
| Passwords, in any form | `AuthService` verifies and discards; `PasswordHasher` never returns the plaintext |
| Refresh and session tokens | only `hash_token(raw)` is stored; `looks_like_opaque_token` exists so a token can be recognised for scrubbing |
| Access tokens | the `jti` claim identifies a token without revealing it |
| Private keys | DKIM and TLS keys are read by the signer and the listener |
| **Full message bodies** | `MailReceived` carries `snippet`, not body ([architecture.md](architecture.md) §6) |
| SMTP/IMAP protocol payloads at `info` | protocol debugging is `debug`, which is not a production level |
| Message subjects at rest in logs | `database.log_statements` is `false` by default, with the reason spelled out in the config comment: *"it prints message subjects"* |

`crates/ferroma-core/src/logging.rs` states it in the crate documentation:

> Passwords, AUTH tokens, private keys and full message bodies are never logged.

### 12.2 What is logged

The SMTP session fields from specification §40 are the ones that make an
incident investigable without reading anyone's mail:

```text
connection_id   remote_ip   helo   authenticated_user
sender          recipient   message_id   result   duration
```

`connection_id` is a UUID that also appears in the `Received:` header Ferroma
prepends, so a log line and a header can be joined ([smtp.md](smtp.md) §10) — an
operator can answer "where did this message come from" without opening the
message.

### 12.3 Log configuration

| Key | Default | Security note |
|---|---|---|
| `server.log_level` | `"info"` | `debug`/`trace` on `ferroma_smtp` or `ferroma_imap` logs protocol detail; keep it off in production |
| `server.log_format` | `"text"` | `"json"` for shipping; either way the same fields |
| `database.log_statements` | `false` | **never enable in production** |
| `NOISY_DEFAULTS` | `hyper=warn,h2=warn,sqlx=warn,hickory_resolver=warn,hickory_proto=warn,rustls=warn,tokio_tungstenite=warn` | third-party crates are held at `warn` unless the operator explicitly opts in, so a dependency cannot start printing request data |
| `RUST_LOG` / `FERROMA_LOG_LEVEL` | — | `logging::init_for_tests` reads these; a test run is quiet by default |

`logging::build_filter` keeps the operator's directive and appends the noisy-crate
defaults only for targets the directive does not already mention, so
`sqlx=debug` is honoured rather than overridden.

### 12.4 Audit trail

`audit_logs` is separate from the operational log and is meant to be durable:
`actor_user_id` (FK `ON DELETE SET NULL`, so the row survives the account),
`action`, `target_type`, `target_id`, `ip`, `user_agent`, `details JSONB`,
`created_at`. Admin actions — creating a user, deleting a domain, revoking a
device, changing a setting — belong here, not in a `tracing` line that rotates
away.

---

## 13. Secrets management

| Secret | Where it must live | Where it must not |
|---|---|---|
| `api.jwt_secret` / `FERROMA_JWT_SECRET` | environment, or a secret manager injected as an environment variable; when neither is set, the server generates one into `<data_dir>/jwt_secret` | the config file in version control; `.env` or a `ferroma-data` archive left readable by others — no backup tooling ships, so nothing excludes credentials for you |
| `POSTGRES_PASSWORD` | `.env`, gitignored, or a secret manager | the compose files, which interpolate `${POSTGRES_PASSWORD:?…}` and refuse to start without it |
| DKIM private key | `dkim.private_key_path` on a read-only mount, or `domains.dkim_private_key` | the public `GET /api/v1/domains/:id/dkim` response, which returns only the `p=` public key |
| TLS private key | `tls.key_path`, mounted read-only (`./tls:/etc/ferroma/tls:ro`) | the image |
| User passwords | nowhere, ever | — |

No backup tooling ships any more, so the protection of a backup is entirely the
operator's. That matters because a backup of the data volume is secret-bearing: it
contains the DKIM private key and, when the secret was generated rather than
configured, `<data_dir>/jwt_secret`; the volume's `<data_dir>/database.json`
remembers the database address, including any password carried in the URL. Encrypt
the archive, or restrict it as tightly as the database itself.

Practices the repository already enforces:

* **The single-host compose fails fast on a missing secret.** `${FERROMA_JWT_SECRET:?set FERROMA_JWT_SECRET in .env}` and `${POSTGRES_PASSWORD:?…}` in `docker-compose.yml` mean a deployment with no secret does not start, rather than starting with a default. `docker-compose.prod.yml` deliberately leaves the JWT secret unset: the server generates one into the data volume on first start, which is one reason that volume is secret-bearing.
* **Backups are the operator's, credentials included.** No script excludes `*.env`
  or `credentials*` for you; an archive of `.env` or of the `ferroma-data` volume
  must be encrypted and stored like the secrets it holds.
* **`.env.example` carries placeholders and the generation command**, never a real value.
* **The database container is not published.** `docker-compose.yml` uses `expose: ['5432']` on the internal network, not a host port mapping.
* **Configuration is mounted read-only.** `./config/ferroma.toml:/etc/ferroma/ferroma.toml:ro`.
* **The container runs unprivileged.** `USER ferroma`, uid 10001, with `/var/lib/ferroma` chowned to it.

Rotation, when it is needed:

| Secret | Rotation cost |
|---|---|
| `FERROMA_JWT_SECRET` | every access token is invalidated; clients refresh and continue. Users are not logged out (the refresh token is opaque and unaffected) |
| `POSTGRES_PASSWORD` | update `.env`, restart both services |
| DKIM key | publish the new selector's TXT record first, then switch `dkim.selector`. Do not delete the old record until mail signed with it has aged out (a week is safe; 30 days is safer) |
| TLS certificate | reload; the listener picks up the PEM on start |

---

## 14. Control → implementation map

| # | Control | Implementation | Status |
|---|---|---|---|
| 1 | Argon2id, m=19456 t=2 p=1 | `Argon2Params::default`, `PasswordHasher` — `crates/ferroma-auth/src/password.rs` | implemented |
| 2 | Transparent rehash on login | `PasswordHasher::needs_rehash`, `AuthService::login` | implemented |
| 3 | Password length policy | `validate_password`, `MIN_PASSWORD_LENGTH`, `MAX_PASSWORD_LENGTH` | implemented |
| 4 | Constant-time password verification | `PasswordHasher::verify` (argon2 + `subtle`) | implemented |
| 5 | Access token: HS256, no `alg` negotiation | `TokenService::verify_access_at` | implemented |
| 6 | Signature verified before claims are parsed | `verify_access_at` ordering | implemented |
| 7 | Access-token TTL | `api.access_token_ttl_secs` (3600), `TokenService::access_ttl_secs` | implemented |
| 8 | Refresh-token rotation | `AuthService::refresh` | implemented |
| 9 | Theft detection: reuse revokes the family | `AuthService::refresh` + `revoke_all_for_user` | implemented |
| 10 | Refresh tokens stored hashed only | `sessions.token_hash`, `hash_token` | implemented |
| 11 | Session revocation takes effect immediately | `AuthService::authenticate` checks the session row | implemented |
| 12 | Password change revokes other sessions | `AuthService::change_password` | implemented |
| 13 | Device registration and revocation | `devices`, `AuthService::register_device` / `revoke_device`, `Event::device_revoked` | implemented |
| 14 | Idle-session policy | `client.session_idle_days` (90) | implemented (sweep callable via `AuthService::purge_expired_sessions`, not yet scheduled) |
| 15 | Per-IP login throttle before hashing | `AuthService::login` + `LoginAttemptsRepository::count_failures_for_ip` | implemented |
| 16 | Per-account lockout | `users.failed_logins`, `users.locked_until`, `record_login_failure`, `User::is_login_allowed` | implemented |
| 17 | No account enumeration | identical `invalid_credentials()` for unknown/wrong/disabled | implemented |
| 18 | Login attempt audit trail | `login_attempts`, `AuthService::record_attempt` | implemented |
| 19 | Open-relay prevention | `SmtpSession::may_relay`, `smtp.require_auth_on_submission`, `550 5.7.1 Relaying denied` | implemented |
| 20 | Recipient validation | `MailboxesRepository::find_by_address`, `DomainsRepository`, `AliasesRepository`, `domains.catch_all`, `550 5.1.1 User unknown` | implemented |
| 21 | Sender address syntax validation | `EmailAddress::parse`, `validate_local_part`, `validate_domain` | implemented |
| 22 | Local `From` must be owned by the user | `mailboxes.user_id` check in the API send path (`resolve_sender` in `crates/ferroma-api/src/routes/mail/store.rs`) | implemented (API path); the SMTP submission listener does not re-verify `MAIL FROM` |
| 23 | Case-normalised addresses | schema `CHECK`s + `normalise_domain` + `to_lowercase` | implemented |
| 24 | SPF | `policy.spf_enabled`, `policy.spf_max_lookups`, `spf.rs` | implemented |
| 25 | DKIM verification | `dkim.verify_inbound`, `dkim.rs` (`DkimVerifier`) | implemented |
| 26 | DKIM signing | `[dkim]` block, `DkimConfig`, `DkimSigner` | implemented |
| 27 | DMARC | `policy.dmarc_enabled`, `policy.dmarc_failure_action`, `dmarc.rs` + `inbound.rs` | implemented |
| 28 | `Authentication-Results` | `policy.add_auth_results` | implemented |
| 29 | TLS everywhere it can be terminated | `[tls]`, `tls.min_version`, `smtps_port`, `imaps_port`, `api.tls_port` | implemented |
| 30 | rustls only | workspace `Cargo.toml` pins; `AGENTS.md` §1.1 | implemented |
| 31 | Self-signed cert gated twice | `Config::validate()` + `tls.allow_insecure_dev_mode` | implemented |
| 32 | No cleartext AUTH/LOGIN in production | `smtp.require_tls_for_auth` (`may_auth` → `538 5.7.11`), `imap.require_tls_for_login` | implemented |
| 33 | Message size, recipient, connection and rate limits | `Limits`, `Limits::validate()`, `[limits]`, enforced in the SMTP command loop and the API | implemented |
| 34 | Parse limits, no unbounded recursion | `ParseLimits`, `ParsedMessage::parse_with_limits` | implemented |
| 35 | Total parsing: no message dropped by a parser error | `ferroma-mail` parser design | implemented |
| 36 | HTML sanitisation | `security.sanitize_html`, `sanitize_html` in `ferroma-api` `store.rs` | implemented |
| 37 | Path-traversal defence, mail root | `sanitize_component`, `Maildir::absolute` | implemented |
| 38 | Path-traversal defence, blob store | `AttachmentStore::absolute`, `path_for_digest` validation | implemented |
| 39 | No `unwrap()` on peer input | convention, `AGENTS.md` §4.4 | implemented |
| 40 | Bound SQL parameters only | `sqlx::query_as`, no `sqlx::query!` — `AGENTS.md` §4.3 | implemented |
| 41 | Secrets never logged | `logging.rs` contract, `looks_like_opaque_token` | implemented |
| 42 | `log_statements` off | `database.log_statements = false` | implemented |
| 43 | Noisy dependencies held at `warn` | `NOISY_DEFAULTS`, `build_filter` | implemented |
| 44 | Durable audit trail | `audit_logs`, `AuditRepository` | implemented |
| 45 | Secrets required at boot | compose `${VAR:?}` interpolation | implemented |
| 46 | Container runs unprivileged | `Dockerfile` `USER ferroma`, uid 10001 | implemented |
| 47 | Database not published | `expose`, not `ports`, on the `postgres` service | implemented |
| 48 | `Secure` cookies in production | `api.secure_cookies`, set true by `docker-compose.prod.yml` | implemented (config) |
| 49 | CORS closed by default | `api.cors_origins = []` (same-origin only) | implemented (config) |
| 50 | `X-Forwarded-For` only when trusted | `api.trust_proxy_headers = false` by default | implemented (config) |
| 51 | Unauthenticated HTTP cannot reach data | bearer/cookie on every `/api/v1` route except `/health`, `/version`, `/.well-known/*` | implemented |
| 52 | Admin endpoints require `is_admin` | `Authenticated::is_admin()` check | implemented |

The wiring behind these rows is described in [api.md](api.md) and
[smtp.md](smtp.md).

---

## 15. Known gaps

Deliberate omissions from Ferroma v1. Each is a decision, not an oversight; the
mitigation is what an operator should do instead.

### 15.1 No antivirus or malware scanning

Ferroma does not scan attachments. There is no ClamAV integration, no
`clamd` socket, no `virus_scan` config key. An attachment is stored because it
arrived; whether it is malicious is the recipient's problem.

**Why it is acceptable for v1:** an antivirus engine is a large, stateful
dependency with its own update channel and its own failure modes, and a scanner
that silently stops updating is worse than no scanner because it creates false
confidence. It is on the extensibility list (specification §57,
"病毒扫描").

**Operator mitigation:** run a `clamd` and scan the mail root out of band, or
route inbound through a gateway. Block executable attachment types at the
receiving client, which is where the user actually opens them.

### 15.2 No Bayesian or heuristic spam filter

There is no content classifier, no `X-Spam-Score`, no `spamassassin` integration.
The only inbound filtering is:

* SPF/DKIM/DMARC verdicts — these authenticate the sender, they do not
  classify content;
* `Junk` as a folder with `special_use = \Junk`, and DMARC `quarantine` filing
  into it;
* rate limits and connection limits, which bound volume but not content.

**Why:** a Bayesian filter needs a corpus, per-user training and a tuning loop,
and a badly tuned one produces false positives that lose real mail — which
specification §54 identifies as the risk. Shipping a filter that silently eats
invoices is worse than shipping none.

**Operator mitigation:** put a filtering gateway in front, or use a hosted
filtering service. The `Junk` folder and the `\Junk` special-use marker are
already in the schema (`special_use` `CHECK`), so a filter can be added later
without a migration.

### 15.3 No OIDC, no OAuth2, no 2FA

Specification §15 lists `OAuth2`, `OIDC` and `2FA` under "后续" (later) and §57
repeats them. Ferroma v1 supports password authentication only, over:

* `POST /api/v1/auth/login` and `/api/v1/client/auth/login`,
* SMTP `AUTH PLAIN` / `AUTH LOGIN`,
* IMAP `LOGIN` / `AUTHENTICATE PLAIN` / `AUTHENTICATE LOGIN`.

There is no TOTP, no WebAuthn, no recovery codes, no `mfa_required` flag. A
phished password is a full account takeover, bounded only by the login throttle
and by `limits.max_failed_logins`.

**Operator mitigation:** for the Webmail surface, put an SSO proxy in front that
performs 2FA and passes an authenticated identity; for SMTP/IMAP there is no
honest mitigation other than app passwords managed outside Ferroma. Do not
describe a Ferroma deployment as "2FA-protected".

### 15.4 No cross-process event bus

`EventBus` is one in-process object (`crates/ferroma-events/src/bus.rs`). There is
no Redis, NATS or PostgreSQL-`LISTEN`/`NOTIFY` backend.

Consequences:

| Situation | Result |
|---|---|
| Two `ferroma` processes sharing one database | two independent event streams |
| A WebSocket client on process A | does not receive an event produced by process B |
| An `IDLE`ing IMAP session on process A | does not get pushed a change made through process B |
| Reconnect / next sync | the change **is** delivered, because `change_log` is in the shared database |

So the failure mode is a delayed notification, not lost data — and that is the
property that makes a single-process bus acceptable for v1. But it means
horizontal scaling is **not** a configuration change: running two replicas behind
a load balancer gives users a real-time experience that depends on which replica
they landed on.

**Why:** a broker is another stateful service to operate, secure and monitor, and
the specification's first-version Docker stack (§41) is explicitly Ferroma plus
PostgreSQL, with Redis under "后期".

**Operator mitigation:** run one `ferroma` process. If you need more capacity,
scale the database and the storage first; both are likelier bottlenecks than the
event fan-out.

### 15.5 Smaller gaps, stated plainly

| Gap | Consequence | Mitigation |
|---|---|---|
| No `SEARCH BODY` / `TEXT` ([imap.md](imap.md) §8) | a client searching body text gets no results from the server | search in the client, or the API's header/subject search |
| No S/MIME or PGP | Ferroma cannot encrypt or verify end-to-end signatures | use a client that does |
| No DKIM ARC sealing | a forwarded message loses its authentication | leave forwarding to clients that can seal |
| No Sieve or server-side rules | filtering is client-side only | — |
| No breach-corpus password check | a user may set a known-breached password | enforce at account creation from a list you trust |
| No per-user IP allow-listing for `AUTH` | a stolen password works from anywhere | device revocation, and monitor `sessions.ip` |
| No DMARC aggregate report processing | `rua` reports go unread unless the operator reads them | point `rua` at a mailbox you check |
| No request signing on webhooks | webhooks, when they arrive, are unauthenticated HTTP POSTs | do not enable webhooks on an untrusted network |
| No rate limit on `GET /.well-known/ferroma` | an unauthenticated endpoint can be used for reconnaissance and load | front it with a proxy limit if it matters |
| PTR is not verified on inbound | mail from a host with no PTR is still accepted | SPF/DKIM/DMARC and a gateway |
| No alerting | a growing queue, a full disk or a login flood is visible only to someone looking | monitor the health endpoint and `mail_queue_status_idx` counts — [deployment.md](deployment.md) §10 |

---

## 16. Related documents

| Topic | Document |
|---|---|
| Endpoint-level error mapping and authentication headers | [api.md](api.md) §1 |
| FCP authentication, token rotation from the client's side | [fcp.md](fcp.md) §2, §11 |
| Reply codes, relay policy, TLS port roles, `Received:` header | [smtp.md](smtp.md) |
| `require_tls_for_login`, flag handling, `APPEND` limits | [imap.md](imap.md) |
| Path-traversal functions in full, quota, backup and restore | [storage.md](storage.md) §7, §5, §8 |
| Idempotency, tombstone retention, failure matrix | [sync.md](sync.md) |
| DNS records, TLS termination, secrets in `.env`, hardening checklist | [deployment.md](deployment.md) |
| Crate graph, layering rule, event bus scope | [architecture.md](architecture.md) |
| Build-time rustls constraint and conventions | [../AGENTS.md](../AGENTS.md) |
