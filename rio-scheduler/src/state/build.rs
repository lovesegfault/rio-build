//! Build request state machine: [`BuildState`] transitions and
//! [`BuildInfo`] (per-SubmitBuild tracking).
//!
//! State machine: pending → active → succeeded|failed|cancelled.
//! Pending can also go straight to cancelled (client abort before start).

use std::collections::HashSet;
use std::time::Instant;

use uuid::Uuid;

use super::{DrvHash, PriorityClass, TransitionError};

/// State of a build request. Re-export of the proto enum so the scheduler
/// uses the wire type directly; behavior (transitions, DB string repr)
/// lives on [`BuildStateExt`].
pub use rio_proto::types::BuildState;

/// Extension trait adding the scheduler's state-machine semantics and
/// PG string repr to the proto [`BuildState`]. The proto type carries
/// an `Unspecified` variant (proto3 default-0) that never appears in
/// scheduler-produced state — it falls through `validate_transition`'s
/// catch-all and round-trips as `"unspecified"` for diagnostics.
pub trait BuildStateExt: Sized + Copy {
    fn is_terminal(&self) -> bool;
    fn validate_transition(self, to: Self) -> Result<(), TransitionError>;
    /// Lowercase string repr written to / read from the `builds.status`
    /// column. Distinct from prost's `as_str_name()` (SCREAMING_SNAKE).
    fn as_str(&self) -> &'static str;
    /// Inverse of [`as_str`](Self::as_str). Replaces the `FromStr` impl
    /// (orphan rules: can't impl `FromStr` for a foreign type here).
    fn parse_db(s: &str) -> Result<Self, TransitionError>;
}

impl BuildStateExt for BuildState {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    fn validate_transition(self, to: Self) -> Result<(), TransitionError> {
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

    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unspecified => "unspecified",
        }
    }

    fn parse_db(s: &str) -> Result<Self, TransitionError> {
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

/// The first failure a build recorded — summary, culprit, and wire
/// classification captured TOGETHER so they can never name different
/// failures (the pre-capture trio of independent `Option` fields could).
#[derive(Debug, Clone, PartialEq)]
pub struct FirstFailure {
    /// Human-readable summary (BuildFailed.error_message / builds.error_summary).
    pub summary: String,
    /// The derivation that caused it. `None` for build-level failures
    /// (per-build timeout, recovery-synthesized) — those are about the
    /// BUILD, not any one derivation, and splicing an unrelated drv hash
    /// next to a build-level summary was exactly merged_bug_036.
    pub failed_drv: Option<String>,
    /// Worker classification for the nix wire. `None` ⇒ wire
    /// `0`/`Unspecified` ⇒ nix `MiscFailure`.
    pub status: Option<rio_proto::types::BuildResultStatus>,
}

/// Terminal outcome payload — which arm is populated IS the terminal
/// state. Constructing a terminal lifecycle without its payload does not
/// typecheck; that is the structural close of merged_bug_097/302/036.
#[derive(Debug, Clone)]
pub enum TerminalOutcome {
    Succeeded {
        /// Final output store paths of the build's root derivations,
        /// collected at the terminal transition (the DAG may be mutated
        /// by other builds afterwards; this snapshot is what watchers see).
        output_paths: Vec<String>,
    },
    Failed(FirstFailure),
    Cancelled {
        /// Why. Required — the only function able to mark a build
        /// Cancelled takes it as an argument (merged_bug_302).
        reason: String,
    },
}

impl TerminalOutcome {
    /// The BuildState this outcome settles into.
    pub fn build_state(&self) -> BuildState {
        match self {
            Self::Succeeded { .. } => BuildState::Succeeded,
            Self::Failed(_) => BuildState::Failed,
            Self::Cancelled { .. } => BuildState::Cancelled,
        }
    }
}

/// Aggregate counts frozen at the terminal transition. Live counts are
/// recomputed from the (shared, mutable) DAG; a finished build's numbers
/// must stop tracking a DAG other builds keep mutating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledCounts {
    pub total: u32,
    pub completed: u32,
    pub cached: u32,
    pub failed: u32,
}

