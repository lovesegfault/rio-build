//! Per-replica admission gate for `try_substitute_on_miss`.
//!
//! Substitution rate-limiting belongs in rio-store (which performs the
//! upstream HTTP fetch + NAR ingest), not in the scheduler (which only
//! observes the result). This module gates the COUNT of concurrent
//! substitute calls per replica; [`crate::grpc::StoreServiceImpl`]'s
//! `nar_bytes_budget` separately gates buffered BYTES.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::Status;

/// Domain error from [`AdmissionGate::acquire_bounded`]. Kept tonic-free
/// so [`crate::substitute::Substituter`] can hold the gate without
/// pulling gRPC types into its error enum; the gRPC layer maps via
/// `From<AdmissionError> for Status`.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum AdmissionError {
    /// Queued for [`SUBSTITUTE_ADMISSION_WAIT`] without acquiring.
    /// Transient — caller retries per `r[sched.substitute.detached+2]`.
    #[error("substitute admission saturated; retry (transient)")]
    Saturated,
    /// Semaphore closed. Unreachable in production (nothing calls
    /// `Semaphore::close`); mapped so a future shutdown-close surfaces
    /// as transient retry, not panic.
    #[error("admission gate closed")]
    Closed,
}

impl From<AdmissionError> for Status {
    fn from(e: AdmissionError) -> Self {
        match e {
            AdmissionError::Saturated => Status::resource_exhausted(e.to_string()),
            AdmissionError::Closed => Status::unavailable(e.to_string()),
        }
    }
}

/// How long [`AdmissionGate::acquire_bounded`] queues server-side
/// before returning `RESOURCE_EXHAUSTED`. 25 s sits inside the
/// scheduler's detached-fetch retry window (8 attempts × backoff ≈
/// 90 s; see `r[sched.substitute.detached+2]`), so a transient burst
/// is absorbed in ONE store-side wait rather than N client retries.
/// Spike 0.1 proved the prior immediate-RE design demoted 50/50
/// derivations to build-from-source under any hold ≥ 8 s.
///
/// MUST stay below `DEFAULT_GRPC_TIMEOUT` (30 s,
/// `rio_common::grpc`) so callers observe `ResourceExhausted`
/// (transient → retry) and not the client-side `DeadlineExceeded`
/// — at 30 s the two timers race and the client's fires first.
pub const SUBSTITUTE_ADMISSION_WAIT: Duration = Duration::from_secs(25);

/// Per-replica admission gate for `try_substitute_on_miss`. Wraps a
/// [`Semaphore`] + its capacity (tokio's `Semaphore` doesn't expose
/// total permits, only `available_permits()`). Shared via clone
/// between [`crate::grpc::StoreServiceImpl`] (acquires) and
/// [`crate::grpc::StoreAdminServiceImpl`] (reports utilization via
/// `GetLoad`). The inner `Arc` makes [`Clone`] cheap and the share
/// observable: a permit acquired through one clone reduces
/// `available_permits()` on every other.
#[derive(Clone)]
pub struct AdmissionGate {
    sem: Arc<Semaphore>,
    capacity: usize,
}

impl AdmissionGate {
    /// New gate with `capacity` permits. `capacity` is recorded
    /// alongside the semaphore so [`Self::utilization`] has a
    /// denominator (tokio doesn't expose it).
    pub fn new(capacity: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// Fraction of permits currently held: `(capacity − available) /
    /// capacity`, clamped to `[0, 1]`. The clamp guards against
    /// `available > capacity` (impossible today, but `Semaphore::
    /// add_permits` exists) and a `capacity = 0` test gate.
    pub fn utilization(&self) -> f32 {
        let in_use = self.capacity.saturating_sub(self.sem.available_permits());
        (in_use as f32 / self.capacity.max(1) as f32).clamp(0.0, 1.0)
    }

    /// Acquire one permit, queueing up to [`SUBSTITUTE_ADMISSION_WAIT`].
    ///
    /// `Ok(permit)` on success (permit released on drop — bind as
    /// `let _permit = ...` so it lives to end-of-scope). On success
    /// the `rio_store_substitute_admission_utilization` gauge is
    /// updated — emitting here (not at the call site) keeps the gauge
    /// coupled to wherever the acquire moves. After the wait expires,
    /// returns [`AdmissionError::Saturated`] (maps to
    /// `RESOURCE_EXHAUSTED` — transient per
    /// [`rio_common::grpc::is_transient`], so callers retry). The
    /// timeout-expiry path increments
    /// `rio_store_substitute_admission_rejected_total`; sustained
    /// non-zero on that counter means the replica is genuinely
    /// saturated (ComponentScaler should already be reacting via the
    /// `GetLoad` utilization signal).
    // r[impl store.substitute.admission]
    pub async fn acquire_bounded(&self) -> Result<OwnedSemaphorePermit, AdmissionError> {
        match tokio::time::timeout(SUBSTITUTE_ADMISSION_WAIT, self.sem.clone().acquire_owned())
            .await
        {
            Ok(Ok(p)) => {
                metrics::gauge!("rio_store_substitute_admission_utilization")
                    .set(f64::from(self.utilization()));
                Ok(p)
            }
            Ok(Err(_)) => Err(AdmissionError::Closed),
            Err(_) => {
                metrics::counter!("rio_store_substitute_admission_rejected_total").increment(1);
                Err(AdmissionError::Saturated)
            }
        }
    }

    /// Borrow the inner semaphore. Tests use this to hold permits
    /// directly (bypassing the bounded wait) when asserting
    /// utilization or release-on-error.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.sem
    }

    /// Configured capacity (denominator of [`Self::utilization`]).
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
