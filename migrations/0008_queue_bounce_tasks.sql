-- Delivery failures created before this migration must never generate new DSNs.
ALTER TABLE mail_queue
    ADD COLUMN bounce_status TEXT NOT NULL DEFAULT 'none',
    ADD COLUMN bounce_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN bounce_next_attempt_at TIMESTAMPTZ,
    ADD COLUMN bounce_claimed_at TIMESTAMPTZ,
    ADD COLUMN bounce_message_id BIGINT,
    ADD CONSTRAINT mail_queue_bounce_status_known CHECK (
        bounce_status IN ('none', 'pending', 'processing', 'sent', 'skipped')
    ),
    ADD CONSTRAINT mail_queue_bounce_attempts_sane CHECK (bounce_attempts >= 0);

UPDATE mail_queue SET bounce_status = 'skipped' WHERE status = 'failed';

CREATE INDEX mail_queue_bounce_due_idx ON mail_queue (bounce_next_attempt_at, id)
    WHERE status = 'failed' AND bounce_status = 'pending';

-- The message body is still needed until its requested DSN reaches a final state.
-- Replace both existing functions: parent BEFORE triggers must check before
-- their foreign-key cascades, just like direct message deletion.
CREATE OR REPLACE FUNCTION reject_active_queue_message_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM mail_queue q WHERE q.message_id = OLD.id
               AND (q.status IN ('pending', 'retry', 'delivering')
                    OR (q.status = 'failed' AND q.bounce_status IN ('pending', 'processing')))) THEN
        RAISE EXCEPTION 'message % has an active mail queue entry', OLD.id
            USING ERRCODE = '23514', CONSTRAINT = 'active_queue_message_delete_guard';
    END IF;
    RETURN OLD;
END;
$$;

CREATE OR REPLACE FUNCTION reject_active_queue_parent_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    message_row RECORD;
BEGIN
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
                   AND (q.status IN ('pending', 'retry', 'delivering')
                        OR (q.status = 'failed' AND q.bounce_status IN ('pending', 'processing')))) THEN
            RAISE EXCEPTION 'message % has an active mail queue entry', message_row.id
                USING ERRCODE = '23514', CONSTRAINT = 'active_queue_message_delete_guard';
        END IF;
    END LOOP;
    RETURN OLD;
END;
$$;
