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
///
/// **Retired by `M_080`** (substitution-replacement Phase D′): the
/// stored union was the walk world's persistence/recovery fallback.
/// Successor: the `build_wanted_outputs` relation (`M_078`) — exact
/// per-(build, derivation) interest, joined live at classification
/// (`effective_wanted` over the rebuilt per-build contributions, with
/// conservative-absent saturation for relation-less live builds).
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
/// all produced — see the durable 4-cell classifier `classify_durable_evidence_in_tx`);
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
///
/// **Retired by `M_080`** (substitution-replacement Phase D′): the
/// mark's job moved to `materialization_jobs.origin = 'pruned'`
/// (`M_078`/`M_079`), written in the same merge transaction that
/// prunes (with the pruned-wins origin upgrade on dedup) and consumed
/// by the arm-3 settlement discriminator at job consumption — a
/// durable per-decision fact instead of a clearable per-row bit.
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
/// edges once nothing links it, after which a leader failover would see
/// only the produced survivors, clear the restored mark, and re-arm the
/// doomed from-source dispatch the mark exists to prevent. That erasure
/// is narrower than this paragraph originally implied: the GC skips any
/// row still carrying a `build_derivations` link, links are removed
/// only when a builds row is deleted, and the only in-tree deleter of
/// builds rows is the failed-merge rollback — so the laundering shape
/// additionally requires an external/manual builds-row purge before the
/// next recovery (which otherwise re-derives the hole from the child's
/// still-present row). The breadcrumb closes the shape regardless of
/// how the row disappears.
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
/// resubmit it solicits. The frozen `.sql` header still lists "when the
/// fail-fast consumes the mark" among the clears — that wording is
/// historical (the retention shipped later); like `M_063`, the frozen
/// header is not the record, this doc-const is.
/// Restored by `from_recovery_row`
/// (`from_poisoned_row` keeps `false`) and consulted by the
/// recovery-time produced-children gate: a holed flagged parent is
/// never enrolled as a clear candidate, so it keeps the restored mark
/// and at worst takes the bounded resubmit-directing fail-fast. A stale
/// `true` left by a lost best-effort clear errs the same conservative
/// direction. `DEFAULT false` means no backfill and old binaries'
/// column-naming INSERTs keep working during a rolling deploy.
///
/// Read/written by **rio-scheduler** only.
///
/// **Retired by `M_080`** (substitution-replacement Phase D′): the
/// breadcrumb protected children-keyed verdicts from reap-truncated
/// in-memory child sets. Successor: settlement-time judgments classify
/// over the persisted graph instead (`classify_durable_evidence` — the
/// strict three-part criterion: pg.edges + pg.status + a LIVE
/// co-owning build voucher per produced child), which a truncated
/// in-memory view cannot launder.
pub const M_064: () = ();

/// `migrations/065_leader_generation_claims.sql`
///
/// Append-only ledger of every leadership generation handed to dispatch.
/// A new leader INSERTs its generation during recovery, BEFORE
/// `recovery_complete` ungates dispatch (the Chubby-sequencer
/// discipline: the epoch must be durable somewhere the next leader will
/// read before the current leader starts using it). The recovery seed
/// reads `GREATEST(MAX(assignments.generation), MAX(claims))`.
///
/// Why a second table when `assignments.generation` already exists:
/// (1) the assignments high-water only advances when an assignment
/// *persists* — a leader deposed before its first dispatch leaves no
/// trace, and after a `kubectl delete lease` resets `leaseTransitions`
/// its successor would seed from the same stale value and reuse a
/// generation a live believer may still hold; (2) the assignments
/// high-water *decays* — M_034's `ON DELETE CASCADE` plus the periodic
/// orphan-terminal-derivation sweep delete old assignment rows, so
/// `MAX(generation)` regresses toward NULL on a quiescent cluster.
///
/// The PRIMARY KEY on `generation` doubles as the CAS: two holders
/// claiming the same generation concurrently → one INSERT returns zero
/// rows via `ON CONFLICT DO NOTHING` → that holder bumps past
/// `MAX(claims)` and retries. `holder_id` is the replica's pod
/// identity and is LOAD-BEARING: a holder re-acquiring its own epoch
/// (self-fence false alarm → successful renew) finds its own row at
/// its current (recovery-entry) generation and retains it instead of bumping —
/// without the holder comparison every connectivity blip would burn a
/// generation and fence the leader's own in-flight assignments. The
/// safety argument is that no two LIVE processes ever share a
/// `holder_id` — a container restart within the same pod reuses
/// `HOSTNAME`, but the predecessor is dead before the successor
/// starts, so there is never a second live believer to collide with.
/// `claimed_at` is forensic.
///
/// Never garbage-collected: one row per leadership *epoch* (a holder
/// change, or a post-deletion re-floor — a few per day at worst; a
/// same-holder re-acquire of the same epoch reuses its existing row),
/// and deleting rows would re-open the decay problem the table exists
/// to close.
///
/// The table starts EMPTY: there is no backfill of the assignment
/// history that predates it. The recovery claim logic therefore treats
/// an assignments-only floor that ties the recovery-entry generation as
/// foreign and exceeds it (`sched.recovery.fetch-max-seed`) — assignment
/// rows carry no scheduler-holder identity, so a silent ledger cannot
/// affirm the floor is ours; this is what fences the deposed pre-upgrade
/// leader's term on the first post-upgrade handover. The one
/// upgrade-boundary residual that cannot close: a final pre-upgrade term
/// that dispatched nothing leaves no floor at all, so its generation can
/// be reused for one term by the first post-upgrade leader — the same
/// class as the documented unclaimed-proceed residual, in its fault-free
/// variant.
///
/// Read/written by **rio-scheduler** only.
pub const M_065: () = ();

/// `migrations/066_log_chunks.sql`
///
/// The schema for the chunked, store-owned build-log pipeline (harden-logs).
/// Build-log durability moves off the leader-elected scheduler onto
/// rio-store: builders stream batches to the store's `LogService.AppendLog`,
/// the store cuts immutable zstd chunks to S3 and records them here, and
/// readers (`TailLog`) reassemble a log from the chunk manifest. Three
/// tables instead of one because each has exactly one writer and one
/// lifecycle:
///
/// - `drv_executions` — lifecycle. Written by **rio-scheduler** (INSERT at
///   `assign_to_worker`, UPDATE of `status`/`finished_at`/`final_line_count`
///   at terminal), aged out by **rio-store**'s TTL sweep. Deliberately
///   duplicates `exec_id`/`executor_id`/timestamps that also exist on
///   `assignments`: that table keeps one row per *attempt* with its own
///   audit semantics and an active-rows-only partial unique index, while
///   this one is the log subsystem's stable per-execution anchor and the
///   completeness predicate's source of truth (`is_complete` ⇔ status is
///   terminal ∧ `final_line_count` known ∧ the chunk manifest covers a
///   contiguous `[0, final_line_count)`). `drv_hash` is the 32-char
///   `drv_log_hash()` form, NOT `derivations.drv_hash` (same caveat as
///   M_061). The two indexes serve the latest-exec lookup (UUIDv7 DESC =
///   newest) and the TTL sweep's sub-`LIMIT` passes (same pattern as
///   `drv_logs_started_at`, M_061).
///
/// - `drv_log_chunks` — the append-only chunk manifest. Written by
///   **rio-store** only, INSERT-only (`ON CONFLICT DO NOTHING`), one row
///   per durably committed S3 object. The PK is
///   `(exec_id, session_id, chunk_seq)` rather than `(exec_id, chunk_seq)`
///   or `(exec_id, first_line)` because two ingest sessions for the same
///   execution can coexist across a store-replica failover or a builder
///   reconnect: the replayed tail lands in a *new* session's chunks, so
///   sessions must never collide on a key. Overlapping line ranges across
///   sessions are legal; readers select chunks by line-range intersection
///   on `drv_log_chunks_range` and dedup by line number. Stored coverage is
///   therefore a monotone union — no writer can regress another writer's
///   data, which is the property the previous (`drv_logs` + mutable
///   `.partial` blob) design had to maintain with a reconciliation
///   protocol.
///
/// - `log_ingest_sessions` — the live-ingest routing registry. Written by
///   **rio-store** only. `exec_id` PK = at most one live session per
///   execution (the single-session admission rule); the row carries which
///   replica owns the in-memory ingest buffer so `TailLog` subscribers on
///   other replicas can proxy to it. Heartbeat-leased (15 s beat /
///   [`SESSION_STALE_AFTER_SECS`](crate::sql::SESSION_STALE_AFTER_SECS)-second
///   staleness), deleted on clean stream close; a stale row is stealable.
///
/// `drv_logs` is intentionally NOT dropped here: the scheduler code that
/// reads and writes it survives until the harden-logs cutover commit
/// deletes it, and a later migration drops the table. Until then both
/// schemas coexist.
pub const M_066: () = ();

/// `migrations/067_drop_drv_logs.sql`
///
/// Drops the table M_066 promised a "later migration" would drop. The
/// build-log data plane now lives entirely in rio-store
/// (`drv_executions` + `drv_log_chunks` + `log_ingest_sessions`, 066);
/// the scheduler's ring buffers, flusher, `GetDerivationLogs`, and every
/// query against `drv_logs` are deleted in the same commit this migration
/// ships in, so the table outlives its last reader by zero deploys — the
/// branch is landed as a clean cut onto a wiped data plane
/// (`xtask k8s up --wipe`), not a rolling deploy, so there is no
/// schema/code coexistence window to protect.
///
/// The rows are discarded without backfill (pre-prod greenfield, the same
/// precedent as M_061's `DROP TABLE build_logs`). The `.log.zst` /
/// `.partial.log.zst` S3 objects the dropped rows pointed at become
/// unreferenced; they age out under the `logs/` prefix lifecycle rule
/// added to the chunks bucket alongside this migration (infra/eks/s3.tf),
/// which also collects the new chunk layout's orphans.
pub const M_067: () = ();

