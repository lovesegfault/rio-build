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
pub mod componentscaler;
pub mod fetcherpool;
pub mod gc_schedule;

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kube::Client;

/// Per-pool manifest idle-tracking state: `builderpool::job_common::
/// Bucket` → when it first went surplus. See `builderpool::manifest::
/// update_idle_and_reapable`. Type alias for `Ctx::manifest_idle`'s
/// inner map (clippy::type_complexity).
///
/// Not an intra-doc link: `Bucket` is `pub(crate)` (visible enough
/// for the type to resolve, not visible enough for rustdoc's public-
/// docs link resolver). `ManifestIdleState` is `pub` only because
/// `Ctx` (also `pub`) names it in a field type — neither is part of
/// the external API.
pub type ManifestIdleState = BTreeMap<builderpool::job_common::Bucket, Instant>;

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
    /// Headless Service host for rio-store. Injected as
    /// `RIO_STORE_BALANCE_HOST` into executor pods so the
    /// `BalancedChannel` p2c spreads load across store replicas.
    /// `None` = single-channel fallback (and on a single-replica
    /// store, single-channel is fine).
    pub store_balance_host: Option<String>,
    pub store_balance_port: u16,
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
    /// Per-pool per-bucket idle-since timestamp for manifest-mode
    /// scale-down (`r[ctrl.pool.manifest-scaledown]`). Outer key:
    /// pool `{namespace}/{name}`. Inner key: `(est_memory_bytes,
    /// est_cpu_millicores)` — same as `builderpool::job_common::Bucket`.
    /// Value: the Instant the bucket FIRST went surplus
    /// (`supply > demand`). Cleared when demand returns. A bucket
    /// idle for `scale_down_window` is eligible for Job deletion.
    ///
    /// `std::sync::Mutex` (not tokio) — same reasoning as
    /// `error_counts`: the critical section is a single map op.
    pub manifest_idle: Mutex<HashMap<String, ManifestIdleState>>,
    /// TTL-cached `GetSizeClassStatus` response. The ephemeral
    /// builderpool reconciler and the ComponentScaler reconciler
    /// both poll this on ~10s ticks; without the cache they'd
    /// double-poll the scheduler. 5s TTL: short enough that a
    /// reconciler never acts on data more than half a tick stale,
    /// long enough that two reconcilers ticking within the same
    /// second share one RPC.
    ///
    /// `tokio::sync::Mutex` (not `std`) because the critical
    /// section spans an await (the gRPC call).
    pub size_class_cache:
        tokio::sync::Mutex<Option<(Instant, rio_proto::types::GetSizeClassStatusResponse)>>,
    /// Scale-down stabilization window. Shared between the
    /// autoscaler (STS mode) and the manifest reconciler's per-
    /// bucket idle grace. Same 600s default / env-tunable as
    /// `ScalingTiming::scale_down_window` — the same anti-flap
    /// rationale (don't kill workers right before the next burst)
    /// applies per-bucket.
    pub scale_down_window: Duration,
}

impl Ctx {
    /// Bundle the scheduler address fields for pod-spec injection.
    /// Every reconciler that builds an executor pod-spec calls this
    /// instead of constructing `SchedulerAddrs` inline.
    pub fn scheduler_addrs(&self) -> common::sts::SchedulerAddrs {
        common::sts::UpstreamAddrs {
            addr: self.scheduler_addr.clone(),
            balance_host: self.scheduler_balance_host.clone(),
            balance_port: self.scheduler_balance_port,
        }
    }

    /// Bundle the store address fields for pod-spec injection.
    pub fn store_addrs(&self) -> common::sts::StoreAddrs {
        common::sts::UpstreamAddrs {
            addr: self.store_addr.clone(),
            balance_host: self.store_balance_host.clone(),
            balance_port: self.store_balance_port,
        }
    }

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
    /// `GetSizeClassStatus` with a 5s TTL cache. See the
    /// `size_class_cache` field doc for rationale.
    ///
    /// The lock is held across the gRPC call so two concurrent
    /// callers don't both miss-then-fetch (the second waits for the
    /// first's fetch then reads the fresh cache). Starvation isn't a
    /// concern: ≤2 reconcilers, ~10s tick.
    pub async fn size_class_status(
        &self,
    ) -> std::result::Result<rio_proto::types::GetSizeClassStatusResponse, tonic::Status> {
        const TTL: Duration = Duration::from_secs(5);
        let mut cache = self.size_class_cache.lock().await;
        if let Some((at, resp)) = cache.as_ref()
            && at.elapsed() < TTL
        {
            return Ok(resp.clone());
        }
        let resp = self
            .admin
            .clone()
            .get_size_class_status(rio_proto::types::GetSizeClassStatusRequest {})
            .await?
            .into_inner();
        *cache = Some((Instant::now(), resp.clone()));
        Ok(resp)
    }

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
