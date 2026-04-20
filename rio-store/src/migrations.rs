//! Migration commentary — the "living" half of `migrations/*.sql`.
//!
//! sqlx checksums migration files by content (SHA-384 over the full
//! file body, including comments). Editing a comment in a `.sql` file
//! changes the checksum → any persistent DB that already applied the
//! old checksum fails with `VersionMismatch` on next deploy. We hit
//! this twice pre-production (`76ba3999` renumber comment;
//! P0350 CASCADE dead-code note).
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

#![allow(dead_code)] // M_NNN doc-consts; never referenced, only `cargo doc`'d

// The try-then-wait advisory-lock wrapper moved to
// `rio_common::migrate` so rio-scheduler (which runs the SAME
// migration set against the SAME database) uses the same lock key
// and the same non-blocking strategy. Re-exported here so existing
// callers (`rio_store::migrations::run`) and the [`crate::gc`] doc
// cross-ref to `MIGRATE_LOCK_ID` keep working.
pub use rio_common::migrate::{MIGRATE_LOCK_ID, run};

/// `migrations/008_round4.sql`
///
/// FK CASCADE on `build_derivations.build_id` (Z1) and GIN index on
/// `narinfo."references"` (Z2) for the GC-sweep referrer re-check.
///
/// ## The Z2 comment is wrong about `= ANY()`
///
/// The frozen `.sql` says the GIN index "makes `WHERE $path =
/// ANY("references")` index-scannable". It does not — PostgreSQL's
/// array-GIN opclass only supports `@>` / `<@` / `&&` / `=`, and the
/// planner does NOT rewrite `scalar = ANY(arrcol)` into a `@>` probe.
/// I-145 measured ~1.3s/path seqscans at 100k+ rows under the original
/// `= ANY()` query. The sweep query (`rio-store/src/gc/sweep.rs`) was
/// rewritten to `n."references" @> ARRAY[$path]`, which EXPLAIN-
/// verifies as a Bitmap Index Scan on `idx_narinfo_references_gin`.
/// The index itself was always correct; only the comment and the
/// caller were wrong.
pub const M_008: () = ();

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
/// → `INSERT chunk_tenants` "same txn, no race." **It is not one
/// txn.** The "no race" property holds at *application* time
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

/// `migrations/020_tenant_name_check.sql`
///
/// ## Header overstates the guarantee
///
/// The frozen header claims "PG-side enforcement of the
/// `NormalizedName` invariant" and that a Rust-side rejection branch
/// is "provably dead". That holds only for ASCII whitespace: PG
/// `trim()` strips only U+0020 and POSIX `[[:space:]]` matches only
/// the six ASCII whitespace chars, while `NormalizedName::new`
/// (`rio-common/src/tenant.rs`) uses Unicode-aware `str::trim()` /
/// `char::is_whitespace()` (~25 codepoints incl. NBSP U+00A0). A name
/// like `'team\u{00A0}a'` passes this CHECK but is rejected by Rust —
/// a manual-INSERT (which the header explicitly scopes in) yields a
/// zombie row reachable by no normalized request.
///
/// ## Stale file reference
///
/// Header line 10 references "rio-store auth.rs", which does not
/// exist. The relevant Rust-side normalization lives in
/// `rio-common/src/tenant.rs`; tenant resolution callers are in
/// `rio-scheduler/src/grpc/` and `rio-scheduler/src/db/tenants.rs`.
///
/// ## Superseded
///
/// Migration 050 (`tenant_name_allowlist`) adds a strict ASCII
/// allowlist `^[a-zA-Z0-9._-]+$`, which is strictly stronger than
/// both 020's CHECK and Rust's whitespace check — any string passing
/// the allowlist has no whitespace of any kind. 020's
/// `tenant_name_normalized` constraint stays (cheap, redundant).
pub const M_020: () = ();

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
/// substitution (P0461..P0464). Follows the `tenant_keys` precedent
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
// r[impl store.substitute.upstream]
// r[impl store.substitute.sig-mode]
pub const M_026: () = ();

