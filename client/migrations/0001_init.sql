-- Ferroma client local cache — initial schema (specification §26).
--
-- `Server = Source of Truth`; every row in this file is *cache* or *pending intent*.
-- Nothing here is authoritative, and dropping the whole database is always a legal
-- (if expensive) recovery step.
--
-- Two conventions run through the whole schema:
--
--   * Every cache table is keyed by `(account_id, <server id>)`, because two accounts
--     on two different servers both have a folder 5 and a message 4821.
--   * Timestamps are RFC 3339 UTC strings. `sqlx` maps them to `DateTime<Utc>`.
--
-- Which column serves which query is documented per table; the FTS5 index used by
-- local search is created in code (`ClientDatabase::ensure_search_index`) so that a
-- SQLite build without FTS5 degrades to `LIKE` instead of failing to open.

-- accounts -------------------------------------------------------------------
-- One row per configured account. `base_url` is the FCP base (`…/api/v1/client`).
-- Queries: `AccountManager::list` orders by `id`; `find_by_email` uses the
-- `(email, base_url)` uniqueness; the sync engine reads `paused`,
-- `refresh_token` and `sync_window_days` for this account before every run.
CREATE TABLE accounts (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    email             TEXT    NOT NULL,
    display_name      TEXT,
    base_url          TEXT    NOT NULL,
    device_uid        TEXT    NOT NULL,
    device_id         INTEGER,
    user_id           INTEGER,
    -- Rotated on every refresh (§2); never logged, never exported.
    refresh_token     TEXT,
    protocol_version  INTEGER,
    server_version    TEXT,
    -- 1 = the user paused sync for this account; the sync engine skips it.
    paused            INTEGER NOT NULL DEFAULT 0,
    -- NULL = sync everything, otherwise the settings §52 window (30 / 90 days).
    sync_window_days  INTEGER,
    cache_limit_bytes INTEGER,
    created_at        TEXT    NOT NULL,
    updated_at        TEXT    NOT NULL,
    last_sync_at      TEXT,
    last_error        TEXT,
    UNIQUE (email, base_url)
);

-- mailboxes ------------------------------------------------------------------
-- The addresses of an account (§4). Queries: the folder tree in the UI joins
-- `mailboxes` to `folders` on `(account_id, id)`; `is_primary` picks the default
-- `From:` address when composing.
CREATE TABLE mailboxes (
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    id           INTEGER NOT NULL,
    address      TEXT    NOT NULL,
    display_name TEXT,
    is_primary   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, id)
);

-- folders --------------------------------------------------------------------
-- Queries: the sidebar lists folders by `(account_id, mailbox_id)` ordered by
-- `name`; `special_use` maps `\Sent`/`\Drafts`/`\Trash` without guessing names
-- (§4); `uid_validity` is compared against the server on every sync — a change
-- wipes `messages` for the folder and resets its cursor to 0.
CREATE TABLE folders (
    account_id    INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    id            INTEGER NOT NULL,
    mailbox_id    INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    special_use   TEXT,
    -- Counters reported by the server, used for the first-sync progress bar (§3.7).
    message_count INTEGER NOT NULL DEFAULT 0,
    unseen_count  INTEGER NOT NULL DEFAULT 0,
    uid_validity  INTEGER NOT NULL DEFAULT 0,
    uid_next      INTEGER NOT NULL DEFAULT 0,
    parent_id     INTEGER,
    updated_at    TEXT,
    PRIMARY KEY (account_id, id)
);
CREATE INDEX idx_folders_mailbox ON folders (account_id, mailbox_id, name);
CREATE INDEX idx_folders_special ON folders (account_id, special_use);

-- messages -------------------------------------------------------------------
-- The header/flags cache written by `message_created` / `message_updated` /
-- `message_moved`. Queries:
--   * message list  — `idx_messages_folder_date` (account_id, folder_id, internal_date DESC)
--   * unread counts — `idx_messages_folder_flags` (account_id, folder_id, flags)
--   * by UID        — `idx_messages_folder_uid`, used when a UID is renumbered
--   * tombstones    — `deleted = 1` rows are kept so a re-delivered change is a no-op
-- `body_state` drives lazy download: `pending` (headers only), `ready` (body cached
-- in `message_bodies`), `missing` (the server refused it; do not retry in a loop).
CREATE TABLE messages (
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    id               INTEGER NOT NULL,
    folder_id        INTEGER,
    mailbox_id       INTEGER,
    uid              INTEGER,
    subject          TEXT    NOT NULL DEFAULT '',
    from_address     TEXT    NOT NULL DEFAULT '',
    from_name        TEXT,
    -- Comma-joined recipient addresses; the list view shows it and FTS indexes it.
    to_summary       TEXT,
    snippet          TEXT,
    -- Space separated IMAP flags, e.g. "seen flagged" (§5 PATCH body).
    flags            TEXT    NOT NULL DEFAULT '',
    size_bytes       INTEGER NOT NULL DEFAULT 0,
    has_attachments  INTEGER NOT NULL DEFAULT 0,
    attachment_count INTEGER NOT NULL DEFAULT 0,
    internal_date    TEXT,
    sent_at          TEXT,
    rfc_message_id   TEXT,
    body_state       TEXT    NOT NULL DEFAULT 'pending',
    deleted          INTEGER NOT NULL DEFAULT 0,
    cached_at        TEXT    NOT NULL,
    PRIMARY KEY (account_id, id)
);
CREATE INDEX idx_messages_folder_date ON messages (account_id, folder_id, internal_date DESC);
CREATE INDEX idx_messages_folder_flags ON messages (account_id, folder_id, flags);
CREATE INDEX idx_messages_folder_uid ON messages (account_id, folder_id, uid);
CREATE INDEX idx_messages_rfc_id ON messages (account_id, rfc_message_id);

