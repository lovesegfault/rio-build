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
//! (`-- Commentary: see rio-migrations/src/migrations.rs M_NNN`).
//!
//! When you need to explain a migration's behavior: add or extend the
//! `M_NNN` const below. Do NOT edit the `.sql`. The checksum-freeze
//! test at `rio-migrations/tests/migrations.rs` enforces this — a
//! comment edit to a shipped `.sql` fails CI with a pointer back here.
//!
//! **Stale `.sql` headers are intentional.** Migrations that shipped
//! before this crate was extracted carry headers like `-- Commentary:
//! see rio-store/src/migrations.rs M_NNN` — that file moved here, but
//! editing the headers would change the checksum and break every
//! persistent DB that already applied them. The pointers in this
//! file's `M_NNN` consts are authoritative; the `.sql` headers are
//! frozen at whatever path was current when the migration shipped.
//!
//! The try-then-wait advisory-lock runner that applies these lives in
//! `rio_migrations::migrate` (`run_with_roles`, called by `rio-store
//! migrate`); app services only verify via `assert_current`. Role and
//! grant management is deliberately NOT in this migration set — see
//! `src/ensure_roles.rs` for why frozen SQL is the wrong home for
//! desired-state reconciliation.
//!
//! **NUMBERING POLICY:** migration numbers are allocated by the
//! branch that deploys to a persistent DB. A stacked branch's
//! migrations are review artifacts; whichever branch integrates
//! second renumbers ONLY migrations that no persistent DB has
//! applied, adopting the deployed branch's files byte-identically.
//! Numbers a persistent DB has recorded are never reused — see the
//! burned-numbers note after `M_064` below. With
//! `ignore_missing(true)` permanently on in the runner
//! (`migrate.rs`), sqlx no longer errors on an orphaned applied row:
//! accidentally RENUMBERING an already-applied migration (same
//! content, new version) would silently RE-APPLY its SQL on the
//! persistent DB instead of failing `VersionMissing`. The reverse
//! check in `migration_checksums_frozen` ("PINNED lists migration v
//! but migrations/ has no such file") is now the ONLY guard against
//! that — do not weaken it.

#![allow(dead_code)] // M_NNN doc-consts; never referenced, only `cargo doc`'d

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
///
/// **Second regression (same index, different shape):** the rewrite
/// initially inlined the hash→path resolution as a scalar subquery in
/// the array constructor (`@> ARRAY[(SELECT store_path FROM narinfo
/// WHERE store_path_hash = $1)]`), and the earlier "EXPLAIN-verified
/// even with the InitPlan subquery" claim here did not hold at scale:
/// at 225k narinfo rows the planner seq-scanned again (51.6 ms/probe,
/// 2 probes/path — 98% of sweep time once the file_blobs cascade was
/// fixed by 071). The probe now resolves the path in Rust and binds
/// TEXT directly (`LIVE_REFERRER_PROBE_SQL` in sweep.rs, with an
/// EXPLAIN regression test).
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
/// [ADR-019]: ../../../docs/spec/components/fetcher.typ
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
/// to tiny → OOMs again. With ephemeral fetcher Jobs (one Job per
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
/// for backend (S3) presence. Set by rio-store's
/// `metadata::chunked::mark_chunks_uploaded` AFTER a successful
/// `ChunkBackend::put`; cleared back to NULL when GC marks the chunk
/// `deleted=true`.
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

/// `migrations/051_derivations_resubmit_cycles.sql`
///
/// `resubmit_cycles` on `derivations`: counts poison→resubmit reset
/// events (NOT per-attempt retries). Splits the cross-cycle accumulator
/// from `retry_count`: a single counter cannot be both
/// per-cycle-reset (`max_retries` gate) and cross-cycle-accumulated
/// (`POISON_RESUBMIT_RETRY_LIMIT` gate). With both roles on
/// `retry_count`, the per-cycle cap (`max_retries=2`) was the
/// permanent ceiling, so `2 < 6` was always true and the resubmit
/// bound never fired (bug_152). `resubmit_cycles` is incremented in PG
/// by `clear_poison_batch` (the resubmit-reset chokepoint) so the
/// bound survives leader failover; `clear_poison` (admin/TTL) zeroes
/// it as a full reset.
pub const M_051: () = ();

