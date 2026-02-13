# Error Taxonomy

All errors in rio-build are classified into categories that determine retry policy, client-visible behavior, and operational response.

Referenced by: [Phase 3](phases/phase3.md) (basic retry), [Phase 4](phases/phase4.md) (hardened retry/poison), [rio-scheduler](components/scheduler.md)

## Error Classification

| Classification | Retryable | Example | Client Sees | Phase |
|---------------|-----------|---------|-------------|-------|
| **PermanentFailure** | No | Build script exits non-zero, sandbox violation, derivation resolution failed | `BuildResult::PermanentFailure` | 2a |
| **TransientFailure** | Yes (with backoff) | Worker OOM-killed, worker pod preempted, network timeout during input fetch, MiscFailure | `BuildResult::TransientFailure` | 3 |
| **InfrastructureFailure** | Yes (different worker) | S3 unavailable, PostgreSQL connection timeout, FUSE cache I/O error, overlay mount failure | `BuildResult::TransientFailure` + reassignment | 3 |
| **TimedOut** | Configurable | Build exceeded `maxSilentTime` or overall timeout | `BuildResult::TimedOut` | 3 |
| **DependencyFailed** | No (dep must succeed first) | An input derivation failed | `BuildResult::DependencyFailed` | 2a |
| **CachedFailure** | No (until TTL expires) | Derivation marked as poisoned | `BuildResult::CachedFailure` | 4 |

### Additional Nix BuildResult Mappings

| Nix Status | rio-build Classification | Rationale |
|------------|------------------------|-----------|
| `MiscFailure` | TransientFailure | Catch-all for unclassified Nix errors; retry is safe |
| `LogLimitExceeded` | PermanentFailure | Build produces excessive output; retry would repeat |
| `NotDeterministic` | PermanentFailure | Build detected as non-deterministic by Nix's `--check` |
| `ResolveFailed` | PermanentFailure | Derivation references unresolvable store path |
| `NoSubstituters` | PermanentFailure | rio-build does not use external substituters; if the store doesn't have it, it can't be substituted |

### FUSE/Overlay Failures

| Failure | Classification | Action |
|---------|---------------|--------|
| FUSE cache I/O error | InfrastructureFailure | Retry on different worker (local disk may be failing) |
| Overlay mount failure | InfrastructureFailure | Retry on different worker (kernel/capability issue) |
| FUSE daemon crash | InfrastructureFailure | Restart FUSE daemon; retry build on same worker if restart succeeds |

## Retry Policy

| Parameter | Default | Description | Phase |
|-----------|---------|-------------|-------|
| `maxRetries` | 2 | Maximum retry attempts per derivation after the initial attempt (total attempts = maxRetries + 1, across all workers) | 3 |
| `retryBackoffBase` | 5s | Initial backoff delay | 3 |
| `retryBackoffMultiplier` | 2.0 | Exponential multiplier | 3 |
| `retryBackoffMax` | 300s | Maximum backoff delay | 3 |
| `retryBackoffJitter` | 0.2 | Jitter factor (0.0-1.0) | 3 |
| `retryOnDifferentWorker` | true | Whether to prefer a different worker on retry | 3 |

Only `TransientFailure` and `InfrastructureFailure` errors trigger retries. `PermanentFailure` and `DependencyFailed` are terminal. `TimedOut` is configurable (retryable if the timeout was `maxSilentTime`, terminal if overall build timeout).

> **Interaction with poison tracking:** The poison threshold (default: 3 different workers) spans across all builds, not just one build's retry budget. A derivation that fails with `maxRetries=2` in Build A (3 total attempts) may be attempted again in Build B. After failing on 3 distinct workers across any number of builds, it is marked poisoned. Within a single build, `maxRetries` bounds the retry count.

## Poison Derivation Tracking

Derivations that consistently fail are marked as "poisoned" to prevent infinite retry loops. Introduced in Phase 4.

| Parameter | Default | Description |
|-----------|---------|-------------|
| `poisonThreshold` | 3 | Consecutive failures across different workers before marking as poisoned |
| `poisonTTL` | 24h | Time after which poison state automatically expires |
| `poisonScope` | per-derivation-hash | Granularity of poison tracking |

Poisoned derivations:
- Are immediately reported as `CachedFailure` to all interested builds
- Are NOT silently dropped --- the client receives an explicit error
- Expire after `poisonTTL` so transient infrastructure issues self-heal
- Can be manually cleared via `AdminService.ClearPoison(derivation_hash)` (Phase 4)

**Per-worker tracking:** If a derivation fails only on a specific worker (e.g., hardware issue), the scheduler tracks per-worker failure counts separately. A derivation is only globally poisoned if it fails on `poisonThreshold` *different* workers.

## Timeout Enforcement

| Level | Enforced By | Mechanism |
|-------|------------|-----------|
| Per-derivation silence timeout | Worker | Monitors build process stdout/stderr; kills if no output for `maxSilentTime` |
| Per-derivation wall-clock timeout | Worker | Kills build process if exceeds `2 * estimatedDuration` (configurable factor) |
| Per-build overall timeout | Scheduler | Cancels remaining derivations if `Build.spec.timeout` exceeded |
| Scheduler backstop | Scheduler | If worker doesn't report completion within `max(3 * estimatedDuration, heartbeat_interval * 30)`, assumes worker is lost and reassigns. The `heartbeat_interval * 30` floor ensures long builds with poor estimates aren't killed prematurely. |

## Error Propagation: What the Client Sees

| Internal Failure | Client-Visible Behavior |
|-----------------|------------------------|
| Worker OOM-killed | Build retried on another worker. Client sees continued STDERR streaming. If all retries exhausted: `TransientFailure`. |
| S3 unavailable | Upload retried with backoff. If persistent: `TransientFailure` for the derivation, reassigned. |
| PostgreSQL down | Gateway returns `STDERR_ERROR("build service temporarily unavailable")`. Client can retry. |
| Scheduler failover | Client's `SubmitBuild` stream breaks. Client reconnects; build reattaches if within orphan timeout. |
| Gateway crash | SSH connection drops. Client reconnects; build reattaches. Logs between crash and reconnect are lost unless log persistence is configured. |
| Derivation poisoned | `CachedFailure` with message identifying the poisoned derivation and the failure history. |
