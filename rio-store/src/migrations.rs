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

/// `migrations/024_pending_deletes_unique.sql`
///
/// Adds a partial UNIQUE INDEX on `pending_s3_deletes(blake3_hash)`.
///
/// ## Why — the ON CONFLICT was a no-op
///
/// `enqueue_chunk_deletes` (`gc/mod.rs`) has always written:
///
/// ```sql
/// INSERT INTO pending_s3_deletes (s3_key, blake3_hash)
///   SELECT * FROM unnest(...) ON CONFLICT DO NOTHING
/// ```
///
/// but `pending_s3_deletes` (migration 005) had only `id BIGSERIAL
/// PRIMARY KEY` — no unique constraint on `s3_key` or `blake3_hash`.
/// `ON CONFLICT DO NOTHING` without a conflict target matches any
/// unique/exclusion violation; with none to match, the clause was
/// dead code. A chunk queued twice got two rows.
///
/// Not a correctness bug: drain re-checks `chunks.(deleted AND
/// refcount=0)` before the S3 DELETE (migration 006 TOCTOU fix), and
/// S3 DeleteObject is idempotent. The second drain sees the chunk
/// already gone, issues a redundant DELETE, removes its row. Waste,
/// not breakage.
///
/// The ON CONFLICT was clearly *intended* to dedupe — adding the
/// index makes it work.
///
/// ## Partial, because blake3_hash is nullable
///
/// Migration 006 added `blake3_hash` as nullable for back-compat
/// (pre-006 rows have NULL). A plain UNIQUE INDEX would allow
/// multiple NULLs anyway (PG treats NULLs as distinct by default),
/// but the partial `WHERE blake3_hash IS NOT NULL` makes the intent
/// explicit and keeps the index smaller.
///
/// ## Not a constraint
///
/// `ALTER TABLE ... ADD CONSTRAINT ... UNIQUE` can't have a WHERE
/// clause. `CREATE UNIQUE INDEX` can, and PG accepts it as an
/// ON CONFLICT arbiter just the same.
pub const M_024: () = ();

/// `migrations/025_rename_worker_to_builder.sql`
///
/// [ADR-019] builder/fetcher split: rename the `worker_id` columns to
/// `builder_id`. `ALTER TABLE ... RENAME COLUMN` (not an edit to the
/// source migrations) because `001_scheduler.sql` and
/// `004_recovery.sql` are checksum-frozen.
///
/// Four renames:
///
/// - `derivations.assigned_worker_id` → `assigned_builder_id`
///   (001:50)
/// - `assignments.worker_id` → `builder_id` (001:91)
/// - index `assignments_worker_idx` → `assignments_builder_idx`
///   (001:100)
/// - `derivations.failed_workers` → `failed_builders` (004:71)
///
/// The frozen `.sql` comments in 001/004 still say "worker" — they
/// can't be corrected in-place without breaking persistent-DB
/// deploys. Read them as "builder" post-025.
///
/// ## Rust-side query bindings
///
/// The `.sqlx/*.json` cache and the Rust query bindings that
/// reference these columns update in P0451 — this migration lands
/// first so P0451's `cargo xtask regen sqlx` sees the new column
/// names instead of failing on column-not-found.
///
/// [ADR-019]: ../../../docs/src/decisions/019-builder-fetcher-split.md
pub const M_025: () = ();

