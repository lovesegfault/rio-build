//! Executor connection state: [`ExecutorState`] (heartbeat/capacity tracking)
//! and [`RetryPolicy`] (backoff configuration).
//!
//! An executor is "registered" once BOTH a BuildExecution stream opens
//! (`stream_tx`) AND a heartbeat arrives (`system`). Neither alone
//! suffices — can't dispatch to an executor we can't reach, and can't
//! match its system/features without a heartbeat.

use std::time::Instant;

use rio_proto::types::ExecutorKind;

use super::{DrvHash, ExecutorId};

/// In-memory state for a connected executor.
#[derive(Debug)]
pub struct ExecutorState {
    /// Unique executor ID (from pod UID).
    pub executor_id: ExecutorId,
    /// Builder (airgapped) or fetcher (open egress, FOD-only). Populated
    /// on first heartbeat from `HeartbeatRequest.kind`. Default Builder
    /// (wire-default = 0 = Builder) so pre-ADR-019 executors that don't
    /// send the field are treated as builders. `hard_filter()` uses this
    /// for spec sched.dispatch.fod-to-fetcher — FODs route only to
    /// fetchers, non-FODs only to builders, never falling back across
    /// kinds (spec sched.dispatch.no-fod-fallback).
    pub kind: ExecutorKind,
    /// Target systems (e.g., ["x86_64-linux", "aarch64-linux"] for
    /// a multi-arch executor). Populated on first heartbeat. Empty
    /// vec = no heartbeat yet (not registered). can_build() does
    /// any-match against the derivation's singular target system.
    pub systems: Vec<String>,
    /// Features this executor supports.
    pub supported_features: Vec<String>,
    /// Derivation currently being built by this executor (P0537: at
    /// most one). `None` = idle.
    pub running_build: Option<DrvHash>,
    /// Channel to send scheduler messages (assignments, cancels) to the executor.
    /// Set when the BuildExecution stream opens.
    pub stream_tx: Option<tokio::sync::mpsc::Sender<rio_proto::types::SchedulerMessage>>,
    /// Timestamp of last heartbeat (for timeout detection).
    pub last_heartbeat: Instant,
    /// Number of consecutive missed heartbeats.
    pub missed_heartbeats: u32,
    /// Bloom filter of store paths this executor has cached (from heartbeat).
    /// `None` until the first heartbeat with a filter arrives. Used by
    /// `assignment::best_executor()` for transfer-cost scoring — an executor
    /// that already has most of a derivation's inputs is preferred.
    ///
    /// Stale-by-design: updated every 10s (heartbeat interval), so the
    /// snapshot is up to 10s behind. That's fine for a scoring HINT —
    /// the actual fetch still happens on the executor if the filter lied
    /// (false positive) or was stale (evicted since last heartbeat).
    pub bloom: Option<rio_common::bloom::BloomFilter>,
    /// Size class (e.g., "small", "large") reported by the executor.
    /// `None` = executor didn't declare a class. If the scheduler has
    /// size_classes configured, best_executor() REJECTS unclassified
    /// executors (misconfiguration — visible failure, not silent wildcard).
    /// If the scheduler has no size_classes, this field is ignored.
    pub size_class: Option<String>,
    /// Stop accepting new assignments (graceful shutdown).
    ///
    /// Set by `AdminService.DrainExecutor` which the executor's SIGTERM
    /// handler calls as step 1 of preStop. `has_capacity()` checks this
    /// (and `store_degraded` below) so `best_executor()` filters both out
    /// — no explicit filter in assignment.rs needed. In-flight builds
    /// continue; no
    /// new work. The executor then waits for in-flight completion (step
    /// 2) and exits. terminationGracePeriodSeconds=7200 gives 2h.
    ///
    /// One-way: no "un-drain." A draining executor is on its way out; the
    /// only recovery is a fresh pod (new executor_id). This simplifies the
    /// state machine — no drain/undrain race with dispatch.
    ///
    /// NOT cleared on reconnect: if a draining executor's stream drops and
    /// reopens (transient network blip inside the grace period), it stays
    /// draining. Reconnect creates a fresh ExecutorState (draining=false by
    /// default) only if `handle_executor_disconnected` ran first (removes
    /// the entry). If the stream blips without full disconnect, the
    /// existing entry (with draining=true) is reused by `handle_heartbeat`
    /// via `entry().or_insert_with()`. Either behavior is acceptable:
    /// worst case, a reconnected-during-drain executor briefly accepts one
    /// assignment before the next DrainExecutor call (which the preStop
    /// hook sends on every SIGTERM, so it would re-drain).
    pub draining: bool,
    /// SIGTERM received on the worker — finishing in-flight, not
    /// accepting new work. Worker-authoritative: set/cleared on
    /// every heartbeat from `HeartbeatRequest.draining` (I-063).
    /// Distinct from `draining` above: that one is scheduler-side
    /// (DrainExecutor RPC, controller/operator), this one is the
    /// worker reporting its OWN process state. `is_draining()`
    /// (and `has_capacity()`) check the OR.
    ///
    /// The split matters when the SAME draining process reconnects
    /// after a scheduler restart: I-056a's reconnect-clear correctly
    /// resets the admin `draining` (stale session), but must NOT
    /// reset this — the worker re-asserts it on the next heartbeat
    /// anyway, but in the gap the new scheduler would mis-dispatch.
    /// Heartbeat is the only writer.
    pub draining_hb: bool,
    /// Last derivation this executor sent a `CompletionReport` for
    /// (any terminal status). Set in `handle_completion` alongside the
    /// `running_build = None` clear. In-memory only.
    ///
    /// I-197: refines the I-188 ephemeral-disconnect gate. An ephemeral
    /// disconnect with `running_build == Some(X)` and `last_completed
    /// != Some(X)` means X was mid-build when the pod died (OOMKilled)
    /// → promote `size_class_floor`. Only when `last_completed ==
    /// running_build` is the disconnect the expected post-completion
    /// one-shot exit (the I-188 race) → suppress promotion. The
    /// blanket I-188 suppress made an OOMKilled `tiny` ephemeral loop
    /// at the same class for hours (openssl, `size_class_floor` empty).
    pub last_completed: Option<DrvHash>,
    /// FUSE circuit breaker open on the executor — it can't fetch inputs
    /// from rio-store. Treated like `draining`: `has_capacity()` returns
    /// false, `best_executor()` excludes it. Unlike `draining` this is
    /// two-way: the executor clears it when the breaker closes/half-opens
    /// (next heartbeat). Wire-default false — old executors don't send it.
    ///
    /// Set from `HeartbeatRequest.store_degraded` in `handle_heartbeat`.
    pub store_degraded: bool,
    /// When this ExecutorState was created (= stream open or first
    /// heartbeat, whichever came first via `entry().or_insert_with()`).
    /// Reported in `ListExecutors`.
    pub connected_since: Instant,
    /// Last `ResourceUsage` from heartbeat. `None` until the first
    /// heartbeat with resources. Reported in `ListExecutors`. Not
    /// cleared on heartbeats that omit resources (keep the last-known
    /// reading rather than clobbering with `None`).
    pub last_resources: Option<rio_proto::types::ResourceUsage>,
    /// Warm-gate: `true` once  the executor has ACKed the initial
    /// `PrefetchHint` (sent `PrefetchComplete` on the BuildExecution
    /// stream). Cold executors (`warm=false`) are filtered out of
    /// `best_executor()` candidates UNLESS no warm executor passes the
    /// hard filter. Default `false` on registration; flipped by
    /// `handle_prefetch_complete()`. When the ready queue is empty
    /// at registration time the scheduler flips this `true`
    /// immediately (nothing to prefetch for).
    ///
    /// Ephemeral pools (`r[ctrl.pool.ephemeral]`): every executor
    /// starts cold. Without this gate the first build on a fresh
    /// Job-builder eats full-closure fetch latency on every input
    /// path. With it, the scheduler waits for cache warm before
    /// dispatching — adds ~prefetch-time to time-to-first-dispatch,
    /// but the build itself runs at warm speed.
    pub warm: bool,
    /// One-shot Job executor (`r[ctrl.pool.ephemeral]`): exits after
    /// completing its single build. Reported via
    /// `HeartbeatRequest.ephemeral`. When `true`,
    /// `handle_process_completion` marks `draining=true` immediately
    /// on slot-free so the same actor turn's `dispatch_ready` doesn't
    /// re-assign to the about-to-exit slot (I-188:
    /// `r[sched.ephemeral.no-redispatch-after-completion]`).
    /// Wire-default `false` — pre-I-188 executors don't send it and
    /// are treated as long-lived (their freed slot is real capacity).
    pub ephemeral: bool,
    /// Prior heartbeat's reconcile KEPT `running_build` (still
    /// Assigned/Running in DAG) but the worker did NOT report it.
    /// One miss = TOCTOU race (assignment landed between worker
    /// snapshot and heartbeat arrival, ~10s). Two consecutive misses
    /// = phantom: assignment over 10s old, worker still doesn't know.
    /// Either the completion was lost in transit (I-032 pre-d11245b4)
    /// or the send succeeded into a stream that died right after.
    /// The slot is dead capacity until cleared.
    ///
    /// `handle_heartbeat` checks this against the current miss; on
    /// second hit, resets the drv to Ready + clears `running_build`.
    /// In-memory only — restart clears all executor state, and
    /// post-recovery `handle_reconcile_assignments` is the cold-start
    /// equivalent.
    pub phantom_suspect: Option<DrvHash>,
}

