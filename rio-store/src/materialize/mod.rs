//! Store-side materialization executor (substitution-replacement
//! design §2.2/§5).
//!
//! Each store replica runs the pull-protocol client side of
//! materialization jobs: poll the scheduler's leader for claimable
//! jobs ([`client::poll_and_claim`]), claim one open attempt per job
//! through `PullAssignment` (kind=MATERIALIZATION + the per-replica
//! [`executor_instance`] identity), execute the in-process
//! reference-closure walk against this replica's own substitution
//! machinery, pin every ingested/verified path at ingest, and report
//! the outcome through `ReportOutcome` retried until acknowledged.
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

use tracing::{info, warn};

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
/// live_046 (R-B) cadence re-derivation: the scheduler's 5 s steal
/// horizon was derived from "a healthy IDLE worker lists at least
/// every ~1.2 s" — the worst idle gap is one jittered beat,
/// `poll_interval × (1 + 0.2)`, and four missed beats
/// (4 × 1 × 1.2 = 4.8 s) fit inside the horizon. Eager re-poll
/// changes cadence ONLY for productive passes (more frequent beats —
/// freshness strictly improves) and leaves the idle/EMPTY cadence —
/// the horizon's binding worst case — byte-identical; mid-walk
/// silence (the intended stealing trigger) is unchanged. The
/// `idle_beat_worst_gap_times_four_fits_the_steal_horizon` pin
/// asserts the relation through this const, the config default, and
/// the mirrored horizon symbol (R17).
const POLL_JITTER: rio_common::backoff::Jitter = rio_common::backoff::Jitter::Proportional(0.2);

/// Spawn the materialization-executor task set:
/// `cfg.executor_concurrency` claim loops, each running
/// poll → claim → execute → report against the scheduler's
/// ExecutorService until shutdown.
///
/// **The spawn gate lives HERE and is unit-testable:** with no
/// `scheduler_addr` configured this function spawns NOTHING and
/// returns 0 (PD-D2 — the schedulerless pure-store deployment).
/// main.rs calls this unconditionally; the gate is not duplicated at
/// the call site so there is exactly one tested gate.
///
/// Returns the number of claim loops spawned (0 without an address;
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
        "materialization executor enabled; spawning claim loops"
    );
    // r[impl store.materialize.gate-share]
    // PERMANENT baseline-queuing tripwire: with n > P even width-1
    // walks contend for slots — some claimed jobs always queue at the
    // baseline acquire. (The TRANSIENT regime — n×F > P, reachable at
    // helm defaults — is watched by the pool's wait facet instead:
    // queued-baseline-waiters gauge + wait-age histogram.)
    if cfg.executor_concurrency > path_slots.capacity() {
        warn!(
            executor_concurrency = cfg.executor_concurrency,
            path_slots = path_slots.capacity(),
            "executor_concurrency exceeds the path-slot pool: claimed jobs \
             will queue at the baseline slot even at width 1 (lower the \
             worker count or raise the admission cap)"
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
    let mut spawned = 0;
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
            return spawned;
        }
    };
    for worker in 0..cfg.executor_concurrency {
        // merged_bug_158: the concurrency unit is the WORKER, not the
        // pod — the scheduler's one-winner arbiter keys on the
        // composite {drv}@{identity}, so two workers sharing one
        // identity could both believe they hold the same attempt.
        // Mint `{pod}-w{n}` per worker for BOTH the claim field and
        // the token binding (T-5.1: claim and credential agree); a
        // restarted worker n re-claims as the same `…-w{n}`.
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
        let transport = match client::SchedulerTransport::connect_lazy(
            &scheduler_addr,
            service_signer.clone(),
            // T-5.1: the same identity the claim loop asserts as
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
        let ctx = executor::ExecutorContext {
            pool: pool.clone(),
            substituter: std::sync::Arc::clone(&substituter),
            // live_047/R-C: width from the config lever (default 4);
            // ONE pod-wide slot pool shared by every worker bounds
            // the executor's total gate draw at P = cap/2.
            path_fanout: cfg.path_fanout,
            path_slots: path_slots.clone(),
        };
        let cfg_for_worker = cfg.clone();
        let instance_for_worker = worker_instance.clone();
        let shutdown_for_worker = shutdown.clone();
        rio_common::task::spawn_monitored("materialization-executor", async move {
            claim_loop(
                worker,
                cfg_for_worker,
                ctx,
                transport,
                instance_for_worker,
                shutdown_for_worker,
            )
            .await;
        });
        spawned += 1;
    }
    spawned
}

