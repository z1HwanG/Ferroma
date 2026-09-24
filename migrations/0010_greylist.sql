-- =============================================================================
-- Ferroma — greylisting
-- =============================================================================
--
-- A triplet of (peer address, envelope sender, envelope recipient) that has not
-- been seen before is deferred once with `451 4.7.1`. A real MTA queues the
-- message and comes back; most bulk senders do not. That is the whole mechanism,
-- and it is why the table stores the *first* sighting: the delay is measured from
-- it, not from the last retry.
--
-- `first_seen_at` is never overwritten on conflict. A sender that retries before
-- the delay elapses must not push its own deadline forward, or a persistent
-- client would never be let through.
--
-- The peer address is `TEXT` rather than `inet`: the value comes from an accepted
-- socket and is stored as it was observed, and no query needs network arithmetic.
-- Rows are pruned by the operator's storage sweep (see `docs/smtp.md` §13), the
-- same way expired tombstones are.
-- =============================================================================

CREATE TABLE greylist (
    id            BIGSERIAL   PRIMARY KEY,
    peer_ip       TEXT        NOT NULL,
    sender        TEXT        NOT NULL,
    recipient     TEXT        NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT greylist_triplet_unique UNIQUE (peer_ip, sender, recipient),
    CONSTRAINT greylist_peer_ip_present CHECK (btrim(peer_ip) <> ''),
    CONSTRAINT greylist_recipient_present CHECK (btrim(recipient) <> '')
);

CREATE INDEX greylist_seen_idx ON greylist (last_seen_at);
