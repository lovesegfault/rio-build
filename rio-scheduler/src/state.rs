//! Derivation and build state machines.
//!
//! All state transitions are validated by the state machine rules:
//! - Derivation: created -> queued -> ready -> assigned -> running -> completed|failed|poisoned
//! - Build: pending -> active -> succeeded|failed|cancelled
//!
//! Idempotency rules:
//! - completed -> completed: no-op
//! - poisoned -> poisoned: no-op
//! - terminal -> non-terminal: rejected (except poisoned -> created on 24h TTL expiry)

use std::collections::HashSet;
use std::time::Instant;

use uuid::Uuid;

/// State of a single derivation in the global DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DerivationStatus {
    Created,
    Queued,
    Ready,
    Assigned,
    Running,
    Completed,
    Failed,
    Poisoned,
}

impl DerivationStatus {
    /// Whether this is a terminal state (no further progress without external reset).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Poisoned)
    }

    /// Validate a state transition.
    ///
    /// Returns `Ok(())` if the transition is valid, `Err` with a description otherwise.
    pub fn validate_transition(self, to: Self) -> Result<(), TransitionError> {
        // Idempotent no-ops
        if self == to {
            match self {
                Self::Completed => return Ok(()),
                Self::Poisoned => return Ok(()),
                _ => {
                    return Err(TransitionError::Invalid {
                        from: self,
                        to,
                        reason: "non-terminal self-transition is not allowed",
                    });
                }
            }
        }

        // Terminal -> non-terminal is rejected (except poisoned -> created via TTL)
        if self.is_terminal() && !to.is_terminal() {
            if self == Self::Poisoned && to == Self::Created {
                // Allowed: 24h TTL expiry resets poisoned -> created
                return Ok(());
            }
            return Err(TransitionError::TerminalToNonTerminal { from: self, to });
        }

        // Valid transitions
        let valid = match (self, to) {
            (Self::Created, Self::Completed) => true, // cache hit
            (Self::Created, Self::Queued) => true,    // build accepted
            (Self::Queued, Self::Ready) => true,      // all deps complete
            (Self::Ready, Self::Assigned) => true,    // worker selected
            (Self::Assigned, Self::Running) => true,  // worker ack
            (Self::Assigned, Self::Ready) => true,    // worker lost
            (Self::Running, Self::Completed) => true, // build succeeded
            (Self::Running, Self::Failed) => true,    // retriable failure
            (Self::Running, Self::Poisoned) => true,  // failed on 3+ workers
            (Self::Failed, Self::Ready) => true,      // retry scheduled
            _ => false,
        };

        if valid {
            Ok(())
        } else {
            Err(TransitionError::Invalid {
                from: self,
                to,
                reason: "transition not in state machine",
            })
        }
    }

    /// Convert to the wire/database string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Queued => "queued",
            Self::Ready => "ready",
            Self::Assigned => "assigned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Poisoned => "poisoned",
        }
    }
}

impl std::fmt::Display for DerivationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DerivationStatus {
    type Err = TransitionError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "created" => Ok(Self::Created),
            "queued" => Ok(Self::Queued),
            "ready" => Ok(Self::Ready),
            "assigned" => Ok(Self::Assigned),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "poisoned" => Ok(Self::Poisoned),
            other => Err(TransitionError::UnknownStatus(other.to_string())),
        }
    }
}

/// State of a build request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildState {
    Pending,
    Active,
    Succeeded,
    Failed,
    Cancelled,
}

impl BuildState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub fn validate_transition(self, to: Self) -> Result<(), TransitionError> {
        if self == to {
            return Err(TransitionError::InvalidBuild { from: self, to });
        }
        if self.is_terminal() {
            return Err(TransitionError::TerminalBuild { from: self, to });
        }

        let valid = match (self, to) {
            (Self::Pending, Self::Active) => true,
            (Self::Active, Self::Succeeded) => true,
            (Self::Active, Self::Failed) => true,
            (Self::Active, Self::Cancelled) => true,
            // Pending can be cancelled before becoming active
            (Self::Pending, Self::Cancelled) => true,
            _ => false,
        };

