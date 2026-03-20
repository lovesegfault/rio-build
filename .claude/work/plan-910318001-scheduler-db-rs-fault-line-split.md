# Plan 910318001: Split scheduler db.rs — 2859L monolith along 9 section fault-lines

consol-mc220 finding at [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs). At 2859L and **collision count 42** (highest in the repository — `onibus collisions top 1` → `42 rio-scheduler/src/db.rs`), db.rs is the single hottest file in the DAG. 72 `pub async fn` query methods, all on a single `SchedulerDb` impl block, plus a 1204L inline `mod tests` (27 `#[test]`/`#[tokio::test]` fns). The section banners were added incrementally as each domain's queries accreted (`:312` Tenant ops, `:470` Build ops, …, `:1123` recovery reads) — the fault lines are ALREADY MARKED, the split is mechanical.

[P0403](plan-0403-keyset-index-float8-precision.md) just rewrote `list_builds_keyset` at `:384` + deleted the `:365` comment + added `count_builds` gating at `:376` — typical plan-touches-db.rs pattern: one 40L region in a 2859L file, but the whole file invalidates crane's `rio-scheduler` drv for every OTHER in-flight plan. [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md)/[P0386](plan-0386-admin-tests-rs-split-submodule-seams.md)/[P0395](plan-0395-grpc-tests-rs-split-submodule-seams.md)/[P0396](plan-0396-workerpool-tests-rs-split-submodule-seams.md) established the per-domain-submodule-under-`mod.rs` pattern for `admin/`, `grpc/`, `workerpool/tests/`; this applies the same to `db/`.

Nine production-code sections (banners at `:312`, `:470`, `:566`, `:710`, `:838`, `:863`, `:1010`, `:1090`, `:1123`) map onto eight files: `mod.rs` (SchedulerDb struct + pool + `TERMINAL_STATUS_SQL` const + DerivationStatus impls + shared newtype rows), then one file per domain. The `:838` build-derivation-mapping section is 25L and folds into `:863` batch ops (both serve `persist_merge_to_db`). The inline `mod tests` (`:1655-2859`, 1204L) moves to a `db/tests/` subdir split along the same seams — `r[verify sched.db.*]` annotations at `:2423`, `:2492`, `:2529`, `:2584` move with their tests.

## Entry criteria

- [P0403](plan-0403-keyset-index-float8-precision.md) merged (rewrites `list_builds_keyset` + `count_builds` gating + deletes `:365` false-PK comment — without it the split carries stale code that P0403 then has to re-locate across two new files)

## Tasks

### T1 — `refactor(scheduler):` db/mod.rs — SchedulerDb struct + shared consts + row types

NEW [`rio-scheduler/src/db/mod.rs`](../../rio-scheduler/src/db/mod.rs) carrying the shared surface:

| Item | Current `db.rs` lines | Notes |
|---|---|---|
| `TERMINAL_STATUS_SQL` const + doc-comment | `:22-51` | `r[impl sched.db.partial-index-literal]` at `:22` — annotation moves with it |
| `DerivationStatus::as_str` + `is_terminal` + `FromStr` | `:112-160` (approx) | Ground-truth enum impls — shared by all domain modules |
| `BuildState::as_str` + `FromStr` | adjacent | Same — `builds.rs` and `recovery.rs` both consume |
| Row structs (`BuildRow`, `TenantRow`, `BuildHistoryRow`, `RecoveryBuildRow`, …) | scattered `:160-300` | One-file home for every `#[derive(FromRow)]` — domain modules import |
| `SchedulerDb` struct + `new(PgPool)` + `pool()` | `:303-310` | Struct definition + constructor. Each domain module adds an `impl SchedulerDb` block |
| `list_builds` + `list_builds_keyset` | `:326-428` (post-P0403) | Listing — could go in `builds.rs` but these are the admin-facing read queries; keep in mod.rs alongside `list_tenants`. OR move to `builds.rs` — check at dispatch which makes `mod.rs` smallest |
| `pub mod tenants; pub mod builds; …` | new | Module declarations |
| `mod tests;` | new | Points at `tests/` subdir (T9) |