/// `migrations/068_drv_attempts.sql`
///
/// The scheduler-owned durable attempt ledger: one row per attempt or
/// reset event in a derivation's failure history. This is the data
/// half of the retry-machinery replacement — the ten RAM-only
/// `RetryState` counters become a fold over these rows; in the ledger
/// phase (1a) the rows are written but nothing reads them for
/// decisions.
///
/// **Why a new table** (and not `drv_executions` or `assignments`):
/// both existing tables are read by rio-store with latest-row
/// semantics — `drv_executions` is the log subsystem's per-execution
/// anchor (latest-exec resolution in `logs/tail.rs`, the completeness
/// gate in `logs/gate.rs`) and is TTL-swept by the store at
/// `log_retention_days`; `assignments`' latest row per derivation
/// authorizes the executor's log appends (`logs/gate.rs`). Non-dispatch
/// attempt rows (cascade victims, fleet-exhaust markers, resets,
/// never-dispatched attempts) inserted there would shadow the real
/// execution for those readers and put retry history under store-owned
/// retention. `drv_attempts` is read by the scheduler alone and its
/// retention is scheduler-owned.
///
/// **Key choice:** rows key on `derivations.derivation_id` (the DAG
/// key). `drv_executions.drv_hash` is the 32-char `drv_log_hash()`
/// chunk-key form, not the DAG key, and never-dispatched attempts have
/// no exec_id to join through `assignments` — so the recovery fold
/// loads the suffix directly by `derivation_id = ANY(...)`.
///
/// **No FK on `exec_id`** — a deliberate deviation from the design's
/// "optional exec_id FK" wording: an enforced FK to the store-swept
/// `drv_executions` would re-introduce exactly the retention coupling
/// the new table exists to avoid (the store's TTL sweep would either
/// fail or cascade into scheduler-owned history). Same no-FK posture
/// as M_028. `derivation_id` likewise carries no FK: terminal
/// derivations are GC-swept (`gc_orphan_terminal_derivations`) on the
/// scheduler's own schedule.
///
/// **Two-installment write discipline:** the controller-reported paths
/// (pod OOM/DiskPressure/DeadlineExceeded) never see the failure kind
/// and the exec_id in the same scope, so those attempts are written in
/// two installments on ONE row — the disconnect appends the row
/// (`outcome_class = 'disconnected'`, `termination_reason` NULL), and
/// the controller's later report fills `termination_reason` (and
/// reclassifies) via an UPDATE guarded `WHERE termination_reason IS
/// NULL`, never a second INSERT. The partial unique index on `exec_id`
/// makes one-row-per-execution a schema property rather than a caller
/// discipline: a duplicate append for the same execution is rejected
/// by the index regardless of arrival order. Rows with no exec_id are
/// outside the partial index — they are verdict markers or reset
/// events, not physical executions.
///
/// **`outcome_class` CHECK is the `classify()` alphabet** (the third
/// total function of the decision surface). Extending the alphabet is
/// a new migration, never an edit here. `substitution` is deliberately
/// absent: the substitution-failure decider stays outside the retry
/// collapse (harden-subst ownership), so reserving the name without a
/// writer would be dead schema.
///
/// **Decision-input completeness** (the §5a-1 contract): every input
/// the nine retry/poison entry points consult is either a column here
/// (outcome class, exemption + floor flags, executor, exec_id,
/// timestamps, termination reason, error message, resubmit cycle), a
/// `Budget` field (the caps, the 300 s window, the poison threshold,
/// backoff curve, resubmit limit, poison TTL), or a named caller-side
/// input (the live eligible fleet for the fleet-exhaust check, the
/// clock for the window/TTL/backoff). The floor outcome is consumed at
/// append time — the row carries its classification — so the fold
/// itself never reads the floor ladder.
///
/// **Retention** is scheduler-owned by construction; there is no TTL
/// sweep in Phase 1 (the suffix bound — rows since the last reset
/// event — keeps reads O(per-cycle attempts)). A startup assertion in
/// rio-scheduler pins any future sweep at ≥ max(infra retry window,
/// poison TTL); a real GC policy is a recorded Phase-2 follow-up.
pub const M_068: () = ();

/// `migrations/070_chunks_last_referenced_at.sql`
///
/// Adds `chunks.last_referenced_at TIMESTAMPTZ` (nullable, no
/// backfill) for the refcount-formal campaign's lazy mark-and-collect
/// chunk GC (design §4.1): the collector derives chunk liveness from
/// the durable manifests at collect time, and this timestamp closes
/// the mark-snapshot race the exact refcount closes today — a manifest
/// whose upgrade transaction commits after the cycle's mark snapshot
/// but references an old, otherwise-unreferenced chunk is invisible to
/// that cycle's mark; the upsert's touch keeps the chunk out of that
/// cycle's collect via the grace term.
///
/// **Single writer:** the chunked-upgrade upsert's `ON CONFLICT DO
/// UPDATE` arm (`upgrade_manifest_to_chunked` in
/// `rio-store/src/metadata/chunked.rs`) sets it to `now()`. No cleanup
/// path (reaper, rollback, sweep, drain) ever writes it, and nothing
/// reads it until the collector lands — it is a timestamp with no
/// arithmetic to corrupt, not a counter.
///
/// **Why nullable with no backfill:** the collect predicate is
/// `GREATEST(created_at, last_referenced_at) < cycle_start - grace`,
/// and PostgreSQL's `GREATEST()` ignores NULL arguments, so a NULL is
/// semantically identical to "backfilled to created_at" while keeping
/// the startup-run migration metadata-only on the largest table in the
/// schema (no full-table rewrite or update).
///
/// **Numbering:** 069 was reserved by the retry campaign's deferred
/// mirror-column drop while that drop was gated on a drain condition;
/// the drop ultimately shipped as migration 075 (see M_075 for why the
/// reservation was not used), so the 069 gap is permanent and
/// deliberate, not a missing file.
pub const M_070: () = ();

/// `migrations/071_drop_refcount_check_and_index.sql`
///
/// Release B of the refcount-formal campaign (lazy mark-and-collect
/// chunk GC, design §4.5): drops the M_023
/// `chunks_refcount_nonneg CHECK (refcount >= 0)` and the
/// `idx_chunks_gc` partial index (`WHERE refcount = 0 AND deleted =
/// FALSE`, migration 002). Both are metadata-only drops; `IF EXISTS`
/// per the 035 style.
///
/// **Why the CHECK must go before the writer-deletion code rolls
/// out:** this repo applies migrations at new-pod startup, so 071 runs
/// at the first Release-B pod's boot while Release-A pods still serve.
/// Release-B pods stop incrementing the counter (the upsert no longer
/// names it); Release-A pods still decrement it (rollback DEC-1 and
/// the reaper's write-only DEC-2). An A-pod decrement against a chunk
/// first referenced by a B-pod upload would take the counter 0 → -1,
/// and with the CHECK still in place that aborts the A pod's rollback
/// or reap transaction mid-rollout. After Release A nothing reads the
/// counter (collection eligibility is the manifest fold —
/// `store.chunk.liveness-derived`), so the underflow is silent and
/// harmless once the CHECK is gone; dropping the CHECK first turns a
/// rolling-deploy failure mode into a no-op. The index existed solely
/// for the retired `refcount = 0` GC-candidate scans and no statement
/// references it (or the constraint) by name.
///
/// **Why the column is NOT dropped here:** Release-A pods' upsert,
/// rollback, and reapers still name `chunks.refcount` in their SQL for
/// the duration of the rollout; dropping the column at the first B
/// pod's boot would break every A-pod chunked upload. The column drop
/// is migration 072 (see M_072 for the §4.5 (ii) ordering rationale
/// and why it ships in-tree).
pub const M_071: () = ();

/// `migrations/072_drop_chunks_refcount.sql`
///
/// Phase 1c of the refcount-formal campaign (lazy mark-and-collect
/// chunk GC, design §4.5): drops the `chunks.refcount` column itself.
/// Metadata-only on PG (catalog update, no table rewrite); `IF EXISTS`
/// per the 035 style. 071 dropped the M_023 CHECK and `idx_chunks_gc`;
/// this completes the schema retirement — chunk liveness is the
/// manifest fold computed by the collect cycle
/// (`store.chunk.liveness-derived`), and no production code has read
/// or written the counter since the Release B writer deletion.
///
/// **Why this is a separate migration from 071 (the §4.5 (ii)
/// ordering):** migrations run at new-pod startup, so a release that
/// carried both the writer deletion and the column drop would drop the
/// column while previous-release pods still serve, and their upsert,
/// rollback (DEC-1), and reapers (DEC-2/ZERO) name the column in their
/// SQL — every chunked upload on those pods would fail for the
/// duration of the rollout. Keeping the drop in its own migration is
/// what makes a staged rollout of the pre-cutover releases safe.
///
/// **Why it ships in-tree rather than as an operator-gated
/// follow-up:** the campaign owner clarified (2026-05-27) that there
/// is no staged rollout and no existing cluster or live database —
/// every eventual deployment is fresh — so the mixed-fleet hazard
/// above is vacuous and the drop is ordinary development work. On a
/// fresh database the chain (… 070, 071, 072, 073) applies in order at
/// first startup before any pod serves; the former deployment-time
/// "apply 072 only after the Release-B rollout completes"
/// precondition (checklist row D7 in `docs/ops/gc-enablement.typ`)
/// reduces to the ordinary "migrations run on deploy" statement and
/// matters again only if pre-Release-B images were ever deliberately
/// staged against a shared database.
pub const M_072: () = ();