        if valid {
            Ok(())
        } else {
            Err(TransitionError::InvalidBuild { from: self, to })
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for BuildState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BuildState {
    type Err = TransitionError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "active" => Ok(Self::Active),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(TransitionError::UnknownBuildState(other.to_string())),
        }
    }
}

/// Errors from state machine transitions.
#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("invalid derivation transition {from} -> {to}: {reason}")]
    Invalid {
        from: DerivationStatus,
        to: DerivationStatus,
        reason: &'static str,
    },

    #[error("cannot transition from terminal state {from} to {to}")]
    TerminalToNonTerminal {
        from: DerivationStatus,
        to: DerivationStatus,
    },

    #[error("unknown derivation status: {0}")]
    UnknownStatus(String),

    #[error("invalid build state transition {from} -> {to}")]
    InvalidBuild { from: BuildState, to: BuildState },

    #[error("cannot transition from terminal build state {from} to {to}")]
    TerminalBuild { from: BuildState, to: BuildState },

    #[error("unknown build state: {0}")]
    UnknownBuildState(String),
}

/// In-memory state for a single derivation node in the global DAG.
#[derive(Debug, Clone)]
pub struct DerivationState {
    /// Unique hash identifying this derivation (store path for input-addressed, modular hash for CA).
    pub drv_hash: String,
    /// Store path of the .drv file.
    pub drv_path: String,
    /// Package name (for duration estimation).
    pub pname: Option<String>,
    /// Target system (e.g. "x86_64-linux").
    pub system: String,
    /// Required system features the building worker must support.
    pub required_features: Vec<String>,
    /// Output names (e.g. ["out", "dev"]).
    pub output_names: Vec<String>,
    /// Whether this is a fixed-output derivation (fetchurl, etc.).
    pub is_fixed_output: bool,
    /// Current state machine status. Private: mutate only via `transition()`,
    /// `reset_to_ready()`, or `reset_from_poison()` to preserve invariants.
    status: DerivationStatus,
    /// Set of build IDs interested in this derivation.
    pub interested_builds: HashSet<Uuid>,
    /// Worker currently assigned/running this derivation.
    pub assigned_worker: Option<String>,
    /// Number of retry attempts so far.
    pub retry_count: u32,
    /// Workers that have failed building this derivation (for poison tracking).
    pub failed_workers: HashSet<String>,
    /// When the derivation entered the poisoned state (for TTL expiry).
    pub poisoned_at: Option<Instant>,
    /// Realized output store paths (filled on completion).
    pub output_paths: Vec<String>,
    /// Database UUID (set after insertion).
    pub db_id: Option<Uuid>,
    /// When the derivation entered Ready state (for assignment latency metric).
    pub(crate) ready_at: Option<Instant>,
}

impl DerivationState {
    /// Create a new derivation state from a proto DerivationNode.
    pub fn from_node(node: &rio_proto::types::DerivationNode) -> Self {
        Self {
            drv_hash: node.drv_hash.clone(),
            drv_path: node.drv_path.clone(),
            pname: if node.pname.is_empty() {
                None
            } else {
                Some(node.pname.clone())
            },
            system: node.system.clone(),
            required_features: node.required_features.clone(),
            output_names: node.output_names.clone(),
            is_fixed_output: node.is_fixed_output,
            status: DerivationStatus::Created,
            interested_builds: HashSet::new(),
            assigned_worker: None,
            retry_count: 0,
            failed_workers: HashSet::new(),
            poisoned_at: None,
            output_paths: Vec::new(),
            db_id: None,
            ready_at: None,
        }
    }

    /// Current status (read-only). Use `transition()` etc. to mutate.
    pub fn status(&self) -> DerivationStatus {
        self.status
    }

    /// Attempt to transition to a new status. Returns the old status on success.
    pub fn transition(
        &mut self,
        to: DerivationStatus,
    ) -> Result<DerivationStatus, TransitionError> {
        let from = self.status;
        from.validate_transition(to)?;

        // Idempotent no-ops: don't change anything
        if from == to {
            return Ok(from);
        }

        self.status = to;

        // Track ready_at for assignment latency metric
        if to == DerivationStatus::Ready {
            self.ready_at = Some(Instant::now());
        }

        Ok(from)
    }

