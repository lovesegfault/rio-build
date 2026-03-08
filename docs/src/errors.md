# Error Taxonomy

All errors in rio-build are classified into categories that determine retry policy, client-visible behavior, and operational response.

Referenced by: [Phase 3](phases/phase3.md) (basic retry), [Phase 4](phases/phase4.md) (hardened retry/poison), [rio-scheduler](components/scheduler.md)

## Error Classification

| Classification | Retryable | Example | Client Sees | Phase |
|---------------|-----------|---------|-------------|-------|
| **PermanentFailure** | No | Build script exits non-zero, sandbox violation, output rejected, timeout | `BuildResult::PermanentFailure` | 2a |
| **TransientFailure** | Yes (with backoff) | Worker OOM-killed, worker pod preempted, network timeout during input fetch | `BuildResult::TransientFailure` | 3 |
| **InfrastructureFailure** | Yes (different worker) | S3 unavailable, PostgreSQL connection timeout, FUSE cache I/O error, overlay mount failure | `BuildResult::TransientFailure` + reassignment | 3 |
| **DependencyFailed** | No (dep must succeed first) | An input derivation failed | `BuildResult::DependencyFailed` | 2a |
| **CachedFailure** | No (until TTL expires) | Derivation marked as poisoned | `BuildResult::CachedFailure` | 4 |

> **Note:** There is no `TimedOut` variant in the `BuildResultStatus` proto enum. Nix's `BuildStatus::TimedOut` (wire value 8) currently maps to `PermanentFailure` via the worker's fallthrough mapping.

### Additional Nix BuildResult Mappings

The worker maps `rio_nix::BuildStatus` (the real Nix wire enum) to the proto `BuildResultStatus`. Only `PermanentFailure` and `TransientFailure` are mapped explicitly; **all other Nix statuses fall through to `PermanentFailure`**:

| Nix Status | rio-build Classification | Notes |
|------------|------------------------|-------|
| `PermanentFailure` (3) | PermanentFailure | Explicit mapping |
| `TransientFailure` (6) | TransientFailure | Explicit mapping |
| `MiscFailure` (9) | PermanentFailure | Fallthrough. TODO(phase3b): consider mapping to TransientFailure if field experience shows these are often retryable. |
| `TimedOut` (8) | PermanentFailure | Fallthrough. No distinct proto variant. |
| `LogLimitExceeded` (11) | PermanentFailure | Fallthrough. Retry would repeat. |
| `NotDeterministic` (12) | PermanentFailure | Fallthrough. Detected by Nix's `--check`. |
| `InputRejected` (4) | PermanentFailure | Fallthrough. Derivation references an invalid/unresolvable input. |
| `OutputRejected` (5) | PermanentFailure | Fallthrough. Output hash mismatch (FOD) or path collision. |
| `NoSubstituters` (14) | PermanentFailure | Fallthrough. rio-build does not use external substituters. |

See `rio-worker/src/executor/mod.rs` for the mapping implementation.

### FUSE/Overlay Failures

| Failure | Classification | Action |
|---------|---------------|--------|
| FUSE cache I/O error | InfrastructureFailure | Retry on different worker (local disk may be failing) |
| Overlay mount failure | InfrastructureFailure | Retry on different worker (kernel/capability issue) |
| FUSE daemon crash | InfrastructureFailure | The FUSE filesystem is mounted in-process; a crash terminates the worker. External supervisor (systemd / Kubernetes) restarts the pod. Builds in flight are reported as `InfrastructureFailure` and retried on a different worker. |

## Retry Policy

The scheduler's `RetryPolicy` struct (see `rio-scheduler/src/state/worker.rs`):

| Parameter | Default | Description | Phase |
|-----------|---------|-------------|-------|
| `max_retries` | 2 | Maximum retry attempts per derivation after the initial attempt (total attempts = max_retries + 1) | 2a |
| `backoff_base_secs` | 5.0 | Initial backoff delay in seconds | 2a |
| `backoff_multiplier` | 2.0 | Exponential multiplier | 2a |
| `backoff_max_secs` | 300.0 | Maximum backoff delay in seconds | 2a |
| `jitter_fraction` | 0.2 | Jitter factor (0.0-1.0) applied to computed backoff | 2a |

Only `TransientFailure` and `InfrastructureFailure` errors trigger retries. `PermanentFailure` and `DependencyFailed` are terminal.

> **Phase 3b deferral:** The backoff duration is computed and logged but **not yet applied** --- the derivation is re-queued immediately after a transient failure. A delayed re-queue using `tokio::time::sleep` on the computed duration is planned for Phase 3b (see `TODO(phase3b)` at `rio-scheduler/src/actor/completion.rs`).

**Worker avoidance on retry:** There is no explicit config flag for "retry on a different worker." Instead, the scheduler maintains a `failed_workers: HashSet<WorkerId>` on each derivation (see `DerivationState`). This set is currently used only for poison-threshold detection (the derivation is poisoned once `failed_workers.len() >= POISON_THRESHOLD`). Dispatch does not yet exclude workers in `failed_workers` from re-assignment --- a retry may land on the same worker.

> **Interaction with poison tracking:** The poison threshold (`POISON_THRESHOLD = 3` distinct workers) spans across all builds, not just one build's retry budget. A derivation that fails with `max_retries=2` in Build A (3 total attempts) may be attempted again in Build B. After failing on 3 distinct workers across any number of builds, it is marked poisoned. Within a single build, `max_retries` bounds the retry count.

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

| Level | Enforced By | Mechanism | Status |
|-------|------------|-----------|--------|
| Per-derivation wall-clock timeout | Worker | `tokio::time::timeout` wrapping the nix-daemon build. Duration is `WorkAssignment.build_options.build_timeout` if nonzero, else `DEFAULT_DAEMON_TIMEOUT` (7200s / 2h). Configurable via `RIO_DAEMON_TIMEOUT_SECS`, `--daemon-timeout-secs`, or `worker.toml`. | **Implemented** |
| Per-derivation silence timeout | --- | `maxSilentTime` (kill if no output for N seconds) is **not** enforced by the worker. The nix-daemon subprocess may enforce it internally via `nix.conf`, but the worker does not monitor output cadence. | Not implemented |
| Per-build overall timeout | --- | Scheduler-side cancellation when `Build.spec.timeout` is exceeded is **not** implemented. | Not implemented |
| Scheduler backstop timeout | --- | Scheduler-side "worker is lost, reassign" based on missing completion within a multiple of estimated duration is **not** implemented. Worker loss is detected via heartbeat disconnect, not via per-derivation timeout. | Not implemented |

> **Phase 3b deferral:** Per-build overall timeout and scheduler backstop timeout are planned for Phase 3b as part of K8s-aware retry hardening.

## Error Propagation: What the Client Sees

| Internal Failure | Client-Visible Behavior |
|-----------------|------------------------|
| Worker OOM-killed | Build retried on another worker. Client sees continued STDERR streaming. If all retries exhausted: `TransientFailure`. |
| S3 unavailable | Upload retried with backoff. If persistent: `TransientFailure` for the derivation, reassigned. |
| PostgreSQL down | Gateway returns `STDERR_ERROR("build service temporarily unavailable")`. Client can retry. |
| Scheduler failover | Client's `SubmitBuild` stream breaks. Client reconnects; build reattaches if within orphan timeout. |
| Gateway crash | SSH connection drops. Client reconnects; build reattaches. Logs between crash and reconnect are lost unless log persistence is configured. |
| Derivation poisoned | `CachedFailure` with message identifying the poisoned derivation and the failure history. |
