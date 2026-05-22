#import "/lib/rio.typ": *

#show: rio.with(domains: none)

This page documents the behavior of rio-build when individual components fail,
including cascading effects and recovery procedures.

// The §Component Failure Matrix table is intentionally omitted from this
// chapter — its rows shard to the per-component chapters in batch-4.

= Partial Failure Scenarios

== S3 Read-Only Degradation

If S3 writes fail but reads succeed (e.g., S3 rate limiting on PUTs):
- *Reads* (cache hits, binary cache serving): continue normally
- *Writes* (build output uploads): fail and retry. If all retries fail, the
  build output on the executor's overlay is lost
- *Impact*: New builds that produce outputs can't persist them, but builds that
  only consume existing cache entries work fine

== Network Partition: Scheduler ↔ Executors

- Executors detect partition via heartbeat timeout (\~30--40s wall-clock:
  `HEARTBEAT_TIMEOUT_SECS = 30s` checked on each \~10s scheduler tick)
- Executors close their `BuildExecution` stream and attempt reconnection with
  backoff
- The scheduler calls `reset_to_ready()` on disconnected executors' running
  builds --- they go directly back to Ready (increment `retry_count`), no
  intermediate status classification
- Builds already assigned but not yet started are reassigned immediately

== Network Partition: Gateway ↔ Scheduler

- Gateway's `SubmitBuild` calls fail with `UNAVAILABLE`
- The gateway returns `STDERR_ERROR` to the Nix client, which retries (standard
  Nix behavior)
- Builds already submitted continue in the scheduler; the gateway can re-attach
  via `WatchBuild` after reconnection

== Scheduler Split-Brain Mitigation

Split-brain is closed by the fence/steal asymmetry --- a leader that cannot
renew self-fences at `SELF_FENCE_AFTER` (11s), `2 × FENCE_MARGIN` before any
standby's `STEAL_AFTER` (19s) steal threshold --- and bounded by the
executor-side generation fence for the clock-pause residual:
- The leadership generation derives from the Lease's `leaseTransitions` count
  (the lease loop applies `fetch_max(transitions + 1)` to the shared in-memory
  `Arc<AtomicU64>`), floored during recovery by the durable PG history
  (`assignments` plus the `leader_generation_claims` ledger); a same-epoch
  re-acquire keeps its generation
- The generation flows into `WorkAssignment.generation`; the new leader's
  heartbeat replies advertise 0 (the proto-unset sentinel) until its recovery
  completes (#rref("sched.lease.claim-before-advertise")), so executors begin
  rejecting the old leader's stale-generation assignments once the
  post-recovery generation reaches them via heartbeat; the pre-arming interim
  is the same dual-leader window priced by the idempotent-writes bullet below
- *No PostgreSQL-level write fencing exists* --- a deposed leader's in-flight
  PG writes will succeed. PG writes are idempotent (INSERT ON CONFLICT,
  status-check UPDATEs), which limits the damage
- Optional future hardening: add a `scheduler_meta` row with a generation-guard
  WHERE clause for strict fencing (current: idempotent writes tolerate
  dual-leader window)

== Cascading FUSE Cache Miss Storm

If rio-store is degraded (slow but not down), all executors' @fuse cache misses
queue up:
- Executors' FUSE read operations block, causing build sandboxes to stall
- The scheduler's @backpressure mechanism (actor queue depth > 80%) rejects new
  builds with `RESOURCE_EXHAUSTED`
- After 5 consecutive `ensure_cached` failures, the FUSE circuit breaker opens
  and `check()` returns `EIO` immediately (fail-fast). The existing
  `WAIT_DEADLINE` timeout on each fetch feeds the failure counter. See
  `r[builder.fuse.circuit-breaker]`.
