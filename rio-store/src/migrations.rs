//! Migration commentary — the "living" half of `migrations/*.sql`.
//!
//! sqlx checksums migration files by content (SHA-384 over the full
//! file body, including comments). Editing a comment in a `.sql` file
//! changes the checksum → any persistent DB that already applied the
//! old checksum fails with `VersionMismatch` on next deploy. We hit
//! this twice pre-production (`76ba3999` renumber comment;
//! [P0350] CASCADE dead-code note).
//!
//! **POLICY:** `migrations/*.sql` are **frozen** after they ship to
//! any persistent DB. Commentary, rationale, "why we chose X over Y",
//! dead-code notes — all go HERE, keyed by migration number. The
//! `.sql` files carry only the minimal SQL + a one-line pointer
//! (`-- Commentary: see rio-store/src/migrations.rs M_NNN`).
//!
//! When you need to explain a migration's behavior: add or extend the
//! `M_NNN` const below. Do NOT edit the `.sql`. The checksum-freeze
//! test at `rio-store/tests/migrations.rs` enforces this — a comment
//! edit to a shipped `.sql` fails CI with a pointer back here.
//!
//! [P0350]: ../../../.claude/work/plan-0350-chunk-tenants-junction-cleanup-on-gc.md

#![allow(dead_code)] // doc-only consts; never referenced, only `cargo doc`'d

/// `migrations/009_phase4.sql`
///
/// Phase 4 rollup: tenants table + FK backfill (Part A) and
/// `derivations.poisoned_at` persistence (Part B).
///
/// ## The header lies about Parts C/D
///
/// The frozen `.sql` header reads:
///
/// ```text
/// Part C (4b): path_tenants junction (appended later)
/// Part D (4c): build_samples (appended later)
/// ```
///
/// **Parts C and D were never appended to 009.** They shipped as
/// standalone migrations instead:
///
/// - Part C → `migrations/012_path_tenants.sql`
/// - Part D → `migrations/013_build_samples.sql`
///
/// The original plan was to grow 009 across sub-phases (append-only
/// within one file), but that broke once 009 shipped to a persistent
/// DB — appending to a shipped migration changes its checksum, same
/// `VersionMismatch` trap as editing a comment. So C/D became new
/// migration numbers. The header comment was already frozen by then
/// and can't be corrected in-place; this const is the correction.
pub const M_009: () = ();

/// `migrations/018_chunk_tenants.sql`
///
/// Adds `chunk_tenants(blake3_hash, tenant_id)` junction for
/// tenant-scoped `FindMissingChunks` dedup. Mirrors the `path_tenants`
/// precedent from migration 012: composite PK, FK CASCADE on both
/// sides, secondary index leading with `tenant_id` for the lookup
/// shape.
///
/// ## Why a junction, not a `tenant_id` column on `chunks`
///
/// Chunks are content-addressed. Tenant A uploads `glibc.so`; tenant
/// B uploads byte-identical content → same `blake3_hash`. A
/// single-column owner would either overwrite on conflict (B steals
/// A's attribution) or `DO NOTHING` (B told "missing" forever). The
/// many-to-many junction lets both tenants see the chunk as present.
///
/// ## CASCADE is dead code
///
/// The `chunks(blake3_hash)` FK has `ON DELETE CASCADE`, but chunks
/// are soft-deleted (`UPDATE SET deleted=TRUE`, never `DELETE FROM`)
/// — the CASCADE trigger never fires in practice. Junction rows are
/// explicitly `DELETE`d by `enqueue_chunk_deletes` (`gc/mod.rs`) in
/// the same transaction as the soft-delete (P0350). The FK + CASCADE
/// guard against a future hard-delete path only.
///
/// ## "No race" — true, but not for the stated reason
///
/// The original comment block claimed `PutChunk` does `INSERT chunks`
/// → `INSERT chunk_tenants` "same txn, no race." **True, but sqlx
/// runs each migration in autocommit mode** — the two `CREATE`
/// statements in 018 are NOT atomic (two separate autocommit
/// statements). The "no race" property holds at *application* time
/// because `PutChunk` does `INSERT chunks` (autocommit) THEN
/// `INSERT chunk_tenants` (autocommit) — two separate statements,
/// chunks commits BEFORE junction, FK satisfied sequentially. If the
/// junction insert fails, the chunk row stands unattributed; tenant's
/// next FindMissingChunks says "missing" → retry self-heals. See
/// chunk.rs "Not in a single transaction" for the honest code-side
/// note. Not one txn; not atomic; sequential-commit is the actual
/// reason no-race holds. P0295-T40 caught the original sloppy wording.
///
/// ## Renumber history
///
/// Originally shipped as `017` in the plan doc. Renumbered to `018`
/// after a migration-number collision with
/// `017_tenant_keys_fk_cascade.sql` (the P0332 incident). `76ba3999`
/// fixed stale "017" refs in code comments — and that comment edit
/// was the FIRST checksum-break instance. P0350's CASCADE note was
/// the second. P0353 freezes the `.sql` and moves commentary here to
/// prevent the third.
pub const M_018: () = ();

/// `migrations/023_chunks_refcount_nonneg.sql`
///
/// Adds `CHECK (refcount >= 0)` to `chunks`.
///
/// ## Why a CHECK, not just "the code is correct"
///
/// `chunks.refcount` is decremented by `decrement_and_enqueue`
/// (`gc/mod.rs`) — once per manifest that references the chunk. A
/// double-decrement bug (e.g. a retry path that re-runs the
/// decrement on a partially-committed batch) would drive refcount
/// negative. Without the CHECK, that's **silent**: the chunk sits
/// at `refcount = -1`, the GC sweep's `WHERE refcount = 0` never
/// matches, and the chunk leaks forever. Worse, the next legitimate
/// decrement takes it to -2, etc. — the chunk is permanently
/// unreachable to GC.
///
/// With the CHECK, the double-decrement fails at the source: the
/// `UPDATE chunks SET refcount = refcount - 1` raises a constraint
/// violation, the transaction rolls back, and the error surfaces
/// immediately (logged by the GC task's error handler) instead of
/// manifesting as unexplained storage growth months later.
///
/// ## Not a performance concern
///
/// PG evaluates CHECK constraints per-row on INSERT/UPDATE only.
/// The decrement path already touches the row; the extra `>= 0`
/// comparison is negligible.
pub const M_023: () = ();

// Add M_NNN consts for other migrations as commentary accumulates.
// Not all migrations need one — only those with non-obvious history,
// dead-code constraints, or "we chose X over Y" rationale. The .sql
// files carry the WHAT; this module carries the WHY.
