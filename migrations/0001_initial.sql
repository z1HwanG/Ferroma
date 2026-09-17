-- =============================================================================
-- Ferroma — initial schema
-- =============================================================================
--
-- Target: PostgreSQL 14+ (uses core `gen_random_uuid()`; no extensions required).
--
-- Design notes, including the two deliberate deviations from the specification's
-- §37 sketch:
--
--  1. `mailboxes` keeps the specification's meaning: an *address* owned by a user
--     in a domain (`alice@example.com`) — the SMTP delivery target, with
--     UNIQUE(domain_id, local_part). IMAP folders live in a separate `folders`
--     table, because the §37 sketch conflated "account address" with "folder"
--     (`messages.mailbox_id` alone cannot express INBOX vs Sent vs Archive).
--     `messages.folder_id` is the authoritative parent; `messages.mailbox_id` is
--     a denormalised copy so account-scoped queries and quota accounting stay
--     single-index fast.
--
--  2. `messages.rfc_message_id` holds the RFC 5322 `Message-ID` header. The row's
--     own identity is the BIGSERIAL `messages.id` (the specification called the
--     latter `message_id`, which collided with the header name in every query).
--
-- Ownership: the database is authoritative for *what exists*; the filesystem
-- (`storage_path`, relative to the Maildir root) is authoritative for *the bytes*.
--
-- Timestamps are always TIMESTAMPTZ and always UTC.
-- =============================================================================

-- -----------------------------------------------------------------------------
-- Identity
-- -----------------------------------------------------------------------------

CREATE TABLE users (
    id            BIGSERIAL   PRIMARY KEY,
    email         TEXT        NOT NULL,
    password_hash TEXT        NOT NULL,
    display_name  TEXT,
    enabled       BOOLEAN     NOT NULL DEFAULT TRUE,
    is_admin      BOOLEAN     NOT NULL DEFAULT FALSE,
    quota_bytes   BIGINT      NOT NULL DEFAULT 1073741824,
    used_bytes    BIGINT      NOT NULL DEFAULT 0,
    failed_logins INTEGER     NOT NULL DEFAULT 0,
    locked_until  TIMESTAMPTZ,
    last_login_at TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT users_email_not_blank  CHECK (length(btrim(email)) > 3),
    CONSTRAINT users_email_lowercase  CHECK (email = lower(email)),
    CONSTRAINT users_quota_sane       CHECK (quota_bytes >= 0),
    CONSTRAINT users_used_sane        CHECK (used_bytes  >= 0)
);

CREATE UNIQUE INDEX users_email_key ON users (email);
CREATE INDEX users_admin_idx ON users (is_admin) WHERE is_admin;

CREATE TABLE domains (
    id               BIGSERIAL   PRIMARY KEY,
    name             TEXT        NOT NULL,
    description      TEXT,
    enabled          BOOLEAN     NOT NULL DEFAULT TRUE,
    -- Local part that receives mail addressed to a non-existent mailbox here.
    catch_all        TEXT,
    dkim_selector    TEXT,
    dkim_private_key TEXT,
    dkim_public_key  TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT domains_name_lowercase CHECK (name = lower(name)),
    CONSTRAINT domains_name_not_blank CHECK (length(btrim(name)) > 3)
);

CREATE UNIQUE INDEX domains_name_key ON domains (name);

-- -----------------------------------------------------------------------------
-- Addresses, aliases, folders
-- -----------------------------------------------------------------------------

-- An address such as alice@example.com. This is what SMTP RCPT TO resolves against.
CREATE TABLE mailboxes (
    id           BIGSERIAL   PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id)   ON DELETE CASCADE,
    domain_id    BIGINT      NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
    local_part   TEXT        NOT NULL,
    display_name TEXT,
    enabled      BOOLEAN     NOT NULL DEFAULT TRUE,
    is_primary   BOOLEAN     NOT NULL DEFAULT FALSE,
    -- NULL = inherit the owner's quota.
    quota_bytes  BIGINT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT mailboxes_local_lowercase CHECK (local_part = lower(local_part)),
    CONSTRAINT mailboxes_local_not_blank CHECK (length(btrim(local_part)) > 0)
);

