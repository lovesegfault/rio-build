# Plan 0347: Ephemeral pool-matching queue depth + exit semantics — 3 correctness gaps from P0296 review

[P0296](plan-0296-ephemeral-builders-opt-in.md) post-PASS review surfaced three gaps in the ephemeral-mode implementation. Two are correctness issues that can leak Jobs/pods indefinitely; one is speculative proto that should be removed.

**Gap 1 — cluster-wide queue depth triggers wrong-pool spawns.** [`ephemeral.rs:155`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) (p296 worktree ref) decides spawn count from `ClusterStatusResponse.queued_derivations` — the **cluster-wide** queued count, not pool-matching depth. A queue full of `x86_64-linux` builds on a `aarch64-darwin` ephemeral pool triggers Job spawn; the spawned worker heartbeats in, never matches dispatch (wrong `system`), and hangs indefinitely. No idle-timeout; no `activeDeadlineSeconds`. Same limitation the persistent autoscaler acknowledges at [`scaling.rs:189-195`](../../rio-controller/src/reconcilers/workerpool/scaling.rs) ("per-pool wired in phase4 WorkerPoolSet"), but ephemeral cost is a **leaked Job+pod**, not just an idle STS replica.

**Gap 2 — `maxConcurrentBuilds>1` breaks single-shot exit.** [`rio-worker/src/main.rs:652-670`](../../rio-worker/src/main.rs) (p296 ref) spawns a NEW exit-watcher per-assignment, each waiting for `acquire_many(max)`. With `max=4`, the worker exits after ALL 4 concurrent builds complete in one round — NOT after one build. If the scheduler dispatches N<max, then M more before the first N complete, watchers compete and the worker may never exit (permit churn keeps `acquire_many(max)` starved). CEL enforces `ephemeral → replicas.min==0` but NOT `ephemeral → maxConcurrentBuilds==1`. The doc comment says "worker exits after one build" — that's only true for `max=1`.

**Gap 3 — `CreateEphemeralWorker` is dead proto.** [`admin.proto:53-61`](../../rio-proto/proto/admin.proto) + [`types.proto:956-975`](../../rio-proto/proto/types.proto) landed as speculative machinery (comment: "Currently declaration-only... future latency-sensitive path"). Zero implementers, zero callers. Same orphan class as [P0338](plan-0338-tenant-signer-wiring-putpath.md)'s `TenantSigner` (resolved there). [`ephemeral.rs:22-37`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) rationale explicitly says poll-driven is the chosen architecture — the push RPC contradicts the design.

## Entry criteria

- [P0296](plan-0296-ephemeral-builders-opt-in.md) merged — ephemeral.rs, worker single-shot gate, and the dead proto exist

## Tasks

### T1 — `fix(controller):` ephemeral spawn — idle-timeout backstop via activeDeadlineSeconds

MODIFY [`rio-controller/src/reconcilers/workerpool/ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs). Three fix options were considered:

- **(a) per-class queue depth in ClusterStatus** — correct long-term fix but requires proto change + scheduler work + autoscaler benefits too. Out of scope here (noted for phase5).
- **(b) worker-side idle-timeout** — "exit if no assignment within N heartbeats" in `rio-worker/src/main.rs`. Requires threading heartbeat counter into the ephemeral exit-watcher.
- **(c) Job `activeDeadlineSeconds` backstop** — simplest: the Job spec gets a deadline; if the worker doesn't complete within N seconds, k8s kills the pod.

**Choose (c) for this plan** — it's a one-field addition to the Job builder, doesn't require coordinated worker changes, and k8s enforces it regardless of worker state. Set it to `cfg.ephemeral_deadline_seconds` (new WorkerPoolSpec field, default e.g. 3600 = 1h). The field goes alongside `maxConcurrentBuilds` in the CRD.

```rust
// In the Job spec builder (near where ttl_seconds_after_finished is set):
spec: Some(JobSpec {
    active_deadline_seconds: Some(pool.spec.ephemeral_deadline_seconds.unwrap_or(3600) as i64),
    ttl_seconds_after_finished: Some(300),
    // ...
})
```

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — add `ephemeral_deadline_seconds: Option<u32>` to `WorkerPoolSpec` with a CEL rule gating it to ephemeral mode only (`self.ephemeral == true || !has(self.ephemeralDeadlineSeconds)`).

Add `// r[impl ctrl.pool.ephemeral-deadline]` at the Job spec field.

### T2 — `fix(controller):` CEL — `ephemeral → maxConcurrentBuilds == 1`

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs). Add a CEL validation rule alongside the existing `ephemeral → replicas.min == 0` rule:

