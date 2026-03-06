//! Controller error types.

use thiserror::Error;

/// Top-level reconcile error. kube's `Controller::run` takes an
/// `error_policy` fn that receives this and decides requeue timing.
///
/// `Box` on the finalizer variant: `finalizer::Error<Self>` is
/// recursive (it wraps our Error for ApplyFailed/CleanupFailed).
/// Unboxed it's an infinite-size type. Box breaks the cycle with
/// a pointer indirection.
#[derive(Debug, Error)]
pub enum Error {
    /// K8s API error (connection, permission, not-found).
    #[error("kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    /// finalizer() wrapper failed. Inner is either our own Error
    /// (ApplyFailed/CleanupFailed wrapping the reconcile body's
    /// error) or a finalizer-specific failure (couldn't patch
    /// metadata.finalizers).
    #[error("finalizer error: {0}")]
    Finalizer(#[from] Box<kube::runtime::finalizer::Error<Error>>),

    /// Failed to build a K8s object (StatefulSet, Service) from
    /// the CRD spec. Usually a bad Quantity string (fuseCacheSize
    /// that doesn't parse) or some spec field we can't translate.
    /// These are OPERATOR errors — fix the CRD and reconcile
    /// retries. Not transient.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),

    /// Scheduler gRPC unreachable. Autoscaler can't read
    /// ClusterStatus; Build reconciler can't SubmitBuild. Requeue
    /// with backoff — the scheduler may come back.
    #[error("scheduler unavailable: {0}")]
    SchedulerUnavailable(#[from] tonic::Status),
}

/// Result alias used throughout reconcilers.
pub type Result<T, E = Error> = std::result::Result<T, E>;
