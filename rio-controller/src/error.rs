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

    /// Failed to build a K8s object (Job, PodSpec) from
    /// the CRD spec. Usually a bad Quantity string (fuseCacheSize
    /// that doesn't parse) or some spec field we can't translate.
    /// These are OPERATOR errors — fix the CRD and reconcile
    /// retries. Not transient.
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
}

/// Result alias used throughout reconcilers.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Unwrap `Finalizer(ApplyFailed|CleanupFailed(e))` to `e`. Those
/// variants wrap the reconcile body's *own* error, not a finalizer-
/// patch failure — `finalized()` (reconcilers/mod.rs) wraps every
/// `apply`/`cleanup` result in them. `AddFinalizer`/`RemoveFinalizer`
/// /`UnnamedObject`/`InvalidFinalizer` stay as `Finalizer` (genuinely
/// a finalizer-layer failure).
///
/// Called at the top of [`error_kind`] and `standard_error_policy` so
/// a Pool-reconciler `Error::Kube` shows as `error_kind="kube"` (not
/// `"finalizer"`) and a wrapped `InvalidSpec` takes the fixed-requeue
/// arm. Recursive: defends against a future `finalized(finalized(..))`.
pub fn leaf(err: &Error) -> &Error {
    use kube::runtime::finalizer::Error as Fin;
    match err {
        Error::Finalizer(b) => match &**b {
            Fin::ApplyFailed(e) | Fin::CleanupFailed(e) => leaf(e),
            _ => err,
        },
        _ => err,
    }
}

/// Discriminator string for metric labels. Stable across
/// error-message changes; low cardinality (3 values).
///
/// `rio_controller_reconcile_errors_total{error_kind=...}` uses
/// this. Don't switch to Display — error messages contain
/// dynamic data (addresses, status codes) that would explode
/// metric cardinality.
pub fn error_kind(err: &Error) -> &'static str {
    match leaf(err) {
        Error::Kube(_) => "kube",
        Error::Finalizer(_) => "finalizer",
        Error::InvalidSpec(_) => "invalid_spec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::runtime::finalizer::Error as Fin;

    fn wrap(inner: Error) -> Error {
        Error::Finalizer(Box::new(Fin::ApplyFailed(inner)))
    }

    fn kube_503() -> kube::Error {
        kube::Error::Api(Box::new(
            kube::core::Status::failure("boom", "ServiceUnavailable").with_code(503),
        ))
    }

    /// Regression: `finalized()` wraps every Pool reconcile-body error
    /// as `Finalizer(ApplyFailed(inner))`; before `leaf()` the metric
    /// label collapsed to `"finalizer"` for all of them.
    #[test]
    fn leaf_unwraps_apply_failed() {
        assert_eq!(error_kind(&wrap(Error::Kube(kube_503()))), "kube");
    }

    #[test]
    fn leaf_unwraps_invalid_spec() {
        let e = wrap(Error::InvalidSpec("bad".into()));
        assert_eq!(error_kind(&e), "invalid_spec");
        // CleanupFailed too.
        let e = Error::Finalizer(Box::new(Fin::CleanupFailed(Error::InvalidSpec("x".into()))));
        assert_eq!(error_kind(&e), "invalid_spec");
    }

    /// `AddFinalizer`/`RemoveFinalizer` are genuine finalizer-layer
    /// failures (couldn't patch metadata.finalizers) — stay labeled
    /// `"finalizer"`.
    #[test]
    fn leaf_preserves_add_finalizer() {
        let e = Error::Finalizer(Box::new(Fin::AddFinalizer(kube_503())));
        assert_eq!(error_kind(&e), "finalizer");
        let e = Error::Finalizer(Box::new(Fin::RemoveFinalizer(kube_503())));
        assert_eq!(error_kind(&e), "finalizer");
    }

    #[test]
    fn leaf_passthrough_unwrapped() {
        assert_eq!(error_kind(&Error::Kube(kube_503())), "kube");
        assert_eq!(error_kind(&Error::InvalidSpec("x".into())), "invalid_spec");
    }
}
