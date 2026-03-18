# Failure Modes

This page documents the behavior of rio-build when individual components fail, including cascading effects and recovery procedures.

## Component Failure Matrix

| Component Down | Immediate Effect | Cascading Effect | Recovery |
|---|---|---|---|
| **Gateway pod(s)** | Active SSH connections drop; clients see connection reset | Log streams for in-progress builds are lost; builds continue in the scheduler | Clients reconnect to surviving replicas via NLB. No data loss. |
| **Scheduler** | No new builds accepted; `SubmitBuild` returns `UNAVAILABLE` | Workers' `BuildExecution` streams disconnect; workers go idle. Gateways return errors to clients. | New leader acquires the Kubernetes Lease → fire-and-forgets `LeaderAcquired` → `recover_from_pg` rebuilds DAG from PG. Workers reconnect; `ReconcileAssignments` (45s delayed) checks for orphan completions or resets stale assignments. Build CRD controllers reconnect via `WatchBuild(build_id, since_sequence=last_seen)`. |
| **Store (all replicas)** | `QueryPathInfo`, `PutPath`, `GetPath` fail | Workers can't fetch inputs (FUSE cache misses fail) or upload outputs; builds stall on I/O. Gateway can't answer `wopQueryPathInfo`. | Retry on store recovery. Builds that timed out waiting for store are retried. Worker overlay outputs are lost if all upload retries fail. |
| **PostgreSQL** | Scheduler can't persist state; store can't query metadata | Full system halt --- no scheduling, no metadata lookups, no new builds | Restore PG from backup. All components reconnect via connection retry. Scheduler rebuilds DAG via `recover_from_pg` on next LeaderAcquired. If PG is restored but DAG state is lost (full data loss), clients must resubmit. |
| **S3 (object storage)** | Chunk reads/writes fail | Store returns errors to workers and gateways; worker uploads fail. Builds whose inputs are fully SSD-cached may continue. | Retry with backoff (S3 DELETE is idempotent). Worker overlay outputs are lost if all upload retries fail. |
| **Worker pod** | Running builds on that worker are orphaned | Scheduler detects via missed heartbeats (~50-60s wall-clock), calls `reset_to_ready()` on affected derivations --- they go straight back to Ready (increment `retry_count`) and re-queue, no intermediate `InfrastructureFailure` classification | StatefulSet recreates pod. New pod starts with cold FUSE cache. |
| **Controller** | No autoscaling decisions; CRD reconciliation pauses | WorkerPool sizes remain static; no GC scheduling; no Build CRD status updates | Restart controller. State is in CRDs and K8s API; no persistent state lost. |

## Partial Failure Scenarios

### S3 Read-Only Degradation

If S3 writes fail but reads succeed (e.g., S3 rate limiting on PUTs):
- **Reads** (cache hits, binary cache serving): continue normally
- **Writes** (build output uploads): fail and retry. If all retries fail, the build output on the worker's overlay is lost
- **Impact**: New builds that produce outputs can't persist them, but builds that only consume existing cache entries work fine

### Network Partition: Scheduler <-> Workers

- Workers detect partition via heartbeat timeout (~50-60s wall-clock: 30s staleness threshold + 3-tick confirmation)
- Workers close their `BuildExecution` stream and attempt reconnection with backoff
- The scheduler calls `reset_to_ready()` on disconnected workers' running builds --- they go directly back to Ready (increment `retry_count`), no intermediate status classification
- Builds already assigned but not yet started are reassigned immediately

### Network Partition: Gateway <-> Scheduler

- Gateway's `SubmitBuild` calls fail with `UNAVAILABLE`
- The gateway returns `STDERR_ERROR` to the Nix client, which retries (standard Nix behavior)
- Builds already submitted continue in the scheduler; the gateway can re-attach via `WatchBuild` after reconnection

### Scheduler Split-Brain Mitigation

Split-brain is bounded by the Kubernetes Lease renew deadline (default 15s):
- Each Lease acquisition increments an in-memory `Arc<AtomicU64>` generation counter
- The generation flows into `WorkAssignment.generation`; workers reject stale-generation assignments after their next heartbeat sync
- **No PostgreSQL-level write fencing exists** --- a deposed leader's in-flight PG writes will succeed. PG writes are idempotent (INSERT ON CONFLICT, status-check UPDATEs), which limits the damage
- Optional Phase 4 hardening: add a `scheduler_meta` row with a generation-guard WHERE clause for strict fencing (current: idempotent writes tolerate dual-leader window)

### Cascading FUSE Cache Miss Storm

If rio-store is degraded (slow but not down), all workers' FUSE cache misses queue up:
- Workers' FUSE read operations block, causing build sandboxes to stall
- The scheduler's backpressure mechanism (actor queue depth > 80%) rejects new builds with `RESOURCE_EXHAUSTED`
- **Phase 4b:** See `r[worker.fuse.circuit-breaker]` — after 5 consecutive `ensure_cached` failures, `check()` returns `EIO` immediately (fail-fast). The existing `WAIT_DEADLINE` timeout on each fetch feeds the failure counter.
