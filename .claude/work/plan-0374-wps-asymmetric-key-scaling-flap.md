# Plan 0374: WPS scaling asymmetric-key flap/orphan + prune-TODO tag

**rev-p234 correctness finding.** The per-class autoscaler (`scale_wps_class`) and the standalone-pool skip guard (`is_wps_owned`) use **different keys** to identify WPS children: the skip guard checks `ownerReferences[kind=WorkerPoolSet, controller=true]` ([`scaling.rs:945-952`](../../rio-controller/src/scaling.rs)), while `scale_wps_class` finds the child by **name-match only** (`format!("{}-{}", wps.name_any(), class.name)` at [`scaling.rs:508-517`](../../rio-controller/src/scaling.rs)). Two failure modes:

- **Flap**: a standalone pool (no WPS ownerRef) whose name happens to match `{wps}-{class}` — e.g. operator manually created `prod-small` before standing up a WPS named `prod` with a `small` class — is **scaled by BOTH loops**: the standalone loop at [`:277`](../../rio-controller/src/scaling.rs) (no ownerRef → `is_wps_owned` false → proceeds) AND `scale_wps_class` at [`:517`](../../rio-controller/src/scaling.rs) (name matches → proceeds). Two autoscalers fighting over `spec.replicas` with different queue-depth signals → flap.
- **Orphan**: a WPS-owned pool whose class was removed from `wps.spec.classes` — `is_wps_owned` returns true → standalone loop skips at [`:273`](../../rio-controller/src/scaling.rs); per-class loop iterates `wps.spec.classes` (removed class not in iteration) → `scale_wps_class` never fires for it. **NEITHER** loop scales it → orphaned child stays at its last replica count forever.

The flap is the correctness bug; the orphan is the "prune stale children" gap documented at [`workerpoolset/mod.rs:34-40`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) — but that comment says "a followup plan can add the prune" with no `TODO(Pnnnn)` tag (per [deferred-work discipline](../../CLAUDE.md) — every deferred task must have a plan-tagged TODO).

T1 fixes the flap (gate `scale_wps_class` on `is_wps_owned`). T2 tags the prune TODO. T3 adds flap regression test. T4 optionally implements the prune (or defers to a followup with a proper tag).

## Entry criteria

- [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) merged (`scale_wps_class`, `is_wps_owned`, per-class loop at `scaling.rs:282-299` exist)

## Tasks

### T1 — `fix(controller):` gate scale_wps_class on is_wps_owned after name-match

MODIFY [`rio-controller/src/scaling.rs`](../../rio-controller/src/scaling.rs) at the name-match block in `scale_wps_class` (`:515-521`, p234 worktree line refs — re-grep at dispatch). After the `.find(|p| p.name_any() == child_name && ...)` returns `Some(child)`, add an ownerRef guard:

```rust
let Some(child) = pools
    .iter()
    .find(|p| p.name_any() == child_name && p.namespace().as_deref() == Some(&wps_ns))
else {
    debug!(child = %child_name, "WPS child not yet created; skipping scale");
    return;
};

// r[impl ctrl.wps.autoscale]
// Symmetry with is_wps_owned: the standalone-pool loop skips
// pools WITH ownerRef; this loop must skip pools WITHOUT. A
// name-match without ownerRef means the pool was manually
// created (or created by something else) with a colliding name
// — scaling it here would fight the standalone loop. See
// P0374 for the flap scenario this prevents.
if !is_wps_owned(child) {
    warn!(
        child = %child_name,
        "pool name matches {{wps}}-{{class}} but has no WPS ownerRef — \
         not scaling per-class (would flap against standalone loop)"
    );
    return;
}
```

Use `warn!` not `debug!` — operator should know there's a name collision. The two-key asymmetry is now aligned: both the skip at `:273` and the per-class entry use `is_wps_owned`.

### T2 — `docs(controller):` tag prune-stale-children TODO

MODIFY [`rio-controller/src/reconcilers/workerpoolset/mod.rs:38`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs). The "a followup plan can add the prune" comment is an orphan TODO. Replace with a proper plan-tagged deferral:

```rust
//!   TODO(P0374): prune stale children (child pool whose
//!   class was removed from spec.classes). Currently: operators
//!   `kubectl delete wp {wps}-{removed-class}` manually. See T4.
```

T4 below either lands the prune or re-tags this to a successor plan.

### T3 — `test(controller):` flap regression — name-match-without-ownerRef skipped

