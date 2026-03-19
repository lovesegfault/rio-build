# Plan 0296: Ephemeral builders opt-in — Job-per-assignment security mode

**USER FEATURE REQUEST.** "Crucial feature for security that users need to be able to opt into."

**The shape:** `WorkerPoolSpec.ephemeral: bool` (default `false`). When `true`:
- Controller deploys **Job-per-assignment**, not StatefulSet
- Worker runtime exits after one build (no event loop)
- Scheduler treats ephemeral-pool workers as single-use: assign → wait for completion → worker deregisters → pod terminates → Job reaps
- No FUSE cache reuse (fresh pod = fresh cache)
- No bloom accumulation (obviates [P0288](plan-0288-bloom-fill-ratio-gauge.md) for ephemeral pools; STS pools still need the gauge)

**Security value:** zero cross-build contamination. Untrusted tenants can't leave behind poisoned cache entries for the next build. Together with [P0286](plan-0286-privileged-hardening-device-plugin.md) (device plugin + hostUsers:false + privileged-false), this is the multi-tenant security story.

**Tradeoffs:**
- Cold-start cost per build (container pull, FUSE mount, scheduler registration — ~10-30s typical)
- No locality (`W_LOCALITY=0.7` at [`assignment.rs:153`](../../rio-scheduler/src/actor/assignment.rs) becomes meaningless — every worker is fresh, `count_missing()` always returns full closure)
- Pod churn (k8s API load; Karpenter might thrash if not tuned)

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(controller):` WorkerPoolSpec.ephemeral field + CEL validation

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — add field to `WorkerPoolSpec` at `:45`:

```rust
/// Ephemeral mode: one pod per build assignment. Default false
/// (StatefulSet, long-lived workers with locality). When true:
/// controller spawns a K8s Job per dispatch, worker exits after one
/// build, pod terminates, Job reaps. Zero cross-build contamination
/// (fresh FUSE cache, fresh filesystem). Tradeoffs: cold-start per
/// build (~10-30s), no locality (W_LOCALITY meaningless), pod churn.
///
/// CEL: ephemeral=true requires replicas.min==0 (the "pool size" is
/// Job parallelism per-dispatch, not a standing set).
#[serde(default)]
pub ephemeral: bool,
```

CEL validation on the struct (same spot as the existing `replicas.min <= replicas.max` constraint): `ephemeral` → `replicas.min == 0 && replicas.max > 0` (max becomes the concurrent-Job ceiling).

Regenerate CRD yaml: `cargo run --bin crd-gen > infra/helm/crds/workerpools.rio.build.yaml` (or whatever the project's CRD-gen pattern is).

### T2 — `feat(controller):` Job-mode branch in reconciler

MODIFY [`rio-controller/src/reconcilers/workerpool/mod.rs`](../../rio-controller/src/reconcilers/workerpool/mod.rs) — at the point where STS is built/applied, branch on `wp.spec.ephemeral`:

```rust
if wp.spec.ephemeral {
    // r[impl ctrl.pool.ephemeral]
    // Ephemeral mode: NO standing StatefulSet. Instead, the scheduler
    // dispatches via EphemeralDispatch RPC → controller creates a
    // Job. Reconciler maintains the Job template (PodDisruptionBudget
    // is meaningless; no headless Service). Status reflects active
    // Job count instead of STS replicas.
    reconcile_ephemeral(&wp, &ctx).await?;
    return Ok(Action::requeue(Duration::from_secs(30)));
}
// ... existing STS path
```

NEW `reconcile_ephemeral()` fn: manages a ConfigMap with the pod template (same container spec `build_pod_spec` produces, minus the STS-specific bits). The Job creation itself is driven by scheduler dispatch (T4), not the reconcile loop — the reconciler's role is to keep the template current and garbage-collect completed Jobs.

NEW [`rio-controller/src/reconcilers/workerpool/ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs):

