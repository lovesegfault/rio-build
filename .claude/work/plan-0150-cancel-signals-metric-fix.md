# Plan 0150: Fix cancel_signals metric name mismatch

## Design

The scheduler's cancel-signal emission sites (`actor/build.rs:121`, `actor/worker.rs:254`) emitted `rio_scheduler_cancel_signals_sent_total`, but `lib.rs:179` registered (and `observability.md` documented) `rio_scheduler_cancel_signals_total`. The documented metric had always read zero in Prometheus since phase 3a; the emitted metric appeared as an unregistered counter with no description and no label schema.

This is the first item in the phase 4a task list ("Critical: fix `cancel_signals` metric name mismatch") because it's a one-line bug that had silently broken an operator-visible metric for two phases. Both emission sites changed to use the registered name. Existing tests (`test_backstop_timeout_cancels_and_reassigns`) already exercised the emission code path; name correctness was initially verified by grep.

Round 2 (`b946cbc`) added a backstop-cancel assertion as a rider. Late in phase 4a (`f9dc08d`), a proper recorder-based unit test was added that asserts the counter actually increments — the grep-verified approach had no regression guard.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "cancel_signals_sent_total → cancel_signals_total"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "cancel_signals_sent_total → cancel_signals_total"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "registered name confirmed (no change, source of truth)"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "recorder helper for metric assertions (f9dc08d)"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "recorder assert for cancel_signals_total increment (f9dc08d)"}
]
```

## Tracey

No new markers. The metric is covered by `r[obs.metric.scheduler]` (existing spec rule); no `r[verify]` was added specifically for this metric name — the recorder test is a regression guard, not a spec verification.

## Entry

- Depends on P0148: phase 3b complete

## Exit

Merged as `8fe3d0e` (1 commit) + `f9dc08d` recorder test late in phase. `.#ci` green at merge. Prometheus now shows non-zero `rio_scheduler_cancel_signals_total` on cancel paths.