-- message_headers ------------------------------------------------------------
-- The raw header list of one message, in wire order. Queries: the reading pane
-- renders `name: value` for the "show original headers" disclosure, ordered by
-- `ordinal`.
CREATE TABLE message_headers (
    account_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    ordinal    INTEGER NOT NULL,
    name       TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    PRIMARY KEY (account_id, message_id, ordinal),
    FOREIGN KEY (account_id, message_id) REFERENCES messages (account_id, id) ON DELETE CASCADE
);

-- message_bodies -------------------------------------------------------------
-- Separated from `messages` so the list view never pays for body I/O and so the
-- body cache can be evicted (setting `cache.bodies`) independently of headers.
-- Queries: `get_message` (PK lookup); `raw` holds the RFC 5322 bytes used by the
-- "download original" action and by re-parsing after a format upgrade.
CREATE TABLE message_bodies (
    account_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    text_body  TEXT,
    html_body  TEXT,
    raw        BLOB,
    truncated  INTEGER NOT NULL DEFAULT 0,
    fetched_at TEXT    NOT NULL,
    PRIMARY KEY (account_id, message_id),
    FOREIGN KEY (account_id, message_id) REFERENCES messages (account_id, id) ON DELETE CASCADE
);

-- attachments ----------------------------------------------------------------
-- Attachment metadata per message plus a pointer into the content-addressed blob
-- cache. Queries: the reading pane lists attachments for a message
-- (`idx_attachments_message`); `blob_hash` links to `blob_cache.sha256` and is
-- NULL until the bytes have actually been downloaded (§29 offline cache).
CREATE TABLE attachments (
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    id           INTEGER NOT NULL,
    message_id   INTEGER,
    filename     TEXT    NOT NULL DEFAULT '',
    content_type TEXT    NOT NULL DEFAULT 'application/octet-stream',
    size_bytes   INTEGER NOT NULL DEFAULT 0,
    sha256       TEXT,
    content_id   TEXT,
    disposition  TEXT,
    blob_hash    TEXT,
    cached_at    TEXT,
    last_access  TEXT,
    PRIMARY KEY (account_id, id)
);
CREATE INDEX idx_attachments_message ON attachments (account_id, message_id);
CREATE INDEX idx_attachments_blob ON attachments (account_id, blob_hash);

-- blob_cache -----------------------------------------------------------------
-- The on-disk attachment cache index. Queries: `hit` is a PK lookup by SHA-256;
-- eviction scans `access_seq` ascending ("least recently used") and stops when
-- the sum of `size_bytes` is under the configured cap. `pinned` rows (a file the
-- user just opened) are skipped by eviction.
--
-- `access_seq` exists because `last_access` is a wall-clock string with
-- second precision: three attachments cached in the same second would be
-- indistinguishable, and eviction would drop an arbitrary one. `access_seq` is
-- a monotonic counter, so the LRU order is total and reproducible; `last_access`
-- stays for humans reading the table.
CREATE TABLE blob_cache (
    sha256      TEXT PRIMARY KEY,
    size_bytes  INTEGER NOT NULL,
    path        TEXT NOT NULL,
    last_access TEXT NOT NULL,
    access_seq  INTEGER NOT NULL DEFAULT 0,
    pinned      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_blob_cache_lru ON blob_cache (access_seq);

-- drafts ---------------------------------------------------------------------
-- Server-side drafts (§7) with a local, offline-first copy. `local_uid` is stable
-- for the life of a draft even before the server assigns an id, so the compose
-- window survives a restart. `dirty = 1` means "local edits not yet pushed";
-- `server_updated_at` is what the server told us, and is the input to the
-- last-write-wins conflict rule.
CREATE TABLE drafts (
    account_id          INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    local_uid           TEXT    NOT NULL,
    id                  INTEGER,
    mailbox_id          INTEGER,
    subject             TEXT    NOT NULL DEFAULT '',
    text_body           TEXT,
    html_body           TEXT,
    to_json             TEXT    NOT NULL DEFAULT '[]',
    cc_json             TEXT    NOT NULL DEFAULT '[]',
    bcc_json            TEXT    NOT NULL DEFAULT '[]',
    in_reply_to         TEXT,
    references_json     TEXT    NOT NULL DEFAULT '[]',
    attachment_ids_json TEXT    NOT NULL DEFAULT '[]',
    server_updated_at   TEXT,
    local_updated_at    TEXT    NOT NULL,
    dirty               INTEGER NOT NULL DEFAULT 1,
    deleted             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, local_uid),
    UNIQUE (account_id, id)
);
CREATE INDEX idx_drafts_dirty ON drafts (account_id, dirty);

