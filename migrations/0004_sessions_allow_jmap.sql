-- JMAP bearer tokens are backed by ordinary, revocable `sessions` rows. Existing
-- installations created before JMAP need their enum-like check widened separately
-- from 0001, which only affects fresh databases.
--
-- The kind belongs here and not in 0001. sqlx checksums a migration's bytes, and
-- a database that already applied 0001 refuses to start (`VersionMismatch`) the
-- moment that file changes — a comment is enough. Fresh databases still end at
-- the same constraint, because this migration runs immediately after 0001.
ALTER TABLE sessions DROP CONSTRAINT IF EXISTS sessions_kind_known;
ALTER TABLE sessions
    ADD CONSTRAINT sessions_kind_known
    CHECK (kind IN ('web', 'api', 'client', 'jmap', 'imap', 'smtp'));