**Care — `TERMINAL_STATUS_SQL`:** all three `format!("… NOT IN {TERMINAL_STATUS_SQL}")` callsites (in recovery reads at `:1140`, `:1157`, `:1184` approx) must resolve to the mod.rs const. Either `use super::TERMINAL_STATUS_SQL` in `recovery.rs` or `pub(super) const` with explicit path. The `#[cfg(test)] const TERMINAL_STATUSES: &[&str]` at `:44-51` is tests-only — moves to `tests/mod.rs`.

Target `mod.rs` size: ≤400L (struct + consts + row types + module decls; no query bodies).

### T2 — `refactor(scheduler):` db/tenants.rs — tenant CRUD queries

Extract `:312-469` (section banner "Tenant operations") into `db/tenants.rs`:

- `list_tenants()` `:430`
- `create_tenant()` `:445`
- any tenant-key queries in the same block (grep `tenant_keys` at dispatch — migration-017 FK may have added query here)

Adds `use super::{SchedulerDb, TenantRow};` + `impl SchedulerDb { … }` block. Rust permits multiple `impl` blocks across files for the same struct.

### T3 — `refactor(scheduler):` db/builds.rs — build CRUD + status transitions

Extract `:470-565` (section banner "Build operations"):

- `insert_build()` `:480`
- `delete_build()` `:520`
- `update_build_status()` `:528`

If T1 did NOT keep `list_builds`/`list_builds_keyset` in mod.rs, they land here instead. Check collision implications: `list_builds_keyset` is what P0403 just rewrote — P0404 (`cursor-chained paging`) soft-conflicts with it but is dashboard-side.

### T4 — `refactor(scheduler):` db/derivations.rs — per-derivation state + poison

Extract `:566-709` (section banner "Derivation operations"):

- `update_derivation_status()` `:571`
- `increment_retry_count()` `:594`
- `append_failed_worker()` `:618`
- `persist_poisoned()` `:646`
- `clear_poison()` `:662`
- `load_poisoned_derivations()` `:691`

Poison + retry counters are ONE logical domain. `r[sched.poison.ttl-persist]` at [`scheduler.md:106`](../../docs/src/components/scheduler.md) — check for `r[impl]` annotation at `:646` (persist_poisoned) that moves with it.

### T5 — `refactor(scheduler):` db/live_pins.rs + db/batch.rs — scheduler_live_pins + persist_merge batch ops

Two small adjacent sections combine:

`:710-837` ("scheduler_live_pins — auto-pin live-build input closure"):
- `pin_live_inputs()` `:722`
- `upsert_path_tenants()` `:766`
- `unpin_live_inputs()` `:803`
- `sweep_stale_live_pins()` `:821`

`:838-1009` ("Build-derivation mapping" + "Batch operations (for persist_merge_to_db)"):
- `insert_build_derivation()` `:843`
- `batch_upsert_derivations()` `:874` — **`r[impl sched.db.batch-unnest]` at `:867` moves with this**
- `batch_insert_build_derivations()` `:963`
- `batch_insert_edges()` `:988`

Whether to combine into one `batch.rs` or split `live_pins.rs`/`batch.rs`: check at dispatch. `live_pins.rs` ~120L is small but conceptually distinct (GC-pin vs merge-persist). Prefer two files — `live_pins.rs` (~120L) + `batch.rs` (~180L).

### T6 — `refactor(scheduler):` db/assignments.rs — assignment CRUD

Extract `:1010-1089` (section banner "Assignment operations"):

- `insert_assignment()` `:1015`
- `delete_latest_assignment()` `:1044`
- `update_assignment_status()` `:1056`

Small domain (~80L). `r[sched.assign.*]` markers at [`scheduler.md:456`](../../docs/src/components/scheduler.md) — check for `r[impl]` annotations here.

