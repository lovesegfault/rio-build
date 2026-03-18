# Plan 0162: Round 6 — hard-hang: poisoned-stub zombie on recovery → ClearPoison resubmit hangs forever

## Design

Final branch-review. Found a **hard-hang regression** introduced by phase-4a's own `load_poisoned_derivations` (new in P0155).

**The bug (`7078da2`):** `load_poisoned_derivations` inserts recovered nodes with stub fields (`output_names=[]`, `expected_output_paths=[]`). `dag.merge()` on an existing node only touches `interested_builds`+`traceparent` (never refreshes stubs), and `compute_initial_states` only iterates `newly_inserted` — so `reset_from_poison`'s Poisoned→Created transition left a zombie. Scenario: scheduler restarts → recovery loads poisoned X with stub fields → user retries → build stuck at `completed=0, failed=0, total=1` forever. Pre-4a, `TERMINAL_STATUSES` filtered poisoned rows out of recovery so restart simply forgot them.

**Fix:** ClearPoison/TTL now `remove_node()` instead of resetting in-place — next merge inserts fresh with real fields. `reset_from_poison` deleted (both callers replaced). Regression test poisons on actor A, recovers on actor B, clears, resubmits, asserts dispatch reaches worker.

**Latent same-lifetime bug (`c03d527`):** separately found the existing-node merge loop only checked `== Completed`; a single-node resubmit of a still-Poisoned drv (no new dependent to carry the `first_dep_failed` signal via `compute_initial_states`) also hung. Extended the match to `Poisoned | DependencyFailed`.

**Layering (`8bb63d8`):** `query_unchecked` now returns `ActorError` — round 4 had put `tonic::Status` in the actor module; also had divergent channel-closed mapping vs `actor_error_to_status`.

**Deleted `EMIT_TRACE_ID` OnceLock (`c89a73d`):** docstrings claimed tests set `RIO_EMIT_TRACE_ID=false` but none ever did; the adjacent `!trace_id.is_empty()` guard was already the gate. Speculative config never used.

**Moved `BuildListRow`+SQL into `SchedulerDb::list_builds` (`e6ec34f`):** mirrors `list_tenants`.

Polish: worker status mapping deduped (`b67cc16`); Bearer scheme case-insensitive per RFC 7235 (`4c57313`).

## Files

```json files
[
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "remove_node() on clear/TTL instead of reset_from_poison; merge match extended to Poisoned | DependencyFailed"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "reset_from_poison DELETED"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "ClearPoison calls remove_node not reset_from_poison"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "TTL-expiry calls remove_node"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "regression test: poison A → recover B → clear → resubmit → dispatch reaches worker"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "query_unchecked returns ActorError not tonic::Status"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "delete EMIT_TRACE_ID OnceLock"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "BuildListRow+SQL moved into SchedulerDb::list_builds"},
  {"path": "rio-scheduler/src/admin/workers.rs", "action": "MODIFY", "note": "dedup worker status mapping"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "case-insensitive Bearer scheme (RFC 7235)"}
]
```

## Tracey

No new markers. This round is the final hard-hang fix + polish.

## Entry

- Depends on P0161: round 5 (fixes P0155's recovery path properly — round 4 and round 5 fixed symptoms, this fixes the root)

## Exit

Merged as `7078da2..546b7fb` (8 commits). `.#ci` green. Phase doc round-6 summary: "7078da2..4c57313, +7 commits". **Phase 4a "6 rounds" complete** — 303 commits on `phase-4a-dev` per phase doc line 70.
