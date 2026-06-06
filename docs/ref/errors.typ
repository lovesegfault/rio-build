#import "/lib/rio.typ": *

#show: rio.with(domains: none)

All errors in rio-build are classified into categories that determine retry
policy, client-visible behavior, and operational response.

Referenced by: #cross-link("/spec/components/scheduler.typ")[rio-scheduler] (retry/poison
state machine)

= Error Classification

#table(
  columns: 5,
  table.header(
    [Classification], [Retryable], [Example], [Client Sees], [Phase]
  ),
  [*PermanentFailure*],
  [No],
  [Build script exits non-zero, sandbox violation, output rejected, timeout],
  [`BuildResult::PermanentFailure`],
  [2a],

  [*TransientFailure*],
  [Yes (with backoff)],
  [Executor #gls("oom")-killed, executor pod preempted, network timeout during input
    fetch],
  [`BuildResult::TransientFailure`],
  [3],

  [*InfrastructureFailure*],
  [Yes (different executor)],
  [S3 unavailable, PostgreSQL connection timeout, @fuse cache I/O error,
    overlay mount failure],
  [`BuildResult::TransientFailure` + reassignment],
  [3],

  [*DependencyFailed*],
  [No (dep must succeed first)],
  [An input derivation failed],
  [`BuildResult::DependencyFailed`],
  [2a],

  [*CachedFailure*],
  [No (until TTL expires)],
  [Derivation marked as poisoned],
  [`BuildResult::CachedFailure`],
  [2a (in-memory poison)],
)

== Additional Nix BuildResult Mappings

`rio_nix::BuildStatus` (the Nix wire enum) maps to the proto `BuildResultStatus`
via an exhaustive `From` impl. Most variants map 1:1; only `MiscFailure` and
`NoSubstituters` collapse to `PermanentFailure`:

#table(
  columns: 3,
  table.header([Nix Status], [rio-build Classification], [Notes]),
  [`PermanentFailure` (3)], [PermanentFailure], [1:1],
  [`TransientFailure` (6)], [TransientFailure], [1:1],
  [`MiscFailure` (9)],
  [PermanentFailure],
  [Collapsed. nix-daemon's own catch-all ("failed, unclassified"). Treated as
    deterministic: don't retry.],

  [`TimedOut` (8)],
  [TimedOut],
  [1:1. Scheduler treats as permanent-no-reassign.],

  [`LogLimitExceeded` (11)], [LogLimitExceeded], [1:1. Retry would repeat.],

  [`NotDeterministic` (12)],
  [NotDeterministic],
  [1:1. Detected by Nix's `--check`.],

  [`InputRejected` (4)],
  [InputRejected],
  [1:1. Derivation references an invalid/unresolvable input.],

  [`OutputRejected` (5)],
  [OutputRejected],
  [1:1. Output hash mismatch (@fod) or path collision.],

  [`NoSubstituters` (14)],
  [PermanentFailure],
  [Collapsed. Workers run with `substitute = false`; seeing this means
    misconfiguration.],
)

See `rio-proto/src/status.rs` for the mapping implementation.

== Infrastructure Error Types

// Curated highlight list. Each name asserts existence in
// gen/errors.json.enums (catches a deleted enum); description from the
// enum-level `///` doc (asserted non-empty unless an override exists),
// or from _highlight-override for typst-side rich bodies the rust
// comment can't carry (refs.const, per-variant refs.error-doc).
#let _errs = json("/gen/errors.json")
#let _enums = (:)
#for e in _errs.enums { _enums.insert(e.name, e) }
#let _highlight = ("HmacError", "JwtError", "StreamProcessError")
#let _highlight-override = (
  HmacError: [Token verification failures: I/O reading key file, empty
    key, malformed token (wrong part count, bad base64/JSON), signature
    mismatch, expiry in the past. Surfaced to clients as
    `PERMISSION_DENIED`.],
  JwtError: [Tenant-JWT verification failures: signature mismatch, expired,
    unknown tenant, malformed claims. Surfaced as `PERMISSION_DENIED` at
    SSH auth (gateway) or gRPC interceptor (admin).],
  StreamProcessError: [Gateway-internal: distinguishes `Transport`
    (#(refs.error-doc)("StreamProcessError", "Transport")) and
    `EofWithoutTerminal`
    (#(refs.error-doc)("StreamProcessError", "EofWithoutTerminal")) ---
    *both retried* up to #(refs.const)("MAX_RECONNECT")× with backoff
    1/2/4/8/16 s capped at 16 s --- from `Wire`
    (#(refs.error-doc)("StreamProcessError", "Wire") --- *not
    retried*).],
)
#table(
  columns: 3,
  table.header([Error Type], [Crate], [Description]),
  .._highlight
    .map(name => {
      assert(
        name in _enums,
        message: "highlighted enum not in gen/errors.json: " + name,
      )
      let e = _enums.at(name)
      let desc = if name in _highlight-override {
        _highlight-override.at(name)
      } else {
        assert(e.doc != "", message: "highlighted enum has no /// doc: " + name)
        [#e.doc]
      }
      (raw(name), raw(e.crate), desc)
    })
    .flatten(),
)

== FUSE/Overlay Failures

#table(
  columns: 3,
  table.header([Failure], [Classification], [Action]),
  [FUSE cache I/O error],
  [InfrastructureFailure],
  [Retry on different executor (local disk may be failing)],

  [Overlay mount failure],
  [InfrastructureFailure],
  [Retry on different executor (kernel/capability issue)],

  [FUSE daemon crash],
  [InfrastructureFailure],
  [The FUSE filesystem is mounted in-process; a crash terminates the executor.
    External supervisor (systemd / Kubernetes) restarts the pod. Builds in
    flight are reported as `InfrastructureFailure` and retried on a different
    executor.],
)

= Retry Policy

The scheduler's `RetryPolicy` struct (see
`rio-scheduler/src/state/executor.rs`):

