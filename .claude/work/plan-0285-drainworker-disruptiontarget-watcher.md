# Plan 0285: DrainWorker DisruptionTarget watcher — make the 4 lying comments TRUE

**Retro P0129 finding.** `DrainWorker{force:true}` at [`rio-scheduler/src/actor/worker.rs:211-258`](../../rio-scheduler/src/actor/worker.rs) — the preemption hook — has **zero production callers**. Both `DrainWorkerRequest` construction sites set `force: false` ([`rio-controller/src/reconcilers/workerpool/mod.rs:336`](../../rio-controller/src/reconcilers/workerpool/mod.rs), [`rio-worker/src/main.rs:694`](../../rio-worker/src/main.rs)). No `DisruptionTarget` watcher exists (`grep -rn DisruptionTarget rio-controller/` = 0). No `preStop` hook in `builders.rs`.

**Four comment sites lie in present tense.** They assert wiring that doesn't exist:
- [`rio-scheduler/src/actor/worker.rs:219-225`](../../rio-scheduler/src/actor/worker.rs) — "when the controller sees DisruptionTarget condition on a pod, it calls DrainWorker(force=true)"
- [`rio-scheduler/src/actor/tests/worker.rs:342-344`](../../rio-scheduler/src/actor/tests/worker.rs) — "controller sees DisruptionTarget on a pod, calls this"
- [`rio-controller/src/reconcilers/workerpool/builders.rs:162-165`](../../rio-controller/src/reconcilers/workerpool/builders.rs) — PDB docstring: "builds in flight on the evicting pod get reassigned (DrainWorker force)"
- [`rio-controller/src/reconcilers/workerpool/mod.rs:148-151`](../../rio-controller/src/reconcilers/workerpool/mod.rs) — "evicting pod's builds get reassigned (via DrainWorker force → preemption)"

**Runtime impact:** on node drain / spot-interrupt / Karpenter consolidation, evicted worker pods self-drain with `force=false` and wait up to 2h (`termination_grace_period_seconds.unwrap_or(7200)` at [`builders.rs:433`](../../rio-controller/src/reconcilers/workerpool/builders.rs)). Builds that could be reassigned in seconds instead burn up to 2h of wall-clock before SIGKILL loses them anyway.

**User decision:** Pod watcher on DisruptionTarget — **make the comments TRUE**, not rewrite them. The watcher alternative beats the preStop-exec-hook alternative: preStop fires on EVERY termination (including graceful scale-down where `force=false` is correct); DisruptionTarget fires only on eviction-budget-mediated disruption (node drain, spot interrupt) where preemption IS the right call.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(controller):` Pod watcher filtered to DisruptionTarget=True on workerpool-owned pods

NEW [`rio-controller/src/reconcilers/workerpool/disruption.rs`](../../rio-controller/src/reconcilers/workerpool/disruption.rs) — ~150 LoC.

```rust
//! DisruptionTarget watcher: pre-empt builds on evicting workers.
//!
//! K8s sets status.conditions[type=DisruptionTarget, status=True]
//! when eviction is imminent (node drain, spot interrupt, PDB-mediated).
//! We fire DrainWorker{force:true} → scheduler iterates running_builds,
//! sends CancelSignal per build → worker cgroup.kill()s → builds reassign
//! in seconds instead of burning the 2h terminationGracePeriodSeconds.
// r[impl ctrl.drain.disruption-target]

use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::{watcher, WatchStreamExt};
use kube::{Api, Client};

pub async fn run_disruption_watcher(
    client: Client,
    ns: &str,
    admin: AdminServiceClient<Channel>,
) {
    let pods: Api<Pod> = Api::namespaced(client, ns);
    // Label selector: only pods owned by a WorkerPool STS.
    // Same label builders.rs stamps on every pod.
    let cfg = watcher::Config::default()
        .labels("rio.build/pool");

    let mut stream = watcher(pods, cfg).applied_objects().boxed();
    while let Some(pod) = stream.next().await {
        let Ok(pod) = pod else { continue };
        // DisruptionTarget status=True? K8s sets this BEFORE SIGTERM
        // lands — that's the window we act in.
        let disrupted = pod.status.as_ref()
            .and_then(|s| s.conditions.as_ref())
            .map(|cs| cs.iter().any(|c|
                c.type_ == "DisruptionTarget" && c.status == "True"
            ))
            .unwrap_or(false);
        if !disrupted { continue; }

        // worker_id = pod name (STS gives stable names; heartbeat
        // registers with pod name — see runtime.rs).
        let worker_id = pod.metadata.name.as_deref().unwrap_or_default();
        // Best-effort. Force=true triggers the preemption block
        // at actor/worker.rs:211-258.
        let _ = admin.clone()
            .drain_worker(DrainWorkerRequest {
                worker_id: worker_id.to_string(),
                force: true,
            })
            .await;
    }
}
```

**Idempotence:** DisruptionTarget stays True for the pod's remaining lifetime. Repeated watcher events fire repeated `DrainWorker{force:true}` calls — this is fine, [`actor/worker.rs:169`](../../rio-scheduler/src/actor/worker.rs) handles `force=true` with `draining` already set (re-preempts, which is a no-op on an already-empty `running_builds`).

**Not the finalizer path.** The existing `cleanup()` at [`workerpool/mod.rs:336`](../../rio-controller/src/reconcilers/workerpool/mod.rs) stays `force=false` — WorkerPool deletion is graceful, the operator has time to wait.

### T2 — `feat(controller):` spawn watcher from main.rs

MODIFY [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) — `tokio::spawn` the watcher alongside the existing `Controller::run` futures. Reuse the existing `AdminServiceClient` from `Ctx`. Joins the same `tokio::join!`.

### T3 — `test(controller):` unit test — DisruptionTarget pod → force=true admin call

NEW test in [`rio-controller/src/reconcilers/workerpool/tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests.rs) — mock `AdminServiceClient` (same pattern as existing `test_drain_on_cleanup`), feed a Pod with `DisruptionTarget=True` through the watcher's inner filter logic, assert `force==true` in the captured request. Annotate `// r[verify ctrl.drain.disruption-target]`.

