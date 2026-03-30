# Plan 990719402: manifest-mode reconcile loop — VM test subtest

`reconcile_manifest` (586L net-of-comments, [P0503](plan-0503-manifest-diff-reconciler.md)) has **zero VM test coverage**. [`manifest_tests.rs:7`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs) claims "the reconcile-loop wiring (`reconcile_manifest` I/O) is what VM tests cover" — this is FALSE. `nix/tests/default.nix` carries no `r[verify ctrl.pool.manifest-reconcile]`; `tracey query rule ctrl.pool.manifest-reconcile` confirms all four verify refs live at `manifest_tests.rs:{84,126,151,534}` — pure-function tests against `compute_spawn_plan`, `group_by_bucket`, `build_manifest_job`.

The contrast is stark: `ctrl.pool.ephemeral` has a ~180s subtest at [`default.nix:593-605`](../../nix/tests/default.nix) ([`lifecycle.nix:1973`](../../nix/tests/scenarios/lifecycle.nix)) that applies a real `WorkerPoolSpec.ephemeral=true`, submits two builds, asserts Job-per-build + pod reaping via `kubectl`. Manifest mode has NOTHING equivalent despite sharing the same Job-creation surface and adding per-bucket diff complexity on top.

Dark paths the bughunter identified (mc=56):
- [`manifest.rs:269→302`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) — `jobs_api.create` non-409 error → early return before `status_patch`. Status drifts from reality; operator sees stale `replicas` in `kubectl get wp`.
- [`:245,267`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) — 409 Conflict decrements spawn budget without spawning. Under repeated name collision, budget goes to zero with nothing created.
- [`:171`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) — `GetCapacityManifest` RPC fail → fall back to cold-start floor. Intended degrade path; never exercised against a real k3s apiserver.

Diff logic IS well-covered (18 unit tests, idempotency proven). The K8s I/O wiring is not.

This plan adds a `manifest-pool` subtest to `vm-lifecycle-autoscale-k3s` parallel to `ephemeral-pool`, AND fixes the false comment. P0505-P0510 do not schedule this; bughunter finding surfaced at mc=56.

## Entry criteria

- [P0503](plan-0503-manifest-diff-reconciler.md) merged (`reconcile_manifest` exists) — DONE
- [P0505](plan-0505-manifest-scaledown-grace.md) merged (scale-down path exists; subtest can assert delete-on-idle) — DONE

## Tasks

### T1 — `test(controller):` manifest-pool VM subtest in lifecycle.nix

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix). Add a `manifest-pool` fragment after `ephemeral-pool` (`:1973`+). Same structure: apply a `BuilderPool` with `spec.sizing: Manifest`, wait for `reconcile_manifest` to tick, assert via `kubectl`.

Minimal assertion chain (target ~150-200s — manifest pods are long-lived so no per-build pod churn to wait on):

1. `kubectl apply` a BuilderPool with `sizing: Manifest`, `replicas.max: 3`. Scheduler must already have ≥1 derivation with `est_memory_bytes` set (the `k3sFull` fixture seeds `build_history` via its init path — verify or seed explicitly).
2. Wait for ≥1 Job with `rio.build/sizing=manifest` label. Asserts `jobs_api.create` succeeded (not a dark path).
3. `kubectl get builderpool X -o jsonpath='{.status.replicas}'` → non-zero. Asserts `status_patch` ran (rules out the `:269→302` early-return dark path).
4. Assert the Job carries `rio.build/memory-class` + `rio.build/cpu-class` labels (`r[verify ctrl.pool.manifest-labels]`).
5. Assert the Job's pod has NO `RIO_EPHEMERAL` env (`r[verify ctrl.pool.manifest-long-lived]`).
6. Delete the BuilderPool → ownerRef cascade GCs the Jobs.

Skip scale-down assertion initially — `SCALE_DOWN_WINDOW` is 600s (tunable via `controller.extraEnv` like `vm-lifecycle-autoscale-k3s` does at [`default.nix:177`](../../nix/tests/default.nix), but even at 10s the idle-check requires the pod to heartbeat + report `running_builds=0`, which needs the full `ListExecutors` round-trip — budget it as a follow-on if the base subtest times in under 150s).

### T2 — `test(controller):` wire manifest-pool into default.nix subtests list

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix). Add `"manifest-pool"` to `vm-lifecycle-autoscale-k3s` subtests at `:588`+. Place `r[verify ...]` markers AT the subtests entry, NOT in the scenario header (CLAUDE.md VM-test placement rule):

```nix
subtests = [
  # r[verify obs.metric.controller]
  "autoscaler"
  # r[verify ctrl.autoscale.skip-deleting]
  "finalizer"
  # r[verify ctrl.pool.ephemeral]
  # r[verify ctrl.pool.ephemeral-single-build]
  # r[verify ctrl.pool.ephemeral-deadline]
  "ephemeral-pool"
  # r[verify ctrl.pool.manifest-reconcile]
  # r[verify ctrl.pool.manifest-labels]
  # r[verify ctrl.pool.manifest-long-lived]
  # After ephemeral-pool: clean slate again. Manifest-mode pod is
  # long-lived (no RIO_EPHEMERAL), so ONE Job spawns and persists.
  # ~150s: reconcile tick + Job schedule + pod start + heartbeat +
  # status_patch assertion + delete cascade.
  "manifest-pool"
];
# autoscaler ~238s + finalizer 300s + ephemeral ~180s + manifest ~150s.
globalTimeout = 1600;
```

Bump `globalTimeout` from 1400 to ~1600 (budget 150s + margin).

### T3 — `docs(controller):` fix manifest_tests.rs:7 false claim