CREATE UNIQUE INDEX mailboxes_address_key ON mailboxes (domain_id, local_part);
CREATE INDEX mailboxes_user_idx ON mailboxes (user_id);
-- Exactly one primary address per user.
CREATE UNIQUE INDEX mailboxes_primary_key ON mailboxes (user_id) WHERE is_primary;

CREATE TABLE aliases (
    id         BIGSERIAL   PRIMARY KEY,
    domain_id  BIGINT      NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
    local_part TEXT        NOT NULL,
    -- Full destination address; a bare local part means "same domain".
    target     TEXT        NOT NULL,
    enabled    BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT aliases_local_lowercase CHECK (local_part = lower(local_part))
);

CREATE UNIQUE INDEX aliases_key ON aliases (domain_id, local_part);

-- IMAP folders. `INBOX` is created for every mailbox; Sent/Drafts/Trash/Junk/Archive
-- carry `special_use` so clients can map them without guessing names.
CREATE TABLE folders (
    id             BIGSERIAL   PRIMARY KEY,
    mailbox_id     BIGINT      NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    name           TEXT        NOT NULL,
    parent_id      BIGINT      REFERENCES folders(id) ON DELETE CASCADE,
    special_use    TEXT,
    subscribed     BOOLEAN     NOT NULL DEFAULT TRUE,
    uid_validity   BIGINT      NOT NULL DEFAULT 1,
    uid_next       BIGINT      NOT NULL DEFAULT 1,
    highest_modseq BIGINT      NOT NULL DEFAULT 1,
    message_count  INTEGER     NOT NULL DEFAULT 0,
    unseen_count   INTEGER     NOT NULL DEFAULT 0,
    total_bytes    BIGINT      NOT NULL DEFAULT 0,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT folders_name_not_blank CHECK (length(btrim(name)) > 0),
    CONSTRAINT folders_special_use_known CHECK (
        special_use IS NULL OR special_use IN ('\Sent', '\Drafts', '\Trash', '\Junk', '\Archive', '\All', '\Flagged')
    )
);

CREATE UNIQUE INDEX folders_name_key ON folders (mailbox_id, name);
CREATE INDEX folders_mailbox_idx ON folders (mailbox_id);
CREATE UNIQUE INDEX folders_special_use_key ON folders (mailbox_id, special_use) WHERE special_use IS NOT NULL;

-- -----------------------------------------------------------------------------
-- Messages
-- -----------------------------------------------------------------------------

CREATE TABLE messages (
    id               BIGSERIAL   PRIMARY KEY,
    folder_id        BIGINT      NOT NULL REFERENCES folders(id)   ON DELETE CASCADE,
    mailbox_id       BIGINT      NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    -- IMAP UID, unique and monotonically increasing within the folder.
    uid              BIGINT      NOT NULL,
    rfc_message_id   TEXT,
    -- Conversation grouping: the root Message-ID of the References chain.
    thread_id        TEXT,
    subject          TEXT,
    sender           TEXT,
    sender_name      TEXT,
    snippet          TEXT,
    size_bytes       BIGINT      NOT NULL,
    -- Relative to the Maildir root, e.g. example.com/alice/Maildir/cur/1234.M1P.ferroma:2,S
    storage_path     TEXT        NOT NULL,
    checksum_sha256  TEXT,
    -- Space-separated IMAP flags, lower-case system names first, e.g. "seen flagged $label1".
    flags            TEXT        NOT NULL DEFAULT '',
    internal_date    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    received_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    sent_at          TIMESTAMPTZ,
    has_attachments  BOOLEAN     NOT NULL DEFAULT FALSE,
    attachment_count INTEGER     NOT NULL DEFAULT 0,
    is_draft         BOOLEAN     NOT NULL DEFAULT FALSE,
    modseq           BIGINT      NOT NULL DEFAULT 1,
    -- Soft delete (`\Deleted` + client sync tombstone) then hard expiry.
    deleted_at       TIMESTAMPTZ,
    expunged_at      TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT messages_size_sane  CHECK (size_bytes >= 0),
    CONSTRAINT messages_uid_sane   CHECK (uid > 0)
);