```rust
// r[impl ctrl.pool.ephemeral]
/// Create a K8s Job for a single build assignment.
/// Called by the scheduler via CreateEphemeralWorker admin RPC (T4).
pub async fn spawn_ephemeral_job(
    client: &kube::Client,
    ns: &str,
    pool: &str,
    assignment_id: &str,
) -> Result<String> {
    // Job name: {pool}-{assignment_id_prefix} (k8s names ≤63 chars).
    // restartPolicy: Never (fail → scheduler reassigns, not K8s).
    // ttlSecondsAfterFinished: 60 (cleanup; scheduler already observed
    //   completion via worker's final heartbeat).
    // backoffLimit: 0 (no K8s retry; scheduler owns retry).
    // The pod gets RIO_EPHEMERAL=1 env — worker runtime checks this
    // to enter single-shot mode (T3).
}
```

### T3 — `feat(worker):` single-shot runtime mode

MODIFY [`rio-worker/src/runtime.rs`](../../rio-worker/src/runtime.rs) — the event loop currently does `loop { recv assignment → spawn → continue }`. Add an early-exit branch:

```rust
// r[impl ctrl.pool.ephemeral]
// Ephemeral mode: exit after one build. Pod terminates, Job reaps.
// Set via RIO_EPHEMERAL env (controller's spawn_ephemeral_job sets it).
// FUSE mount + scheduler registration still happen (per-build cost is
// the tradeoff); the difference is no second iteration.
let ephemeral = std::env::var("RIO_EPHEMERAL").is_ok();
// ... loop body ...
if ephemeral {
    // Sent final ReportCompletion. Deregister explicitly so scheduler
    // doesn't wait for heartbeat-miss timeout.
    let _ = admin.deregister_worker(...).await;
    break;  // main() returns → Drop runs → FUSE unmounts → exit 0
}
```

MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) — skip the `acquire_many(max_builds)` drain-wait if ephemeral (only one permit was ever taken, it's already released).

### T4 — `feat(proto):` CreateEphemeralWorker admin RPC + scheduler wiring

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto):

```protobuf
// Scheduler → Controller. Scheduler has a ready-to-dispatch derivation
// for an ephemeral pool; controller spawns a Job, returns the Job name.
// Scheduler holds dispatch until the ephemeral worker heartbeats in.
rpc CreateEphemeralWorker(CreateEphemeralWorkerRequest) returns (CreateEphemeralWorkerResponse);
```