/// `migrations/053_build_logs_first_line.sql`
///
/// `first_line bigint` on `build_logs`: the worker-assigned line number
/// of the FIRST line in the S3 blob. Non-zero iff the ring buffer
/// evicted (>100k lines). Before this column the blob's offset was
/// discarded on `LogBuffers::drain()`; `try_s3` then compared the
/// client's `since` cursor (true-line-number space) against
/// `line_count` (≤100k survivors) — `since=120000` against a 150k-line
/// build short-circuited at `120000 >= 100000` and silently dropped the
/// final 30k lines (bug_084). `DEFAULT 0` is backwards-compat for
/// pre-053 rows: at worst the client re-receives lines it already has.
pub const M_053: () = ();

/// `migrations/054_hw_perf_jsonb.sql`
///
/// `hw_perf_samples.factor` f64 → jsonb K=3 vector (ADR-023 §13a). The
/// `USING jsonb_build_object('alu', factor)` cast preserves shipped
/// scalar rows as `{"alu": <f64>}`; key-addressed so future K-change
/// pads rather than invalidates. View `hw_perf_factors` (043) is
/// dropped first because PG refuses an `ALTER COLUMN ... TYPE` on a
/// column a view depends on; the per-dimension MAD-reject the view did
/// moves to app-side `HwTable` aggregation.
/// +`submitting_tenant` for §Threat-model gap (b) median-of-medians.
pub const M_054: () = ();

// M_055 deleted: `hw_cost_factors` EMA-state columns were dead schema —
// the EMA persist they described shipped via `sla_ema_state` (042's
// other table; see `cost.rs` `CostTable::{load,persist}`). The
// `hw_cost_factors` base table itself (042) is also dead — DROP
// deferred to a follow-up migration since 042 is frozen. sqlx tolerates
// the version gap; renumbering 056-058 would churn `PINNED` for nothing.

/// `migrations/056_build_samples_enable_parallel_checking.sql`
///
/// `build_samples.enable_parallel_checking` (ADR-023 §13a): the
/// `enableParallelBuilding && !enableParallelChecking` distinction
/// matters for the §Model-staging p̄:=1 seed. Sibling
/// `enable_parallel_building` already exists from 039.
pub const M_056: () = ();

/// `migrations/057_build_samples_is_fixed_output.sql`
///
/// `build_samples.is_fixed_output` (ADR-023 §13a §A17): FOD fleet-prior
/// exclusion is keyed on the derivation's output-spec (`outputHashMode`
/// set), NOT pname-absence. Named FODs exist (`fetchurl { name = … }`)
/// and would otherwise pollute `fleet_median` with download-time
/// outliers. The column is informational on the row (the SLA fit per
/// `(pname, system, tenant)` ring is unchanged); `SlaEstimator::refresh`
/// reads it to set `FittedParams.is_fod` so the fleet-aggregate filter
/// can exclude.
pub const M_057: () = ();

/// `migrations/058_sla_config_epoch.sql`
///
/// Persists the active `[sla].reference_hw_class` per cluster so a
/// scheduler restart (the only `[sla]` config-change path — there is no
/// SIGHUP hot-reload) can detect a reference change. Replaces the dead
/// `SlaConfig::validate_reload`, which guarded the same invariant on a
/// code path that never ran (no `prev` config across restarts).
///
/// `epoch` bumps on each accepted reference change; not read by the
/// scheduler today but lets a future corpus-export carry "valid for
/// epoch N" so stale ref-second data can be rejected at import.
///
/// `build_samples` / `hw_perf_samples` are NOT cluster-scoped (no
/// `cluster` column), so the reset on reference change TRUNCATEs them
/// unconditionally. Multi-cluster shared-DB deploys (ADR-023 §2.13)
/// must coordinate reference changes across regions — acceptable for a
/// rare, operator-intended, destructive operation.
pub const M_058: () = ();

