# Plan 0396: Split workerpool/tests.rs along prod seams (1716L → 5 files)

[`rio-controller/src/reconcilers/workerpool/tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests.rs) has grown to **1716L** (followup said 1541L at filing; [P0365](plan-0365-warn-on-spec-degrades-helper.md)'s event-reason tests added ~175L since). Consolidator-mc185 flagged the same trajectory as `admin/tests.rs` (→ [P0386](plan-0386-admin-tests-rs-split-submodule-seams.md)) and `grpc/tests.rs` (→ [P0395](plan-0395-grpc-tests-rs-split-submodule-seams.md)).

The prod module already has the seams ([`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) / [`disruption.rs`](../../rio-controller/src/reconcilers/workerpool/disruption.rs) / [`ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) / `mod.rs`). Section markers from `grep '^// -----'`:

| Range | Cluster | Tests | Seam |
|---|---|---|---|
| `:1-927` | StatefulSet builder-spec (~30 tests: seccomp, hostUsers, fuse, tls, env, replicas, pdb, bloom-knob) | ~30 | `builders.rs` |
| `:928-1037` | quantity parsing (section `:928`) | ~6 | helper (stays in builders) |
| `:1038-1468` | mock-apiserver integration (reconcile loop, owner-refs, drain) | ~6 | `mod.rs` reconcile |
| `:1469-1716` | coverage-propagation via `figment::Jail` + P0365 event-reason tests (section `:1469`) | ~8 | `mod.rs` + `disruption.rs` |

The followup at row 22 also notes: "sprint-1 is actively editing this file (P0365 event-reason work, P0380 fixture-extraction landed) — dep on settling." Both are DONE per dag (P0365 @ DONE, P0380 @ DONE), so the churn is behind us.

## Entry criteria

- [P0386](plan-0386-admin-tests-rs-split-submodule-seams.md) merged (split pattern precedent)
- [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md) merged (fixtures extracted — tests.rs stable)
- [P0365](plan-0365-warn-on-spec-degrades-helper.md) merged (event-reason tests landed — final churn)

## Tasks

### T1 — `refactor(controller):` workerpool/tests/mod.rs — shared fixtures + annotations

NEW `rio-controller/src/reconcilers/workerpool/tests/mod.rs`. Extract:

- File-top `r[verify]` annotations at `:1-4` (`ctrl.crd.workerpool`, `ctrl.reconcile.owner-refs`, `ctrl.drain.all-then-scale`, `ctrl.drain.sigterm`) — these are file-level and apply across clusters; move to mod.rs OR distribute to the specific test they cover (grep the test bodies to find which test actually exercises each — `ctrl.drain.*` probably belongs to `apply_tests.rs` not `builders_tests.rs`)
- Shared `use` block + `test_workerpool_spec`/`test_wps_spec` fixture calls (if not already in [`fixtures.rs`](../../rio-controller/src/fixtures.rs) per P0380 — they should be, but the shim `use crate::fixtures::*` goes here)
- Module declarations

### T2 — `refactor(controller):` builders_tests.rs — StatefulSet spec coverage

NEW `rio-controller/src/reconcilers/workerpool/tests/builders_tests.rs`. Move from `:36-1037`:

- All `#[test]` fns covering seccomp (`:73-153` × 3 tests, `r[verify worker.seccomp.localhost-profile]`), hostUsers/fuse (`:213-431`, `r[verify sec.pod.fuse-device-plugin]` + `r[verify sec.pod.host-users-false]` + `r[verify ctrl.crd.host-users-network-exclusive]`), pdb (`:550`, `r[verify ctrl.pdb.workers]`), bloom-knob (`:816`, `r[verify ctrl.pool.bloom-knob]`), disruption-target builder (`:464`, `r[verify ctrl.drain.disruption-target]`)
- quantity-parsing tests at `:928-1037`

~1000L. The densest cluster by far — 30+ sync tests of builder output shape. Could be sub-split (`seccomp_tests.rs`, `security_tests.rs`, `sts_tests.rs`) if 1000L is still too large, but keep it to one file unless it exceeds 1200L at dispatch.

### T3 — `refactor(controller):` apply_tests.rs — mock-apiserver reconcile loop

NEW `rio-controller/src/reconcilers/workerpool/tests/apply_tests.rs`. Move from `:1038-1468`:

- `#[tokio::test]` fns using `ApiServerVerifier` (or the mock-apiserver pattern) — reconcile-loop integration tests
- Owner-ref verification, drain orchestration — carries `r[verify ctrl.reconcile.owner-refs]` and `r[verify ctrl.drain.all-then-scale]` if they apply here (decide at dispatch which tests actually exercise these)

~430L.

### T4 — `refactor(controller):` disruption_tests.rs — drain + event-reason coverage

NEW `rio-controller/src/reconcilers/workerpool/tests/disruption_tests.rs`. Move from `:1469-1716`:

- figment::Jail coverage-propagation tests (section `:1469-1475`)
- P0365's `warn_on_spec_degrades` event-reason tests (`:1648`, `:1709` per [P0304](plan-0304-trivial-batch-p0222-harness.md)-T152's assert-site refs)
- `event_post_scenario` helper at `:1081` — if only used by disruption tests, move here; if shared, stays in mod.rs

~250L.

### T5 — `refactor(controller):` delete monolith + wire tests/ mod

DELETE `rio-controller/src/reconcilers/workerpool/tests.rs`. MODIFY `rio-controller/src/reconcilers/workerpool/mod.rs` — `#[cfg(test)] mod tests;` stays (points at `tests/` dir via `tests/mod.rs`).

