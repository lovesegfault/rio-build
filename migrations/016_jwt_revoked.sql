-- migration 016: jwt_revoked — persistent JWT revocation (P0259 consumes).
--
-- USER Q4: PG-backed revocation. Gateway stays PG-free — SCHEDULER does
-- the lookup (it already has PG + does tenant resolve). Scheduler reads
-- jti from interceptor-attached Claims extension, NOT a proto body
-- field — the JWT is already parsed at r[gw.jwt.verify], zero wire
-- change needed.
--
-- Cleanup: a jti whose JWT has expired (exp < now()) is invalid
-- regardless of revocation, so rows older than max-JWT-lifetime can be
-- swept. No index for that — full scan during a maintenance sweep is
-- fine at expected row counts.

-- r[gw.jwt.verify] — scheduler-side revocation check lands in P0259
CREATE TABLE jwt_revoked (
    jti         TEXT PRIMARY KEY,
    revoked_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    reason      TEXT NULL
);

-- ------------------------------------------------------------------------
-- Audit trail: which JWT submitted this build? Handler reads jti from
-- Claims extension → INSERT. Client never sends it; zero wire change.
-- NULL for pre-migration rows and internal/system-triggered builds.
-- ------------------------------------------------------------------------
ALTER TABLE builds ADD COLUMN jwt_jti TEXT NULL;
