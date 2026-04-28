//! Reconciliation loops.
//!
//! Each reconciler is a `fn(Arc<CR>, Arc<Ctx>) -> Result<Action>`.
//! `Controller::new().owns().run()` calls it whenever the CR or an
//! owned child changes. The reconcile fn makes the world match the
//! spec: spawn/reap Jobs to track demand, patch status to reflect
//! observed state.
//!
//! Idempotent by construction: every reconcile starts from
//! scratch, reads current state, computes desired, applies the
//! diff via server-side apply. Reconciling twice is a no-op.

pub mod componentscaler;
pub mod gc_schedule;
pub mod node_informer;
pub mod nodeclaim_pool;
pub mod nodepoolbudget;
// WONTFIX: pub(crate) here ICEs stable rustdoc 1.94.1 ("no resolutions
// for a doc link" — collect_intra_doc_links). Re-verified 2026-04: still
// crashes the .#checks.*.doc build. Nightly is fine. Leaf binary; pub-ness
// is cosmetic. Re-tighten once the upstream ICE is fixed.
pub mod pool;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kube::Client;
use kube::runtime::controller::Action;
use parking_lot::Mutex;

use crate::error::{Error, Result, error_kind};

/// Re-export so reconciler modules can `use crate::reconcilers::
/// KubeErrorExt` without naming rio-crds directly.
pub use rio_crds::{KubeErrorExt, KubeResultExt};

/// Upper bound on every `AdminServiceClient` RPC issued from a
/// reconcile/watcher loop. `build_endpoint` sets `.connect_timeout()`
/// only; h2 keepalive detects dead transport (~40s) but not a
/// live-but-stalled scheduler (actor mailbox backlog, slow PG). A
/// hung await blocks the watcher loop indefinitely → subsequent
/// `DisruptionTarget` pods miss fast-preemption; a hung await inside
/// kube-runtime's reconciler blocks the entire `pool` reconciler with
/// no requeue. 5s: short enough to keep best-effort watchers
/// responsive, long enough that p99 admin RPCs (PG round-trip) clear.
pub(crate) const ADMIN_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound an `AdminServiceClient` RPC by [`ADMIN_RPC_TIMEOUT`],
/// mapping `Elapsed` → `Status::deadline_exceeded` so existing `Err`
/// arms handle it uniformly. ALL controller-side admin RPCs go
/// through this — the chokepoint makes "live-but-stalled scheduler
/// hangs the watcher/reconciler" unrepresentable. Data-plane RPCs
/// (long-poll, streaming) MUST NOT use this.
// r[impl ctrl.admin.rpc-timeout]
pub(crate) async fn admin_call<T>(
    fut: impl Future<Output = std::result::Result<tonic::Response<T>, tonic::Status>>,
) -> std::result::Result<tonic::Response<T>, tonic::Status> {
    match tokio::time::timeout(ADMIN_RPC_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_elapsed) => Err(tonic::Status::deadline_exceeded(
            "admin rpc timeout (live-but-stalled scheduler?)",
        )),
    }
}

/// `obj.namespace()` or `InvalidSpec("{Kind} has no namespace")`.
/// All rio-controller CRDs are `Namespaced`-scope; a missing
/// namespace is an apply-time error, not a transient. Replaces five
/// per-reconciler open-coded `ok_or_else(InvalidSpec("X has no
/// namespace"))` sites with one generic.
pub fn require_namespace<K: kube::Resource<DynamicType = ()>>(obj: &K) -> Result<String> {
    use kube::ResourceExt;
    obj.namespace()
        .ok_or_else(|| Error::InvalidSpec(format!("{} has no namespace", K::kind(&()))))
}

/// Shared `AdminServiceClient` shape: balanced channel +
/// [`ServiceTokenInterceptor`](rio_auth::hmac::ServiceTokenInterceptor)
/// (`caller="rio-controller"`). The scheduler gates controller-only
/// mutating RPCs (`AppendInterruptSample`, `DrainExecutor`,
/// `ReportExecutorTermination`, `AckSpawnedIntents`) on
/// `x-rio-service-token` — builders share the scheduler's port 9001 at L4
/// (CCNP), so without the gate a compromised builder could poison λ\[h\],
/// drain executors, or arm false ICE marks. All controller→scheduler
/// callsites (`Ctx.admin`, `disruption::run`, `node_informer::*`) use
/// this alias so a single interceptor covers every RPC.
pub type AdminClient = rio_proto::AdminServiceClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        rio_auth::hmac::ServiceTokenInterceptor,
    >,