MODIFY [`rio-scheduler/src/actor/assignment.rs`](../../rio-scheduler/src/actor/assignment.rs) — at dispatch time, if the chosen pool is ephemeral (pool metadata carries the flag, populated from `WorkerPool.spec.ephemeral` via the controller's `ClusterStatus` feed), call `CreateEphemeralWorker` instead of picking from the standing set:

```rust
if pool.ephemeral {
    // No standing workers. Ask controller to spawn one. Hold the
    // derivation in Pending-Dispatched until the new pod heartbeats.
    // Timeout: if no heartbeat within 120s, treat as infra-failure
    // (Job scheduling problem, image pull failure).
    //
    // W_LOCALITY is 0 for all ephemeral candidates (fresh cache).
    // Scoring reduces to pure W_LOAD, which for ephemeral means
    // "are we under the replicas.max concurrent-Job ceiling?"
}
```

### T5 — `test:` VM fragment — ephemeral pool → Job created → build completes → pod terminates

NEW fragment in [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix):

```python
# r[verify ctrl.pool.ephemeral]
# (at col-0 file-header comment block)
ephemeral-pool = ''
  with subtest("ephemeral-pool: build → Job spawned → pod gone after completion"):
      # Apply an ephemeral WorkerPool.
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: WorkerPool\n"
          "metadata: {name: ephemeral-pool, namespace: ${ns}}\n"
          "spec:\n"
          "  ephemeral: true\n"
          "  replicas: {min: 0, max: 4}\n"
          "  # ... rest of spec\n"
          "EOF"
      )
      # No STS should exist for this pool.
      k3s_server.fail(
          "k3s kubectl -n ${ns} get sts ephemeral-pool-workers"
      )

      # Submit a build routed to ephemeral-pool.
      client.succeed("nix build --store ssh-ng://k3s-server ${ephemeralDrv}")

      # A Job was created.
      jobs = k3s_server.succeed(
          "k3s kubectl -n ${ns} get jobs -l rio.build/pool=ephemeral-pool -o name"
      ).strip()
      assert jobs, "no ephemeral Job created"

      # Build completed (nix build succeeded above). Pod should be
      # gone (ttlSecondsAfterFinished or already Succeeded → reaped).
      k3s_server.wait_until_succeeds(
          "test -z \"$(k3s kubectl -n ${ns} get pods "
          "-l rio.build/pool=ephemeral-pool "
          "--field-selector status.phase!=Succeeded -o name)\"",
          timeout=120,
      )

      # Second build → NEW Job (not reusing the terminated pod).
      client.succeed("nix build --store ssh-ng://k3s-server ${ephemeralDrv2}")
      jobs2 = k3s_server.succeed(
          "k3s kubectl -n ${ns} get jobs -l rio.build/pool=ephemeral-pool -o name | wc -l"
      ).strip()
      assert int(jobs2) >= 2, f"expected ≥2 Jobs, got {jobs2}"
'';
```

### T6 — `docs:` tradeoff prose + operator guidance

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) — new section after the WorkerPool YAML example (after `:96` where the YAML ends).

MODIFY [`docs/src/security.md`](../../docs/src/security.md) — add a "## Ephemeral Builders" section pointing at `r[ctrl.pool.ephemeral]`, positioned as the strongest isolation option when combined with P0286's hardening.

## Exit criteria

- `/nbr .#ci` green — includes the ephemeral-pool VM fragment
- `grep ephemeral rio-controller/src/crds/workerpool.rs` → ≥1 hit (the field)
- `grep RIO_EPHEMERAL rio-worker/src/runtime.rs` → ≥1 hit (single-shot check)
- `grep CreateEphemeralWorker rio-proto/proto/admin.proto` → 1 hit
- `tracey query rule ctrl.pool.ephemeral` shows both `impl` and `verify`
- VM fragment proves: no STS, Job created, pod terminated after build, second build = new Job

## Tracey

References existing markers:
- `r[ctrl.crd.workerpool]` — T1 adds the `ephemeral` field to the same spec struct
- `r[ctrl.drain.sigterm]` — T3's single-shot exit bypasses the drain-wait (meaningless for one-build lifecycle)