/// `migrations/073_attempt_source_node.sql`
///
/// Three additive columns for the executor-lifecycle campaign's
/// pull-mode dispatch (Phase 1, Wave 1a): `drv_attempts.source_node`,
/// `drv_executions.source_node`, and `drv_executions.dispatch_mode`.
///
/// **`source_node` (both tables, nullable, no backfill, no index):**
/// AD2c durable source attribution. The retry exclusion set re-keys
/// from executor/pod identity to the node the pod ran on, and the key
/// must be the *controller-authoritative* pod→node binding (the
/// `AckSpawnedIntents.bound_intents` informer data, or the controller's
/// `ReportAttemptOutcome.node_name`), persisted so the re-keyed
/// exclusion survives leader failover per `sched.retry.failover-budget`.
/// The in-memory `authoritative_binding` map is explicitly NOT the
/// home — it dies with the leader. Worker-supplied node identity is
/// not accepted as the writer (the hung-node detector's threat model).
/// Nullable because the binding is only known once the controller has
/// reported it; no decision input requires a backfill. No index: the
/// exclusion fold reads recent rows per derivation via the existing
/// derivation/exec_id indexes — revisit only with EXPLAIN evidence.
///
/// **`dispatch_mode` (`drv_executions`, NOT NULL DEFAULT 'stream',
/// CHECK IN ('stream','pull')):** the pull/stream coexistence
/// discriminator. Only the pull transaction writes `'pull'`; the
/// as-built stream dispatch path is never modified and relies on the
/// default. Every pull-only consumer (the establishment sweep, the
/// open-attempt view behind `ListOpenAttempts`, the controller's
/// synthesize-on-delete arm, the open-attempts gauge) filters on this
/// column — `source_node IS NOT NULL` is NOT the discriminator,
/// because it is only populated when known for pull attempts and would
/// fail unsafe by dropping pull attempts from the sweep/busy view.
/// Dropped by migration 076 once the stream path itself was deleted
/// (see M_076).
///
/// **Numbering:** 071/072 are the refcount campaign's Release B and
/// column-drop migrations (see M_071/M_072). 074 stays reserved for
/// this campaign as the open-attempt-view escape hatch and is unused
/// unless that view needs an extra column.
pub const M_073: () = ();

/// `drv_executions.deadline_secs` (nullable DOUBLE PRECISION, no
/// backfill, no index) — the open-attempt-view extra column M_073
/// reserved 074 for.
///
/// The establishment sweep's window must be anchored to the deadline
/// the attempt was actually dispatched under (the same solve that
/// sized the spawn intent and the Job's `activeDeadlineSeconds`), not
/// re-derived at sweep time: a fitted estimate or hw-table change that
/// shrinks between dispatch and sweep would otherwise establish a
/// healthy, still-building attempt as an unreported executor crash
/// (spurious charge toward the poison threshold, duplicate build, the
/// genuine report later AckIgnored as assignment-inactive). The fenced
/// pull mint writes the solved deadline here; the sweep takes
/// `max(persisted, re-solved)` so the window can widen (estimate grew,
/// floor bump) but never shrink while the attempt is open
/// (`sched.attempt.establishment-window+6`). Nullable: pre-074 rows
/// fall back to the sweep-time re-solve, and stream rows never
/// populate it. Known residual: the Job's `activeDeadlineSeconds` is
/// rendered slightly before the pull mint, so the persisted value can
/// in principle undershoot the Job's by however much the estimate
/// moved inside that gap; the configured report slack plus the
/// controller's worker-timeout margin covers that residue.
pub const M_074: () = ();

/// `migrations/075_drop_retry_mirror_columns.sql`
///
/// Retry-formal campaign Phase-2 coda: drops the three frozen retry
/// mirror columns `derivations.{retry_count, failed_builders,
/// resubmit_cycles}`. Metadata-only (catalog update, no table rewrite);
/// `IF EXISTS` per the 035 style. `poisoned_at` is NOT dropped — it is
/// the poison-lifecycle TTL anchor (`sched.poison.ttl-persist`), not a
/// counter mirror.
///
/// **Why the columns are dead:** since the Phase-1b cutover (T-1b.13)
/// every retry/poison budget is a fold over the `drv_attempts` ledger
/// (068) and survives failover through it; the per-counter mirror
/// writers were deleted then, leaving the columns frozen and read only
/// by the transitional legacy seed (`load_retry_seed_in_tx`, decision
/// P5) that floored the fold for failure histories predating 068. The
/// reset paths' column zeroing and the `load_poisoned_display` legacy
/// union are removed together with this migration; the seed machinery
/// itself (the `legacy_seed` argument of `decide()`) is retired in the
/// follow-up commit, restoring the frozen three-argument decision
/// surface.
///
/// **Why the drain condition is satisfied:** the Phase-2 close-out
/// deferred this drop behind a drain condition — no non-terminal or
/// poisoned derivation may still carry a failure history whose only
/// record is the mirror columns — because at the 068 release boundary
/// that condition is unmet by definition for any database that upgrades
/// through 068. The campaign owner's deployment-model clarification
/// (2026-05-27, the same fresh-only directive M_072 records: no staged
/// rollouts, no existing cluster or live database — every eventual
/// deployment is fresh) makes the condition vacuously true: a fresh
/// database never has pre-068 failure histories, so there is nothing
/// for the seed to preserve and the drop is ordinary development work.
/// The operational drain probe recorded in the retry invariant map
/// close-out is therefore moot for fresh deployments and is retained
/// there only as history.
///
/// **Numbering (why 075 and not the reserved 069):** the close-out
/// reserved 069 for this drop, but 070–074 shipped while it was
/// deferred. Claiming the lower number now would make the applied order
/// on a fresh database differ from the authored order and would
/// re-order the chain relative to databases that hypothetically applied
/// 070+ already; the only value of the 069 slot — keeping the drop
/// adjacent to 068 — matters to neither, so the drop takes the next
/// free number and 069 stays a permanent, deliberate gap (see M_070).
pub const M_075: () = ();

/// `migrations/076_drop_dispatch_mode.sql`
///
/// Executor-lifecycle dispatch-mode knob retirement (PR #46 Track C):
/// drops `drv_executions.dispatch_mode`, the pull/stream coexistence
/// discriminator M_073 added. Metadata-only (catalog update, no table
/// rewrite); `IF EXISTS` per the 035 style. The column's CHECK
/// constraint goes with it.
///
/// **Why the column is dead:** the discriminator existed so pull-only
/// consumers (the establishment sweep, the open-attempt view behind
/// `ListOpenAttempts`, the controller's synthesize-on-delete arm, the
/// open-attempts gauge) could exclude rows written by the as-built
/// stream dispatch path during coexistence. The stream path was
/// deleted by the executor campaign's 1c'/1d slices: the pull
/// transaction is the only `drv_executions` writer left, every row it
/// writes carried the constant `'pull'`, and a column whose value is
/// invariant discriminates nothing. The Pool CRD `dispatchMode` field,
/// the `RIO_DISPATCH_MODE` pod discriminator, and the controller-side
/// gates retire in the same change set.
///
/// **Why a drop rather than freezing the column:** the campaign owner's
/// fresh-deployments-only directive (2026-05-27, recorded at M_072 and
/// M_075) means no database ever carries stream-era rows; on a fresh
/// database the chain (… 073, 074, 075, 076) applies in order at first
/// startup, so the add-then-drop pair is just history, not churn any
/// live deployment observes.
pub const M_076: () = ();

/// `migrations/077_drop_build_event_log.sql`
///
/// WatchBuild resumability-layer deletion (the build-event-sourcing
/// rescope memo's §4.5 work item, executed by owner decision): drops
/// `build_event_log`, the prost-encoded per-(build_id, sequence) event
/// mirror that existed solely so a reconnecting gateway could replay
/// events it missed across scheduler failover.
///
/// **Why the table is dead:** reconnection is snapshot-first
/// (`sched.watch.snapshot-first`): a `WatchBuild` stream's first message
/// describes the build's current state, so missed events are never
/// re-delivered — their net effect is what the snapshot reports. The
/// table's only writer (the event-log persister task), its only readers
/// (the `since_sequence` replay query and the recovery sequence seeding),
/// both GC paths (per-build DELETE on terminal cleanup, the 24h Tick
/// sweep), and the per-build sequence counters are deleted in the same
/// change set this migration ships in.
///
/// **Why a new migration rather than editing 003:** `003_event_log.sql`
/// is frozen and PINNED (the checksum-freeze rule); the M_055/042
/// precedent applies — a shipped migration's content never changes, so
/// the drop takes the next free number. 003 remains as history: a fresh
/// database creates the table at 003 and drops it at 077, which is just
/// chain replay (the fresh-deployments-only directive recorded at M_072 /
/// M_075 / M_076 means no live database carries rows to migrate).
pub const M_077: () = ();

/// `migrations/078_materialization_jobs.sql`
///
/// Substitution-replacement campaign Phase A (additive, dormant): the
/// durable wanted relation (`build_wanted_outputs`), the
/// materialization-job table + `materialization_interest` view, the
/// `drv_executions.attempt_kind` work-class discriminator, and
/// pin-kind discrimination on `scheduler_live_pins`.
///
/// **Dormancy:** no code path writes any of this while
/// `scheduler.materialization.enabled` / `store.materialization.enabled`
/// are false (the Phase A defaults). Every ALTER carries a DEFAULT so
/// existing writers (`mint_pull_attempt_fenced`, `pin_live_inputs`)
/// are untouched.
///
/// **Why `attempt_kind` exists when M_076 just dropped `dispatch_mode`:**
/// 076 dropped a *transport* discriminator whose value space had
/// collapsed to a constant ('pull') — it discriminated nothing. This
/// column is a *work-class* discriminator (build vs materialization)
/// whose value space becomes non-constant the moment the
/// materialization executor ships. The retry fold's kind partition
/// (materialization rows invisible to build budgets), the
/// establishment sweep's no-adopt branch, and the report intake's
/// kind check all read it. Kind is never derived from an executor-id
/// prefix (the newtypes.rs convention).
///
/// **Forward reference:** `build_wanted_outputs` is the
/// PG-authoritative successor of the 062 stored union + the in-memory
/// per-build contributions (AW4). 062 is NOT touched here; its
/// retirement is a Phase D' migration gated on the campaign's
/// verify-then-simplify gate (design §4).
pub const M_078: () = ();