#table(
  columns: 4,
  table.header([Parameter], [Default], [Description], [Phase]),
  [`max_retries`],
  [2],
  [Maximum retry attempts per derivation after the initial attempt (total
    attempts = max_retries + 1)],
  [2a],

  [`backoff_base_secs`], [5.0], [Initial backoff delay in seconds], [2a],
  [`backoff_multiplier`], [2.0], [Exponential multiplier], [2a],
  [`backoff_max_secs`], [300.0], [Maximum backoff delay in seconds], [2a],
  [`jitter_fraction`],
  [0.2],
  [Jitter factor (0.0-1.0) applied to computed backoff],
  [2a],
)

Only `TransientFailure` and `InfrastructureFailure` errors trigger retries.
`PermanentFailure` and `DependencyFailed` are terminal.

*Delayed re-queue (Phase 3b):* The computed backoff is stored in
`DerivationState.backoff_until`. a deferred derivation stays `Ready` but is
withheld from claimability until `Instant::now() >= backoff_until`
(re-evaluated on every pull/tick — no timer state). Cleared on successful
dispatch. Stateless --- no timer tasks to clean up on cancel.

*Executor avoidance (Phase 3b):* Dispatch's `best_executor()` filter excludes
executors in `DerivationState.failed_builders`. Combined with the backoff
above: a transient fail → Ready + backoff_until set +
failed_builders.insert(executor) → next dispatch goes to a DIFFERENT executor
after the backoff. `reassign_derivations` (executor disconnect) also feeds
failed_builders, so an executor crashing mid-build counts as a
distinct-executor failure for poison detection.

#info[
  *Interaction with poison tracking:* The poison threshold (`POISON_THRESHOLD =
  3` distinct executors) spans across all builds, not just one build's retry
  budget. A derivation that fails with `max_retries=2` in Build A (3 total
  attempts) may be attempted again in Build B. After failing on 3 distinct
  executors across any number of builds, it is marked poisoned. Within a
  single build, `max_retries` bounds the retry count.
]

= Poison Derivation Tracking

Derivations that consistently fail are marked as "poisoned" to prevent
infinite retry loops. In-memory poison tracking (`failed_builders` HashSet per
derivation) is *live*. PostgreSQL-backed `failed_builders TEXT[]` persistence
exists (migration 004); `poisoned_at TIMESTAMPTZ` persistence (migration 009
Part B) means the 24h TTL survives scheduler restart. See
#cross-link("/spec/components/scheduler.typ")[`sched.poison.ttl-persist`] for details.

#table(
  columns: 3,
  table.header([Parameter], [Default], [Description]),
  [`poisonThreshold`],
  [3],
  [Consecutive failures across different executors before marking as poisoned],

  [`poisonTTL`], [24h], [Time after which poison state automatically expires],

  [`poisonScope`], [per-derivation-hash], [Granularity of poison tracking],
)