/// `migrations/059_nodeclaim_cell_state.sql`
///
/// ADR-023 §13b @alg-pool: per-`(hw_class, capacity_type)` quantile-sketch
/// state for the `nodeclaim_pool` reconciler's lead-time forecast.
/// `z_sketch_*` is the demand-to-Registered lead time; `boot_sketch_*`
/// is Launch→Registered alone (the Karpenter+kubelet overhead the
/// reconciler can't compress). Active/shadow pairs let the reconciler
/// rotate at `sketch_epoch` without losing the warm quantile during
/// the cold-start window. Sketches are persisted as version-tagged
/// HdrHistogram V2 `bytea` (originally postcard-encoded
/// sketches-ddsketch; the version tag is what made that swap a
/// re-learn instead of a migration) — `bytea` not `jsonb` because the
/// payload is a packed counts array, not a JSON-friendly shape.
///
/// `idle_gap_events` (jsonb) is the consolidator's recent reap log —
/// small (capped ring), read-rarely, schema-evolving; jsonb avoids a
/// second migration when the event shape changes.
///
/// Read/written by **rio-controller** only, so this table is
/// not covered by `cross_service_schema_contract` (which scopes to
/// rio-store reads); revisit if the persist moves behind a store RPC.
pub const M_059: () = ();

/// `migrations/060_sla_observed_instance_types.sql`
///
/// ADR-023 §13a R24B7: per-cell instance-type menu, populated by
/// controller-observed `node.kubernetes.io/instance-type` feedback via
/// `AckSpawnedIntents` (option-i autodiscovery — observing what
/// Karpenter resolves rather than reimplementing its label-derivation).
/// `last_observed` is the most recent time the controller reported this
/// `(cell, type)` pair; written from `InstanceType.last_observed`
/// (data-time, NOT `now()`) so a future eviction sweep has a real
/// recency signal. `cores`/`mem_bytes` are kubelet allocatable (the
/// bin-pack view), not nominal — ~3% systematic under-count, uniform
/// across cells so cost ranking is unaffected.
///
/// Read/written by **rio-scheduler** only (`CostTable::load`/`persist`).
pub const M_060: () = ();

/// `migrations/061_drv_logs.sql`
///
/// Re-key build log storage by derivation execution. Replaces
/// `build_logs (build_id, drv_hash)` with `drv_logs (exec_id PK, drv_hash)`.
/// One row/blob per execution instead of N per interested build — the
/// scheduler dedups derivations across all concurrent builds, so a build log
/// is a property of the *execution*, not the build request that asked for it.
///
/// `exec_id` is UUIDv7 (time-sortable), minted by the scheduler at
/// `assign_to_worker`. The PK is `exec_id` alone — globally unique by
/// construction, schema-enforced — with two secondary indexes:
/// `drv_logs_drv_latest (drv_hash, exec_id DESC)` for the latest-exec lookup,
/// and `drv_logs_started_at (started_at)` for the TTL GC sweep
/// (`LogFlusher::sweep_expired_logs`). The latter exists because the sweep's
/// `IN (SELECT … LIMIT N)` subquery cannot short-circuit a seq scan on a
/// sub-`LIMIT` pass — without it, the empty terminal pass that breaks the GC
/// loop scans the full heap on every hourly tick. Same pattern as
/// `idx_build_event_log_created` (003) and `build_samples_completed_at_idx`
/// (013). `drv_hash` is the 32-char `drv_log_hash()` form of the `.drv` store
/// path, NOT `derivations.drv_hash` (the polymorphic dedup identity: full path
/// for IA, modular hash for CA — they cannot be joined directly).
///
/// Adds `assignments.exec_id` (recovery carrier — the new leader reloads it
/// for currently-dispatched derivations after failover so the flusher keys
/// subsequent uploads correctly; a reset drv's leaked `pending` row is not
/// reloaded) and `build_derivations.exec_id` (build↔exec
/// correlation — set by the completion handler on terminal paths where an
/// execution ran: `Completed`, `Poisoned`, `Cancelled` reached from
/// `Assigned`/`Running`, and any terminal reached while a prior, reset
/// execution's stamped log buffer is retained (build-cancel sweep,
/// failed-substitute revert, dependency-failure cascade); `NULL` for
/// never-dispatched cascade-swept `DependencyFailed`/`Skipped`/
/// cache-hit `Completed`/never-dispatched/non-terminal — see
/// `sched.merge.exec-correlation+7`).
///
/// `drv_logs.line_count` records the execution's TRUE line span: when a
/// flush folds a recovered pre-failover prefix and inserts a
/// `[rio: ~N earlier lines lost across scheduler failover]` marker, the lost
/// range is counted even though the blob replaces it with that single line,
/// so `first_line + line_count` is one past the last true worker line
/// (`obs.log.gap-span`); the same holds when the flush payload itself
/// carries an unmarked interior hole (lines only an unflushed interim
/// leader received) — the hole is counted with no stand-in line at all;
/// the blob's physical line count may therefore be smaller, which is how
/// `GetDerivationLogs` detects such blobs.
///
/// Greenfield drop+recreate, no backfill.
pub const M_061: () = ();