/// `migrations/027_cluster_key_history.sql`
///
/// Prior cluster signing keys for `sig_visibility_gate` verification
/// after rotation. Route I of P0521 — history-row pattern instead of
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
///
/// **Known defect:** the backfill predicate matches only `'completed'`,
/// missing `'skipped'` (added by M_021). See [`M_048`] for the recount.
pub const M_030: () = ();

/// `migrations/031_manifests_uploading_idx.sql`
///
/// Partial index on `manifests(updated_at) WHERE status = 'uploading'`
/// for the orphan scanner (`gc/orphan.rs::scan_once`). I-148: the scan
/// query `WHERE m.status = 'uploading' AND m.updated_at < now() -
/// make_interval(secs => $1)` had no covering index — only the PK on
/// `store_path_hash`. At ~1.5M manifest rows that's a ~4s Seq Scan
/// returning 0 rows, run periodically by every store replica (14×).
///
/// ## Why partial
///
/// `status` is two-valued (`uploading` / `complete`, migration 002
/// CHECK). At steady state, `uploading` rows are <100 (in-flight
/// uploads only); `complete` rows are the ~1.5M. A partial index
/// `WHERE status = 'uploading'` indexes only the in-flight rows — tiny
/// (kilobytes), and the predicate exactly matches the scan query so PG
/// uses it without a status filter step. Indexing `updated_at` (not
/// just the predicate) lets the `< now() - threshold` range scan as
/// well; the typical answer is "0 rows" via a single index probe.
///
/// EXPLAIN-verified: Index Scan on `idx_manifests_uploading_updated_at`
/// even at low row counts (the partial predicate makes the index small
/// enough that PG's cost model prefers it over a seq-scan regardless).
/// Dev-only `#[ignore]` sanity test at `gc/orphan.rs`
/// `scan_query_uses_uploading_partial_idx`.
///
/// ## Not CONCURRENTLY
///
/// sqlx wraps each migration in a tx; `CREATE INDEX CONCURRENTLY`
/// can't run inside one. Migrations run before the store starts
/// serving (P0543), so the brief `ACCESS EXCLUSIVE` on a write-idle
/// table is fine — no deadlock risk with concurrent uploads.
pub const M_031: () = ();

/// `migrations/032_derivations_size_class_floor.sql`
///
/// Nullable `size_class_floor TEXT` on `derivations` — persists the
/// I-170 reactive FOD promotion (legacy class-name floor) across
/// scheduler restart. P0556: without this, a scheduler failover
/// between an OOMKilled tiny-fetcher attempt and the retry resets
/// `DerivationState.size_class_floor` to `None` → the FOD goes back
/// to tiny → OOMs again. With ephemeral FetcherPools (one Job per
/// FOD, the production default since P0541) that's a guaranteed
/// wasted pod-start per failover; under chaos-monkey scheduler
/// restarts it's an OOM loop.
///
/// Written by `SchedulerDb::update_size_class_floor` at promotion
/// time (`record_failure_and_check_poison`). Loaded by
/// `load_nonterminal_derivations` → `from_recovery_row`. NOT in
/// `batch_upsert_derivations` (merge-time floor is always None; an
/// `ON CONFLICT DO UPDATE` there would clobber a promoted floor on
/// re-merge).
///
/// Nullable, no default — existing rows read NULL → `None` →
/// "smallest class" (same as fresh state). No backfill needed.
///
/// **Superseded by 044/045:** the per-dimension `resource_floor_*`
/// columns (M_044) replace the class-name string; M_045 drops
/// `size_class_floor`.
pub const M_032: () = ();