impl ExecutorState {
    /// Create an unregistered executor entry. Registration completes when both
    /// a BuildExecution stream connects (sets `stream_tx`) and a heartbeat
    /// arrives (sets `systems`).
    pub fn new(executor_id: ExecutorId) -> Self {
        Self {
            executor_id,
            // Wire-default = Builder. Overwritten on first heartbeat
            // from `HeartbeatRequest.kind`. Pre-ADR-019 executors that
            // don't send the field stay Builder (correct — they're all
            // builders until the fetcher rollout).
            kind: ExecutorKind::Builder,
            systems: Vec::new(),
            supported_features: Vec::new(),
            running_build: None,
            stream_tx: None,
            last_heartbeat: Instant::now(),
            missed_heartbeats: 0,
            bloom: None,
            size_class: None,
            draining: false,
            draining_hb: false,
            last_completed: None,
            store_degraded: false,
            connected_since: Instant::now(),
            last_resources: None,
            // Warm-gate: cold until PrefetchComplete (or until the
            // registration hook flips it for an empty ready-queue).
            warm: false,
            ephemeral: false,
            phantom_suspect: None,
        }
    }

    /// Whether we have received both a stream connection and a
    /// heartbeat. Derived from `stream_tx.is_some() &&
    /// !systems.is_empty()` — no manual bookkeeping, so the two
    /// channels can't get out of sync. A heartbeat with an empty
    /// systems vec would mean a misconfigured executor (no system
    /// to build for); treating it as unregistered surfaces the
    /// misconfig via "executor never dispatches" rather than a
    /// silent wildcard.
    pub fn is_registered(&self) -> bool {
        self.stream_tx.is_some() && !self.systems.is_empty()
    }