/// `migrations/062_derivation_wanted_outputs.sql`
///
/// Demand-driven cache-hit criterion: `derivations.wanted_output_names`
/// is the subset of `output_names` that any consumer actually references
/// (union over every parent's `inputDrvs` output-name set for this drv,
/// ∪ the root request's `OutputsSpec`). The scheduler consults it ONLY
/// for the cache-hit / substitutability classification — a missing
/// output nothing consumes (`glibc-debug`) must not condemn the
/// derivation and its build-time closure to a from-source rebuild. The
/// assignment-token allowlist, GC pins, and the client-facing output
/// report keep using the full `output_names`/`expected_output_paths`
/// pair.
///
/// `'{}'` (the column DEFAULT) means "all declared outputs wanted":
/// pre-migration rows, the `wopBuildDerivation` inline-`BasicDerivation`
/// fallback, and `^*` roots all degrade to the old conservative
/// all-outputs criterion.
///
/// Upserted with **union-on-conflict + empty-saturation** semantics
/// (see `rio-scheduler/src/db/batch.rs`): the wanted set only ever
/// grows for a given `drv_hash`. Overwrite would let a later build's
/// narrower set un-want an output an earlier, still-live build needs.
/// Because empty is the "all" sentinel, `all ∪ X = all`: if either the
/// existing array or the incoming array is empty the result is `'{}'`,
/// otherwise the sorted distinct union. Mirrors
/// `DerivationState::union_wanted`.
///
/// Read/written by **rio-scheduler** only.
pub const M_062: () = ();

/// `migrations/063_derivation_topdown_pruned.sql`
///
/// Failover-safe persistence of the scheduler's `topdown_pruned`
/// marker. When the roots-only prune fires, the kept (demanded) nodes
/// are merged WITHOUT their dependency closure, so they MUST complete
/// via substitution — a from-source dispatch would ENOENT on inputDrvs
/// that were never merged. The marker was previously in-memory only:
/// after a leader failover a pruned root persisted as `substituting`
/// was recovered childless, came back Ready, and — when its stored
/// wanted union contained an output that was genuinely missing and not
/// substitutable — was dispatched from source with no input-presence
/// check (doomed dispatch → worker ENOENT → wrong-reason Poisoned →
/// every interested build fails). Persisting the flag lets the new
/// leader restore it and take the fail-fast (resubmit-directing) arm
/// instead.
///
/// Written `true` — inside the same transaction that persists a pruned
/// merge — for the kept nodes whose dependency closure the prune
/// dropped and whose existing children the closure classifier does not
/// vouch for at stamp time (a dep-less demanded leaf never had a
/// closure to drop and is not marked, and a kept node whose child set
/// is Vouched — at least one child, all of them Completed/Skipped, no
/// closure hole — has its closure in the store; childless kept nodes
/// ARE marked, present-but-unbuilt children do not exempt a node, and
/// a closure-holed node is stamped even if its surviving children are
/// all produced — see `closure_vouched` in `actor/merge.rs`);
/// **OR-combined on conflict**
/// (`derivations.topdown_pruned OR EXCLUDED.topdown_pruned`) so an
/// unrelated non-pruned merge of the same drv never clears it; cleared
/// only once the node's children are all produced (by the
/// post-reconciliation clear pass in `handle_merge_dag` at merge time,
/// by the completion-time `clear_topdown_pruned_for_produced_parents`
/// when children become produced later, by the recovery-time gate that
/// drops a restored mark whose persisted children are all produced and
/// vouched for by a still-live build that also owns the parent, or by
/// the lazy walk-failure backstop in `handle_substitute_complete`)
/// and by the topdown fail-fast when it consumes the marker.
/// The header comment in the frozen `.sql` keeps the original
/// childless-era wording — this doc-const is the corrected record. See
/// `rio-scheduler/src/db/batch.rs` and `actor/merge.rs`.
///
/// Read/written by **rio-scheduler** only.
pub const M_063: () = ();