/// `migrations/026_tenant_upstreams.sql`
///
/// Per-tenant upstream binary-cache configuration for block-and-fetch
/// substitution ([P0461]..[P0464]). Follows the `tenant_keys` precedent
/// (migration 014): per-tenant config table, FK CASCADE on tenant
/// removal, surrogate SERIAL PK with a business-unique constraint
/// `(tenant_id, url)`.
///
/// ## Columns
///
/// - `url` — the upstream cache base URL (e.g.
///   `https://cache.nixos.org`). No trailing-slash normalization at
///   the schema level; P0462's fetch layer strips it.
/// - `priority` — lower tried first (`ORDER BY priority ASC`). Default
///   50 mirrors Nix's `nix.conf` `priority = 50` convention for
///   substituters.
/// - `trusted_keys` — `TEXT[]` of `name:base64(pubkey)` strings, same
///   shape as `narinfo.signatures` (migration 002) and Nix's
///   `trusted-public-keys`. A narinfo fetched from this upstream is
///   accepted iff at least one `Sig:` verifies against one of these.
/// - `sig_mode` — `keep | add | replace`. Controls what lands in
///   `narinfo.signatures` post-substitution:
///   - `keep`: upstream sigs stored as-is
///   - `add`: upstream sigs + a fresh rio sig (tenant key or cluster
///     key fallback)
///   - `replace`: upstream sigs discarded, only rio sig stored
///
/// ## Why a CHECK, not a PG ENUM
///
/// `sig_mode` is a closed three-value set — a natural PG ENUM
/// candidate. We use `TEXT + CHECK` instead because adding a fourth
/// mode later is a single `ALTER TABLE ... DROP CONSTRAINT ... ADD
/// CONSTRAINT` (one migration). Adding a value to a PG ENUM is `ALTER
/// TYPE ... ADD VALUE`, which cannot run inside a transaction before
/// PG 12 and still has sharp edges (new value invisible to concurrent
/// sessions until commit). The CHECK approach matches migration 001's
/// `derivations.status` precedent.
///
/// ## Index shape
///
/// `(tenant_id, priority)` — the substitution path's query is
/// `SELECT * FROM tenant_upstreams WHERE tenant_id = $1 ORDER BY
/// priority ASC`. Composite index makes that a single index scan
/// (no sort step).
///
/// [P0461]: ../../../.claude/work/plan-0461-upstream-substitution-foundation.md
/// [P0464]: ../../../.claude/work/plan-0464-upstream-substitution-validation.md
// r[impl store.substitute.upstream]
// r[impl store.substitute.sig-mode]
pub const M_026: () = ();

/// `migrations/027_cluster_key_history.sql`
///
/// Prior cluster signing keys for `sig_visibility_gate` verification
/// after rotation. Route I of [P0521] — history-row pattern instead of
/// GC re-sign.
///
/// ## Why this exists
///
/// The sig-visibility gate (grpc/mod.rs `sig_visibility_gate`) pushes
/// the cluster key into the trusted set so a freshly-built path
/// (rio-signed, `path_tenants` not yet populated) isn't rejected as
/// "untrusted substitution" during the PutPath→scheduler window. But
/// it pushed ONLY the current `Signer`'s pubkey.
///
/// After rotation: paths signed under the old cluster key, whose
/// `path_tenants` rows get CASCADE-deleted (tenant deletion), become
/// invisible — old sig doesn't verify against the new key, no
/// `path_tenants` row to bypass the gate.
///
/// ## Why not GC re-sign (Route II)
///
/// The spec previously prescribed GC-mark re-signing reachable paths
/// with the new key. Never implemented (zero `Signer` refs in
/// `gc/mark.rs`). Would change GC's write profile: mark is currently a
/// ~1s read-only CTE; re-sign = N SIGNATURE + N UPDATE per cycle.
/// Route I is readpath-only — matches the `tenant_keys` precedent.
///
/// ## `pubkey` column format
///
/// Full `name:base64(pubkey)` string (what `Signer::trusted_key_entry`
/// returns), NOT raw pubkey bytes. `any_sig_trusted` matches
/// signatures by name first (`keys.iter().find(|(n, _)| *n ==
/// sig_name)`), so the name is load-bearing. Storing the entry-format
/// string means zero parsing at gate time — just `Vec::extend`.
///
/// ## `retired_at` semantics
///
/// NULL = old key still within grace period, gate trusts it.
/// Non-NULL = grace expired; row retained for audit only. The loader
/// query filters `WHERE retired_at IS NULL`.
///
/// [P0521]: ../../../.claude/work/plan-0521-cluster-key-rotation-contradiction.md
// r[impl store.key.rotation-cluster-history]
pub const M_027: () = ();

