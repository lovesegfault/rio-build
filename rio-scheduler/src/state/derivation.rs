//! Derivation state machine: [`DerivationStatus`] transitions and
//! [`DerivationState`] (per-derivation DAG node).
//!
//! State machine: created → queued → ready → assigned → running →
//! completed|failed|poisoned. Poisoned has a 24h TTL (→ created).

use std::collections::HashSet;
use std::time::Instant;

use uuid::Uuid;

use super::{DrvHash, TransitionError, WorkerId};

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
    /// A dependency of this derivation failed/poisoned. This derivation can
    /// never complete in the current build. Terminal (like Poisoned).
    /// Maps to Nix BuildStatus::DependencyFailed=10.
    DependencyFailed,
}

impl DerivationStatus {
    /// Whether this is a terminal state (no further progress without external reset).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Poisoned | Self::DependencyFailed
        )
    }

    /// Validate a state transition.
    ///
    /// Returns `Ok(())` if the transition is valid, `Err` with a description otherwise.
    pub fn validate_transition(self, to: Self) -> Result<(), TransitionError> {
        // Idempotent no-ops
        if self == to {
            match self {
                Self::Completed | Self::Poisoned => return Ok(()),
                Self::DependencyFailed => return Ok(()),
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
            (Self::Created, Self::Completed) => true,       // cache hit
            (Self::Created, Self::Queued) => true,          // build accepted
            (Self::Queued, Self::Ready) => true,            // all deps complete
            (Self::Queued, Self::DependencyFailed) => true, // dep poisoned, cascade
            (Self::Ready, Self::DependencyFailed) => true,  // dep poisoned, cascade
            (Self::Created, Self::DependencyFailed) => true, // dep poisoned before queue
            (Self::Ready, Self::Assigned) => true,          // worker selected
            (Self::Assigned, Self::Running) => true,        // worker ack
            (Self::Assigned, Self::Ready) => true,          // worker lost
            (Self::Running, Self::Completed) => true,       // build succeeded
            (Self::Running, Self::Failed) => true,          // retriable failure
            (Self::Running, Self::Poisoned) => true,        // failed on 3+ workers
            (Self::Failed, Self::Ready) => true,            // retry scheduled
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
            Self::DependencyFailed => "dependency_failed",
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
            "dependency_failed" => Ok(Self::DependencyFailed),
            other => Err(TransitionError::UnknownStatus(other.to_string())),
        }
    }
}

/// In-memory state for a single derivation node in the global DAG.
#[derive(Debug, Clone)]
pub struct DerivationState {
    /// Unique hash identifying this derivation (store path for input-addressed, modular hash for CA).
    pub drv_hash: DrvHash,
    /// Store path of the .drv file. Private because the DAG maintains a
    /// `path_to_hash` reverse index keyed on this field — mutating it
    /// directly would silently corrupt that index. Read via `drv_path()`.
    drv_path: rio_nix::store_path::StorePath,
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
    pub assigned_worker: Option<WorkerId>,
    /// Size-class this derivation was dispatched to. Recorded at
    /// assign time so completion can check misclassification (actual
    /// duration > 2× the class cutoff). `None` = size-classes not
    /// configured, or never assigned.
    pub assigned_size_class: Option<String>,
    /// ATerm-serialized .drv content, inlined by the gateway for
    /// nodes that will actually dispatch (outputs missing from store).
    /// Empty = worker fetches from store via GetPath (the pre-D8
    /// path, still works). Forwarded verbatim into WorkAssignment.
    /// ≤256 KB bound enforced at gRPC ingress.
    pub drv_content: Vec<u8>,
    /// Number of retry attempts so far.
    pub retry_count: u32,
    /// Workers that have failed building this derivation (for poison tracking).
    pub failed_workers: HashSet<WorkerId>,
    /// When the derivation entered the poisoned state (for TTL expiry).
    pub poisoned_at: Option<Instant>,
    /// Realized output store paths (filled on completion).
    pub output_paths: Vec<String>,
    /// Expected output paths (from the proto node at merge time).
    /// Used for: cache-check (merge.rs), transfer-cost scoring (D6),
    /// and as the closure approximation for locality (children's
    /// expected_output_paths = parent's inputs).
    pub expected_output_paths: Vec<String>,
    /// Estimated build duration (from Estimator). Set at merge time;
    /// never updated after. The critical-path priority uses this;
    /// stale is fine (a build taking longer than estimated doesn't
    /// change the OPTIMAL schedule mid-execution — what's queued is
    /// queued).
    pub est_duration: f64,
    /// Critical-path priority: `est_duration + max(children's priority)`.
    /// Bottom-up: leaves have `priority = est_duration`; roots have
    /// the sum along the longest path. Higher = more urgent (dispatch
    /// first). Recomputed incrementally on completion (D4) via
    /// ancestor-walk. D5 uses this for BinaryHeap ordering.
    pub priority: f64,
    /// Database UUID (set after insertion).
    pub db_id: Option<Uuid>,
    /// When the derivation entered Ready state (for assignment latency metric).
    pub(crate) ready_at: Option<Instant>,
}