/// `migrations/033_chunks_uploaded_at.sql`
///
/// Nullable `uploaded_at TIMESTAMPTZ` on `chunks` — the commit point
/// for backend (S3) presence. Set by [`crate::metadata::chunked::
/// mark_chunks_uploaded`] AFTER a successful `ChunkBackend::put`;
/// cleared back to NULL when GC marks the chunk `deleted=true`.
///
/// **Race this closes (observed in production 2026-04-06):** under the
/// previous `RETURNING (refcount = 1)` heuristic, two concurrent
/// PutPaths sharing chunk X would have exactly one (the upsert
/// winner) attempt the S3 PUT. If that winner is SIGKILLed mid-upload
/// — as during a helm rolling update with ≥2 store replicas under
/// active traffic — the loser has already seen rc=2, skipped upload,
/// and completed its manifest. The orphan reaper later cleans the
/// winner's stale `'uploading'` row and decrements rc, but the
/// loser's `'complete'` manifest keeps rc>0 forever. Result: PG says
/// the chunk exists, S3 has nothing, GetPath returns DataLoss.
///
/// `(uploaded_at IS NULL)` instead: a chunk is skipped only when a
/// prior writer has confirmed the S3 object. Concurrent uncommitted
/// writers all upload (idempotent — same key, same bytes); the first
/// to call `mark_chunks_uploaded` wins the timestamp.
///
/// **Backfill:** sets `uploaded_at = created_at` for all existing
/// rows. This is a lie for chunks already stranded by the race above
/// — those need a separate scan-and-purge (PG vs S3 diff). Greenfield
/// deploys are unaffected (empty table).
pub const M_033: () = ();

/// `migrations/034_assignments_terminal_backfill.sql`
///
/// I-209/I-210: only `handle_success_completion` ever called
/// `update_assignment_status`. Every other derivation-terminal path
/// (poison, cancel, cache-hit-at-merge, orphan recovery,
/// FOD-from-store) left the active `assignments` row at `'pending'`.
/// `gc_orphan_terminal_derivations`' `NOT EXISTS assignments` then
/// matched nothing for those derivations, so they leaked unbounded —
/// 12,609 stuck rows on terminal derivations observed in production
/// before this migration. The Rust-side fix folds the assignment
/// terminal into `update_derivation_status[_batch]`/`persist_poisoned`;
/// this migration backfills the existing rows and switches the FK to
/// `ON DELETE CASCADE` so the (now-narrowed) pruner can delete a
/// derivation that still has terminal assignment rows.
///
/// Backfill maps `derivations.status` → `assignments.status` the same
/// way `terminal_assignment_status` does (`completed`→`completed`,
/// `cancelled`→`cancelled`, everything else → `failed`).
/// `completed_at` falls back to `derivations.updated_at` to preserve
/// rough timing for audit queries.
pub const M_034: () = ();

/// `migrations/035_drop_dead_rpc_tables.sql`
///
/// Drops `chunk_tenants`, `content_index`, and `narinfo.refs_backfilled`
/// — all three backed RPCs that were never wired into a production
/// caller (PutChunk/FindMissingChunks, ContentLookup, ResignPaths).
/// Chunking is server-side only; CA cutoff uses realisations; the
/// pre-refscan-fix data was greenfield-reset long ago.
///
/// `DROP TABLE` cascades the indexes; the standalone `DROP INDEX` for
/// `narinfo_refs_backfill_pending_idx` is belt-and-suspenders (PG drops
/// a partial index when its predicate column is dropped, but the
/// explicit drop documents intent and survives a reorder).
pub const M_035: () = ();

/// `migrations/036_drop_gc_roots.sql`
///
/// Drops the `gc_roots` explicit-pin table created in 005. It was
/// reserved as an "operator pin" extension point but never gained a
/// production writer (no `AddGcRoot` RPC, no `rio-cli` subcommand, no
/// controller reconciler). Mark/sweep paid a JOIN + per-swept-path
/// EXISTS subquery against a permanently-empty table on every GC.
///
/// Operator pinning, if needed, goes through `extra_roots` (scheduler
/// `ActorCommand::GcRoots`) or `scheduler_live_pins`; the grace window
/// covers transient cases. Per project posture (no dev-phase reserved
/// knobs without users), the table is dropped rather than wired.
pub const M_036: () = ();