### T7 — `refactor(scheduler):` db/history.rs — build_history EMA + samples

Extract `:1090-1122` ("Build history (async/batched)") + the sample-retention queries at `:1450-1530`:

- `read_build_history()` `:1112`
- `update_build_history()` `:1293`
- `update_build_history_misclassified()` `:1356`
- `update_ema_peak_memory_proactive()` `:1412`
- `insert_build_sample()` `:1450`
- `delete_samples_older_than()` `:1475`
- `query_build_samples_last_days()` `:1497`

The "history" section banner is at `:1090` but the history-related queries straddle past `:1293`. Include everything with `build_history` or `build_samples` in the SQL. `r[sched.estimate.ema-alpha]` at [`scheduler.md:192`](../../docs/src/components/scheduler.md) — check for impl annotation on `EMA_ALPHA` const (moves to mod.rs if shared, else history.rs).

### T8 — `refactor(scheduler):` db/recovery.rs — leadership-acquire state reload

Extract `:1123-1654` ("Phase 3b: state recovery read queries"):

- `load_nonterminal_builds()` `:1140`
- `load_nonterminal_derivations()` `:1157`
- `load_edges_for_derivations()` `:1184`
- `load_build_derivations()` `:1205`
- `max_assignment_generation()` `:1235`
- `max_sequence_per_build()` `:1251`
- `load_build_graph()` `:1533`
- `read_event_log()` `:1634` (top-level fn, not on SchedulerDb — check at dispatch)

`r[sched.recovery.fetch-max-seed]` at [`scheduler.md:416`](../../docs/src/components/scheduler.md) — `r[impl]` likely on `max_assignment_generation` or `max_sequence_per_build`. Recovery reads consume `TERMINAL_STATUS_SQL` — `use super::TERMINAL_STATUS_SQL`.

### T9 — `refactor(scheduler):` db/tests/ — split 1204L inline mod along domain seams

Move `:1655-2859` inline `mod tests` to `db/tests/` subdir mirroring the production split:

```
db/tests/
├── mod.rs              # shared: TestDb import, roundtrip tests (:1665-1700),
│                       # TERMINAL_STATUSES cfg-test const, test_partial_index_predicate_matches_const
├── tenants.rs          # tenant CRUD tests
├── builds.rs           # build insert/delete/status tests
├── derivations.rs      # poison/retry tests
├── batch.rs            # batch_upsert tests (r[verify sched.db.batch-unnest] :2529,:2584)
├── history.rs          # EMA + samples tests (the ema_peak_memory tests at :2106-2160)
├── recovery.rs         # load_nonterminal_* tests
└── transactions.rs     # "Remediation 12: PG transaction safety" :2377-2859
                        # (r[verify sched.db.partial-index-literal] :2423,:2492)
```

