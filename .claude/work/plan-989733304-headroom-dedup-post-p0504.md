# Plan 989733304: headroom_multiplier deduplication — post-P0504 cleanup

After [P0504](plan-0504-scheduler-resource-fit-filter.md) merges,
`cfg.headroom_multiplier` is plumbed through TWO paths to the same
config-static value:

**Path A (P0501):** `cfg.headroom_multiplier` →
[`AdminServiceImpl.headroom_multiplier`](../../rio-scheduler/src/admin/mod.rs)
at `:95` → [`ActorCommand::CapacityManifest { headroom_mult, .. }`](../../rio-scheduler/src/actor/command.rs)
at `:269` → `compute_capacity_manifest(headroom_mult)` as a per-call
parameter.

**Path B (P0504):** `cfg.headroom_multiplier` → `DagActor.headroom_mult`
field + `with_headroom_mult()` builder → read at dispatch time at
[`dispatch.rs:115`](../../rio-scheduler/src/actor/dispatch.rs) via
`self.headroom_mult`.

P0504's worktree comment at `actor/mod.rs:+196` says "Must match the
manifest RPC headroom (both come from cfg.headroom_multiplier)" — it's
aware there are two copies. Once P0504 adds the `DagActor` field,
Path A's per-call parameter is redundant: `compute_capacity_manifest`
can read `self.headroom_mult` directly (it's a `&self` method on
`DagActor`).

Zero drift risk today (both read `cfg.headroom_multiplier` in `main.rs`).
The risk is future: someone adds a per-request headroom knob to
`ActorCommand::CapacityManifest` without touching the dispatch filter, or
vice versa. Two-copies-of-one-value is a footgun waiting for a trigger.

~15 lines net removal across 4 files.

## Entry criteria

- [P0501](plan-0501-capacity-manifest-rpc.md) merged (provides
  `ActorCommand::CapacityManifest { headroom_mult }` + `AdminServiceImpl`
  copy)
- [P0504](plan-0504-scheduler-resource-fit-filter.md) merged (provides
  `DagActor.headroom_mult` field + `with_headroom_mult()` builder)

## Tasks

### T1 — `refactor(scheduler):` drop ActorCommand::CapacityManifest headroom_mult field

MODIFY [`rio-scheduler/src/actor/command.rs:268-271`](../../rio-scheduler/src/actor/command.rs):

```rust
// BEFORE (P0501)
CapacityManifest {
    headroom_mult: f64,
    reply: oneshot::Sender<Vec<BucketedEstimate>>,
},

// AFTER
/// Bucketed resource estimates for ready-queue derivations
/// (ADR-020 capacity manifest). Headroom applied from
/// `self.headroom_mult` (config-static, same value the dispatch
/// filter uses at `dispatch.rs` — one source of truth post-P989733304).
/// Cold-start derivations (no `build_history` sample) are omitted —
/// controller uses its operator floor.
///
/// `send_unchecked`: the controller polls this to size the builder
/// fleet. Same blinding-under-load rationale as above.
CapacityManifest {
    reply: oneshot::Sender<Vec<BucketedEstimate>>,
},
```

The match arm in `actor/mod.rs` that destructures `CapacityManifest {
headroom_mult, reply }` drops the `headroom_mult` binding;
`compute_capacity_manifest` reads `self.headroom_mult` instead of the
parameter.

### T2 — `refactor(scheduler):` drop AdminServiceImpl.headroom_multiplier copy

MODIFY [`rio-scheduler/src/admin/mod.rs:92-95`](../../rio-scheduler/src/admin/mod.rs).
Delete the field + its `new()` parameter + the call-site argument in
`main.rs`. The `get_capacity_manifest` handler sends
`ActorCommand::CapacityManifest { reply }` (no headroom param after T1).

P0504's comment at `actor/mod.rs:+196` ("Must match the manifest RPC
headroom") becomes a pointer to the single source:

```rust
/// ADR-020 capacity manifest headroom. Applied by both
/// `compute_capacity_manifest` (manifest RPC) and the dispatch-time
/// resource-fit filter. Config-global; per-pool later if needed.
/// Validated finite + positive at startup (main.rs).
```

### T3 — `refactor(scheduler):` main.rs plumbs to ONE place

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) near
the `AdminServiceImpl::new(...)` call and the actor-builder chain.
`cfg.headroom_multiplier` appears ONCE: in
`.with_headroom_mult(cfg.headroom_multiplier)` on the actor builder. The
`AdminServiceImpl::new(...)` argument is removed (T2).

### T4 — `test(scheduler):` compute_capacity_manifest reads self.headroom_mult

The existing capacity-manifest tests (from
[P0501](plan-0501-capacity-manifest-rpc.md)) pass a `headroom_mult`
parameter. After T1 they construct a `DagActor` with
`.with_headroom_mult(1.25)` (or whatever the test used). Update the test
setup; assertions unchanged.