    /// Whether this executor has available build capacity.
    ///
    /// `!draining && !store_degraded` first: short-circuit the
    /// is_empty for excluded executors. `best_executor()` calls this in
    /// a hot-ish loop over candidates; draining executors are common
    /// during scale-down, degraded executors during store outages.
    // r[impl builder.heartbeat.store-degraded]
    pub fn has_capacity(&self) -> bool {
        !self.is_draining()
            && !self.store_degraded
            && self.is_registered()
            // I-095: is_registered() only checks is_some(), not
            // is_closed() (gauge accounting needs the distinction).
            // Kept in sync with rejection_reason()'s stream-closed.
            && !self.stream_tx.as_ref().is_some_and(|tx| tx.is_closed())
            && self.running_build.is_none()
    }

    /// Effective drain state: scheduler-side OR worker-side. The
    /// only correct read of "is this executor draining" — callers
    /// (has_capacity, ClusterStatus, diagnostics) use this, not the
    /// raw fields.
    pub fn is_draining(&self) -> bool {
        self.draining || self.draining_hb
    }

    /// Whether this executor can build the given derivation based on
    /// system and features. The derivation has a SINGLE target
    /// system; the executor may support multiple (any-match). All
    /// required features must be present (all-match).
    pub fn can_build(&self, system: &str, required_features: &[String]) -> bool {
        if !self.is_registered() {
            return false;
        }
        // Derivation's target system must be among the executor's
        // supported systems. iter().any() — a multi-arch executor
        // can build either arch.
        if !self.systems.iter().any(|s| s == system) {
            return false;
        }
        // All required features must be present on the executor.
        // iter().all() — a derivation needing [kvm, big-parallel]
        // needs BOTH; an executor with just [kvm] can't build it.
        required_features
            .iter()
            .all(|f| self.supported_features.contains(f))
    }
}

