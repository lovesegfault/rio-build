//! Worker connection state: [`WorkerState`] (heartbeat/capacity tracking)
//! and [`RetryPolicy`] (backoff configuration).
//!
//! A worker is "registered" once BOTH a BuildExecution stream opens
//! (`stream_tx`) AND a heartbeat arrives (`system`). Neither alone
//! suffices — can't dispatch to a worker we can't reach, and can't
//! match its system/features without a heartbeat.

use std::collections::HashSet;
use std::time::Instant;

use super::{DrvHash, WorkerId};

/// In-memory state for a connected worker.
#[derive(Debug)]
pub struct WorkerState {
    /// Unique worker ID (from pod UID).
    pub worker_id: WorkerId,
    /// Target systems (e.g., ["x86_64-linux", "aarch64-linux"] for
    /// a multi-arch worker). Populated on first heartbeat. Empty
    /// vec = no heartbeat yet (not registered). can_build() does
    /// any-match against the derivation's singular target system.
    pub systems: Vec<String>,
    /// Features this worker supports.
    pub supported_features: Vec<String>,
    /// Maximum concurrent builds.
    pub max_builds: u32,
    /// Derivation hashes currently being built by this worker.
    pub running_builds: HashSet<DrvHash>,
    /// Channel to send scheduler messages (assignments, cancels) to the worker.
    /// Set when the BuildExecution stream opens.
    pub stream_tx: Option<tokio::sync::mpsc::Sender<rio_proto::types::SchedulerMessage>>,
    /// Timestamp of last heartbeat (for timeout detection).
    pub last_heartbeat: Instant,
    /// Number of consecutive missed heartbeats.
    pub missed_heartbeats: u32,
    /// Bloom filter of store paths this worker has cached (from heartbeat).
    /// `None` until the first heartbeat with a filter arrives. Used by
    /// `assignment::best_worker()` for transfer-cost scoring — a worker
    /// that already has most of a derivation's inputs is preferred.
    ///
    /// Stale-by-design: updated every 10s (heartbeat interval), so the
    /// snapshot is up to 10s behind. That's fine for a scoring HINT —
    /// the actual fetch still happens on the worker if the filter lied
    /// (false positive) or was stale (evicted since last heartbeat).
    pub bloom: Option<rio_common::bloom::BloomFilter>,
    /// Size class (e.g., "small", "large") reported by the worker.
    /// `None` = worker didn't declare a class. If the scheduler has
    /// size_classes configured, best_worker() REJECTS unclassified
    /// workers (misconfiguration — visible failure, not silent wildcard).
    /// If the scheduler has no size_classes, this field is ignored.
    pub size_class: Option<String>,
    /// Stop accepting new assignments (graceful shutdown).
    ///
    /// Set by `AdminService.DrainWorker` which the worker's SIGTERM
    /// handler calls as step 1 of preStop. `has_capacity()` checks this
    /// so `best_worker()` filters draining workers out — no explicit
    /// filter in assignment.rs needed. In-flight builds continue; no
    /// new work. The worker then waits for in-flight completion (step
    /// 2) and exits. terminationGracePeriodSeconds=7200 gives 2h.
    ///
    /// One-way: no "un-drain." A draining worker is on its way out; the
    /// only recovery is a fresh pod (new worker_id). This simplifies the
    /// state machine — no drain/undrain race with dispatch.
    ///
    /// NOT cleared on reconnect: if a draining worker's stream drops and
    /// reopens (transient network blip inside the grace period), it stays
    /// draining. Reconnect creates a fresh WorkerState (draining=false by
    /// default) only if `handle_worker_disconnected` ran first (removes
    /// the entry). If the stream blips without full disconnect, the
    /// existing entry (with draining=true) is reused by `handle_heartbeat`
    /// via `entry().or_insert_with()`. Either behavior is acceptable:
    /// worst case, a reconnected-during-drain worker briefly accepts one
    /// assignment before the next DrainWorker call (which the preStop
    /// hook sends on every SIGTERM, so it would re-drain).
    pub draining: bool,
}