CREATE UNIQUE INDEX messages_folder_uid_key ON messages (folder_id, uid);
CREATE INDEX messages_mailbox_date_idx ON messages (mailbox_id, internal_date DESC);
CREATE INDEX messages_folder_date_idx ON messages (folder_id, internal_date DESC);
CREATE INDEX messages_rfc_id_idx ON messages (rfc_message_id) WHERE rfc_message_id IS NOT NULL;
CREATE INDEX messages_thread_idx ON messages (thread_id) WHERE thread_id IS NOT NULL;
CREATE INDEX messages_sender_idx ON messages (sender);
-- The live set of a folder: everything not expunged.
CREATE INDEX messages_live_idx ON messages (folder_id, internal_date DESC) WHERE expunged_at IS NULL;
-- Server-side SEARCH by subject, per the specification §30.
CREATE INDEX messages_subject_fts_idx
    ON messages USING GIN (to_tsvector('simple', coalesce(subject, '')));

CREATE TABLE message_recipients (
    id           BIGSERIAL PRIMARY KEY,
    message_id   BIGINT    NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- 'to' | 'cc' | 'bcc' | 'reply-to' | 'sender'
    kind         TEXT      NOT NULL,
    address      TEXT      NOT NULL,
    display_name TEXT,
    ordinal      INTEGER   NOT NULL DEFAULT 0,
    CONSTRAINT message_recipients_kind_known CHECK (
        kind IN ('to', 'cc', 'bcc', 'reply-to', 'sender')
    )
);

CREATE INDEX message_recipients_message_idx ON message_recipients (message_id);
CREATE INDEX message_recipients_address_idx ON message_recipients (address);

CREATE TABLE attachments (
    id              BIGSERIAL   PRIMARY KEY,
    message_id      BIGINT      NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename        TEXT,
    content_type    TEXT        NOT NULL DEFAULT 'application/octet-stream',
    size_bytes      BIGINT      NOT NULL,
    storage_path    TEXT        NOT NULL,
    content_id      TEXT,
    is_inline       BOOLEAN     NOT NULL DEFAULT FALSE,
    checksum_sha256 TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT attachments_size_sane CHECK (size_bytes >= 0)
);

CREATE INDEX attachments_message_idx ON attachments (message_id);

-- -----------------------------------------------------------------------------
-- Outbound queue
-- -----------------------------------------------------------------------------

CREATE TABLE mail_queue (
    id               BIGSERIAL   PRIMARY KEY,
    message_id       BIGINT      NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id          BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    sender           TEXT        NOT NULL,
    recipient        TEXT        NOT NULL,
    -- pending -> delivering -> delivered | retry -> delivering … | failed
    status           TEXT        NOT NULL DEFAULT 'pending',
    attempts         INTEGER     NOT NULL DEFAULT 0,
    max_attempts     INTEGER     NOT NULL DEFAULT 12,
    next_attempt_at  TIMESTAMPTZ,
    last_attempt_at  TIMESTAMPTZ,
    delivered_at     TIMESTAMPTZ,
    last_error       TEXT,
    last_status_code INTEGER,
    last_status_text TEXT,
    remote_mx        TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT mail_queue_status_known CHECK (
        status IN ('pending', 'delivering', 'delivered', 'retry', 'failed', 'cancelled')
    ),
    CONSTRAINT mail_queue_attempts_sane CHECK (attempts >= 0)
);

