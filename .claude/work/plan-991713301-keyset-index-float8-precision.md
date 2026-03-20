# Plan 991713301: Keyset query — composite index + float8 precision

Two defects in [P0271](plan-0271-cursor-pagination-admin-builds.md)'s `list_builds_keyset` query at [`db.rs:368-395`](../../rio-scheduler/src/db.rs) (p271 worktree). Surfaced by rev-p271.

**Index claim is false.** Comment at `:365` says "planner can use the PK-implied btree" for `ORDER BY submitted_at DESC, build_id DESC`. PK is `build_id` only ([`migrations/001_scheduler.sql:17`](../../migrations/001_scheduler.sql)). No index on `(submitted_at, build_id)`. The row-value WHERE + ORDER BY seq-scans on large tables — defeats the "O(limit) per page regardless of depth" claim in the [`builds.rs:89`](../../rio-scheduler/src/admin/builds.rs) docstring. Also: `count_builds()` is called per page (`:376`) — `COUNT(*)` is O(n) seq-scan every time, so total work is O(n × pages) not O(n + limit × pages).

**Float8 round-trip is lossy.** `:381` binds `to_timestamp($3::bigint / 1e6)`. The `/ 1e6` is bigint÷numeric → numeric, but PG's `to_timestamp(double precision)` overload coerces numeric→float8. An epoch-microsecond near 2026 is ~1.74×10¹⁵ → 1.74×10⁹ seconds with 6 decimals = 16 significant figures, right at the IEEE754 double precision limit. A page-boundary row whose `submitted_at` loses a microsecond in the round-trip gets skipped (the row-value `<` becomes `<=` for that microsecond-bucket) or duplicated. Comment at `:366-367` claims "preserves PG's native microsecond precision exactly" — not proven through float8.

Both gaps make P0271's keyset pagination WORSE than the offset path it replaces for mid-size tables.

## Entry criteria

- [P0271](plan-0271-cursor-pagination-admin-builds.md) merged (`list_builds_keyset` at [`db.rs`](../../rio-scheduler/src/db.rs) + `decode_cursor` at [`admin/builds.rs`](../../rio-scheduler/src/admin/builds.rs) exist)

## Tasks

### T1 — `perf(scheduler):` migration — composite keyset index

NEW `migrations/022_builds_keyset_idx.sql`:

```sql
-- no-transaction
-- Keyset pagination index for AdminService.ListBuilds cursor mode.
-- Row-value WHERE (submitted_at, build_id) < (x, y) + ORDER BY DESC needs
-- a matching composite btree. PK is build_id only (001_scheduler.sql:17);
-- without this index the keyset query seq-scans.
--
-- `-- no-transaction` marker (line 1) tells sqlx-migrate to skip the
-- implicit BEGIN/COMMIT wrapper — CREATE INDEX CONCURRENTLY cannot run
-- inside a transaction block. Precedent: migrations/011_refscan_backfill_idx.sql:1.
CREATE INDEX CONCURRENTLY IF NOT EXISTS builds_keyset_idx
    ON builds (submitted_at DESC, build_id DESC);
```

`CONCURRENTLY` for production-safe deploy (non-blocking). `IF NOT EXISTS` for idempotency across re-runs. The `-- no-transaction` sqlx marker on line 1 is REQUIRED — `CREATE INDEX CONCURRENTLY` fails with `ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block` without it; sqlx wraps migrations in BEGIN/COMMIT by default. The DESC-DESC ordering matches the query's ORDER BY exactly — PG can use the index for both the row-value filter and the sort, avoiding a separate Sort node.

Delete the false "PK-implied btree" comment at [`db.rs:365`](../../rio-scheduler/src/db.rs). Replace with:

```rust
/// Row-value comparison `(a, b) < (x, y)` is SQL-standard lexicographic.
/// Uses `builds_keyset_idx` (migration 022) — composite DESC-DESC matches
/// this query's ORDER BY, so the planner does an index-scan + no Sort.
```

### T2 — `fix(scheduler):` float8 round-trip — compare on integer microseconds

MODIFY [`db.rs`](../../rio-scheduler/src/db.rs) `list_builds_keyset` query. Two routes:

**Route (a) — compare on bigint, not timestamp.** Keep the cursor's micros representation but compare on the EXTRACT side, not the to_timestamp side:

```sql
WHERE ( (EXTRACT(EPOCH FROM b.submitted_at) * 1000000)::bigint, b.build_id )
    < ( $3::bigint, $4::uuid )
```

Integer-only arithmetic, exact round-trip. **Requires an expression index** for the planner to use it (or it seq-scans) — add to T1's migration:

```sql
CREATE INDEX CONCURRENTLY IF NOT EXISTS builds_keyset_micros_idx
    ON builds (((EXTRACT(EPOCH FROM submitted_at) * 1000000)::bigint) DESC, build_id DESC);
```

**Route (b) — `make_timestamp` from integer parts.** Avoid float8 by reconstructing the timestamp from the bigint via integer division + modulo:

```sql
to_timestamp($3 / 1000000) + make_interval(0, 0, 0, 0, 0, 0, ($3 % 1000000) / 1e6)
```

Still has a `/ 1e6` but on a ≤6-digit remainder — well within float8 exactness. Uses T1's `builds_keyset_idx` directly (compares on native TIMESTAMPTZ). Cleaner; no expression index.

**Prefer (b).** Route (a) needs an expression index + changes the SELECT's `micros` projection for consistency; route (b) is a single-expression swap in the WHERE.

Update the `:366-367` comment to explain exactness through integer-remainder path, not hand-waved "native precision."

### T3 — `perf(scheduler):` count_builds — don't run per page