/// `migrations/079_materialization_outcome_classes.sql`
///
/// Substitution-replacement Phase A: expands the `drv_attempts.
/// outcome_class` CHECK alphabet with `materialization_unobtainable`
/// and `materialization_infra` (DROP CONSTRAINT + ADD CONSTRAINT with a
/// strict-superset literal list — 068 itself stays frozen).
///
/// **The typed E5 carve-out (design §2.5 / adjudication OQ1):** these
/// classes exist so materialization outcomes enter the SAME durable
/// ledger as build attempts (one history, one fold input) instead of a
/// side table — but they are partitioned OUT of every build budget by
/// the attempt *kind* (`drv_executions.attempt_kind`, M_078), never by
/// class-keyed special cases. `materialization_infra` counts toward
/// `max_materialization_attempts` and toward NOTHING else;
/// `materialization_unobtainable` is a routing verdict, not a retry
/// charge. This supersedes the old structural carve-out ("substitution
/// is deliberately NOT in the alphabet"): the substitution-replacement
/// campaign brings the work into the alphabet precisely because the
/// kind partition makes that safe.
///
/// **Lockstep commit requirement (design FP-2):** this migration MUST
/// land in the same commit as the Rust enum variants in
/// `rio-scheduler/src/state/derivation.rs::OutcomeClass` and
/// `rio-retry-kernel::OutcomeClass`, the two retry_policy.rs bridges,
/// and the kernel's `row_to_event` arms — the
/// `test_attempt_outcome_class_alphabet_matches_check_constraint` test
/// is green only when the SQL alphabet and the Rust alphabet carry the
/// same 15 literals.
///
/// **Dormancy:** no code path constructs either class while
/// `scheduler.materialization.enabled` is false; the consumption
/// transaction (Wave 3) and the establishment-sweep materialization
/// branch (Wave 3) are the only writers, both flag-gated.
pub const M_079: () = ();

/// `migrations/080_drop_walk_evidence.sql`
///
/// Substitution-replacement Phase D′.2 (design §4/§8; owner GO
/// 2026-06-01): retires the walk-era evidence surface after D′.1
/// deleted every binary reader/writer — the `'substituting'` status
/// (added by `M_038` for the detached walk fetch), the
/// `topdown_pruned` mark (`M_063`), the `closure_hole` breadcrumb
/// (`M_064`), and the stored wanted union (`M_062`). Each retirement
/// note on those consts names the durable successor.
///
/// **The data step before the CHECK narrowing:** leftover
/// `'substituting'` rows are only possible when upgrading from a
/// pre-Phase-B era in one hop (a walk-era leader crashed
/// mid-substitution and no later binary rewrote the row). The D′.1
/// binaries carried a transitional decode arm (`"substituting"` →
/// `Queued` + warn, PD-D3) so they could recover against a pre-080
/// database; the UPDATE here makes that absorption durable, and the
/// narrowed CHECK makes the state unrepresentable. The decode arm was
/// removed in the same commit as this migration: both scheduler and
/// store run `rio_migrations::migrate::run` at startup BEFORE any
/// derivation-status read (scheduler recovery runs only on
/// LeaderAcquired, after boot), so a post-080 binary can never see the
/// legacy string.
///
/// **Idempotent failure mode (risk R4):** the UPDATE and the
/// ADD CONSTRAINT run in one migration transaction. A not-yet-rolled
/// pre-D′.1 replica writing `'substituting'` between them fails the
/// ADD — re-running the deploy retries the whole migration. Deployment
/// order is therefore: roll the D′.1 binaries everywhere FIRST, then
/// ship the 080-carrying release.
///
/// **Roll-forward only (FP-7):** a pre-D′ binary against a post-080
/// database fails (it SELECTs the dropped columns); a post-D′ binary
/// against a pre-080 database works (no column reads; the decode arm
/// only existed in the transitional D′.1 window). Rollback to walk
/// behavior = rollback to a pre-D′ BINARY, and only before this
/// migration is applied to a persistent database.
///
/// The 038 partial index (`derivations_status_idx`) predicates on
/// terminal statuses only, so no index DDL rides this migration.
pub const M_080: () = ();

/// `migrations/081_manifests_substitute_progress.sql`
///
/// Per-path download-progress evidence for the substitution ingest
/// path (harden-store reconciliation memo, work item S — the design's
/// migration 064 renumbered). Four additive columns on `manifests`,
/// written only for substitution-claimed `'uploading'` placeholders:
///
/// - `fetched_bytes` / `last_progress_at`: the placeholder guard's
///   30s heartbeat carries the owner's decompressed-byte count; the
///   single UPDATE advances `last_progress_at` only when the count
///   changed. Together they discriminate **stuck ≠ slow**: a slow
///   owner advances `last_progress_at`; a wedged one does not.
///   PutPath/PutPathBatch claims never write them (`fetched_bytes`
///   stays NULL → structurally exempt from every stall rule).
/// - `stall_count`: durable per-path stall evidence. Incremented
///   exactly once per stall event (claim-guarded: the owner-side
///   abort's release-in-place and a competing stall-reclaim race on
///   the same `claim_id` — whichever lands first wins). Survives
///   in-place ownership handoffs; reset by row deletion
///   (heartbeat-death and guard-drop reaps DELETE, so benign churn —
///   deploys, scale-in, crashes — never accrues strikes).
/// - `claimed_by`: owner attribution (pod name) for operator-side
///   stall/takeover diagnosis.
///
/// Nullable/default-only, no backfill, no index changes (every
/// consumer reaches the row by the `store_path_hash` PK). Old
/// binaries ignore the columns: a mixed-version window degrades to
/// the pre-081 heartbeat-death-only reclaim, never misbehaves.
pub const M_081: () = ();

/// `migrations/082_materialization_job_carried_paths.sql`
///
/// Realized-path carrier for the floating-CA stale-reset lane
/// (substitution-replacement follow-up ledger row 1; spec rule
/// `sched.merge.stale-substitutable+3`). One nullable column on
/// `materialization_jobs`:
///
/// - `carried_realized_paths TEXT[]`: written ONLY by the
///   `stale_reset` origin at job creation — a snapshot of the realized
///   floating-CA output paths the stale-Completed verify destroys in
///   memory (`state.output_paths.clear()`). The paths are immutable
///   content-addressed data, so the creation-time snapshot does not
///   violate the "live wanted reads" rule (which governs the wanted
///   NAME set — that stays live). Set-if-null on the creation dedup
///   arm: an existing pending job gains the carrier, a present
///   carrier is never overwritten.
///
/// Consumers: the store executor's wanted resolution unions the
/// carried paths into its walk seed set (the empty-path placeholder
/// slots produce nothing); the scheduler's success-consumption
/// coverage unions the same column, so seed set and coverage agree by
/// construction and a vacuous `Success{[],[]}` no longer
/// "re-completes" the node with the `[""]` placeholder.
///
/// NULL = no carrier (every other origin): floating-CA slots then
/// resolve through `expected_output_paths` exactly as before this
/// column existed.
///
/// Nullable, no backfill, no index. Mixed-version window: an old
/// scheduler never writes the column (stale-reset jobs degrade to the
/// pre-082 vacuous shape — today's behavior, not a regression); an
/// old store executor ignores it (the scheduler-side coverage union
/// then refuses the vacuous success and the job re-arms until an
/// upgraded executor claims it — additive-only, never a crash).
pub const M_082: () = ();

/// `migrations/083_materialization_job_park_began.sql`
///
/// Dwell-clock carrier for the Item T conversion-strictness knob
/// (follow-up ledger row 7, second half; spec rule
/// `sched.materialize.conversion-strictness`; owner decision
/// 2026-06-02: durable carrier — failover-EXACT dwell over the
/// in-memory alternative). One nullable column on
/// `materialization_jobs`:
///
/// - `park_began_at TIMESTAMPTZ`: when the job's MOST RECENT park
///   began. Written by the park UPDATE in the same statement as
///   `park_until`; a re-park overwrites it (the dwell clock restarts
///   by design — one mechanism, no re-arm-path throttling). The
///   recovery view rebuild replays it as an in-memory Instant
///   (`now - (now_db - park_began_at)`), so a leader failover does
///   not restart the dwell.
///
/// NULL = never parked, or parked before this migration: the dwell
/// gate treats NULL as unmet (stays parked — conservative) and the
/// next park cycle stamps it (self-healing). Old binaries ignore the
/// column entirely; the knob ships default-off, so a mixed-version
/// window has zero behavioral delta.
pub const M_083: () = ();

