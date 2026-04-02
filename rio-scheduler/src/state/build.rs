//! Build request state machine: [`BuildState`] transitions and
//! [`BuildInfo`] (per-SubmitBuild tracking).
//!
//! State machine: pending → active → succeeded|failed|cancelled.
//! Pending can also go straight to cancelled (client abort before start).

use std::collections::HashSet;
use std::time::Instant;

use uuid::Uuid;

use super::{DrvHash, PriorityClass, TransitionError};

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

impl From<BuildState> for rio_proto::types::BuildState {
    fn from(s: BuildState) -> Self {
        match s {
            BuildState::Pending => Self::Pending,
            BuildState::Active => Self::Active,
            BuildState::Succeeded => Self::Succeeded,
            BuildState::Failed => Self::Failed,
            BuildState::Cancelled => Self::Cancelled,
        }
    }
}

/// In-memory state for a build request.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// Unique build ID.
    pub build_id: Uuid,
    /// Tenant UUID resolved from name by the gRPC handler. `None` =
    /// single-tenant mode (gateway sent empty string).
    pub tenant_id: Option<Uuid>,
    /// Priority class. Interactive gets INTERACTIVE_BOOST (+1e9) in the ready queue.
    pub priority_class: PriorityClass,
    /// Current build state. Private: use `state()` to read, `transition()` to mutate.
    /// This enforces the BuildState transition validation at every write site.
    state: BuildState,
    /// Whether to continue building independent derivations on failure.
    pub keep_going: bool,
    /// Build options propagated from the client.
    pub options: BuildOptions,
    /// All derivation hashes involved in this build.
    pub derivation_hashes: HashSet<DrvHash>,
    /// Absolute total derivation count. Equals `derivation_hashes.len()`
    /// for fresh builds. After recovery, `derivation_hashes` only holds
    /// drvs that were non-terminal at recovery (completed ones aren't
    /// loaded into the DAG), so this is seeded from `builds.total_drvs`
    /// in PG instead. I-111: previously `derivation_hashes.len()` was
    /// used as the persisted total, which made `update_build_counts`
    /// stomp the DB total with the remaining-only count after restart.
    pub total_count: u32,
    /// Count of drvs already Completed at recovery and thus absent from
    /// the in-memory DAG. 0 for fresh builds. `dag.build_summary()`
    /// only sees in-DAG nodes, so the absolute completed count for
    /// persist/display is `recovered_completed + summary.completed`.
    pub recovered_completed: u32,
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
    /// When the build was submitted (for rio_scheduler_build_duration_seconds).
    pub submitted_at: Instant,
    /// When the orphan-watcher sweep first observed this build's
    /// `build_events` broadcast channel with zero receivers. `None`
    /// while at least one watcher (gateway SubmitBuild/WatchBuild
    /// stream) is attached. Reset to `None` if a watcher reattaches
    /// before the grace period elapses. After `ORPHAN_BUILD_GRACE`
    /// with no watcher, the build is auto-cancelled — defense-in-depth
    /// for the cases the gateway-side P0331 cancel can't reach: gateway
    /// crash, gateway→scheduler timeout during disconnect cleanup, or
    /// post-recovery (recovered builds start with zero watchers until
    /// the gateway WatchBuild-reconnects). I-112/I-036.
    pub orphaned_since: Option<Instant>,
}

impl BuildInfo {
    /// Construct a new BuildInfo in the Pending state with zeroed counts.
    pub fn new_pending(
        build_id: Uuid,
        tenant_id: Option<Uuid>,
        priority_class: PriorityClass,
        keep_going: bool,
        options: BuildOptions,
        derivation_hashes: HashSet<DrvHash>,
    ) -> Self {
        let total_count = derivation_hashes.len() as u32;
        Self {
            build_id,
            tenant_id,
            priority_class,
            state: BuildState::Pending,
            keep_going,
            options,
            derivation_hashes,
            total_count,
            recovered_completed: 0,
            completed_count: 0,
            cached_count: 0,
            failed_count: 0,
            error_summary: None,
            failed_derivation: None,
            submitted_at: Instant::now(),
            orphaned_since: None,
        }
    }

    /// Read the current state.
    pub fn state(&self) -> BuildState {
        self.state
    }

    /// Attempt to transition to a new state, validating against the BuildState
    /// machine. Returns the old state on success, `TransitionError` on invalid
    /// transition.
    pub fn transition(&mut self, to: BuildState) -> Result<BuildState, TransitionError> {
        let from = self.state;
        from.validate_transition(to)?;
        self.state = to;
        Ok(from)
    }
}

/// Build configuration options.
///
/// Serialize/Deserialize for JSONB persistence (Phase 3b state
/// recovery): `insert_build` writes this as `options_json`,
/// `load_nonterminal_builds` reads it back. Default for NULL rows
/// (written before migration 004) = all zeroes = unlimited.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BuildOptions {
    pub max_silent_time: u64,
    pub build_timeout: u64,
    pub build_cores: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_build_info_transition_validated() -> anyhow::Result<()> {
        let mut b = BuildInfo::new_pending(
            Uuid::new_v4(),
            None,
            PriorityClass::Scheduled,
            false,
            BuildOptions::default(),
            HashSet::new(),
        );
        assert_eq!(b.state(), BuildState::Pending);

        // Valid: Pending -> Active
        let old = b.transition(BuildState::Active)?;
        assert_eq!(old, BuildState::Pending);
        assert_eq!(b.state(), BuildState::Active);

        // Valid: Active -> Succeeded
        b.transition(BuildState::Succeeded)?;
        assert_eq!(b.state(), BuildState::Succeeded);

        // Invalid: terminal -> anything
        assert!(b.transition(BuildState::Active).is_err());
        assert_eq!(
            b.state(),
            BuildState::Succeeded,
            "state must be unchanged after rejected transition"
        );
        Ok(())
    }

    #[test]
    fn test_build_info_transition_rejects_skip() {
        let mut b = BuildInfo::new_pending(
            Uuid::new_v4(),
            None,
            PriorityClass::Scheduled,
            false,
            BuildOptions::default(),
            HashSet::new(),
        );
        // Invalid: Pending -> Succeeded (skips Active)
        assert!(b.transition(BuildState::Succeeded).is_err());
        assert_eq!(b.state(), BuildState::Pending);
    }
}