/// `migrations/028_drop_derivations_fks.sql`
///
/// Drop the three FKs referencing `derivations(derivation_id)` from
/// `derivation_edges` (parent_id, child_id) and `build_derivations`
/// (derivation_id). [P0539] perf — `persist_merge_to_db` for a
/// 1085-node closure spent ~20s in FK validation.
///
/// ## Why drop instead of DEFERRABLE
///
/// `DEFERRABLE INITIALLY DEFERRED` moves the per-row trigger to
/// COMMIT but still does N PK lookups. The DAG actor is the SOLE
/// writer (`persist_merge_to_db`, `merge.rs:616-674`): one tx that
/// inserts derivations first (line 619) then edges/build_derivations
/// referencing the just-returned `id_map`. Referential integrity is
/// structural in that code path; the FK check is redundant validation
/// of UUIDs the application just round-tripped from the same tx.
///
/// ## What's NOT dropped
///
/// `build_derivations.build_id_fkey` (→ `builds`, `ON DELETE CASCADE`
/// since migration 008) is kept. `delete_build` (`db/builds.rs:178`)
/// relies on the cascade for `cleanup_failed_merge` rollback.
///
/// `assignments.derivation_id_fkey` is also untouched — assignments
/// are inserted one-at-a-time on dispatch, not in the merge batch hot
/// path.
///
/// [P0539]: ../../../.stress-test/issues/2026-03-31-stress-findings.md
pub const M_028: () = ();

/// `migrations/029_narinfo_store_path_idx.sql`
///
/// Index on `narinfo(store_path)`. I-078: `query_path_info` /
/// `find_missing_paths` / `get_manifest` filtered `WHERE n.store_path
/// = $1` — the only narinfo index was the PK on `store_path_hash`, so
/// every QPI was a Seq Scan. Under autoscaled-builder fan-out (60
/// builders × ~100 input paths each), every PG connection sat seq-
/// scanning 56k rows; surfaced as `sqlx::pool::acquire 16s` and was
/// initially misread as pool exhaustion (I-076).
///
/// The hot-path queries now compute `store_path_hash` client-side and
/// use the PK (`metadata/queries.rs`). This index is defense-in-depth
/// for the remaining text-filter callers (`append_signatures`, GC mark
/// CTE walks `references` text-array, ad-hoc operator queries).
///
/// Hot-applied with `CREATE INDEX CONCURRENTLY` on 2026-04-02 EKS;
/// the migration runs non-CONCURRENTLY (sqlx wraps in a tx, and
/// CONCURRENTLY can't run inside one) but `IF NOT EXISTS` makes the
/// hot-applied case a no-op.
pub const M_029: () = ();

/// `030_builds_denorm_counts.sql` — denormalize total/completed/cached
/// drv counts onto `builds` (I-103).
///
/// `LIST_BUILDS_SELECT` previously did `builds ⟕ build_derivations ⟕
/// derivations` with COUNT aggregation + a correlated `NOT EXISTS
/// (assignments)` for `cached`. The LIMIT applied AFTER the GROUP BY,
/// so listing 10 builds aggregated EVERY drv of EVERY build. I-102
/// showed it going 16ms→2.3s with stale stats at only 10 builds × ~5k
/// drvs; at 1000 builds × 5k it'd be 5M rows/call regardless of stats.
///
/// Counts are now columns maintained by `update_build_counts()` (sets
/// from in-mem ground truth at merge + every completion). The backfill
/// SELECT replicates the original aggregation once, then it's never
/// joined again. Recovery re-runs `update_build_counts` for active
/// builds, so a missed best-effort write self-heals on failover.
///
/// Semantic note: `cached_drvs` is now "merge-time hits + dispatch_fod
/// short-circuits" (the in-mem `cached_count`). The original SQL's
/// `NOT EXISTS (assignments)` heuristic is equivalent in practice —
/// both mean "completed without dispatch".
pub const M_030: () = ();

// Add M_NNN consts for other migrations as commentary accumulates.
// Not all migrations need one — only those with non-obvious history,
// dead-code constraints, or "we chose X over Y" rationale. The .sql
// files carry the WHAT; this module carries the WHY.
