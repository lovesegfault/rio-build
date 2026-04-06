-- ═══════════════════════════════════════════════════════════════════
-- Migration 020 — CHECK constraint on tenants.tenant_name
--
-- PG-side enforcement of the NormalizedName invariant (trimmed,
-- non-empty, no interior whitespace). rio-common/src/tenant.rs
-- NormalizedName::new checks the same three properties at the Rust
-- layer; this makes the invariant defense-in-depth at the storage
-- layer too.
--
-- Effect: the rio-store auth.rs "normalization rejected a PG-stored
-- name" branch becomes provably dead for post-migration rows. Pre-
-- migration rows that violate the constraint fail ADD CONSTRAINT
-- below — surface them to the operator rather than silently accepting
-- bad data. Manual-INSERT and CreateTenant-bypass paths are covered.
--
-- `~` is POSIX regex match. `[[:space:]]` matches any whitespace
-- (space, tab, newline, CR, vtab, form-feed — the POSIX class, which
-- is what PG's default regex engine speaks). The regex check subsumes
-- trim-equality (leading/trailing whitespace would match the class),
-- but the trim check is cheaper and makes the intent explicit, so
-- keep all three.
--
-- NOT VALID → VALIDATE pattern is overkill for a table that's
-- typically <100 rows and a migration that runs at cold-start; direct
-- ADD is fine. If this fails on an existing row, the operator sees
-- exactly which tenant_name is malformed and can fix it before
-- re-running the migration.
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE tenants
    ADD CONSTRAINT tenant_name_normalized
    CHECK (
        tenant_name = trim(tenant_name)
        AND tenant_name <> ''
        AND tenant_name !~ '[[:space:]]'
    );
