# Plan 996394102: Enforce maxConcurrentBuilds=1 for ephemeral WorkerPools

Bughunter (mc98-105) found an isolation-guarantee violation: `WorkerPoolSpec.ephemeral: true` with `maxConcurrentBuilds > 1` silently allows multiple builds to share a pod, contradicting the security.md claim at [`docs/src/security.md:177`](../../docs/src/security.md): "strongest cross-build isolation rio offers: one pod per build, zero shared state."

**The gap has three surfaces:**

1. [`rio-crds/src/workerpool.rs:50`](../../rio-crds/src/workerpool.rs) CEL only enforces `!self.ephemeral || (self.replicas.min == 0 && self.replicas.max > 0)`. No constraint on `maxConcurrentBuilds`.
2. [`rio-controller/src/reconcilers/workerpool/ephemeral.rs:302-306`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) `build_job` reuses `build_pod_spec` which sets `RIO_MAX_BUILDS=spec.max_concurrent_builds` ([`builders.rs:473`](../../rio-controller/src/reconcilers/workerpool/builders.rs)). It appends `RIO_EPHEMERAL=1` but does NOT override `RIO_MAX_BUILDS`.
3. [`rio-worker/src/main.rs:652-671`](../../rio-worker/src/main.rs) the ephemeral single-shot watcher spawns per-Assignment (inside the `if ephemeral` block at `:652`, which runs for EVERY Assignment-arm iteration) and does `acquire_many(cfg.max_builds)`. With `max_builds=4`, worker accepts and runs up to 4 builds before the watcher fires `ephemeral_done`.

**Test coverage hides it:** VM test hardcodes `maxConcurrentBuilds: 1` ([`lifecycle.nix:1634`](../../nix/tests/scenarios/lifecycle.nix)); unit test hardcodes `max_concurrent_builds: 1` ([`ephemeral.rs:391`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs)). Gap never exercised.

**Fix options from the followup:**
- **(a)** CEL: `!self.ephemeral || self.maxConcurrentBuilds == 1`. Operator-facing — `kubectl apply` with the bad combo gets a clear error. Doesn't fix existing CRs applied before CEL update.
- **(b)** `build_job` overrides `RIO_MAX_BUILDS` to `"1"`. Smallest blast radius — `ephemeral.rs:302-306` already mutates the env vec; one more push or find-and-replace. Defensive regardless of CEL.
- **(c)** Worker-side: `if ephemeral { cfg.max_builds = 1; }`. Least invasive to controller; belt-and-suspenders.

**Decision: (a) + (b).** CEL is the operator UX (clear error at apply time). Env override is defensive (existing bad CRs, future CEL drift). Worker-side (c) is redundant given (b) — the env var IS the config.

## Entry criteria

- [P0296](plan-0296-ephemeral-builders-opt-in.md) merged (ephemeral reconciler + `build_job` + CEL rules exist) — **DONE**

## Tasks

### T1 — `fix(crds):` CEL enforces maxConcurrentBuilds==1 for ephemeral

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) at `:50` — extend the existing CEL:

```rust
// CEL: ephemeral=true requires replicas.min==0 AND maxConcurrentBuilds==1.
// replicas.min==0 because "pool size" is purely a concurrent-Job ceiling
// (replicas.max); there IS no standing set.
// maxConcurrentBuilds==1 because ephemeral's isolation guarantee is
// one-pod-per-build. A pod running 4 builds shares FUSE cache +
// overlayfs upper across those 4 — violates the "zero cross-build state"
// claim at security.md § Ephemeral Builders.
// r[impl ctrl.pool.ephemeral-single-build]
#[x_kube(validation = "!self.ephemeral || (self.replicas.min == 0 && self.replicas.max > 0 && self.maxConcurrentBuilds == 1)")]
```

MODIFY the `cel_rules_in_schema` test at `:467+` — add an assert for the new clause:

```rust
assert!(
    json.contains("self.maxConcurrentBuilds == 1"),
    "ephemeral maxConcurrentBuilds CEL clause missing from schema"
);
```

**Regenerate CRD YAML:** run `just crdgen` (or `cargo run --bin crdgen > infra/helm/crds/workerpools.rio.build.yaml`) — the CEL rule ends up in the rendered `x-kubernetes-validations`.

### T2 — `fix(controller):` build_job overrides RIO_MAX_BUILDS to "1"

MODIFY [`rio-controller/src/reconcilers/workerpool/ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) after `:306` (the `RIO_EPHEMERAL` push):

```rust
    // r[impl ctrl.pool.ephemeral-single-build]
    // Force RIO_MAX_BUILDS=1 regardless of spec.max_concurrent_builds.
    // build_pod_spec set it from spec (builders.rs:473); ephemeral mode
    // needs exactly one build per pod. The CEL at workerpool.rs:50
    // rejects ephemeral+maxConcurrentBuilds>1 at apply time; this
    // override is defensive (existing CRs, CEL drift).
    //
    // Find-and-replace, not push: build_pod_spec already set it. A
    // second env var with the same name is last-wins in K8s but
    // depending on that is fragile — explicit replace.
    for e in pod_spec.containers[0]
        .env
        .as_mut()
        .expect("build_container always sets env")
        .iter_mut()
    {
        if e.name == "RIO_MAX_BUILDS" {
            e.value = Some("1".into());
        }
    }
```

### T3 — `fix(worker):` spawn watcher ONCE, not per-assignment

MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) at `:652-671` — the watcher currently spawns inside the Assignment-arm iteration. Each assignment spawns a NEW watcher doing `acquire_many(max)`. With `max=1` this is harmless (first watcher fires after first build completes); with `max>1` it's still broken (N watchers, all blocked on `acquire_many(N)`, all fire at once when N builds complete). Even with T1+T2 enforcing `max=1`, spawning N watchers for N assignments is wasteful.

Move the watcher spawn to ONCE before the select loop, or use a `Once`-style guard:

```rust
// Before the inner select loop — ONE watcher, not one-per-assignment.
let ephemeral_watcher_spawned = ephemeral.then(|| {
    let sem = Arc::clone(&build_semaphore);
    let max = cfg.max_builds;
    let done = Arc::clone(&ephemeral_done);
    tokio::spawn(async move {
        // Wait for a permit to be TAKEN first (some build started),
        // then for all permits to return (all builds done).
        // available_permits() == max means nothing started yet.
        loop {
            if sem.available_permits() < max as usize {
                break; // a build started
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if sem.acquire_many(max).await.is_ok() {
            done.notify_one();
        }
    })
});
```

**Simpler alternative:** keep the per-assignment spawn but guard it with `std::sync::Once` or an `AtomicBool` — spawn only on the FIRST assignment. The `acquire_many(1)` pattern (T2 ensures max=1) means the watcher fires correctly after the ONE build. The bughunter's "spawned per-assignment" finding is a defect regardless of the enforcement fix — clean it up here.

**Impl choice at dispatch:** the simpler `Once` guard is less invasive (keeps the watcher logic co-located with the assignment arm, where the "why spawn a watcher" comment lives).

### T4 — `test(controller):` build_job RIO_MAX_BUILDS=1 regardless of spec

NEW test in [`rio-controller/src/reconcilers/workerpool/ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) tests module:

```rust
/// Regression: ephemeral with maxConcurrentBuilds=4 still gets
/// RIO_MAX_BUILDS=1 in the Job pod. The CEL rejects this combo at
/// apply time (workerpool.rs:50); this test proves the defensive
/// env-override fires regardless (existing bad CRs, CEL drift).
// r[verify ctrl.pool.ephemeral-single-build]
#[test]
fn build_job_forces_max_builds_1_ignoring_spec() {
    let wp = fixture_with(|spec| {
        spec.ephemeral = true;
        spec.max_concurrent_builds = 4; // CEL rejects, but pretend
    });
    let job = build_job(&wp, oref(), &scheduler_addrs(), "store:9090").unwrap();
    let env = &job.spec.unwrap().template.spec.unwrap().containers[0]
        .env.as_ref().unwrap();
    let max_builds = env.iter()
        .find(|e| e.name == "RIO_MAX_BUILDS")
        .expect("RIO_MAX_BUILDS must be set");
    assert_eq!(
        max_builds.value.as_deref(), Some("1"),
        "ephemeral Job must force RIO_MAX_BUILDS=1 — spec had {}",
        wp.spec.max_concurrent_builds
    );
}
```

### T5 — `test(crds):` CEL rejects ephemeral + maxConcurrentBuilds>1

MODIFY [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) — the existing `cel_rules_in_schema` test at `:467` checks the CEL string is IN the schema; add a CEL-evaluation test (if a CEL test harness exists — check at dispatch) OR a VM-level `kubectl apply` rejection test:

```rust
/// T5: the CEL clause for maxConcurrentBuilds is present in schema
/// output. Extends cel_rules_in_schema.
// r[verify ctrl.pool.ephemeral-single-build]
#[test]
fn cel_ephemeral_max_concurrent_in_schema() {
    let schema = WorkerPool::crd();
    let json = serde_json::to_string(&schema).unwrap();
    assert!(
        json.contains("self.maxConcurrentBuilds == 1"),
        "ephemeral→maxConcurrentBuilds==1 CEL missing"
    );
}
```

**VM-level (optional, if CEL-eval harness doesn't exist):** extend [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) ephemeral subtest with a `kubectl apply` of a bad CR (ephemeral + maxConcurrentBuilds: 4) → assert rejection with the CEL message in stderr.

## Exit criteria

- `/nbr .#ci` green
- `grep 'maxConcurrentBuilds == 1' rio-crds/src/workerpool.rs` → ≥2 hits (T1: CEL rule + test assert)
- `grep 'maxConcurrentBuilds == 1' infra/helm/crds/workerpools.rio.build.yaml` → ≥1 hit (T1: crdgen picked up the rule)
- `cargo nextest run -p rio-crds cel_ephemeral_max_concurrent_in_schema` → pass (T5)
- `cargo nextest run -p rio-controller build_job_forces_max_builds_1_ignoring_spec` → pass (T4)
- `grep 'RIO_MAX_BUILDS.*"1"' rio-controller/src/reconcilers/workerpool/ephemeral.rs` → ≥1 hit (T2: override present)
- `grep 'if ephemeral' rio-worker/src/main.rs | wc -l` — watcher spawn guarded (T3: check specific pattern at dispatch; either `Once` or moved-before-loop)
- **T4 mutation:** comment out the T2 env-replace loop → `build_job_forces_max_builds_1_ignoring_spec` fails with "spec had 4"
- `nix develop -c tracey query rule ctrl.pool.ephemeral-single-build` → shows ≥1 `impl` and ≥2 `verify` sites (T1 impl, T4+T5 verify)

## Tracey

References existing markers:
- `r[ctrl.pool.ephemeral]` — T2 serves (the isolation guarantee is part of this marker's text at [`controller.md:76-81`](../../docs/src/components/controller.md))

Adds new markers to component specs:
- `r[ctrl.pool.ephemeral-single-build]` → `docs/src/components/controller.md` (see ## Spec additions below) — the maxConcurrentBuilds=1 enforcement is a distinct sub-guarantee worth a sibling marker

## Spec additions

New paragraph in [`docs/src/components/controller.md`](../../docs/src/components/controller.md), inserted after the `r[ctrl.pool.ephemeral]` section (after the **Cleanup:** paragraph at `:107-109`):

```markdown
r[ctrl.pool.ephemeral-single-build]
Ephemeral WorkerPools MUST enforce `maxConcurrentBuilds == 1`. The
one-pod-per-build isolation guarantee depends on it: a pod running N
builds shares FUSE cache and overlayfs upper across those N. CEL
validation rejects `ephemeral: true` with `maxConcurrentBuilds > 1` at
`kubectl apply` time; `build_job` defensively overrides `RIO_MAX_BUILDS`
to `"1"` regardless of the spec value.
```

Also MODIFY [`docs/src/security.md:197`](../../docs/src/security.md) — the table row for "Pod lifetime" currently says `WorkerPoolSpec.ephemeral: true` → add `+ maxConcurrentBuilds: 1 (CEL-enforced)` so the security doc reflects the enforcement.

## Files

```json files
[
  {"path": "rio-crds/src/workerpool.rs", "action": "MODIFY", "note": "T1: extend CEL :50 with && self.maxConcurrentBuilds==1; T5: cel_ephemeral_max_concurrent_in_schema test"},
  {"path": "infra/helm/crds/workerpools.rio.build.yaml", "action": "MODIFY", "note": "T1: regenerated via just crdgen — CEL rule in x-kubernetes-validations"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T2: find-and-replace RIO_MAX_BUILDS→1 after :306; T4: build_job_forces_max_builds_1_ignoring_spec test"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T3: guard ephemeral watcher spawn with Once/AtomicBool OR move before select loop — spawn ONCE not per-assignment at :652-671"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "Spec addition: r[ctrl.pool.ephemeral-single-build] after :109"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": ":197 table row — add maxConcurrentBuilds:1 CEL-enforced to Pod lifetime row"}
]
```

```
rio-crds/src/workerpool.rs                       # T1: CEL + T5: test
infra/helm/crds/workerpools.rio.build.yaml       # T1: crdgen regen
rio-controller/src/reconcilers/workerpool/
└── ephemeral.rs                                 # T2: env override + T4: test
rio-worker/src/main.rs                           # T3: watcher Once-guard
docs/src/
├── components/controller.md                     # spec addition
└── security.md                                  # :197 table row
```

## Dependencies

```json deps
{"deps": [296], "soft_deps": [311, 347], "note": "discovered_from=bughunter(mc98-105). P0296 (DONE) shipped ephemeral.rs + the CEL at workerpool.rs:50 — this extends both. Three surfaces (CEL, env-override, watcher per-assignment spawn). Soft-dep P0311-T25 (spawn_count arithmetic test — same file ephemeral.rs, non-overlapping: T25 extracts a free fn near :176, this edits :302-306 env mutation + adds a test). Soft-dep P0347 (adds activeDeadlineSeconds to build_job — also ephemeral.rs :327+ JobSpec block, non-overlapping with T2's env mutation at :302-306). Security.md:177 claim is VIOLATED without this — 'one pod per build, zero shared state' is false when maxConcurrentBuilds>1. Option (a)+(b) chosen: CEL for operator UX at apply time; env-override defensive for existing bad CRs + CEL drift. Option (c) worker-side redundant given (b) — env IS the config. T3 watcher-per-assignment is a separate defect (wasteful N spawns) — cleanup regardless."}
```

**Depends on:** [P0296](plan-0296-ephemeral-builders-opt-in.md) — ephemeral reconciler + CEL baseline exist. **DONE.**
**Soft-conflicts:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T25 (ephemeral.rs :176 spawn_count extraction — different region). [P0347](plan-0347-ephemeral-activedeadlineseconds.md) (ephemeral.rs JobSpec at `:327+` — different region from T2's `:302-306`).
**Conflicts with:** [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) count=38 (HOT) — T3 edits `:652-671` ephemeral watcher spawn; additive guard or code-move, no signature change. [`rio-crds/src/workerpool.rs`](../../rio-crds/src/workerpool.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T11 adds `.message()` to CEL attrs (different rules); [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T6 adds seccomp asserts to `cel_rules_in_schema` (T5 here adds a sibling test fn — no collision).