/// `migrations/084_attempt_kind_partition.sql`
///
/// The attempt-kind partition becomes a DURABLE LANE on every ledger
/// row (bughunt wave, merged_bug_011/bug_266; spec rule
/// `sched.db.attempts-gc`). Before this migration the kind existed
/// only on `drv_executions` and reached `drv_attempts` via the
/// `exec_id` join — so reset rows (`exec_id IS NULL` always) were
/// unrepresentably build-kind, every loader cut at the LAST RESET OF
/// ANY KIND, and the GC sweep deleted under that same any-kind cut: a
/// build resubmit reset silently hid (loader) and then deleted (GC)
/// materialization-infra evidence, resurrecting parked jobs.
///
/// - `drv_attempts.attempt_kind TEXT NOT NULL DEFAULT 'build'` with
///   the two-value CHECK: every row now CARRIES its lane; the backfill
///   stamps existing materialization rows from their execution. The
///   DEFAULT keeps the column append-compatible, but the Rust writer
///   makes kind a CONSTRUCTOR PARAMETER — no production INSERT relies
///   on the default.
/// - Loaders and the GC sweep cut PER LANE from here on: each row
///   survives iff it is at-or-after the last reset row OF ITS OWN
///   lane (`rio_retry_kernel::row_survives_load`); a build reset
///   structurally cannot truncate the materialization lane, and vice
///   versa. Mat-lane reset rows have no production writer until the
///   materialization-lifecycle workstream's job-creation resets land
///   (085) — until then the mat lane simply has no cut and keeps its
///   full history (bounded by the per-derivation GC orphan arm).
/// - `drv_executions.source_node` is scrubbed for materialization
///   executions and locked by the `drv_executions_build_only_source_node`
///   CHECK (bug_075): node attribution is a build-lane concept — the
///   mint stamped builder pod bindings onto store executions, feeding
///   wrong exclusion keys. Schema/binary lockstep: the CHECK and the
///   kind-aware mint deploy together (the repo's stop-the-world
///   migration posture).
pub const M_084: () = ();

/// `migrations/085_materialization_reset_class.sql`
///
/// The materialization lane's reset class (bughunt wave A3
/// materialization-lifecycle-kernel, merged_bug_020 / bug_067 — the
/// per-job budget window). Expands the drv_attempts outcome-class
/// CHECK alphabet with `'materialization_reset'` (DROP+ADD over the
/// 079 constraint, the same never-edit-frozen-files discipline).
///
/// Written by `create_materialization_jobs_in_tx` — ONE row per
/// genuinely created job (`event_kind='reset'`,
/// `attempt_kind='materialization'`, `party='scheduler'`), in the SAME
/// transaction as the job INSERT; the dedup arm writes none. The row
/// re-windows every materialization counter at job creation: the
/// kernel cut is `(attempt_kind, event_kind)` (`ledger_suffix_start`),
/// so a successor job's budget/one-shot/strictness counts start fresh
/// instead of inheriting the resolved predecessor's charges, and the
/// count is identical live, post-failover (suffix loaders), and under
/// the GC sweep (which preserves per-lane suffixes). The class string
/// is row DATA — the cut predicate never keys on it (a class-keyed
/// anchor would diverge from the kernel on any future mat-kind reset
/// row of another class).
pub const M_085: () = ();

/// 086 — `live_wanted_interest`: one durable home for live interest
/// (bughunt wave, merged_bug_176). Interest derives from
/// `build_derivations` MEMBERSHIP (which row-less builds DO have),
/// LEFT-JOINed to the optional `build_wanted_outputs` contribution; a
/// missing row contributes the saturating `'{}'` ("all declared
/// outputs wanted") default, surfaced as `saturated_default` so
/// readers can record the width saturation. `materialization_interest`
/// is re-created over the new view with its 078 column shape preserved
/// (job_id, build_id, wanted_output_names) — the §5.3 pin-release
/// predicate and its sqlx pins are unchanged. Closes the
/// under-statement class where a live build without a wanted row was
/// invisible to interest, pin retention, tenant resolution, and the
/// effective-wanted union.
pub const M_086: () = ();

/// `migrations/087_build_terminal_payload.sql`
///
/// Durable terminal payload for builds (bughunt fix wave C1
/// terminal-capture, merged_bug_323; spec rule
/// `sched.watch.terminal-from-durable-row`). Five nullable columns on
/// `builds`, written ONCE by the terminal arm of
/// `update_build_status` in the same UPDATE as the status flip:
///
/// - `failed_derivation TEXT` / `failure_status TEXT` (CHECK over the
///   proto `BuildResultStatus` `as_str_name()` set): the captured
///   first failure. Build-level failures (per-build timeout) leave
///   `failed_derivation` NULL by construction.
/// - `cancel_reason TEXT`: the REQUIRED reason of the cancel intent.
/// - `output_paths TEXT[]`: root output paths captured at the
///   terminal transition (Succeeded arm).
/// - `failed_drvs INTEGER`: the settled failed count, completing the
///   persisted accounting (total/completed/cached pre-existed).
///
/// This deliberately UPGRADES the documented in-memory-only posture of
/// the failure trio: a post-cleanup or post-failover `WatchBuild` now
/// answers from the `builds` row with the recorded verdict instead of
/// `NotFound` (which the gateway converted into a fabricated failure
/// after ~111s of reconnect attempts). NULL columns (pre-087 terminal
/// rows) degrade to the old empty-payload snapshot — additive only.
pub const M_087: () = ();
/// `migrations/088_store_degraded_outcome_class.sql`
///
/// The `store_degraded` outcome class joins the attempt-ledger alphabet
/// (bughunt fix wave B1 bounded-await-transport, bug_408; spec rules
/// `builder.outcome.store-degraded` /
/// `sched.retry.store-degraded-uncharged`). Re-creates
/// `drv_attempts_outcome_class_check` as 085's seventeen-minus-one set
/// plus `'store_degraded'`: an infrastructure failure the builder
/// stamped `BuildResult.store_degraded` (FUSE breaker open at
/// completion, or tripped during the build). The class is pure pacing
/// in the kernel fold — it advances no count budget, mints no
/// exclusion key, and can never reach a poison verdict (there is no
/// `AttemptEvent` for it; `decide()` folds it as backoff-only from
/// the consecutive run) — restoring the heartbeat-era
/// "wait out the outage" disposition the 1d collapse traded away,
/// without per-node capacity state. Rows carry `source_node = NULL`
/// (the failure is the STORE's, not the node's).
pub const M_088: () = ();
/// `migrations/089_log_authority.sql`
///
/// The log-ingest authority surface goes durable (bughunt wave,
/// merged_bug_207/bug_248/merged_bug_111; B2 log-ingest-authority):
///
/// - `drv_log_chunks.accounted_bytes BIGINT NOT NULL DEFAULT 0`: the
///   cut's ACCOUNTED size — post-truncation content bytes plus
///   `PER_LINE_OVERHEAD` (32) per line, the exact formula every ingest
///   resource bound charges (`rio-store/src/logs/ingest.rs::
///   accounted_len`). Written by `commit_chunk` at cut time. The
///   per-execution byte cap and chunk-attempt cap are seeded from
///   `SUM(accounted_bytes)` / `COUNT(*)` at every `AppendLog` open
///   (`gate::log_seed`), so a reconnect can no longer reset either cap
///   to zero (merged_bug_207's per-session-cap hole: the documented
///   per-EXECUTION caps were enforced against session-local counters
///   that every reconnect zeroed, making both caps per-session and the
///   abuse bound unbounded). Rows cut before this migration default to
///   0: a ≤`log_retention_days` (default 30d) under-count window for
///   the abuse bound — pre-089 bytes age out of retention on that
///   schedule, after which the durable account is exact. Accepted: the
///   cap is an abuse bound, not an accounting invariant, and the
///   alternative (backfilling from `byte_size`, the COMPRESSED size)
///   would under-charge by the compression ratio forever.
/// - `latest_build_exec` view + `drv_executions_build_latest_idx`
///   partial index: THE one resolver for "the derivation's latest
///   BUILD execution". The unpinned `TailLog` resolution used a raw
///   `ORDER BY exec_id DESC` (pre-latest_build_exec) over all kinds, so a freshly-minted
///   materialization execution (which never has chunks) shadowed the
///   build whose log the caller wanted (merged_bug_111). The view is
///   kind-filtered at the definition, every consumer inherits the
///   filter; `log-no-raw-latest-exec` bans new raw `ORDER BY exec_id DESC`
///   reads of `drv_executions` outside migrations.
pub const M_089: () = ();
/// `migrations/090_gc_collect_state.sql`
///
/// The chunk-collector's cluster state becomes a durable singleton row
/// (bughunt wave D1, bug_174 + merged_bug_211):
///
/// - Cadence: `last_live_cycle_at` + `cycle_epoch`. The backstop runs
///   a cycle ONLY when `now() - last_live_cycle_at` crosses the
///   interval (DB clock) — pre-090 every replica armed its own daily
///   boot-anchored timer, so N replicas ran up to N heavy cycles/day
///   (the advisory lock gave mutual exclusion, not rate limiting).
/// - `cursor`: the keyset resume point of a capped pass — pre-090 a
///   process static, so a capped pass restarted from scratch on
///   whichever replica won next.
/// - `backlog_estimate`, `last_mark_set_size`, `last_would_collect`:
///   the gauge sources. Every replica publishes
///   rio_store_gc_{collect_backlog_chunks,chunks_live,chunks_would_collect}
///   from a 60s read of this row — pre-090 only the cycle-winning pod's
///   gauges ever moved; the rest sat frozen at their pre-registered 0.
///   The gauges are a REPLICATED CLUSTER FACT: aggregate with max(),
///   never sum() (owner decision Q6 2026-06-03; gc-enablement runbook).
///
/// Writers: `GcCycleLease::commit_cycle` (epoch+1 + stamps, through
/// the advisory-lock session) — live cycles stamp `last_live_cycle_at`
/// and the cursor; shadow (dry-run) cycles anchor the backlog and
/// observation sizes WITHOUT the live stamp (an observation must not
/// answer the cadence question).
pub const M_090: () = ();