```rust
#[schemars(schema_with = "cel_rules")]
// ... existing rules ...
// r[impl ctrl.pool.ephemeral-single-build]
"!self.ephemeral || self.maxConcurrentBuilds == 1",
// message: "ephemeral mode requires maxConcurrentBuilds=1 (worker exits
//   after one build round; N>1 breaks single-shot semantics, may never exit
//   under permit churn)"
```

This is simpler than reworking the exit-watcher to spawn once (option b) or using a `tokio::sync::Barrier` (option c from the review). The constraint is **operational** (ephemeral + concurrent-build doesn't make sense: the point is per-build isolation) so encoding it at the CRD validation layer is appropriate.

Also MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) — update the doc comment at the ephemeral exit-watcher block (`:652-670` p296 ref) to say "exits after one build **round** (maxConcurrentBuilds builds complete; CEL enforces max=1 in ephemeral mode)".

### T3 — `refactor(proto):` remove CreateEphemeralWorker dead RPC

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto) — delete the `CreateEphemeralWorker` RPC at `:53-61` (p296 ref) and reserve the RPC number. MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) — delete `CreateEphemeralWorkerRequest`/`CreateEphemeralWorkerResponse` at `:956-975` and reserve the field numbers.

Check at dispatch: `grep -rn 'CreateEphemeralWorker' rio-*/src/` — should be 0 callers/implementers. If any appeared since P0296 review, this task becomes a re-scope decision.

The rationale comment at `ephemeral.rs:22-37` explicitly chose poll-driven over push — the dead RPC contradicts the architecture doc. Removing it reduces proto surface and generated-code noise. If a latency-sensitive push path is ever needed, add it back with an actual implementer.

### T4 — `test(controller):` CEL rule assertions

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) `cel_rules_in_schema` test (same test P0311-T6 extends). Add two asserts:

```rust
// T1's rule
assert!(json.contains(r#""!self.ephemeral || has(self.ephemeralDeadlineSeconds)""#)
    || json.contains(r#""self.ephemeral == true || !has(self.ephemeralDeadlineSeconds)""#));
// T2's rule
assert!(json.contains(r#""!self.ephemeral || self.maxConcurrentBuilds == 1""#));
```

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) in the ephemeral subtest — add a negative `kubectl apply` asserting a WorkerPool with `ephemeral: true, maxConcurrentBuilds: 4` is rejected by the admission webhook. Copy the shape of the existing seccomp CEL-reject test.

## Exit criteria

- `/nixbuild .#ci` green
- T1: `grep 'active_deadline_seconds' rio-controller/src/reconcilers/workerpool/ephemeral.rs` → ≥1 hit
- T1: `grep 'ephemeral_deadline_seconds\|ephemeralDeadlineSeconds' rio-controller/src/crds/workerpool.rs` → ≥2 hits (field + CEL)
- T2: `grep 'maxConcurrentBuilds == 1' rio-controller/src/crds/workerpool.rs` → ≥1 hit
- T2: `grep 'one build round\|CEL enforces max=1' rio-worker/src/main.rs` → ≥1 hit (doc comment corrected)
- T3: `grep 'CreateEphemeralWorker' rio-proto/proto/admin.proto rio-proto/proto/types.proto` → 0 hits (removed) OR `grep 'reserved.*CreateEphemeralWorker' rio-proto/proto/admin.proto` → ≥1 (reserved comment remains)
- T3: `grep -rn 'CreateEphemeralWorker' rio-*/src/` → 0 hits (no generated-code consumers; prost regenerates cleanly)
- T4: `cargo nextest run -p rio-controller cel_rules_in_schema` → pass (with the new asserts)
- T4: lifecycle.nix ephemeral subtest — negative apply with `maxConcurrentBuilds: 4` → `Invalid value` in kubectl stderr
- `nix develop -c tracey query rule ctrl.pool.ephemeral-deadline` → shows `impl` site (T1 annotation)
- `nix develop -c tracey query rule ctrl.pool.ephemeral-single-build` → shows `impl` site (T2 annotation)

## Tracey