Add one new test proving the dispatch filter and the manifest RPC agree:

```rust
#[test]
fn dispatch_and_manifest_use_same_headroom() {
    // P989733304 regression guard: before deduplication, headroom was
    // plumbed via ActorCommand param (manifest) AND DagActor field
    // (dispatch). Now both read self.headroom_mult. This test ensures
    // a future per-request knob can't bypass one path.
    let actor = DagActor::builder()
        .with_headroom_mult(1.5)
        /* ... minimal test setup ... */
        .build();
    // Construct a derivation with known EMA, compute manifest bucket,
    // compute dispatch est_memory_bytes, assert both used 1.5×.
    // Exact assertion depends on the Estimator test-injection API
    // (test_inject_ready + test_refresh_estimator per P0501).
}
```

Lean on P0501's existing test helpers — this is a delta, not new
infrastructure.

## Exit criteria

- `grep 'headroom_mult' rio-scheduler/src/actor/command.rs` → 0 hits in `CapacityManifest` variant definition (T1 dropped the field)
- `grep 'headroom_multiplier' rio-scheduler/src/admin/mod.rs` → 0 hits (T2 dropped the copy)
- `grep -c 'cfg.headroom_multiplier' rio-scheduler/src/main.rs` == 1 (T3: plumbed to exactly one place — the actor builder)
- `cargo nextest run -p rio-scheduler capacity_manifest` — all pre-existing tests pass with updated setup
- `cargo nextest run -p rio-scheduler dispatch_and_manifest_use_same_headroom` passes (T4)
- Net line delta: `git diff --shortstat` shows ≥10 deletions > insertions (this is a removal, not an addition)
- `/nixbuild .#ci` green

## Tracey

References existing markers:
- `r[sched.admin.capacity-manifest]` — T1-T3 touch the implementation
  under this marker at
  [`scheduler.md:164`](../../docs/src/components/scheduler.md). No
  annotation change: the RPC contract (returns bucketed estimates with
  headroom applied) doesn't change, only the internal plumbing.
- `r[sched.admin.capacity-manifest.bucket]` — the "EMA × headroom_multiplier"
  clause at [`scheduler.md:168`](../../docs/src/components/scheduler.md)
  is still true; now there's one `headroom_multiplier` not two.

No new markers. Deduplication is invisible at the spec level.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "T1: drop headroom_mult field from CapacityManifest variant at :269"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "T1: match arm drops headroom_mult binding; compute_capacity_manifest reads self.headroom_mult; T2: update P0504's :+196 comment to name single-source. HOT — count=37, but this is a small subtractive change in the match arm"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T2: delete headroom_multiplier field at :95 + new() param + get_capacity_manifest handler send-site"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T3: drop AdminServiceImpl::new headroom arg; cfg.headroom_multiplier → actor builder ONLY. HOT — additive-removal only"},
  {"path": "rio-scheduler/src/actor/misc.rs", "action": "MODIFY", "note": "T4: update capacity-manifest test setup (headroom via builder not param) + add dispatch_and_manifest_use_same_headroom"}
]
```

```
rio-scheduler/src/
├── actor/
│   ├── command.rs    # T1: drop CapacityManifest.headroom_mult
│   ├── mod.rs        # T1: match arm + compute_capacity_manifest sig
│   └── misc.rs       # T4: test updates
├── admin/mod.rs      # T2: drop field + param
└── main.rs           # T3: plumb to one place
```

## Dependencies

```json deps
{"deps": [501, 504], "soft_deps": [], "note": "HARD-GATED on P0504 — DagActor.headroom_mult doesn't exist until P0504 merges. P0504 is in-flight. Do NOT dispatch this until P0504 is DONE (rebase collision otherwise: this removes what P0504 is about to add a sibling to). discovered_from=consolidator."}
```

**Depends on:**
- [P0501](plan-0501-capacity-manifest-rpc.md) — introduced
  `ActorCommand::CapacityManifest { headroom_mult }` + `AdminServiceImpl`
  copy (Path A).
- [P0504](plan-0504-scheduler-resource-fit-filter.md) — introduces
  `DagActor.headroom_mult` + builder (Path B). **GATING** — this plan
  removes Path A only AFTER Path B exists.

**Conflicts with:** `rio-scheduler/src/actor/dispatch.rs` is in
`onibus collisions top 30` (count=29). This plan does NOT touch
`dispatch.rs` directly (`dispatch.rs:115` reads `self.headroom_mult`
which stays), but `actor/mod.rs` (count=37 per prior batch-append
metadata) IS hot. Serialize behind P0504 (hard dep anyway); the
subtractive change in the match arm is small enough that rebase on top
of anything else touching `mod.rs` is mechanical.