impl WorkerState {
    /// Create an unregistered worker entry. Registration completes when both
    /// a BuildExecution stream connects (sets `stream_tx`) and a heartbeat
    /// arrives (sets `system`/`max_builds`).
    pub fn new(worker_id: WorkerId) -> Self {
        Self {
            worker_id,
            systems: Vec::new(),
            supported_features: Vec::new(),
            max_builds: 0,
            running_builds: HashSet::new(),
            stream_tx: None,
            last_heartbeat: Instant::now(),
            missed_heartbeats: 0,
            bloom: None,
            size_class: None,
            draining: false,
        }
    }

    /// Whether we have received both a stream connection and a
    /// heartbeat. Derived from `stream_tx.is_some() &&
    /// !systems.is_empty()` — no manual bookkeeping, so the two
    /// channels can't get out of sync. A heartbeat with an empty
    /// systems vec would mean a misconfigured worker (no system
    /// to build for); treating it as unregistered surfaces the
    /// misconfig via "worker never dispatches" rather than a
    /// silent wildcard.
    pub fn is_registered(&self) -> bool {
        self.stream_tx.is_some() && !self.systems.is_empty()
    }

    /// Whether this worker has available build capacity.
    ///
    /// `!draining` first: short-circuit the arithmetic for draining
    /// workers. `best_worker()` calls this in a hot-ish loop over
    /// candidates; a draining worker is common during scale-down.
    pub fn has_capacity(&self) -> bool {
        !self.draining
            && self.is_registered()
            && (self.running_builds.len() as u32) < self.max_builds
    }

    /// Whether this worker can build the given derivation based on
    /// system and features. The derivation has a SINGLE target
    /// system; the worker may support multiple (any-match). All
    /// required features must be present (all-match).
    pub fn can_build(&self, system: &str, required_features: &[String]) -> bool {
        if !self.is_registered() {
            return false;
        }
        // Derivation's target system must be among the worker's
        // supported systems. iter().any() — a multi-arch worker
        // can build either arch.
        if !self.systems.iter().any(|s| s == system) {
            return false;
        }
        // All required features must be present on the worker.
        // iter().all() — a derivation needing [kvm, big-parallel]
        // needs BOTH; a worker with just [kvm] can't build it.
        required_features
            .iter()
            .all(|f| self.supported_features.contains(f))
    }
}

/// Retry policy configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retries for transient failures.
    pub max_retries: u32,
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
        let final_secs = with_jitter.max(0.0);

        std::time::Duration::from_secs_f64(final_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_worker(systems: Vec<&str>, features: Vec<&str>) -> WorkerState {
        let mut w = WorkerState::new("test".into());
        w.systems = systems.into_iter().map(Into::into).collect();
        w.supported_features = features.into_iter().map(Into::into).collect();
        // is_registered() checks stream_tx.is_some() — fake it.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        w.stream_tx = Some(tx);
        w
    }

    /// Multi-arch worker: any-match on system. A worker declaring
    /// both archs can build derivations for either.
    #[test]
    fn can_build_multi_system_any_match() {
        let w = registered_worker(vec!["x86_64-linux", "aarch64-linux"], vec![]);
        assert!(w.can_build("x86_64-linux", &[]));
        assert!(w.can_build("aarch64-linux", &[]));
        assert!(
            !w.can_build("riscv64-linux", &[]),
            "not in worker's systems"
        );
    }

    /// Features: all-match. Derivation needing [kvm, big-parallel]
    /// dispatches only to workers with BOTH. Previously features
    /// was hardcoded to Vec::new() in worker heartbeat — this test
    /// proves the scheduler side correctly honors them now that
    /// they flow through.
    #[test]
    fn can_build_features_all_match() {
        let w = registered_worker(vec!["x86_64-linux"], vec!["kvm", "big-parallel"]);
        assert!(w.can_build("x86_64-linux", &["kvm".into()]));
        assert!(w.can_build("x86_64-linux", &["kvm".into(), "big-parallel".into()]));
        assert!(
            !w.can_build("x86_64-linux", &["kvm".into(), "nixos-test".into()]),
            "worker missing one required feature → can't build"
        );
        // Worker with features can still build featureless derivs.
        assert!(w.can_build("x86_64-linux", &[]));
    }

    /// Empty systems = not registered. Prevents a misconfigured
    /// worker (no systems to build for) from being treated as a
    /// wildcard.
    #[test]
    fn can_build_empty_systems_not_registered() {
        let mut w = WorkerState::new("test".into());
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
}
