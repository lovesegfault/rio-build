//! [`DagActorConfig`] / [`DagActorPlumbing`]: grouped construction
//! inputs for [`super::DagActor`]. Replaces the 14-positional
//! `spawn_with_leader` + 16-builder chain that had accreted around
//! `DagActor::new`.

use std::sync::Arc;

use tokio::sync::mpsc;
use tonic::transport::Channel;

use rio_proto::StoreServiceClient;

use crate::assignment::{SizeClassConfig, SoftFeature};
use crate::dag::DerivationDag;
use crate::lease::LeaderState;
use crate::state::{PoisonConfig, RetryPolicy};

/// Size-class routing + soft-feature config. Sub-struct of
/// [`super::DagActor`] — bundles the five fields that control where a
/// derivation lands. All read-only after construction except
/// `size_classes`, whose `cutoff_secs` the rebalancer mutates hourly
/// via the shared `Arc<RwLock<_>>`.
///
/// Fields are `pub(super)` for direct read access from
/// dispatch/completion/snapshot — these are pure config, no invariant
/// to guard. The struct exists to make ownership clear and to
/// centralize the DAG soft-feature setup ([`apply_to_dag`]) that was
/// previously open-coded at both construction and leader-transition
/// reset.
pub(crate) struct SizingConfig {
    /// Builder size-class cutoff config. Empty = feature off (no
    /// classification). dispatch.rs calls classify() with a read
    /// guard; completion.rs reads cutoff_for() for misclassification
    /// detection.
    ///
    /// `Arc<parking_lot::RwLock<...>>` — shared with the rebalancer
    /// task (spawned in `run_inner`) which writes new cutoffs hourly.
    /// parking_lot not tokio::sync: writes are rare (1/hour) so
    /// contention is near-zero, and a sync lock keeps `classify()`
    /// sync — no `.await` inside dispatch's hot read path.
    ///
    /// R10 CHECK: callers MUST NOT hold a read/write guard across
    /// `.await`. parking_lot guards are not `Send` so the borrow
    /// checker catches some misuse, but a `.read()` followed by
    /// `.await` on the same task blocks the executor thread. See
    /// dispatch.rs: guards are dropped before any await boundary.
    pub(super) size_classes: Arc<parking_lot::RwLock<Vec<SizeClassConfig>>>,
    /// Fetcher size-class config (I-170). Empty = feature off (single
    /// fetcher pool, no class filter — original behavior). Ordered
    /// smallest→largest; `find_executor_with_overflow`'s FOD branch
    /// walks from `DerivationState.sched.size_class_floor` upward.
    /// Plain `Vec` (not `Arc<RwLock>`): no rebalancer mutates this —
    /// it's an ordered name list, config-static after construction.
    pub(super) fetcher_classes: Vec<String>,
    /// Static TOML cutoffs, captured once from `cfg.size_classes`
    /// BEFORE the rebalancer's first write. The rebalancer mutates
    /// `size_classes[i].cutoff_secs` in-place hourly; without this
    /// snapshot, there's no way to report drift to operators.
    /// `(name, cutoff_secs)` pairs — Vec preserves config order
    /// (matters for the RPC response which sorts by effective cutoff,
    /// but configured order is useful for logging).
    pub(super) configured_cutoffs: Vec<(String, f64)>,
    /// ADR-020 capacity manifest headroom. Applied by both
    /// `compute_capacity_manifest` (manifest RPC) and the
    /// dispatch-time resource-fit filter. Config-global; per-pool
    /// later if needed. Validated finite + positive at startup
    /// (main.rs).
    pub(super) headroom_mult: f64,
    /// I-204: capability-hint features stripped at DAG insertion.
    /// Re-applied by [`apply_to_dag`] on every fresh DAG (recovery
    /// replaces `DagActor.dag` on each leader transition).
    pub(super) soft_features: Vec<SoftFeature>,
}

impl SizingConfig {
    pub(super) fn new(cfg: &DagActorConfig) -> Self {
        // Snapshot the as-loaded cutoffs BEFORE the rebalancer sees
        // them. GetSizeClassSnapshot reports both: effective (mutated
        // hourly) vs configured (this snapshot) for drift visibility.
        let configured_cutoffs = cfg
            .size_classes
            .iter()
            .map(|c| (c.name.clone(), c.cutoff_secs))
            .collect();
        Self {
            size_classes: Arc::new(parking_lot::RwLock::new(cfg.size_classes.clone())),
            fetcher_classes: cfg.fetcher_size_classes.clone(),
            configured_cutoffs,
            headroom_mult: cfg.headroom_mult,
            soft_features: cfg.soft_features.clone(),
        }
    }

    /// Configure soft-feature stripping on `dag`. The class-order
    /// snapshot (for I-213 floor-hint comparison) is derived from
    /// `size_classes` so soft_features always sees the right order.
    /// Called from `DagActor::new` and `clear_persisted_state` — the
    /// latter replaces `self.dag` on every leader transition and would
    /// otherwise drop soft_features (regression: the original
    /// open-coded reset at each site meant the first prod deploy of
    /// I-204 was a no-op after the lease acquired).
    pub(super) fn apply_to_dag(&self, dag: &mut DerivationDag) {
        let order = crate::assignment::builder_class_order(&self.size_classes.read());
        dag.set_soft_features(self.soft_features.clone(), order);
    }
}

