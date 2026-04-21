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

/// How long [`AdmissionGate::acquire_bounded`] queues server-side
/// before returning `RESOURCE_EXHAUSTED`. 30 s sits inside the
/// scheduler's detached-fetch retry window (8 attempts × backoff ≈
/// 90 s; see `r[sched.substitute.detached]`), so a transient burst
/// is absorbed in ONE store-side wait rather than N client retries.
/// Spike 0.1 proved the prior immediate-RE design demoted 50/50
/// derivations to build-from-source under any hold ≥ 8 s.
pub const SUBSTITUTE_ADMISSION_WAIT: Duration = Duration::from_secs(30);

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
    /// `let _permit = ...` so it lives to end-of-scope). After the
    /// wait expires, returns `RESOURCE_EXHAUSTED` — transient per
    /// [`rio_common::grpc::is_transient`], so callers retry. The
    /// timeout-expiry path increments
    /// `rio_store_substitute_admission_rejected_total`; sustained
    /// non-zero on that counter means the replica is genuinely
    /// saturated (ComponentScaler should already be reacting via the
    /// `GetLoad` utilization signal).
    ///
    /// `Unavailable` on semaphore-closed is unreachable in production
    /// (nothing calls `Semaphore::close`); mapped for completeness so
    /// a future `close()` on shutdown surfaces as a transient retry,
    /// not a panic.
    // r[impl store.substitute.admission]
    pub async fn acquire_bounded(&self) -> Result<OwnedSemaphorePermit, Status> {
        match tokio::time::timeout(SUBSTITUTE_ADMISSION_WAIT, self.sem.clone().acquire_owned())
            .await
        {
            Ok(Ok(p)) => Ok(p),
            Ok(Err(_)) => Err(Status::unavailable("admission gate closed")),
            Err(_) => {
                metrics::counter!("rio_store_substitute_admission_rejected_total").increment(1);
                Err(Status::resource_exhausted(
                    "substitute admission saturated; retry (transient)",
                ))
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