>;

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
    /// Balanced AdminServiceClient with service-token interceptor
    /// (DrainExecutor in pool finalizer, AppendInterruptSample,
    /// ClusterStatus).
    pub admin: AdminClient,
    /// rio-scheduler addresses. For builder pod env injection ONLY
    /// (reconcilers use `admin` above). `balance_host = None` → env
    /// var NOT injected → builders fall back to single-channel.
    pub scheduler: pool::pod::UpstreamAddrs,
    /// rio-store addresses. Injected into builder pod containers
    /// (builders connect to the store directly for PutPath/GetPath).
    pub store: pool::pod::UpstreamAddrs,
    /// Recorder for K8s Events. Reconcilers call `ctx.publish_
    /// event(obj, ev)` to emit; events show in `kubectl describe`
    /// and `kubectl get events`. Operator visibility for "what
    /// did the controller just do" without scraping logs.
    ///
    /// kube 3.0 API: Recorder is constructed ONCE with (client,
    /// reporter); `publish` takes the object_ref per-call. So we
    /// hold one Recorder and pass the ref at publish time.
    pub recorder: kube::runtime::events::Recorder,
    /// `x-rio-service-token` minting interceptor
    /// (`caller="rio-controller"`). Cloned onto every
    /// `StoreAdminServiceClient` channel — `r[store.admin.service-gate]`
    /// requires it on `TriggerGC` / `GetLoad`. Same interceptor
    /// instance as the `admin` client above (same key, same caller).
    pub service_interceptor: rio_auth::hmac::ServiceTokenInterceptor,
    /// Consecutive error count per `{kind}/{ns}/{name}`. Incremented
    /// by `error_policy`, reset on successful reconcile. Drives
    /// exponential backoff so a persistent apiserver 5xx doesn't
    /// retry every 30s indefinitely. `parking_lot::Mutex` (not
    /// tokio): error_policy is a sync fn, the critical section is
    /// a single HashMap op, and parking_lot has no poison so a
    /// panic in one reconciler can't wedge another's error_policy.
    pub error_counts: Mutex<HashMap<String, u32>>,
    /// In-process state owned by the ComponentScaler reconciler.
    pub scaler: ScalerState,
    /// ADR-023 §13a: `resources.requests.memory` floor below which the
    /// `rio.build/hw-bench-needed` annotation is forced false. STREAM
    /// triad's working set is ~4.6 GiB on the largest LLC; running it
    /// on a `preferLocalBuild`/fetcher pod sized at 1-4 GiB would OOM
    /// before the build even starts. Same value as the scheduler's
    /// `[sla].hw_bench_mem_floor` (helm renders both from
    /// `sla.hwBenchMemFloor`); duplicated here so the controller stays
    /// PG-/scheduler-config-free.
    pub hw_bench_mem_floor: u64,
}

/// ComponentScaler reconciler state.
#[derive(Default)]
pub struct ScalerState {
    /// Per-ComponentScaler low-load tick counter, keyed by
    /// `{ns}/{name}`. In-process (NOT in `.status`): writing
    /// `lowLoadTicks` to status on every tick changes the CR,
    /// which the Controller watch picks up, which re-triggers
    /// reconcile immediately — tight loop instead of the 10s
    /// `Action::requeue`. The cost of in-process is a controller
    /// restart resets the streak (≤5 min of extra over-
    /// provisioning) — acceptable. `learnedRatio` (the durable
    /// bit) stays in `.status`.
    pub low_ticks: Mutex<HashMap<String, u32>>,
    /// Per-ComponentScaler last successful `patch_status` time,
    /// keyed by `{ns}/{name}`. Rate-limits status writes to once
    /// per `REQUEUE` window: `decide()` mutates `learnedRatio` on
    /// every high-load tick, so without this gate every write →
    /// resourceVersion bump → watch fires → re-reconcile at
    /// loop-rate, and the 5%-per-10s decay becomes 5%-per-loop-
    /// iteration (ratio 50→1 in seconds, not ~13min). bug_213.
    pub last_status_write: Mutex<HashMap<String, Instant>>,
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