-- outbox ---------------------------------------------------------------------
-- The eight-state send pipeline (§28). Queries:
--   * the Outbox view  — `idx_outbox_state` (account_id, state, id)
--   * `counts_by_state` — a GROUP BY over the same index
--   * resume after restart — every non-terminal row is re-read on start-up
-- `operation_id` is generated when the draft enters the outbox, never at send
-- time, so a crash mid-request can be retried without double-sending (§5, §55).
CREATE TABLE outbox (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id          INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    operation_id        TEXT    NOT NULL,
    state               TEXT    NOT NULL,
    mailbox_id          INTEGER,
    from_address        TEXT    NOT NULL,
    to_json             TEXT    NOT NULL DEFAULT '[]',
    cc_json             TEXT    NOT NULL DEFAULT '[]',
    bcc_json            TEXT    NOT NULL DEFAULT '[]',
    subject             TEXT    NOT NULL DEFAULT '',
    text_body           TEXT,
    html_body           TEXT,
    attachment_ids_json TEXT    NOT NULL DEFAULT '[]',
    in_reply_to         TEXT,
    references_json     TEXT    NOT NULL DEFAULT '[]',
    created_at          TEXT    NOT NULL,
    updated_at          TEXT    NOT NULL,
    attempts            INTEGER NOT NULL DEFAULT 0,
    last_error          TEXT,
    server_message_id   INTEGER,
    queued_recipients   INTEGER,
    sent_at             TEXT,
    UNIQUE (operation_id)
);
CREATE INDEX idx_outbox_state ON outbox (account_id, state, id);

-- pending_operations ---------------------------------------------------------
-- The offline queue (§27). `seq` is an AUTOINCREMENT rowid, which is the FIFO
-- order the flusher must preserve: it walks `seq` ascending and stops at the
-- first retryable failure so a later `mark_read` can never overtake an earlier
-- `move`. `operation_id` is generated at enqueue time (§5).
CREATE TABLE pending_operations (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id TEXT    NOT NULL UNIQUE,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind         TEXT    NOT NULL,
    payload      TEXT    NOT NULL DEFAULT '{}',
    created_at   TEXT    NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT
);
CREATE INDEX idx_pending_order ON pending_operations (account_id, seq);

-- sync_state -----------------------------------------------------------------
-- One cursor per stream (§3). `folder_id = 0` is the account-level stream
-- (folder list, drafts, settings). The cursor is written *in the same
-- transaction* as the changes it covers — "apply, then advance" — which is why
-- a crash re-fetches the same page instead of skipping it.
CREATE TABLE sync_state (
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id    INTEGER NOT NULL DEFAULT 0,
    mailbox_id   INTEGER,
    cursor       TEXT    NOT NULL DEFAULT '0',
    uid_validity INTEGER,
    last_sync_at TEXT,
    status       TEXT    NOT NULL DEFAULT 'idle',
    last_error   TEXT,
    PRIMARY KEY (account_id, folder_id)
);

-- devices --------------------------------------------------------------------
-- The device list from §9 / §33. Queries: the settings → devices pane lists by
-- `(account_id, id)`; `current` marks the device this installation is logged in
-- as, so "revoke" can warn that it is the one you are using.
CREATE TABLE devices (
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    id               INTEGER NOT NULL,
    device_uid       TEXT    NOT NULL,
    name             TEXT,
    platform         TEXT,
    client_version   TEXT,
    protocol_version INTEGER,
    last_seen_at     TEXT,
    last_ip          TEXT,
    created_at       TEXT,
    revoked          INTEGER NOT NULL DEFAULT 0,
    current          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, id)
);

-- settings -------------------------------------------------------------------
-- The settings surface (§52) as a section-keyed JSON blob per row: `key` is the
-- section (`sync`, `appearance`, …) and `value` is its serialised form. A typed
-- accessor reads/merges one section, so an unknown key from a newer build is
-- preserved rather than dropped.
CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- notifications --------------------------------------------------------------
-- Notifications the shell has not shown yet (§34). Queries: the shell drains
-- `delivered = 0` ordered by `id`. The core never shells out to a platform
-- notifier; it records here and lets the UI shell do the presentation.
CREATE TABLE notifications (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER,
    kind       TEXT    NOT NULL,
    title      TEXT    NOT NULL,
    body       TEXT,
    payload    TEXT,
    created_at TEXT    NOT NULL,
    delivered  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_notifications_pending ON notifications (delivered, id);
