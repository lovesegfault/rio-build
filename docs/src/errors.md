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
| **CachedFailure** | No (until TTL expires) | Derivation marked as poisoned | `BuildResult::CachedFailure` | 2a (in-memory poison) |

> **Note:** There is no `TimedOut` variant in the `BuildResultStatus` proto enum. Nix's `BuildStatus::TimedOut` (wire value 8) currently maps to `PermanentFailure` via the worker's fallthrough mapping.

### Additional Nix BuildResult Mappings

The worker maps `rio_nix::BuildStatus` (the real Nix wire enum) to the proto `BuildResultStatus`. Only `PermanentFailure` and `TransientFailure` are mapped explicitly; **all other Nix statuses fall through to `PermanentFailure`**:

| Nix Status | rio-build Classification | Notes |
|------------|------------------------|-------|
| `PermanentFailure` (3) | PermanentFailure | Explicit mapping |
| `TransientFailure` (6) | TransientFailure | Explicit mapping |
| `MiscFailure` (9) | PermanentFailure | Fallthrough for uncategorized failures. Kept as permanent: field experience (Phase 3a VM tests) shows these are usually deterministic (malformed .drv, missing env var) rather than transient. |
| `TimedOut` (8) | PermanentFailure | Fallthrough. No distinct proto variant. |
| `LogLimitExceeded` (11) | PermanentFailure | Fallthrough. Retry would repeat. |
| `NotDeterministic` (12) | PermanentFailure | Fallthrough. Detected by Nix's `--check`. |
| `InputRejected` (4) | PermanentFailure | Fallthrough. Derivation references an invalid/unresolvable input. |
| `OutputRejected` (5) | PermanentFailure | Fallthrough. Output hash mismatch (FOD) or path collision. |
| `NoSubstituters` (14) | PermanentFailure | Fallthrough. rio-build does not use external substituters. |

See `rio-worker/src/executor/mod.rs` for the mapping implementation.

### Infrastructure Error Types (rio-common)

| Error Type | Source | Description |
|------------|--------|-------------|
| `HmacError` | `rio-common/src/hmac.rs` | Token verification failures: I/O reading key file, empty key, malformed token (wrong part count, bad base64/JSON), signature mismatch, expiry in the past. Surfaced to clients as `PERMISSION_DENIED`. |
| `TlsError` | `rio-common/src/tls.rs` | TLS config load failures: I/O reading PEM files, empty cert/key, PEM parse errors. These are **startup** errors (fail-fast), not runtime. |
| `StreamProcessError` | `rio-gateway/src/handler/build.rs` | Gateway-internal enum distinguishing `Transport` (scheduler connection dropped — **retried** with 5× backoff 1/2/4/8/16s), `EofWithoutTerminal` (stream ended without terminal BuildEvent — **not retried**, build state lost), and `Wire` (protocol parse error — **not retried**). Only `Transport` triggers the WatchBuild reconnect loop. |

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

**Delayed re-queue (Phase 3b):** The computed backoff is stored in `DerivationState.backoff_until`. `dispatch_ready` defers the derivation until `Instant::now() >= backoff_until` using the same defer-and-requeue pattern as size-class mismatch. Cleared on successful dispatch. Stateless — no timer tasks to clean up on cancel.

**Worker avoidance (Phase 3b):** Dispatch's `best_worker()` filter excludes workers in `DerivationState.failed_workers`. Combined with the backoff above: a transient fail → Ready + backoff_until set + failed_workers.insert(worker) → next dispatch goes to a DIFFERENT worker after the backoff. `reassign_derivations` (worker disconnect) also feeds failed_workers, so a worker crashing mid-build counts as a distinct-worker failure for poison detection.

> **Interaction with poison tracking:** The poison threshold (`POISON_THRESHOLD = 3` distinct workers) spans across all builds, not just one build's retry budget. A derivation that fails with `max_retries=2` in Build A (3 total attempts) may be attempted again in Build B. After failing on 3 distinct workers across any number of builds, it is marked poisoned. Within a single build, `max_retries` bounds the retry count.

## Poison Derivation Tracking

Derivations that consistently fail are marked as "poisoned" to prevent infinite retry loops. In-memory poison tracking (`failed_workers` HashSet per derivation) is **live**. PostgreSQL-backed persistence (surviving scheduler restart) is Phase 4.

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
| Per-build overall timeout | --- | Scheduler-side cancellation when `Build.spec.timeout` is exceeded is **not** implemented. | Not implemented (Phase 4) |
| Scheduler backstop timeout | `handle_tick` | When a Running derivation's `running_since.elapsed()` exceeds `max(est_duration × 3, daemon_timeout + 10min)`, scheduler sends CancelSignal + resets to Ready + increments retry_count + adds worker to failed_workers. Catches "worker heartbeating but daemon wedged." | Implemented (Phase 3b) |

## Error Propagation: What the Client Sees

| Internal Failure | Client-Visible Behavior |
|-----------------|------------------------|
| Worker OOM-killed | Build retried on another worker. Client sees continued STDERR streaming. If all retries exhausted: `TransientFailure`. |
| S3 unavailable | Upload retried with backoff. If persistent: `TransientFailure` for the derivation, reassigned. |
| PostgreSQL down | Gateway returns `STDERR_ERROR("build service temporarily unavailable")`. Client can retry. |
| Scheduler failover | Gateway's `BuildEvent` stream breaks with a `Transport` error. Gateway transparently reconnects via `WatchBuild(since_sequence)` up to 5× with backoff (1/2/4/8/16s); scheduler replays from `build_event_log`. If reconnect budget exhausted → `MiscFailure` to client. |
| Gateway crash | SSH connection drops. Client reconnects; build reattaches via DAG-merge cache hits (stored outputs are instant-hit). Logs between crash and reconnect are lost unless log persistence is configured. |
| Derivation poisoned | `CachedFailure` with message identifying the poisoned derivation and the failure history. |
