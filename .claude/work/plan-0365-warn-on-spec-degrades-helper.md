# Plan 0365: `warn_on_spec_degrades()` helper — consolidate pre-CEL-spec Warning events

Three-agent convergence (bughunt-mc133 + rev-p354 + consol-mc135) on the same structural gap: the controller's defensive-override pattern (CEL rejects new specs → builder silently degrades old specs) has inconsistent operator visibility. Each constraint grows its own Warning-emit block in a different place, and two of the three existing surfaces are broken or missing.

**P0359's Warning at [`mod.rs:150-168`](../../rio-controller/src/reconcilers/workerpool/mod.rs) is UNREACHABLE for ephemeral pools.** The `hostNetwork:true + !privileged → HostUsersSuppressedForHostNetwork` event fires at `:150` — but the ephemeral early-return at `:130` (`if wp.spec.ephemeral { return reconcile_ephemeral(...) }`) runs before it. An ephemeral pool with `hostNetwork: true` (latent today — no shipped values files do this) gets the `build_pod_spec` silent-suppress in [`ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) via the shared `build_job → build_pod_spec` chain, but no Warning event. Operator sees nothing in `kubectl get events`.

**P0354's `RIO_MAX_BUILDS` clamp at [`ephemeral.rs:309-335`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) has NO Warning at all.** The env-replace loop silently rewrites `spec.max_concurrent_builds=4` → `RIO_MAX_BUILDS=1`. The doc-comment at `:317` says "DEFENSIVE for existing CRs applied before the CEL landed" — but silent defense means the operator who applied a pre-CEL `maxConcurrentBuilds: 4` never learns the spec is stale. They see `kubectl get wp` showing `maxConcurrentBuilds: 4` and assume it's honored.

**The third constraint (the NEXT spec-degrade field) will repeat the pattern.** Three independent CEL rules at [`workerpool.rs:56-88`](../../rio-crds/src/workerpool.rs) with three builder-side defensive overrides → three hand-rolled Warning-emit blocks with three different placement decisions. Extract a shared helper that runs ONCE before the ephemeral branch, checks ALL known degrade conditions, emits a Warning per violation. Both STS-mode and ephemeral-mode paths get the same visibility for free.

## Entry criteria

- [P0354](plan-0354-ephemeral-enforce-single-build.md) merged (`RIO_MAX_BUILDS` clamp at `ephemeral.rs:309-335` exists; `r[ctrl.pool.ephemeral-single-build]` marker exists)
- [P0359](plan-0359-hostusers-hostnetwork-guardrail.md) merged (`HostUsersSuppressedForHostNetwork` event at `mod.rs:150-168` exists; `r[ctrl.crd.host-users-network-exclusive]` marker exists)

## Tasks

### T1 — `refactor(controller):` extract `warn_on_spec_degrades` helper

NEW free fn in [`rio-controller/src/reconcilers/workerpool/mod.rs`](../../rio-controller/src/reconcilers/workerpool/mod.rs). Place it after the `Ctx`-level helper block (near `publish_event` at [`reconcilers/mod.rs:65`](../../rio-controller/src/reconcilers/mod.rs)) or keep it local to `workerpool/mod.rs` — prefer the latter since all checks are WorkerPool-shaped.

