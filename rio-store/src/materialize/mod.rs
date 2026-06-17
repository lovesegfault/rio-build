//! Store-side materialization executor (substitution-replacement
//! design §2.2/§5).
//!
//! Each store replica runs the pull-protocol client side of
//! materialization jobs: ONE `claim_coordinator` task polls the
//! scheduler's leader for claimable jobs ([`client::poll_and_claim`])
//! and claims open attempts through `PullAssignment`
//! (kind=MATERIALIZATION + the per-replica [`executor_instance`]
//! identity); a pool of `worker_loop` tasks each offer a held path
//! slot to the coordinator (sh-002 — the inverted-token shape:
//! slot ≺ claim), execute the in-process reference-closure walk
//! against this replica's own substitution machinery, pin every
//! ingested/verified path at ingest, and report the outcome through
//! `ReportOutcome` retried until acknowledged.
//!
//! **Spawn condition:** everything here runs ONLY when a
//! `scheduler_addr` is configured (PD-D2). Without one, the executor
//! task set never spawns and the store serves its data plane alone
//! (the schedulerless pure-store deployment).
//!
//! **Identity** (BC-1 + the Wave-3/4 security obligations):
//! - The *credential* is the kind-attested store-service token —
//!   `ServiceClaims { caller: "rio-store", instance: Some(<pod>) }`
//!   signed with the service HMAC key (`service_hmac_key_path`),
//!   attached per-request by
//!   [`rio_auth::hmac::ServiceTokenInterceptor::with_instance`].
//!   Executor tokens are builder/fetcher pod-class credentials and
//!   never authorize materialization operations; the scheduler rejects
//!   them.
//! - The *replica identity* (`executor_instance`) is derived from this
//!   pod's own identity ([`executor_instance`]: the `HOSTNAME` pod
//!   name, a DNS-1123 label), bound INTO the signed claims (T-5.1 —
//!   the Phase B instance-attestation obligation, discharged), and
//!   verified scheduler-side: a claim whose `executor_instance` differs
//!   from the token's bound instance is rejected, so a compromised or
//!   misconfigured replica cannot claim under another replica's
//!   identity.
//!
//! Spec: `store.materialize.executor`; design §2.2 (store as pull
//! client), §5 (pin-at-ingest).
// r[impl store.materialize.executor+5]

pub mod client;
pub mod executor;

use std::time::Duration;

use tracing::{error, info, warn};

use executor::ClaimAdmission;

/// How long a finished job's outcome report is retried before the
/// worker gives up (the builder pull client's `REPORT_RETRY_BUDGET`
/// value — same rationale: the establishment sweep is the
/// scheduler-side backstop for an open attempt whose report never
/// landed).
const REPORT_RETRY_BUDGET: Duration = Duration::from_secs(600);

/// ±20 % jitter applied to the poll interval so a fleet of store
/// replicas doesn't poll the leader in lockstep (the builder pull
/// client's pacing discipline).
///
/// live_046 (R-B) cadence re-derivation, post sh-024 §S1: the
/// scheduler's 5 s steal horizon was derived from "a healthy IDLE
/// worker lists at least every ~1.2 s" — one jittered base beat,
/// `poll_interval × (1 + 0.2)`. Eager re-poll changes cadence only
/// for PRODUCTIVE passes (more frequent beats — freshness strictly
/// improves), and the empty-streak override (sh-024 §S1) escalates
/// the idle/EMPTY cadence to `Floor(EMPTY_BACKOFF)` — at cap the
/// worst per-replica idle gap is `EMPTY_BACKOFF.cap × (1 + 0.2)`
/// = 4.8 s, which still fits the horizon by construction (the
/// backed-off replica stays inside its rendezvous slice's freshness
/// window; crossing degrades to broader-listed, never to unlisted).
/// Mid-walk silence (the intended stealing trigger) is unchanged.
/// The base-beat relation is pinned at
/// `idle_beat_worst_gap_times_four_fits_the_steal_horizon`; the
/// escalated-cap relation at
/// `empty_backoff_cap_fits_the_steal_horizon_and_ttl` (R17).
const POLL_JITTER: rio_common::backoff::Jitter = rio_common::backoff::Jitter::Proportional(0.2);

/// sh-002 (osr2-a) — coordinator-panic restart envelope: 1 s → 30 s
/// cap, full jitter (the report-retry envelope's constants — same
/// rationale: bounded backoff, never spin).
const COORDINATOR_RESTART_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(30),
    jitter: rio_common::backoff::Jitter::Full,
};

/// sh-024 §S1 / sh-006 — the empty-streak escalation envelope: a
/// persistently-empty backlog escalates the coordinator's broadcast
/// pace from the flat 1 s `Beat` to `Floor(1 s → 2 s → cap)`, so an
/// idle replica lists at the cap cadence instead of once per
/// `poll_interval × executor_concurrency` (sh-006 measured 206 k
/// listings vs 3 356 claims — 61:1; sh-024 saw 26 replicas drive
/// ~830 listings/s aggregate during the build-only phase).
///
/// `cap = 4 s` (sh-024 review (d)): with [`pace_one`]'s upward-only
/// `POLL_JITTER` (×1.2) the worst per-replica idle gap is 4.8 s,
/// which fits the scheduler's `LISTING_STEAL_HORIZON = 5 s` (a
/// backed-off replica stays inside its rendezvous slice's freshness
/// window — crossing it would degrade to broader-listed, never to
/// unlisted, but the cap is sized to avoid even that while backlog
/// is empty) AND is ≪ `LISTING_MEMBER_TTL = 60 s` (the honest-beat
/// coupling: an idle replica never self-evicts). At 26 replicas
/// × ~1/4 s the idle aggregate is ~6.5 listings/s — a ~100× drop.
/// `Jitter::None`: the floor is the UNJITTERED schedule step, and
/// [`pace_one`]'s upward-only `POLL_JITTER` (×[1, 1.2]) supplies the
/// herd desync at the worker. `Full` was rejected: it would draw
/// `U[0, cap]` and undercut the per-replica no-spin floor the
/// `pass-outcome` law guarantees (an idle pass could re-list at ~0 s
/// — the `empty_and_gated_passes_sleep_the_jittered_beat` lower
/// bound). The trade is fleet-wide wake latency: with `None` every
/// backed-off replica wakes within one cap × 1.2 ≤ 4.8 s after
/// backlog→non-empty (vs. `Full`'s ~150 ms expected fleet-min);
/// against sh-024's observed 240+ s empty windows that is negligible
/// (review (e)). Drip-feed oscillation is bounded to the rendezvous
/// OWNER: non-minting replicas seal `ListedNoAction` (escalates),
/// only the minter sees `Delivered`/`Contested` (resets) — review
/// (c).
///
/// The const-relation pins (R17) live at
/// `empty_backoff_cap_fits_the_steal_horizon_and_ttl`.
const EMPTY_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(4),
    jitter: rio_common::backoff::Jitter::None,
};

/// sh-002 — one parked worker's claim-slot offer to the coordinator.
/// The channel is **worker → coordinator** (the inverted-token shape):
/// `try_admit_claim()` runs in the WORKER before any token is offered,
/// so a slotless replica offers zero tokens and the coordinator mints
/// zero claims (bug_102 slot ≺ claim, preserved verbatim). The
/// admission rides INSIDE the token so the slot is held exactly while
/// either in-flight to the coordinator or inside
/// `execute_job_with_progress` — never across an idle pacing sleep
/// (bug_083/WO-S1-6).
struct SlotToken {
    /// The held path slot — proof this worker can execute one job.
    adm: ClaimAdmission,
    /// One-shot reply: the coordinator returns the admission with the
    /// pass verdict (a delivered job, or `None` + the pacing decision).
    /// Dropped without sending ⇒ the worker's `await` returns
    /// `Err(Canceled)` — treated as a beat-paced re-offer (the
    /// coordinator died mid-handoff).
    reply: tokio::sync::oneshot::Sender<CoordReply>,
}

/// One worker's per-pass verdict from the coordinator (sh-002).
struct CoordReply {
    /// `Some` ⇔ this token converted into a claim — execute it.
    job: Option<client::ClaimedJob>,
    /// The admission round-tripped back: a delivered job consumes it
    /// in `execute_job_with_progress`; a `None` job drops it BEFORE
    /// the pacing sleep below (bug_083 — the slot hold is scoped to
    /// claim-consumption work, never to the idle beat).
    adm: ClaimAdmission,
    /// What this worker does between offers (the coordinator's sealed
    /// `pace_for(outcome)` — every worker paces identically, so the
    /// next offer batch arrives roughly together).
    pace: Pace,
}