/// `migrations/091_chunks_deleted_at.sql`
///
/// Tombstone-reap support (bughunt wave D1, merged_bug_336): pre-091,
/// soft-deleted (`deleted = TRUE`) chunk rows whose S3 objects the
/// drain had already removed stayed in the table FOREVER — permanent
/// tombstones with no reaper.
///
/// - `deleted_at`: stamped by the collect batch UPDATE alongside
///   `deleted = TRUE`; NULLed by the resurrect upsert
///   (metadata/chunked.rs) so a resurrected row is never reap-eligible.
/// - `idx_chunks_reapable` (partial, `WHERE deleted`): the reap's
///   age-scan index.
///
/// The reaper is the post-pass step of a COMPLETE live collect cycle:
/// `DELETE … WHERE deleted AND deleted_at < now() - grace AND NOT
/// EXISTS (pending_s3_deletes)` — the outbox conjunct keeps the
/// drain's resurrect-skip exact, the grace term gives the drain (and
/// any in-flight resurrect) time, and batching+capping mirror the
/// collect loop (stopping early only retains tombstones longer).
/// Lifecycle: insert → live → soft-deleted (`deleted_at`) → drained
/// (outbox row gone) → reaped (row gone); spec rule
/// store.gc.bounded-garbage-retention carries the row-retention
/// clause.
///
/// **Backfill (bug_354, amended in place 2026-06-04 under the signed
/// §5-Q9 evidence standard — origin/main tops out at 064, the
/// introducing commit is contained only in the unmerged feature
/// branch, and the owner attested no persistent DB was migrated at
/// ≥091; PINNED row 91 re-pinned in the same commit):** the original
/// body added the column without backfilling rows soft-deleted BEFORE
/// 091 — their `deleted_at` stayed NULL, the reap predicate
/// (`deleted_at < now() - grace`) never matched, and pre-upgrade
/// tombstones were exactly the permanent rows this migration exists
/// to make reapable. `UPDATE … SET deleted_at = now() WHERE deleted
/// AND deleted_at IS NULL` between the ADD COLUMN and the index
/// anchors their grace clock at upgrade time (retention-erring).
///
/// Class rule: a column referenced in a deletion predicate must be
/// backfilled in the migration that introduces it, or the predicate
/// must be NULL-total (treat NULL as a match) — an unbackfilled
/// NULL is an accidental KeepForever.
pub const M_091: () = ();

/// `migrations/092_manifests_claim_phase.sql`
///
/// The owner-side claim phase becomes durable data (bughunt wave D1,
/// merged_bug_003): `claim_phase IN ('downloading','budget_parked',
/// 'persisting')`, NULL = non-substitution (PutPath) claim.
///
/// Written by: the placeholder claim writers ('downloading' for
/// substitution claimants, NULL otherwise), every progress heartbeat
/// (the owner mirrors its in-process `ProgressHandle` phase — same
/// claim-guarded UPDATE, still one statement per heartbeat), the
/// stall takeover ('downloading' for the new owner), and the
/// release-in-place (NULL).
///
/// Read by: the stall-takeover predicate (`STALL_TAKEOVER_PREDICATE`,
/// metadata/inline.rs) — strikes ONLY `'downloading'` claims whose
/// progress froze while liveness stayed fresh. Pre-092 the persist
/// exemption was the inference `fetched_bytes == nar_size` (only
/// correct when the competitor's expected size equalled the owner's),
/// budget-parked owners were deposed after >stall-window parks (their
/// progress froze with liveness fresh), and dead owners in the
/// 180-300s window were striked instead of reaped. See
/// store.substitute.stale-reclaim+3.
pub const M_092: () = ();

/// `migrations/093_live_pins_kind_key.sql`
///
/// Pin kinds become DISJOINT ROW SETS (bughunt wave, bug_253/bug_233;
/// D1 store-gc-claim-state):
///
/// - PK widens `(store_path_hash, drv_hash)` →
///   `(store_path_hash, drv_hash, pin_kind)`. A build_input pin and a
///   materialization pin for the same `(path, drv)` coexist with
///   independent lifecycles: build_input releases on the pinning
///   build's terminal status, materialization on the §5.3
///   all-interest-terminal rule. The pre-093 `ON CONFLICT … DO UPDATE`
///   re-kind (PD-10/DF-3's "release moves strictly later" argument)
///   was FALSE in the from_source sequence: re-kinding the build pin
///   and then resolving the job released the only row protecting a
///   still-live build's input.
/// - CHECK `scheduler_live_pins_materialization_job`: a
///   materialization pin without `job_id` is unrepresentable — the
///   §5.3 release rule resolves every pin's job, so the immortal
///   NULL-job pin class (bug_233: the store client swallowed job_id
///   parse failures into NULL) cannot exist. The store client now
///   refuses claims whose descriptor job_id does not parse
///   (`ClaimedJob.job_id: Uuid`, parse-don't-validate).
/// - The defensive DELETE is expected to remove 0 rows (the only
///   NULL-job writer was the store client's swallowed parse, and the
///   §5.3 release rule never matched those rows — any that exist are
///   exactly the immortal pins the CHECK now forbids).
///
/// The shared upsert text both crates execute against this key lives
/// in `rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL` (bug_192 —
/// PD-13's duplicated-SQL pair collapsed to one const).
pub const M_093: () = ();

/// `migrations/094_establishment_clusters.sql`
///
/// Maintained per-node clustering for the hung-node runbook
/// (bughunt wave, merged_bug_010). `establishment_clusters(window)`
/// groups establishment charges (`outcome_class = 'executor_crash'`,
/// `termination_reason = 'unreported'`) by **`drv_attempts
/// .source_node`** — the authority the establishment sweep PERSISTS
/// (`actor/housekeeping.rs` establishment arm: the attempt-row value
/// or the spawn-ack binding fallback; written through
/// `db/attempts.rs` append). The runbook's previous prose SQL joined
/// `drv_executions` and grouped on `e.source_node`, whose only
/// writers are the race-conditional pull mint and a report-only
/// backfill that never fires for a wedged-but-Ready pod — exactly
/// the rows this query exists to cluster came back NULL there.
///
/// - `window_span` (the spec sketch's `window`; renamed — `WINDOW`
///   is a reserved word) defaults to `'30 minutes'`, mirroring the
///   `RioSchedulerAttemptEstablishmentCluster` alert's `[30m]` range
///   so "the alert fired, run the query" sees the same population.
/// - `NULL` rows mean NEITHER the attempt row nor the spawn-ack
///   binding carried a node — not node-attributable; investigate the
///   scheduler/store, not a node.
/// - `STABLE` SQL function: one statement, planner-inlinable;
///   operators call `SELECT * FROM establishment_clusters();` instead
///   of maintaining hand-rolled SQL (the ops-SQL docs-lint forbids
///   raw `drv_*` queries in runbooks).
pub const M_094: () = ();

/// `migrations/095_drop_derivations_tenant_id.sql`
///
/// Drops the never-production-written `derivations.tenant_id`
/// (bughunt-2 wave, merged_bug_064; owner decision Q2, 2026-06-04).
/// `TailLog` ownership keyed on this column was constant-false in
/// production, and the test fixtures that stamped it proved a vacuous
/// truth; ownership is now **build-membership** over the
/// production-written `builds.tenant_id` chain
/// (`assignments`→`build_derivations`→`builds`, swept-assignment arm
/// via `drv_executions`⨝`derivations` on the execution's own recorded
/// hash) — see `store.log.tail-ownership` and
/// `rio_store::logs::tail::authorize_tail`.
///
/// Never-written census (re-run at the slot-4 candidate tip,
/// 4a9d9b535, 2026-06-04 — the evidence standard the Q2 signature
/// attests over):
/// - The sole production INSERT into `derivations`
///   (`batch_upsert_derivations`, rio-scheduler/src/db/batch.rs:105)
///   omits the column from its column list.
/// - `git log --all -S 'UPDATE derivations SET tenant_id'
///   --pickaxe-regex -- '*.rs'` → exactly a57034dd8 (branch-local,
///   `#[cfg(test)]` fixtures only) and the slot-4 commit REMOVING
///   those fixtures; `origin/main` history has zero writers.
/// - `.sqlx/` prepared queries and non-Rust sources carry no writer.
/// - Structural anchor: checksum-frozen `009_phase4.sql:29`
///   force-nulls the column before this migration can ever run, and
///   `derivations_tenant_fk` (ON DELETE SET NULL) bounded even manual
///   writes.
/// - Zero production reads remain at the candidate tip (the last
///   reader was the constant-false tail gate this wave re-keyed).
/// - The one non-tree residual — no environment carries out-of-band
///   manual writes to the column — is the owner attestation in the
///   §5-S Q2 signature (2026-06-04).
///
/// `DROP COLUMN` cascade-drops `derivations_tenant_fk` and the partial
/// index `derivations_tenant_idx` (009). Irreversible without a new
/// migration by design; the `authz-fixture-policy` misc-check bans
/// `INSERT`/`UPDATE` writes naming `derivations`…`tenant_id`
/// workspace-wide (allowlist: rio-migrations/) so the dead shape
/// cannot grow back in fixtures either.
pub const M_095: () = ();

/// `096_assignments_claim_nonce.sql` — bug_251 (rule-4b, SIGNED
/// 2026-06-04 at the scheduler.typ pull-contract amendment anchor).
///
/// `assignments.claim_nonce UUID NULL`: the client-chosen claim nonce,
/// the materialization resume credential that SURVIVES response loss.
/// The store worker mints a v4 BEFORE each `PullAssignment` and the
/// scheduler persists it with the assignment row at mint
/// (`mint_assignment_upsert_in_tx`); the kernel's re-delivery cell
/// then accepts `resume_exec_id` match OR persisted-nonce match —
/// the exec_id token travels only on the RESPONSE, so the one failure
/// mode re-delivery exists for (the lost response) was exactly the
/// case the token could never cover.
///
/// NULL = nonceless claim (old store, build pull): the kernel's nonce
/// leg never matches NULL (None==None is not a credential), so
/// pre-096 rows and rolling deployments degrade to the establishment
/// window — the pre-nonce behavior, never a wedge. Recovery hydrates
/// the nonce off the same guarded assignments join as `exec_id`
/// (`load_nonterminal_derivations`), so failover preserves both the
/// credential and the reset-clear contract.
pub const M_096: () = ();

