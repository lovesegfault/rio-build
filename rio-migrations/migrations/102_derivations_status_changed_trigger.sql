-- Commentary: see rio-migrations/src/migrations.rs M_102
CREATE FUNCTION derivations_stamp_status_changed() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.status_changed_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER derivations_status_changed_stamp
    BEFORE UPDATE ON derivations
    FOR EACH ROW
    WHEN (OLD.status IS DISTINCT FROM NEW.status)
    EXECUTE FUNCTION derivations_stamp_status_changed();