## Exit criteria

- `wc -l rio-controller/src/reconcilers/workerpool/tests.rs` → file-not-found
- `wc -l rio-controller/src/reconcilers/workerpool/tests/*.rs` → 4-5 files, sum ≈1650-1750L
- `cargo nextest run -p rio-controller reconcilers::workerpool::tests` → all pass, same count as pre-split
- `nix develop -c tracey query rule worker.seccomp.localhost-profile` → shows ≥3 verify sites at `tests/builders_tests.rs`
- `nix develop -c tracey query rule sec.pod.fuse-device-plugin` → shows ≥2 verify sites at `tests/builders_tests.rs`
- `nix develop -c tracey query rule ctrl.pdb.workers` → shows ≥1 verify site at `tests/builders_tests.rs`
- `grep -c 'r\[verify' rio-controller/src/reconcilers/workerpool/tests/` → matches pre-split count (no annotations lost)
- `/nbr .#ci` green

## Tracey

References existing markers (annotations MOVE, not added):
- `r[ctrl.crd.workerpool]` — file-top `:1`, distribute to specific builder test
- `r[ctrl.reconcile.owner-refs]` — file-top `:2` → `apply_tests.rs`
- `r[ctrl.drain.all-then-scale]` + `r[ctrl.drain.sigterm]` — file-top `:3-4` → `apply_tests.rs` or `disruption_tests.rs`
- `r[worker.seccomp.localhost-profile]` × 3 at `:73,97,124` → `builders_tests.rs`
- `r[sec.pod.fuse-device-plugin]` × 3 at `:213,265,384` → `builders_tests.rs`
- `r[sec.pod.host-users-false]` × 4 at `:303,321,350,383` → `builders_tests.rs`
- `r[ctrl.crd.host-users-network-exclusive]` at `:321` → `builders_tests.rs`
- `r[ctrl.drain.disruption-target]` at `:464` → `builders_tests.rs` (or disruption_tests.rs if the test is drain-orchestration not builder-shape; check test body)
- `r[ctrl.pdb.workers]` at `:550` → `builders_tests.rs`
- `r[ctrl.pool.bloom-knob]` at `:811` → `builders_tests.rs`

No new markers. Pure file-reorg; config.styx `include` glob `rio-controller/src/**/*.rs` at `:46` covers new paths.

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "DELETE", "note": "T5: monolith deleted after split"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/mod.rs", "action": "NEW", "note": "T1: shared use + fixtures shim + mod declarations (~80L)"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/builders_tests.rs", "action": "NEW", "note": "T2: StatefulSet spec + seccomp/hostUsers/fuse/pdb/bloom + quantity-parsing (~1000L); r[verify worker.seccomp.*]×3, r[verify sec.pod.*]×7, r[verify ctrl.pdb.workers], r[verify ctrl.pool.bloom-knob]"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/apply_tests.rs", "action": "NEW", "note": "T3: mock-apiserver reconcile loop (~430L); r[verify ctrl.reconcile.owner-refs], r[verify ctrl.drain.*]"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests/disruption_tests.rs", "action": "NEW", "note": "T4: figment::Jail coverage-prop + P0365 event-reason tests (~250L)"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T5: #[cfg(test)] mod tests; → dir (likely no-op)"}
]
```

```
rio-controller/src/reconcilers/workerpool/
├── mod.rs                 # T5: tests mod wire
└── tests/
    ├── mod.rs             # T1: shared
    ├── builders_tests.rs  # T2: ~1000L
    ├── apply_tests.rs     # T3: ~430L
    └── disruption_tests.rs # T4: ~250L
```

## Dependencies

```json deps
{"deps": [386, 380, 365], "soft_deps": [304, 311, 0395, 381], "note": "discovered_from=386 (rev-p386 trivial — consol-mc185 flagged same trajectory). P0386 (DONE) establishes the split pattern. P0380 (DONE) extracted WorkerPoolSpec test fixtures — tests.rs is now stable enough to split (no more E0063-magnet literals in the file). P0365 (DONE) added event-reason tests at :1648+:1709 — the last churn before this split. Soft-dep P0304-T99/T152 (POOL_LABEL const-hoist + event-reason REASON_* const — T99 touches tests.rs:627 selector.get, T152 touches :1648/:1709 asserts; both move with T2/T4; coordinate at dispatch, or sequence P0304 FIRST so T2/T4 carry the fixes). Soft-dep P0311-T6/T7 (cel_rules_in_schema asserts + Unconfined test — T6 at workerpool.rs:516 not tests.rs, T7 at builders.rs not tests.rs; unaffected by this split). Soft-dep P0395 (grpc/tests.rs split — sibling plan, pattern-sharing, no file overlap). Soft-dep P0381 (scaling.rs split — different module, non-overlapping). workerpool/tests.rs count unknown but inherently high (it's the monolith). This split REDUCES future collision surface."}
```

**Depends on:** [P0386](plan-0386-admin-tests-rs-split-submodule-seams.md) — pattern. [P0380](plan-0380-controller-workerpoolspec-test-fixture-dedup.md) — stable fixtures. [P0365](plan-0365-warn-on-spec-degrades-helper.md) — last churn.

**Conflicts with:** Any plan adding tests to [`workerpool/tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests.rs). After this split, new tests target `tests/{domain}_tests.rs`. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T99/T152 touch specific assert lines; sequence those BEFORE this split or carry them forward in T2/T4.