/// Immutable-after-init configuration for [`super::DagActor`]. All
/// fields are operator deploy config (scheduler.toml or env) and are
/// not mutated after the actor is spawned.
///
/// `Default` matches the prior `DagActor::new()` hardcoded defaults so
/// tests / non-K8s spawns can `..Default::default()` and override one
/// or two fields.
#[derive(Debug, Clone)]
pub struct DagActorConfig {
    /// Retry backoff policy. Default: 2 retries, 5s→300s exponential
    /// with 20% jitter. main.rs loads from scheduler.toml `[retry]`.
    pub retry_policy: RetryPolicy,
    /// Poison threshold + distinct-workers config. Default (3 distinct
    /// workers) matches the former `POISON_THRESHOLD` const. main.rs
    /// loads from scheduler.toml `[poison]`.
    pub poison: PoisonConfig,
    /// Timeout for metadata gRPC calls to the store (FindMissingPaths,
    /// QueryPathInfo). Tests that arm a hung MockStore override to 3s
    /// for the wrapper-exists proof.
    pub grpc_timeout: std::time::Duration,
    /// Max in-flight `QueryPathInfo` calls during merge-time eager
    /// substitute fetch. Overridable via `RIO_SUBSTITUTE_MAX_CONCURRENT`.
    pub substitute_max_concurrent: usize,
    /// Builder size-class cutoff config. Empty = feature off (no
    /// classification). The rebalancer mutates the `cutoff_secs` of
    /// the actor-owned `Arc<RwLock<...>>` derived from this; the
    /// initial values are also captured into `configured_cutoffs` for
    /// drift reporting.
    pub size_classes: Vec<crate::assignment::SizeClassConfig>,
    /// Fetcher size-class names (I-170), ordered smallest→largest.
    /// Empty = feature off (single fetcher pool, no class filter).
    pub fetcher_size_classes: Vec<String>,
    /// `requiredSystemFeatures` values stripped at DAG insertion
    /// (I-204). Empty preserves pre-I-204 behavior — every feature is
    /// a gate.
    pub soft_features: Vec<crate::assignment::SoftFeature>,
    /// ADR-020 capacity manifest headroom. Validated finite + positive
    /// at startup (main.rs).
    pub headroom_mult: f64,
}

impl Default for DagActorConfig {
    fn default() -> Self {
        Self {
            retry_policy: RetryPolicy::default(),
            poison: PoisonConfig::default(),
            grpc_timeout: rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            substitute_max_concurrent: super::DEFAULT_SUBSTITUTE_CONCURRENCY,
            size_classes: Vec::new(),
            fetcher_size_classes: Vec::new(),
            soft_features: Vec::new(),
            headroom_mult: crate::estimator::DEFAULT_HEADROOM_MULTIPLIER,
        }
    }
}

/// Runtime plumbing for [`super::DagActor`]: channels and shared state
/// that connect the actor to other tasks. Unlike [`DagActorConfig`],
/// these are not "settings" — they are wires.
///
/// `Default` gives the test/bench shape: no store, no flusher/persister,
/// always-leader, fresh never-cancelled shutdown token.
pub struct DagActorPlumbing {
    /// Store service client for scheduler-side cache checks. `None` in
    /// tests that don't need the store (cache check is then skipped).
    pub store_client: Option<StoreServiceClient<Channel>>,
    /// Channel to the LogFlusher task. Completion handlers `try_send` a
    /// FlushRequest here.
    pub log_flush_tx: Option<mpsc::Sender<crate::logs::FlushRequest>>,
    /// Channel to the event-log persister task.
    pub event_persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
    /// HMAC signer for assignment tokens. `None` = legacy unsigned
    /// format-string (dev mode).
    pub hmac_signer: Option<Arc<rio_common::hmac::HmacSigner>>,
    /// Leader-election shared state. The lease task writes
    /// `is_leader`/`generation`; the actor reads both and writes
    /// `recovery_complete`. Non-K8s/test default is
    /// [`LeaderState::always_leader`].
    pub leader: LeaderState,
    /// Shutdown token. The run loop `select!`s on `cancelled()` with
    /// `biased` ordering so SIGTERM drains workers immediately.
    pub shutdown: rio_common::signal::Token,
    /// Test-only: oneshot pair for deterministic interleaving in
    /// `handle_leader_acquired`. When set, the actor sends on `.0`
    /// after `recover_from_pg()` returns, then awaits `.1` before the
    /// gen re-check — lets the TOCTOU test bump `generation` between
    /// recovery completion and the staleness check without mocking PG.
    #[cfg(test)]
    pub recovery_toctou_gate: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
}

impl Default for DagActorPlumbing {
    fn default() -> Self {
        Self {
            store_client: None,
            log_flush_tx: None,
            event_persist_tx: None,
            hmac_signer: None,
            leader: LeaderState::default(),
            shutdown: rio_common::signal::Token::new(),
            #[cfg(test)]
            recovery_toctou_gate: None,
        }
    }
}