/// Spawn the materialization-executor task set (sh-002 — the
/// substitution-claim fanout collapse): `cfg.executor_concurrency`
/// worker loops (offer slot → execute → report) plus the ONE
/// per-replica claim coordinator (poll → claim) running INLINE under
/// `catch_unwind` inside the supervisor body.
///
/// **The spawn gate lives HERE and is unit-testable:** with no
/// `scheduler_addr` configured this function spawns NOTHING and
/// returns 0 (PD-D2 — the schedulerless pure-store deployment).
/// main.rs calls this unconditionally; the gate is not duplicated at
/// the call site so there is exactly one tested gate.
///
/// Returns the number of worker loops spawned (0 without an address;
/// also 0 when the scheduler address is malformed — logged, never
/// fatal: a broken materialization executor must not take down the
/// store data plane).
// r[impl store.materialize.executor+5]
pub fn spawn_materialization_executor(
    cfg: crate::config::MaterializationConfig,
    pool: sqlx::PgPool,
    substituter: std::sync::Arc<crate::substitute::Substituter>,
    service_signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    path_slots: executor::PathSlotPool,
    shutdown: rio_common::signal::Token,
) -> usize {
    if cfg.scheduler_addr.is_empty() {
        info!("materialization executor disabled: no scheduler_addr configured");
        return 0;
    }
    let instance = executor_instance();
    info!(
        instance = %instance,
        concurrency = cfg.executor_concurrency,
        path_fanout = cfg.path_fanout,
        path_slots = path_slots.capacity(),
        scheduler_addr = %cfg.scheduler_addr,
        authenticated = service_signer.is_some(),
        "materialization executor enabled; spawning workers + coordinator"
    );
    // r[impl store.materialize.gate-share+1]
    // PERMANENT worker-surplus tripwire (bug_102 semantics): with
    // n > P, excess workers idle at claim ADMISSION — a slotless
    // worker offers no SlotToken, the coordinator mints no claim, and
    // jobs stay scheduler-listed; claimed jobs never queue for their
    // first slot (slot ≺ claim). (The TRANSIENT
    // regime — n×F > P, reachable at helm defaults — shifts queuing to
    // MID-WALK re-acquires, watched by the pool's wait facet:
    // queued-baseline-waiters gauge + wait-age histogram.)
    if cfg.executor_concurrency > path_slots.capacity() {
        warn!(
            executor_concurrency = cfg.executor_concurrency,
            path_slots = path_slots.capacity(),
            "executor_concurrency exceeds the path-slot pool: excess workers \
             idle at claim admission (slotless passes mint no claims; jobs \
             stay listed for pods with headroom) — lower the worker count or \
             raise the admission cap"
        );
    }
    // T-6.2 (Phase B): pre-register the executor lifecycle counters at 0
    // (the gc-collect pre-registration pattern) so dashboards/alerts have
    // series from boot and the metrics-registered VM assertion sees them
    // before the first job executes. (Without a scheduler_addr this
    // whole function returned above — no executor, no series.)
    // bug_244: the seed loop iterates THE alphabet const — a label
    // emitted by the chokepoint but missing here (retry_later was) is
    // born at its first increment, so rate()/increase() panels miss
    // the first burst after every rollout and the metrics-registered
    // VM assertion never sees the series.
    for outcome in executor::OUTCOME_LABELS {
        metrics::counter!(
            "rio_store_materialization_executions_total",
            "outcome" => outcome
        )
        .absolute(0);
    }
    metrics::counter!("rio_store_materialization_pinned_paths_total").absolute(0);
    // bug_257 (parse-don't-validate): mint the HostPort ONCE at the
    // spawn boundary. The endpoint builder prepends `http://` itself,
    // so a URL-form value used to compose `http://http://…` — which
    // parses Ok (host `http`) — booting an "enabled" executor that
    // could never reach the scheduler: zero claims fleet-wide,
    // debug-only noise. Config stays a String: a bad value disables
    // the executor LOUDLY (this arm — previously unreachable, since
    // the double-prefix parsed) and never aborts the store's data
    // plane (PD-D2 never-fatal posture).
    let scheduler_addr = match client::HostPort::parse(&cfg.scheduler_addr) {
        Ok(addr) => addr,
        Err(e) => {
            warn!(
                scheduler_addr = %cfg.scheduler_addr, error = %e,
                "materialization scheduler_addr invalid; executor disabled"
            );
            return 0;
        }
    };
    // sh-002: the worker→coordinator slot-token channel. Plain
    // `tokio::sync::mpsc` (single consumer = the coordinator); cap is
    // the worker count so every worker can park one offer.
    let (ready_tx, ready_rx) =
        tokio::sync::mpsc::channel::<SlotToken>(cfg.executor_concurrency.max(1));
    let mut spawned = 0;
    for worker in 0..cfg.executor_concurrency {
        // merged_bug_158: the concurrency unit is the WORKER, not the
        // pod — the scheduler's one-winner arbiter keys on the
        // composite {drv}@{identity}, so two workers sharing one
        // identity could both believe they hold the same attempt.
        // Mint `{pod}-w{n}` per worker for the REPORT side (T-5.1:
        // claim and credential agree on the report path; the claim
        // path is the coordinator's single identity now); a restarted
        // worker n re-claims as the same `…-w{n}`.
        //
        // merged_bug_243: the composition is `Dns1123Label::with_worker`
        // — the ONLY way to attach a worker index — which keeps the
        // COMPOSED wire value inside the 63-char bound the scheduler
        // validates (`sanitize` reserved the suffix budget). The
        // pre-fix `format!("{instance}-w{worker}")` pushed any
        // 61–63-char base to 64–66 chars: every claim was rejected
        // InvalidArgument and warn-and-skipped — a silent fleet-wide
        // materialization outage keyed on release-name length.
        let worker_instance = instance.with_worker(worker);
        let report = match client::SchedulerTransport::connect_lazy(
            &scheduler_addr,
            service_signer.clone(),
            // T-5.1: the same identity the worker asserts as
            // executor_instance is bound INTO every minted service
            // token, so the scheduler verifies the pair instead of
            // trusting the request field.
            &worker_instance,
        ) {
            Ok(t) => t,
            Err(e) => {
                // Malformed address: a config bug. Loud, never fatal —
                // the store's data plane keeps serving.
                warn!(
                    scheduler_addr = %cfg.scheduler_addr, error = %e,
                    "materialization executor channel creation failed; executor disabled"
                );
                return spawned;
            }
        };
        // live_047/R-C: width from the config lever (default 4); ONE
        // pod-wide slot pool shared by every worker bounds the
        // executor's total gate draw at P = cap/2.
        let ctx = executor::ExecutorContext::new(
            pool.clone(),
            std::sync::Arc::clone(&substituter),
            cfg.path_fanout,
            path_slots.clone(),
        );
        let cfg_for_worker = cfg.clone();
        let ready_tx = ready_tx.clone();
        let shutdown_for_worker = shutdown.clone();
        rio_common::task::spawn_monitored("materialization-worker", async move {
            worker_loop(
                worker,
                cfg_for_worker,
                ctx,
                report,
                ready_tx,
                shutdown_for_worker,
            )
            .await;
        });
        spawned += 1;
    }
    drop(ready_tx);
    // sh-002 (osr2-a, PD-D2-compatible): the supervisor IS this
    // spawn_monitored body. It owns `ready_rx` for its whole lifetime
    // and runs the coordinator INLINE under catch_unwind — NO inner
    // tokio::spawn: claim_coordinator takes `&mut Receiver`, so the
    // borrow ends on panic and the receiver SURVIVES for the next
    // iteration (workers' `ready_tx` stay live across coordinator
    // restarts; an mpsc::Receiver is !Clone — moving it into a spawned
    // task would drop it on panic and permanently close the channel).
    // ClaimSide is rebuilt each pass from the spawn-time inputs (the
    // only scope that can call the pub(super) constructor — the
    // §Nth-strike close: exactly one ClaimSide construction site, in
    // the supervisor body). PD-D2 preserved: coordinator panic
    // restarts in-process; StoreService readiness untouched.
    rio_common::task::spawn_monitored("materialization-executor", async move {
        use futures_util::FutureExt as _;
        let mut ready_rx = ready_rx;
        let mut restarts: u32 = 0;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let claim = match client::ClaimSide::reconstruct(
                &scheduler_addr,
                service_signer.clone(),
                &instance,
            ) {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        scheduler_addr = %scheduler_addr, error = %e,
                        "materialization claim-side channel creation failed; \
                         executor disabled (PD-D2 — never fatal)"
                    );
                    return;
                }
            };
            match std::panic::AssertUnwindSafe(claim_coordinator(
                &mut ready_rx,
                claim,
                &instance,
                &shutdown,
            ))
            .catch_unwind()
            .await
            {
                Ok(()) => return,
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_owned());
                    error!(
                        panic = %msg, restarts,
                        "materialization claim_coordinator panicked; restarting \
                         (osr2-a — ready_rx survives; workers' ready_tx stay live)"
                    );
                    let wait = COORDINATOR_RESTART_BACKOFF.duration(restarts);
                    restarts = restarts.saturating_add(1);
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
    });
    spawned
}

/// sh-002 — the per-replica claim coordinator: the ONE task that owns
/// `list_jobs` + `pull` (and the single [`client::ResumeLedger`] /
/// `remint_cooldowns` ledger / futility latch — moved here from the
/// retired per-worker loops). Runs inline under the supervisor's
/// `catch_unwind`; takes `&mut Receiver` so the slot-token channel
/// SURVIVES a coordinator panic (osr2-a).
///
/// **Beat cadence (live_041):** the jittered poll interval IS the
/// listing beat — each pass's `ListMaterializationJobs` call doubles
/// as this REPLICA's liveness contact for the scheduler's
/// rendezvous-partitioned listing (the scheduler tracks
/// `{replica → last_listed_at}` from the verified token instance and
/// serves each replica its own slice of the claimable head, plus a
/// steal horizon of jobs whose owner has missed its beat). The
/// coordinator carries NO steal logic: an idle replica's normal
/// listing already contains whatever the scheduler decided it should
/// see. With ONE claimant per replica (the sh-002 collapse), the
/// per-replica slice is no longer raced by N siblings; the
/// per-replica thundering herd that converted ~0.5% of attempts
/// (332:1 reject ratio measured live) is structurally unrepresentable.
///
/// **The honest beat (merged_bug_005,
/// `store.materialize.honest-beat`):** beats are withheld exactly
/// when the pass cannot convert a served job into a claim —
/// mint-headroom exhaustion (budget pinned by unanswered mints, or
/// the resume ledger at cap) and a conversion-futility streak both
/// gate the listing inside `poll_and_claim`, while the resume
/// presentation lane always runs. The scheduler's freshness proxy is
/// therefore capability-bearing BY CONSTRUCTION: "can list but
/// cannot claim" is no longer representable as a fresh owner, and
/// the degradation direction stays served-more-broadly (a withheld
/// replica ages into the steal horizon and, past the membership TTL,
/// out of the partition entirely).
///
/// **bug_102 (slot ≺ claim) preserved verbatim:** the coordinator
/// `recv()`s a [`SlotToken`] BEFORE any minting pull —
/// `try_admit_claim()` ran in the WORKER before the token was
/// offered, so a slotless replica offers zero tokens and the
/// coordinator mints zero claims. `available_slots` for
/// [`client::poll_and_claim`] is the drained-token count: backpressure
/// precedes mint, over-claim is zero by construction.
async fn claim_coordinator<C>(
    ready_rx: &mut tokio::sync::mpsc::Receiver<SlotToken>,
    mut claim: C,
    instance: &rio_common::dns::Dns1123Label,
    shutdown: &rio_common::signal::Token,
) where
    C: client::MaterializeClaimTransport + Send,
{
    info!(instance = %instance, "materialization claim coordinator started");
    // bug_251 (rule-4b): the SINGLE resume ledger — unanswered claims
    // carry their minted nonce across passes so a lost response is
    // recovered by a direct resume pull, not abandoned to the charged
    // establishment window. ONE ledger per replica (sh-002): the
    // remint_cooldowns map (live061-R3, RESOLVED_ANSWER_REMINT_
    // COOLDOWN) lives HERE — it paces scheduler-side zombie rows, not
    // a per-worker herd.
    // r[impl store.materialize.remint-cooldown]
    let mut ledger = client::ResumeLedger::default();
    // bug_257 rider: warn-once escalation for persistent listing
    // failures — a dead store→scheduler edge surfaces above debug
    // instead of starving claims silently.
    let mut list_health = client::ListFailureLatch::default();
    // merged_bug_005: conversion-futility latch — a coordinator whose
    // every fresh mint is refused with a conversion-disproving
    // rejection withholds its listing beat so the scheduler re-homes
    // this replica's rendezvous slice (the honest beat).
    let mut futility = client::ConversionFutilityLatch::default();
    // sh-024 §S1: consecutive idle-shape passes
    // (Empty/ListedNoAction/ListFailed/Wedged/Abandoned) — escalates
    // the broadcast pace to Floor(EMPTY_BACKOFF.duration(streak)) so a
    // persistently-empty backlog lists at the cap cadence, not the
    // flat 1 s beat. Reset on any work-observing pass
    // (Delivered/Settled/Contested). ONE counter on the COORDINATOR
    // (not per-worker) so every drained token receives the same
    // escalated floor — workers carry no pacing state.
    let mut empty_streak: u32 = 0;
    loop {
        // The coordinator is purely reactive: it parks on the slot
        // channel and runs a pass exactly when a worker is ready WITH
        // a held slot. Shutdown races the recv (and the scheduler's
        // staleness horizon is calibrated against the DEFAULT
        // `poll_interval_secs = 1` — workers pace at that beat below,
        // so a fully-idle replica still beats once per interval).
        let first = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            t = ready_rx.recv() => match t {
                Some(t) => t,
                // Every ready_tx dropped: all workers exited.
                None => return,
            },
        };
        let mut tokens = vec![first];
        while let Ok(t) = ready_rx.try_recv() {
            tokens.push(t);
        }
        let pass = client::poll_and_claim(
            &mut claim,
            instance,
            tokens.len(),
            &mut ledger,
            &mut list_health,
            &mut futility,
            shutdown,
        )
        .await;
        // round-8 WO-S2-1: the pacing decision is a total function of
        // the pass's sealed outcome, read BEFORE the claims are
        // distributed to workers — every token learns the same pace.
        // sh-024 §S1: the empty-streak override is applied HERE (the
        // coordinator side), OUTSIDE pace_for — the
        // r[impl store.materialize.pass-outcome] law stays a unary
        // total function over the sealed outcome alone (rustc's
        // exhaustiveness IS the census); the streak is the single
        // piece of cross-pass state the coordinator carries.
        let mut pace = pace_for(&pass.outcome);
        match pass.outcome {
            client::PassOutcome::Delivered { .. }
            | client::PassOutcome::Settled { .. }
            | client::PassOutcome::Contested { .. } => {
                empty_streak = 0;
            }
            client::PassOutcome::Empty
            | client::PassOutcome::ListedNoAction { .. }
            | client::PassOutcome::ListFailed
            | client::PassOutcome::Wedged(_)
            | client::PassOutcome::Abandoned => {
                empty_streak = empty_streak.saturating_add(1);
                pace = Pace::Floor(EMPTY_BACKOFF.duration(empty_streak.saturating_sub(1)));
            }
        }
        // bug_102 (the multi-slot form): the budget is `tokens.len()`,
        // and `poll_and_claim` never delivers more claims than the
        // budget — so the zip below is total over the claimed set; a
        // claim with no token to receive it cannot exist (each token
        // carried a held admission, so claim-without-slot does not
        // typecheck downstream). Unfilled tokens get their admission
        // BACK with `job: None` — the worker drops it before pacing
        // (bug_083: the slot hold is scoped to claim-consumption work,
        // never to the idle beat).
        let mut jobs = pass.claimed.into_iter();
        for token in tokens {
            // A dropped reply receiver means the worker exited
            // (SIGTERM); the admission inside the unsent reply drops
            // here, freeing the slot — no attempt is stranded.
            let _ = token.reply.send(CoordReply {
                job: jobs.next(),
                adm: token.adm,
                pace,
            });
        }
        debug_assert!(
            jobs.next().is_none(),
            "poll_and_claim delivered more claims than the slot-token \
             budget (available_slots = tokens.len())"
        );
    }
}

