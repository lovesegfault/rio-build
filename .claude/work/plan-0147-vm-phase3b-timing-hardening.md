# Plan 0147: vm-phase3b timing hardening — recovery race + Tick-stale gauge

## Design

Four one-line timing fixes for intermittent vm-phase3b failures. These aren't flakes in the sense of "retry and it passes" — they're races that the prior validation rounds introduced but only manifested under specific qemu scheduling.

**`c51e50d` — Move PG non-terminal check BEFORE restart.** V3's recovery test: start slow build, check PG has non-terminal rows, SIGKILL scheduler, restart, assert recovery loaded rows. But the check was AFTER restart — by then, if the slow build happened to complete during the ~2s restart window, recovery would sweep it (X5's `check_build_completion`) and the assertion would see 0 rows. Moved the PG count check to BEFORE the SIGKILL — proves non-terminal state exists at crash time; post-recovery assertion checks `recovery_derivations_loaded` metric instead.

**`7c56daf` — Bump slow-build sleep 60s→90s, queue-drain timeout 90s→120s.** Z3's fix reconciles completed-during-downtime derivations. If the slow build's 60s sleep finished during the scheduler-restart window, Z3 would complete it → V3's "recovery loads non-terminal" fails. 90s gives enough margin for qemu variance.

**`9221ee0` — POISON_TTL expiry sleep 150ms→300ms.** Scheduler test: mark derivation poisoned, sleep past `POISON_TTL` (cfg(test)=100ms), tick, assert cleared. But `POISON_TTL` uses `std::time::Instant::elapsed()` — can't be mocked with `tokio::time::pause()`. Real sleep. 150ms was 1.5× margin; under load, not enough. Bumped to 300ms (3×).

**`60de439` — Settle queue before slow build; `derivations_running` is Tick-stale.** V3 asserted `derivations_running >= 1` immediately after submitting the slow build. But that gauge is updated by `handle_tick` every 10s, not synchronously by dispatch. If the assert ran within the same tick window as a PRIOR build completing, the gauge showed 0 (stale). Fix: wait for prior queue to settle first, then submit, then wait one tick.

## Files

```json files
[
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "move PG check before restart; bump sleep 60s→90s; settle queue before slow build"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "POISON_TTL sleep 150ms→300ms (Instant can't be mocked)"}
]
```

## Tracey

No tracey markers landed. Timing adjustments to existing tests.

## Entry

- Depends on P0141: Z3/X5 behavior changes that introduced these races.
- Depends on P0137: vm-phase3b V3 section exists.

## Exit

Merged as `c51e50d..60de439` (4 commits). `.#ci` green at merge — vm-phase3b stable across 10+ consecutive runs.
