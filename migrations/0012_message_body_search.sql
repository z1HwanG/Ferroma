-- =============================================================================
-- Ferroma — full-text search over message bodies
-- =============================================================================
--
-- Search matched `subject`, `sender` and the stored `snippet` only, so a word that
-- appeared anywhere in the body of a message found nothing. The body itself is not
-- in the database — it lives in the Maildir — so it cannot be indexed in place;
-- `body_text` holds the extracted text, written by the delivery path at insert time.
--
-- This is a stored column rather than an expression index because the extraction
-- (MIME walking, charset decoding, HTML-to-text) happens in Rust, not in SQL.
--
-- The `snippet` is in the vector as well as the body: it is a short stored prefix of
-- the body, so a row stored before this migration — whose `body_text` is still NULL —
-- keeps matching the words it matched before, under a word-based query rather than a
-- substring one. Including it costs a little index space and removes a silent
-- regression for existing mail.
--
-- `search_vector` is generated so the two can never disagree: there is no code path
-- that writes one and forgets the other. `to_tsvector` with a constant configuration
-- is immutable, which is what a generated column requires.
--
-- Rows that already exist have `body_text IS NULL` and stay unsearchable by body
-- until an operator runs `ferroma storage reindex-search`, which walks the Maildir and
-- fills them in. That is the honest cost of indexing at write time; the alternative —
-- reading and parsing every message on every search — is what the IMAP layer already
-- does for `SEARCH BODY` and is too slow for a list query.
-- =============================================================================

ALTER TABLE messages ADD COLUMN body_text TEXT;

ALTER TABLE messages
    ADD COLUMN search_vector tsvector
    GENERATED ALWAYS AS (
        to_tsvector(
            'simple',
            coalesce(subject, '') || ' ' ||
            coalesce(sender, '') || ' ' ||
            coalesce(sender_name, '') || ' ' ||
            coalesce(snippet, '') || ' ' ||
            coalesce(body_text, '')
        )
    ) STORED;

-- Replaces the subject-only GIN index from 0001: the generated column covers the
-- subject as well, and two indexes over the same column would only be maintained
-- twice.
DROP INDEX IF EXISTS messages_subject_fts_idx;

CREATE INDEX messages_search_vector_idx ON messages USING GIN (search_vector);

-- The backfill looks for rows it has not indexed yet, so it must be cheap to find
-- them without scanning every message twice.
CREATE INDEX messages_body_text_missing_idx
    ON messages (internal_date)
    WHERE body_text IS NULL AND expunged_at IS NULL;
