# DONE-Plan Retrospective Decisions — 2026-03-19

Findings from `.claude/notes/done-plan-retrospective.md` + user validation.
12 new plans (P0285-P0296) + 2 UNIMPL-plan edits (P0276, P0284).

## Correctness (4 findings → 3 plans)

| # | Finding | **Decision** | Plan |
|---|---|---|---|
| P0129 | `DrainWorker(force=true)` has zero callers; 4 comments lie | **Pod watcher on DisruptionTarget** — make the comments true, not rewrite them. Controller watches pods with `status.conditions[type=DisruptionTarget,status=True]` filtered to workerpool-owned → calls DrainWorker{force:true}. | **P0285** MED |
| P0007 | `privileged:true` default, fabricated deferral authority, P0284 T4 would remove warning | **Full scope: device-plugin + hostUsers:false + default flip.** `values.yaml:382` lying comment fixed. **GATES P0284 T4.** | **P0286** HIGH |
| P0151 | gateway→scheduler trace linkage never worked (orphan spans) | **Fix it** — option (b) from `observability.nix:279`: scheduler returns trace_id in SubmitBuild initial metadata, gateway emits in STDERR_NEXT. Proto change is metadata-only. | **P0287** MED |
| P0188 | `worker.cgroup.kill-on-teardown` untested; spec fully written, never ported | Batched into P0289 (shared pattern with P0194, P0200) | → P0289 |

## Perf (1)

| # | Finding | **Decision** | Plan |
|---|---|---|---|
| P0089 | Bloom saturation: zero observability, metrics point wrong way | **Gauge + configurable const.** `fill_ratio()` + `rio_worker_bloom_fill_ratio` gauge + `worker.toml bloom_expected_items`. Alert threshold ~0.5. | **P0288** LOW |

## Tech-debt (6 findings → 3 plans + 1 direct edit)

| # | Finding | **Decision** | Plan |
|---|---|---|---|
| P0188+P0194+P0200 | Spec'd-but-unported trio (all have test fragments written in remediation docs) | **One batch plan.** Port cgroup.kill-on-teardown (09-doc:605), shutdown.sigint (15-doc:382), fuse.passthrough (21-doc:66) to lifecycle.nix + fuse/mod.rs. | **P0289** MED |
| P0020+P0191 | from_utf8_lossy reintroduced 1 day post-elimination; tx-commit debug_assert never landed | **Fix + clippy disallowed-methods** (prevents round 3). tx-commit: 8-line `#[cfg(debug_assertions)]` loop from 12-doc:1091. | **P0290** LOW |
| P0027 | is_cutoff dead column; P0276 would ship always-FALSE proto field | **Edit P0276 directly** (like Batch A). Strip `is_cutoff` from GraphEdge message + query. Migration DROP goes in P0291. | edit P0276 + **P0291** LOW |
| P0116 | Build CRD single-node; "Phase 4 deferral" comment misleading | **FULL RIP** — user doesn't want the feature. ~1500 LoC deletion. See §Build CRD below. | **P0294** HIGH |

## Doc-rot (11 findings → 3 plans)

| # | Finding | **Decision** | Plan |
|---|---|---|---|
| P0128 | `main.rs:416` CRITICAL comment says "fix manifest to gRPC probe" — doing so crash-loops standby | **Standalone plan** — this is a landmine, not doc-rot. Ships immediately. | **P0292** LOW |
| P0160 | 5 sites claim `link_parent()` = same trace_id. VM-proven false. | **Standalone, soft-dep on P0287.** If P0287 lands first, the claim becomes true again (no fix needed). Otherwise: rewrite 5 sites to "LINKED not parented." | **P0293** LOW |
| 9 remaining | README dead attrs, branch-cov warning, remediation index stale, etc. | **Batch sweep.** All comment/doc updates, no behavior change. | **P0295** LOW |

## User-driven additions (2)

### Build CRD rip (P0294) — escalated from P0116 finding

User doesn't want K8s-native `kubectl apply -f build.yaml` builds. Landed phase3a (`64455fb8`), never asked for.