```rust
/// Emit Warning events for every spec field the builder will silently
/// degrade. Each check mirrors a CEL rule at apply-time (workerpool.rs
/// `#[x_kube(validation)]` attrs); the builder's defensive override
/// handles pre-CEL specs that the apiserver already accepted, but the
/// OPERATOR doesn't know their spec is stale unless we surface it.
///
/// Runs BEFORE the ephemeral branch so both paths (STS-mode +
/// ephemeral) get visibility. `build_pod_spec` is shared by both
/// (ephemeral calls it via `build_job`), so any builder-side degrade
/// applies to both; the Warning should too.
///
/// Best-effort: event-publish failures are logged in ctx.publish_event,
/// never block reconcile.
// r[impl ctrl.event.spec-degrade]
async fn warn_on_spec_degrades(wp: &WorkerPool, ctx: &Ctx) {
    use kube::runtime::events::{Event as KubeEvent, EventType};

    // r[ctrl.crd.host-users-network-exclusive] — hostUsers suppressed
    // when hostNetwork:true + !privileged. CEL rejects NEW; builder
    // suppresses for OLD (builders.rs host_users gate).
    if wp.spec.host_network == Some(true) && wp.spec.privileged != Some(true) {
        ctx.publish_event(wp, &KubeEvent {
            type_: EventType::Warning,
            reason: "HostUsersSuppressedForHostNetwork".into(),
            note: Some(
                "hostNetwork:true forces hostUsers omitted (K8s admission \
                 rejects the combo). Set privileged:true explicitly, or \
                 drop hostNetwork.".into(),
            ),
            action: "Reconcile".into(),
            secondary: None,
        }).await;
    }

    // r[ctrl.pool.ephemeral-single-build] — RIO_MAX_BUILDS clamped
    // to 1 when ephemeral:true + maxConcurrentBuilds>1. CEL rejects
    // NEW; ephemeral.rs build_job env-replace handles OLD.
    if wp.spec.ephemeral && wp.spec.max_concurrent_builds > 1 {
        ctx.publish_event(wp, &KubeEvent {
            type_: EventType::Warning,
            reason: "MaxConcurrentBuildsClampedForEphemeral".into(),
            note: Some(format!(
                "ephemeral:true forces maxConcurrentBuilds=1 (spec has {}). \
                 One-pod-per-build isolation requires it. Update spec to \
                 maxConcurrentBuilds: 1 to silence this warning.",
                wp.spec.max_concurrent_builds
            )),
            action: "Reconcile".into(),
            secondary: None,
        }).await;
    }

    // NEXT constraint lands HERE as a third `if` block — that's the
    // point: one helper, N checks, consistent visibility.
}
```

Call it from `apply()` BEFORE the `if wp.spec.ephemeral { return ... }` branch at `:130`:

```rust
async fn apply(wp: Arc<WorkerPool>, ctx: &Ctx) -> Result<Action> {
    let ns = wp.namespace().expect("checked in reconcile()");
    let name = wp.name_any();

    // Surface silent degrades BEFORE branching on ephemeral — both
    // paths share build_pod_spec, both get the Warning.
    warn_on_spec_degrades(&wp, ctx).await;

    // r[impl ctrl.pool.ephemeral]
    if wp.spec.ephemeral {
        return ephemeral::reconcile_ephemeral(&wp, ctx).await;
    }
    // ... STS-mode continues
}
```

DELETE the inline `HostUsersSuppressedForHostNetwork` block at `:134-168` — it moves into the helper.

### T2 — `test(controller):` warn fires for ephemeral pool with hostNetwork

NEW test in [`rio-controller/src/reconcilers/workerpool/tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests.rs). The existing `Recorder::new` fixture at `:949` shows the test harness shape.

```rust
/// Regression: ephemeral pool with hostNetwork:true gets the
/// HostUsersSuppressedForHostNetwork warning. Before T1 the
/// Warning-emit at mod.rs:150 ran AFTER the ephemeral early-return
/// at :130 → unreachable for ephemeral. Now warn_on_spec_degrades
/// runs BEFORE the branch, both paths covered.
// r[verify ctrl.event.spec-degrade]
#[tokio::test]
async fn warn_fires_for_ephemeral_with_host_network() {
    let mut wp = minimal_wp();
    wp.spec.ephemeral = true;
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;

    let (ctx, recorder_spy) = test_ctx_with_recorder_spy();
    let _ = apply(Arc::new(wp), &ctx).await;  // may fail post-warn; we only care about the event

    let events = recorder_spy.recorded_events();
    assert!(
        events.iter().any(|e| e.reason == "HostUsersSuppressedForHostNetwork"),
        "ephemeral+hostNetwork should emit the Warning — events: {events:?}"
    );
}
```