/// One worker's claim loop: poll → claim (one job at a time) →
/// execute → report, with jittered pacing; shutdown-aware.
///
/// **Beat cadence (live_041):** the jittered poll interval IS the
/// listing beat — each pass's `ListMaterializationJobs` call doubles
/// as this worker's liveness contact for the scheduler's
/// rendezvous-partitioned listing (the scheduler tracks
/// `{worker → last_listed_at}` from the verified token instance and
/// serves each worker its own slice of the claimable head, plus a
/// steal horizon of jobs whose owner has missed its beat). The
/// worker carries NO steal logic: an idle worker's normal listing
/// already contains whatever the scheduler decided it should see.
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
/// worker ages into the steal horizon and, past the membership TTL,
/// out of the partition entirely).
///
/// Two cadence consequences, both intended:
///   - execution is inline-serial below — ONE job per worker per
///     pass, executed on this loop — so a worker mid-walk skips
///     beats for the walk's duration; its unclaimed slice ages past
///     the scheduler's steal horizon and is offered to idle workers
///     (work stealing exactly when this worker cannot claim anyway).
///     live_047/R-C: the walk is internally path-concurrent
///     (`path_fanout` window, `store.materialize.path-fold`), which
///     shortens mid-walk silence for multi-path closures but changes
///     NOTHING at this layer — the claim unit, the inline execution,
///     and the beat semantics are untouched;
///   - the scheduler's staleness horizon is calibrated against the
///     DEFAULT `poll_interval_secs = 1` (±20 % jitter): raising the
///     interval past that horizon degrades to broader, more-contested
///     listings (the pre-live_041 shape) — never to unlisted jobs.
async fn claim_loop<T>(
    worker: usize,
    cfg: crate::config::MaterializationConfig,
    ctx: executor::ExecutorContext,
    mut transport: T,
    instance: rio_common::dns::Dns1123Label,
    shutdown: rio_common::signal::Token,
) where
    T: client::MaterializeTransport + Clone + Send + Sync + 'static,
{
    info!(worker, instance = %instance, "materialization claim loop started");
    // bug_251 (rule-4b): the per-worker resume ledger — unanswered
    // claims carry their minted nonce across passes so a lost
    // response is recovered by a direct resume pull, not abandoned
    // to the charged establishment window.
    let mut ledger = client::ResumeLedger::default();
    // bug_257 rider: per-worker warn-once escalation for persistent
    // listing failures — a dead store→scheduler edge surfaces above
    // debug instead of starving claims silently.
    let mut list_health = client::ListFailureLatch::default();
    // merged_bug_005: per-worker conversion-futility latch — a worker
    // whose every fresh mint is refused with a conversion-disproving
    // rejection withholds its listing beat so the scheduler re-homes
    // its rendezvous slice (the honest beat).
    let mut futility = client::ConversionFutilityLatch::default();
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        // Each worker claims at most one job per pass — concurrency is
        // the worker count, and the scheduler's one-winner arbitration
        // (per-replica composite identity) handles claim races.
        let pass = client::poll_and_claim(
            &mut transport,
            &instance,
            1,
            &mut ledger,
            &mut list_health,
            &mut futility,
            &shutdown,
        )
        .await;
        // round-8 WO-S2-1: the pacing decision is a total function of
        // the pass's sealed outcome, read BEFORE the claims are
        // consumed by execution.
        let pace = pace_for(&pass.outcome);
        for job in pass.claimed {
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
            let mut progress_transport = transport.clone();
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
                move |bytes_done, bytes_expected, upstream| {
                    let _ = progress_tx.try_send(
                        rio_proto::types::ReportMaterializationProgressRequest {
                            exec_id: exec_id_for_progress.clone(),
                            bytes_done,
                            bytes_expected,
                            upstream_uri: upstream.to_string(),
                        },
                    );
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
                &mut transport,
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
        }
        // live_046 (R-B) → round-8 WO-S2-1: the eager re-poll rides
        // the sealed outcome — work re-polls now, idle passes sleep
        // the jittered beat, contested passes honor the server's
        // floor (the `Floor` arm is unconstructed at this commit —
        // the law flips next commit). Jitter is KEPT on every beat
        // sleep; floor sleeps jitter upward only (the floor is never
        // undercut).
        match pace {
            Pace::Now => {}
            Pace::Beat => {
                let interval =
                    POLL_JITTER.apply(Duration::from_secs(cfg.poll_interval_secs.max(1)));
                if pace_after_empty_pass(&shutdown, interval).await {
                    return;
                }
            }
            Pace::Floor(floor) => {
                let interval = POLL_JITTER.apply(floor).max(floor);
                if pace_after_empty_pass(&shutdown, interval).await {
                    return;
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

/// round-8 WO-S2-1 — THE pacing law: one total function over the
/// sealed [`client::PassOutcome`] alphabet (the pacing loop's only
/// input; rustc's exhaustiveness is the census — a new outcome
/// variant fails this build). This commit mirrors the RETIRED
/// projection law cell-for-cell (semantics-neutral restructure); the
/// two defect cells are annotated and flip in the NEXT commit:
///
///   - `Delivered`/`Settled` → `Now`: conversion work re-polls
///     (reachable only on un-gated exits at this commit — the seal's
///     exit-first precedence reproduces the old withheld
///     short-circuit, the merged_bug_038 defect).
///   - `Contested` → `Now`: **defect cell (merged_bug_008, FS-4
///     face)** — contested passes re-poll at RPC speed, discarding
///     the server's stated floor; flips to `Floor` next commit.
///   - `ListedNoAction` → `Now`: **defect cell (merged_bug_008, hot
///     loop)** — a pass that listed work and took zero actions
///     re-polls unpaced; flips to `Beat` next commit.
///   - `Empty`/`Wedged`/`ListFailed` → `Beat`: nothing to do (or the
///     beat was withheld) — idle at the beat, never spin.
///   - `Abandoned` → `Beat`: SIGTERM — the loop exits before any
///     sleep matters; paced for totality.
pub fn pace_for(outcome: &client::PassOutcome) -> Pace {
    match outcome {
        client::PassOutcome::Delivered { .. } => Pace::Now,
        client::PassOutcome::Settled { .. } => Pace::Now,
        client::PassOutcome::Contested { .. } => Pace::Now,
        client::PassOutcome::ListedNoAction { .. } => Pace::Now,
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
        impl client::MaterializeTransport for SigtermAfterClaim {
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
                // SIGTERM lands right after the claim delivers.
                self.shutdown.cancel();
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

        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let transport = SigtermAfterClaim {
            shutdown: shutdown.clone(),
            reports: Arc::clone(&reports),
        };
        let ctx = executor::ExecutorContext {
            pool: db.pool.clone(),
            substituter,
            path_fanout: 1,
            path_slots: executor::PathSlotPool::new(32),
        };
        tokio::time::timeout(
            Duration::from_secs(30),
            claim_loop(
                0,
                crate::config::MaterializationConfig::default(),
                ctx,
                transport,
                rio_common::dns::Dns1123Label::sanitize(
                    "store-replica-0",
                    rio_common::dns::WORKER_SUFFIX_RESERVED,
                    "rio-store-dev",
                )
                .with_worker(0),
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
    /// Proposition (R16): per-cell pacing at THIS commit mirrors the
    /// retired projection law (semantics-neutral restructure). Two
    /// cells are the documented merged_bug_008 defect cells — they
    /// flip in the next commit:
    ///   - `Contested` → `Now` (defect: the server's floor is
    ///     discarded; flips to `Floor`);
    ///   - `ListedNoAction` → `Now` (defect: the zero-action hot
    ///     loop; flips to `Beat`).
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
            // DEFECT CELL (merged_bug_008, FS-4 face): the wire floor
            // is discarded — contested passes re-poll at RPC speed.
            // Flips to Floor(5 s) in the next commit.
            (
                PassOutcome::Contested {
                    floor: Some(Duration::from_secs(5)),
                },
                Pace::Now,
            ),
            (PassOutcome::Contested { floor: None }, Pace::Now),
            // DEFECT CELL (merged_bug_008, hot loop): a zero-action
            // listing re-polls unpaced. Flips to Beat next commit.
            (
                PassOutcome::ListedNoAction {
                    refused: 1,
                    skipped: 0,
                },
                Pace::Now,
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

    /// R17 const-relation pin (the beat-cadence floor as a typed
    /// envelope): the steal horizon was derived from "a healthy IDLE
    /// worker lists at least every ~1.2 s"; eager re-poll changes
    /// cadence only for PRODUCTIVE passes (freshness strictly
    /// improves) and leaves the idle/empty cadence — the horizon's
    /// binding worst case — byte-identical. The pin:
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

    /// Scripted pacing transport for the claim-loop tests: pops
    /// nothing, answers from a fixed shape, cancels shutdown after N
    /// listing calls (the loop's exit valve).
    #[derive(Clone)]
    struct PacingTransport {
        state: std::sync::Arc<std::sync::Mutex<PacingState>>,
    }

    struct PacingState {
        descriptors: Vec<rio_proto::types::MaterializationJobDescriptor>,
        list_calls: u32,
        cancel_after: u32,
        shutdown: rio_common::signal::Token,
    }

    impl client::MaterializeTransport for PacingTransport {
        async fn list_jobs(
            &mut self,
            _req: rio_proto::types::ListMaterializationJobsRequest,
        ) -> Result<rio_proto::types::ListMaterializationJobsResponse, tonic::Status> {
            let mut st = self.state.lock().unwrap();
            st.list_calls += 1;
            if st.list_calls >= st.cancel_after {
                st.shutdown.cancel();
            }
            Ok(rio_proto::types::ListMaterializationJobsResponse {
                jobs: st.descriptors.clone(),
            })
        }

        async fn pull(
            &mut self,
            _req: rio_proto::types::PullAssignmentRequest,
        ) -> Result<rio_proto::types::PullAssignmentResponse, tonic::Status> {
            Ok(rio_proto::types::PullAssignmentResponse {
                outcome: Some(
                    rio_proto::types::pull_assignment_response::Outcome::NotYetReady(
                        rio_proto::types::NotYetReady {
                            retry_after_seconds: 5,
                        },
                    ),
                ),
            })
        }

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

    async fn run_claim_loop_with(
        descriptors: Vec<rio_proto::types::MaterializationJobDescriptor>,
        cancel_after: u32,
    ) -> std::time::Duration {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let substituter =
            std::sync::Arc::new(crate::substitute::Substituter::new(db.pool.clone(), None));
        let shutdown = rio_common::signal::Token::new();
        let transport = PacingTransport {
            state: std::sync::Arc::new(std::sync::Mutex::new(PacingState {
                descriptors,
                list_calls: 0,
                cancel_after,
                shutdown: shutdown.clone(),
            })),
        };
        let cfg = crate::config::MaterializationConfig::default();
        let ctx = executor::ExecutorContext {
            pool: db.pool.clone(),
            substituter,
            path_fanout: 1,
            path_slots: executor::PathSlotPool::new(32),
        };
        // Pause the clock ONLY after the real-I/O setup (the ephemeral
        // PG pool's connect timeouts run on tokio time; pausing before
        // setup makes auto-advance fire them ahead of the socket).
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        claim_loop(
            0,
            cfg,
            ctx,
            transport,
            executor_instance().with_worker(0),
            shutdown,
        )
        .await;
        started.elapsed()
    }

    /// live_046 RED (loop-level, paused virtual clock):
    /// PRODUCTIVE passes re-poll immediately. Three consecutive
    /// productive passes (pass 1 mints + gets a contested answer —
    /// a ledger transition; passes 2-3 list live work) must complete
    /// with ZERO virtual time elapsed: no beat sleep was taken.
    /// Pre-fix (strawman-disclosed: the law fn and pacing facts do
    /// not exist pre-fix, so the red is structural-by-construction):
    /// the loop slept the jittered interval after EVERY pass — three
    /// passes cost >= 2 x ~1 s of virtual time.
    #[tokio::test]
    async fn productive_pass_repolls_without_sleep() {
        let descriptor = rio_proto::types::MaterializationJobDescriptor {
            job_id: uuid::Uuid::now_v7().to_string(),
            drv_hash: "pacing-live-drv".into(),
            tenant_id: String::new(),
            origin: "cache_opportunity".into(),
        };
        let elapsed = run_claim_loop_with(vec![descriptor], 3).await;
        assert_eq!(
            elapsed,
            std::time::Duration::ZERO,
            "left: the second listing waits out poll_interval despite a \
             productive first pass / right: immediate re-poll (zero beat \
             sleeps across three productive passes)"
        );
    }

    /// live_046 companion pin (the no-spin half): EMPTY passes sleep
    /// the jittered beat — three empty passes advance the virtual
    /// clock by at least two jittered intervals (>= 2 x 0.8 s at the
    /// 1 s default with ±20% jitter; the third pass exits via
    /// shutdown before its sleep). Jitter is preserved on every sleep
    /// that occurs.
    #[tokio::test]
    async fn empty_and_gated_passes_sleep_the_jittered_beat() {
        let elapsed = run_claim_loop_with(vec![], 3).await;
        assert!(
            elapsed >= std::time::Duration::from_millis(1600),
            "two empty-pass beats must elapse (got {elapsed:?})"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(2600),
            "and no more than two jittered beats before the exit \
             (got {elapsed:?})"
        );
    }
}
