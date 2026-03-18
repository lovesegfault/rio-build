# Plan 0187: Rem-08 — Lease renewal timeout + local self-fence + recovery gen-snapshot TOCTOU

## Design

**P1 (HIGH).** Three related liveness/safety gaps sharing a root: the lease loop trusts the apiserver to return promptly and trusts its own `is_leader` flag to be authoritative. Neither holds under network partition.

**Finding 1 — no timeout on renewal:** `api.get_opt(...).await` and `api.replace(...).await` have no deadline. kube-rs doesn't install a default. An apiserver that accepts TCP but never replies (overloaded etcd, half-open socket) hangs the call past `LEASE_TTL`. `interval.tick()` is only awaited at the top of the loop — once `try_acquire_or_renew().await` is entered, the interval can't preempt it. Timeline: T+0 leader's GET hangs; T+15 `LEASE_TTL` elapses, standby steals; T+15..∞ hung leader keeps dispatching (workers reject via generation fence but RPCs go out).

**Finding 2 — no local self-fence on error arm:** when `try_acquire_or_renew()` finally returns `Err`, the code logs and retries — but `is_leader` is still `true`. Should flip `is_leader=false` immediately on any error (pessimistic self-fence).

**Finding 3 — recovery gen-snapshot TOCTOU:** `recovery.rs` snapshots the generation at start; if generation increments mid-recovery (standby stole), recovery proceeds with stale snapshot and dispatches work the new leader will also dispatch.

Remediation doc: `docs/src/remediations/phase4a/08-lease-renewal-timeout.md` (594 lines).

## Files

```json files
[
  {"path": "rio-scheduler/src/lease/mod.rs", "action": "MODIFY", "note": "tokio::time::timeout wrapper on api.get_opt + api.replace; flip is_leader=false on Err"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "generation snapshot check at dispatch gate"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "re-check generation at end of recovery; abort if stale"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "TOCTOU regression test"}
]
```

## Tracey

- `r[verify sched.recovery.gate-dispatch]` — `52be5df`

1 marker annotation.

## Entry

- Depends on P0167: in-house leader election (this hardens it)

## Exit

Merged as `52be5df` (fix; plan at index §2.2). `.#ci` green.