/// `097_executor_confirm_fences.sql` — merged_bug_145 (bughunt-4 S5b;
/// R11 escalation in the wave-log, EXPLICIT orchestrator ACK; number
/// 097 per the §1.6 reservation).
///
/// One row per confirm-exited executor pod: the scheduler INSERTs it
/// when a `confirm_only` pull is answered "nothing held"
/// (NotYetReady/Gone) — the durable half of the builder's exit-0
/// license, written BEFORE the reply (write-ahead: no clean-exit
/// answer without the fence on disk). Any LATER `DeliverNew`
/// admission presenting the same executor token is screened to Gone:
/// pre-fence, a late abandoned pull (timed out client-side, still in
/// the actor mailbox or network) could mint an open attempt against a
/// Job that had already exited 0 (`Succeeded`), which the
/// establishment sweep — keyed to FAILED pods — would never reap.
///
/// Key: SHA-256 hex of the RAW executor token bytes, the finest pod
/// discriminator the wire carries (the token is per-intent claims
/// {intent_id, kind, expiry_unix}; a retry pod for the same intent
/// mints a fresh token with a later expiry → different bytes → not
/// fenced). Hash-only storage: the raw credential never lands in PG.
/// Disclosed residual (accepted at ACK): two pods minted in the SAME
/// second carry byte-identical tokens — indistinguishable on the wire
/// even without the fence; the consequence is one bounded charge-free
/// pod cycle (fenced pull answers Gone → pod exits 0 → controller
/// respawns), never a wedge or a false mint. Fences are garbage after
/// any straggler has long since timed out: the attempt-ledger
/// housekeeping tick deletes rows older than 24h (CONFIRM_FENCE_GC
/// in db/confirm_fences.rs).
pub const M_097: () = ();

/// `100_gc_collect_last_attempt.sql` — bug_284 (bughunt-4 S4; R11
/// escalation in the wave-log, number 100 chosen clear of the 097-099
/// reservations).
///
/// `gc_collect_state.last_attempt_at TIMESTAMPTZ NULL`: the live
/// collect ATTEMPT stamp, written under the GC cycle lease immediately
/// before every live cycle runs (backstop and run_gc phase 3 alike),
/// regardless of how the cycle ends. The backstop due-predicate
/// requires BOTH `last_live_cycle_at` staleness (the success cadence;
/// unchanged, still what the stalled alert keys on) AND
/// `last_attempt_at` staleness — so a cycle that aborts without
/// committing (the fail-closed ParseFailure on a corrupt chunk_list,
/// or a mid-cycle DB error) cannot be re-attempted faster than the
/// documented once-per-interval heavy-cycle cadence. Pre-fix the
/// hourly backstop check re-ran the full 4GB-work_mem validation scan
/// 24x/day for as long as a single corrupt manifest persisted — a
/// condition the fail-closed design expects to wait for a HUMAN.
/// Shadow (dry-run) cycles do NOT stamp it: a dry run must never defer
/// the live collection cadence.
pub const M_100: () = ();

/// `101_derivations_status_changed_at.sql` — merged_bug_004 (bughunt-6
/// S6; the wave's single sanctioned DDL, SIGNED Q3, R11-as-amended).
///
/// `derivations.status_changed_at TIMESTAMPTZ NOT NULL DEFAULT now()`:
/// the instant the row's `status` VALUE last changed. Comparand-purity
/// law: its only writers are the status-setting statements in
/// rio-scheduler/src/db/derivations.rs (every `UPDATE derivations`
/// whose SET list names `status` names this column; every SET list
/// omitting `status` omits it — the source-scan census
/// `derivations_status_stamp_census` in db/tests/fence_coverage.rs
/// enforces the biconditional). It exists because the outbox-replay
/// precedence conjunct previously cut on `updated_at` — a column EVERY
/// writer touches (the replay's own stamp, the resource-floor ratchet,
/// the merge-parity upsert), so the precedence law quantified over
/// "status events" while comparing against "any write": the replay's
/// own stamp refused newer truths, and a floor bump permanently
/// cancelled a latched terminal persist.
///
/// Backfill semantics: the PG fast-path stable default — every
/// pre-existing row reads the MIGRATION instant, not its true last
/// status transition. SAFE because the column's only consumer is the
/// in-memory leader-scoped status outbox, which cannot survive the
/// deploy restart (actor/mod.rs clears it on leadership loss); no
/// pre-migration latch can ever be compared against a backfilled
/// stamp.
///
/// No-op-write nuance — RETRACTED (bughunt-8 S4, migration 102). The
/// paragraph that stood here priced the five sibling writers'
/// unconditional `status_changed_at = now()` stamps as "a fresh
/// same-value write is a transition-time re-assertion; the error
/// direction is conservative (a too-new stamp only ever REFUSES a
/// stale replay, never admits one)". That pricing is FALSE for the
/// terminal-KEEP arm of the outbox re-derivation: a latched terminal
/// status for a DAG-absent node IS the node's last truth (the flush
/// keeps it precisely because nothing newer can exist), so a
/// value-preserving sibling write landing in the latch→flush window
/// advanced the comparand past the cut and the refused replay
/// dropped the node's FINAL status — the refusal was the error, not
/// the conservative arm. It also contradicted the spec's own MUST
/// ("a status-preserving write MUST NOT refuse a latched persist",
/// scheduler.typ `sched.attempt.cancel-close-driven`). Migration 102
/// moves the stamp into the column's single authority (see M_102):
/// the value-change guard is now schema law, not per-writer
/// discipline, and no Rust SET list names the column at all.
pub const M_101: () = ();

/// `102_derivations_status_changed_trigger.sql` — merged_bug_006
/// (bughunt-8 S4; the wave's single sanctioned DDL, H7,
/// R11-as-amended).
///
/// `derivations_stamp_status_changed()` + the BEFORE UPDATE trigger
/// `derivations_status_changed_stamp`, `WHEN (OLD.status IS DISTINCT
/// FROM NEW.status)`: the trigger is the SOLE author of
/// `status_changed_at` on UPDATE — the comparand moves IFF the status
/// VALUE changes, as a property of the column itself rather than of N
/// client statements. 101 shipped the column with the value-change
/// guard at exactly one of six writers (the outbox replay's `status
/// IS DISTINCT FROM $2`); the five siblings stamped unconditionally,
/// so a value-preserving write (same-status re-assignment with a new
/// builder id, duplicate batch cancel, `clear_poison` on a row
/// already `'created'`) advanced the comparand inside the
/// latch→flush window and popped a kept DAG-absent terminal latch as
/// `RefusedNewer` — permanently stale durable state (the M_101
/// retraction above). Post-102 every Rust writer DROPS the stamp
/// from its SET list (the census in db/tests/fence_coverage.rs
/// enforces the total ban: no production SET/DO-UPDATE/INSERT list
/// names the column); INSERTs ride the 101 DEFAULT (a fresh row's
/// status is born at insert). Test backdates (`SET status_changed_at
/// = …` without `status`) still land: the WHEN clause is false for
/// them.
pub const M_102: () = ();

/// `103_gc_holds_and_tombstones.sql` — round-9 WO-S1-4 (bw9-s1; the
/// wave's single sanctioned DDL, R11-as-amended, allocation BANKED
/// with the full ritual).
///
/// Two independent consequences of the signed Q1 registration
/// invariant ("completed uploads survive cancellation as registered
/// evidence"), both signed under the Q3 record:
///
/// **Evidence-outlives-bytes** (`path_tenant_tombstones`,
/// `realisation_tombstones`): the pre-103 sweep deleted the
/// registration stamps and identity rows WITH the path
/// (`delete_swept_path` steps 2a/2a′ — the Q3 record's "sweep.rs:238
/// deletes stamps with paths"). Tombstone tables, APPEND-ONLY at
/// sweep, chosen over in-place `deleted_at` columns deliberately:
/// every live reader of `path_tenants`/`realisations` (the visibility
/// projection, mark seed (f), tenant quota, gateway readers) keeps
/// its semantics untouched — an in-place tombstone would have forced
/// a `deleted_at IS NULL` filter into every consumer across four
/// crates and re-opened the wrong-tenant-revival leak the sweep
/// delete defends against. The copy runs INSIDE the sweep batch
/// transaction, so a swept path's records are atomically either live
/// or tombstoned, never lost. No FKs: tombstones reference history
/// (tenants/paths may be gone).
///
/// **GC-hold** (`gc_holds`): the first-class operator control the Q3
/// signature commissioned ("tonight's freeze was scale-to-0").
/// Typed axes per R17: `scope` global|tenant (CHECK-paired with
/// `tenant_id`), mandatory `reason`/`created_by`, `expires_at` NULL =
/// UNBOUNDED — an explicit operator decision recorded in the row,
/// not an accident (the column's nullability IS the recorded form);
/// `released_at` closes a hold without deleting it (the hold history
/// is itself audit evidence). Mark/sweep consult active holds: a
/// global hold short-circuits `run_gc` before mark; a tenant hold
/// joins seed (f) and the sweep re-check as a reachability conjunct.
pub const M_103: () = ();

