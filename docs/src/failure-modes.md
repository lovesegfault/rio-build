# Failure Modes

This page documents the behavior of rio-build when individual components fail, including cascading effects and recovery procedures.

## Component Failure Matrix

| Component Down | Immediate Effect | Cascading Effect | Recovery |
|---|---|---|---|
| **Gateway pod(s)** | Active SSH connections drop; clients see connection reset | Log streams for in-progress builds are lost; builds continue in the scheduler | Clients reconnect to surviving replicas via NLB. No data loss. |
| **Scheduler** | No new builds accepted; `SubmitBuild` returns `UNAVAILABLE` | Workers' `BuildExecution` streams disconnect; workers go idle. Gateways queue requests briefly, then return errors to clients. | New leader acquires PG advisory lock, reconstructs DAGs from PostgreSQL, workers and gateways reconnect streams. Log events between crash and reconnection may be lost. |
| **Store (all replicas)** | `QueryPathInfo`, `PutPath`, `GetPath` fail | Workers can't fetch inputs (FUSE cache misses fail) or upload outputs; builds stall on I/O. Gateway can't answer `wopQueryPathInfo`. | Retry on store recovery. Builds that timed out waiting for store are retried. Worker overlay outputs are lost if all upload retries fail. |
| **PostgreSQL** | Scheduler can't persist state; store can't query metadata | Full system halt --- no scheduling, no metadata lookups, no new builds | Restore PG from backup. All components reconnect via connection retry. Scheduler reconstructs in-memory state from recovered tables. |
| **S3 (object storage)** | Chunk reads/writes fail | Store returns errors to workers and gateways; worker uploads fail. Builds whose inputs are fully SSD-cached may continue. | Retry with backoff (S3 DELETE is idempotent). Worker overlay outputs are lost if all upload retries fail. |
| **Worker pod** | Running builds on that worker fail with `InfrastructureFailure` | Scheduler detects via missed heartbeats (30s timeout), reassigns affected derivations to other workers | StatefulSet recreates pod. New pod starts with cold FUSE cache. |
| **Controller** | No autoscaling decisions; CRD reconciliation pauses | WorkerPool sizes remain static; no GC scheduling; no Build CRD status updates | Restart controller. State is in CRDs and K8s API; no persistent state lost. |

## Partial Failure Scenarios

### S3 Read-Only Degradation

If S3 writes fail but reads succeed (e.g., S3 rate limiting on PUTs):
- **Reads** (cache hits, binary cache serving): continue normally
- **Writes** (build output uploads): fail and retry. If all retries fail, the build output on the worker's overlay is lost
- **Impact**: New builds that produce outputs can't persist them, but builds that only consume existing cache entries work fine

### Network Partition: Scheduler <-> Workers

- Workers detect partition via heartbeat timeout (3 missed heartbeats = 30s)
- Workers close their `BuildExecution` stream and attempt reconnection with backoff
- The scheduler marks disconnected workers' running builds as `InfrastructureFailure` and requeues them (up to retry limit)
- Builds already assigned but not yet started are reassigned immediately

### Network Partition: Gateway <-> Scheduler

- Gateway's `SubmitBuild` calls fail with `UNAVAILABLE`
- The gateway returns `STDERR_ERROR` to the Nix client, which retries (standard Nix behavior)
- Builds already submitted continue in the scheduler; the gateway can re-attach via `WatchBuild` after reconnection

### Scheduler Split-Brain Prevention

Split-brain is prevented by the PostgreSQL advisory lock generation counter:
- Each leader election increments a generation number stored in PG
- All state-modifying SQL includes `WHERE leader_generation = $current`
- A stale leader (whose PG connection silently dropped) will have its writes rejected by the generation check
- Result: at most one leader can successfully write state at any time

### Cascading FUSE Cache Miss Storm

If rio-store is degraded (slow but not down), all workers' FUSE cache misses queue up:
- Workers' FUSE read operations block, causing build sandboxes to stall
- Backpressure: FUSE daemon has a configurable read timeout (default: 60s). After timeout, the read returns an I/O error to the build sandbox, which typically causes the build to fail
- The scheduler's backpressure mechanism (actor queue depth > 80%) rejects new builds with `RESOURCE_EXHAUSTED`
- Mitigation: workers can implement a circuit breaker on FUSE fetches, failing fast after N consecutive timeouts