If the existing tests don't have a `recorder_spy` pattern, use the `MockRecorder` from `rio-test-support` (check at dispatch — [`reconcilers/workerpool/tests.rs:949`](../../rio-controller/src/reconcilers/workerpool/tests.rs) constructs a real `Recorder`; may need a spy wrapper). Alternative: stub `publish_event` via a `#[cfg(test)]` channel on `Ctx`.

### T3 — `test(controller):` warn fires for ephemeral with maxConcurrentBuilds>1

NEW test in the same file. P0354's test ([`ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) `build_job_forces_max_builds_1_ignoring_spec`) proves the env-replace fires; this proves the Warning fires.

```rust
// r[verify ctrl.pool.ephemeral-single-build]
#[tokio::test]
async fn warn_fires_for_ephemeral_with_maxbuilds_gt_1() {
    let mut wp = minimal_wp();
    wp.spec.ephemeral = true;
    wp.spec.max_concurrent_builds = 4;  // CEL rejects; pre-CEL spec

    let (ctx, recorder_spy) = test_ctx_with_recorder_spy();
    let _ = apply(Arc::new(wp), &ctx).await;

    let events = recorder_spy.recorded_events();
    let warn = events.iter()
        .find(|e| e.reason == "MaxConcurrentBuildsClampedForEphemeral")
        .expect("should emit Warning for stale maxConcurrentBuilds");
    assert!(
        warn.note.as_deref().unwrap_or("").contains("spec has 4"),
        "warning should name the spec value — note: {:?}", warn.note
    );
}
```

### T4 — `test(controller):` STS-mode path still emits hostNetwork warn (non-regression)

The T1 move deletes the inline block at `:134-168` — prove the STS-mode path (non-ephemeral) still gets the Warning via the helper.

```rust
#[tokio::test]
async fn warn_fires_for_sts_mode_with_host_network() {
    let mut wp = minimal_wp();
    wp.spec.ephemeral = false;  // STS mode
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;

    let (ctx, recorder_spy) = test_ctx_with_recorder_spy();
    let _ = apply(Arc::new(wp), &ctx).await;

    assert!(
        recorder_spy.recorded_events().iter()
            .any(|e| e.reason == "HostUsersSuppressedForHostNetwork"),
        "STS-mode must still get the warning after helper extraction"
    );
}
```

## Exit criteria

- `/nbr .#ci` green
- `grep -n 'warn_on_spec_degrades' rio-controller/src/reconcilers/workerpool/mod.rs` → ≥2 hits (T1: definition + callsite before `:130` ephemeral branch)
- `grep -A3 'if wp.spec.ephemeral' rio-controller/src/reconcilers/workerpool/mod.rs | grep warn_on_spec_degrades` → empty (T1: helper call is BEFORE the branch, not inside)
- `grep 'HostUsersSuppressedForHostNetwork' rio-controller/src/reconcilers/workerpool/mod.rs | wc -l` → ≥1 inside `warn_on_spec_degrades`, 0 at the old `:150` inline site (T1: moved, not duplicated)
- `grep 'MaxConcurrentBuildsClampedForEphemeral' rio-controller/src/reconcilers/workerpool/mod.rs` → ≥1 hit (T1: new Warning reason)
- `cargo nextest run -p rio-controller warn_fires_for_ephemeral_with_host_network` → pass (T2: reachability regression)
- `cargo nextest run -p rio-controller warn_fires_for_ephemeral_with_maxbuilds_gt_1` → pass (T3)
- `cargo nextest run -p rio-controller warn_fires_for_sts_mode_with_host_network` → pass (T4: non-regression)
- **T1 mutation:** move the `warn_on_spec_degrades` call to AFTER the ephemeral branch → T2 fails
- `nix develop -c tracey query rule ctrl.event.spec-degrade` → shows ≥1 `impl` + ≥1 `verify` (new marker)

## Tracey