/// `migrations/037_drop_write_only_cols.sql`
///
/// Drops five write-only/never-written columns surfaced by the dead-code
/// audit:
///
/// - `build_history.{ema_output_size_bytes, size_class,
///   misclassification_count}` — `size_class` was never written (the
///   001 comment said "informational for dashboards"; rio-dashboard
///   never grew a `build_history` view). The other two were written by
///   `update_build_history[_misclassified]` and never read back —
///   `read_build_history` SELECTs only the duration/mem/cpu EMAs the
///   estimator actually uses. The misclassification penalty's live
///   effect is the `ema_duration_secs` overwrite, which stays.
/// - `builds.requestor` — bound to `''` on every INSERT, never SELECTed.
///   The audit-trail role is served by `jwt_jti` (migration 016).
/// - `build_logs.byte_size` — compressed S3 object size, written by the
///   log flusher and never read. Dashboard log views resolve via
///   `s3_key` + `is_complete` + `line_count`.
///
/// `CompletionReport.output_size_bytes` (proto field 5) is kept for
/// wire compatibility — the builder still measures and sends it; the
/// scheduler simply stops persisting it.
///
/// **Superseded by 044/045:** the remaining `build_history` EMAs were
/// only read by the legacy size-class estimator; ADR-023's
/// `build_samples` (M_039) feeds the SLA fit instead. M_045 drops the
/// table.
pub const M_037: () = ();

/// `migrations/039_sla_telemetry.sql`
///
/// ADR-023 phase-1 telemetry: `build_samples` gains the columns the SLA
/// model fits on. `cpu_limit_cores`/`cpu_seconds_total`/`peak_cpu_cores`
/// feed T(c); `peak_disk_bytes`/`peak_io_pressure_pct` feed D and
/// storage-class bias; `version`/`tenant`/`hw_class` are key components;
/// `enable_parallel_building`/`prefer_local_build` are drv-declared
/// shortcuts; `outlier_excluded` is the MAD-reject flag (sample recorded
/// but excluded from fit).
///
/// Two indexes: `key_idx` for per-key ring-buffer reads,
/// `incremental_idx` for the `WHERE completed_at > $last_tick` refresh
/// path.
pub const M_039: () = ();

/// `migrations/040_sla_overrides.sql`
///
/// ADR-023 phase-6 operator overrides. One row pins a `(pname, system?,
/// tenant?)` key to a forced tier / `(cores, mem)` / capacity_type,
/// short-circuiting the fitted-curve solve. NULL `system`/`tenant` are
/// wildcards — `r[sched.sla.override-precedence]` resolves most-specific
/// first (`pname+system+tenant` > `pname+system` > `pname`). `cluster`
/// scopes a row to one deployment so a shared multi-region DB can carry
/// per-region pins. `expires_at NULL` = never expires; `created_by` is
/// audit-only (rio-cli stamps `$USER`).
///
/// `p50/p90/p99_secs` let an override carry a custom tier target
/// without naming a configured tier (deferred — phase-7 SlaExplain
/// surfaces them; phase-6 only reads `tier`/`cores`/`mem_bytes`/
/// `capacity_type`).
///
/// `lookup_idx` covers the hot path: `SlaEstimator::refresh` reads all
/// non-expired rows once per tick, then resolves in-memory.
pub const M_040: () = ();

