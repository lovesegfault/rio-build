# Plan 0135: WatchBuild reconnect — controller last_sequence + gateway backoff

## Design

Phase 3a's `WatchBuild` stream was one-shot: controller or gateway opened it, got events, and if the stream died (scheduler restart, network blip), the client was stuck. The Build CRD's status stopped updating; the gateway's nix client hung waiting for a build that would never report completion. This plan added reconnect-with-resume on both sides.

**Controller:** `BuildStatus` gets `last_sequence: i64` field (0 default). In `drain_stream`, each event's `ev.sequence` is captured before the status patch. On reconcile, if `status.build_id` is a real UUID (not empty, not the `"submitted"` sentinel): instead of `await_change()`, call `scheduler.watch_build(WatchBuildRequest { build_id, since_sequence: status.last_sequence })`, spawn `drain_stream` on the response, return `await_change()`. In `drain_stream`'s error arm: backoff-retry loop (5 attempts, 1s/2s/4s/8s/16s) — reconnect, `watch_build(build_id, last_seq_seen)`, `continue` outer loop on success. After 5 fails: warn + patch `status.phase = "Unknown"` + return.

The scheduler side already supported `since_sequence` in `WatchBuildRequest` (from phase 2b's log replay); this plan just made clients use it.

**Gateway (F3, landed in `ccc1ae1` which is tracked under P0134):** `active_build_ids: HashMap<String, u64>` already existed and was populated per-event. `process_build_events` on stream `Err`: backoff-retry loop (up to ~60s total, exponential capped at 16s) — reconnect `SchedulerServiceClient`, `watch_build(build_id, since=active_build_ids[id])`, resume. After timeout: existing `MiscFailure` path.

This plan is most useful AFTER state recovery (P0130) — without recovery, reconnecting to a fresh scheduler gets "unknown build" gracefully. With recovery, the scheduler reloads the build from PG and the reconnect picks up where it left off.

## Files

```json files
[
  {"path": "rio-controller/src/crds/build.rs", "action": "MODIFY", "note": "BuildStatus.last_sequence: i64"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "apply() reconnect gate + drain_stream backoff-retry loop"},
  {"path": "infra/base/crds.yaml", "action": "MODIFY", "note": "regenerated Build CRD with last_sequence"}
]
```

## Tracey

No tracey markers landed in this commit. The `r[impl gw.reconnect.backoff]` annotation for the gateway reconnect loop was added retroactively in `dadc70c` (P0141) when the spec rule was written during the round-4 docs catchup.

## Entry

- Depends on P0127: phase 3a complete (Build reconciler, `drain_stream`, `WatchBuildRequest.since_sequence`).
- Depends on P0130: state recovery makes reconnect meaningful (scheduler can answer `watch_build` after restart).

## Exit

Merged as `1bf36bc` (1 commit). `.#ci` green at merge. Controller reconnect test: mock scheduler errors once then succeeds → assert reconnect + continued status updates. Gateway reconnect tests landed later in P0145 (`test_build_paths_reconnect_on_transport_error`, `test_build_paths_reconnect_exhausted_returns_failure`).