References existing markers:
- `r[ctrl.crd.host-users-network-exclusive]` — T1 helper emits the Warning this marker describes ("emitting a Warning event" at [`controller.md:355`](../../docs/src/components/controller.md)); T2+T4 add `r[verify]`
- `r[ctrl.pool.ephemeral-single-build]` — T1 adds the Warning half; T3 adds a second `r[verify]` (P0354 already has one on the env-replace test)

Adds new markers to component specs:
- `r[ctrl.event.spec-degrade]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions)

## Spec additions

New paragraph in [`docs/src/components/controller.md`](../../docs/src/components/controller.md), inserted after the CRD Validation section (after `r[ctrl.crd.host-users-network-exclusive]` at `:354-355`), standalone paragraph, blank line before, col 0:

```
r[ctrl.event.spec-degrade]
The WorkerPool reconciler MUST emit a `Warning`-type Kubernetes Event for every spec field the builder silently degrades. CEL validation rejects NEW specs with invalid combinations; existing specs applied before the CEL rule landed are defensively corrected at pod-template time (e.g., `hostUsers` suppressed for `hostNetwork: true`, `RIO_MAX_BUILDS` clamped to `1` for `ephemeral: true`). Without a Warning event, the operator has no signal that their spec is stale — `kubectl get wp -o yaml` shows the original value; the pod template shows the corrected value. The Warning names the field, the spec value, and the remediation. Emission happens before the ephemeral/STS-mode branch so both reconcile paths have identical visibility.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T1: +warn_on_spec_degrades helper fn; call BEFORE :130 ephemeral branch; DELETE inline HostUsersSuppressedForHostNetwork block :134-168"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "T2+T3+T4: +3 tests — warn_fires_for_ephemeral_with_host_network, warn_fires_for_ephemeral_with_maxbuilds_gt_1, warn_fires_for_sts_mode_with_host_network; may need recorder-spy helper"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "Spec addition: +r[ctrl.event.spec-degrade] after :355"}
]
```

```
rio-controller/src/reconcilers/workerpool/
├── mod.rs               # T1: helper + callsite + delete inline block
└── tests.rs             # T2+T3+T4: 3 event-emit tests
docs/src/components/
└── controller.md        # spec addition: r[ctrl.event.spec-degrade]
```

## Dependencies

```json deps
{"deps": [354, 359], "soft_deps": [311, 347], "note": "discovered_from=bughunt-mc133+rev-p354+consol-mc135 (3-agent convergence). P0359 shipped HostUsersSuppressedForHostNetwork event at mod.rs:150 — unreachable for ephemeral (early-return :130 runs first). P0354 shipped RIO_MAX_BUILDS clamp at ephemeral.rs:309-335 — zero Warning. Extract shared helper BEFORE the ephemeral branch → both modes get visibility. Third constraint (whatever lands next) extends the helper, not yet-another hand-rolled block. Soft-dep P0311-T25 (ephemeral.rs spawn_count extraction — different file, different fn, but same tests.rs where T2-T4 land; additive test-fns, zero name collision). Soft-dep P0347 (activeDeadlineSeconds — also touches build_job in ephemeral.rs; non-overlapping, no shared helper)."}
```

**Depends on:** [P0354](plan-0354-ephemeral-enforce-single-build.md) — `RIO_MAX_BUILDS` clamp + `r[ctrl.pool.ephemeral-single-build]` marker. [P0359](plan-0359-hostusers-hostnetwork-guardrail.md) — `HostUsersSuppressedForHostNetwork` event + `r[ctrl.crd.host-users-network-exclusive]` marker.

**Conflicts with:** [`workerpool/mod.rs`](../../rio-controller/src/reconcilers/workerpool/mod.rs) — T1 deletes `:134-168` (the inline event block) and inserts a helper call before `:130` + a ~50-line helper fn. No other UNIMPL plan edits this region. [`workerpool/tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests.rs) — T2-T4 add 3 test fns; [P0311-T25](plan-0311-test-gap-batch-cli-recovery-dash.md) adds `spawn_count` arithmetic tests; additive, non-overlapping names. [`controller.md`](../../docs/src/components/controller.md) count=17 — spec addition at `:355+`, pure append after an existing marker.