/// `migrations/041_hw_perf.sql`
///
/// ADR-023 §Hardware heterogeneity, phase-10 normalization. Three objects:
///
/// - `hw_perf_samples` — append-only: each builder pod runs a ~5s
///   single-threaded CRC32 microbench at init and inserts `(hw_class,
///   pod_id, factor)` where `factor = REF_TIME / measured`. `pod_id` is
///   the executor_id (k8s pod name) so the view's `count(DISTINCT
///   pod_id)` floor isn't satisfied by one pod retrying.
/// - `interrupt_samples` — spot-interrupt / preemption telemetry per
///   hw_class (phase-10.5 capacity-type bias; written by the
///   controller's disruption watcher + 60s exposure flush, read by the
///   SLA solve). Bounded by M_047 (`event_uid` partial-unique dedup
///   for `kind='interrupt'`) and a 7-day age sweep in
///   `rio-scheduler/src/sla/cost.rs::sweep_interrupt_samples` for
///   `kind='exposure'` rows (the 24h-halflife EMA gives >7d ≈0 weight).
/// - `hw_perf_factors` view — per-hw_class median factor, gated on ≥3
///   distinct pods. The scheduler's `HwTable::load` reads this to map
///   wall-seconds → reference-seconds before fitting T(c). Median (not
///   mean) so one cold-neighbour outlier doesn't skew the class.
///
/// `hw_perf_samples` is append-only, no retention sweep — volume
/// bounded by pod churn (one row per pod start, M_046 upsert), and
/// keeping the full history lets the view's median converge as the
/// fleet grows.
pub const M_041: () = ();

/// `migrations/042_hw_cost.sql`
///
/// ADR-023 phase-13 hw-band + capacity-type targeting. Three objects:
///
/// - `hw_cost_factors` — cluster-local price snapshot. The lease-gated
///   spot-price poller writes EMA-smoothed `$/vCPU·hr` per `(region,
///   az, instance_type, capacity_type)`; `solve_full` joins to compute
///   `E[cost]` per `(band, cap)` candidate. PK is the full quad — one
///   row per spot-market cell, upserted each poll.
/// - `sla_ema_state` — generic decayed-EMA persistence so the poller
///   and the `λ[h]` interrupt-rate estimator survive scheduler restart
///   without re-warming. `key` is caller-namespaced
///   (`spot:{type}:{az}` / `lambda:{hw_class}`); `numerator`/
///   `denominator` carry the running decayed sums when the EMA is a
///   ratio (interrupts ÷ node-seconds).
/// - `builds.attempted_candidates JSONB` — ICE-backoff ladder
///   provenance: which `(band, cap)` pairs were tried before the
///   dispatched one. Forensics-only; the in-process `IceBackoff` map
///   is the live state.
///
/// `hw_cost_factors` is cluster-local (one scheduler deployment per
/// region) so `region`/`az` are denormalized into the key rather than
/// joined from a fleet table — the multi-region forward-compat work
/// (ADR-019) isn't load-bearing yet.
pub const M_042: () = ();

/// `migrations/043_sla_hardening.sql`
///
/// ADR-023 hardening from adversarial review:
///
/// - `hw_perf_samples_recent_idx` + `hw_perf_factors` 7-day window —
///   the 041 view aggregated all rows ever; a hw_class that ran 1000
///   gen-6 pods six months ago and 3 gen-6a pods today reports the
///   stale median. The window plus `(hw_class, measured_at DESC)`
///   index keeps the median fresh and the view scan bounded as the
///   append-only table grows.
/// - `sla_ema_state.cluster` PK + `interrupt_samples.cluster` —
///   ADR-023 §2.13 says these are per-cluster, but under the global-DB
///   topology every region's scheduler upserted the SAME `key` and
///   read every region's interrupt rows. `DEFAULT ''` keeps the
///   greenfield single-cluster path working with no config.
pub const M_043: () = ();

/// `migrations/044_resource_floor.sql`
///
/// D4 (legacy-sizer removal): per-dimension reactive floor replaces
/// the class-name `size_class_floor` (M_032). All three columns are
/// `bigint` — `deadline_secs` would naturally be `integer` (the
/// in-memory type is `u32`) but repeated doubling under a runaway
/// reactive loop would overflow `i32` at ~24 days; the read path
/// saturating-casts back to `u32`.
///
/// `size_class_floor` (M_032) is NOT dropped here — Phase 8 does that
/// once the SLA-only dispatch path lands and no recovery code reads
/// the legacy column.
pub const M_044: () = ();

