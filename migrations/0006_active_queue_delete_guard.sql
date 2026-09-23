-- A queued delivery owns its message body until delivery reaches a terminal state.
-- The message trigger covers direct DELETE and cascades. Parent BEFORE triggers
-- check before FK cascades can remove queue rows; locking the message rows also
-- conflicts with the queue INSERT foreign key's KEY SHARE lock, closing the race.
CREATE FUNCTION reject_active_queue_message_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM mail_queue q WHERE q.message_id = OLD.id
               AND q.status IN ('pending', 'retry', 'delivering')) THEN
        RAISE EXCEPTION 'message % has an active mail queue entry', OLD.id
            USING ERRCODE = '23514', CONSTRAINT = 'active_queue_message_delete_guard';
    END IF;
    RETURN OLD;
END;
$$;

CREATE TRIGGER protect_active_queue_message
BEFORE DELETE ON messages FOR EACH ROW
EXECUTE FUNCTION reject_active_queue_message_delete();

CREATE FUNCTION reject_active_queue_parent_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    message_row RECORD;
BEGIN
    -- A folder deletion also cascades through any nested child folders.
    -- For other parent types the recursive seed is harmless and unused.
    FOR message_row IN
        WITH RECURSIVE subtree(id) AS (
            SELECT OLD.id
            UNION ALL
            SELECT f.id FROM folders f JOIN subtree s ON f.parent_id = s.id
        )
        SELECT m.id FROM messages m
        JOIN mailboxes b ON b.id = m.mailbox_id
        WHERE (TG_TABLE_NAME = 'folders' AND m.folder_id IN (SELECT id FROM subtree))
           OR (TG_TABLE_NAME = 'mailboxes' AND m.mailbox_id = OLD.id)
           OR (TG_TABLE_NAME = 'users' AND b.user_id = OLD.id)
           OR (TG_TABLE_NAME = 'domains' AND b.domain_id = OLD.id)
        ORDER BY m.id FOR UPDATE OF m
    LOOP
        IF EXISTS (SELECT 1 FROM mail_queue q WHERE q.message_id = message_row.id
                   AND q.status IN ('pending', 'retry', 'delivering')) THEN
            RAISE EXCEPTION 'message % has an active mail queue entry', message_row.id
                USING ERRCODE = '23514', CONSTRAINT = 'active_queue_message_delete_guard';
        END IF;
    END LOOP;
    RETURN OLD;
END;
$$;

CREATE TRIGGER protect_active_queue_folder
BEFORE DELETE ON folders FOR EACH ROW
EXECUTE FUNCTION reject_active_queue_parent_delete();
CREATE TRIGGER protect_active_queue_mailbox
BEFORE DELETE ON mailboxes FOR EACH ROW
EXECUTE FUNCTION reject_active_queue_parent_delete();
CREATE TRIGGER protect_active_queue_user
BEFORE DELETE ON users FOR EACH ROW
EXECUTE FUNCTION reject_active_queue_parent_delete();
CREATE TRIGGER protect_active_queue_domain
BEFORE DELETE ON domains FOR EACH ROW
EXECUTE FUNCTION reject_active_queue_parent_delete();