### T4 — `test:` VM fragment — node drain → `force=true` logged

NEW fragment `disruption-drain` in [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix):

```python
# r[verify ctrl.drain.disruption-target]
# (at col-0 file-header comment block per tracey .nix parser constraint)
with subtest("disruption-drain: kubectl drain → DrainWorker force=true"):
    # Start a long build (sleepSecs=120) so it's mid-flight when drain hits.
    # kubectl drain sets DisruptionTarget on the pod → watcher fires.
    k3s_server.succeed("k3s kubectl drain k3s-agent --ignore-daemonsets --delete-emptydir-data --timeout=60s &")
    # Scheduler logs "DrainWorker force=true" at INFO. 30s budget — watcher
    # has to observe the condition, fire the RPC.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} logs deploy/rio-scheduler --since=60s | grep -q 'force.*true'",
        timeout=30,
    )
    # Undo: uncordon so the rest of lifecycle.nix can use the node.
    k3s_server.succeed("k3s kubectl uncordon k3s-agent")
```

### T5 — `docs:` the 4 comments are now TRUE — spot-check + date them

No rewrites. Append a single dated line after each of the 4 comment blocks: `// (wired: P0285 disruption.rs watcher)`. A future grep for "DisruptionTarget" finds the impl; the dated marker gives the reader a traceability hook.

## Exit criteria

- `/nbr .#ci` green
- `grep -rn DisruptionTarget rio-controller/src/` → ≥3 hits (watcher + test + mod decl)
- The `if force` block at `actor/worker.rs:211-258` has a production caller (grep `force: true` in `rio-controller/` → ≥1 non-test hit)
- `tracey query rule ctrl.drain.disruption-target` shows both `impl` and `verify`

## Tracey

References existing markers:
- `r[ctrl.drain.sigterm]` — T2 wires alongside this existing path
- `r[ctrl.drain.all-then-scale]` — UNCHANGED (finalizer path stays `force=false`)
- `r[ctrl.pdb.workers]` — T5 validates the PDB docstring claim

Adds new markers to component specs:
- `r[ctrl.drain.disruption-target]` → `docs/src/components/controller.md` (see ## Spec additions below)

## Spec additions

New paragraph in [`docs/src/components/controller.md`](../../docs/src/components/controller.md), inserted after `r[ctrl.drain.sigterm]` (after the "A preStop hook doing the same is redundant" paragraph, before `r[ctrl.autoscale.direct-patch]`):

```markdown
r[ctrl.drain.disruption-target]

**Eviction-triggered preemption:** the controller runs a Pod watcher
filtered to `rio.build/pool`-labeled pods with `status.conditions[type=DisruptionTarget,status=True]`.
When K8s marks a pod for eviction (node drain, spot interrupt, PDB-mediated disruption),
the watcher calls `AdminService.DrainWorker{force:true}` — the scheduler iterates
`running_builds`, sends `CancelSignal` per build → worker `cgroup.kill()`s → builds
reassign to healthy workers within seconds. Without this, the evicting pod would
self-drain with `force=false` and wait up to `terminationGracePeriodSeconds` (2h)
for in-flight builds to complete naturally before SIGKILL loses them anyway.
The SIGTERM self-drain path (above) is the fallback if the watcher misses the window.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/disruption.rs", "action": "NEW", "note": "T1: Pod watcher, DisruptionTarget → DrainWorker{force:true}"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T1: pub mod disruption; T5: (wired: P0285) annotation after :148-151"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T2: spawn disruption watcher in tokio::join!"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "T3: unit test DisruptionTarget → force=true captured"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T5: (wired: P0285) annotation after PDB docstring :162-165"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T5: (wired: P0285) annotation after :219-225"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "T5: (wired: P0285) annotation after :342-344"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T4: disruption-drain VM fragment"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "Spec addition: r[ctrl.drain.disruption-target] marker after r[ctrl.drain.sigterm]"}
]
```

```
rio-controller/src/reconcilers/workerpool/
├── disruption.rs                 # T1: NEW — Pod watcher
├── mod.rs                        # T1: pub mod; T5: annotation
├── builders.rs                   # T5: annotation
└── tests.rs                      # T3: unit test
rio-controller/src/main.rs        # T2: spawn watcher
rio-scheduler/src/actor/
├── worker.rs                     # T5: annotation
└── tests/worker.rs               # T5: annotation
nix/tests/scenarios/lifecycle.nix # T4: disruption-drain fragment
docs/src/components/controller.md # spec addition
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0129 — discovered_from=129. force=true has ZERO prod callers; 4 comments lie in present tense. Pod watcher (not preStop): DisruptionTarget only fires on eviction-budget disruption, preStop fires on EVERY termination incl. graceful. builders.rs:433 grace=7200s means builds burn 2h then SIGKILL loses them anyway."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Conflicts with:** [`rio-controller/src/main.rs`](../../rio-controller/src/main.rs) is touched by [P0294](plan-0294-build-crd-full-rip.md) (unwires Build reconciler from same `tokio::join!`). Advisory-serial: P0294 first (larger diff), this plan's spawn slots into the trimmed main.rs cleanly. [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) also touched by P0289 + P0294 — all three EOF-append fragments, low conflict.
