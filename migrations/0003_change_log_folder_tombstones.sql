-- =============================================================================
-- Ferroma — let a `folder_deleted` tombstone outlive the folder it names
-- =============================================================================
--
-- `0001_initial.sql` gives `change_log.message_id` no foreign key, and says why in
-- the table's own comment: "tombstones must outlive rows". `folder_id` was given one
-- anyway (`REFERENCES folders(id) ON DELETE CASCADE`), and the two rules cannot both
-- hold:
--
--   * deleting a folder and *then* recording `folder_deleted` violates the key.
--     `DELETE /api/v1/folders/:id` answered `500 storage_error` —
--     `insert or update on table "change_log" violates foreign key constraint
--     "change_log_folder_id_fkey"` — so the folder disappeared and its journal entry
--     was never written; and
--   * recording the entry *first* would have it cascade away a moment later, which is
--     the same missing tombstone with an extra step.
--
-- The fix is the one `message_id` already documents: drop the constraint. The column
-- and its index stay, so the journal can still be filtered by folder.
--
-- `mailbox_id` keeps its cascade deliberately: `ChangeKind` has no `mailbox_deleted`,
-- so nothing writes a mailbox tombstone and the cascade can only remove entries
-- belonging to a mailbox whose contents are gone too.
--
-- A new migration rather than an edit to `0001_initial.sql`: sqlx records a checksum
-- per applied migration and refuses to run when one changes (see `0002`).
-- =============================================================================

ALTER TABLE change_log DROP CONSTRAINT IF EXISTS change_log_folder_id_fkey;

COMMENT ON COLUMN change_log.folder_id IS
    'The folder a folder-scoped change names. Deliberately NOT a foreign key, for the '
    'same reason change_log.message_id is not: a folder_deleted tombstone has to '
    'outlive the row it names, or a client that was offline when the folder went away '
    'never learns it is gone.';