Adds two new markers to [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after the existing `r[ctrl.pool.ephemeral]` region (p296 adds that marker around `:60`; re-grep at dispatch):

- `r[ctrl.pool.ephemeral-deadline]` — T1 implements (Job activeDeadlineSeconds backstop)
- `r[ctrl.pool.ephemeral-single-build]` — T2 implements (CEL constraint)

T4 verifies `r[ctrl.pool.ephemeral-single-build]` via the `cel_rules_in_schema` assert and the lifecycle.nix negative apply.

No existing markers referenced beyond `r[ctrl.pool.ephemeral]` (P0296's parent marker — T1/T2 are sub-constraints of it; splitting to sub-markers per the P0296 reviewer's [`controller.md:60`](../../docs/src/components/controller.md) note).

## Spec additions

Add to [`docs/src/components/controller.md`](../../docs/src/components/controller.md) after P0296's `r[ctrl.pool.ephemeral]` marker (dispatch-time grep for exact line — p296 adds the marker, location unknown until merge):

```
r[ctrl.pool.ephemeral-deadline]
Ephemeral Jobs MUST set `spec.activeDeadlineSeconds` from `WorkerPoolSpec.ephemeralDeadlineSeconds` (default 3600). This is a backstop: if the spawned worker never receives a matching assignment (queue depth was for a different pool's size_class/system), k8s kills the pod at deadline instead of leaking indefinitely. Per-pool queue depth (the proper fix) is deferred to phase5's WorkerPoolSet.

r[ctrl.pool.ephemeral-single-build]
CRD validation MUST reject `ephemeral: true` with `maxConcurrentBuilds > 1`. The ephemeral exit-watcher spawns per-assignment and waits for `acquire_many(max)` permits; with max>1 under permit churn (scheduler dispatches N<max then M more before N completes), the watcher may never win the race and the worker never exits. Ephemeral mode's per-build isolation premise breaks with concurrent builds anyway.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T1: +active_deadline_seconds in Job spec builder; r[impl ctrl.pool.ephemeral-deadline]. p296 worktree ref :155 for spawn-decision context."},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "T1: +ephemeral_deadline_seconds field + CEL gate; T2: +CEL rule ephemeral→maxConcurrentBuilds==1 + r[impl ctrl.pool.ephemeral-single-build]; T4: +2 cel_rules_in_schema asserts"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T2: doc comment at :652-670 (p296 ref) — 'one build' → 'one build round (CEL enforces max=1)'"},
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T3: delete CreateEphemeralWorker RPC :53-61 (p296 ref), reserve number"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T3: delete CreateEphemeralWorkerRequest/Response :956-975 (p296 ref), reserve fields"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T4: negative kubectl apply for ephemeral+maxConcurrentBuilds=4 → CEL reject. Near the existing ephemeral-pool subtest (p296 ref ~:1780)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "Spec additions: +r[ctrl.pool.ephemeral-deadline] +r[ctrl.pool.ephemeral-single-build] after P0296's r[ctrl.pool.ephemeral] marker"}
]
```

```
rio-controller/src/
├── reconcilers/workerpool/ephemeral.rs  # T1: activeDeadlineSeconds
└── crds/workerpool.rs                    # T1+T2+T4: field + 2 CEL + asserts
rio-worker/src/main.rs                    # T2: doc comment correction
rio-proto/proto/
├── admin.proto                           # T3: delete dead RPC
└── types.proto                           # T3: delete dead messages
nix/tests/scenarios/lifecycle.nix         # T4: CEL-reject negative test
docs/src/components/controller.md         # Spec: 2 new markers
```

## Dependencies

```json deps
{"deps": [296], "soft_deps": [311, 285], "note": "HARD dep P0296: ephemeral.rs, the worker exit-watcher at main.rs:652-670, the dead proto, and the lifecycle.nix ephemeral subtest all arrive with it. discovered_from=296 (post-PASS review). T4 soft-conflicts P0311-T6: both extend cel_rules_in_schema asserts — same test fn, additive asserts, sequence-independent. T1 soft-conflicts P0285 on lifecycle.nix and main.rs (DrainWorker watcher) — different sections (ephemeral subtest vs drain subtest; exit-watcher block vs drain-watcher block). Option (a) per-class-queue-depth is explicitly deferred: it benefits the persistent autoscaler too (scaling.rs:189-195 acknowledges the gap), so it belongs in a phase5 ClusterStatus proto extension. T3 is a proto-delete: class-3 weak (field-delete from unused messages = no exhaustive-destructure break); nextest post-rebase proves."}
```

**Depends on:** [P0296](plan-0296-ephemeral-builders-opt-in.md) — the ephemeral reconciler, worker single-shot gate, and dead proto all land there. Review findings at [`ephemeral.rs:155`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs), [`main.rs:652`](../../rio-worker/src/main.rs), [`admin.proto:53`](../../rio-proto/proto/admin.proto) (all p296 worktree refs — re-grep at dispatch).

**Conflicts with:** [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) count=37 (HOT) — T2 is a doc-comment edit only at the ephemeral exit-watcher block; additive, low-risk. [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T6 adds seccomp CEL asserts to the same test fn; both additive. [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=18 — T4 adds to the ephemeral subtest that P0296 creates; no other UNIMPL plan edits that subtest. [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) count=33 (HOT) — T3 is a delete+reserve at `:956-975`; [P0287](plan-0287-trace-linkage-submitbuild-metadata.md) adds `x-rio-trace-id` metadata (no proto body field per the plan); non-overlapping. [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto) — [P0271](plan-0271-cursor-pagination-admin-builds.md) adds cursor fields to ListBuilds; non-overlapping sections.