MODIFY [`db.rs`](../../rio-scheduler/src/db.rs) `list_builds_keyset`. The `count_builds()` call at `:376` runs a `COUNT(*)` seq-scan on EVERY page — defeats keyset's O(limit)-per-page guarantee. Three options:

- **(a)** Only call `count_builds` when `cursor_micros == i64::MAX` (first page). Subsequent pages carry `total_count` from the first response — client already has it. Change `list_builds_keyset` signature to `Option<i64>` total; `builds.rs` populates `total_count` only on first page (or `0` on subsequent — proto allows it, dashboard reads `total` once).
- **(b)** Cache `count_builds` result in `SchedulerDb` with a short TTL (30s — same cadence as the autoscaler's `ClusterStatus` poll). Dashboard doesn't need exact counts mid-walk.
- **(c)** Approximate via `pg_class.reltuples` (free, stale by up to autovacuum cadence). Acceptable for a dashboard "~N builds" display.

**Prefer (a).** Dashboard renders the page-count widget once; chained cursor requests don't need a fresh total. Option (b) adds state to `SchedulerDb`; option (c) is confusing when filters apply (reltuples is whole-table).

### T4 — `test(scheduler):` index-usage + round-trip proof

MODIFY [`rio-scheduler/src/admin/tests.rs`](../../rio-scheduler/src/admin/tests.rs) (or wherever P0271-T3's pagination test landed):

1. **Round-trip exactness:** insert a build with `submitted_at = '2026-03-20T13:17:30.123456Z'` (6-decimal microsecond precision), encode a cursor from its micros, decode+query, assert the row is NOT returned (it's the boundary — `<` excludes it) AND the row at `.123457Z` IS returned. Proves the T2 fix: pre-fix, float8 jitter makes this non-deterministic at the boundary.

2. **Index-usage (optional, dev-only):** `EXPLAIN (FORMAT JSON)` the keyset query, assert `"Index Name": "builds_keyset_idx"` appears. Gates on postgres version (EXPLAIN format stable since PG11). `#[cfg_attr(not(debug_assertions), ignore)]` — CI cost not worth it, but catches a dropped-index regression in local dev.

## Exit criteria

- `/nbr .#ci` green
- `ls migrations/022_builds_keyset_idx.sql` — exists
- `head -1 migrations/022_builds_keyset_idx.sql | grep '^-- no-transaction'` → match (sqlx CONCURRENTLY marker present — precedent migrations/011:1)
- `grep 'PK-implied btree' rio-scheduler/src/db.rs` → 0 hits (false claim removed)
- `grep 'builds_keyset_idx' migrations/022_builds_keyset_idx.sql rio-scheduler/src/db.rs` → ≥2 hits (migration + comment reference)
- `grep '/ 1e6' rio-scheduler/src/db.rs | grep -v 'remainder\|modulo\|% 1000000'` → 0 hits in `list_builds_keyset` body (direct bigint/1e6 path removed; remainder-division allowed)
- `grep 'count_builds' rio-scheduler/src/db.rs | grep -c 'list_builds_keyset\|fn '` — `count_builds` either not called from keyset body, OR gated on `cursor_micros == i64::MAX` (option-a)
- `cargo nextest run -p rio-scheduler pagination` — T4's boundary test passes
- T4 boundary self-check: the test asserts BOTH exclusion of the exact-boundary row AND inclusion of the next-microsecond row — proves `<` semantics hold exactly, not `<=` or lossy

## Tracey

References existing markers:
- `r[sched.admin.list-builds]` — T1/T2/T3 refine the implementation (index correctness + precision), T4 verifies

No new markers. This plan fixes defects in pagination plumbing under an existing spec marker; the spec text at [`scheduler.md:133`](../../docs/src/components/scheduler.md) mentions `LIMIT/OFFSET` today — P0271 extends it to cursor mode but this plan doesn't change the spec contract.

## Files

```json files
[
  {"path": "migrations/022_builds_keyset_idx.sql", "action": "NEW", "note": "T1: composite (submitted_at DESC, build_id DESC) CONCURRENTLY index"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T1: delete false PK-implied comment :365. T2: route-(b) integer-remainder to_timestamp reconstruction at :381. T3: option-(a) count_builds first-page-only"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "T3: total_count Option handling — populate only when cursor absent (first page)"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "T4: keyset boundary round-trip test + optional EXPLAIN index-name assert"}
]
```

```
migrations/
└── 022_builds_keyset_idx.sql    # T1: NEW — composite DESC-DESC index
rio-scheduler/src/
├── db.rs                        # T1+T2+T3: comment fix, float8 fix, count-once
└── admin/
    ├── builds.rs                # T3: total_count option-a wiring
    └── tests.rs                 # T4: boundary round-trip proof
```

## Dependencies

```json deps
{"deps": [271], "soft_deps": [991713302], "note": "Fixes two defects in P0271's keyset query. T3's option-(a) first-page-only total assumes dashboard reads total once — P991713302 (cursor adoption) migrates dashboard to chain next_cursor and can rely on total_count=0 on page 2+. sqlx migration checksum lock: 022 is next-available (021_derivation_status_skipped.sql is highest). db.rs HOT count=41 — T1-T3 touch :360-400 (list_builds_keyset + count_builds); P0304-T139 touches :840 (comment-only, non-overlapping); P0304-T77 touches insert_jwt_claim (different fn)."}
```

**Depends on:** [P0271](plan-0271-cursor-pagination-admin-builds.md) — `list_builds_keyset` + cursor codec exist.
**Conflicts with:** [`db.rs`](../../rio-scheduler/src/db.rs) count=41 (HOT) — T1-T3 touch `:360-400`; other plans (P0304-T77/T139) touch different functions. Migration `022` is next-available after `021_derivation_status_skipped.sql`.