/// `104_gc_holds_restrict.sql` — bw10 WO-S1-3 (bug_095; the wave's
/// single sanctioned DDL, R11-as-amended, full frozen ritual).
///
/// **The statement (the .sql carries only the pointer + bare DDL per
/// the migration-body policy):** drop and re-add
/// `gc_holds_tenant_id_fkey` as `ON DELETE RESTRICT` — the
/// never-deleted doctrine made schema: gc_holds rows are audit
/// evidence and carry NO live deletion vector.
///
/// **The doctrine made schema.** `gc_holds` is KeepForever-registered
/// ("released, never deleted — the hold history is audit evidence",
/// stated in gc/mod.rs, M_103 above, and the retention registry), yet
/// 103 shipped its `tenant_id` FK inline `ON DELETE CASCADE`: the
/// live `delete_tenant` admin path silently erased the tenant's
/// entire hold audit history AND any ACTIVE litigation-class hold
/// with no release record — an unwitnessed terminal disposition
/// outside the hold's {released, expired} alphabet. The
/// retention-truth lint's KeepForever arm was `Ok(())` by
/// construction (an EXEMPTION, not a checked claim), so the
/// registry-vs-schema contradiction was CI-green for a full wave.
///
/// **Why a NEW migration:** 103 is checksum-frozen (shipped; the
/// `migration_checksums_frozen` test pins its bytes) — its CASCADE
/// row is superseded by this RESTRICT re-declaration, never edited in
/// place. DROP + ADD under the same constraint name
/// (`gc_holds_tenant_id_fkey`, the PG default 103's inline form
/// minted).
///
/// **RESTRICT semantics, derived (not the no-FK tombstone sibling):**
/// the hold's protective conjunct joins through `path_tenants`
/// (also cascaded), so dropping the FK entirely would orphan
/// `tenant_id` while the CASCADE survived one hop away; RESTRICT
/// preserves the protective function — a tenant with ANY hold rows
/// (active OR released) refuses deletion at the schema layer.
/// Consequence, stated deliberately: a tenant that ever carried a
/// hold is PERMANENTLY archival — released holds keep their audit
/// rows and those rows pin their tenant anchor (deleting the tenants
/// row would orphan the audit's WHO). `delete_tenant` dispositions
/// the two faces typed: active holds → "release first" (the heal
/// edge stays witnessed); released-history → the archival refusal
/// naming this doctrine. `path_tenants` itself needs NO second DDL:
/// gc_holds RESTRICT structurally precedes any path_tenants cascade
/// (a tenant with hold rows cannot be deleted at all — derivation
/// recorded at the WO). Global-scope holds carry `tenant_id` NULL
/// and are untouched by tenant offboarding.
pub const M_104: () = ();
/// `migrations/105_executor_variant_outcome_class.sql`
///
/// The `executor_variant` outcome class joins the attempt-ledger
/// alphabet (sh-012, the E3a/E3b split; spec rule
/// `sched.retry.executor-variant-threshold`). Re-creates
/// `drv_attempts_outcome_class_check` as 088's set plus
/// `'executor_variant'`: the daemon's heuristic exit≠0
/// (`PermanentFailure`) and unclassified (`MiscFailure`) — the two
/// classifications whose verdict CAN vary by executor — now route to a
/// dedicated class so the scheduler's distinct-executor poison
/// threshold gates the conclusion (E3a, the kernel's `ExecutorVariant`
/// fold arm) instead of poisoning on first observation. The
/// derivation-INTRINSIC permanent statuses (CachedFailure,
/// DependencyFailed, LogLimitExceeded, OutputRejected,
/// NotDeterministic, InputRejected) keep `'permanent'` and stay
/// first-observation poison (E3b).
pub const M_105: () = ();
/// `migrations/106_floor_cores.sql`
///
/// The D4 reactive `ResourceFloor` gains its fourth axis (sh-012):
/// `floor_cores`, sibling to `M_044`'s mem/disk/deadline columns and
/// fed by the same per-dimension `GREATEST()` ratchet
/// (`update_resource_floor`). Fired only when an
/// `ExecutorVariantFailure` (E3a — the daemon's heuristic exit≠0) is
/// corroborated compute-bound: `cpu_seconds_total /
/// (assigned_deadline × assigned_cores) >= compute_bound_threshold`.
/// A genuine compile-error exit (`cpu_util ≪ threshold`) refuses the
/// witness and never escalates cores — the inverse-cost bound the E3a
/// requeue accepts (3 attempts at the same shape, never 3× resource).
/// The SLA model still owns INITIAL core selection; this is the
/// post-E3a corroborated escalation only. `integer` (i32 → u32 at
/// hydration), capped at `Ceilings.max_cores` consume-side.
pub const M_106: () = ();
/// `migrations/107_materialization_jobs_priority.sql`
///
/// `materialization_jobs.priority DOUBLE PRECISION NOT NULL DEFAULT 0`
/// (sh-025): the creating derivation's critical-path remaining-seconds
/// (`state.sched.priority` — `est_duration + max(non-terminal child)`,
/// `critical_path::compute_initial`). The claimable listing's ORDER BY
/// becomes `(priority DESC, created_at, job_id)` so hub dependencies
/// (rustc/stdenv — long dependent chains) are claimed before leaf
/// substitutions when both are pending; today every merge-tx job ties
/// on `created_at` and `job_id` is `Uuid::now_v7` mint order ≈
/// `pending_substitute` HashMap-iteration order — effectively random,
/// so the 2454-wide Queued→Ready promotion fired as a step function
/// near sub-end instead of overlapping ~70s of leaf substitution with
/// build. `DOUBLE PRECISION` not `bigint`: `sched.priority` is `f64`
/// (sums of `est_duration: f64`); `NOT NULL DEFAULT 0` so existing
/// rows degenerate to today's `(created_at, job_id)` order and no
/// `NULLS LAST` is needed. PG ≥ 11 makes a constant-default ADD
/// COLUMN metadata-only (brief ACCESS EXCLUSIVE, no table rewrite).
///
/// Split from the index DDL per the 011/022 CONCURRENTLY precedent
/// (review sh025-q3): bundling ADD COLUMN + DROP INDEX + CREATE INDEX
/// in one file would take SHARE/ACCESS EXCLUSIVE on the hot table for
/// the full index build during a rolling deploy.
pub const M_107: () = ();
/// `migrations/108_materialization_jobs_priority_idx.sql`
///
/// `-- no-transaction` + sole statement `CREATE INDEX CONCURRENTLY IF
/// NOT EXISTS materialization_jobs_pending_priority ON
/// materialization_jobs (priority DESC, created_at, job_id) WHERE
/// state = 'pending'` — the partial index for [`M_107`]'s
/// `(priority DESC, created_at, job_id)` listing ORDER BY, so PG
/// serves the `LIMIT n` head-window via index scan instead of
/// seqscan+sort over the full pending set. Standalone-file
/// CONCURRENTLY per the 011/022 pattern: a multi-statement file is an
/// implicit transaction block even with `-- no-transaction`, and
/// CONCURRENTLY cannot run inside one; sqlx detects the directive via
/// `sql.starts_with("-- no-transaction")` so it MUST be line 1 (the
/// migration-body-policy pointer moves to line 2). Ordered
/// new-before-drop ([`M_109`]) so the listing query is never without
/// index coverage. `IF NOT EXISTS` for idempotency across re-runs; if
/// CONCURRENTLY fails mid-build it may leave an INVALID index behind
/// — recovery is `DROP INDEX materialization_jobs_pending_priority`
/// then re-run this migration.
pub const M_108: () = ();
/// `migrations/109_materialization_jobs_drop_old_idx.sql`
///
/// `-- no-transaction` + sole statement `DROP INDEX CONCURRENTLY IF
/// EXISTS materialization_jobs_pending` — retires 078's
/// `(created_at) WHERE state = 'pending'` partial index now that
/// [`M_108`] covers the listing query (its sole consumer). Plain
/// `DROP INDEX` takes ACCESS EXCLUSIVE on `materialization_jobs` and
/// would queue behind an in-flight merge transaction, head-of-line
/// blocking listing reads; CONCURRENTLY waits out conflicting
/// transactions without holding the table lock. Standalone file for
/// the same implicit-transaction-block reason as [`M_108`].
pub const M_109: () = ();

/// `migrations/110_nar_index.sql`
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
/// closure_hole) landed while ADR-022 was in flight, so it's 110,
/// post formal-sprint rebase.
pub const M_110: () = ();

/// 111 — `file_blobs.size` (P0577/P0570).
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
/// no pre-111 rows to backfill: 110 and 111 ship in the same release.
pub const M_111: () = ();

/// 112 — `directory_paths` + drop `directory_tenants`/`file_blob_tenants`.
///
/// 110's `directory_tenants`/`file_blob_tenants` were a one-shot
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
pub const M_112: () = ();

/// 113 — drop `manifests.nar_indexed` + `manifests_nar_index_pending_idx`.
///
/// Both were the background NAR indexer's work-queue state (110/P0551):
/// the partial index `WHERE NOT nar_indexed AND status = 'complete'`
/// fed the indexer's drain query, and the flag flipped once a path's
/// castore index was written. The background indexer is gone — the
/// castore index is now written eagerly in the same transaction that
/// completes the manifest (`complete_manifest_in_conn` →
/// `set_nar_index_in_conn`), so the flag was write-only and the partial
/// index had zero readers while still being maintained on every
/// `manifests` UPDATE.
pub const M_113: () = ();

// BURNED NUMBERS — 069 and 070 (next free migration: 071):
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
pub const M_114: () = ();

// Add M_NNN consts for other migrations as commentary accumulates.
// Not all migrations need one — only those with non-obvious history,
// dead-code constraints, or "we chose X over Y" rationale. The .sql
// files carry the WHAT; this module carries the WHY.