    /// Worker-lost recovery. Transitions Assigned -> Ready, or Running -> Failed -> Ready.
    /// Clears `assigned_worker`. Returns error if not in Assigned or Running state.
    ///
    /// Running -> Ready is not a valid direct transition, so Running goes through
    /// Failed first (this is a failed build attempt that the caller should count
    /// as a retry).
    pub fn reset_to_ready(&mut self) -> Result<(), TransitionError> {
        match self.status {
            DerivationStatus::Assigned => {
                self.transition(DerivationStatus::Ready)?;
            }
            DerivationStatus::Running => {
                self.transition(DerivationStatus::Failed)?;
                self.transition(DerivationStatus::Ready)?;
            }
            _ => {
                return Err(TransitionError::Invalid {
                    from: self.status,
                    to: DerivationStatus::Ready,
                    reason: "reset_to_ready only valid from Assigned or Running",
                });
            }
        }
        self.assigned_worker = None;
        Ok(())
    }

    /// Poison TTL expiry. Transitions Poisoned -> Created and clears poison state.
    pub fn reset_from_poison(&mut self) -> Result<(), TransitionError> {
        self.transition(DerivationStatus::Created)?;
        self.poisoned_at = None;
        self.retry_count = 0;
        self.failed_workers.clear();
        Ok(())
    }

    /// Test-only: directly set status bypassing state machine validation.
    /// For setting up test preconditions where the full transition chain
    /// would be verbose noise.
    #[cfg(test)]
    pub(crate) fn set_status_for_test(&mut self, status: DerivationStatus) {
        self.status = status;
    }
}

/// In-memory state for a build request.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// Unique build ID.
    pub build_id: Uuid,
    /// Tenant ID (unused in Phase 2a).
    pub tenant_id: Option<String>,
    /// Priority class ("ci", "interactive", "scheduled").
    pub priority_class: String,
    /// Current build state.
    pub state: BuildState,
    /// Whether to continue building independent derivations on failure.
    pub keep_going: bool,
    /// Build options propagated from the client.
    pub options: BuildOptions,
    /// All derivation hashes involved in this build.
    pub derivation_hashes: HashSet<String>,
    /// Number of derivations that are completed (including cache hits).
    pub completed_count: u32,
    /// Number of derivations that are cached.
    pub cached_count: u32,
    /// Number of derivations that have failed.
    pub failed_count: u32,
    /// Error summary (set on failure).
    pub error_summary: Option<String>,
    /// The derivation that caused the failure (if any).
    pub failed_derivation: Option<String>,
}

/// Build configuration options.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    pub max_silent_time: u64,
    pub build_timeout: u64,
    pub build_cores: u64,
}

/// In-memory state for a connected worker.
#[derive(Debug)]
pub struct WorkerState {
    /// Unique worker ID (from pod UID).
    pub worker_id: String,
    /// Target system (e.g. "x86_64-linux"). Set on first heartbeat.
    pub system: Option<String>,
    /// Features this worker supports.
    pub supported_features: Vec<String>,
    /// Maximum concurrent builds.
    pub max_builds: u32,
    /// Derivation hashes currently being built by this worker.
    pub running_builds: HashSet<String>,
    /// Channel to send scheduler messages (assignments, cancels) to the worker.
    /// Set when the BuildExecution stream opens.
    pub stream_tx: Option<tokio::sync::mpsc::Sender<rio_proto::types::SchedulerMessage>>,
    /// Timestamp of last heartbeat (for timeout detection).
    pub last_heartbeat: Instant,
    /// Number of consecutive missed heartbeats.
    pub missed_heartbeats: u32,
}

impl WorkerState {
    /// Whether we have received both a stream connection and a heartbeat.
    /// Derived from `stream_tx.is_some() && system.is_some()` — no manual
    /// bookkeeping, so the two channels can't get out of sync.
    pub fn is_registered(&self) -> bool {
        self.stream_tx.is_some() && self.system.is_some()
    }