-- The dispatcher's hot path: "what is due right now?"
CREATE INDEX mail_queue_due_idx ON mail_queue (next_attempt_at)
    WHERE status IN ('pending', 'retry');
CREATE INDEX mail_queue_message_idx ON mail_queue (message_id);
CREATE INDEX mail_queue_status_idx ON mail_queue (status);
CREATE INDEX mail_queue_user_idx ON mail_queue (user_id, created_at DESC) WHERE user_id IS NOT NULL;

-- One row per delivery attempt, for the Admin "Delivery Logs" screen.
CREATE TABLE delivery_attempts (
    id          BIGSERIAL   PRIMARY KEY,
    queue_id    BIGINT      NOT NULL REFERENCES mail_queue(id) ON DELETE CASCADE,
    attempt     INTEGER     NOT NULL,
    remote_mx   TEXT,
    status_code INTEGER,
    status_text TEXT,
    error       TEXT,
    duration_ms INTEGER,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX delivery_attempts_queue_idx ON delivery_attempts (queue_id, attempt);

-- -----------------------------------------------------------------------------
-- Sessions, devices, client sync state
-- -----------------------------------------------------------------------------

CREATE TABLE devices (
    id               BIGSERIAL   PRIMARY KEY,
    user_id          BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Stable identifier generated by the client installation.
    device_uid       TEXT        NOT NULL,
    name             TEXT,
    platform         TEXT,
    client_version   TEXT,
    protocol_version INTEGER,
    last_seen_at     TIMESTAMPTZ,
    last_ip          TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at       TIMESTAMPTZ
);

CREATE UNIQUE INDEX devices_uid_key ON devices (user_id, device_uid);
CREATE INDEX devices_user_idx ON devices (user_id);

CREATE TABLE sessions (
    id           BIGSERIAL   PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- 'web' | 'api' | 'client' | 'imap' | 'smtp'
    kind         TEXT        NOT NULL,
    -- SHA-256 of the opaque token; the raw token is never stored.
    token_hash   TEXT        NOT NULL,
    device_id    BIGINT      REFERENCES devices(id) ON DELETE SET NULL,
    ip           TEXT,
    user_agent   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ NOT NULL,
    revoked_at   TIMESTAMPTZ,
    CONSTRAINT sessions_kind_known CHECK (kind IN ('web', 'api', 'client', 'imap', 'smtp'))
);

CREATE UNIQUE INDEX sessions_token_key ON sessions (token_hash);
CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_expiry_idx ON sessions (expires_at) WHERE revoked_at IS NULL;

-- Per-device, per-folder synchronisation cursor (specification §21, §37).
CREATE TABLE client_sync_states (
    id         BIGSERIAL   PRIMARY KEY,
    device_id  BIGINT      NOT NULL REFERENCES devices(id)    ON DELETE CASCADE,
    mailbox_id BIGINT      NOT NULL REFERENCES mailboxes(id)  ON DELETE CASCADE,
    -- NULL means "account level" (folder list changes, settings).
    folder_id  BIGINT      REFERENCES folders(id) ON DELETE CASCADE,
    cursor     BIGINT      NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- COALESCE because PostgreSQL treats NULLs as distinct in unique indexes, which
-- would otherwise let one device accumulate many account-level rows.
CREATE UNIQUE INDEX client_sync_states_key
    ON client_sync_states (device_id, mailbox_id, COALESCE(folder_id, 0));
CREATE INDEX client_sync_states_device_idx ON client_sync_states (device_id, updated_at DESC);

-- -----------------------------------------------------------------------------
-- Drafts
-- -----------------------------------------------------------------------------

CREATE TABLE drafts (
    id             BIGSERIAL   PRIMARY KEY,
    user_id        BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mailbox_id     BIGINT      REFERENCES mailboxes(id) ON DELETE CASCADE,
    folder_id      BIGINT      REFERENCES folders(id)   ON DELETE SET NULL,
    message_id     BIGINT      REFERENCES messages(id)  ON DELETE SET NULL,
    subject        TEXT,
    body_text      TEXT,
    body_html      TEXT,
    -- [{ "address": "...", "name": "..." }]
    recipients     JSONB       NOT NULL DEFAULT '[]'::jsonb,
    -- [{ "filename": "...", "content_type": "...", "size_bytes": 0, "storage_path": "..." }]
    attachments    JSONB       NOT NULL DEFAULT '[]'::jsonb,
    in_reply_to    TEXT,
    reference_ids  JSONB       NOT NULL DEFAULT '[]'::jsonb,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX drafts_user_idx ON drafts (user_id, updated_at DESC);

-- -----------------------------------------------------------------------------
-- Operations (idempotency), change log (sync journal), audit
-- -----------------------------------------------------------------------------

-- Specification §55: a replayed client request must never execute twice.
CREATE TABLE operations (
    operation_id TEXT        PRIMARY KEY,
    user_id      BIGINT      REFERENCES users(id) ON DELETE CASCADE,
    kind         TEXT        NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'applied',
    -- Cached response, replayed verbatim to the retrying client.
    result       JSONB,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    CONSTRAINT operations_status_known CHECK (status IN ('applied', 'failed'))
);

CREATE INDEX operations_user_idx ON operations (user_id, created_at DESC);
CREATE INDEX operations_created_idx ON operations (created_at);

-- The sync journal behind `GET /api/v1/client/sync?cursor=…`.
-- `message_id` is intentionally NOT a foreign key: tombstones must outlive rows.
CREATE TABLE change_log (
    seq        BIGSERIAL   PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mailbox_id BIGINT      REFERENCES mailboxes(id) ON DELETE CASCADE,
    folder_id  BIGINT      REFERENCES folders(id)   ON DELETE CASCADE,
    message_id BIGINT,
    kind       TEXT        NOT NULL,
    payload    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX change_log_cursor_idx  ON change_log (user_id, seq);
CREATE INDEX change_log_mailbox_idx ON change_log (mailbox_id, seq);
CREATE INDEX change_log_folder_idx  ON change_log (folder_id, seq);
CREATE INDEX change_log_created_idx ON change_log (created_at);

CREATE TABLE audit_logs (
    id            BIGSERIAL   PRIMARY KEY,
    actor_user_id BIGINT      REFERENCES users(id) ON DELETE SET NULL,
    action        TEXT        NOT NULL,
    target_type   TEXT,
    target_id     TEXT,
    ip            TEXT,
    user_agent    TEXT,
    details       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX audit_logs_actor_idx  ON audit_logs (actor_user_id, created_at DESC);
CREATE INDEX audit_logs_action_idx ON audit_logs (action, created_at DESC);
CREATE INDEX audit_logs_created_idx ON audit_logs (created_at);

-- -----------------------------------------------------------------------------
-- Login throttling
-- -----------------------------------------------------------------------------

CREATE TABLE login_attempts (
    id         BIGSERIAL   PRIMARY KEY,
    email      TEXT        NOT NULL,
    ip         TEXT,
    kind       TEXT        NOT NULL DEFAULT 'password',
    success    BOOLEAN     NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX login_attempts_email_idx ON login_attempts (email, created_at DESC);
CREATE INDEX login_attempts_ip_idx    ON login_attempts (ip, created_at DESC);
-- Retention sweeps delete by age.
CREATE INDEX login_attempts_created_idx ON login_attempts (created_at);

-- -----------------------------------------------------------------------------
-- System settings (DB-backed, overrides nothing in ferroma.toml but is readable
-- and writable from the Admin panel without a restart)
-- -----------------------------------------------------------------------------

CREATE TABLE settings (
    key        TEXT        PRIMARY KEY,
    value      JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
