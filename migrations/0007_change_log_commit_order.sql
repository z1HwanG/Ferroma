-- Keep each user's FCP cursor in commit order, not sequence-allocation order.
-- The BIGSERIAL column default would obtain nextval before a BEFORE INSERT
-- trigger executes, so the default is dropped and the trigger below becomes the
-- sole allocator: it takes a transaction-scoped per-user lock first, then takes
-- the sequence value. A second transaction for this user cannot allocate its
-- visible cursor until the first commits or rolls back. Rolled-back values leave
-- gaps, which the sync protocol already tolerates. Use the one-bigint advisory
-- namespace for change_log writers. Other code must not take this lock while
-- holding it in the reverse order of database row locks.
ALTER TABLE change_log ALTER COLUMN seq DROP DEFAULT;
CREATE FUNCTION assign_change_log_commit_order() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(NEW.user_id);
    NEW.seq := nextval(pg_get_serial_sequence('change_log', 'seq'));
    RETURN NEW;
END;
$$;

CREATE TRIGGER change_log_commit_order
BEFORE INSERT ON change_log FOR EACH ROW
EXECUTE FUNCTION assign_change_log_commit_order();