/// `migrations/045_drop_legacy_sizer.sql`
///
/// Legacy-sizer removal Phase 8: `[sla]` is now mandatory (no
/// `Option<SlaConfig>` arm), so the pre-ADR-023 sizing inputs are
/// dead.
///
/// - `build_history` — per-`(pname,system)` EMA table (M_001/M_037
///   shape: `ema_duration_secs`/`ema_peak_memory_bytes`/
///   `ema_peak_cpu_cores`). The legacy `classify()` read it; the SLA
///   fit reads `build_samples` (M_039). The actor still emits the
///   `rio_scheduler_build_actual_vs_predicted` metric from the
///   in-memory fit; nothing reads the table.
/// - `derivations.size_class_floor` (M_032) — class-name reactive
///   floor. Replaced by the per-dimension `resource_floor_*` columns
///   (M_044); recovery loads those, not this.
pub const M_045: () = ();

/// `migrations/046_hw_perf_unique.sql`
///
/// `UNIQUE(hw_class, pod_id)` on `hw_perf_samples`. M_041's doc claimed
/// "a garbage row from a misbehaving pod is one rank in a median", but
/// nothing enforced one row per pod: the `hw_perf_factors` view's
/// `percentile_cont(0.5)` runs over EVERY row — only the `HAVING
/// count(DISTINCT pod_id) >= 3` is distinct. A compromised builder
/// could spam N inserts and dominate the median (cross-tenant: the
/// factor feeds every tenant's T(c) ref-second normalization). The
/// constraint plus `ON CONFLICT … DO UPDATE` in `AppendHwPerfSample`
/// makes the one-rank claim true. Greenfield dedup keeps the
/// highest-`id` (most recent) row per key before adding the constraint.
pub const M_046: () = ();

/// `migrations/047_interrupt_event_uid.sql`
///
/// `event_uid TEXT` + partial `UNIQUE WHERE event_uid IS NOT NULL` on
/// `interrupt_samples`. The controller's spot-interrupt watcher
/// consumes `watcher(...).applied_objects()`, which re-yields every
/// still-extant `SpotInterrupted` Event on every relist (controller
/// restart, apiserver restart, `resourceVersion too old`, routine
/// ~5min watch timeout). M_041's INSERT had no dedup column, so each
/// relist appended duplicate `kind='interrupt'` rows; `refresh_lambda`
/// then `SUM`med them into λ's numerator while exposure (timer-driven)
/// stayed correct → λ read high → `solve_full` biased away from spot.
///
/// `event_uid` carries the K8s Event `metadata.uid`; the scheduler's
/// `AppendInterruptSample` is `ON CONFLICT (event_uid) WHERE event_uid
/// IS NOT NULL DO NOTHING`. Partial index → exposure rows and legacy
/// rows (NULL uid) are unconstrained. DB-backed dedup (not an
/// in-process `HashSet`) so a controller restart — the most reliable
/// trigger; every rolling deploy re-counts the past hour — is covered.
pub const M_047: () = ();

/// `migrations/048_builds_denorm_recount_skipped.sql`
///
/// [`M_030`]'s backfill counted only `d.status = 'completed'`, but the
/// runtime model treats `Skipped` as completed (`dag/mod.rs`:
/// `Completed | Skipped => summary.completed += 1`, persisted via
/// `db/builds.rs::persist_build_counts`). M_021 added `'skipped'`
/// before 030 shipped, so persistent DBs that accumulated CA-cutoff
/// skips between those deploys had permanently undercounted
/// `completed_drvs`/`cached_drvs` for terminal builds — `ListBuilds`
/// reads the columns directly with no `derivations` join.
///
/// 030 is checksum-frozen (`migration_checksums_frozen`), so this
/// re-runs the aggregate with `IN ('completed','skipped')` instead of
/// editing 030 in place. The `b.status IN ('succeeded','failed',
/// 'cancelled')` guard restricts the rewrite to terminal builds so it
/// doesn't race the live `persist_build_counts` path on active builds
/// (which self-heal on the next tick anyway). Idempotent.
pub const M_048: () = ();