/// `migrations/064_derivation_closure_hole.sql`
///
/// Failover-safe persistence of the scheduler's `closure_hole`
/// breadcrumb — the qualifier that travels with the `topdown_pruned`
/// marker (`M_063`). It records that a terminal build's cleanup reaped
/// an un-produced child out from under the node, so the node's
/// surviving (persisted and in-memory) children are a truncated view of
/// its pruned input closure and must not vouch for a from-source
/// dispatch or launder a mark clear. The breadcrumb was previously
/// in-memory only, justified by "PG still holds the un-produced child's
/// terminal row" — but the orphan-terminal GC
/// (`gc_orphan_terminal_derivations`) deletes exactly that row and its
/// edges, after which a leader failover saw only the produced
/// survivors, cleared the restored mark, and re-armed the doomed
/// from-source dispatch the mark exists to prevent.
///
/// Written `true` **best-effort** by the leader-gated survivor hook in
/// `actor/build.rs::handle_cleanup_terminal_build` (the in-memory hole
/// is set by `remove_build_interest_and_reap` on leaders and standbys
/// alike; only the leader persists it, before the per-parent verdict
/// loop — a survivor that loop immediately fail-fasts keeps the
/// breadcrumb, since the fail-fast consume is mark-only), by the
/// recovery-time stamp in `actor/recovery.rs::load_dag_from_rows` when
/// the new leader drops an edge to an un-produced terminal child of a
/// recovered parent — the recovery-side analogue of the same removal,
/// which sets the in-memory breadcrumb and persists it in one place —
/// and by the poison-clear paths
/// (`actor/completion.rs::handle_clear_poison`,
/// `actor/housekeeping.rs::tick_process_expired_poisons`) for the
/// surviving parents of the Poisoned (by definition un-produced) child
/// they remove, both leader-only by construction (admin leader guard /
/// standby tick no-op). A crash or lease loss between the removal and
/// the stamp loses the persisted breadcrumb — same accepted best-effort
/// posture as every `topdown_pruned` clear; the recovery-time stamp
/// narrows that window by re-deriving the hole from the dropped child's
/// still-present row at the next failover.
/// The hook does not filter on `topdown_pruned`, so the column may
/// also be set for surviving parents that are not (yet) marked; it is
/// inert there because every consumer requires the mark (a hole on an
/// unmarked node only ever matters by keeping the conservative stamp
/// when a later pruned merge would otherwise have exempted that node).
/// **OR-combined on conflict**
/// (`derivations.closure_hole OR EXCLUDED.closure_hole`); the merge-time
/// upsert always binds `false`, so a later non-edge-declaring merge of
/// the same drv can never clear it through the upsert. Cleared by the
/// merge-time heal (`clear_closure_hole_by_hashes` — a full merge
/// re-declares the node's edges, so its child set is representative
/// again) and by the batched Vouched-keyed mark clears
/// (`clear_topdown_pruned_by_hashes` — the breadcrumb travels with the
/// mark those sites clear); the singular `clear_topdown_pruned_by_hash`
/// (the lazy walk-failure clear and the fail-fast consume) is
/// mark-only, so the fail-fast retains the breadcrumb for the directed
/// resubmit it solicits. Restored by `from_recovery_row`
/// (`from_poisoned_row` keeps `false`) and consulted by the
/// recovery-time produced-children gate: a holed flagged parent is
/// never enrolled as a clear candidate, so it keeps the restored mark
/// and at worst takes the bounded resubmit-directing fail-fast. A stale
/// `true` left by a lost best-effort clear errs the same conservative
/// direction. `DEFAULT false` means no backfill and old binaries'
/// column-naming INSERTs keep working during a rolling deploy.
///
/// Read/written by **rio-scheduler** only.
pub const M_064: () = ();