/// Everything a terminal build serves, captured once at the transition:
/// settled counts + the outcome payload. Every consumer (live terminal
/// event, WatchBuild snapshot, QueryBuildStatus, persisted row) reads
/// THIS, never live state.
#[derive(Debug, Clone)]
pub struct SettledBuild {
    pub counts: SettledCounts,
    pub outcome: TerminalOutcome,
}

/// Build lifecycle: terminal state cannot exist without its settled
/// payload, by construction.
#[derive(Debug, Clone)]
enum Lifecycle {
    Pending,
    Active,
    Terminal(SettledBuild),
}

/// In-memory state for a build request.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    /// Unique build ID.
    pub build_id: Uuid,
    /// Tenant UUID resolved from name by the gRPC handler. `None` =
    /// single-tenant mode (gateway sent empty string).
    pub tenant_id: Option<Uuid>,
    /// Priority class (attribution; dispatch order is critical-path based).
    pub priority_class: PriorityClass,
    /// Lifecycle. Private: `state()` derives the BuildState;
    /// `transition()` (non-terminal) / `transition_terminal()` (with
    /// payload) are the only mutators, enforcing transition validation
    /// AND payload capture at every write site.
    lifecycle: Lifecycle,
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
    /// First failure (summary + culprit + classification, one struct).
    /// Private: written only through [`Self::note_first_failure`]
    /// (first wins, the keep_going sticky) and
    /// [`Self::override_failure_build_level`] (whole-struct overwrite
    /// for build-level failures whose `failed_drv` is structurally
    /// `None`). Partial trio writes do not compile any more.
    first_failure: Option<FirstFailure>,
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
            lifecycle: Lifecycle::Pending,
            keep_going,
            options,
            derivation_hashes,
            total_count,
            recovered_completed: 0,
            completed_count: 0,
            cached_count: 0,
            failed_count: 0,
            first_failure: None,
            submitted_at: Instant::now(),
            orphaned_since: None,
        }
    }

    /// Read the current state (derived from the lifecycle).
    pub fn state(&self) -> BuildState {
        match &self.lifecycle {
            Lifecycle::Pending => BuildState::Pending,
            Lifecycle::Active => BuildState::Active,
            Lifecycle::Terminal(s) => s.outcome.build_state(),
        }
    }

    /// The settled terminal payload, if the build is terminal. The ONLY
    /// terminal-data accessor: snapshot, query, terminal events, and the
    /// persisted row all read this, never live state.
    pub fn settled(&self) -> Option<&SettledBuild> {
        match &self.lifecycle {
            Lifecycle::Terminal(s) => Some(s),
            _ => None,
        }
    }

    /// Attempt a NON-terminal transition (Pending → Active), validating
    /// against the BuildState machine. Terminal targets are rejected:
    /// terminal state cannot exist without its settled payload — use
    /// [`Self::transition_terminal`].
    pub fn transition(&mut self, to: BuildState) -> Result<BuildState, TransitionError> {
        let from = self.state();
        from.validate_transition(to)?;
        if to.is_terminal() {
            // A terminal lifecycle REQUIRES a SettledBuild payload.
            return Err(TransitionError::InvalidBuild { from, to });
        }
        self.lifecycle = match to {
            BuildState::Active => Lifecycle::Active,
            // validate_transition admits only Pending→Active among
            // non-terminal targets; anything else was rejected above.
            _ => return Err(TransitionError::InvalidBuild { from, to }),
        };
        Ok(from)
    }

    /// Settle the build into a terminal state, capturing counts +
    /// outcome in one move. Validates the transition from the current
    /// state to the outcome's BuildState.
    pub fn transition_terminal(
        &mut self,
        settled: SettledBuild,
    ) -> Result<BuildState, TransitionError> {
        let from = self.state();
        from.validate_transition(settled.outcome.build_state())?;
        self.lifecycle = Lifecycle::Terminal(settled);
        Ok(from)
    }

    /// Record the build's FIRST failure (later calls are no-ops — the
    /// keep_going sticky). The trio (summary/culprit/classification)
    /// arrives as one struct so the fields cannot name different
    /// failures.
    pub fn note_first_failure(&mut self, failure: FirstFailure) {
        self.first_failure.get_or_insert(failure);
    }

    /// Overwrite the failure with a BUILD-level one (per-build timeout):
    /// whole-struct semantics, `failed_drv` structurally `None` — a
    /// build-level failure cannot splice a derivation hash next to its
    /// summary (merged_bug_036).
    pub fn override_failure_build_level(
        &mut self,
        summary: String,
        status: rio_proto::types::BuildResultStatus,
    ) {
        self.first_failure = Some(FirstFailure {
            summary,
            failed_drv: None,
            status: Some(status),
        });
    }

    /// The recorded first failure, if any.
    pub fn first_failure(&self) -> Option<&FirstFailure> {
        self.first_failure.as_ref()
    }

    /// Derived read: the first failure's summary (the legacy
    /// `error_summary` surface).
    pub fn error_summary(&self) -> Option<&str> {
        self.first_failure.as_ref().map(|f| f.summary.as_str())
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

        // Terminal targets are rejected by `transition` — terminal
        // state cannot exist without its settled payload.
        assert!(b.transition(BuildState::Succeeded).is_err());
        assert_eq!(b.state(), BuildState::Active);

        // Valid: Active -> Succeeded via transition_terminal, which
        // captures the payload in the same move.
        b.transition_terminal(SettledBuild {
            counts: SettledCounts {
                total: 0,
                completed: 0,
                cached: 0,
                failed: 0,
            },
            outcome: TerminalOutcome::Succeeded {
                output_paths: vec![],
            },
        })?;
        assert_eq!(b.state(), BuildState::Succeeded);
        assert!(b.settled().is_some(), "terminal build carries its payload");

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
        // Invalid: Pending -> Succeeded (skips Active), via either path.
        assert!(b.transition(BuildState::Succeeded).is_err());
        assert!(
            b.transition_terminal(SettledBuild {
                counts: SettledCounts {
                    total: 0,
                    completed: 0,
                    cached: 0,
                    failed: 0,
                },
                outcome: TerminalOutcome::Succeeded {
                    output_paths: vec![],
                },
            })
            .is_err()
        );
        assert_eq!(b.state(), BuildState::Pending);
    }

    #[test]
    fn test_first_failure_first_wins_and_build_level_override() {
        let mut b = BuildInfo::new_pending(
            Uuid::new_v4(),
            None,
            PriorityClass::Scheduled,
            true,
            BuildOptions::default(),
            HashSet::new(),
        );
        assert!(b.first_failure().is_none());
        b.note_first_failure(FirstFailure {
            summary: "derivation aaa failed".into(),
            failed_drv: Some("aaa".into()),
            status: Some(rio_proto::types::BuildResultStatus::PermanentFailure),
        });
        // Second note is a no-op — first failure wins.
        b.note_first_failure(FirstFailure {
            summary: "derivation bbb failed".into(),
            failed_drv: Some("bbb".into()),
            status: None,
        });
        assert_eq!(b.error_summary(), Some("derivation aaa failed"));
        assert_eq!(
            b.first_failure().unwrap().failed_drv.as_deref(),
            Some("aaa")
        );

        // Build-level override replaces the whole struct; failed_drv is
        // structurally None (no spliced derivation hash).
        b.override_failure_build_level(
            "build_timeout 10s exceeded".into(),
            rio_proto::types::BuildResultStatus::TimedOut,
        );
        let ff = b.first_failure().unwrap();
        assert_eq!(ff.summary, "build_timeout 10s exceeded");
        assert_eq!(ff.failed_drv, None);
        assert_eq!(
            ff.status,
            Some(rio_proto::types::BuildResultStatus::TimedOut)
        );
    }
}
