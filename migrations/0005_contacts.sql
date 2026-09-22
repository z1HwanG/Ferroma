-- Contacts remembered from mail the account sent or received.
-- A blocked address is delivered to Junk. The address is the identity; the
-- display name, the note and the two flags are what the owner edits.

CREATE TABLE contacts (
    id           BIGSERIAL   PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    address      TEXT        NOT NULL,
    display_name TEXT,
    note         TEXT,
    favorite     BOOLEAN     NOT NULL DEFAULT FALSE,
    blocked      BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT contacts_address_lowercase CHECK (address = lower(address)),
    CONSTRAINT contacts_address_shaped    CHECK (position('@' in address) > 1)
);

CREATE UNIQUE INDEX contacts_owner_address_key ON contacts (user_id, address);
CREATE INDEX contacts_owner_idx ON contacts (user_id);