/// `migrations/049_narinfo_store_path_pattern_idx.sql`
///
/// `text_pattern_ops` index on `narinfo(store_path)` for
/// `query_by_hash_part`'s `LIKE '/nix/store/{hash}-%'` filter. Same
/// I-078 failure class as M_029, but M_029's default-opclass btree
/// only serves `LIKE 'prefix%'` under C/POSIX collation. Production
/// (Aurora / Bitnami) defaults to `en_US.UTF-8`, under which the
/// planner falls back to Seq Scan over ~1.5M-row `narinfo` on every
/// `wopQueryPathFromHashPart`.
///
/// CI couldn't catch it: both `rio-test-support/src/pg.rs` and
/// `process-compose.yaml` `initdb --locale=C`, under which the plain
/// btree DOES serve LIKE-prefix. M_029's stated beneficiaries were
/// equality filters; the LIKE caller was never covered. Keeping both
/// indexes — 029's for `=` / ordering, 049's for byte-prefix.
///
/// Hot-apply with `CREATE INDEX CONCURRENTLY` first; the migration's
/// `IF NOT EXISTS` then no-ops on deploy (same pattern as M_029).
pub const M_049: () = ();

/// `migrations/050_tenant_name_allowlist.sql`
///
/// Strict ASCII allowlist `^[a-zA-Z0-9._-]+$` on `tenants.tenant_name`,
/// superseding 020's weak `[[:space:]]` check (see [`M_020`] for the
/// Unicode-whitespace gap it left). Allowlist over Unicode-regex
/// because PG's POSIX classes are locale-dependent and `\s` does not
/// match NBSP under either `C` (CI) or `en_US.UTF-8` (prod); an
/// explicit allowlist is unambiguous and matches what tenant names
/// actually look like (the 8 OceanSprint tenants are all `[a-z0-9-]+`).
///
/// `NormalizedName::new` is NOT tightened to the allowlist in this
/// change — that's a Rust-side behaviour change with wider blast
/// radius (every CreateTenant/SubmitBuild caller). The migration alone
/// makes PG ⊇ Rust-rejection-set, satisfying 020's frozen-comment
/// intent. Tightening Rust to match is a separate follow-up.
pub const M_050: () = ();

/// `migrations/052_manifests_claim_id.sql`
///
/// `claim_id UUID` on `manifests`: ownership token for `'uploading'`
/// placeholders. `insert_manifest_uploading` generates a fresh v4 and
/// returns it; owner-side cleanup (`reap_one(ReapBy::Claim(id))`,
/// `abort_placeholder`, the PutPath drop-guard) filters
/// `AND claim_id = $2` so a late-firing cleanup CANNOT match a fresh
/// re-upload at the same `store_path_hash`. Before this column,
/// `reap_one(threshold=None)` filtered `status='uploading'` only —
/// status alone is not ownership: the orphan scanner (or stage_chunked
/// rollback) deletes A's row, B inserts a fresh one, then A's
/// `tokio::spawn`'d drop-guard fires and reaps B's narinfo + chunk
/// refcounts mid-upload. NULLable: only `'uploading'` rows carry it;
/// `'complete'` rows and pre-052 rows have NULL (matched only by
/// `ReapBy::Stale`).
pub const M_052: () = ();

// Add M_NNN consts for other migrations as commentary accumulates.
// Not all migrations need one — only those with non-obvious history,
// dead-code constraints, or "we chose X over Y" rationale. The .sql
// files carry the WHAT; this module carries the WHY.
