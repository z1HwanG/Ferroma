-- =============================================================================
-- Ferroma — correct the documented format of `messages.flags`
-- =============================================================================
--
-- `0001_initial.sql` describes the column as "Space-separated IMAP flags, lower-case
-- system names first, e.g. \"seen flagged $label1\"". The implementation joins with
-- **commas**: `ferroma_mail::Flags::to_db_string()` produces `seen,flagged,$label1`
-- and `from_db_string()` is its inverse. A comment that contradicts the only writer
-- is worse than no comment, because it invites a second writer that disagrees.
--
-- This is a new migration rather than an edit to `0001_initial.sql`: sqlx records a
-- checksum per applied migration and refuses to run when one changes, so editing a
-- shipped migration would break every existing deployment.
--
-- The column type and contents are unchanged; only the catalogue comment moves.
-- =============================================================================

COMMENT ON COLUMN messages.flags IS
    'Comma-separated canonical flags, lower-case system names first, e.g. '
    '"seen,flagged,$label1". Read and written exclusively through '
    'ferroma_mail::Flags::{to_db_string,from_db_string,add_keyword,remove_keyword}; '
    'never hand-format this column and never split it on spaces. An empty string '
    'means "no flags", which is what a freshly delivered message has.';

COMMENT ON COLUMN folders.special_use IS
    'IMAP special-use marker with its backslash, e.g. \Sent. NULL for an ordinary '
    'folder. At most one folder per mailbox may carry a given marker '
    '(enforced by folders_special_use_key).';

COMMENT ON COLUMN messages.uid IS
    'IMAP UID, unique and monotonically increasing within folder_id. Allocated by '
    'FoldersRepository::allocate_uid inside the same transaction as the insert; '
    'never assigned by hand. Interpreted only together with the folder''s '
    'uid_validity, which a client must compare before reusing a cached UID.';

COMMENT ON COLUMN change_log.message_id IS
    'Deliberately NOT a foreign key: a tombstone must outlive the row it refers to, '
    'so an offline client that has not synced for weeks can still learn that the '
    'message is gone. Do not add a constraint here.';

COMMENT ON COLUMN users.used_bytes IS
    'Cached sum of the sizes of the owner''s live messages, across every address '
    'they own. Maintained by MailboxesRepository::{add_usage,recompute_usage}; it is '
    'the counter the quota check reads, so a drift here silently over- or '
    'under-admits mail. `recompute_usage` is the authoritative repair.';