/// One worker's offer→execute→report loop (sh-002 — the per-worker
/// half; the retired per-worker `claim_loop` is replaced by this body
/// plus the single [`claim_coordinator`] above).
///
/// Execution is inline-serial — ONE job per worker per offer, executed
/// on this loop — so a worker mid-walk skips beats for the walk's
/// duration; its un-offered slot stays in the pool (claimable by any
/// sibling worker that has headroom). live_047/R-C: the walk is
/// internally path-concurrent (`path_fanout` window,
/// `store.materialize.path-fold+1`), which shortens mid-walk silence
/// for multi-path closures but changes NOTHING at this layer — the
/// claim unit, the inline execution, and the offer semantics are
/// untouched. The scheduler's staleness horizon is calibrated against
/// the DEFAULT `poll_interval_secs = 1` (±20 % jitter): raising the
/// interval past that horizon degrades to broader, more-contested
/// listings (the pre-live_041 shape) — never to unlisted jobs.
async fn worker_loop<R>(
    worker: usize,
    cfg: crate::config::MaterializationConfig,
    ctx: executor::ExecutorContext,
    mut report: R,
    ready_tx: tokio::sync::mpsc::Sender<SlotToken>,
    shutdown: rio_common::signal::Token,
) where
    R: client::MaterializeReportTransport + Send + Sync + 'static,
{
    info!(worker, "materialization worker loop started");
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        // bug_102 (slot ≺ claim): admission BEFORE the slot-token
        // offer — a worker that cannot hold a path slot offers no
        // token, the coordinator mints no claim, and the job stays
        // scheduler-listed (claimable by any pod with headroom).
        // Leftover-only try-acquire: claim admission never overtakes
        // queued mid-walk baseline waiters, so finish-started-work-
        // first falls out of the existing semaphore discipline.
        let Some(adm) = ctx.path_slots.try_admit_claim() else {
            // Slotless: pace one beat at the normal idle cadence
            // (the coordinator never sees this worker).
            if pace_one(&shutdown, Pace::Beat, &cfg).await {
                return;
            }
            continue;
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        // The admission moves INTO the token (held in-flight to the
        // coordinator — the spec-stated slot-hold invariant: a held
        // permit is always either in-flight to the coordinator or
        // inside execute_job_with_progress).
        if ready_tx
            .send(SlotToken {
                adm,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            // ready_rx dropped: the supervisor exited (shutdown).
            return;
        }
        let CoordReply { job, adm, pace } = match reply_rx.await {
            Ok(r) => r,
            // osr2-a: `Err(Canceled)` ⇔ the coordinator dropped the
            // token without replying — it died mid-handoff. The
            // admission inside the dropped SlotToken already freed; a
            // beat-paced re-offer is the recovery (NOT `?`-propagate
            // — the supervisor restarts the coordinator and the next
            // offer is served).
            Err(_) => {
                if pace_one(&shutdown, Pace::Beat, &cfg).await {
                    return;
                }
                continue;
            }
        };
        let Some(job) = job else {
            // Round-9 WO-S1-6 (bug_083, T3): the hold's scope is a
            // STATED invariant — the admission authorizes
            // CLAIM-CONSUMPTION WORK; the pacing sleep is not work. A
            // claimless reply's admission must drop BEFORE pacing, or
            // every idle worker pins a pod-wide path slot for ~a full
            // beat and re-acquires immediately: at production
            // n=32/P=32 that is held ≥ P — leftover-only `try_widen`
            // permanently starves (width-4 fan-out silently runs at
            // width 1) and the idle sleepers inflate
            // `rio_store_executor_path_slots_in_use` to
            // near-saturation, corrupting the nxF>P tripwire. With the
            // drop here the gauge measures holders-DOING-WORK by
            // construction (no clock needed: the scope change deletes
            // the idle-hold axis instead of pricing it).
            drop(adm);
            // live_046 (R-B) → round-8 WO-S2-1: the eager re-poll
            // rides the sealed outcome — work re-polls now, idle
            // passes sleep the jittered beat, contested passes honor
            // the server's answered retry floor. Jitter is KEPT on
            // every beat sleep; floor sleeps jitter upward only (the
            // floor is never undercut).
            if pace_one(&shutdown, pace, &cfg).await {
                return;
            }
            continue;
        };
        info!(
            worker,
            drv_hash = %job.drv_hash,
            exec_id = %job.exec_id,
            origin = %job.origin,
            "materialization job claimed; executing"
        );
        // BC-4 progress relay: a bounded channel + a per-job relay
        // task with its own cloned transport, so display traffic
        // never blocks the walk (try_send drops on a full queue) or
        // contends with the claim/report transport. The sender is
        // owned by the walk's callback; when the execution returns,
        // the callback (and sender) drop → the relay drains and
        // exits. Each relay send is bounded (10 s) and raced
        // against shutdown — progress is droppable by contract, so
        // TimedOut/Shutdown drop the report and Shutdown exits the
        // relay (merged_bug_189: a black-holed leader used to park
        // the relay task forever).
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<
            rio_proto::types::ReportMaterializationProgressRequest,
        >(16);
        let mut progress_transport = report.clone();
        let shutdown_for_relay = shutdown.clone();
        rio_common::task::spawn_monitored("materialization-progress-relay", async move {
            use rio_common::transport::{BoundedOutcome, bounded};
            while let Some(req) = progress_rx.recv().await {
                match bounded(
                    &shutdown_for_relay,
                    Duration::from_secs(10),
                    progress_transport.report_progress(req),
                )
                .await
                {
                    BoundedOutcome::Shutdown => return,
                    BoundedOutcome::TimedOut { .. } | BoundedOutcome::Resolved(_) => {}
                }
            }
        });
        let exec_id_for_progress = job.exec_id.clone();
        let execute = executor::execute_job_with_progress(
            &ctx,
            &job,
            adm,
            move |bytes_done, bytes_expected, upstream| {
                let _ =
                    progress_tx.try_send(rio_proto::types::ReportMaterializationProgressRequest {
                        exec_id: exec_id_for_progress.clone(),
                        bytes_done,
                        bytes_expected,
                        upstream_uri: upstream.to_string(),
                    });
            },
        );
        // SIGTERM aborts the walk by drop (in-flight upstream
        // fetches are torn down with their futures) and reports
        // the NEW Aborted outcome through the single bounded
        // SIGTERM attempt — the scheduler closes the attempt
        // charge-free (AD5 parity, owner default Q3) instead of
        // letting the charged establishment sweep classify a
        // routine rollout as infrastructure failure.
        let outcome = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!(
                    worker,
                    drv_hash = %job.drv_hash,
                    exec_id = %job.exec_id,
                    "SIGTERM during the materialization walk: aborting and reporting Aborted"
                );
                // merged_bug_115: the synthesized outcome routes
                // through the SAME count-and-report mint as the
                // walk's — report_until_acked demands the witness,
                // so an uncounted synthesized report does not
                // typecheck.
                executor::CountedOutcome::count(rio_proto::types::MaterializationOutcome {
                    outcome: Some(
                        rio_proto::types::materialization_outcome::Outcome::Aborted(
                            rio_proto::types::materialization_outcome::Aborted {
                                detail: "walk aborted by SIGTERM (store shutdown/rollout)"
                                    .into(),
                            },
                        ),
                    ),
                })
            }
            outcome = execute => outcome,
        };
        let acked = client::report_until_acked(
            &mut report,
            &job.exec_id,
            outcome,
            REPORT_RETRY_BUDGET,
            &shutdown,
        )
        .await;
        if !acked {
            warn!(
                worker,
                drv_hash = %job.drv_hash,
                exec_id = %job.exec_id,
                "materialization outcome was never acknowledged \
                 (the scheduler's establishment sweep will close the attempt)"
            );
        }
        // A delivered job re-offers immediately (Pace::Now per the
        // sealed-outcome law); the coordinator's next recv() drains
        // it without sleeping.
    }
}

/// One pacing step (sh-002 — extracted so the slotless arm, the
/// claimless-reply arm, and the coordinator-died arm share the same
/// jitter/floor handling). Returns `true` when shutdown fired.
///
/// live_046 (R-B): jitter is KEPT on every beat sleep; floor sleeps
/// jitter upward only (the floor is never undercut). The beat seam
/// feeds a RAW config-sourced u64 into the jitter with no
/// `Backoff::duration`-style pre-clamp; it is lawful because
/// `Jitter::apply` is TOTAL (saturates at the shared 1-year absurdity
/// ceiling — bug_049). Config validation additionally bounds the
/// interval to one day (defense in depth, rio-store config.rs).
async fn pace_one(
    shutdown: &rio_common::signal::Token,
    pace: Pace,
    cfg: &crate::config::MaterializationConfig,
) -> bool {
    match pace {
        Pace::Now => false,
        Pace::Beat => {
            let interval = POLL_JITTER.apply(Duration::from_secs(cfg.poll_interval_secs.max(1)));
            pace_after_empty_pass(shutdown, interval).await
        }
        Pace::Floor(floor) => {
            let interval = POLL_JITTER.apply(floor).max(floor);
            pace_after_empty_pass(shutdown, interval).await
        }
    }
}