/// Retry policy configuration.
///
/// `#[serde(default)]` on the struct → absent keys fall through to
/// `Default::default()`, so `[retry] max_retries = 5` leaves the
/// backoff curve unchanged. `PartialEq` is for the TOML-roundtrip
/// tests (`assert_eq!(cfg.retry, RetryPolicy::default())`). Float
/// fields mean this is a BITWISE compare — acceptable for config
/// (the test just asserts default-constructed identity, not
/// computed-value equality).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RetryPolicy {
    /// Maximum number of retries for transient failures.
    pub max_retries: u32,
    /// Maximum number of retries for InfrastructureFailure. Higher
    /// than `max_retries` because infra failures are executor-local
    /// (not the build's fault) — but NOT unbounded: a scheduler-side
    /// bug that misclassifies a deterministic failure as infra
    /// (e.g., empty CA input path → MetadataFetch error) would
    /// otherwise hot-loop forever. Observed: 9748 re-dispatches in
    /// one session before the CA-path-propagation fix landed.
    pub max_infra_retries: u32,
    /// Maximum number of `TimedOut` re-dispatches before the
    /// derivation goes terminal (`Cancelled`). Each `TimedOut` retry
    /// promotes `size_class_floor` (I-200, `r[sched.timeout.promote-
    /// on-exceed]`), so this caps how many size-class steps a build
    /// can climb on timeout before the operator gets a visible
    /// failure. Default 4: tiny→small→medium→large→xlarge is 4
    /// promotions; a build that times out on xlarge is genuinely
    /// stuck. Separate counter from `max_retries` (timeouts don't
    /// eat the transient budget) and from `max_infra_retries` (no
    /// time-window reset — sparse timeouts over hours are still the
    /// same hung build).
    pub max_timeout_retries: u32,
    /// Seconds since the LAST infra failure after which the
    /// `infra_retry_count` is reset to 0. Infra failures are by
    /// definition transient — N quick failures suggests a
    /// misclassified permanent error, but N failures spread over
    /// hours are independent incidents and shouldn't accumulate
    /// toward poison. I-127: a leaked PutPath lock caused 4 builders
    /// in a row to hit "concurrent PutPath"; the drv was poisoned at
    /// 99.7% despite being fine. With a window, the counter resets
    /// once the cluster self-heals (lock released, store recovered).
    pub infra_retry_window_secs: f64,
    /// Base backoff duration in seconds.
    pub backoff_base_secs: f64,
    /// Backoff multiplier.
    pub backoff_multiplier: f64,
    /// Maximum backoff duration in seconds.
    pub backoff_max_secs: f64,
    /// Jitter fraction (0.0 to 1.0).
    pub jitter_fraction: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            // InfrastructureFailure has NO backoff (re-dispatch is
            // immediate) so a misclassified permanent failure hot-loops.
            // Observed: 12 derivations cycled 146 times in 6 minutes
            // when an S3 auth failure was reported as infra. The cap
            // converts that into a visible poison.
            //
            // I-127 raised 5→10 + added the time-window reset below.
            // 5 was too tight under shallow-1024x: a leaked PutPath
            // lock made 4 builders in a row report infra → poison at
            // 99.7% on a perfectly buildable drv. 10 attempts (no
            // backoff, so still seconds-to-low-minutes for fast
            // builds) gives the cluster room to self-heal, and the
            // 5-min window means slow-drip failures don't accumulate.
            // A true misclassified permanent failure still poisons in
            // 10 immediate cycles — well under a minute.
            max_infra_retries: 10,
            // I-200: 4 = number of promotions to walk a 5-class
            // ladder (tiny→xlarge). After that, terminal Cancelled.
            // Each retry goes to a larger class with a 5× longer
            // activeDeadlineSeconds, so the wall-clock cost is
            // bounded by Σ(cutoff_i × 5) ≈ 5× the largest class
            // cutoff — not unbounded.
            max_timeout_retries: 4,
            // 5min: long enough that a stuck lock (I-125a's leaked
            // PutPath, ~tens of seconds to recover) or a store
            // restart doesn't compound across attempts; short enough
            // that the 9748-dispatch hot-loop scenario above
            // (146 cycles / 6min ≈ 2.5s/cycle) still hits the cap
            // before the window resets it.
            infra_retry_window_secs: 300.0,
            backoff_base_secs: 5.0,
            backoff_multiplier: 2.0,
            backoff_max_secs: 300.0,
            jitter_fraction: 0.2,
        }
    }
}