    /// Whether this worker has available build capacity.
    pub fn has_capacity(&self) -> bool {
        self.is_registered() && (self.running_builds.len() as u32) < self.max_builds
    }

    /// Whether this worker can build the given derivation based on system and features.
    pub fn can_build(&self, system: &str, required_features: &[String]) -> bool {
        if !self.is_registered() {
            return false;
        }
        // System must match
        if self.system.as_deref() != Some(system) {
            return false;
        }
        // All required features must be present on the worker
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

/// Poison threshold: number of distinct workers that must fail before marking
/// a derivation as poisoned.
pub const POISON_THRESHOLD: usize = 3;

/// Poison TTL: duration after which a poisoned derivation is reset to created.
pub const POISON_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Heartbeat interval and timeout constants.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 10;
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 30;
pub const MAX_MISSED_HEARTBEATS: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derivation_valid_transitions() {
        use DerivationStatus::*;

        let valid_transitions = [
            (Created, Completed), // cache hit
            (Created, Queued),    // build accepted
            (Queued, Ready),      // all deps complete
            (Ready, Assigned),    // worker selected
            (Assigned, Running),  // worker ack
            (Assigned, Ready),    // worker lost
            (Running, Completed), // build succeeded
            (Running, Failed),    // retriable failure
            (Running, Poisoned),  // failed on 3+ workers
            (Failed, Ready),      // retry scheduled
            (Poisoned, Created),  // 24h TTL expiry
        ];

        for (from, to) in valid_transitions {
            assert!(
                from.validate_transition(to).is_ok(),
                "expected {from} -> {to} to be valid"
            );
        }
    }

    #[test]
    fn test_derivation_idempotent_transitions() {
        use DerivationStatus::*;

        // completed -> completed is no-op
        assert!(Completed.validate_transition(Completed).is_ok());
        // poisoned -> poisoned is no-op
        assert!(Poisoned.validate_transition(Poisoned).is_ok());
    }

    #[test]
    fn test_derivation_invalid_transitions() {
        use DerivationStatus::*;

        // Terminal -> non-terminal (except poisoned -> created)
        assert!(Completed.validate_transition(Created).is_err());
        assert!(Completed.validate_transition(Running).is_err());
        assert!(Completed.validate_transition(Ready).is_err());
        assert!(Completed.validate_transition(Queued).is_err());

        // Skip states
        assert!(Created.validate_transition(Running).is_err());
        assert!(Created.validate_transition(Ready).is_err());
        assert!(Created.validate_transition(Assigned).is_err());
        assert!(Queued.validate_transition(Assigned).is_err());
        assert!(Queued.validate_transition(Running).is_err());
        assert!(Ready.validate_transition(Running).is_err());
        assert!(Ready.validate_transition(Completed).is_err());

        // Running -> Ready is NOT valid (must go through Failed)
        assert!(Running.validate_transition(Ready).is_err());
        assert!(Running.validate_transition(Queued).is_err());
        assert!(Running.validate_transition(Assigned).is_err());

        // Failed can only go to Ready
        assert!(Failed.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Completed).is_err());
        assert!(Failed.validate_transition(Queued).is_err());

        // Poisoned can only go to Created (TTL expiry) or stay Poisoned
        assert!(Poisoned.validate_transition(Ready).is_err());
        assert!(Poisoned.validate_transition(Running).is_err());
        assert!(Poisoned.validate_transition(Failed).is_err());

        // Non-terminal self-transitions are invalid
        assert!(Created.validate_transition(Created).is_err());
        assert!(Queued.validate_transition(Queued).is_err());
        assert!(Ready.validate_transition(Ready).is_err());
        assert!(Assigned.validate_transition(Assigned).is_err());
        assert!(Running.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Failed).is_err());
    }

    #[test]
    fn test_reset_to_ready() {
        let node = rio_proto::types::DerivationNode {
            drv_hash: "h".into(),
            drv_path: "/nix/store/h.drv".into(),
            pname: String::new(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        };

        // Assigned -> Ready: direct valid transition
        let mut state = DerivationState::from_node(&node);
        state.set_status_for_test(DerivationStatus::Assigned);
        state.assigned_worker = Some("w1".into());
        assert!(state.reset_to_ready().is_ok());
        assert_eq!(state.status(), DerivationStatus::Ready);
        assert!(state.assigned_worker.is_none());

        // Running -> Failed -> Ready: goes through Failed
        let mut state = DerivationState::from_node(&node);
        state.set_status_for_test(DerivationStatus::Running);
        state.assigned_worker = Some("w1".into());
        assert!(state.reset_to_ready().is_ok());
        assert_eq!(state.status(), DerivationStatus::Ready);
        assert!(state.assigned_worker.is_none());

        // Invalid source states rejected
        let mut state = DerivationState::from_node(&node);
        state.set_status_for_test(DerivationStatus::Queued);
        assert!(state.reset_to_ready().is_err());

        let mut state = DerivationState::from_node(&node);
        state.set_status_for_test(DerivationStatus::Completed);
        assert!(state.reset_to_ready().is_err());
    }

    #[test]
    fn test_reset_from_poison() {
        let node = rio_proto::types::DerivationNode {
            drv_hash: "h".into(),
            drv_path: "/nix/store/h.drv".into(),
            pname: String::new(),
            system: "x86_64-linux".into(),
            required_features: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        };

        let mut state = DerivationState::from_node(&node);
        state.set_status_for_test(DerivationStatus::Poisoned);
        state.poisoned_at = Some(Instant::now());
        state.retry_count = 5;
        state.failed_workers.insert("w1".into());
        state.failed_workers.insert("w2".into());

        assert!(state.reset_from_poison().is_ok());
        assert_eq!(state.status(), DerivationStatus::Created);
        assert!(state.poisoned_at.is_none());
        assert_eq!(state.retry_count, 0);
        assert!(state.failed_workers.is_empty());

        // Non-poisoned state rejected
        let mut state = DerivationState::from_node(&node);
        state.set_status_for_test(DerivationStatus::Running);
        assert!(state.reset_from_poison().is_err());
    }

    #[test]
    fn test_build_valid_transitions() {
        use BuildState::*;

        assert!(Pending.validate_transition(Active).is_ok());
        assert!(Active.validate_transition(Succeeded).is_ok());
        assert!(Active.validate_transition(Failed).is_ok());
        assert!(Active.validate_transition(Cancelled).is_ok());
        assert!(Pending.validate_transition(Cancelled).is_ok());
    }

    #[test]
    fn test_build_terminal_rejected() {
        use BuildState::*;

        // Terminal -> any non-terminal
        assert!(Succeeded.validate_transition(Active).is_err());
        assert!(Succeeded.validate_transition(Pending).is_err());
        assert!(Failed.validate_transition(Active).is_err());
        assert!(Failed.validate_transition(Pending).is_err());
        assert!(Cancelled.validate_transition(Active).is_err());
        assert!(Cancelled.validate_transition(Pending).is_err());

        // Terminal -> other terminal
        assert!(Succeeded.validate_transition(Failed).is_err());
        assert!(Succeeded.validate_transition(Cancelled).is_err());
        assert!(Failed.validate_transition(Succeeded).is_err());
        assert!(Failed.validate_transition(Cancelled).is_err());
        assert!(Cancelled.validate_transition(Succeeded).is_err());
        assert!(Cancelled.validate_transition(Failed).is_err());

        // Self-transitions
        assert!(Pending.validate_transition(Pending).is_err());
        assert!(Active.validate_transition(Active).is_err());
        assert!(Succeeded.validate_transition(Succeeded).is_err());
        assert!(Failed.validate_transition(Failed).is_err());
        assert!(Cancelled.validate_transition(Cancelled).is_err());

        // Skip states
        assert!(Pending.validate_transition(Succeeded).is_err());
        assert!(Pending.validate_transition(Failed).is_err());
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