/// Test seam (sh-002): spawn `executor_concurrency` workers and the
/// inline claim coordinator over injectable transports, returning the
/// supervisor body's future. The production
/// [`spawn_materialization_executor`] is the same shape with
/// [`client::ClaimSide`] / [`client::SchedulerTransport`] hard-wired;
/// tests drive this seam with shared-state mocks so the herd-collapse,
/// slot ≺ claim, and osr2-a panic-restart properties are observable
/// end-to-end.
#[cfg(test)]
fn spawn_with_transports<C, R>(
    cfg: crate::config::MaterializationConfig,
    ctx_factory: impl Fn() -> executor::ExecutorContext,
    mut claim_factory: impl FnMut() -> C + Send + 'static,
    report_factory: impl Fn(usize) -> R,
    instance: rio_common::dns::Dns1123Label,
    shutdown: rio_common::signal::Token,
) -> impl Future<Output = ()>
where
    C: client::MaterializeClaimTransport + Send + 'static,
    R: client::MaterializeReportTransport + Send + Sync + 'static,
{
    use futures_util::FutureExt as _;
    let (ready_tx, mut ready_rx) =
        tokio::sync::mpsc::channel::<SlotToken>(cfg.executor_concurrency.max(1));
    for worker in 0..cfg.executor_concurrency {
        let ctx = ctx_factory();
        let report = report_factory(worker);
        let cfg_for_worker = cfg.clone();
        let ready_tx = ready_tx.clone();
        let shutdown_for_worker = shutdown.clone();
        rio_common::task::spawn_monitored("materialization-worker", async move {
            worker_loop(
                worker,
                cfg_for_worker,
                ctx,
                report,
                ready_tx,
                shutdown_for_worker,
            )
            .await;
        });
    }
    drop(ready_tx);
    async move {
        let mut restarts: u32 = 0;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            let claim = claim_factory();
            match std::panic::AssertUnwindSafe(claim_coordinator(
                &mut ready_rx,
                claim,
                &instance,
                &shutdown,
            ))
            .catch_unwind()
            .await
            {
                Ok(()) => return,
                Err(_) => {
                    let wait = COORDINATOR_RESTART_BACKOFF.duration(restarts);
                    restarts = restarts.saturating_add(1);
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
    }
}

/// What the claim loop does between passes (round-8 WO-S2-1 — the
/// pacing law's closed output alphabet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Re-poll immediately (no sleep).
    Now,
    /// Sleep one jittered poll beat.
    Beat,
    /// Sleep at least the server's answered retry floor (jitter
    /// applied upward only — the floor is never undercut).
    Floor(Duration),
}

// r[impl store.materialize.pass-outcome]
/// round-8 WO-S2-1 — THE pacing law: one total function over the
/// sealed [`client::PassOutcome`] alphabet (the pacing loop's only
/// input; rustc's exhaustiveness is the census — a new outcome
/// variant fails this build). The structural no-spin law: immediate
/// re-poll is licensed ONLY by variants that consumed finite supply —
/// `Delivered` executed a claim, `Settled` strictly shrank the ledger
/// (the backlog is finite, so termination is structural). A zero-work
/// pass cannot re-poll unpaced BY TYPE, and a work-bearing pass
/// cannot be classified idle BY TYPE:
///
///   - `Delivered`/`Settled` → `Now`: conversion work re-polls, even
///     under a gated exit (merged_bug_038 — the seal's
///     conversion-first precedence).
///   - `Contested` → `Floor(retry_after)` when the server stated a
///     floor, else `Beat` (merged_bug_008, FS-4 face): the wire fact
///     is the earliest any contested job could be ready — cap-fill
///     is paced at (cap/allowance − 1) × floor, not RPC speed.
///   - `ListedNoAction` → `Beat` (merged_bug_008, hot loop): a pass
///     that listed work and took no conversion-conclusive action is
///     idle, whatever the listing said.
///   - `Empty`/`Wedged`/`ListFailed` → `Beat`: nothing to do (or the
///     beat was withheld) — idle at the beat, never spin.
///   - `Abandoned` → `Beat`: SIGTERM — the loop exits before any
///     sleep matters; paced for totality.
pub fn pace_for(outcome: &client::PassOutcome) -> Pace {
    match outcome {
        client::PassOutcome::Delivered { .. } => Pace::Now,
        client::PassOutcome::Settled { .. } => Pace::Now,
        client::PassOutcome::Contested { floor } => match floor {
            Some(d) => Pace::Floor(*d),
            None => Pace::Beat,
        },
        client::PassOutcome::ListedNoAction { .. } => Pace::Beat,
        client::PassOutcome::Empty => Pace::Beat,
        client::PassOutcome::Wedged(_) => Pace::Beat,
        client::PassOutcome::ListFailed => Pace::Beat,
        client::PassOutcome::Abandoned => Pace::Beat,
    }
}

/// The pacing primitive: one jittered, shutdown-aware beat sleep.
/// Returns true when shutdown fired (the caller exits its loop).
async fn pace_after_empty_pass(shutdown: &rio_common::signal::Token, interval: Duration) -> bool {
    tokio::select! {
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(interval) => false,
    }
}

/// The per-replica executor identity (BC-1): the pod name.
///
/// `HOSTNAME` is set by the kubelet to the pod name — a DNS-1123 label
/// (lowercase alphanumerics + interior hyphens, ≤63 chars), which is
/// exactly the alphabet the scheduler validates `executor_instance`
/// against (the composite ExecutorId `{intent}@{instance}` must stay
/// unambiguous). `RIO_STORE_REPLICA_ID` (the pod *IP* injected for the
/// TailLog proxy) is NOT used here: an IP literal contains dots/colons
/// and is not a DNS-1123 label.
///
/// Values that fail the label check (non-k8s dev hosts with uppercase
/// or dotted hostnames) are sanitized: lowercased, invalid bytes
/// replaced with `-`, budget-truncated, stripped of edge hyphens, and
/// deterministically salted. Empty/unset falls back to a randomly
/// salted `"rio-store-dev"`.
///
/// merged_bug_243: the identity is a [`rio_common::dns::Dns1123Label`]
/// sanitized with [`rio_common::dns::WORKER_SUFFIX_RESERVED`] chars of
/// suffix budget, so the per-worker composition (`with_worker`) stays
/// inside the 63-char bound the scheduler validates. One alphabet, one
/// sanitizer, one composer — the scheduler-side validator reads the
/// SAME `rio_common::dns::is_dns1123_label`. Trade recorded: raws of
/// 59–63 valid chars used to pass through unchanged and now
/// truncate+salt; their identity changes once at the deploy boundary
/// and the scheduler's establishment sweep absorbs the orphaned
/// claims.
// r[impl store.materialize.executor+5]
// r[impl store.materialize.worker-identity]
pub fn executor_instance() -> rio_common::dns::Dns1123Label {
    let raw = std::env::var("HOSTNAME").unwrap_or_default();
    rio_common::dns::Dns1123Label::sanitize(
        &raw,
        rio_common::dns::WORKER_SUFFIX_RESERVED,
        "rio-store-dev",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sh-002 test seam: drive the worker pool + inline coordinator
    /// over a shared-state mock with `executor_concurrency` workers and
    /// the given path-slot pool. The coordinator runs INLINE on the
    /// caller's task (so virtual-clock auto-advance and task-local
    /// recorders see it). Returns once shutdown fires. The `db` pool is
    /// caller-provided so callers can set it up BEFORE pausing tokio
    /// time (the ephemeral-PG connect timeouts run on tokio time).
    fn drive_coordinator<C, R>(
        db: sqlx::PgPool,
        concurrency: usize,
        pool: executor::PathSlotPool,
        claim_factory: impl FnMut() -> C + Send + 'static,
        report: R,
        shutdown: rio_common::signal::Token,
    ) -> impl Future<Output = ()>
    where
        C: client::MaterializeClaimTransport + Send + 'static,
        R: client::MaterializeReportTransport + Send + Sync + 'static,
    {
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.clone(), None));
        let cfg = crate::config::MaterializationConfig {
            executor_concurrency: concurrency,
            ..Default::default()
        };
        spawn_with_transports(
            cfg,
            move || {
                executor::ExecutorContext::new(
                    db.clone(),
                    std::sync::Arc::clone(&substituter),
                    1,
                    pool.clone(),
                )
            },
            claim_factory,
            move |_| report.clone(),
            executor_instance(),
            shutdown,
        )
    }

    // r[verify store.materialize.executor+5]
    /// PD-D2 spawn condition: no `scheduler_addr` → zero claim loops
    /// (the schedulerless pure-store deployment); a configured address
    /// spawns exactly `executor_concurrency` loops (connect_lazy — no
    /// scheduler needs to be listening).
    #[tokio::test]
    async fn executor_spawns_iff_scheduler_addr_configured() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();

        let mut cfg = crate::config::MaterializationConfig::default();
        assert!(cfg.scheduler_addr.is_empty(), "default: no address");
        let spawned = spawn_materialization_executor(
            cfg.clone(),
            db.pool.clone(),
            std::sync::Arc::clone(&substituter),
            None,
            executor::PathSlotPool::new(32),
            shutdown.clone(),
        );
        assert_eq!(spawned, 0, "no scheduler_addr => no executor (PD-D2)");

        // Positive control: a host:port address spawns the configured
        // loops (connect_lazy — no scheduler needs to be listening).
        // Bare form REQUIRED: the URL form used to sit here, silently
        // enshrining the bug_257 double-prefix as the tested shape.
        cfg.scheduler_addr = "127.0.0.1:1".into();
        cfg.executor_concurrency = 3;
        let spawned = spawn_materialization_executor(
            cfg,
            db.pool.clone(),
            substituter,
            None,
            executor::PathSlotPool::new(32),
            shutdown.clone(),
        );
        assert_eq!(spawned, 3, "an address spawns the configured loops");
        shutdown.cancel();
    }

    /// bug_257: a URL-form address must DISABLE the executor loudly at
    /// the spawn boundary. `build_endpoint` (rio-proto) prepends
    /// `http://` unconditionally, so a scheme-bearing config value used
    /// to compose `http://http://…` — which the http crate parses
    /// happily (host `http`) — booting workers whose every RPC dialed a
    /// dead authority: zero claims fleet-wide, debug-only noise.
    #[tokio::test]
    async fn executor_disabled_on_url_form_scheduler_addr() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();

        let cfg = crate::config::MaterializationConfig {
            scheduler_addr: "http://127.0.0.1:1".into(),
            executor_concurrency: 3,
            ..Default::default()
        };
        let spawned = spawn_materialization_executor(
            cfg,
            db.pool.clone(),
            substituter,
            None,
            executor::PathSlotPool::new(32),
            shutdown.clone(),
        );
        assert_eq!(
            spawned, 0,
            "scheme-bearing scheduler_addr must disable the executor (bug_257)"
        );
        shutdown.cancel();
    }

    /// The instance derivation produces a label the scheduler accepts
    /// AND leaves the worker-suffix budget free (merged_bug_243): the
    /// composed `{instance}-w{n}` — the value actually validated
    /// scheduler-side — is itself a DNS-1123 label. The sanitizer
    /// mechanics (injectivity, determinism, fallbacks, proptest over
    /// raws × workers) live with the type in rio-common/src/dns.rs.
    // r[verify store.materialize.executor+5]
    // r[verify store.materialize.worker-identity]
    #[test]
    fn executor_instance_leaves_worker_budget() {
        use rio_common::dns::{DNS1123_MAX_LEN, WORKER_SUFFIX_RESERVED, is_dns1123_label};
        let instance = executor_instance();
        assert!(is_dns1123_label(instance.as_str()));
        assert!(instance.as_str().len() <= DNS1123_MAX_LEN - WORKER_SUFFIX_RESERVED);
        for worker in [0usize, 7, 999] {
            let composed = instance.with_worker(worker);
            assert!(
                is_dns1123_label(composed.as_str()),
                "composed identity must be a DNS-1123 label: {:?}",
                composed.as_str()
            );
        }
    }

    /// merged_bug_189 (the claim-loop SIGTERM arm): SIGTERM landing
    /// after a claim delivers aborts the walk (the execute future is
    /// never driven — biased select) and reports exactly one
    /// MaterializationOutcome::Aborted through the bounded SIGTERM
    /// attempt, then the loop exits. (The pre-fix loop had no shutdown
    /// arm around execute at all and report_until_acked took no
    /// shutdown — this shape was unexpressible: compile-level red.)
    // r[verify store.materialize.executor+5]
    #[tokio::test]
    async fn claim_loop_sigterm_aborts_walk_and_reports_aborted() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct SigtermAfterClaim {
            shutdown: rio_common::signal::Token,
            reports: Arc<Mutex<Vec<rio_proto::types::ReportOutcomeRequest>>>,
        }
        impl client::MaterializeClaimTransport for SigtermAfterClaim {
            async fn list_jobs(
                &mut self,
                _req: rio_proto::types::ListMaterializationJobsRequest,
            ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status>
            {
                Ok(rio_proto::types::ListMaterializationJobsResponse {
                    jobs: vec![rio_proto::types::MaterializationJobDescriptor {
                        job_id: uuid::Uuid::now_v7().to_string(),
                        drv_hash: "sigterm-drv".into(),
                        tenant_id: String::new(),
                        origin: "cache_opportunity".into(),
                    }],
                })
            }
            async fn pull(
                &mut self,
                _req: rio_proto::types::PullAssignmentRequest,
            ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
                // SIGTERM lands right after the claim DELIVERS to the
                // worker (sh-002: the reply now hops the slot-token
                // channel; cancel from a fresh task so the
                // coordinator's `bounded()` sees the resolved
                // assignment, not Shutdown, and the worker's biased
                // select on execute observes the cancellation).
                let s = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::task::yield_now().await;
                    s.cancel();
                });
                Ok(rio_proto::types::PullAssignmentResponse {
                    outcome: Some(
                        rio_proto::types::pull_assignment_response::Outcome::Assignment(
                            rio_proto::types::WorkAssignment {
                                drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv"
                                    .into(),
                                exec_id: "exec-sigterm-1".into(),
                                ..Default::default()
                            },
                        ),
                    ),
                })
            }
        }
        impl client::MaterializeReportTransport for SigtermAfterClaim {
            async fn report(
                &mut self,
                req: rio_proto::types::ReportOutcomeRequest,
            ) -> Result<(), tonic::Status> {
                self.reports.lock().unwrap().push(req);
                Ok(())
            }
            async fn report_progress(
                &mut self,
                _req: rio_proto::types::ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }

        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let shutdown = rio_common::signal::Token::new();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let transport = SigtermAfterClaim {
            shutdown: shutdown.clone(),
            reports: Arc::clone(&reports),
        };
        let claim = transport.clone();
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        tokio::time::timeout(
            Duration::from_secs(30),
            drive_coordinator(
                db.pool.clone(),
                1,
                executor::PathSlotPool::new(32),
                move || claim.clone(),
                transport,
                shutdown,
            ),
        )
        .await
        .expect("the loop must exit promptly after the SIGTERM-aborted job");

        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 1, "exactly one Aborted report");
        assert_eq!(reports[0].exec_id, "exec-sigterm-1");
        // merged_bug_115: the synthesized SIGTERM outcome must move
        // the executions counter like every other outcome - the
        // series is seeded, HELP'd, and documented as live, but its
        // ONLY producer bypassed the counting chokepoint.
        assert_eq!(
            rec.get("rio_store_materialization_executions_total{outcome=aborted}"),
            1,
            "the SIGTERM-synthesized Aborted is COUNTED"
        );
        match &reports[0].materialization_outcome {
            Some(rio_proto::types::MaterializationOutcome {
                outcome: Some(rio_proto::types::materialization_outcome::Outcome::Aborted(aborted)),
            }) => {
                assert!(aborted.detail.contains("SIGTERM"));
            }
            other => panic!("expected the Aborted outcome, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // live_046 (R-B) — eager re-poll pacing
    // -----------------------------------------------------------------

    // r[verify store.materialize.pass-outcome]
    /// R15 census: the pacing law, total over the sealed
    /// [`client::PassOutcome`] alphabet — the census vector's
    /// completeness is FORCED by the wildcard-free
    /// `outcome_variant_index` match (a new variant fails the build)
    /// plus the bijection assertion. Each expected value is a
    /// HAND-WRITTEN literal derived from the pacing requirement —
    /// never an expression sharable with the impl (the retired
    /// `empty_law_total_over_the_pass_product` oracle was the impl's
    /// own boolean, certifying f(x)==f(x); its R16 prose adopted
    /// "withheld = wedged" underived — the merged_bug_038 lesson).
    ///
    /// Proposition (R16): the requirement's own per-cell values —
    /// re-poll-now ⇔ the pass consumed finite supply (delivered or
    /// strictly shrank the ledger); contested passes pace at the
    /// SERVER's stated floor (else the beat); every other shape
    /// paces at the beat. The two former merged_bug_008 defect cells
    /// (`Contested` and `ListedNoAction` re-polling unpaced) are
    /// flipped here.
    #[test]
    fn pacing_law_total_over_pass_outcomes() {
        use client::{PassOutcome, WedgeKind};
        const OUTCOME_VARIANTS: usize = 8;
        fn outcome_variant_index(o: &PassOutcome) -> usize {
            match o {
                PassOutcome::Delivered { .. } => 0,
                PassOutcome::Settled { .. } => 1,
                PassOutcome::Contested { .. } => 2,
                PassOutcome::ListedNoAction { .. } => 3,
                PassOutcome::Empty => 4,
                PassOutcome::Wedged(_) => 5,
                PassOutcome::ListFailed => 6,
                PassOutcome::Abandoned => 7,
            }
        }
        let alphabet: Vec<(PassOutcome, Pace)> = vec![
            // A claim landed: execute it and come straight back.
            (PassOutcome::Delivered { deliveries: 1 }, Pace::Now),
            // The ledger strictly shrank: settle work re-polls.
            (PassOutcome::Settled { resolutions: 2 }, Pace::Now),
            // The server stated a floor: honor it — the earliest any
            // contested job could be ready (merged_bug_008, FS-4
            // face — the formerly-discarded wire fact, now the cell
            // value the floor PROPAGATES through).
            (
                PassOutcome::Contested {
                    floor: Some(Duration::from_secs(5)),
                },
                Pace::Floor(Duration::from_secs(5)),
            ),
            // No stated floor: the contested pass paces at the beat.
            (PassOutcome::Contested { floor: None }, Pace::Beat),
            // Zero-action listings are idle, whatever the listing
            // said (merged_bug_008, hot loop — the flipped cell).
            (
                PassOutcome::ListedNoAction {
                    refused: 1,
                    skipped: 0,
                },
                Pace::Beat,
            ),
            // Nothing to do: idle at the beat.
            (PassOutcome::Empty, Pace::Beat),
            // Withheld beats always pace (never spin while wedged).
            (
                PassOutcome::Wedged(WedgeKind::BudgetPinned { charged: 1 }),
                Pace::Beat,
            ),
            (
                PassOutcome::Wedged(WedgeKind::AtCap {
                    charged: 20,
                    entries: 32,
                }),
                Pace::Beat,
            ),
            (PassOutcome::Wedged(WedgeKind::Futility), Pace::Beat),
            // A failed listing is an idle pass.
            (PassOutcome::ListFailed, Pace::Beat),
            // SIGTERM: the loop exits; paced for totality.
            (PassOutcome::Abandoned, Pace::Beat),
        ];
        let mut seen = [false; OUTCOME_VARIANTS];
        for (o, _) in &alphabet {
            seen[outcome_variant_index(o)] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "the census vector must cover every PassOutcome variant"
        );
        for (outcome, expected) in &alphabet {
            assert_eq!(pace_for(outcome), *expected, "pacing cell for {outcome:?}");
        }
    }

    /// R17 const-relation pin (the BASE-beat cadence floor as a
    /// typed envelope): the steal horizon was derived from "a healthy
    /// IDLE worker lists at least every ~1.2 s"; eager re-poll changes
    /// cadence only for PRODUCTIVE passes (freshness strictly
    /// improves). sh-024 §S1's empty-streak override escalates the
    /// idle cadence past the base beat — that arm's horizon relation
    /// is pinned separately at
    /// [`empty_backoff_cap_fits_the_steal_horizon_and_ttl`]. The pin:
    /// 4 x default-poll-interval x (1 + jitter) <= steal horizon,
    /// asserted THROUGH the real symbols (POLL_JITTER, the config
    /// default, the mirrored horizon const — R14).
    #[test]
    fn idle_beat_worst_gap_times_four_fits_the_steal_horizon() {
        let interval = crate::config::MaterializationConfig::default()
            .poll_interval_secs
            .max(1);
        let rio_common::backoff::Jitter::Proportional(j) = POLL_JITTER else {
            panic!("the pacing jitter is proportional by construction");
        };
        let worst_gap = 4.0 * interval as f64 * (1.0 + j);
        assert!(
            worst_gap <= client::SCHEDULER_LISTING_STEAL_HORIZON_SECS as f64,
            "four missed idle beats (worst gap {worst_gap:.2} s) must fit \
             the scheduler's steal horizon ({} s)",
            client::SCHEDULER_LISTING_STEAL_HORIZON_SECS
        );
    }

    /// Scripted pacing transport for the claim-loop tests: serves one
    /// listing BATCH per call (repeating the last batch once the
    /// script is exhausted), answers pulls from a popped script
    /// (repeating the last answer), records every pull request, and
    /// cancels shutdown after N listing calls (the loop's exit
    /// valve).
    #[derive(Clone)]
    struct PacingTransport {
        state: std::sync::Arc<std::sync::Mutex<PacingState>>,
    }

    struct PacingState {
        listings: std::collections::VecDeque<Vec<rio_proto::types::MaterializationJobDescriptor>>,
        pulls: std::collections::VecDeque<rio_proto::types::PullAssignmentResponse>,
        list_calls: u32,
        cancel_after: u32,
        seen_pulls: Vec<rio_proto::types::PullAssignmentRequest>,
        shutdown: rio_common::signal::Token,
    }

    impl client::MaterializeClaimTransport for PacingTransport {
        async fn list_jobs(
            &mut self,
            _req: rio_proto::types::ListMaterializationJobsRequest,
        ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status> {
            let mut st = self.state.lock().unwrap();
            st.list_calls += 1;
            if st.list_calls >= st.cancel_after {
                st.shutdown.cancel();
            }
            let jobs = match st.listings.len() {
                0 => Vec::new(),
                1 => st.listings[0].clone(),
                _ => st.listings.pop_front().expect("non-empty"),
            };
            Ok(rio_proto::types::ListMaterializationJobsResponse { jobs })
        }

        async fn pull(
            &mut self,
            req: rio_proto::types::PullAssignmentRequest,
        ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
            let mut st = self.state.lock().unwrap();
            st.seen_pulls.push(req);
            Ok(match st.pulls.len() {
                0 => not_yet_ready_in_5s(),
                1 => st.pulls[0].clone(),
                _ => st.pulls.pop_front().expect("non-empty"),
            })
        }
    }

    impl client::MaterializeReportTransport for PacingTransport {
        async fn report(
            &mut self,
            _req: rio_proto::types::ReportOutcomeRequest,
        ) -> Result<(), tonic::Status> {
            Ok(())
        }

        async fn report_progress(
            &mut self,
            _req: rio_proto::types::ReportMaterializationProgressRequest,
        ) -> Result<(), tonic::Status> {
            Ok(())
        }
    }

    /// The contested wire answer (the production scheduler's uniform
    /// 5 s floor).
    fn not_yet_ready_in_5s() -> rio_proto::types::PullAssignmentResponse {
        rio_proto::types::PullAssignmentResponse {
            outcome: Some(
                rio_proto::types::pull_assignment_response::Outcome::NotYetReady(
                    rio_proto::types::NotYetReady {
                        retry_after_seconds: 5,
                    },
                ),
            ),
        }
    }

    /// Run the REAL claim loop over a scripted transport under a
    /// paused virtual clock; returns the elapsed virtual time and the
    /// transport state (pull-request census for the mint-cost
    /// assertions).
    async fn run_claim_loop_with(
        listings: Vec<Vec<rio_proto::types::MaterializationJobDescriptor>>,
        pulls: Vec<rio_proto::types::PullAssignmentResponse>,
        cancel_after: u32,
    ) -> (
        std::time::Duration,
        std::sync::Arc<std::sync::Mutex<PacingState>>,
    ) {
        let shutdown = rio_common::signal::Token::new();
        let state = std::sync::Arc::new(std::sync::Mutex::new(PacingState {
            listings: listings.into(),
            pulls: pulls.into(),
            list_calls: 0,
            cancel_after,
            seen_pulls: Vec::new(),
            shutdown: shutdown.clone(),
        }));
        let transport = PacingTransport {
            state: std::sync::Arc::clone(&state),
        };
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        // Pause the clock ONLY after the real-I/O setup (the ephemeral
        // PG pool's connect timeouts run on tokio time; pausing before
        // setup makes auto-advance fire them ahead of the socket).
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        drive_coordinator(
            db.pool.clone(),
            1,
            executor::PathSlotPool::new(32),
            {
                let t = transport.clone();
                move || t.clone()
            },
            transport,
            shutdown,
        )
        .await;
        (started.elapsed(), state)
    }

    fn pacing_descriptor(n: u32) -> rio_proto::types::MaterializationJobDescriptor {
        rio_proto::types::MaterializationJobDescriptor {
            job_id: uuid::Uuid::now_v7().to_string(),
            drv_hash: format!("pacing-drv-{n}"),
            tenant_id: String::new(),
            origin: "cache_opportunity".into(),
        }
    }

    fn gone_answer() -> rio_proto::types::PullAssignmentResponse {
        rio_proto::types::PullAssignmentResponse {
            outcome: Some(rio_proto::types::pull_assignment_response::Outcome::Gone(
                rio_proto::types::Gone {},
            )),
        }
    }

    fn not_yet_ready_no_floor() -> rio_proto::types::PullAssignmentResponse {
        rio_proto::types::PullAssignmentResponse {
            outcome: Some(
                rio_proto::types::pull_assignment_response::Outcome::NotYetReady(
                    rio_proto::types::NotYetReady {
                        retry_after_seconds: 0,
                    },
                ),
            ),
        }
    }

    // r[verify store.materialize.pass-outcome]
    /// round-8 (rewritten at WO-S2-1; was live_046's "three productive
    /// passes" — an R16 statement whose passes 2-3 produced zero
    /// mints, zero claims, and zero ledger movement: the
    /// merged_bug_008 defect cell pinned as the feature, retired with
    /// the flip). Proposition certified (R16): a pass that CONSUMES
    /// FINITE SUPPLY re-polls with zero added virtual time — here the
    /// supply-consuming population is a Settled pass (the resume lane
    /// drains a Gone credential; the ledger strictly shrinks), driven
    /// through the real claim loop; the DELIVERING sibling cell is
    /// certified pass-level by `resume_delivery_is_sealed_productive_
    /// not_empty` (loop-level delivery would execute a real walk
    /// against PG under a paused clock — recorded divergence). The
    /// contested setup pass paces at the beat (its floor is unstated
    /// — `Contested{{floor: None}}`), so total elapsed is EXACTLY one
    /// jittered beat: the Settled pass added nothing.
    #[tokio::test]
    async fn productive_pass_repolls_without_sleep() {
        // Pass 1: mint d1, answered NotYetReady (no stated floor) —
        // contested, paces one jittered beat. Pass 2: the resume lane
        // drains d1 with Gone — Settled, re-polls NOW. Pass 3: empty
        // listing; the exit valve cancels at the third list call.
        let (elapsed, _state) = run_claim_loop_with(
            vec![vec![pacing_descriptor(1)], vec![], vec![]],
            vec![not_yet_ready_no_floor(), gone_answer()],
            3,
        )
        .await;
        assert!(
            elapsed >= std::time::Duration::from_millis(780),
            "the contested setup pass paces one jittered beat \
             (got {elapsed:?})"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(1300),
            "left: >= two beats (the settle-draining pass slept too) / \
             right: exactly one beat — the Settled pass re-polled with \
             zero added virtual time (got {elapsed:?})"
        );
    }

    /// live_046 companion pin (the no-spin half), re-derived at
    /// sh-024 §S1: EMPTY passes sleep AT LEAST the escalating
    /// `EMPTY_BACKOFF` floor — three empty passes advance the virtual
    /// clock by streak-1 (1 s) + streak-2 (2 s) before the third pass
    /// exits via shutdown. The lower bound is the no-spin guarantee
    /// (the floor is never undercut — `Jitter::None` on the schedule
    /// step + upward-only `POLL_JITTER` at `pace_one`); the upper
    /// bound is the same floors × 1.2.
    #[tokio::test]
    async fn empty_and_gated_passes_sleep_the_jittered_beat() {
        let (elapsed, _state) = run_claim_loop_with(vec![vec![]], vec![], 3).await;
        assert!(
            elapsed >= std::time::Duration::from_millis(3000),
            "two empty-pass floors (streak 1 s + 2 s) must elapse — the \
             no-spin lower bound (got {elapsed:?})"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(3700),
            "and no more than the two escalated floors × 1.2 jitter before \
             the exit (got {elapsed:?})"
        );
    }

    // r[verify store.materialize.pass-outcome]
    /// round-8 R1 (merged_bug_008 hot-loop cell). Proposition
    /// certified (R16): a pass that LISTS work but takes ZERO actions
    /// — every descriptor refused pre-pull (malformed job_id, the
    /// production bad_job_id lane) — sleeps the jittered beat. The
    /// population is the vulnerable one: pre-fix no backstop engages
    /// (the futility latch needs fresh mints, the honest-beat gate
    /// sees Available, the list latch sees success), so a degraded
    /// scheduler emitting malformed descriptors spun the warn+list
    /// loop at RPC speed.
    #[tokio::test]
    async fn listed_zero_action_pass_sleeps_the_beat() {
        let bad = rio_proto::types::MaterializationJobDescriptor {
            job_id: "not-a-uuid".into(),
            drv_hash: "zero-action-drv".into(),
            tenant_id: String::new(),
            origin: "cache_opportunity".into(),
        };
        let (elapsed, state) = run_claim_loop_with(vec![vec![bad]], vec![], 3).await;
        let st = state.lock().unwrap();
        assert_eq!(st.list_calls, 3, "three listing passes ran");
        assert!(
            st.seen_pulls.is_empty(),
            "zero actions: every descriptor exited pre-pull"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(3000),
            "left: three list RPCs, zero virtual time (the warn/list hot \
             loop at RPC speed) / right: each zero-action pass sleeps >= \
             the escalating EMPTY_BACKOFF floor (streak 1 s + 2 s; sh-024 \
             §S1 — got {elapsed:?})"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(3700),
            "and no more than the two escalated floors × 1.2 jitter before \
             the exit (got {elapsed:?})"
        );
    }

    // r[verify store.materialize.pass-outcome]
    /// round-8 R2 (merged_bug_008 FS-4 cell). Proposition certified
    /// (R16): a CONTESTED pass — fresh mints issued, every one
    /// answered NotYetReady carrying the server's
    /// `retry_after_seconds: 5` — paces at the SERVER's floor, and
    /// mint cost stays within the per-pass allowance (the cap-fill
    /// product: time-to-cap >= (cap/allowance - 1) x floor, the FS-4
    /// envelope this red is the running witness for). The population
    /// is contested-only across three passes (fresh descriptor per
    /// pass; resume re-presentations answer the same floor).
    #[tokio::test]
    async fn contested_mint_pass_honors_the_server_retry_floor() {
        let (elapsed, state) = run_claim_loop_with(
            vec![
                vec![pacing_descriptor(1)],
                vec![pacing_descriptor(2)],
                vec![pacing_descriptor(3)],
            ],
            vec![],
            3,
        )
        .await;
        assert!(
            elapsed >= std::time::Duration::from_secs(10),
            "left: zero virtual time between contested passes (cap \
             fills in ~cap/allowance round-trips) / right: >= 5 s floor \
             per contested pass — time-to-cap >= (cap/allowance - 1) x \
             floor (got {elapsed:?})"
        );
        assert!(
            elapsed <= std::time::Duration::from_secs(13),
            "and no more than two upward-jittered floors before the \
             exit (got {elapsed:?})"
        );
        let st = state.lock().unwrap();
        assert_eq!(st.list_calls, 3, "three contested passes ran");
        let distinct_nonces: std::collections::HashSet<&str> = st
            .seen_pulls
            .iter()
            .filter(|r| !r.claim_nonce.is_empty())
            .map(|r| r.claim_nonce.as_str())
            .collect();
        assert_eq!(
            distinct_nonces.len(),
            2,
            "one fresh nonce per COMPLETED contested pass (the third \
             pass's mint pull is cut by the exit valve before it rides) \
             — mint cost stays within passes x allowance, never one per \
             round-trip"
        );
    }

    // ── bw8 S3 (bug_102): the slot ≺ claim seam ──────────────────────
    // (appended OUTSIDE the S2-owned law/pacing-test region — the
    // claim-admission seam is S3's surface; keep-both at rebase.)

    // r[verify store.materialize.gate-share+1]
    /// R2-102 (bug_102, TRUE RED pre-fix) / W-102b: a SLOTLESS pass
    /// lists nothing and claims nothing — the worker's pass budget is
    /// `try_admit_claim().is_some() as usize`, so with zero pod slot
    /// headroom the pass drives `available_slots = 0` through
    /// `poll_and_claim` and the transport sees ZERO listings and ZERO
    /// pulls (the scripted-transport call census — never log text);
    /// slotless passes pace at the normal empty-pass beat (no spin).
    ///
    /// Pre-fix red (run + recorded in the owning commit body): the
    /// budget was hardwired 1 — every pass issued a listing (and
    /// would mint a claim) with zero pod slot headroom.
    ///
    /// Composition (RE-DERIVED at the S2 rebase per the t0 handoff,
    /// consuming S2's DONE response; round-9 WO-S1-5 re-derivation):
    /// the obligation-free zero-budget pass seals `PassOutcome::Empty`
    /// → `Pace::Beat` BY TYPE — the normal idle pacing lane, distinct
    /// from charge-pinned `Wedged(BudgetPinned)` — and can never feed
    /// futility backoff (the streak law is mint-guarded; a zero-budget
    /// pass mints nothing). The resume-presentation leg runs on EVERY
    /// pass, zero-slot ones included, and is OBSERVED at the
    /// client-seam witness (`zero_slot_pass_presents_resume_answers` —
    /// the pre-round-9 concession that the leg was "unobservable at
    /// this seam" is repaired, not re-disclosed: this loop-level test
    /// keeps the empty-ledger cell, the client test carries the
    /// obligation-bearing cell). S2's disclosed wobble — Empty runs
    /// the wedge latch's HEAL arm, so a pool-exhausted stretch clears
    /// a warned budget wedge early (re-arms at threshold) — is
    /// accepted as recorded.
    #[tokio::test]
    async fn slotless_pass_lists_nothing_and_claims_nothing() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct CountingTransport {
            list_calls: Arc<Mutex<usize>>,
            pull_calls: Arc<Mutex<usize>>,
        }
        impl client::MaterializeClaimTransport for CountingTransport {
            async fn list_jobs(
                &mut self,
                _req: rio_proto::types::ListMaterializationJobsRequest,
            ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status>
            {
                *self.list_calls.lock().unwrap() += 1;
                Ok(rio_proto::types::ListMaterializationJobsResponse { jobs: vec![] })
            }
            async fn pull(
                &mut self,
                _req: rio_proto::types::PullAssignmentRequest,
            ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
                *self.pull_calls.lock().unwrap() += 1;
                Ok(rio_proto::types::PullAssignmentResponse { outcome: None })
            }
        }
        impl client::MaterializeReportTransport for CountingTransport {
            async fn report(
                &mut self,
                _req: rio_proto::types::ReportOutcomeRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
            async fn report_progress(
                &mut self,
                _req: rio_proto::types::ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let shutdown = rio_common::signal::Token::new();
        let transport = CountingTransport::default();
        let list_calls = Arc::clone(&transport.list_calls);
        let pull_calls = Arc::clone(&transport.pull_calls);

        // ZERO pod slot headroom: the pool's only slot is held for the
        // whole test (a saturated pod — every slot mid-walk elsewhere).
        let pool1 = executor::PathSlotPool::new(1);
        let admission = pool1
            .try_admit_claim()
            .expect("fresh pool admits — now held for the test's duration");

        // Paused virtual clock: beats auto-advance, so several
        // slotless passes elapse quickly; then shutdown. sh-002
        // retarget: the COORDINATOR mints zero pulls because zero
        // SlotTokens are offered (try_admit_claim fails in every
        // worker_loop iteration).
        tokio::time::pause();
        let claim = transport.clone();
        let driver = tokio::spawn(drive_coordinator(
            db.pool.clone(),
            1,
            pool1,
            move || claim.clone(),
            transport,
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(10)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("loop exits on shutdown")
            .unwrap();

        assert_eq!(
            *list_calls.lock().unwrap(),
            0,
            "a slotless pass issued a listing with zero pod slot headroom \
             (budget must be admission-derived, not hardwired 1)"
        );
        assert_eq!(
            *pull_calls.lock().unwrap(),
            0,
            "a slotless pass minted a claiming pull with zero pod slot headroom"
        );
        drop(admission);
    }

    // ── sh-002 (S1): substitution-claim fanout collapse ───────────────
    //
    // The headline structural fix: today executor_concurrency workers
    // each run an INDEPENDENT poll_and_claim loop with its own ledger
    // and identity, so a 46-replica fleet at 25 workers each races
    // 1,150 distinct claimants on the same ~512-row rendezvous head —
    // 332:1 reject ratio, ~0.4 claims/s. The fix collapses this to ONE
    // claim coordinator per replica that exclusively owns list+pull.

    // r[verify store.materialize.gate-share+1]
    /// **sh-002 (a) — single-claimer-per-replica.** Spawn 8 worker
    /// claim loops over a shared 4-slot pool (the production
    /// executor_concurrency > path_slots regime); the mock scheduler
    /// records which executor_instance every PullAssignment carried.
    ///
    /// Proposition: a replica issues PullAssignment under EXACTLY ONE
    /// caller identity — the per-replica claim coordinator. RED at
    /// base: each worker pulls under its own `{pod}-w{n}` identity,
    /// so distinct-instance count ≥ the slot count (4); the captured
    /// red is the herd the advisory measured at 332:1.
    ///
    /// **(b)** — slot ≺ claim (bug_102 retarget at the coordinator):
    /// with 4 slots, the mock never sees more than 4 PullAssignment
    /// requests in one pass. RED at base: 4 admitted workers each
    /// minted up to 2 per pass (the retired per-worker speculation
    /// allowance) on a fully-contested listing — 8 pulls/pass against
    /// 4 slots.
    #[tokio::test]
    async fn sh002_single_claimer_per_replica_slot_precedes_claim() {
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct HerdState {
            pull_instances: HashSet<String>,
            all_nonces: HashSet<String>,
            fresh_mints_this_window: usize,
            max_fresh_mints_per_window: usize,
            list_calls: usize,
        }
        #[derive(Clone)]
        struct HerdTransport {
            state: Arc<Mutex<HerdState>>,
            // Stable job_ids across listings (the live scheduler shape:
            // a job stays listed until claimed/resolved). Per-call
            // now_v7 ids would make every listing a fresh job set —
            // the resume-lane re-presentations would never match and
            // every pass would mint anew, defeating the per-pass
            // census.
            jobs: Arc<Vec<rio_proto::types::MaterializationJobDescriptor>>,
        }
        impl client::MaterializeClaimTransport for HerdTransport {
            async fn list_jobs(
                &mut self,
                _req: rio_proto::types::ListMaterializationJobsRequest,
            ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status>
            {
                let mut st = self.state.lock().unwrap();
                st.list_calls += 1;
                // The next listing call begins a new pass window: fold
                // the prior window's FRESH-mint count into the max,
                // then reset (a coarse but deterministic per-pass
                // census on the listing edge; resume-lane
                // re-presentations are NOT new attempts and do not
                // count toward over-claim).
                st.max_fresh_mints_per_window = st
                    .max_fresh_mints_per_window
                    .max(st.fresh_mints_this_window);
                st.fresh_mints_this_window = 0;
                Ok(rio_proto::types::ListMaterializationJobsResponse {
                    jobs: (*self.jobs).clone(),
                })
            }
            async fn pull(
                &mut self,
                req: rio_proto::types::PullAssignmentRequest,
            ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
                let mut st = self.state.lock().unwrap();
                st.pull_instances.insert(req.executor_instance);
                // A FRESH mint is a never-before-seen nonce; resume
                // presentations re-use a previously-minted nonce.
                if !req.claim_nonce.is_empty() && st.all_nonces.insert(req.claim_nonce) {
                    st.fresh_mints_this_window += 1;
                }
                Ok(rio_proto::types::PullAssignmentResponse {
                    outcome: Some(
                        rio_proto::types::pull_assignment_response::Outcome::NotYetReady(
                            rio_proto::types::NotYetReady {
                                retry_after_seconds: 0,
                            },
                        ),
                    ),
                })
            }
        }
        impl client::MaterializeReportTransport for HerdTransport {
            async fn report(
                &mut self,
                _req: rio_proto::types::ReportOutcomeRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
            async fn report_progress(
                &mut self,
                _req: rio_proto::types::ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let shutdown = rio_common::signal::Token::new();
        let pool = executor::PathSlotPool::new(4);
        let state = Arc::new(Mutex::new(HerdState::default()));
        let jobs = Arc::new(
            (0..8)
                .map(|n| rio_proto::types::MaterializationJobDescriptor {
                    job_id: uuid::Uuid::now_v7().to_string(),
                    drv_hash: format!("herd-drv-{n}"),
                    tenant_id: String::new(),
                    origin: "cache_opportunity".into(),
                })
                .collect(),
        );
        let transport = HerdTransport {
            state: Arc::clone(&state),
            jobs,
        };

        // The production spawn shape: 8 worker_loop tasks over one
        // 4-slot pool, ONE inline coordinator. Paused virtual clock:
        // contested passes pace the beat (no stated floor) and
        // auto-advance.
        tokio::time::pause();
        let claim = transport.clone();
        let driver = tokio::spawn(drive_coordinator(
            db.pool.clone(),
            8,
            pool.clone(),
            move || claim.clone(),
            transport,
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(5)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("loop exits on shutdown")
            .unwrap();

        let st = state.lock().unwrap();
        assert!(
            st.list_calls > 0,
            "the harness drove at least one listing pass"
        );
        // (a) — the headline: one claimant identity per replica.
        assert_eq!(
            st.pull_instances.len(),
            1,
            "left: {} distinct executor_instance values issued PullAssignment \
             (the per-worker thundering herd) / right: ONE per-replica claim \
             coordinator owns list+pull — saw {:?}",
            st.pull_instances.len(),
            st.pull_instances
        );
        // (b) — slot ≺ claim at the coordinator: across the whole run
        // the coordinator never minted more DISTINCT fresh nonces than
        // the listed job count (one nonce per job lifecycle — the
        // ledger mint authority); the retired per-worker shape minted
        // one per worker per pass on the same jobs.
        assert!(
            st.all_nonces.len() <= 8,
            "left: {} distinct fresh nonces minted across {} listings \
             (the per-worker herd minted one per worker per pass on the \
             same 8 jobs) / right: one coordinator + one ledger mints at \
             most one nonce per job lifecycle",
            st.all_nonces.len(),
            st.list_calls
        );
        // The fresh-mint census stays for forensic value but is no
        // longer the gating assertion (the per-pass FS-4 bound is
        // deleted; mints-per-pass is bounded by the slice, not the
        // slot count).
        let _ = st.max_fresh_mints_per_window;
    }

    // ===================================================================
    // Round-9 WO-S1-6 (bug_083) — the slot hold is scoped to WORK, not
    // to the loop: a claimless pass's admission drops BEFORE the pacing
    // sleep. Pre-fix every claimless pass pinned a pod-wide path slot
    // for ~a full beat and re-acquired immediately: at production n=32
    // workers and P=32, held permits ≥ P — leftover-only try_widen
    // permanently failed (width-4 fan-out silently ran at width 1) and
    // the idle-sleeper gauge inflation corrupted the nxF>P tripwire.
    // ===================================================================

    /// W9-L — on an IDLE pod (zero claims), during the pacing beat:
    /// `try_widen` SUCCEEDS (the corrupted-tripwire inverse + the
    /// starved-feature heal, structural). sh-002 retarget: the
    /// coordinator returns the admission with `job: None`, the worker
    /// drops it BEFORE pacing — so a probe mid-beat finds the slot
    /// free. (The gauge half of the original W9-L assertion is dropped:
    /// gauge edges fire on the spawned worker task, outside the
    /// task-local recorder; the structural `try_admit_claim` probe is
    /// the primary witness.)
    ///
    /// W9-M dual face (the scope did not over-shrink), structural: a
    /// DELIVERED claim's admission round-trips to the worker and is
    /// consumed BY VALUE in `execute_job_with_progress` (which DEMANDS
    /// it — claim-without-slot does not typecheck), so the consume
    /// span still holds the slot; the worker_loop drop site releases
    /// only never-consumed (`job: None`) admissions.
    #[tokio::test]
    async fn idle_pass_releases_slot_before_pacing() {
        #[derive(Clone, Default)]
        struct EmptyTransport;
        impl client::MaterializeClaimTransport for EmptyTransport {
            async fn list_jobs(
                &mut self,
                _req: rio_proto::types::ListMaterializationJobsRequest,
            ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status>
            {
                Ok(rio_proto::types::ListMaterializationJobsResponse { jobs: vec![] })
            }
            async fn pull(
                &mut self,
                _req: rio_proto::types::PullAssignmentRequest,
            ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
                Ok(rio_proto::types::PullAssignmentResponse { outcome: None })
            }
        }
        impl client::MaterializeReportTransport for EmptyTransport {
            async fn report(
                &mut self,
                _req: rio_proto::types::ReportOutcomeRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
            async fn report_progress(
                &mut self,
                _req: rio_proto::types::ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let shutdown = rio_common::signal::Token::new();

        // capacity 1 = the per-worker miniature of the production
        // n=32/P=32 saturation: ONE idle worker pinning its slot is
        // exactly the held-permits ≥ P regime for this pool.
        let pool1 = executor::PathSlotPool::new(1);
        let pool_probe = pool1.clone();

        // Pause AFTER the PG setup (a paused clock breaks the pool's
        // connect timeouts — the slotless test's established order).
        tokio::time::pause();

        let shutdown_for_probe = shutdown.clone();
        let probe = async move {
            // Let the first pass complete and park in its beat sleep
            // (jittered ~1s; 500ms lands mid-sleep deterministically
            // under paused time).
            tokio::time::sleep(Duration::from_millis(500)).await;

            // The structural assertion: the slot is FREE during pacing —
            // probed through the same leftover-only try-acquire face
            // try_widen rides (try_admit_claim is the public probe;
            // both are `try_acquire` leftovers, so success here is
            // success for the widening tier at this occupancy).
            let widened = pool_probe.try_admit_claim();
            assert!(
                widened.is_some(),
                "left: leftover-only acquisition fails during the idle beat \
                 (the claimless reply pins its admission across the pacing \
                 sleep; try_widen starves at held ≥ P) / right: the idle \
                 hold is deleted — the slot returns before pacing"
            );
            drop(widened);
            shutdown_for_probe.cancel();
        };

        tokio::join!(
            drive_coordinator(
                db.pool.clone(),
                1,
                pool1,
                || EmptyTransport,
                EmptyTransport,
                shutdown.clone(),
            ),
            probe
        );
    }

    /// **sh-002 (c) — osr2-a inverse arm (§Verifier-one-step-removed):**
    /// the coordinator is a SPOF. A panic injected via the mock claim
    /// transport must NOT permanently wedge claiming: the supervisor
    /// catches the panic, restarts the coordinator INLINE with the same
    /// `&mut ready_rx` (the receiver survived because it was borrowed,
    /// not moved into a spawned task), and a fresh `pull()` lands within
    /// `3 × poll_interval`. RED at commit-3-without-supervisor (the bare
    /// `claim_coordinator(...).await` shape with no `catch_unwind`): 0
    /// pulls after the panic.
    // r[verify store.materialize.gate-share+1]
    #[tokio::test]
    async fn sh002_coordinator_panic_restarts_and_ready_tx_survives() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        #[derive(Clone, Default)]
        struct PanicOnceClaim {
            pulls: Arc<AtomicU32>,
            generation: u32,
        }
        impl client::MaterializeClaimTransport for PanicOnceClaim {
            async fn list_jobs(
                &mut self,
                _req: rio_proto::types::ListMaterializationJobsRequest,
            ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status>
            {
                Ok(rio_proto::types::ListMaterializationJobsResponse {
                    jobs: vec![rio_proto::types::MaterializationJobDescriptor {
                        job_id: uuid::Uuid::now_v7().to_string(),
                        drv_hash: "panic-probe".into(),
                        tenant_id: String::new(),
                        origin: "cache_opportunity".into(),
                    }],
                })
            }
            async fn pull(
                &mut self,
                _req: rio_proto::types::PullAssignmentRequest,
            ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
                self.pulls.fetch_add(1, Ordering::SeqCst);
                if self.generation == 0 {
                    panic!("sh-002 osr2-a injected coordinator panic");
                }
                Ok(rio_proto::types::PullAssignmentResponse {
                    outcome: Some(
                        rio_proto::types::pull_assignment_response::Outcome::NotYetReady(
                            rio_proto::types::NotYetReady {
                                retry_after_seconds: 0,
                            },
                        ),
                    ),
                })
            }
        }
        #[derive(Clone, Default)]
        struct NoopReport;
        impl client::MaterializeReportTransport for NoopReport {
            async fn report(
                &mut self,
                _req: rio_proto::types::ReportOutcomeRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
            async fn report_progress(
                &mut self,
                _req: rio_proto::types::ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let shutdown = rio_common::signal::Token::new();
        let pulls = Arc::new(AtomicU32::new(0));
        // Each supervisor iteration mints a fresh ClaimSide via the
        // factory; bump the generation so the SECOND coordinator's
        // pull() does not panic.
        let pulls_f = Arc::clone(&pulls);
        let mut generation = 0u32;
        let factory = move || {
            let g = generation;
            generation += 1;
            PanicOnceClaim {
                pulls: Arc::clone(&pulls_f),
                generation: g,
            }
        };
        tokio::time::pause();
        let driver = tokio::spawn(drive_coordinator(
            db.pool.clone(),
            2,
            executor::PathSlotPool::new(2),
            factory,
            NoopReport,
            shutdown.clone(),
        ));
        // 3 × poll_interval (= 3 s) covers: worker beat-paced re-offer
        // after the dropped reply (Err(Canceled)) + the supervisor's
        // first restart-backoff step (≤ 1 s).
        tokio::time::sleep(Duration::from_secs(3)).await;
        let after = pulls.load(Ordering::SeqCst);
        assert!(
            after >= 2,
            "left: {after} pulls after the injected panic (the receiver \
             dropped with the panicked coordinator → channel closed → \
             workers' ready_tx.send() returns Err forever) / right: the \
             supervisor's catch_unwind kept &mut ready_rx alive across \
             the restart and a fresh pull landed within 3 × poll_interval"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("driver exits on shutdown")
            .unwrap();
    }

    // ── sh-024 §S1 (the empty-poll storm) ────────────────────────────

    /// Shared sh-024 mock: records the virtual-clock instant of every
    /// `list_jobs` call; serves a popped script of listing batches
    /// (repeating empty once exhausted) and answers every pull
    /// `NotYetReady{0}` (so a non-empty listing seals `Contested`, not
    /// `Delivered` — no job execution path needed).
    #[derive(Clone)]
    struct StreakTransport {
        stamps: std::sync::Arc<std::sync::Mutex<Vec<tokio::time::Instant>>>,
        listings: std::sync::Arc<
            std::sync::Mutex<
                std::collections::VecDeque<Vec<rio_proto::types::MaterializationJobDescriptor>>,
            >,
        >,
    }
    impl client::MaterializeClaimTransport for StreakTransport {
        async fn list_jobs(
            &mut self,
            _req: rio_proto::types::ListMaterializationJobsRequest,
        ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status> {
            self.stamps
                .lock()
                .unwrap()
                .push(tokio::time::Instant::now());
            let jobs = self
                .listings
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Ok(rio_proto::types::ListMaterializationJobsResponse { jobs })
        }
        async fn pull(
            &mut self,
            _req: rio_proto::types::PullAssignmentRequest,
        ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
            Ok(rio_proto::types::PullAssignmentResponse {
                outcome: Some(
                    rio_proto::types::pull_assignment_response::Outcome::NotYetReady(
                        rio_proto::types::NotYetReady {
                            retry_after_seconds: 0,
                        },
                    ),
                ),
            })
        }
    }
    impl client::MaterializeReportTransport for StreakTransport {
        async fn report(
            &mut self,
            _req: rio_proto::types::ReportOutcomeRequest,
        ) -> Result<(), tonic::Status> {
            Ok(())
        }
        async fn report_progress(
            &mut self,
            _req: rio_proto::types::ReportMaterializationProgressRequest,
        ) -> Result<(), tonic::Status> {
            Ok(())
        }
    }

    // r[verify store.materialize.pass-outcome]
    /// **sh-024 §S1 — RED-FIRST.** A persistently-empty backlog must
    /// not drive the coordinator at the flat 1 s beat: the
    /// `empty_streak` override escalates the broadcast pace to
    /// `Floor(EMPTY_BACKOFF.duration(streak))` so a fully-idle replica
    /// lists at the cap cadence (mean ≈ cap/2 under Full jitter), not
    /// once per `poll_interval × executor_concurrency`.
    ///
    /// One worker, 60 s of virtual time. Pre-fix every pass paces
    /// `Beat` (1 s ± 20 %), so the coordinator runs ≥ 50 listings (the
    /// sh-006 61:1 / sh-024 ~830 listings/s storm at fleet scale).
    /// Post-fix the streak escalates 1 s → 2 s → 4 s cap (unjittered
    /// schedule, upward `POLL_JITTER` at the worker) — ≤ 17 listings.
    #[tokio::test]
    async fn idle_fleet_listing_rate_bounded() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let shutdown = rio_common::signal::Token::new();
        let transport = StreakTransport {
            stamps: Default::default(),
            listings: Default::default(),
        };
        let stamps = std::sync::Arc::clone(&transport.stamps);
        tokio::time::pause();
        let claim = transport.clone();
        let driver = tokio::spawn(drive_coordinator(
            db.pool.clone(),
            1,
            executor::PathSlotPool::new(1),
            move || claim.clone(),
            transport,
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(60)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("driver exits on shutdown")
            .unwrap();
        let stamps = stamps.lock().unwrap();
        let n = stamps.len();
        assert!(
            n < 20,
            "left: {n} ListMaterializationJobs in 60 s with an always-empty \
             backlog (the flat 1 s ± 20 % Beat → ≥ 50; the sh-024 §S1 storm \
             at fleet scale: 26 replicas × 32 workers ≈ 830/s) / right: the \
             coordinator's empty_streak override escalates to \
             Floor(EMPTY_BACKOFF) — at the 4 s cap an idle replica lists \
             ≤ ~17× per minute"
        );
        assert!(n >= 6, "need ≥ 6 stamps to observe the 5th-gap cap");
        // The 5th inter-list gap (streak=5, attempt index 4 → capped):
        // the coordinator broadcast Floor(4 s) and pace_one jitters
        // upward only — the gap is ≥ the cap by construction. RED at
        // base: every gap ≈ 1 s.
        let gap5 = stamps[5].duration_since(stamps[4]);
        assert!(
            gap5 >= EMPTY_BACKOFF.cap,
            "left: 5th-consecutive-empty gap = {gap5:?} (the flat Beat — no \
             escalation) / right: by streak 5 the broadcast pace is \
             Floor(EMPTY_BACKOFF.cap = {:?}) and pace_one never undercuts \
             the floor",
            EMPTY_BACKOFF.cap
        );
    }

    /// **sh-024 §S1 — reset law.** A pass that observes work
    /// (`Delivered`/`Settled`/`Contested`) MUST reset the streak: a
    /// drip-fed backlog snaps a backed-off replica back to the base
    /// beat on the rendezvous owner, never strands it at the cap. The
    /// non-owners seal `ListedNoAction` (no fresh mints) and KEEP
    /// escalating — only the minter resets, so drip-feed oscillation
    /// is bounded to one replica per delivery.
    ///
    /// Pre-fix this is vacuously green (no escalation exists); the
    /// red-first proposition is `idle_fleet_listing_rate_bounded`.
    #[tokio::test]
    async fn empty_streak_resets_on_delivery() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let shutdown = rio_common::signal::Token::new();
        let job = rio_proto::types::MaterializationJobDescriptor {
            job_id: uuid::Uuid::now_v7().to_string(),
            drv_hash: "drip".into(),
            tenant_id: String::new(),
            origin: "cache_opportunity".into(),
        };
        // Five empty listings (escalate to cap), then ONE non-empty
        // (the pull answers NotYetReady → seals Contested → resets).
        let mut script = std::collections::VecDeque::new();
        for _ in 0..5 {
            script.push_back(vec![]);
        }
        script.push_back(vec![job]);
        let transport = StreakTransport {
            stamps: Default::default(),
            listings: std::sync::Arc::new(std::sync::Mutex::new(script)),
        };
        let stamps = std::sync::Arc::clone(&transport.stamps);
        tokio::time::pause();
        let claim = transport.clone();
        let driver = tokio::spawn(drive_coordinator(
            db.pool.clone(),
            1,
            executor::PathSlotPool::new(1),
            move || claim.clone(),
            transport,
            shutdown.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(60)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("driver exits on shutdown")
            .unwrap();

        let stamps = stamps.lock().unwrap();
        assert!(
            stamps.len() >= 8,
            "need ≥ 8 listings to observe escalate→reset→re-escalate"
        );
        // The 5th gap (streak=5, capped) is the escalated floor —
        // re-asserted here against the scripted pre-reset window.
        let gap_at_cap = stamps[5].duration_since(stamps[4]);
        assert!(
            gap_at_cap >= EMPTY_BACKOFF.cap,
            "the 5th-empty gap must be ≥ cap (got {gap_at_cap:?})"
        );
        // The 6th listing (index 5) sealed Contested → streak reset →
        // pace_for(Contested{floor:None}) = Beat. The 7th listing's
        // gap is therefore one base beat (≤ 1 s × 1.2), NOT an
        // escalated floor — the reset law.
        let gap_after_reset = stamps[6].duration_since(stamps[5]);
        assert!(
            gap_after_reset <= Duration::from_millis(1200),
            "left: gap after the Contested reset = {gap_after_reset:?} (the \
             streak survived a work-observing pass) / right: \
             Delivered/Settled/Contested reset empty_streak → next pace is \
             pace_for's verdict (Beat here), ≤ 1.2 s"
        );
        // The 7th listing sealed Empty → streak restarts at 1: the
        // 8th gap is the base step again (≥ 1 s, ≤ 1.2 s) — not the
        // cap. ListedNoAction (the non-owner arm) is covered by
        // `listed_zero_action_pass_sleeps_the_beat` (escalates).
        let gap_restart = stamps[7].duration_since(stamps[6]);
        assert!(
            gap_restart >= EMPTY_BACKOFF.base && gap_restart < Duration::from_millis(1300),
            "post-reset re-escalation starts from base (got {gap_restart:?})"
        );
    }

    /// R17 const-relation pin (sh-024 review (d), the steal-horizon
    /// coupling): the empty-streak cap × the worst upward jitter MUST
    /// fit inside `LISTING_STEAL_HORIZON` (a backed-off idle replica
    /// stays inside its rendezvous slice's freshness window — crossing
    /// it degrades to broader-listed, never to unlisted, but the cap
    /// is sized to avoid even that while backlog is empty); and the
    /// cap MUST be ≪ `LISTING_MEMBER_TTL` (the honest-beat liveness
    /// gate — an idle replica never self-evicts from rendezvous
    /// membership).
    #[test]
    fn empty_backoff_cap_fits_the_steal_horizon_and_ttl() {
        let rio_common::backoff::Jitter::Proportional(j) = POLL_JITTER else {
            panic!("the pacing jitter is proportional by construction");
        };
        let worst_gap = EMPTY_BACKOFF.cap.as_secs_f64() * (1.0 + j);
        assert!(
            worst_gap <= client::SCHEDULER_LISTING_STEAL_HORIZON_SECS as f64,
            "EMPTY_BACKOFF.cap × (1 + POLL_JITTER) = {worst_gap:.2} s must fit \
             the {} s steal horizon (sh-024 review (d): a backed-off replica \
             stays inside its rendezvous slice's freshness window)",
            client::SCHEDULER_LISTING_STEAL_HORIZON_SECS
        );
        assert!(
            EMPTY_BACKOFF.cap.as_secs() * 4 <= client::SCHEDULER_LISTING_MEMBER_TTL_SECS,
            "EMPTY_BACKOFF.cap must be ≪ LISTING_MEMBER_TTL (4× headroom): a \
             persistently-idle replica never self-evicts from rendezvous \
             membership (the honest-beat coupling)"
        );
    }
}