**Scope:**
- DELETE `rio-controller/src/crds/build.rs` (~250 LoC)
- DELETE `rio-controller/src/reconcilers/build.rs` (~1200 LoC)
- Unwire from `main.rs`, `lib.rs`
- Migration to drop CRD from clusters (v1alpha1 Build kind)
- `lifecycle.nix` retargets:
  - `cancel-cgroup-kill` (currently deletes Build CR → cancel) → trigger via gRPC `CancelBuild` directly. Tests same code path.
  - `watch-dedup` (tests reconciler dedup) → DELETE (feature-specific)
- **P0238 scope SHRINKS:** drop CRD condition writes. Keep `BuildEvent::InputsResolved` proto event (useful for gateway path regardless).
- **P0270 scope SHRINKS:** drop CRD mirror. Keep proto+scheduler (dashboard reads gRPC stream; kubectl path dies with CRD).
- `docs/src/components/controller.md` loses Build CRD section
- `r[ctrl.crd.build]` + `r[ctrl.build.sentinel]` tracey markers removed from spec

**Ordering:** P0294 BEFORE P0289 (the cancel-cgroup-kill retarget feeds into the test-port batch).

### Ephemeral builders opt-in (P0296) — NEW feature request

User: "crucial feature for security that users need to be able to opt into."

**The shape:** `WorkerPoolSpec.ephemeral: bool` (default false). When true:
- Controller deploys as **Job-per-assignment**, not StatefulSet
- Worker runtime exits after one build (no event loop)
- Scheduler treats ephemeral-pool workers as single-use: assign → wait for completion → worker deregisters → pod terminates → Job reaps
- No FUSE cache reuse (fresh pod = fresh cache)
- No bloom accumulation (obviates P0089 for ephemeral pools — but STS pools still need the gauge)

**Security value:** zero cross-build contamination. Untrusted tenants can't leave behind poisoned cache entries for the next build.

**Tradeoffs:**
- Cold-start cost per build (container pull, FUSE mount, scheduler registration)
- No locality (W_LOCALITY becomes meaningless for ephemeral pools)
- Pod churn (k8s API load; Karpenter might thrash)

**Related:** P0286 (privileged hardening) + P0296 (ephemeral) together are the multi-tenant security story.

## UNIMPL plan edits (direct, like Batch A)

### P0284 — add P0286 dep

`T4: REMOVE introduction.md:50 warning` gets entry criterion: "P0286 merged — privileged default flipped." Phase5 close blocks on security hardening.

### P0276 — strip is_cutoff

Remove `bool is_cutoff` from `GraphEdge` proto message. Remove from the `SELECT` query. `GraphNode.status` already carries `"skipped"` (per P0252's `Skipped` variant) — edge flag is redundant.

### P0238 — drop CRD tasks (consequence of P0294)

Scope shrinks to: `BuildEvent::InputsResolved` proto event + scheduler fires it. No CRD condition writes.

### P0270 — drop CRD mirror (consequence of P0294)

Scope shrinks to: proto (`Progress` event fields) + scheduler computation. Dashboard reads gRPC. kubectl path dies.

## Full plan seed table

| # | Title | Complexity | Deps | Blocks |
|---|---|---|---|---|
| P0285 | DrainWorker DisruptionTarget watcher | MED | 204 | — |
| P0286 | Privileged hardening (device-plugin + hostUsers + flip) | HIGH | 204 | **P0284 T4** |
| P0287 | Trace linkage via SubmitBuild metadata | MED | 204 | P0293 (soft) |
| P0288 | Bloom fill_ratio gauge + configurable const | LOW | 204 | — |
| P0289 | Port 3 spec'd-but-unlanded test fragments | MED | 294 | — |
| P0290 | from_utf8 strict + clippy disallowed + tx-commit assert | LOW | 204 | — |
| P0291 | is_cutoff migration DROP | LOW | 276 | — |
| P0292 | P0128 CRITICAL-comment landmine fix | LOW | 204 | — |
| P0293 | P0160 5-site link_parent doc fix (soft-dep P0287) | LOW | 204 | — |
| P0294 | Build CRD full rip (~1500 LoC) | HIGH | 204 | P0238/P0270 scope; P0289 sequencing |
| P0295 | Doc-rot batch sweep (9 errata) | LOW | 204 | — |
| P0296 | Ephemeral builders opt-in (security) | HIGH | 204 | — |
