//! [`DagActorConfig`] / [`DagActorPlumbing`]: grouped construction
//! inputs for [`super::DagActor`]. Replaces the 14-positional
//! `spawn_with_leader` + 16-builder chain that had accreted around
//! `DagActor::new`.

use std::sync::Arc;

use tokio::sync::mpsc;
use tonic::transport::Channel;

use rio_proto::StoreServiceClient;

use crate::lease::LeaderState;
use crate::state::{PoisonConfig, RetryPolicy};

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
    /// Fetcher size-class config (I-170). Empty = feature off (single
    /// fetcher pool, no class filter). Ordered smallest→largest.
    pub fetcher_size_classes: Vec<crate::assignment::FetcherSizeClassConfig>,
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
