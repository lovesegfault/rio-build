# Plan 0380: Extract WorkerPoolSpec test fixture — collapse 4× E0063-magnet literals

Consolidator (mc170) flagged a triplication that [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md)'s E0063 at `scaling.rs:1273` (now shifted to `:1354` post-merge) made visceral: there are **four** near-identical 28-field `WorkerPoolSpec { ... }` test literals scattered across the controller crate. `WorkerPoolSpec` has no `Default` derive (CEL-required fields must be explicit), so every new field triggers E0063 at every literal.

Evidence from git history — five field-add commits each required 3-4 site touches:
- [`f5f87895`](https://github.com/search?q=f5f87895&type=commits) `bloom_expected_items` (P0375)
- [`55da672a`](https://github.com/search?q=55da672a&type=commits) `ephemeral`
- [`837b633d`](https://github.com/search?q=837b633d&type=commits) `seccomp_profile`
- [`f740147f`](https://github.com/search?q=f740147f&type=commits) `termination_grace_period_seconds`
- [`c5fe8071`](https://github.com/search?q=c5fe8071&type=commits) `image_pull_policy`

P0374 is the root-cause case: P0234 added [`scaling.rs:1354`](../../rio-controller/src/scaling.rs) (the inline literal inside `is_wps_owned_detects_controller_ownerref`) THEN P0375 immediately hit E0063 at it while adding `bloom_expected_items` — see [P0375](plan-0375-bloom-expected-items-crd-knob.md) exit-criterion #13. The consolidator found a fourth literal at [`scaling.rs:1459`](../../rio-controller/src/scaling.rs) (`test_wp_spec()` helper) that was added by P0374 itself for the `find_wps_child` tests — the plan that HIT the triplication also ADDED to it.

The `#[cfg(test)] pub(crate) mod fixtures` already exists at [`lib.rs:51`](../../rio-controller/src/lib.rs) — currently holds `ApiServerVerifier` re-export and `apply_ok_scenarios()`. It's reachable from all four literal sites.

## Entry criteria

- [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md) merged (adds the 4th literal at `scaling.rs:1459` — without it this plan collapses 3 not 4)
- [P0375](plan-0375-bloom-expected-items-crd-knob.md) merged (adds `bloom_expected_items: None,` to all 4 sites — this plan's fixture must include the field)

## Tasks

### T1 — `refactor(controller):` fixtures.rs — add test_workerpool_spec + test_workerpool + test_sched_addrs

Extend [`rio-controller/src/fixtures.rs`](../../rio-controller/src/fixtures.rs) with:

```rust
use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPool, WorkerPoolSpec};
use crate::reconcilers::workerpool::SchedulerAddrs;

/// Minimal 28-field WorkerPoolSpec. All CEL-required fields explicit;
/// optional fields `None`. Used by test_workerpool() and directly by
/// tests that need to mutate a field before wrapping in WorkerPool.
///
/// NEXT FIELD ADD: touch THIS fn + production builders.rs:101 — 2 sites
/// (down from 4-5). CEL-exhaustiveness is the point; don't derive Default.
pub fn test_workerpool_spec() -> WorkerPoolSpec {
    WorkerPoolSpec {
        replicas: Replicas { min: 2, max: 10 },
        ephemeral: false,
        autoscaling: Autoscaling { metric: "queueDepth".into(), target_value: 5 },
        resources: None,
        max_concurrent_builds: 4,
        fuse_cache_size: "50Gi".into(),
        fuse_threads: None,
        bloom_expected_items: None,
        fuse_passthrough: None,
        daemon_timeout_secs: None,
        features: vec!["kvm".into()],
        systems: vec!["x86_64-linux".into()],
        size_class: "small".into(),
        image: "rio-worker:test".into(),
        image_pull_policy: None,
        node_selector: None,
        tolerations: None,
        termination_grace_period_seconds: None,
        privileged: None,
        seccomp_profile: None,
        host_network: None,
        tls_secret_name: None,
        topology_spread: None,
        fod_proxy_url: None,
    }
}

/// Wrap spec with name + UID + namespace (controller_owner_ref needs
/// UID; apiserver sets it in prod, tests fake it).
pub fn test_workerpool(name: &str) -> WorkerPool {
    let mut wp = WorkerPool::new(name, test_workerpool_spec());
    wp.metadata.uid = Some(format!("{name}-uid"));
    wp.metadata.namespace = Some("rio".into());
    wp
}

/// SchedulerAddrs for builder tests. Duplicated at tests.rs:53 +
/// ephemeral.rs:443 (as `test_sched`).
pub fn test_sched_addrs() -> SchedulerAddrs {
    SchedulerAddrs {
        addr: "sched:9001".into(),
        balance_host: Some("sched-headless".into()),
        balance_port: 9001,
    }
}
```

### T2 — `refactor(controller):` migrate tests.rs:16 test_wp() — 39 callers via local helper

[`rio-controller/src/reconcilers/workerpool/tests.rs:15-50`](../../rio-controller/src/reconcilers/workerpool/tests.rs) defines `fn test_wp() -> WorkerPool` with the 28-field literal + `fn test_sched_addrs()`. Replace the body with calls to `crate::fixtures::{test_workerpool, test_sched_addrs}`:

```rust
fn test_wp() -> WorkerPool {
    // Delegates to shared fixture. Local wrapper kept so 39 call
    // sites don't need a signature change.
    crate::fixtures::test_workerpool("test-pool")
}
// Delete local test_sched_addrs() — use crate::fixtures::test_sched_addrs directly.
```

Add `use crate::fixtures::test_sched_addrs;` to the `use` block at `:11`. 3 call sites at `:68`, `:925`, `:1064` stay unchanged.

### T3 — `refactor(controller):` migrate ephemeral.rs:408 test_wp() — 4 callers, differs in ephemeral=true

[`rio-controller/src/reconcilers/workerpool/ephemeral.rs:407-441`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) has its own `test_wp()` that differs from tests.rs in 5 fields: `min: 0, max: 4`, `ephemeral: true`, `max_concurrent_builds: 1`, `fuse_cache_size: "10Gi"`, `features: vec![]`, `size_class: String::new()`. Replace with:

```rust
fn test_wp() -> WorkerPool {
    let mut spec = crate::fixtures::test_workerpool_spec();
    spec.replicas = Replicas { min: 0, max: 4 };
    spec.ephemeral = true;
    spec.max_concurrent_builds = 1;
    spec.fuse_cache_size = "10Gi".into();
    spec.features = vec![];
    spec.size_class = String::new();
    let mut wp = WorkerPool::new("eph-pool", spec);
    wp.metadata.uid = Some("uid-eph".into());
    wp.metadata.namespace = Some("rio".into());
    wp
}
```

Delete `test_sched()` at `:443` — migrate 3 call sites (`:468/:522/:568`) to `crate::fixtures::test_sched_addrs()`. (The fixture uses `balance_host: Some(...)` vs ephemeral's `None` — ephemeral Jobs don't use balance_host so the difference is immaterial; if a test depends on `None`, override locally.)

### T4 — `refactor(controller):` migrate scaling.rs:1354 + :1459 literals — collapse inline to fixture

[`rio-controller/src/scaling.rs:1347-1441`](../../rio-controller/src/scaling.rs) `is_wps_owned_detects_controller_ownerref` builds its own `WorkerPoolSpec` literal at `:1354`. Replace with `let spec = crate::fixtures::test_workerpool_spec();`. The test calls `.clone()` 3× for standalone/wps_child/other_owned/gc_only — unchanged.

[`rio-controller/src/scaling.rs:1453-1487`](../../rio-controller/src/scaling.rs) `test_wp_spec()` is a DUPLICATE of the above (the comment at `:1457` literally says "Same shape as is_wps_owned_detects_controller_ownerref above — if that fixture changes (new WorkerPoolSpec field), update both"). Replace body with:

```rust
fn test_wp_spec() -> crate::crds::workerpool::WorkerPoolSpec {
    crate::fixtures::test_workerpool_spec()
}
```

Keep the local wrapper — it's called from `test_wp_in_ns` at `:1519` and keeping it avoids a signature change at the caller. The 19 transitive callers via `test_wps`/`test_wp_in_ns`/`with_wps_owner` stay unchanged.

### T5 — `refactor(controller):` builders.rs:101 production literal — UNCHANGED (add rationale comment)

[`rio-controller/src/reconcilers/workerpoolset/builders.rs:101`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs) `build_child_workerpool` has the PRODUCTION `WorkerPoolSpec { ... }` literal that maps `PoolTemplate` → child. **Do not migrate to fixture** — exhaustiveness here IS the contract: if `WorkerPoolSpec` gains a field, the compiler forces a decision about whether `PoolTemplate` mirrors it. Add a comment above `:101`:

```rust
// EXHAUSTIVE by design: when WorkerPoolSpec gains a field, E0063
// here forces a decision — does PoolTemplate mirror it (expose to
// WPS users) or hardcode a default (controller concern)? This is
// the ONE production literal; test literals delegate to
// crate::fixtures::test_workerpool_spec().
let spec = WorkerPoolSpec {
```

## Exit criteria

- `/nixbuild .#ci` green (or nextest-standalone clause-4c if VM-flake)
- `grep -c 'WorkerPoolSpec {' rio-controller/src/` → 2 (fixtures.rs + builders.rs:101 production; the test files delegate)
- `grep 'WorkerPoolSpec {' rio-controller/src/reconcilers/workerpool/tests.rs rio-controller/src/reconcilers/workerpool/ephemeral.rs rio-controller/src/scaling.rs` → 0 hits (all migrated)
- `grep 'test_workerpool_spec\|test_workerpool\b\|test_sched_addrs' rio-controller/src/fixtures.rs` → ≥3 hits (three fns defined)
- `grep 'EXHAUSTIVE by design' rio-controller/src/reconcilers/workerpoolset/builders.rs` → 1 hit (rationale comment)
- `cargo nextest run -p rio-controller` → passes (all ~46 test_wp callers + ~19 scaling callers still green)
- Delete a random `None` field from `fixtures.rs::test_workerpool_spec()` → E0063 at EXACTLY 1 site (the fixture itself) not 4 — proves the collapse worked. Then restore.

## Tracey

No new markers. The fixture consolidation is test-surface refactoring; it does not change what behaviors are verified. Existing `r[verify ctrl.*]` annotations at [`tests.rs:1-4`](../../rio-controller/src/reconcilers/workerpool/tests.rs) (`ctrl.crd.workerpool`, `ctrl.reconcile.owner-refs`, `ctrl.drain.all-then-scale`, `ctrl.drain.sigterm`) are on the test file header and don't move.

## Files

```json files
[
  {"path": "rio-controller/src/fixtures.rs", "action": "MODIFY", "note": "T1: +test_workerpool_spec + test_workerpool + test_sched_addrs (3 new pub fns after apply_ok_scenarios)"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "T2: :15-50 test_wp body → fixtures delegation; :53 test_sched_addrs deleted, use fixture"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T3: :407-441 test_wp body → spec-from-fixture + 5-field override; :443 test_sched deleted"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T4: :1354-1383 inline literal → test_workerpool_spec(); :1453-1487 test_wp_spec body → fixture delegation"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T5: +EXHAUSTIVE rationale comment above :101 production literal (unchanged)"}
]
```

```
rio-controller/src/
├── fixtures.rs                                  # T1: +3 pub fns
├── reconcilers/workerpool/
│   ├── tests.rs                                 # T2: test_wp → delegation
│   └── ephemeral.rs                             # T3: test_wp → spec+override
├── scaling.rs                                   # T4: 2 literals → delegation
└── reconcilers/workerpoolset/builders.rs        # T5: +comment (unchanged literal)
```

## Dependencies

```json deps
{"deps": [374, 375], "soft_deps": [381, 234], "note": "P0374 merged adds scaling.rs:1459 test_wp_spec (the 4th literal) + scaling.rs:1354 inline (the 3rd, via P0234's test that P0374 carried forward). P0375 merged adds bloom_expected_items field — fixture must include it. Soft-dep P0381 (scaling.rs split): this plan touches scaling.rs:1354/:1459 in the tests mod; the split moves those tests to scaling/per_class.rs. If split lands FIRST, re-grep the literal locations. If THIS lands first (preferred — smaller diff), the split carries the already-collapsed test_wp_spec forward. Soft-dep P0234: added the 2nd autoscaler which seeded scaling.rs test growth; already merged but line-refs are p234-worktree — re-grep at dispatch."}
```

**Depends on:** [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md) — adds `scaling.rs:1459` `test_wp_spec()` (the 4th literal). [P0375](plan-0375-bloom-expected-items-crd-knob.md) — adds `bloom_expected_items: None,` to all sites.

**Conflicts with:** [P0381](plan-0381-scaling-rs-fault-line-split.md) — both touch `scaling.rs` tests mod. Preferred sequence: THIS plan FIRST (smaller, makes the split's tests-move carry a 2-line delegation not a 30-line literal). [P0304](plan-0304-trivial-batch-p0222-harness.md) T112-T114/T121 also touch `scaling.rs` — all non-overlapping (T114 is `child_name` at `:1022`, T121 is `ssa_envelope` migration in prod section). `scaling.rs` collision count is high — check `onibus collisions check rio-controller/src/scaling.rs` at dispatch.
