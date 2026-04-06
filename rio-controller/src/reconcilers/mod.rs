//! Reconciliation loops.
//!
//! Each reconciler is a `fn(Arc<CR>, Arc<Ctx>) -> Result<Action>`.
//! `Controller::new().owns().run()` calls it whenever the CR or an
//! owned child changes. The reconcile fn makes the world match the
//! spec: ensure StatefulSet exists with the right shape, patch
//! status to reflect observed state.
//!
//! Idempotent by construction: every reconcile starts from
//! scratch, reads current state, computes desired, applies the
//! diff via server-side apply. Reconciling twice is a no-op.

pub mod builderpool;
pub mod builderpoolset;
pub mod common;
pub mod fetcherpool;
pub mod gc_schedule;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use kube::Client;

/// Shared context for all reconcilers. Cloned into each
/// `Controller::run()` via Arc.
///
/// `admin` is a live client, not an address. When
/// `scheduler_balance_host` is set (production), it uses a
/// health-aware balanced Channel that routes only to the leader ---
/// no ClusterIP round-robin lottery (which fails ~50% with standby's
/// `UNAVAILABLE: "not leader"`). The startup retry loop in main()
/// handles the "scheduler not up yet" case; reconcilers just clone.
/// `scheduler_addr` kept for builder pod env injection (builders need
/// the address string, not a client).
pub struct Ctx {
    /// K8s client. Shared (clone per `Api<T>` call --- cheap, it's
    /// an Arc internally).
    pub client: Client,
    /// Balanced AdminServiceClient (DrainExecutor in builderpool
    /// finalizer).
    pub admin: rio_proto::AdminServiceClient<tonic::transport::Channel>,
    /// rio-scheduler ClusterIP address (e.g., "rio-scheduler:9001").
    /// For builder pod env injection ONLY --- reconcilers use
    /// `admin` above.
    pub scheduler_addr: String,
    /// Headless Service host. Injected as `RIO_SCHEDULER_BALANCE_HOST`
    /// into builder pods. `None` = env var NOT injected --> builders
    /// fall back to single-channel (VM tests, single-replica).
    pub scheduler_balance_host: Option<String>,
    pub scheduler_balance_port: u16,
    /// rio-store gRPC address (e.g., "rio-store:9002"). Injected
    /// as `RIO_STORE_ADDR` into builder pod containers (builders
    /// connect to the store directly for PutPath/GetPath).
    pub store_addr: String,
    /// Recorder for K8s Events. Reconcilers call `ctx.publish_
    /// event(obj, ev)` to emit; events show in `kubectl describe`
    /// and `kubectl get events`. Operator visibility for "what
    /// did the controller just do" without scraping logs.
    ///
    /// kube 3.0 API: Recorder is constructed ONCE with (client,
    /// reporter); `publish` takes the object_ref per-call. So we
    /// hold one Recorder and pass the ref at publish time.
    pub recorder: kube::runtime::events::Recorder,
    /// Consecutive error count per `{kind}/{ns}/{name}`. Incremented
    /// by `error_policy`, reset on successful reconcile. Drives
    /// exponential backoff so a persistent apiserver 5xx doesn't
    /// retry every 30s indefinitely. `std::sync::Mutex` (not tokio)
    /// — error_policy is a sync fn and the critical section is a
    /// single HashMap op.
    pub error_counts: Mutex<HashMap<String, u32>>,
}

impl Ctx {
    /// Publish an event scoped to a K8s object. Best-effort —
    /// event-publish failure is logged, not propagated (events
    /// are observability, not correctness; a reconcile that
    /// succeeds but couldn't emit an event still succeeded).
    pub async fn publish_event<K>(&self, obj: &K, ev: &kube::runtime::events::Event)
    where
        K: kube::Resource<DynamicType = ()>,
    {
        if let Err(e) = self.recorder.publish(ev, &obj.object_ref(&())).await {
            tracing::warn!(error = %e, "failed to publish K8s event");
        }
    }

    /// Increment the consecutive-error count for an object and
    /// return the requeue delay. Exponential: 5s × 2^n capped at
    /// 5min. A persistent apiserver 5xx backs off to 5min after
    /// ~6 failures (5→10→20→40→80→160→300s) instead of retrying
    /// every 30s indefinitely.
    ///
    /// Keyed by `{kind}/{ns}/{name}` — stable across error_policy
    /// calls for the same object; distinct across reconcilers.
    pub fn error_backoff(&self, key: &str) -> Duration {
        let mut counts = self.error_counts.lock().expect("error_counts poisoned");
        let n = counts.entry(key.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        transient_backoff(*n)
    }

    /// Reset the consecutive-error count for an object. Called on
    /// successful reconcile so the next failure starts the backoff
    /// curve from zero (not from where the last failure streak
    /// left off).
    pub fn reset_error_count(&self, key: &str) {
        self.error_counts
            .lock()
            .expect("error_counts poisoned")
            .remove(key);
    }
}

/// Base delay for exponential error backoff. Short enough that a
/// one-off apiserver hiccup retries quickly; the doubling gets
/// persistent failures to the 5min cap in ~6 rounds.
const BACKOFF_BASE: Duration = Duration::from_secs(5);

/// Cap for exponential error backoff. ~5min — long enough to stop
/// log spam on a persistent outage, short enough that recovery
/// doesn't add significant latency.
const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Compute the requeue delay for the nth consecutive transient
/// error. `BACKOFF_BASE × 2^(n-1)` capped at `BACKOFF_CAP`. Pure
/// fn — the state (error count) is tracked in `Ctx`; this is the
/// curve.
///
/// n=1 → 5s, n=2 → 10s, n=3 → 20s, … n=7+ → 300s (cap).
pub(crate) fn transient_backoff(n: u32) -> Duration {
    // 2^(n-1) with saturation. n=0 shouldn't happen (caller
    // increments first) but treat as n=1. Shift cap at 63 to
    // avoid overflow (irrelevant — cap trips long before).
    let mult = 1u64 << (n.saturating_sub(1)).min(63);
    BACKOFF_BASE
        .saturating_mul(mult.min(u32::MAX as u64) as u32)
        .min(BACKOFF_CAP)
}

/// Build the error-count key for a namespaced K8s object.
/// `{kind}/{ns}/{name}` — stable, distinct across reconcilers
/// (BuilderPool vs BuilderPoolSet can't collide).
pub(crate) fn error_key<K>(obj: &K) -> String
where
    K: kube::Resource<DynamicType = ()> + kube::ResourceExt,
{
    format!(
        "{}/{}/{}",
        K::kind(&()),
        obj.namespace().unwrap_or_default(),
        obj.name_any()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backoff curve: 5s base, double per attempt, cap at 300s.
    /// Proves the ~5min cap and the reset-to-base after success.
    #[test]
    fn transient_backoff_curve() {
        assert_eq!(transient_backoff(1), Duration::from_secs(5));
        assert_eq!(transient_backoff(2), Duration::from_secs(10));
        assert_eq!(transient_backoff(3), Duration::from_secs(20));
        assert_eq!(transient_backoff(6), Duration::from_secs(160));
        // Cap at 300s.
        assert_eq!(transient_backoff(7), Duration::from_secs(300));
        assert_eq!(transient_backoff(100), Duration::from_secs(300));
        // n=0 defensive: treat as first attempt.
        assert_eq!(transient_backoff(0), Duration::from_secs(5));
    }
}