NEW test in [`rio-controller/src/scaling.rs`](../../rio-controller/src/scaling.rs) `#[cfg(test)]` mod tests (near the existing `is_wps_owned_detects_controller_ownerref` at `:1266` p234 ref):

```rust
/// A pool named {wps}-{class} but WITHOUT a WPS ownerRef must NOT
/// be scaled by scale_wps_class — it's a name collision, not a
/// WPS child. Without the is_wps_owned gate (T1), both the
/// standalone loop and the per-class loop would scale it → flap.
///
/// This is the flap-prevention half of the asymmetric-keys bug.
// r[verify ctrl.wps.autoscale]
#[tokio::test]
async fn scale_wps_class_skips_name_collision_without_ownerref() {
    let wps = test_wps_with_classes(&["small"]);
    let mut colliding_pool = test_wp("prod-small");
    // Explicitly NO owner_references — this is a manually-created
    // standalone pool that happens to match the {wps}-{class} shape.
    colliding_pool.metadata.owner_references = None;

    let sc_resp = GetSizeClassStatusResponse {
        classes: vec![mock_class_status("small", /*queued=*/ 20)],
    };

    // (Construct autoscaler/mock-api per existing test pattern at
    // :1374 — or use whatever sts_patch-capturing harness the
    // P0234-T3 test established.)
    let mut scaler = test_scaler();
    scaler.scale_wps_class(&wps, &wps.spec.classes[0], &sc_resp, &[colliding_pool]).await;

    // Load-bearing assert: NO replica patch was issued. The pool
    // name matches but is_wps_owned(child)=false → early-return.
    // If the gate is missing, the scaler patches replicas and this
    // mock records it.
    assert!(
        scaler.captured_patches().is_empty(),
        "name-match pool without WPS ownerRef was scaled per-class — \
         would flap against standalone loop. is_wps_owned gate missing?"
    );
}
```

Adjust `test_scaler()` / `captured_patches()` to match whatever mock-capture pattern P0234-T3's `wps_autoscaler_writes_via_ssa_field_manager` established. If no such capture exists, a simpler check: wrap `scale_wps_class` in a `tracing_test::traced_test` and assert the `warn!` fired (`logs_contain("name matches {wps}-{class} but has no WPS ownerRef")`).

### T4 — `feat(controller):` prune stale WPS children (OR defer with re-tag)

**Option A (implement prune, preferred):** extend `apply()` in [`workerpoolset/mod.rs:114-144`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) — after the SSA-apply loop, list children by ownerRef and delete any whose `size_class` (or name-suffix) isn't in `spec.classes`:

```rust
// r[impl ctrl.wps.prune-stale]
// Prune: WPS-owned children whose class was removed from
// spec.classes. The orphan scenario: operator deletes a class →
// the standalone-scaler skips the child (has ownerRef), the
// per-class loop skips it (not in spec.classes iteration) →
// NEITHER scales it. Delete it instead.
//
// List by ownerRef (not name-prefix — an operator could rename
// the WPS; child names wouldn't follow, but ownerRef UID does).
let children = wp_api
    .list(&ListParams::default())
    .await?
    .into_iter()
    .filter(|p| is_wps_owned_by(p, &wps));  // checks ownerRef UID == wps.uid

let active_classes: std::collections::BTreeSet<&str> =
    wps.spec.classes.iter().map(|c| c.name.as_str()).collect();

for child in children {
    if !active_classes.contains(child.spec.size_class.as_str()) {
        info!(
            child = %child.name_any(),
            class = %child.spec.size_class,
            "pruning stale WPS child (class removed from spec.classes)"
        );
        match wp_api.delete(&child.name_any(), &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}  // already gone
            Err(e) => warn!(child = %child.name_any(), error = %e, "prune delete failed"),
        }
    }
}
```

Add helper `is_wps_owned_by(pool, wps) -> bool` that checks `ownerReferences[].uid == wps.metadata.uid` (stronger than `is_wps_owned`'s kind-check — that would prune children of a DIFFERENT WPS in the same namespace).

**Option B (defer):** update T2's TODO tag to point at a new successor plan number, add a stub note in that plan doc. Choose B if the apply-loop is already heavy (it makes an SSA call per class; adding a list+filter+delete pass doubles the apiserver chatter per reconcile).

Implementer's call; prefer A (it's ~40L and closes the orphan half definitively).

## Exit criteria