Poisoned derivations:
- Are immediately reported as `CachedFailure` to all interested builds
- Are NOT silently dropped --- the client receives an explicit error
- Expire after `poisonTTL` so transient infrastructure issues self-heal
- Can be manually cleared via `AdminService.ClearPoison(derivation_hash)` ---
  see #cross-link("/spec/components/scheduler.typ")[`sched.admin.clear-poison`]

*Per-executor tracking:* If a derivation fails only on a specific executor
(e.g., hardware issue), the scheduler tracks per-executor failure counts
separately. A derivation is only globally poisoned if it fails on
`poisonThreshold` _different_ executors.

= Timeout Enforcement

#table(
  columns: 4,
  table.header([Level], [Enforced By], [Mechanism], [Status]),
  [Per-derivation wall-clock timeout],
  [Builder],
  [`tokio::time::timeout` wrapping the nix-daemon build. Duration is
    `WorkAssignment.build_options.build_timeout` if nonzero, else
    `DEFAULT_DAEMON_TIMEOUT` (7200s / 2h). Configurable via
    `RIO_DAEMON_TIMEOUT_SECS`, `--daemon-timeout-secs`, or `builder.toml`.],
  [*Implemented*],

  [Per-derivation silence timeout],
  [Executor],
  [`maxSilentTime` (kill if no output for N seconds) enforced by a `select!`
    arm in the stderr read loop. Resets on each output-producing message
    (`STDERR_NEXT`, `STDERR_RESULT BuildLogLine`). The nix-daemon subprocess
    MAY also enforce it (forwarded via `client_set_options`); rio-side is the
    authoritative backstop. See
    #cross-link("/spec/components/builder.typ")[`executor.silence.timeout-kill`].],
  [*Implemented*],

  [Per-build overall timeout],
  [Scheduler `handle_tick`],
  [Wall-clock limit on the entire build from submission. When
    `submitted_at.elapsed() > BuildOptions.build_timeout`, scheduler cancels
    non-terminal derivations and transitions the build to `Failed` with
    error_summary "build_timeout Ns exceeded". Zero = no overall timeout. See
    #cross-link("/spec/components/scheduler.typ")[`sched.timeout.per-build`].],
  [*Implemented*],

  [Scheduler backstop timeout],
  [`handle_tick`],
  [When a Running derivation's `running_since.elapsed()` exceeds
    `max(est_duration × 3, daemon_timeout + 10min)`, scheduler force-closes
    the execution (the controller's Job deletion aborts the pod) + resets to
    Ready + increments retry_count + adds executor to
    failed_builders. Catches "executor heartbeating but daemon wedged."],
  [Implemented (Phase 3b)],
)

= Error Propagation: What the Client Sees

#table(
  columns: 2,
  table.header([Internal Failure], [Client-Visible Behavior]),
  [Executor OOM-killed],
  [Build retried on another executor. Client sees continued STDERR streaming.
    If all retries exhausted: `TransientFailure`.],

  [S3 unavailable],
  [Upload retried with backoff. If persistent: `TransientFailure` for the
    derivation, reassigned.],

  [PostgreSQL down],
  [Gateway returns `STDERR_ERROR("build service temporarily unavailable")`.
    Client can retry.],

  [Scheduler failover],
  [Gateway's `BuildEvent` stream breaks with a `Transport` or
    `EofWithoutTerminal` error. Gateway transparently reconnects via
    `WatchBuild(build_id)` up to #(refs.const)("MAX_RECONNECT")× with
    backoff (1/2/4/8/16 s, capped at 16 s); the scheduler's snapshot-first
    attach resynchronizes the gateway's display state. If reconnect budget
    exhausted → `MiscFailure` to client.],

  [Gateway crash],
  [SSH connection drops. Client reconnects; build reattaches via #gls("dag")-merge
    cache hits (stored outputs are instant-hit). Logs between crash and
    reconnect are lost unless log persistence is configured.],

  [Derivation poisoned],
  [`CachedFailure` with message identifying the poisoned derivation and the
    failure history.],
)

= Full error inventory

#let errs = json("/gen/errors.json").variants
#table(
  columns: 3,
  [*Crate*], [*Variant*], [*Message*],
  ..errs.map(e => ([#e.crate], raw(e.name), [#e.msg])).flatten(),
)
