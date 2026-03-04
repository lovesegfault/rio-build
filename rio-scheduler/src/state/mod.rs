//! Scheduler state types: derivation/build/worker state machines.
//!
//! Submodules (glob-re-exported for zero external churn):
//! - `derivation` — [`DerivationStatus`] + [`DerivationState`] + POISON consts
//! - `build` — [`BuildState`] + [`BuildInfo`] + [`BuildOptions`]
//! - `worker` — [`WorkerState`] + [`RetryPolicy`]
//!
//! This file holds cross-cutting types:
//! - [`PriorityClass`] — used by both queue.rs and build.rs
//! - [`TransitionError`] — variants reference BOTH `DerivationStatus` and
//!   `BuildState`, so it can't live in either submodule without a cycle.
//!
//! Idempotency rules:
//! - completed -> completed: no-op
//! - poisoned -> poisoned: no-op
//! - terminal -> non-terminal: rejected (except poisoned -> created on TTL)

mod build;
mod derivation;
mod newtypes;
mod worker;

pub use build::*;
pub use derivation::*;
pub use newtypes::{DrvHash, WorkerId};
pub use worker::*;

/// Heartbeat constants. Re-exported from rio-common so the worker's send
/// interval and the scheduler's timeout check derive from the same
/// source of truth. Before this re-export, the worker's `10s` and the
/// scheduler's `30s` were independently hardcoded; now `timeout =
/// interval × max_missed` is a compile-time derivation, not a
/// copy-paste invariant.
pub use rio_common::limits::{
    HEARTBEAT_INTERVAL_SECS, HEARTBEAT_TIMEOUT_SECS, MAX_MISSED_HEARTBEATS,
};

/// Priority class for scheduling.
///
/// Interactive builds get [`INTERACTIVE_BOOST`](crate::queue::INTERACTIVE_BOOST)
/// (+1e9) added to their priority in the [`ReadyQueue`](crate::queue::ReadyQueue)
/// BinaryHeap, so they dispatch before any realistic critical-path value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PriorityClass {
    /// CI builds: normal priority, scheduled order.
    Ci,
    /// Interactive builds (e.g., IFD during evaluation): +1e9 priority boost.
    Interactive,
    /// Scheduled/batch builds: default, lowest priority.
    #[default]
    Scheduled,
}

impl PriorityClass {
    /// Database and wire string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Interactive => "interactive",
            Self::Scheduled => "scheduled",
        }
    }

    /// Whether this class gets the INTERACTIVE_BOOST priority bonus.
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

impl std::str::FromStr for PriorityClass {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ci" => Ok(Self::Ci),
            "interactive" => Ok(Self::Interactive),
            "scheduled" => Ok(Self::Scheduled),
            _ => Err("invalid priority class (must be 'ci', 'interactive', or 'scheduled')"),
        }
    }
}

impl std::fmt::Display for PriorityClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors from state machine transitions. Lives in mod.rs (not derivation.rs
/// or build.rs) because variants reference both `DerivationStatus` and
/// `BuildState` — putting it in either submodule would create a dependency
/// on the other.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_class_from_str_roundtrip() {
        for pc in [
            PriorityClass::Ci,
            PriorityClass::Interactive,
            PriorityClass::Scheduled,
        ] {
            let s = pc.as_str();
            let parsed: PriorityClass = s.parse().expect("as_str output must parse");
            assert_eq!(parsed, pc, "roundtrip failed for {pc:?}");
        }
    }

    #[test]
    fn test_priority_class_from_str_rejects_unknown() {
        assert!("urgent".parse::<PriorityClass>().is_err());
        assert!("".parse::<PriorityClass>().is_err());
        assert!("INTERACTIVE".parse::<PriorityClass>().is_err()); // case-sensitive
    }

    #[test]
    fn test_priority_class_default_is_scheduled() {
        assert_eq!(PriorityClass::default(), PriorityClass::Scheduled);
    }

    #[test]
    fn test_priority_class_is_interactive() {
        assert!(PriorityClass::Interactive.is_interactive());
        assert!(!PriorityClass::Ci.is_interactive());
        assert!(!PriorityClass::Scheduled.is_interactive());
    }
}
