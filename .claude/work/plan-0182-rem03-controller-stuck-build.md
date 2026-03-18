# Plan 0182: Rem-03 — Controller stuck-Build cluster (sentinel clear, status preserve, Unknown requeue)

## Design

**P0 — stuck Build CRs require manual `kubectl edit` to unstick.** All seven findings share one root: **a fire-and-forget `tokio::spawn` patches nothing on a CR whose reconciler already returned `Action::await_change()`.** With no `.owns()` watch and no periodic requeue, a spawned task that exits without mutating the CR leaves it stuck forever.

Causal chain:
```
SubmitBuild stream drops before event 0
 └─ drain_stream: build_id.is_empty() → bare `return`  ← #1
      └─ no status patch → no watch event → apply() never runs again  ← STUCK

spawn_reconnect_watch exhausts MAX_RECONNECT
 └─ patches phase=Unknown with ..Default::default()  ← #2, #3
      └─ Unknown is non-terminal → apply() spawns reconnect again  ← #5 (31s loop)

drain_stream WatchBuild Err
 └─ stream not replaced, next iter reads dead stream → Ok(None) → +1 attempt  ← #4
      └─ effective budget = MAX_RECONNECT / 2  ← #6
```

**Fix:** sentinel clear on empty-stream (patches a sentinel value that apply() recognizes as "retry"); status preserve on Unknown patch (don't clobber existing build_id with `..Default::default()`); Unknown requeue with backoff (not immediate loop); stream replacement on Err before next iter.

Remediation doc: `docs/src/remediations/phase4a/03-controller-stuck-build.md` (1099 lines, 7 findings). The doc's §Remainder mentioned `ctrl-connect-store-outside-main` would land here — it did NOT (see P0200 partial-landing notes).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "sentinel clear on empty-stream; status preserve on Unknown; Unknown requeue with backoff; stream replace on Err"},
  {"path": "rio-controller/src/crds/build.rs", "action": "MODIFY", "note": "BuildStatus sentinel field"}
]
```

## Tracey

No new markers — covered by existing `r[ctrl.build.sentinel]` and `r[ctrl.build.reconnect]` (both already verified via P0179).

## Entry

- Depends on P0177: tonic reconnect class (the stream-drop handling shares the same reconnect-worthy classification)

## Exit

Merged as `35171c7` (plan §3) + `6591ffc` (fix). `.#ci` green. Remediation doc §7 follow-up (controller reads `x-rio-build-id` header) landed separately in P0199.
