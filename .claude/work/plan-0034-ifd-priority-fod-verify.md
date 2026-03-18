# Plan 0034: IFD priority + FOD verification + running_builds heartbeat tracker

## Context

Three phase2a.md task items landed here:

1. **IFD handling** — "wopBuildDerivation during evaluation treated as normal build request, prioritized in scheduler." When Nix evaluates `import (fetchTarball ...)`, it sends `wopBuildDerivation` mid-eval. That's IFD. The gateway tags it `priority_class="interactive"` (P0030's detection flag). This plan makes the scheduler honor that: interactive builds `push_front` the ready queue, not `push_back`.

2. **FOD verification** — Fixed-Output Derivations declare their `outputHash` upfront. Nix-daemon verifies it, but defense-in-depth says the worker should too. After upload, compute the NAR hash (for `r:sha256`) or flat file hash (for plain `sha256`), compare to the declared `outputHash`, return `OutputRejected` on mismatch.

3. **running_builds tracking** — the heartbeat should report which derivations the worker is currently building. `Arc<RwLock<HashSet<String>>>` shared between the build task (insert on start, remove on exit via scopeguard) and the heartbeat loop.

Plus doc corrections: the `assignment_token` proto comment claimed HMAC-signed; it's a plain UUID in 2a.

## Commits

- `d45fc19` — feat(rio-scheduler): IFD priority, FOD verification, running_builds tracking
- `b5882ab` — docs(rio-proto): fix misleading comments and remove dead code

## Files

```json files
[
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "should_prioritize() checks if any interested build is interactive; push_front on initial merge AND downstream release"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "verify_fod_hashes(): r:sha256 → compare NAR hash; flat sha256 → read file from overlay upper, hash contents; mismatch → OutputRejected"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Arc<RwLock<HashSet<String>>> running_builds; inserted before spawn, removed via scopeguard on exit"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "build_heartbeat_request() extracted for testability"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "assignment_token comment: clarify NOT cryptographically signed in Phase 2a (was: false HMAC claim)"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "drv_hash fallback uses drv_path (unique) instead of empty string; upgrade resolve_derivation failure warn!→error!"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "delete dead cache_drv_if_needed (buffered path always used now)"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "comment fixes: describe actual placeholder-row pattern, not vague 'review finding' refs"}
]
```

## Design

**IFD priority:** `should_prioritize(drv_hash)` checks every build in `node.interested_builds` — if any has `priority_class == "interactive"`, the derivation goes to the front. Applied in two places: initial `compute_initial_states` (for newly-merged ready nodes) and `handle_success_completion` (when releasing downstream dependents). Phase 2a deliberately binary (interactive vs. not) — full `max(priority)` across interested builds deferred to phase 2c.

**FOD verification:** `verify_fod_hashes()` iterates `drv.outputs`. For each with a non-empty `hash` field (FOD marker): if `hashAlgo.starts_with("r:")`, it's recursive — compare the upload's computed NAR hash. Else it's flat — read the single file at `overlay_upper/nix/store/{path}`, SHA-256 its contents. On mismatch: `BuildResultStatus::OutputRejected` (permanent, not retriable). Called after `upload_all_outputs`, before returning `Built`.

**running_builds tracker:** `scopeguard::defer!` ensures the `HashSet::remove` runs on all exit paths (success, error, panic). The heartbeat loop reads the set every interval and includes it in `HeartbeatRequest`. The scheduler uses this for reconciliation in P0039 (detect drift between scheduler-tracked and worker-reported).

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.dispatch.ifd-priority]`, `r[worker.fod.verify]`, `r[worker.heartbeat.running-builds]`.

## Outcome

Merged as `d45fc19..b5882ab` (2 commits). Closes review findings C3, C4, C5, C6, I3, I4, S8, S9, S10. TDD tests: interactive-push-front assignment order, FOD recursive/flat ok/mismatch, non-FOD skip, heartbeat reflects tracker.
