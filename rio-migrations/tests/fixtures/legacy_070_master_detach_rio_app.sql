-- TEST FIXTURE (not a live migration). Body of the retired master
-- detach migration as applied to the persistent DB under version 070.
-- Its REASSIGN OWNED rewrites owner-ACL entries and strips ALL of
-- rio_app's table/sequence privileges (the 2026-06-05 live incident);
-- the regression test replays it and asserts ensure_roles re-grants.
-- NOTE: pg_has_role(..., 'SET') below is PG16-only — the fixture
-- replay requires PG >= 16 (devshell ships postgresql 18).
-- migration 066: detach the master from rio_app; ownership back to it.
--
-- 065 granted rio_app to the migrating (master) user and transferred
-- table/sequence ownership to rio_app so rio_app-run migrations could
-- issue DDL. Two things changed after it shipped:
--   1. Migrations moved to a deploy-time runner that always connects
--      as the master (rio-migrate hook Job / `rio-store migrate`), so
--      rio_app never needs ownership.
--   2. With iam_database_authentication_enabled on the cluster, RDS
--      PAM treats INHERITED rds_iam membership as "IAM-only role":
--      the master (member of rio_app, itself a member of rds_iam)
--      lost password auth, which broke the migration hook itself.
-- Reassign application objects back to the migrating user and drop
-- its rio_app membership. rio_app keeps LOGIN, rds_iam, the DML
-- grants and the default privileges from 065. Same degrade-to-no-op
-- shape as 065. Commentary: see rio-migrations/src/migrations.rs M_066.
DO $$
BEGIN
    -- No rio_app (k3s/local: 065 skipped role setup) -> nothing to
    -- detach. Early RETURNs, not one AND chain: SQL does not promise
    -- evaluation order inside a boolean expression, and pg_has_role
    -- raises (not false) on a missing role.
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'rio_app')
       OR current_user = 'rio_app' THEN
        RETURN;
    END IF;

    -- 'SET' matches exactly the explicit SET+INHERIT grant 065 issued
    -- (the same predicate 065 checked before granting). NOT 'MEMBER':
    -- that is also true for the implicit ADMIN-only (INHERIT FALSE,
    -- SET FALSE) membership a PG16+ creator holds on roles it creates,
    -- which does not inherit rds_iam (cannot trip RDS PAM) and whose
    -- revocation would strip the master's ADMIN on rio_app. Membership
    -- absent => 065 never transferred ownership (it RETURNs before the
    -- transfer when it cannot grant) or a manual recovery already
    -- detached => no-op is correct. Superusers short-circuit
    -- pg_has_role to true (ephemeral test instances): there REASSIGN
    -- undoes 065's transfer and the REVOKE of the never-granted
    -- membership is a PG warning, not an error.
    IF pg_has_role(current_user, 'rio_app', 'SET') THEN
        BEGIN
            -- Order matters: REASSIGN acts with rio_app's privileges
            -- through the very membership the REVOKE then removes.
            REASSIGN OWNED BY rio_app TO CURRENT_USER;
            REVOKE rio_app FROM CURRENT_USER;
        EXCEPTION
            WHEN insufficient_privilege THEN
                RAISE WARNING 'rio_app: user % cannot detach from rio_app; skipping (run "REASSIGN OWNED BY rio_app TO <master>; REVOKE rio_app FROM <master>;" manually)', current_user;
        END;
    END IF;
END
$$;