/// `migrations/065_nar_index.sql`
///
/// ADR-022 castore schema, all in one migration so it's pinned once:
///
/// - **P0551** (this commit): `nar_index` (`entries` = encoded
///   `rio.types.NarIndex`) + `manifests.nar_indexed` partial-index
///   work-queue (same pattern as 031). Derived state, FK→`manifests`
///   `ON DELETE CASCADE`. PG forbids cross-table partial-index
///   predicates, hence the same-table bool flag (HOT-update eligible).
/// - **P0572** (pre-created): `nar_index.root_node`, `directories` +
///   `directory_tenants` (refcounted content-addressed `Directory`
///   bodies), `file_blobs` + `file_blob_tenants` (`(file_digest,
///   manifest)` junction — GC of one referrer leaves the other's row).
/// - **P0581** (pre-created): `narinfo.compat_file_hash` for
///   legacy-binary-cache compat-write GC coupling.
/// - **P0586** (pre-created): `chunks.durable` + partial index —
///   "S3-PUT-confirmed" presence flag closing the I-201 WAL-window
///   race in `HasChunks`/`FindMissingChunks`.
///
/// **NOT here:** P0583's `DROP COLUMN inline_blob` — the store still
/// reads it (`metadata::get_manifest`, `cas.rs`, `get_path.rs`); that
/// DROP gets its own migration once the inline-storage path is gone.
///
/// Plan called this `054`; 054-061 landed first (061 went to
/// `drv_logs` on main) and 062-064 (wanted-outputs, topdown_pruned,
/// closure_hole) landed while ADR-022 was in flight, so it's 065.
pub const M_065: () = ();

/// 066 — `file_blobs.size` (P0577/P0570).
///
/// Denormalizes file content length onto the `file_blobs` junction so
/// `ReadBlob`/`StatBlob` compute the chunk window from one row. The
/// alternative is fetching and decoding `nar_index.entries` per call:
/// O(files-in-NAR), ~2.5 MB for a 25k-file chromium output, on the
/// FUSE `open()` fast path. Size is content-derived (same digest ⇒
/// same bytes ⇒ same size), so two rows for one digest cannot
/// disagree.
///
/// `DEFAULT 0` keeps the `ALTER` rewrite-free and lets test fixtures
/// that don't exercise size (`HasBlobs`) omit the column. There are
/// no pre-066 rows to backfill: 065 and 066 ship in the same release.
pub const M_066: () = ();

/// 067 — `directory_paths` + drop `directory_tenants`/`file_blob_tenants`.
///
/// 065's `directory_tenants`/`file_blob_tenants` were a one-shot
/// snapshot of `path_tenants` taken at first-index time inside
/// `set_nar_index`. Two unsound consequences:
///
/// - **Cross-tenant pick.** `file_blob_tenants` is keyed `(digest,
///   tenant)` — coarser than `file_blobs`' `(digest, store_path_hash)`.
///   The blob-fetch query joined on digest only, then `LIMIT 1` with no
///   `ORDER BY`, so it could return another tenant's NAR window/chunk
///   list for a content-shared digest.
/// - **Late-tenant lockout.** Nothing resynced the junctions after
///   first-index. A tenant that gains a `path_tenants` row later
///   (cache hit, scheduler race) was permanently denied DirectoryService
///   reads for a path they legitimately own.
///
/// Fix: derive tenancy at read time from `path_tenants`, the single
/// source of truth. `file_blobs` already carries `store_path_hash`;
/// `directory_paths` is the analogous linkage for `directories` (one
/// row per `(Directory body, NAR containing it)`, FK CASCADE on both
/// sides — GC of either parent removes the row).
pub const M_067: () = ();

/// 068 — drop `manifests.nar_indexed` + `manifests_nar_index_pending_idx`.
///
/// Both were the background NAR indexer's work-queue state (065/P0551):
/// the partial index `WHERE NOT nar_indexed AND status = 'complete'`
/// fed the indexer's drain query, and the flag flipped once a path's
/// castore index was written. The background indexer is gone — the
/// castore index is now written eagerly in the same transaction that
/// completes the manifest (`complete_manifest_in_conn` →
/// `set_nar_index_in_conn`), so the flag was write-only and the partial
/// index had zero readers while still being maintained on every
/// `manifests` UPDATE.
pub const M_068: () = ();

// BURNED NUMBERS — 069 and 070 (next free migration: 073):
//
// - 069/070 were rio_app role/grant migrations that ARE applied on
//   the persistent DB (recorded rows in `_sqlx_migrations`) and were
//   then retired: role/grant management moved out of the frozen
//   migration set into the runner's `ensure_roles` pass
//   (src/ensure_roles.rs — see its module docs for the two live
//   incidents that forced the move). Their applied rows stay as
//   harmless history.
//
//   UNLIKE the M_055 gap above — a never-applied number that sqlx
//   tolerates WITHOUT ignore_missing because no `_sqlx_migrations`
//   row exists anywhere — 069/070 ARE applied rows: un-embedding
//   them is safe ONLY because the runner sets `ignore_missing(true)`
//   (migrate.rs). Reverting that flag would brick deploys against
//   the persistent DB with `VersionMissing(69)`.

