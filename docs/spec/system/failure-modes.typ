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

- Executor pods notice nothing beyond failed unaries: an in-flight build keeps
  running and retries `ReportOutcome` until acked; a not-yet-pulled pod
  retries `PullAssignment` for as long as it lives (bounded by the Job's
  `activeDeadlineSeconds`)
- The scheduler resets nothing on the partition itself --- the open attempt is
  resolved by the report that eventually lands, by the controller's
  pod-terminal report if the pod dies first, or by the establishment sweep at
  deadline + report-slack
- Spawned-but-unpulled intents simply re-arm; the controller keeps its Job
  census and reaps only per its own graces

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
generation-fenced authority transactions for the clock-pause residual:
- The leadership generation derives from the Lease's `leaseTransitions` count
  (the lease loop applies `fetch_max(transitions + 1)` to the shared in-memory
  `Arc<AtomicU64>`), floored during recovery by the durable PG history
  (`assignments` plus the `leader_generation_claims` ledger); a same-epoch
  re-acquire keeps its generation; the claim row is durable before recovery
  completes and work is served (#rref("sched.lease.claim-before-advertise"))
- Every authority-exercising transaction (the pull mint, the establishment
  charge, the synthesized close) carries the serving generation, persists it
  on the row it writes, and aborts when it is below the durable claims floor
  (#rref("sched.lease.generation-fence")), so a deposed leader can neither
  bind new work nor consume outcomes once the new leader's generation is
  durable
- Scheduler writes outside those transactions are not generation-fenced --- a
  deposed leader's in-flight PG writes will succeed. They are idempotent
  (INSERT ON CONFLICT, status-check UPDATEs), which limits the damage

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