- `/nbr .#ci` green
- `grep 'is_wps_owned(child)' rio-controller/src/scaling.rs` → ≥1 hit in `scale_wps_class` body (T1 gate present)
- `grep 'name matches.*no WPS ownerRef' rio-controller/src/scaling.rs` → ≥1 hit (T1 warn! message)
- `grep 'TODO(P0374)\|TODO(P0' rio-controller/src/reconcilers/workerpoolset/mod.rs` → ≥1 hit (T2 tagged — OR 0 if T4-option-A landed and the TODO was deleted)
- T3: `cargo nextest run -p rio-controller scale_wps_class_skips_name_collision_without_ownerref` → pass
- T4-option-A: `grep 'r\[impl ctrl.wps.prune-stale\]' rio-controller/src/reconcilers/workerpoolset/mod.rs` → 1 hit (marker added + prune block present)
- T4-option-A: `grep 'is_wps_owned_by' rio-controller/src/scaling.rs rio-controller/src/reconcilers/workerpoolset/mod.rs` → ≥2 hits (helper defined + used)
- T4-option-A mutation check: manually create a WPS, apply it, delete one class from `spec.classes`, re-apply → child pool gone within one reconcile cycle (VM-level or mock-api level)
- `nix develop -c tracey query rule ctrl.wps.autoscale` shows ≥2 `verify` sites (P0234-T3's managedFields + T3's name-collision)

## Tracey

References existing markers:
- `r[ctrl.wps.autoscale]` — T1 implements (tightens the existing impl with is_wps_owned gate); T3 verifies (name-collision skip)
- `r[ctrl.wps.reconcile]` — T4-option-A implements (prune-stale is part of the reconcile flow)

Adds new markers to component specs:
- `r[ctrl.wps.prune-stale]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions below) — only if T4-option-A

## Spec additions

**T4-option-A — new `r[ctrl.wps.prune-stale]`** (goes to [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after `r[ctrl.wps.autoscale]` at `:129`, standalone paragraph, blank line before, col 0):

```
r[ctrl.wps.prune-stale]
The WPS reconciler prunes child WorkerPools whose `size_class` no longer appears in `spec.classes`. Prune matches by ownerRef UID (not name-prefix — robust to WPS renames); deletes are best-effort (404-tolerant, logged on other errors, non-fatal so a stuck child doesn't wedge the whole reconcile). Without prune, a removed-class child is orphaned: the standalone autoscaler skips it (has ownerRef), the per-class autoscaler skips it (not in `spec.classes` iteration) — neither scales it.
```

## Files

```json files
[
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T1: is_wps_owned gate in scale_wps_class after name-match :515-521 (p234 ref); T3: flap regression test near :1266"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/mod.rs", "action": "MODIFY", "note": "T2: tag prune TODO :34-40; T4-A: prune-stale block in apply() after SSA loop :129"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4-A: +r[ctrl.wps.prune-stale] marker after :129 (only if option-A)"}
]
```

```
rio-controller/src/
├── scaling.rs                        # T1: gate + T3: test
└── reconcilers/workerpoolset/
    └── mod.rs                        # T2: TODO tag + T4-A: prune
docs/src/components/
└── controller.md                     # T4-A: new marker
```

## Dependencies

```json deps
{"deps": [234], "soft_deps": [372], "note": "T1-T4 all depend on P0234 (scale_wps_class + is_wps_owned + per-class loop exist). Soft-dep P0372 (migrate_finalizer resourceVersion lock — touches workerpool/mod.rs:153-189 same crate different module; non-overlapping). discovered_from=234-review (rev-p234). The flap is a real operator-facing bug the moment P0234 lands: a pre-existing pool named {wps}-{class} gets flapped. The orphan is latent (needs operator to remove a class after initial apply — less common but leaves a zombie STS consuming resources). T4-option-A adds r[ctrl.wps.prune-stale] to controller.md — marker-first discipline: this planner run adds the marker IF T4-A is chosen at dispatch (otherwise the marker stays staged here and T2's TODO points at a successor). Line refs are p234 worktree — re-grep at dispatch."}
```

**Depends on:** [P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) — `scale_wps_class` at `scaling.rs:501`, `is_wps_owned` at `:945`, per-class loop at `:291-299` all arrive with it.
**Conflicts with:** [`scaling.rs`](../../rio-controller/src/scaling.rs) also touched by P0234 itself (this plan LANDS AFTER, edits the p234-added code). [`workerpoolset/mod.rs`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) count=low (P0233-landed, P0234 extends, this tightens).
