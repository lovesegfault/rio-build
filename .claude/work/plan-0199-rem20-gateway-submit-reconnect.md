# Plan 0199: Rem-20 — Gateway submit/reconnect gap: build_id in gRPC initial metadata

## Design

**P1 (HIGH).** Closes the SubmitBuild first-event/reconnect gap: scheduler SIGTERM between `MergeDag` commit and `bridge_build_events`' first send left the gateway with zero stream events and no `build_id` to `WatchBuild`. Client retries → zombie build (second `build_id` row, orphaned broadcast).

**Option B per remediation doc:** `build_id` in response initial metadata (headers). No `.proto` change — gRPC initial headers go out when the server handler returns `Ok(Response)`, BEFORE any stream message. Gateway reads `resp.metadata()` before `resp.into_inner()`; if present, inserts into `active_build_ids` immediately → reconnect loop covers event 0. If absent (legacy scheduler), falls back to the old first-event peek (preserved for deploy-order safety).

**Also fixes `gw-submit-build-bare-question-mark-no-stderr`:** all three `return Err` sites now emit `STDERR_NEXT` diagnostic before propagating. NOT `STDERR_ERROR` — callers at opcodes 36/46 convert `Err` to `BuildResult::failure` → `STDERR_LAST`; sending `ERROR` here would recreate rem-07's `ERROR`→`LAST` desync. (This item was in rem-21's §Remainder "hold for plan 07" — it landed here instead. See P0200 notes.)

**Controller follow-up (`4b10f32`):** controller's `apply()` had the same bug — `submit_build(req).await?.into_inner()` discarded the `x-rio-build-id` metadata; `drain_stream` spawned with `known_build_id=None`; empty stream → `is_empty()` check fires `clear_sentinel` → resubmit → zombie PG builds row. Same read-side pattern: `resp.metadata()` before `into_inner()`. Interaction with rem-03: `clear_sentinel` at the reconnect body becomes a LEGACY-ONLY fallback for pre-phase4a schedulers that don't set the header.

Remediation doc: `docs/src/remediations/phase4a/20-gateway-submit-reconnect.md` (662 lines).

## Files

```json files
[
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "BUILD_ID_HEADER const (\"x-rio-build-id\")"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "set x-rio-build-id in SubmitBuild response initial metadata"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "read resp.metadata() before into_inner(); insert active_build_ids immediately; STDERR_NEXT before return Err"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "same pattern: read header → known_build_id → no resubmit on empty stream"}
]
```

## Tracey

- `r[impl gw.reconnect.since-seq]` — `4775759` (second impl site)
- `r[verify gw.reconnect.backoff]` — `4775759`

2 marker annotations.

## Entry

- Depends on P0192: rem-13 (MockScheduler since_sequence — this tests against that mock)
- Depends on P0182: rem-03 (the controller half interacts with clear_sentinel)

## Exit

Merged as `4b0c79c` (plan doc, rider) + `4775759` + `4b10f32` (2 fix commits). `.#ci` green. Scheduler SIGTERM mid-submit: gateway reconnects from event 0; no zombie build.
