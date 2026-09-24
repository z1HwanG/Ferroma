-- =============================================================================
-- Ferroma — multi-factor authentication
-- =============================================================================
--
-- Three tables back what an account can present besides its password:
--
-- * `user_totp` holds one TOTP registration per account, keyed by the account so
--   the primary key *is* the "at most one" rule. A registration exists in two
--   states, told apart by `confirmed_at`: `NULL` means the secret has been handed
--   to the account but the user has not yet proved that their authenticator
--   produces codes from it, and a timestamp means it is live. A half-finished
--   enrolment must never be a second factor, which is why the unconfirmed state is
--   the default and why re-enrolling resets `confirmed_at` to `NULL`.
--
-- * `totp_recovery_codes` holds the single-use codes printed when TOTP is turned
--   on. Only the hash is stored — the codes are shown once and never again, exactly
--   like a password reset token. `used_at` is the spend marker: consumption is a
--   single conditional `UPDATE`, so two simultaneous logins cannot both spend the
--   same code. `(user_id, code_hash)` is unique so the same hash cannot be stored
--   twice for one account and "is this code unused" has one answer.
--
-- * `app_passwords` holds long-lived per-client credentials. These are *not* second
--   factors: an app password replaces the account password for one mail client, and
--   it is revoked rather than deleted so the list still shows what used to exist and
--   when it last worked. `token_hash` is globally unique because the token is
--   presented on its own, without a username, so the hash alone must identify the
--   row. Only hashes are stored; the plaintext exists once, in the response.
--
-- `secret`, `label` and `token_hash` are checked non-blank: an empty value would
-- otherwise store a credential that matches nothing (or, for a hash, everything a
-- blank lookup produces), and a CHECK names the mistake better than a silent row.
-- All three tables cascade from `users`, so deleting an account leaves no
-- second-factor material behind.
-- =============================================================================

CREATE TABLE user_totp (
    user_id      BIGINT      PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    secret       TEXT        NOT NULL,
    confirmed_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT user_totp_secret_present CHECK (btrim(secret) <> '')
);

CREATE TABLE totp_recovery_codes (
    id         BIGSERIAL   PRIMARY KEY,
    user_id    BIGINT      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash  TEXT        NOT NULL,
    used_at    TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT totp_recovery_codes_user_hash_unique UNIQUE (user_id, code_hash)
);

CREATE INDEX totp_recovery_codes_user_idx ON totp_recovery_codes (user_id);

CREATE TABLE app_passwords (
    id           BIGSERIAL   PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    label        TEXT        NOT NULL,
    token_hash   TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ,
    CONSTRAINT app_passwords_label_present    CHECK (btrim(label) <> ''),
    CONSTRAINT app_passwords_token_hash_unique UNIQUE (token_hash)
);

CREATE INDEX app_passwords_user_idx ON app_passwords (user_id);
