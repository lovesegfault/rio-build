-- TEST FIXTURE (not a live migration). Body of the retired rio_app
-- role migration as applied to the persistent DB under version 069.
-- Kept verbatim so the ACL-strip regression test can reproduce the
-- legacy databases' state (ownership transfer + master membership)
-- before running ensure_roles over it.
-- NOTE: pg_has_role(..., 'SET') below is PG16-only — the fixture
-- replay requires PG >= 16 (devshell ships postgresql 18).
-- migration 065: rio_app role for RDS IAM database authentication.
--
-- EKS services connect as rio_app with a 15-minute IRSA-minted token
-- instead of the auto-rotating Aurora master password. The whole
-- migration is one DO block that degrades to a no-op wherever the
-- migrating user lacks the privileges RDS masters have: k3s runs
-- migrations as the unprivileged bitnami app user (no CREATEROLE),
-- local/test postgres has no rds_iam role. Those deployments never
-- use IAM auth, so skipping is correct, not a silent failure.
-- See rio-migrations/src/migrations.rs M_065 for the full rationale.
DO $$
DECLARE obj record;
BEGIN
    -- Roles are cluster-wide; this migration runs once per DATABASE.
    -- Parallel test databases on one shared instance can race the
    -- create, hence the existence check + duplicate handler. The
    -- insufficient_privilege handler is the k3s case: bitnami's app
    -- user cannot CREATE ROLE, and nothing below makes sense without
    -- the role, so warn and stop.
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'rio_app') THEN
        BEGIN
            CREATE ROLE rio_app WITH LOGIN;
        EXCEPTION
            WHEN duplicate_object OR unique_violation THEN NULL;
            WHEN insufficient_privilege THEN
                RAISE WARNING 'rio_app: migrating user % lacks CREATEROLE; skipping IAM role setup (expected on k3s/local postgres)', current_user;
        END;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'rio_app') THEN
        RETURN;
    END IF;

    -- rds_iam exists only on RDS/Aurora; membership is what switches
    -- the role from password auth to IAM-token auth there.
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'rds_iam')
       AND NOT pg_has_role('rio_app', 'rds_iam', 'MEMBER') THEN
        GRANT rds_iam TO rio_app;
    END IF;

    -- Schema rights BEFORE the ownership transfer below: ALTER ...
    -- OWNER TO rio_app requires the NEW owner to hold CREATE on the
    -- containing schema.
    GRANT USAGE, CREATE ON SCHEMA public TO rio_app;

    -- Ownership, not just privileges: once the IAM flip lands,
    -- store/scheduler run sqlx migrations as rio_app, and DDL against
    -- tables owned by the migrating master user (ALTER TABLE, DROP,
    -- CREATE INDEX ...) requires OWNERSHIP. PG16+ forbids the master
    -- granting ITSELF to rio_app (no ADMIN on own role), so instead:
    -- grant rio_app to the migrating user (allowed - the creator
    -- holds ADMIN on roles it creates) and transfer ownership of
    -- application objects to rio_app. Membership keeps the master
    -- acting as owner of everything rio_app owns (rollback path);
    -- rio_app-run migrations own what they must alter (forward path).
    IF current_user <> 'rio_app' THEN
        -- 'SET', not 'MEMBER': the creator's implicit PG16+
        -- membership is ADMIN-only (INHERIT FALSE, SET FALSE) and
        -- does not allow the ALTER ... OWNER below; the explicit
        -- grant adds SET+INHERIT.
        IF NOT pg_has_role(current_user, 'rio_app', 'SET') THEN
            BEGIN
                EXECUTE format('GRANT rio_app TO %I', current_user);
            EXCEPTION
                WHEN insufficient_privilege THEN
                    RAISE WARNING 'rio_app: cannot grant rio_app to %; skipping ownership transfer', current_user;
                    RETURN;
            END;
        END IF;
        -- SERIAL/IDENTITY-owned sequences are excluded: Postgres
        -- forbids changing their owner directly ("linked to table"),
        -- and they follow their table's owner automatically.
        FOR obj IN
            SELECT c.relkind, c.relname
            FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public'
              AND c.relkind IN ('r', 'p', 'S')
              AND pg_get_userbyid(c.relowner) = current_user
              AND NOT EXISTS (
                  SELECT 1 FROM pg_depend d
                  WHERE d.classid = 'pg_class'::regclass
                    AND d.objid = c.oid
                    AND d.deptype IN ('a', 'i')
              )
        LOOP
            EXECUTE format('ALTER %s public.%I OWNER TO rio_app',
                           CASE WHEN obj.relkind = 'S' THEN 'SEQUENCE' ELSE 'TABLE' END,
                           obj.relname);
        END LOOP;
    END IF;

    -- Mirror the access the master user has over application objects.
    -- Existing objects now; default privileges cover objects created
    -- by later migrations run as the current (master) user.
    GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO rio_app;
    GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public TO rio_app;
    ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL PRIVILEGES ON TABLES TO rio_app;
    ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL PRIVILEGES ON SEQUENCES TO rio_app;
END
$$;