    // r[impl ctrl.backoff.per-object]
    /// Increment the consecutive-error count for an object and
    /// return the requeue delay. Exponential: 5s × 2^n capped at
    /// 5min. A persistent apiserver 5xx backs off to 5min after
    /// ~6 failures (5→10→20→40→80→160→300s) instead of retrying
    /// every 30s indefinitely.
    ///
    /// Keyed by `{kind}/{ns}/{name}` — stable across error_policy
    /// calls for the same object; distinct across reconcilers.
    pub fn error_backoff(&self, key: &str) -> Duration {
        let mut counts = self.error_counts.lock();
        let n = counts.entry(key.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        transient_backoff(*n)
    }

    /// Reset the consecutive-error count for an object. Called on
    /// successful reconcile so the next failure starts the backoff
    /// curve from zero (not from where the last failure streak
    /// left off).
    pub fn reset_error_count(&self, key: &str) {
        self.error_counts.lock().remove(key);
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
    rio_common::backoff::Backoff {
        base: BACKOFF_BASE,
        mult: 2.0,
        cap: BACKOFF_CAP,
        jitter: rio_common::backoff::Jitter::None,
    }
    .duration(n.saturating_sub(1))
}

/// Build the error-count key for a namespaced K8s object.
/// `{kind}/{ns}/{name}` — stable, distinct across reconcilers
/// (Pool vs ComponentScaler can't collide).
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

/// Standard reconcile span+timing+metrics wrapper. Every reconciler's
/// public `reconcile()` is `timed("name", obj, ctx, reconcile_inner)`:
/// opens a `reconcile{reconciler=name, name=…, ns=…}` span, records
/// `rio_controller_reconcile_duration_seconds{reconciler=name}`
/// (success AND error paths — slow apiserver timeouts show as long
/// duration + error), and resets the per-object error-backoff counter
/// on success so the next failure starts the curve from 5s.
///
/// The span lives here (not as a per-reconciler `#[instrument]` attr)
/// so the `reconciler` label is named once — previously each module
/// passed it both in `#[instrument(fields(reconciler=…))]` AND in
/// `timed("…", …)`, and the four attrs differed only in the field
/// name for the object (`pool`/`wps`/`cs`). The uniform `name` field
/// is what `error_key` already uses.
pub async fn timed<K, F, Fut>(
    reconciler: &'static str,
    obj: Arc<K>,
    ctx: Arc<Ctx>,
    f: F,
) -> Result<Action>
where
    K: kube::Resource<DynamicType = ()> + kube::ResourceExt,
    F: FnOnce(Arc<K>, Arc<Ctx>) -> Fut,
    Fut: Future<Output = Result<Action>>,
{
    use tracing::Instrument;
    let span = tracing::info_span!(
        "reconcile",
        reconciler,
        name = %obj.name_any(),
        ns = obj.namespace().as_deref().unwrap_or(""),
    );
    async move {
        let start = Instant::now();
        let key = error_key(obj.as_ref());
        let result = f(obj, ctx.clone()).await;
        if result.is_ok() {
            ctx.reset_error_count(&key);
        }
        metrics::histogram!("rio_controller_reconcile_duration_seconds",
            "reconciler" => reconciler)
        .record(start.elapsed().as_secs_f64());
        result
    }
    .instrument(span)
    .await
}

/// Standard finalizer-wrapped reconcile body for namespaced CRs.
/// Derives `Api<K>` from the object's namespace, runs
/// `kube::runtime::finalizer` with the given apply/cleanup, and maps
/// the recursive `finalizer::Error<Error>` into our boxed
/// [`Error::Finalizer`].
///
/// `apply`/`cleanup` take `Arc<Ctx>` (not `&Ctx`) so the closure
/// passed to `finalizer()` can move them in by value without lifetime
/// gymnastics; both arms can't run, so each `FnOnce` is consumed at
/// most once.
pub(crate) async fn finalized<K, A, AFut, C, CFut>(
    obj: Arc<K>,
    ctx: Arc<Ctx>,
    finalizer_name: &'static str,
    apply: A,
    cleanup: C,
) -> Result<Action>
where
    K: kube::Resource<DynamicType = (), Scope = kube::core::NamespaceResourceScope>
        + Clone
        + serde::Serialize
        + serde::de::DeserializeOwned
        + std::fmt::Debug
        + Send
        + Sync
        + 'static,
    A: FnOnce(Arc<K>, Arc<Ctx>) -> AFut + Send,
    C: FnOnce(Arc<K>, Arc<Ctx>) -> CFut + Send,
    AFut: Future<Output = Result<Action>> + Send,
    CFut: Future<Output = Result<Action>> + Send,
{
    use kube::runtime::finalizer::{Event, finalizer};
    let ns = require_namespace(&*obj)?;
    let api: kube::Api<K> = kube::Api::namespaced(ctx.client.clone(), &ns);
    finalizer(&api, finalizer_name, obj, move |event| async move {
        match event {
            Event::Apply(o) => apply(o, ctx.clone()).await,
            Event::Cleanup(o) => cleanup(o, ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

/// Standard `error_policy`: emit `reconcile_errors_total`, then
/// `InvalidSpec` → fixed `invalid_requeue` (operator must fix the
/// CRD; retrying fast is noise), transient → exponential backoff via
/// [`Ctx::error_backoff`] (5s → 300s cap, reset on next success).
///
/// `invalid_requeue` is per-reconciler: most use 300s; ComponentScaler
/// uses 30s because transient scheduler/store unreachability funnels
/// through `InvalidSpec` there and 5min of no scaling under a builder
/// burst is the I-105 cliff.
pub fn standard_error_policy<K>(
    name: &'static str,
    obj: Arc<K>,
    err: &Error,
    ctx: Arc<Ctx>,
    invalid_requeue: Duration,
) -> Action
where
    K: kube::Resource<DynamicType = ()> + kube::ResourceExt,
{
    // Unwrap `Finalizer(ApplyFailed|CleanupFailed(e))` → `e`: the
    // reconcile body's real error. Without this every Pool error
    // labels `error_kind="finalizer"` and a wrapped InvalidSpec takes
    // the exponential arm below instead of fixed `invalid_requeue`.
    let err = crate::error::leaf(err);
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => name, "error_kind" => error_kind(err))
    .increment(1);
    match err {
        Error::InvalidSpec(msg) => {
            tracing::warn!(reconciler = name, error = %msg,
                "invalid spec / upstream unreachable; fix the CRD");
            Action::requeue(invalid_requeue)
        }
        _ => {
            let delay = ctx.error_backoff(&error_key(obj.as_ref()));
            tracing::warn!(reconciler = name, error = %err, backoff = ?delay,
                "reconcile failed; retrying");
            Action::requeue(delay)
        }
    }
}

/// Build a `Controller::new().owns().run().for_each()` future for a
/// reconciler module. Expands to the ~25L block that was repeated 4×
/// in `main.rs`. The result is a future — caller `tokio::join!`s them.
///
/// Two forms: with `owns: Type` (Pool watches child Jobs) and
/// without (ComponentScaler owns nothing — it patches `/scale` on a
/// helm-owned Deployment).
///
/// The `for_each` body is debug-only: `error_policy` already logged
/// the real error; the controller-runtime wrapper errors here
/// (ObjectNotFound on a delete race etc.) are normal.
#[macro_export]
macro_rules! spawn_controller {
    ($client:expr, $shutdown:expr, $ctx:expr, $crd:ty, $module:ident $(, owns: $owned:ty)?) => {{
        use ::futures_util::StreamExt as _;
        let api: ::kube::Api<$crd> = ::kube::Api::all($client.clone());
        ::kube::runtime::Controller::new(api, ::kube::runtime::watcher::Config::default())
            $(.owns(
                ::kube::Api::<$owned>::all($client.clone()),
                ::kube::runtime::watcher::Config::default(),
            ))?
            .graceful_shutdown_on($shutdown.clone().cancelled_owned())
            .run($module::reconcile, $module::error_policy, $ctx.clone())
            .for_each(|res| async move {
                match res {
                    Ok((obj, _)) => ::tracing::debug!(obj = %obj.name, "reconciled"),
                    Err(e) => ::tracing::debug!(error = %e, "reconcile loop error"),
                }
            })
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backoff curve: 5s base, double per attempt, cap at 300s.
    /// Proves the ~5min cap and the reset-to-base after success.
    // r[verify ctrl.backoff.per-object]
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

    /// `admin_call` bounds a hung future by `ADMIN_RPC_TIMEOUT` and
    /// maps `Elapsed` → `Status::deadline_exceeded`. Every controller-
    /// side admin RPC goes through this chokepoint, so the live-but-
    /// stalled-scheduler hang is unrepresentable by construction.
    // r[verify ctrl.admin.rpc-timeout]
    #[tokio::test(start_paused = true)]
    async fn admin_call_times_out_on_stalled_scheduler() {
        let pending = std::future::pending::<Result<tonic::Response<()>, tonic::Status>>();
        let r = admin_call(pending).await;
        assert_eq!(
            r.unwrap_err().code(),
            tonic::Code::DeadlineExceeded,
            "hung admin RPC → DeadlineExceeded after {ADMIN_RPC_TIMEOUT:?}"
        );
        // Passthrough: completed Ok/Err are not touched.
        let ok = admin_call(async { Ok(tonic::Response::new(())) }).await;
        assert!(ok.is_ok());
        let err =
            admin_call(async { Err::<tonic::Response<()>, _>(tonic::Status::unavailable("x")) })
                .await;
        assert_eq!(err.unwrap_err().code(), tonic::Code::Unavailable);
    }
}
