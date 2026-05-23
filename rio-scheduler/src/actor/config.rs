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
    /// In-flight detached substitute-fetch task bound — memory-safety
    /// only, NOT a throughput throttle. Per-replica admission is
    /// `r[store.substitute.admission]`; saturation surfaces as
    /// `ResourceExhausted` → handled by `SUBSTITUTE_FETCH_BACKOFF`.
    /// Overridable via `RIO_SUBSTITUTE_MAX_CONCURRENT`.
    pub substitute_max_concurrent: usize,
    /// `requiredSystemFeatures` values stripped at DAG insertion
    /// (I-204). Empty preserves pre-I-204 behavior — every feature is
    /// a gate.
    pub soft_features: Vec<String>,
    /// ADR-023 SLA-driven sizing config (`[sla]` table). Mandatory —
    /// `Default` uses [`crate::sla::config::SlaConfig::test_default`]
    /// (single best-effort tier, tiny ceilings).
    pub sla: crate::sla::config::SlaConfig,
}

impl Default for DagActorConfig {
    fn default() -> Self {
        Self {
            retry_policy: RetryPolicy::default(),
            poison: PoisonConfig::default(),
            grpc_timeout: rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            substitute_max_concurrent: super::DEFAULT_SUBSTITUTE_CONCURRENCY,
            soft_features: Vec::new(),
            sla: crate::sla::config::SlaConfig::test_default(),
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
    /// Shared log ring buffers. The actor only calls
    /// [`LogBuffers::seal`](crate::logs::LogBuffers::seal) on terminal
    /// completion so a late `LogBatch` can't recreate a drained entry.
    /// `None` in tests that don't exercise the log pipeline.
    pub log_buffers: Option<Arc<crate::logs::LogBuffers>>,
    /// Channel to the event-log persister task.
    pub event_persist_tx: Option<mpsc::Sender<crate::event_log::EventLogEntry>>,
    /// HMAC signer for assignment tokens. `None` = legacy unsigned
    /// format-string (dev mode).
    pub hmac_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// HMAC signer for `x-rio-service-token` (SEPARATE key from
    /// `hmac_signer`). `None` = dispatch-time substitution probe
    /// degrades to local-presence-only —
    /// `r[sched.dispatch.fod-substitute]`.
    pub service_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// Leader-election shared state. Same Arcs as the actor's
    /// `DagActor::leader` field — see that field's doc and
    /// [`LeaderState`] for the full writer split (lease task vs
    /// recovery path). Non-K8s/test default is
    /// [`LeaderState::always_leader`].
    pub leader: LeaderState,
    /// This replica's lease holder identity (the pod name), recorded on
    /// `leader_generation_claims` rows. LOAD-BEARING for the
    /// same-epoch re-claim: recovery compares the claim row at the
    /// recovery-entry generation against this value to distinguish
    /// "our own previous claim, retain it" from "anything the ledger
    /// cannot prove is ours, exceed it". Empty in non-K8s/test mode
    /// (where the claim path either never runs or has no concurrent
    /// claimant to be distinguished from).
    pub holder_id: String,
    /// ADR-023 phase-13 hw-band cost table. Shared with
    /// `spot_price_poller`; the actor reads a snapshot per
    /// `solve_intent_for`. Default → seed prices.
    pub cost_table: Arc<parking_lot::RwLock<crate::sla::cost::CostTable>>,
    /// Shared edge-reload latch. Written ONLY by
    /// [`interrupt_housekeeping`](crate::sla::cost::interrupt_housekeeping)
    /// (the single edge-reload owner); read by `spot_price_poller` AND
    /// the actor's `handle_ack_spawned_intents` so neither writes the
    /// pre-reload `cost_table` and gets clobbered by `*cost.write() =
    /// CostTable::load(...)`.
    pub cost_was_leader: Arc<std::sync::atomic::AtomicBool>,
    /// Nudge `interrupt_housekeeping` to run its edge-reload promptly
    /// instead of waiting up to `POLL_INTERVAL_SECS` (600s). The
    /// actor's `handle_leader_acquired` calls `notify_one()` so the
    /// `cost_was_leader` false→true edge happens within ~0s of lease
    /// win, not ~600s — shrinking the window where `cost_was_leader`-
    /// gated writers (this actor's `observe_instance_types`) drop
    /// observations.
    pub cost_reload_notify: Arc<tokio::sync::Notify>,
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
    /// Test-only: when set, the next `recover_from_pg()` call fails its
    /// DAG-load phase up front (the independent PG-floor read in
    /// `handle_leader_acquired` is deliberately unaffected) — lets the
    /// load-failure tests exercise the floored degraded path
    /// deterministically without closing the pool.
    #[cfg(test)]
    pub fail_next_recovery_load: bool,
    /// Test-only: when set, the next independent PG-floor read in
    /// `handle_leader_acquired` fails (same `ActorError` shape a sqlx
    /// error maps to) without touching the DB — the DAG load is
    /// deliberately unaffected, so the floor-unreadable tests can
    /// exercise that fallback in isolation, without closing the pool.
    #[cfg(test)]
    pub fail_next_floor_read: bool,
}

impl Default for DagActorPlumbing {
    fn default() -> Self {
        Self {
            store_client: None,
            log_flush_tx: None,
            log_buffers: None,
            event_persist_tx: None,
            hmac_signer: None,
            service_signer: None,
            leader: LeaderState::default(),
            holder_id: String::new(),
            cost_table: Arc::default(),
            cost_was_leader: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            cost_reload_notify: Arc::default(),
            shutdown: rio_common::signal::Token::new(),
            #[cfg(test)]
            recovery_toctou_gate: None,
            #[cfg(test)]
            fail_next_recovery_load: false,
            #[cfg(test)]
            fail_next_floor_read: false,
        }
    }
}