impl RetryPolicy {
    /// Compute the backoff duration for a given retry attempt.
    pub fn backoff_duration(&self, attempt: u32) -> std::time::Duration {
        use rand::Rng;

        let base = self.backoff_base_secs * self.backoff_multiplier.powi(attempt as i32);
        let clamped = base.min(self.backoff_max_secs);

        // Apply jitter: duration * (1 +/- jitter_fraction * random)
        let mut rng = rand::rng();
        let jitter = rng.random_range(-self.jitter_fraction..=self.jitter_fraction);
        let with_jitter = clamped * (1.0 + jitter);
        // Clamp to [0, 1yr]. max(0.0) handles NaN (NaN.max(x) = x);
        // min(1yr) handles infinity (from_secs_f64(inf) PANICS).
        // A 1-year backoff is far above any sane value — the clamp
        // exists to prevent a crash from misconfigured backoff_max_
        // secs=inf (e.g., parsed from TOML that had "inf" literally).
        //
        // NOT using .clamp() (clippy suggestion): clamp returns NaN
        // if input is NaN, which would then panic from_secs_f64.
        // The .max(0.0) form handles NaN correctly (NaN.max(x) = x
        // per IEEE 754).
        const MAX_BACKOFF_SECS: f64 = 365.0 * 86400.0;
        #[allow(clippy::manual_clamp)]
        let final_secs = with_jitter.max(0.0).min(MAX_BACKOFF_SECS);

        std::time::Duration::from_secs_f64(final_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_executor(systems: Vec<&str>, features: Vec<&str>) -> ExecutorState {
        let mut w = ExecutorState::new("test".into());
        w.systems = systems.into_iter().map(Into::into).collect();
        w.supported_features = features.into_iter().map(Into::into).collect();
        // is_registered() checks stream_tx.is_some(); has_capacity()
        // additionally checks !is_closed() (I-095). forget(rx) keeps
        // the channel open — see assignment.rs make_worker for the
        // same pattern + rationale.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        std::mem::forget(rx);
        w.stream_tx = Some(tx);
        w
    }

    /// Multi-arch executor: any-match on system. A executor declaring
    /// both archs can build derivations for either.
    #[test]
    fn can_build_multi_system_any_match() {
        let w = registered_executor(vec!["x86_64-linux", "aarch64-linux"], vec![]);
        assert!(w.can_build("x86_64-linux", &[]));
        assert!(w.can_build("aarch64-linux", &[]));
        assert!(
            !w.can_build("riscv64-linux", &[]),
            "not in executor's systems"
        );
    }

    /// Features: all-match. Derivation needing [kvm, big-parallel]
    /// dispatches only to executors with BOTH. This test proves the
    /// scheduler side correctly honors features when they flow
    /// through from the executor heartbeat.
    #[test]
    fn can_build_features_all_match() {
        let w = registered_executor(vec!["x86_64-linux"], vec!["kvm", "big-parallel"]);
        assert!(w.can_build("x86_64-linux", &["kvm".into()]));
        assert!(w.can_build("x86_64-linux", &["kvm".into(), "big-parallel".into()]));
        assert!(
            !w.can_build("x86_64-linux", &["kvm".into(), "nixos-test".into()]),
            "executor missing one required feature → can't build"
        );
        // Executor with features can still build featureless derivs.
        assert!(w.can_build("x86_64-linux", &[]));
    }

    /// store_degraded gates has_capacity() regardless of running state.
    /// Set every OTHER capacity input favorably (not draining, fully
    /// registered, running empty) so the only reason has_capacity can
    /// return false is the store_degraded flag itself.
    ///
    /// Precondition assert proves the test is not trivially passing:
    /// with store_degraded=false, the same executor DOES have capacity.
    #[test]
    fn has_capacity_gates_on_store_degraded() {
        let mut w = registered_executor(vec!["x86_64-linux"], vec![]);
        // running_build None by default.
        assert!(!w.draining, "precondition: not draining");
        assert!(
            w.has_capacity(),
            "precondition: healthy executor has capacity (test would \
             pass trivially without this — store_degraded=true below \
             must be the ONLY reason has_capacity flips)"
        );

        w.store_degraded = true;
        assert!(
            !w.has_capacity(),
            "store_degraded=true → has_capacity false even with \
             running_build=None and draining=false"
        );

        // Two-way (unlike draining): clearing the flag restores
        // capacity. Executor recovers when the FUSE breaker closes.
        w.store_degraded = false;
        assert!(
            w.has_capacity(),
            "store_degraded cleared → capacity restored"
        );
    }

    /// Empty systems = not registered. Prevents a misconfigured
    /// executor (no systems to build for) from being treated as a
    /// wildcard.
    #[test]
    fn can_build_empty_systems_not_registered() {
        let mut w = ExecutorState::new("test".into());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        w.stream_tx = Some(tx);
        // systems still empty
        assert!(!w.is_registered(), "stream but no systems → not ready");
        assert!(!w.can_build("x86_64-linux", &[]));
    }

    #[test]
    fn test_retry_backoff() {
        let policy = RetryPolicy::default();
        let d0 = policy.backoff_duration(0);
        let d1 = policy.backoff_duration(1);

        // Base is 5s, so first attempt should be around 5s +/- jitter
        assert!(d0.as_secs_f64() > 3.0 && d0.as_secs_f64() < 7.0);
        // Second attempt should be around 10s +/- jitter
        assert!(d1.as_secs_f64() > 7.0 && d1.as_secs_f64() < 13.0);
    }

    /// Regression: backoff_max_secs = infinity (e.g., from a
    /// misconfigured TOML that had "inf" literally) must not panic
    /// in Duration::from_secs_f64. The 1-year clamp catches it.
    #[test]
    fn test_retry_backoff_infinity_clamped() {
        let policy = RetryPolicy {
            backoff_max_secs: f64::INFINITY,
            ..Default::default()
        };
        // Large attempt → base * multiplier^N → unbounded → .min(inf)
        // = inf → from_secs_f64(inf) would PANIC without the clamp.
        let d = policy.backoff_duration(100);
        // Clamped to 1yr. Jitter is applied BEFORE the clamp, and
        // inf * (1 +/- jitter) = inf, so the final value is exactly
        // 1yr (jitter has no effect on infinity).
        assert!(
            d.as_secs() <= 366 * 86400,
            "infinity backoff clamped to ~1yr"
        );
    }
}