MODIFY [`rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs). Replace the false claim at `:7-8`:

```rust
//! `build_manifest_job` spec shape. No K8s apiserver interaction —
//! the reconcile-loop wiring (`reconcile_manifest` I/O) is what
//! VM tests cover.
```

with

```rust
//! `build_manifest_job` spec shape. No K8s apiserver interaction —
//! reconcile-loop wiring (`reconcile_manifest` I/O against a real
//! apiserver) is covered by the `manifest-pool` subtest at
//! `nix/tests/scenarios/lifecycle.nix` (wired via default.nix
//! vm-lifecycle-autoscale-k3s).
```

The old comment was aspirational — now it points at something that exists.

## Exit criteria

- `nix build .#checks.x86_64-linux.vm-lifecycle-autoscale-k3s` passes with `manifest-pool` in the subtest chain
- `grep 'r\[verify ctrl.pool.manifest-reconcile\]' nix/tests/default.nix` → ≥1 hit (was 0)
- `grep 'r\[verify ctrl.pool.manifest-labels\]' nix/tests/default.nix` → ≥1 hit
- `grep 'r\[verify ctrl.pool.manifest-long-lived\]' nix/tests/default.nix` → ≥1 hit
- `nix develop -c tracey query rule ctrl.pool.manifest-reconcile` shows ≥1 verify site in `nix/tests/default.nix`
- `grep 'FALSE\|is what VM tests cover' rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs` → 0 hits; comment names the real subtest
- `globalTimeout` in `vm-lifecycle-autoscale-k3s` ≥ 1550

## Tracey

References existing markers (all exist at [`controller.md:137-158`](../../docs/src/components/controller.md)):
- `r[ctrl.pool.manifest-reconcile]` — T1+T2 add the first VM-level `r[verify]`. Four unit-test verifies exist; this is the first end-to-end.
- `r[ctrl.pool.manifest-labels]` — T1 step 4 asserts label presence on real Jobs. Unit verifies at `manifest_tests.rs:{346,481}` cover `bucket_labels` round-trip; this covers apiserver persistence.
- `r[ctrl.pool.manifest-long-lived]` — T1 step 5 asserts no `RIO_EPHEMERAL` on the spawned pod. Unit verifies at `manifest_tests.rs:{636,652}` check the Job spec; this checks the running pod.

No new markers — this plan fills a coverage gap for existing spec.

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1: manifest-pool fragment after :1973 ephemeral-pool — kubectl apply BuilderPool sizing=Manifest, assert Job labels + no RIO_EPHEMERAL + status.replicas"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: manifest-pool subtest wire at :605+ with r[verify ctrl.pool.manifest-{reconcile,labels,long-lived}] markers; globalTimeout 1400→1600"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T3: :7-8 false 'VM tests cover' claim → name the real subtest"}
]
```

```
nix/tests/
├── scenarios/lifecycle.nix   # T1: manifest-pool fragment
└── default.nix               # T2: subtest wire + r[verify] markers
rio-controller/src/reconcilers/builderpool/tests/
└── manifest_tests.rs         # T3: :7 comment fix
```

## Dependencies

```json deps
{"deps": [503, 505], "soft_deps": [990719401], "note": "P0503 (reconcile_manifest exists) + P0505 (scale-down exists, though T1 skips asserting it for time budget). Both DONE. Soft-dep P990719401: once the Failed-Job sweep lands, the subtest should assert it (inject a failing pod, check sweep) — follow-on, not this plan."}
```

**Depends on:** [P0503](plan-0503-manifest-diff-reconciler.md) (DONE) — `reconcile_manifest` is the system-under-test. [P0505](plan-0505-manifest-scaledown-grace.md) (DONE) — scale-down path exists; T1 doesn't assert it (600s window) but the code is there.

**Conflicts with:** `nix/tests/default.nix` count=29 (hot file). T2 is an additive subtest append at `:605`; adjacent plans touching `vm-lifecycle-autoscale-k3s` would conflict — grep at dispatch. `lifecycle.nix` is large (2300+ lines) but the `ephemeral-pool` block at `:1973-2283` is a clean insertion boundary (manifest-pool goes immediately after).

[P990719401](plan-990719401-manifest-failed-job-sweep.md) touches `manifest_tests.rs` (T3 adds tests at file-end); this plan's T3 edits `:7` header — non-overlapping. Both MODIFY `manifest.rs` indirectly via its test file, but neither touches `manifest.rs` itself.

## Risks

- **Time budget:** `vm-lifecycle-autoscale-k3s` is already at 718s observed (`:607` comment says `~238+300+180`). Adding 150s puts it at ~870s with `globalTimeout=1600`. The test is already one of the longest; if it tips over CI timing flake thresholds, extract `manifest-pool` + `ephemeral-pool` into a separate `vm-lifecycle-jobmode-k3s` test instead. Check `known-flakes.jsonl` for existing `vm-lifecycle-autoscale-k3s` entries at dispatch.
- **Manifest seeding:** the subtest needs `GetCapacityManifest` to return a non-empty manifest for `compute_spawn_plan` to have anything to do. The `k3sFull` fixture may not seed `build_history` by default — verify. If not, either (a) submit a build FIRST (populates history) then wait for the manifest refresh, or (b) seed `build_history` directly via psql in the subtest prelude. Option (a) is more realistic but adds ~60s.
- **Comment fix without test is a half-measure:** if the VM subtest proves too expensive (time budget OR manifest-seeding complexity), T3 alone is still correct — but the comment should then say "unit-test only; VM coverage deferred to P0NNN" with a real plan number, not perpetuate the current lie.