The two `r[verify sched.db.batch-unnest]` (`:2529`, `:2584`) go to `tests/batch.rs`; the two `r[verify sched.db.partial-index-literal]` (`:2423`, `:2492`) go to `tests/transactions.rs` (or `tests/mod.rs` if they're the `TERMINAL_STATUSES` drift-checks). `test_ema_alpha_range` at `:1660` is a compile-time const-assert — stays in `tests/mod.rs` or promotes to a `const _: () = assert!(…)` in `mod.rs` itself (no test fn needed for const-eval).

### T10 — `refactor(scheduler):` lib.rs + actor/*.rs import paths — back-compat re-exports

`rio-scheduler/src/lib.rs` currently does `pub mod db;` — stays. Callers (`actor/mod.rs`, `grpc/mod.rs`, `admin/mod.rs`, `actor/recovery.rs`) import `crate::db::SchedulerDb` + row types — all continue to resolve via `db/mod.rs` re-exports with zero caller change.

Check at dispatch:
```bash
grep -r 'use crate::db::\|use super::db::' rio-scheduler/src/ --include='*.rs'
```

Each import must either (a) resolve to a `pub` re-export in `db/mod.rs`, or (b) get a path update. Add `pub use` lines in `db/mod.rs` for anything callers need that ends up in a submodule.

## Exit criteria

- `/nixbuild .#ci` green (or clause-4c nextest-standalone if VM-flake)
- `test ! -f rio-scheduler/src/db.rs` — monolith deleted
- `wc -l rio-scheduler/src/db/mod.rs` → ≤400L (shared struct + consts + row types + module decls; 85% of the 2859L is in submodules)
- `ls rio-scheduler/src/db/` → contains `mod.rs tenants.rs builds.rs derivations.rs live_pins.rs batch.rs assignments.rs history.rs recovery.rs tests/`
- `wc -l rio-scheduler/src/db/tests/*.rs | tail -1` → ~1200L total (same as pre-split inline mod; no test lost)
- `cargo nextest list -p rio-scheduler 2>/dev/null | grep '::db::' | wc -l` — pre-split count vs post-split count identical (capture pre-split via `cargo nextest list -p rio-scheduler | grep db:: | wc -l` before T1)
- `grep 'r\[impl sched.db' rio-scheduler/src/db/` → ≥2 hits (partial-index-literal on TERMINAL_STATUS_SQL in mod.rs; batch-unnest on batch_upsert in batch.rs)
- `grep 'r\[verify sched.db' rio-scheduler/src/db/tests/` → ≥4 hits (all four moved intact)
- `nix develop -c tracey query rule sched.db.partial-index-literal` → shows ≥1 impl + ≥2 verify sites (annotations moved with code, not dropped)
- `nix develop -c tracey query rule sched.db.batch-unnest` → shows ≥1 impl + ≥2 verify sites
- `grep -r 'use crate::db::' rio-scheduler/src/ --include='*.rs' | grep -v 'src/db/'` → all paths compile-resolve (check via `cargo check -p rio-scheduler`)
- `.claude/bin/onibus collisions check rio-scheduler/src/db.rs` → 0 (path no longer exists post-split; the 42 collisions migrate to `db/mod.rs` + `db/<domain>.rs` as plan-docs update their Files fences)
- `.claude/bin/onibus collisions check rio-scheduler/src/db/mod.rs` → ≤10 (most plan-docs touch ONE domain — only cross-domain plans hit mod.rs; expect single-digit)

## Tracey

References existing markers:
- `r[sched.db.partial-index-literal]` — T1 keeps the impl annotation on `TERMINAL_STATUS_SQL` in `mod.rs`; T9 keeps two verify sites in `tests/transactions.rs`
- `r[sched.db.batch-unnest]` — T5's `batch.rs` carries the impl annotation; T9's `tests/batch.rs` carries both verify sites
- `r[sched.db.tx-commit-before-mutate]` — check at dispatch for impl/verify sites in the current `:2377-2859` transaction-safety block

No new markers. This is a file-layout refactor; the spec contract at [`scheduler.md:520-526`](../../docs/src/components/scheduler.md) is unchanged.

**Post-split tracey sanity:** `nix develop -c tracey query validate` → `0 total error(s)` (no annotations dropped/typo'd during the move). Kill the tracey daemon first (`ps aux | grep 'tracey daemon' | grep -v grep | awk '{print $2}' | xargs kill`) to force rescan — cached results would silently pass.

## Files

```json files
[
  {"path": "rio-scheduler/src/db/mod.rs", "action": "NEW", "note": "T1: SchedulerDb struct+new+pool; TERMINAL_STATUS_SQL const (r[impl sched.db.partial-index-literal]); DerivationStatus/BuildState as_str+is_terminal+FromStr; row structs (BuildRow/TenantRow/BuildHistoryRow/RecoveryBuildRow etc.); pub mod declarations + pub use re-exports. Target ≤400L"},
  {"path": "rio-scheduler/src/db/tenants.rs", "action": "NEW", "note": "T2: impl SchedulerDb { list_tenants, create_tenant, tenant_keys queries }. Source :312-469 section"},
  {"path": "rio-scheduler/src/db/builds.rs", "action": "NEW", "note": "T3: impl SchedulerDb { insert_build, delete_build, update_build_status, list_builds+list_builds_keyset if not in mod.rs }. Source :470-565 section"},
  {"path": "rio-scheduler/src/db/derivations.rs", "action": "NEW", "note": "T4: impl SchedulerDb { update_derivation_status, increment_retry_count, append_failed_worker, persist_poisoned, clear_poison, load_poisoned_derivations }. Source :566-709 section"},
  {"path": "rio-scheduler/src/db/live_pins.rs", "action": "NEW", "note": "T5: impl SchedulerDb { pin_live_inputs, upsert_path_tenants, unpin_live_inputs, sweep_stale_live_pins }. Source :710-837 section (~120L)"},
  {"path": "rio-scheduler/src/db/batch.rs", "action": "NEW", "note": "T5: impl SchedulerDb { insert_build_derivation, batch_upsert_derivations (r[impl sched.db.batch-unnest]), batch_insert_build_derivations, batch_insert_edges }. Source :838-1009 (~180L)"},
  {"path": "rio-scheduler/src/db/assignments.rs", "action": "NEW", "note": "T6: impl SchedulerDb { insert_assignment, delete_latest_assignment, update_assignment_status }. Source :1010-1089 (~80L)"},
  {"path": "rio-scheduler/src/db/history.rs", "action": "NEW", "note": "T7: impl SchedulerDb { read_build_history, update_build_history, update_build_history_misclassified, update_ema_peak_memory_proactive, insert_build_sample, delete_samples_older_than, query_build_samples_last_days }. Source :1090-1122 + :1293-1530"},
  {"path": "rio-scheduler/src/db/recovery.rs", "action": "NEW", "note": "T8: impl SchedulerDb { load_nonterminal_builds, load_nonterminal_derivations, load_edges_for_derivations, load_build_derivations, max_assignment_generation, max_sequence_per_build, load_build_graph }. Plus top-level read_event_log() fn. Source :1123-1654. use super::TERMINAL_STATUS_SQL"},
  {"path": "rio-scheduler/src/db/tests/mod.rs", "action": "NEW", "note": "T9: shared test imports (TestDb, seed_tenant); roundtrip tests (:1665-1700); TERMINAL_STATUSES cfg-test const; test_partial_index_predicate_matches_const; pub mod declarations"},
  {"path": "rio-scheduler/src/db/tests/tenants.rs", "action": "NEW", "note": "T9: tenant CRUD integration tests"},
  {"path": "rio-scheduler/src/db/tests/builds.rs", "action": "NEW", "note": "T9: build insert/delete/status tests"},
  {"path": "rio-scheduler/src/db/tests/derivations.rs", "action": "NEW", "note": "T9: poison/retry/status transition tests"},
  {"path": "rio-scheduler/src/db/tests/batch.rs", "action": "NEW", "note": "T9: batch_upsert_derivations tests (r[verify sched.db.batch-unnest] ×2 from :2529,:2584)"},
  {"path": "rio-scheduler/src/db/tests/history.rs", "action": "NEW", "note": "T9: EMA update + build_samples retention tests (:2106-2160 ema_peak_memory scenarios)"},
  {"path": "rio-scheduler/src/db/tests/recovery.rs", "action": "NEW", "note": "T9: load_nonterminal_*/max_*/recovery-reload tests"},
  {"path": "rio-scheduler/src/db/tests/transactions.rs", "action": "NEW", "note": "T9: 'Remediation 12: PG transaction safety' block :2377-2859 (r[verify sched.db.partial-index-literal] ×2 from :2423,:2492)"},
  {"path": "rio-scheduler/src/db.rs", "action": "DELETE", "note": "T1-T9: replaced by db/ directory"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T10: verify 'pub mod db;' resolves to db/mod.rs (likely zero change — Rust auto-discovers db/mod.rs)"}
]
```

```
rio-scheduler/src/db/
├── mod.rs              # T1:  SchedulerDb + TERMINAL_STATUS_SQL + row types + re-exports  ≤400L
├── tenants.rs          # T2:  tenant CRUD                                                  ~150L
├── builds.rs           # T3:  build CRUD + status + list_builds[_keyset]                   ~200L
├── derivations.rs      # T4:  derivation status + poison + retry                           ~180L
├── live_pins.rs        # T5:  scheduler_live_pins table ops                                ~120L
├── batch.rs            # T5:  persist_merge_to_db batch inserts                            ~180L
├── assignments.rs      # T6:  assignment CRUD                                              ~80L
├── history.rs          # T7:  build_history EMA + build_samples retention                  ~300L
├── recovery.rs         # T8:  leadership-acquire state reload queries                      ~400L
└── tests/
    ├── mod.rs          # T9:  shared + roundtrip + TERMINAL_STATUSES + partial-index-drift
    ├── tenants.rs      # T9
    ├── builds.rs       # T9
    ├── derivations.rs  # T9
    ├── batch.rs        # T9:  r[verify sched.db.batch-unnest] ×2
    ├── history.rs      # T9:  ema_peak_memory scenarios
    ├── recovery.rs     # T9
    └── transactions.rs # T9:  r[verify sched.db.partial-index-literal] ×2
```

## Dependencies

```json deps
{"deps": [403], "soft_deps": [304, 311, 366, 408, 405, 398], "note": "consol-mc220 (discovered_from=403 via collision count). HARD-dep P0403: rewrites list_builds_keyset (:384 comment delete + :381 integer-remainder to_timestamp + count_builds first-page-only gating) — split without it means P0403 re-locates across db/builds.rs + db/mod.rs. Soft-dep P0304-T77 (adds jwt_jti INSERT column in db.rs — post-split targets db/builds.rs or db/tenants.rs depending on where insert_build lands), T106 (DEFAULT_GC_RETENTION_HOURS const — moves to mod.rs), T139 (:840 is_ca UPDATE idempotent comment — moves to db/batch.rs). Soft-dep P0311-T60 (adds tests — land in db/tests/<domain>.rs post-split). Soft-dep P0366 (cutoff_seconds gauge re-emit — touches db.rs for query; post-split targets db/history.rs or wherever cutoff queries live — grep 'cutoff' at dispatch). Soft-dep P0408 (CA recovery-resolve fetch — touches dispatch.rs not db.rs, but reads drv_path via db query — check if it adds a db.rs method). Soft-dep P0405 (BFS walker dedup — touches completion.rs not db.rs). Soft-dep P0398 (CA resolve inputDrvs always-empty — touches resolve.rs not db.rs). PREFERRED ORDER: P0403 → this split → {P0304-T77/T106/T139, P0311-T60, P0366} target db/<domain>.rs directly. This is a big-diff mechanical refactor; sequence in a quiet window (consolidator-cadence merge, low-concurrent-impls)."}
```

**Depends on:** [P0403](plan-0403-keyset-index-float8-precision.md) — list_builds_keyset rewrite + count_builds gating + `:365` comment delete must land first or the split carries stale code.

**Conflicts with:** `db.rs` is the single hottest file at count=42. Post-split, plan-docs that said `{"path": "rio-scheduler/src/db.rs", "action": "MODIFY"}` need Files-fence updates to cite `db/<domain>.rs`. In-flight plans touching db.rs: [P0304](plan-0304-trivial-batch-p0222-harness.md)-T77/T106/T139, [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T60, [P0366](plan-0366-cutoff-seconds-gauge-re-emit.md). Each is additive (new column, new const, new comment, new test) — non-overlapping at hunk level, but all need `git rebase` path-retarget post-split. Same ship-order as [P0381](plan-0381-scaling-rs-fault-line-split.md)/[P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md)/[P0386](plan-0386-admin-tests-rs-split-submodule-seams.md): split lands, then a `/bump-refs` pass updates the stale `db.rs` references in batch-plan Files fences.
