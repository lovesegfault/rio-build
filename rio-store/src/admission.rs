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
    /// Transient — the gateway caller retries
    /// (`r[gw.store.transient-retry]`); the in-process materialization
    /// executor re-arms through its job budget.
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
/// retry windows of the surviving callers (the gateway's transient
/// retry; the executor's claim/re-arm cycle), so a transient burst
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
#[derive(Clone, Debug)]
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
    /// `let _permit = ...` so it lives to end-of-scope). The returned
    /// [`AdmissionPermit`] gives the gate BOTH gauge edges
    /// (obs.metric.store-gauge-ownership): acquire publishes the rise
    /// here, the permit's `Drop` releases and republishes the fall —
    /// emitting in this module (not at call sites) keeps the gauge
    /// coupled to wherever acquires and releases move. After the wait
    /// expires, returns [`AdmissionError::Saturated`] (maps to
    /// `RESOURCE_EXHAUSTED` — transient per
    /// [`rio_common::grpc::is_transient`], so callers retry). The
    /// timeout-expiry path increments
    /// `rio_store_substitute_admission_rejected_total`; sustained
    /// non-zero on that counter means the replica is genuinely
    /// saturated (the store ScaledObject's backlog/CPU triggers
    /// should already be scaling replicas out).
    // r[impl store.substitute.admission+2]
    // r[impl obs.metric.store-gauge-ownership]
    pub async fn acquire_bounded(&self) -> Result<AdmissionPermit, AdmissionError> {
        match tokio::time::timeout(SUBSTITUTE_ADMISSION_WAIT, self.sem.clone().acquire_owned())
            .await
        {
            Ok(Ok(p)) => {
                metrics::gauge!("rio_store_substitute_admission_utilization")
                    .set(f64::from(self.utilization()));
                Ok(AdmissionPermit {
                    inner: Some(p),
                    gate: self.clone(),
                })
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

/// A held substitution-admission permit. Releasing (Drop) returns the
/// permit to the gate FIRST, then republishes
/// `rio_store_substitute_admission_utilization` from the gate's
/// post-release truth — the gate owns both edges of its gauge, so the
/// series can never freeze at an acquire-time value (bug_245: the
/// last acquire froze 1.0 on the scrape surface after the burst
/// drained; GetLoad — whose retired ComponentScaler caller was the
/// only periodic refresher — never corrected it).
///
/// Concurrent drops may interleave release/republish pairs and
/// transiently overstate by one permit; the periodic store gauge tick
/// (grpc::spawn_store_gauge_tick) heals any such race within 30s — do
/// NOT add locking here.
#[derive(Debug)]
pub struct AdmissionPermit {
    inner: Option<OwnedSemaphorePermit>,
    gate: AdmissionGate,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        drop(self.inner.take());
        metrics::gauge!("rio_store_substitute_admission_utilization")
            .set(f64::from(self.gate.utilization()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::metrics::CountingRecorder;

    const GAUGE: &str = "rio_store_substitute_admission_utilization{}";

    /// bug_245's structural pin: the gauge follows the permit DOWN.
    /// The gate owns BOTH edges — acquire publishes the rise,
    /// [`AdmissionPermit`]'s Drop releases the permit and republishes
    /// the fall. Pre-fix red (recorded in the introducing commit):
    /// `acquire_bounded` returned a bare `OwnedSemaphorePermit`, so
    /// the LAST acquire-time value froze on the scrape surface — a
    /// cap-2 gate read 1.0 forever after its burst drained, and KEDA /
    /// the store-scaling dashboard saw a permanently saturated
    /// replica.
    #[tokio::test]
    async fn permit_drop_republishes_utilization() {
        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let gate = AdmissionGate::new(2);
        let p1 = gate.acquire_bounded().await.expect("permit 1");
        let p2 = gate.acquire_bounded().await.expect("permit 2");
        assert_eq!(
            recorder.gauge_value(GAUGE),
            Some(1.0),
            "two of two permits held — acquire edge publishes 1.0"
        );

        drop(p1);
        assert_eq!(
            recorder.gauge_value(GAUGE),
            Some(0.5),
            "one permit released — the drop edge must republish 0.5 \
             (pre-fix: frozen at the last acquire-time 1.0)"
        );
        drop(p2);
        assert_eq!(
            recorder.gauge_value(GAUGE),
            Some(0.0),
            "all permits released — the drop edge must republish 0.0"
        );
    }
}