Adds new markers to component specs:
- `r[ctrl.pool.ephemeral]` → `docs/src/components/controller.md` (see ## Spec additions below)

## Spec additions

New section in [`docs/src/components/controller.md`](../../docs/src/components/controller.md), inserted after the `r[ctrl.crd.workerpool]` YAML block (before `### WorkerPoolSet`):

```markdown
### Ephemeral WorkerPools

r[ctrl.pool.ephemeral]

When `WorkerPoolSpec.ephemeral: true`, the controller does NOT create a
StatefulSet. Instead, the scheduler requests per-assignment Jobs via
`AdminService.CreateEphemeralWorker`: scheduler has a ready-to-dispatch
derivation → controller creates a Job → pod starts, heartbeats, receives
the one assignment → completes → exits → pod terminates → Job reaps
(`ttlSecondsAfterFinished: 60`). Worker runtime detects `RIO_EPHEMERAL`
env and exits after one `ReportCompletion` (no event-loop second iteration).

**Isolation guarantee:** zero cross-build state. Fresh FUSE cache, fresh
overlayfs upper, fresh filesystem. Untrusted tenants cannot leave poisoned
cache entries for subsequent builds. Strongest isolation when combined
with `hostUsers: false` + non-privileged (P0286).

**Cost:** per-build cold start (container pull, FUSE mount, scheduler
registration — typically 10–30s). `W_LOCALITY` is meaningless (every
worker has an empty cache); scheduler scoring reduces to `W_LOAD` which
becomes "under the `replicas.max` concurrent-Job ceiling?". Pod churn may
require Karpenter tuning (consolidation policy). Not recommended for
high-throughput trusted-tenant workloads; intended for untrusted multi-tenant
where isolation > throughput.
```

## Files

```json files
[
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "T1: ephemeral bool field + CEL validation"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T2: branch on ephemeral — no STS path"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "NEW", "note": "T2: spawn_ephemeral_job + reconcile_ephemeral"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T2: build_job_spec helper (reuses build_pod_spec container)"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T3: RIO_EPHEMERAL check → exit after one build"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T3: skip acquire_many drain-wait if ephemeral"},
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T4: CreateEphemeralWorker RPC"},
  {"path": "rio-scheduler/src/actor/assignment.rs", "action": "MODIFY", "note": "T4: ephemeral pool → CreateEphemeralWorker instead of pick-from-standing"},
  {"path": "rio-controller/src/grpc.rs", "action": "MODIFY", "note": "T4: controller serves CreateEphemeralWorker (wraps spawn_ephemeral_job)"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T5: ephemeral-pool VM fragment"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "MODIFY", "note": "T1: regenerate with ephemeral field"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T6 + Spec addition: r[ctrl.pool.ephemeral] section"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T6: Ephemeral Builders section"}
]
```

```
rio-controller/src/
├── crds/workerpool.rs                # T1: ephemeral field
├── reconcilers/workerpool/
│   ├── mod.rs                        # T2: branch
│   ├── ephemeral.rs                  # T2: NEW — Job spawn + reconcile
│   └── builders.rs                   # T2: build_job_spec helper
└── grpc.rs                           # T4: serve CreateEphemeralWorker
rio-worker/src/
├── runtime.rs                        # T3: single-shot exit
└── main.rs                           # T3: skip drain-wait
rio-proto/proto/admin.proto           # T4: RPC
rio-scheduler/src/actor/assignment.rs # T4: dispatch path
nix/tests/scenarios/lifecycle.nix     # T5: VM fragment
infra/helm/crds/workerpools.rio.build.yaml  # T1: regen
docs/src/
├── components/controller.md          # T6 + spec addition
└── security.md                       # T6
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [286], "note": "USER FEATURE REQUEST — discovered_from=none. WorkerPoolSpec.ephemeral bool, Job-per-assignment, zero cross-build contamination. Tradeoffs: cold-start (~10-30s), no locality (W_LOCALITY=0), pod churn. soft_dep 286: P0286+P0296 together are the multi-tenant security story. P0288 bloom gauge obviated for ephemeral pools (fresh bloom every pod); STS pools still need it. HEAVY: builders.rs (P0285/P0286 also), runtime.rs (P0288 also), lifecycle.nix (P0285/P0289/P0294 also), controller.md (4 plans touch)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Soft-depends on:** [P0286](plan-0286-privileged-hardening-device-plugin.md) — together they're the multi-tenant security story. `security.md` prose in T6 references P0286's `r[sec.pod.host-users-false]`. If P0286 hasn't merged, that reference is forward-looking.
**Conflicts with:** HEAVY. [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) also touched by P0285 (annotation-only) + P0286 (securityContext block). [`runtime.rs`](../../rio-worker/src/runtime.rs) also touched by P0288 (heartbeat gauge). [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) also touched by P0285/P0289/P0294. [`controller.md`](../../docs/src/components/controller.md) also touched by P0285/P0292/P0294. [`admin.proto`](../../rio-proto/proto/admin.proto) advisory-serial with other proto-EOF appenders. Advisory: serialize after P0286 (same `build_pod_spec` block this plan's `build_job_spec` helper reuses).