/// 071 — `file_blobs(store_path_hash)` index for the GC-sweep FK CASCADE.
///
/// GC sweep deletes a path's `narinfo` row; the CASCADE chain runs
/// `manifests` → `file_blobs`, and PG executes the referencing-side
/// delete as `DELETE FROM file_blobs WHERE store_path_hash = $1`. The
/// PK is `(digest, store_path_hash)` — leading column `digest` — so
/// without a dedicated index that delete is O(table) per swept path.
/// `directory_paths` (the castore sibling junction) got the analogous
/// `directory_paths_path_idx` at creation in 067; `file_blobs`
/// (created in 065, pre-dating the GC-scale work) never did.
///
/// Measured on a 100×-scale copy of the dev DB (12M `file_blobs`
/// rows): 130.8 ms/path for the cascade without the index, 0.61 ms
/// with it (~214×); a 1000-path sweep dropped 170.8 s → 40.7 s.
///
/// CONCURRENTLY + `-- no-transaction` (precedent: 011/022): plain
/// `CREATE INDEX` takes a SHARE lock that blocks every `file_blobs`
/// writer (PutPath castore indexing) for the duration of a build over
/// millions of rows while the previous release is still serving. The
/// runner's try-then-wait advisory lock was built to allow CIC
/// (I-194). `IF NOT EXISTS` + the documented DROP-then-rerun recovery
/// for a failed half-built INVALID index follow 022 verbatim.
pub const M_071: () = ();

/// 072 — drv blobs as a castore object kind + tenant-scoped chunk presence
/// (ADR-024 P2).
///
/// `drv_blobs` stores the canonical proto `Derivation` bytes keyed by
/// `digest = blake3(received bytes)` — the negotiation key of ADR-024's
/// build plan. The store verifies every put server-side
/// (`rio_proto::derivation_util::verify_drv_blob`: digest recompute,
/// decode, structural validation, canonical re-encode byte-compare,
/// drv_path recompute) so non-canonical bytes are never stored; `body`
/// is therefore byte-identical to what the client sent AND to the
/// canonical encoding. Bodies live in PG like `directories.body` — drv
/// blobs are a few KB (3.4 KB mean), the same size class as Directory
/// bodies, and far below the chunked-CAS minimum.
///
/// `drv_path_hash = sha256(drv_path)` is denormalized at insert so the
/// GC drv sweep joins `scheduler_live_pins.store_path_hash` (the same
/// keying `pin_live_inputs` writes) without computing digests in SQL —
/// a drv blob referenced by a live build survives GC through the
/// existing pin mechanism, no parallel pin table.
///
/// `drv_blob_tenants` follows the `path_tenants` junction pattern:
/// presence (`HasDrvs`) and reads (`GetDrvBlob`) are tenant-scoped,
/// storage is digest-keyed global (dedup at rest unaffected).
///
/// `chunk_tenants` re-creates migration 018's table (dropped as dead in
/// 035 when the global-namespace HasChunks shipped): ADR-024 settles
/// presence as tenant-scoped for ALL object kinds, so `HasChunks` now
/// answers present only for chunks the calling tenant has seen — a row
/// is written for every chunk of a manifest when that manifest
/// completes for a tenant. Migration honesty: chunks ingested before
/// this migration have no tenant rows, so presence answers false and
/// the next upload re-sends them — the write-through idempotent put
/// (S3 PutObject overwrite of identical content) binds visibility
/// without any backfill. No `(tenant_id, …)` secondary index this
/// time: the only reader is `HasChunks`' `(blake3_hash = ANY, tenant_id
/// =)` probe, which the PK serves.
pub const M_072: () = ();

// Add M_NNN consts for other migrations as commentary accumulates.
// Not all migrations need one — only those with non-obvious history,
// dead-code constraints, or "we chose X over Y" rationale. The .sql
// files carry the WHAT; this module carries the WHY.