impl DerivationState {
    /// Create a new derivation state from a proto DerivationNode.
    ///
    /// Validates `node.drv_path` parses as a well-formed `StorePath`. The
    /// gRPC layer also validates upfront (returns INVALID_ARGUMENT), so this
    /// is belt-and-suspenders for when the actor is driven by something
    /// other than gRPC (tests, future admin APIs).
    pub fn try_from_node(
        node: &rio_proto::types::DerivationNode,
    ) -> Result<Self, rio_nix::store_path::StorePathError> {
        let drv_path = rio_nix::store_path::StorePath::parse(&node.drv_path)?;
        Ok(Self {
            drv_hash: node.drv_hash.as_str().into(),
            drv_path,
            pname: (!node.pname.is_empty()).then(|| node.pname.clone()),
            system: node.system.clone(),
            required_features: node.required_features.clone(),
            output_names: node.output_names.clone(),
            is_fixed_output: node.is_fixed_output,
            status: DerivationStatus::Created,
            interested_builds: HashSet::new(),
            assigned_worker: None,
            assigned_size_class: None,
            drv_content: node.drv_content.clone(),
            retry_count: 0,
            failed_workers: HashSet::new(),
            poisoned_at: None,
            output_paths: Vec::new(),
            expected_output_paths: node.expected_output_paths.clone(),
            // Placeholder — merge.rs sets this from the Estimator right
            // after try_from_node (try_from_node doesn't have estimator
            // access). 0.0 is a visible "not yet set" marker.
            est_duration: 0.0,
            priority: 0.0,
            db_id: None,
            ready_at: None,
        })
    }

    /// Current status (read-only). Use `transition()` etc. to mutate.
    pub fn status(&self) -> DerivationStatus {
        self.status
    }

    /// Store path of the .drv file (read-only; DAG owns the reverse index).
    ///
    /// Callers using `&str` auto-deref via `StorePath::Deref<Target=str>`.
    pub fn drv_path(&self) -> &rio_nix::store_path::StorePath {
        &self.drv_path
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

/// Poison threshold: number of distinct workers that must fail before marking
/// a derivation as poisoned.
pub const POISON_THRESHOLD: usize = 3;

/// Poison TTL: duration after which a poisoned derivation is reset to created.
/// 24h in production. Short in tests so poison-expiry can be observed without
/// clock manipulation (`std::time::Instant` can't be faked).
#[cfg(not(test))]
pub const POISON_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
#[cfg(test)]
pub const POISON_TTL: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_node() -> rio_proto::types::DerivationNode {
        rio_test_support::fixtures::make_derivation_node("h", "x86_64-linux")
    }

    #[test]
    fn test_derivation_valid_transitions() {
        use DerivationStatus::*;

        let valid_transitions = [
            (Created, Completed),        // cache hit
            (Created, Queued),           // build accepted
            (Queued, Ready),             // all deps complete
            (Ready, Assigned),           // worker selected
            (Assigned, Running),         // worker ack
            (Assigned, Ready),           // worker lost
            (Running, Completed),        // build succeeded
            (Running, Failed),           // retriable failure
            (Running, Poisoned),         // failed on 3+ workers
            (Failed, Ready),             // retry scheduled
            (Poisoned, Created),         // 24h TTL expiry
            (Queued, DependencyFailed),  // dep poisoned cascade
            (Ready, DependencyFailed),   // dep poisoned cascade
            (Created, DependencyFailed), // dep poisoned before queue
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
        // dependency_failed -> dependency_failed is no-op
        assert!(
            DependencyFailed
                .validate_transition(DependencyFailed)
                .is_ok()
        );
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

        // DependencyFailed is terminal: can't go anywhere except self
        assert!(DependencyFailed.validate_transition(Ready).is_err());
        assert!(DependencyFailed.validate_transition(Queued).is_err());
        assert!(DependencyFailed.validate_transition(Created).is_err());
        // Assigned/Running cannot cascade to DependencyFailed (already started)
        assert!(Assigned.validate_transition(DependencyFailed).is_err());
        assert!(Running.validate_transition(DependencyFailed).is_err());

        // Non-terminal self-transitions are invalid
        assert!(Created.validate_transition(Created).is_err());
        assert!(Queued.validate_transition(Queued).is_err());
        assert!(Ready.validate_transition(Ready).is_err());
        assert!(Assigned.validate_transition(Assigned).is_err());
        assert!(Running.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Failed).is_err());
    }

    #[test]
    fn test_reset_to_ready() -> anyhow::Result<()> {
        let node = dummy_node();

        // Assigned -> Ready: direct valid transition
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Assigned);
        state.assigned_worker = Some("w1".into());
        assert!(state.reset_to_ready().is_ok());
        assert_eq!(state.status(), DerivationStatus::Ready);
        assert!(state.assigned_worker.is_none());

        // Running -> Failed -> Ready: goes through Failed
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Running);
        state.assigned_worker = Some("w1".into());
        assert!(state.reset_to_ready().is_ok());
        assert_eq!(state.status(), DerivationStatus::Ready);
        assert!(state.assigned_worker.is_none());

        // Invalid source states rejected
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Queued);
        assert!(state.reset_to_ready().is_err());

        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Completed);
        assert!(state.reset_to_ready().is_err());
        Ok(())
    }

    #[test]
    fn test_reset_from_poison() -> anyhow::Result<()> {
        let node = dummy_node();

        let mut state = DerivationState::try_from_node(&node)?;
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
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Running);
        assert!(state.reset_from_poison().is_err());
        Ok(())
    }
}
