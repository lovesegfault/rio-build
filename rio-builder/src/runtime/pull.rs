//! The pull client loop — the builder runtime's delivery path.
//!
//! The pod is born knowing its derivation (`RIO_INTENT_ID` + the HMAC
//! executor token injected at Job spawn) and speaks exactly two
//! unaries: `ExecutorService.PullAssignment` (three outcomes —
//! `WorkAssignment` | `Gone` | `NotYetReady{retry_after}`) and
//! `ExecutorService.ReportOutcome`. There is no registration, no
//! heartbeat task, and no dispatch stream.
//!
//! Build execution is the existing machinery, unchanged:
//! [`spawn_build_task`] runs the build and
//! sends the `CompletionReport` into the build-task sink; this loop
//! consumes the sink directly and forwards the
//! report through `ReportOutcome` until it is acknowledged. Input
//! prefetch is the executor's own closure-compute + JIT/manifest warm
//! path inside `execute_build`; there are no scheduler-pushed
//! prefetch hints.
//!
//! Exit codes (`builder.pull.exit-codes`): 0 only for `Gone`, for an
//! acknowledged `ReportOutcome`, and for the charge-free idle exit
//! after receiving only `NotYetReady` for the `idle_timeout` bound.
//! Every other termination exits nonzero so the Job goes Failed and
//! classification arrives via the controller's pod-terminal path.
//!
//! SIGTERM in pull mode is an **abort**, not a drain (AD5): an
//! in-flight build is cgroup-killed through the same cancel-honor path
//! `CancelSignal` uses (`try_cancel_build`: slot cancel flag +
//! cgroup.kill), the resulting `Cancelled` completion gets exactly one
//! bounded best-effort `ReportOutcome` attempt, logs are finalized by
//! teardown, and the process exits — all inside the pull-mode
//! `terminationGracePeriodSeconds` (45 s). There is no
//! finish-if-you-can mode for pull pods; any pod termination
//! (including graceful node drain) aborts charge-free and the drv
//! requeues. The pull/report retry loops keep their bounded
//! best-effort SIGTERM arms.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use rio_proto::types::{
    CompletionReport, PullAssignmentRequest, PullAssignmentResponse, ReportOutcomeRequest,
    WorkAssignment, pull_assignment_response,
};

use rio_common::grpc::DEFAULT_GRPC_TIMEOUT;
use rio_common::transport::{
    AttemptBudget, BoundedOutcome, GraceBudget, SIGTERM_FINAL_ATTEMPT, bounded,
};

use super::idle::IdleClock;
use super::{BuilderRuntime, run_teardown, spawn_build_task, try_cancel_build};
use crate::executor::BuildTaskMessage;

/// Default re-pull delay when `NotYetReady.retry_after_seconds` is 0
/// (defensive — the scheduler always suggests one; decision P4 = 5 s).
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);

/// ±20 % jitter applied to the server-suggested `retry_after`
/// (decision P4) so a forecast-spawned cohort doesn't re-pull in
/// lockstep.
const RETRY_AFTER_JITTER: rio_common::backoff::Jitter =
    rio_common::backoff::Jitter::Proportional(0.2);

/// Retry envelope for unservable pulls and unacked reports (decision
/// P5): exponential 1 s → 30 s cap, full jitter. Builder consts, not
/// Config fields.
const RETRY_ENVELOPE: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(30),
    jitter: rio_common::backoff::Jitter::Full,
};

/// How long the report phase keeps retrying before giving up and
/// exiting nonzero (red-first item (e)). The pod's
/// `activeDeadlineSeconds` usually ends the wait first; this bound
/// exists so a pod whose Job deadline is generous still terminates and
/// surfaces the failure through the pod-terminal path instead of
/// holding a node forever.
const REPORT_RETRY_BUDGET: Duration = Duration::from_secs(600);

/// AD5 grace partition: the pod's `terminationGracePeriodSeconds`
/// (the controller stamps the same constant on the pod spec) split
/// into the abort-drain slice — how long the SIGTERM path waits for a
/// killed build to surface its completion before synthesizing one —
/// and the reserved report slice for the final best-effort
/// `ReportOutcome`. Reserving the report window structurally is what
/// guarantees the report phase always runs before the kubelet's
/// SIGKILL, even when the build task is parked on a pre-cgroup RPC or
/// a wedged daemon (bug_377).
const PULL_GRACE: GraceBudget = GraceBudget::new(
    Duration::from_secs(rio_common::limits::PULL_MODE_TERMINATION_GRACE_SECS),
    Duration::from_secs(15),
);
// The reserved report slice always covers the SIGTERM final attempt.
const _: () = assert!(PULL_GRACE.report().as_secs() >= SIGTERM_FINAL_ATTEMPT.as_secs());

/// What the pull phase resolved to.
#[derive(Debug)]
pub(super) enum PullPhaseOutcome {
    /// The scheduler delivered the dispatch payload — build it.
    Assigned(Box<WorkAssignment>),
    /// No longer wanted: exit 0 without building (charge-free).
    Gone,
    /// Received only `NotYetReady` for the idle bound: exit 0
    /// charge-free (the OA6 pod-side bounded retry loop).
    IdleExit,
    /// Shutdown fired while still waiting for work: exit 0 (nothing
    /// started, nothing to report).
    Shutdown,
    /// The scheduler rejected the pull with a permanent,
    /// non-retryable status (identity/auth rejection, unimplemented
    /// RPC, invalid request): exit nonzero promptly instead of holding
    /// the node for the full `activeDeadlineSeconds`.
    Rejected(tonic::Status),
}

/// Permanent, non-retryable rejection codes: a mis-bound or
/// expired/invalid executor token (PermissionDenied / Unauthenticated),
/// a scheduler without the pull RPCs (Unimplemented), or a request the
/// server will never accept (InvalidArgument). Retrying these holds a
/// node for the full `activeDeadlineSeconds` with no chance of
/// progress; the loops terminate promptly and loudly instead
/// (`builder.pull.retry-loop+2`).
fn is_fatal_rejection(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
            | tonic::Code::Unimplemented
            | tonic::Code::InvalidArgument
    )
}

/// The two unaries the loop speaks, abstracted so the state machine is
/// unit-testable against a scripted transport (no wire, no scheduler).
pub(super) trait PullTransport {
    fn pull(
        &mut self,
        req: PullAssignmentRequest,
    ) -> impl Future<Output = Result<PullAssignmentResponse, tonic::Status>> + Send;
    fn report(
        &mut self,
        req: ReportOutcomeRequest,
    ) -> impl Future<Output = Result<(), tonic::Status>> + Send;
}

/// Wrap one unary request with the executor identity header (no-op in
/// dev mode where the token is empty and the scheduler is
/// keyless/permissive).
fn authed_request<T>(req: T, executor_token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(req);
    if !executor_token.is_empty() {
        // r[impl sec.executor.identity-token+3]
        let _ = rio_common::grpc::inject_metadata(
            request.metadata_mut(),
            &[(rio_proto::EXECUTOR_TOKEN_HEADER, executor_token)],
        );
    }
    request
}

/// The production transport: the same `ExecutorServiceClient` the
/// stream path uses (balanced channel in K8s, single channel in VM
/// tests), wrapped with the executor identity credential so both
/// unaries present `x-rio-executor-token`. `PullAssignment` keeps its
/// body `executor_token` too (frozen signature; the scheduler accepts
/// either carrier), but `ReportOutcome` has no body field — under the
/// enforced executor-HMAC posture the metadata header is the only
/// carrier, so without it every pull-mode report would be rejected
/// Unauthenticated and outcomes would degrade to establishment-sweep
/// classification.
pub(super) struct AuthedPullTransport {
    pub(super) client: super::setup::WorkerClient,
    pub(super) executor_token: String,
}

impl PullTransport for AuthedPullTransport {
    async fn pull(
        &mut self,
        req: PullAssignmentRequest,
    ) -> Result<PullAssignmentResponse, tonic::Status> {
        self.client
            .pull_assignment(authed_request(req, &self.executor_token))
            .await
            .map(|r| r.into_inner())
    }

    async fn report(&mut self, req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
        self.client
            .report_outcome(authed_request(req, &self.executor_token))
            .await
            .map(|_| ())
    }
}

/// Ask for the assignment until the question is resolved.
///
/// - Unservable (retryable RPC error: not-leader, recovery-gated,
///   transport error/timeout) → retry with the P5 envelope for as long
///   as the pod lives; the pod never exits merely because the pull
///   cannot land (`activeDeadlineSeconds` bounds the wait).
/// - Permanent rejection (Unauthenticated / PermissionDenied /
///   Unimplemented / InvalidArgument) → resolve `Rejected` after the
///   single answer: retrying cannot succeed and would silently hold
///   the node for the full deadline.
/// - `NotYetReady{retry_after}` → re-pull after the suggested delay
///   (±20 % jitter); after receiving only `NotYetReady` for
///   `idle_timeout`, exit charge-free (the I-116 successor).
/// - `Gone` / `WorkAssignment` → resolved.
/// - Shutdown → stop waiting (nothing started yet).
// r[impl builder.pull.retry-loop+2]
pub(super) async fn pull_until_resolved<T: PullTransport>(
    transport: &mut T,
    intent_id: &str,
    executor_token: &str,
    idle_timeout: Duration,
    shutdown: &rio_common::signal::Token,
) -> PullPhaseOutcome {
    let mut attempt: u32 = 0;
    // The idle bound measures accumulated "told wanted-but-not-
    // deliverable" time — only NotYetReady ANSWERS advance it, capped
    // at 2x the previous answer's suggested pacing, so transport
    // errors and leader outages between answers are structurally
    // uncounted (merged_bug_209: wall-clock-since-first-NYR matured
    // whole cohorts through a 5-minute failover and exited them en
    // masse on the first post-recovery answer).
    let mut idle = IdleClock::default();
    loop {
        if shutdown.is_cancelled() {
            return PullPhaseOutcome::Shutdown;
        }
        let req = PullAssignmentRequest {
            executor_token: executor_token.to_owned(),
            intent_id: intent_id.to_owned(),
            // Phase-A additive fields (attempt kind, executor_instance):
            // builders never set them; prost omits default values on the
            // wire, so this request encodes byte-identically to before.
            ..Default::default()
        };
        // The pull is a promptly-answered unary: bound it so an
        // accepted-never-answered request (black-holed leader) becomes
        // a retryable timeout instead of pinning the pod, and race it
        // against SIGTERM so an in-flight pull yields to shutdown
        // (merged_bug_167's pull half).
        let delay = match bounded(shutdown, DEFAULT_GRPC_TIMEOUT, transport.pull(req)).await {
            BoundedOutcome::Shutdown => return PullPhaseOutcome::Shutdown,
            BoundedOutcome::TimedOut { after } => {
                warn!(
                    after_secs = after.as_secs(),
                    "PullAssignment unanswered; retrying"
                );
                idle.on_non_answer();
                attempt = attempt.saturating_add(1);
                RETRY_ENVELOPE.duration(attempt - 1)
            }
            BoundedOutcome::Resolved(Ok(resp)) => match resp.outcome {
                Some(pull_assignment_response::Outcome::Assignment(a)) => {
                    return PullPhaseOutcome::Assigned(Box::new(a));
                }
                Some(pull_assignment_response::Outcome::Gone(_)) => {
                    return PullPhaseOutcome::Gone;
                }
                Some(pull_assignment_response::Outcome::NotYetReady(nyr)) => {
                    // A NotYetReady answer is contact with the leader:
                    // reset the unservable backoff curve.
                    attempt = 0;
                    let suggested = if nyr.retry_after_seconds == 0 {
                        DEFAULT_RETRY_AFTER
                    } else {
                        Duration::from_secs(u64::from(nyr.retry_after_seconds))
                    };
                    idle.on_answer(tokio::time::Instant::now(), suggested);
                    if idle.idle_for() >= idle_timeout {
                        return PullPhaseOutcome::IdleExit;
                    }
                    RETRY_AFTER_JITTER.apply(suggested)
                }
                // Defensive: an empty oneof from a newer/older server is
                // not an answer — treat like an unservable pull.
                None => {
                    warn!("PullAssignment returned an empty outcome; retrying");
                    idle.on_non_answer();
                    attempt = attempt.saturating_add(1);
                    RETRY_ENVELOPE.duration(attempt - 1)
                }
            },
            BoundedOutcome::Resolved(Err(status)) if is_fatal_rejection(status.code()) => {
                tracing::error!(code = ?status.code(), msg = status.message(),
                    "PullAssignment permanently rejected; exiting instead of holding the node");
                return PullPhaseOutcome::Rejected(status);
            }
            BoundedOutcome::Resolved(Err(status)) => {
                warn!(code = ?status.code(), msg = status.message(),
                    "PullAssignment unservable; retrying");
                idle.on_non_answer();
                attempt = attempt.saturating_add(1);
                RETRY_ENVELOPE.duration(attempt - 1)
            }
        };
        // Sleep, but wake immediately on SIGTERM (nothing is running
        // yet — exiting promptly is strictly better than waiting out a
        // backoff under a deletion grace period).
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return PullPhaseOutcome::Shutdown,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Forward the finished build's `CompletionReport` until the scheduler
/// acknowledges it (the ack is what licenses exit 0 — the scheduler
/// commits the attempt row before answering).
///
/// Returns `true` on ack. `false` when the budget is exhausted, when
/// the scheduler answers with a permanent rejection (identity/auth,
/// unimplemented, invalid — retrying cannot succeed; the establishment
/// sweep remains the backstop for the open attempt), or when shutdown
/// fired and the single best-effort attempt also failed — the caller
/// exits nonzero so the Job goes Failed and the controller's
/// pod-terminal path classifies it.
// r[impl builder.pull.retry-loop+2]
pub(super) async fn report_until_acked<T: PullTransport>(
    transport: &mut T,
    exec_id: &str,
    report: CompletionReport,
    budget: Duration,
    shutdown: &rio_common::signal::Token,
) -> bool {
    let budget = AttemptBudget::new(budget);
    let mut attempt: u32 = 0;
    loop {
        let req = ReportOutcomeRequest {
            exec_id: exec_id.to_owned(),
            report: Some(report.clone()),
            // Phase-A additive field (materialization_outcome): builders
            // never set it; prost omits the absent message on the wire,
            // so this request encodes byte-identically to before.
            ..Default::default()
        };
        if shutdown.is_cancelled() {
            // SIGTERM: one bounded best-effort attempt, then out
            // (plain timeout — the shutdown token is already
            // cancelled, so racing it again would never poll the RPC).
            return matches!(
                tokio::time::timeout(SIGTERM_FINAL_ATTEMPT, transport.report(req)).await,
                Ok(Ok(()))
            );
        }
        // Each attempt is bounded by the per-attempt cap clamped to
        // the remaining phase budget, and raced against SIGTERM. An
        // accepted-never-answered report (black-holed scheduler) now
        // spends the budget exactly like an answered failure does —
        // the 600 s bound fires on hung attempts too, instead of only
        // on Err answers (merged_bug_167's report half). Report acks
        // are idempotent scheduler-side, so a timed-out-but-actually-
        // applied attempt retried later answers Ok.
        let outcome = bounded(
            shutdown,
            budget.attempt_bound(DEFAULT_GRPC_TIMEOUT),
            transport.report(req),
        )
        .await;
        match outcome {
            // Loop back: the next iteration takes the SIGTERM
            // single-attempt arm.
            BoundedOutcome::Shutdown => continue,
            BoundedOutcome::Resolved(Ok(())) => return true,
            BoundedOutcome::Resolved(Err(status)) if is_fatal_rejection(status.code()) => {
                tracing::error!(code = ?status.code(), msg = status.message(),
                    "ReportOutcome permanently rejected; exiting nonzero (establishment sweep \
                     remains the backstop)");
                return false;
            }
            BoundedOutcome::TimedOut { after } => {
                if budget.expired() {
                    warn!(
                        after_secs = after.as_secs(),
                        "ReportOutcome never acknowledged within the retry budget (hung attempts)"
                    );
                    return false;
                }
                warn!(
                    after_secs = after.as_secs(),
                    "ReportOutcome attempt unanswered; retrying"
                );
                attempt = attempt.saturating_add(1);
                let delay = RETRY_ENVELOPE.duration(attempt - 1);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {}
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            BoundedOutcome::Resolved(Err(status)) => {
                if budget.expired() {
                    warn!(code = ?status.code(), msg = status.message(),
                        "ReportOutcome never acknowledged within the retry budget");
                    return false;
                }
                warn!(code = ?status.code(), msg = status.message(),
                    "ReportOutcome not acknowledged; retrying");
                attempt = attempt.saturating_add(1);
                let delay = RETRY_ENVELOPE.duration(attempt - 1);
                tokio::select! {
                    biased;
                    // Loop back immediately: the next iteration takes the
                    // SIGTERM single-attempt arm.
                    _ = shutdown.cancelled() => {}
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// Wait for the single build's `CompletionReport` on the build-task
/// sink. Everything else the build machinery emits on the sink (phase
/// edges) has no consumer here and is discarded; draining it also
/// keeps the build task from ever blocking on a full channel.
async fn wait_for_completion(
    sink_rx: &mut mpsc::Receiver<BuildTaskMessage>,
) -> Option<CompletionReport> {
    while let Some(msg) = sink_rx.recv().await {
        if let BuildTaskMessage::Completion(report) = msg {
            return Some(*report);
        }
    }
    None
}

// r[impl builder.cancel.cgroup-kill+2]
// r[impl builder.shutdown.sigint+4]
/// AD5 abort phase: wait for the single build's completion, aborting
/// the build if SIGTERM arrives first. The abort is the same
/// cancel-honor path `CancelSignal` uses (`try_cancel_build`: slot
/// cancel flag + cgroup.kill); the killed build task then emits its
/// `Cancelled` completion through the permanent sink exactly as a
/// scheduler-cancelled build does, and the caller's report phase makes
/// the one bounded best-effort attempt (the shutdown token is already
/// cancelled by then). There is no finish-if-you-can mode in pull
/// mode.
async fn build_phase_with_abort(
    slot: &std::sync::Arc<super::BuildSlot>,
    drv_path: &str,
    sink_rx: &mut mpsc::Receiver<BuildTaskMessage>,
    shutdown: &rio_common::signal::Token,
) -> Option<CompletionReport> {
    tokio::select! {
        completion = wait_for_completion(sink_rx) => completion,
        _ = shutdown.cancelled() => {
            info!(
                drv_path = %drv_path,
                "SIGTERM during the build: aborting (cgroup-kill) and reporting Cancelled"
            );
            try_cancel_build(slot, drv_path);
            // Bound the post-kill drain to the grace partition's
            // abort-drain slice. A build task parked where the cancel
            // flag is not consulted (pre-cgroup store RPC, wedged
            // daemon) never surfaces a completion — synthesize the
            // Cancelled report so the report phase STRUCTURALLY runs
            // inside the reserved slice and the scheduler takes the
            // AD5 charge-free close instead of the charged
            // establishment sweep (bug_377).
            match tokio::time::timeout(PULL_GRACE.abort_drain(), wait_for_completion(sink_rx))
                .await
            {
                Ok(completion) => completion,
                Err(_elapsed) => {
                    warn!(
                        drv_path = %drv_path,
                        drain_secs = PULL_GRACE.abort_drain().as_secs(),
                        "abort-drain budget elapsed; synthesizing the Cancelled report"
                    );
                    Some(CompletionReport {
                        drv_path: drv_path.to_owned(),
                        result: Some(rio_proto::types::BuildResult {
                            status: rio_proto::types::BuildResultStatus::Cancelled.into(),
                            error_msg: "abort-drain budget elapsed; build task parked \
                                        (pre-cgroup RPC or wedged daemon); pod exiting under grace"
                                .into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                }
            }
        }
    }
}

/// The pull lifecycle: pull → build (existing machinery) → report →
/// exit. The builder runtime's only delivery path.
// r[impl builder.pull.exit-codes]
// r[impl sched.executor.one-shot+2]
pub(super) async fn run_pull(mut rt: BuilderRuntime) -> anyhow::Result<()> {
    anyhow::ensure!(
        !rt.intent_id.is_empty(),
        "the pull runtime requires RIO_INTENT_ID (the controller injects it at Job spawn)"
    );
    let executor_token = rt.executor_token.clone();
    let mut transport = AuthedPullTransport {
        client: rt.scheduler_client.clone(),
        executor_token: executor_token.clone(),
    };
    let outcome = pull_until_resolved(
        &mut transport,
        &rt.intent_id,
        &executor_token,
        rt.idle_timeout,
        &rt.shutdown,
    )
    .await;

    let assignment = match outcome {
        PullPhaseOutcome::Gone => {
            info!(intent_id = %rt.intent_id, "derivation no longer wanted (Gone); exiting 0");
            run_teardown(rt);
            return Ok(());
        }
        PullPhaseOutcome::IdleExit => {
            info!(
                intent_id = %rt.intent_id,
                idle_secs = rt.idle_timeout.as_secs(),
                "only NotYetReady for the idle bound; exiting 0 (charge-free)"
            );
            run_teardown(rt);
            return Ok(());
        }
        PullPhaseOutcome::Shutdown => {
            info!(intent_id = %rt.intent_id, "shutdown while waiting for work; exiting 0");
            run_teardown(rt);
            return Ok(());
        }
        PullPhaseOutcome::Rejected(status) => {
            // Permanent rejection: nothing was started and nothing is
            // owed; exit nonzero promptly so the Job goes Failed and
            // the pod-terminal path (charge-free, no attempt row)
            // surfaces the misconfiguration instead of the node idling
            // until activeDeadlineSeconds.
            run_teardown(rt);
            anyhow::bail!(
                "PullAssignment permanently rejected ({:?}): {}",
                status.code(),
                status.message()
            );
        }
        PullPhaseOutcome::Assigned(a) => a,
    };

    // Readiness in pull mode = pulled/building (there is no heartbeat
    // to flip it). Cleared again before exit so a terminating pod
    // drops out of any Service endpoints promptly.
    rt.ready.store(true, Ordering::Relaxed);
    info!(
        drv_path = %assignment.drv_path,
        exec_id = %assignment.exec_id,
        "pull accepted; starting build"
    );

    let exec_id = assignment.exec_id.clone();
    let drv_path = assignment.drv_path.clone();
    let Some(guard) = rt.slot.try_claim(&assignment.drv_path) else {
        // Unreachable in practice (one pull per process, fresh slot);
        // fail loudly rather than build twice.
        anyhow::bail!("build slot unexpectedly busy at first pull");
    };
    spawn_build_task(*assignment, guard, &rt.build_ctx).await;

    let mut sink_rx = rt
        .pull_sink_rx
        .take()
        .expect("pull mode always carries the sink receiver");
    let completion = build_phase_with_abort(&rt.slot, &drv_path, &mut sink_rx, &rt.shutdown).await;
    let Some(completion) = completion else {
        // Cannot happen while build_ctx holds a sender; treat as a
        // builder bug and let the pod-terminal path classify it.
        anyhow::bail!("completion channel closed before the build reported");
    };

    let acked = report_until_acked(
        &mut transport,
        &exec_id,
        completion,
        REPORT_RETRY_BUDGET,
        &rt.shutdown,
    )
    .await;
    rt.ready.store(false, Ordering::Relaxed);
    run_teardown(rt);
    if acked {
        info!(exec_id = %exec_id, "outcome reported and acknowledged; exiting 0");
        Ok(())
    } else {
        // Nonzero exit → Job goes Failed → the controller's
        // pod-terminal path reports it; the scheduler's establishment
        // sweep is the final backstop.
        anyhow::bail!("ReportOutcome was never acknowledged; exiting nonzero")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted transport: pops one scripted answer per call; repeats
    /// the last entry forever once the script is exhausted. Counts
    /// calls so the tests can assert retry/once-only behavior.
    /// `pub(super)` so the AD5 abort battery (`abort_tests`) reuses it.
    pub(super) struct ScriptedTransport {
        pulls: VecDeque<Result<PullAssignmentResponse, tonic::Status>>,
        reports: VecDeque<Result<(), tonic::Status>>,
        pub(super) pull_calls: u32,
        pub(super) report_calls: u32,
    }

    impl ScriptedTransport {
        pub(super) fn new(
            pulls: Vec<Result<PullAssignmentResponse, tonic::Status>>,
            reports: Vec<Result<(), tonic::Status>>,
        ) -> Self {
            Self {
                pulls: pulls.into(),
                reports: reports.into(),
                pull_calls: 0,
                report_calls: 0,
            }
        }
    }

    impl PullTransport for ScriptedTransport {
        async fn pull(
            &mut self,
            _req: PullAssignmentRequest,
        ) -> Result<PullAssignmentResponse, tonic::Status> {
            self.pull_calls += 1;
            match self.pulls.len() {
                0 => Err(tonic::Status::unavailable("script exhausted")),
                1 => self.pulls[0].clone(),
                _ => self.pulls.pop_front().expect("non-empty"),
            }
        }

        async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
            self.report_calls += 1;
            match self.reports.len() {
                0 => Err(tonic::Status::unavailable("script exhausted")),
                1 => self.reports[0].clone(),
                _ => self.reports.pop_front().expect("non-empty"),
            }
        }
    }

    fn assignment_resp(exec_id: &str) -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::Assignment(
                WorkAssignment {
                    drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
                    exec_id: exec_id.into(),
                    ..Default::default()
                },
            )),
        }
    }

    fn gone_resp() -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::Gone(
                rio_proto::types::Gone {},
            )),
        }
    }

    fn not_yet_ready_resp(secs: u32) -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::NotYetReady(
                rio_proto::types::NotYetReady {
                    retry_after_seconds: secs,
                },
            )),
        }
    }

    fn token() -> rio_common::signal::Token {
        rio_common::signal::Token::new()
    }

    const IDLE: Duration = Duration::from_secs(120);

    /// (a) Unservable pulls (not-leader / timeout / transport error)
    /// are retried with backoff and the loop never exits on its own —
    /// an hour of simulated unavailability and it is still asking.
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn pull_retries_unservable_and_never_gives_up() {
        let mut t = ScriptedTransport::new(
            vec![Err(tonic::Status::unavailable(
                "not leader (standby replica)",
            ))],
            vec![],
        );
        let shutdown = token();
        let resolved = tokio::time::timeout(
            Duration::from_secs(3600),
            pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown),
        )
        .await;
        assert!(
            resolved.is_err(),
            "an unservable scheduler must never resolve the pull phase by itself"
        );
        // The cap is 30 s, so an hour of retrying is at least ~120 calls.
        assert!(
            t.pull_calls >= 100,
            "expected continuous retries under the 1→30 s envelope, saw {}",
            t.pull_calls
        );
    }

    /// (a/continued) After transient unavailability the next answer is
    /// honored — here the third pull delivers.
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn pull_recovers_after_transient_errors() {
        let mut t = ScriptedTransport::new(
            vec![
                Err(tonic::Status::unavailable("not leader")),
                Err(tonic::Status::deadline_exceeded("timeout")),
                Ok(assignment_resp("exec-1")),
            ],
            vec![],
        );
        let shutdown = token();
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        match outcome {
            PullPhaseOutcome::Assigned(a) => assert_eq!(a.exec_id, "exec-1"),
            other => panic!("expected Assigned, got {other:?}"),
        }
        assert_eq!(t.pull_calls, 3);
    }

    /// (b) `Gone` resolves immediately: exit 0 without building, no
    /// further pulls.
    // r[verify builder.pull.exit-codes]
    #[tokio::test(start_paused = true)]
    async fn gone_resolves_without_building() {
        let mut t = ScriptedTransport::new(vec![Ok(gone_resp())], vec![]);
        let shutdown = token();
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(matches!(outcome, PullPhaseOutcome::Gone));
        assert_eq!(t.pull_calls, 1, "Gone is final — no re-pull");
    }

    /// (c) `NotYetReady{5}` re-pulls after ~5 s (±20 % jitter), and a
    /// pod that receives only `NotYetReady` for `idle_timeout` exits
    /// charge-free (the OA6 pod-side bounded retry loop).
    // r[verify builder.pull.retry-loop+2]
    // r[verify builder.pull.exit-codes]
    #[tokio::test(start_paused = true)]
    async fn not_yet_ready_repulls_and_idle_bounds() {
        let mut t = ScriptedTransport::new(vec![Ok(not_yet_ready_resp(5))], vec![]);
        let shutdown = token();
        let started = tokio::time::Instant::now();
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(matches!(outcome, PullPhaseOutcome::IdleExit));
        let waited = started.elapsed();
        assert!(
            waited >= IDLE,
            "idle exit must not fire before the idle bound (waited {waited:?})"
        );
        // 120 s of 5 s (±20 %) re-pulls: between ~120/6=20 and ~120/4=30
        // calls, plus the initial one. Anything in [20, 32] proves the
        // suggested retry_after (not the unservable envelope) paces it.
        assert!(
            (20..=32).contains(&t.pull_calls),
            "expected ~24 re-pulls paced by retry_after=5s±20%, saw {}",
            t.pull_calls
        );
    }

    /// (f) The pull phase stops at the first delivered assignment — it
    /// never pulls again, so the client cannot start a second build
    /// (one slot, one claim, one spawned task).
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn pull_phase_stops_at_first_assignment() {
        let mut t = ScriptedTransport::new(
            vec![Ok(assignment_resp("exec-1")), Ok(assignment_resp("exec-2"))],
            vec![],
        );
        let shutdown = token();
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        match outcome {
            PullPhaseOutcome::Assigned(a) => assert_eq!(a.exec_id, "exec-1"),
            other => panic!("expected Assigned, got {other:?}"),
        }
        assert_eq!(
            t.pull_calls, 1,
            "the loop must not pull again after accepting an assignment"
        );
    }

    /// (d) The report is retried until acknowledged, then the loop
    /// stops (exit 0 follows at the caller).
    // r[verify builder.pull.retry-loop+2]
    // r[verify builder.pull.exit-codes]
    // r[verify builder.completion.exactly-once-or-death+2]
    #[tokio::test(start_paused = true)]
    async fn report_retries_until_acked() {
        let mut t = ScriptedTransport::new(
            vec![],
            vec![
                Err(tonic::Status::unavailable("not leader")),
                Err(tonic::Status::unavailable("still settling")),
                Ok(()),
            ],
        );
        let shutdown = token();
        let acked = report_until_acked(
            &mut t,
            "exec-1",
            CompletionReport::default(),
            REPORT_RETRY_BUDGET,
            &shutdown,
        )
        .await;
        assert!(acked);
        assert_eq!(t.report_calls, 3);
    }

    /// (e) A report that is never acknowledged within the budget gives
    /// up with `false` — the caller exits nonzero so the Job goes
    /// Failed and the pod-terminal path classifies it.
    // r[verify builder.pull.exit-codes]
    // r[verify builder.completion.exactly-once-or-death+2]
    #[tokio::test(start_paused = true)]
    async fn report_budget_exhausted_is_nonzero_exit() {
        let mut t = ScriptedTransport::new(
            vec![],
            vec![Err(tonic::Status::unavailable("scheduler gone"))],
        );
        let shutdown = token();
        let acked = report_until_acked(
            &mut t,
            "exec-1",
            CompletionReport::default(),
            Duration::from_secs(60),
            &shutdown,
        )
        .await;
        assert!(!acked, "an unacked report must not be reported as success");
        assert!(
            t.report_calls >= 3,
            "the budget window must be spent retrying, saw {} calls",
            t.report_calls
        );
    }

    /// (g) Permanent rejections (mis-bound/expired token, pre-pull
    /// scheduler, invalid request) resolve the pull phase after exactly
    /// one call instead of silently holding the node for the full
    /// activeDeadlineSeconds.
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn pull_fatal_rejection_resolves_after_one_call() {
        for status in [
            tonic::Status::permission_denied("token bound to a different intent"),
            tonic::Status::unauthenticated("executor token expired"),
            tonic::Status::unimplemented("PullAssignment not served"),
            tonic::Status::invalid_argument("intent_id is required"),
        ] {
            let code = status.code();
            let mut t = ScriptedTransport::new(vec![Err(status)], vec![]);
            let shutdown = token();
            let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
            match outcome {
                PullPhaseOutcome::Rejected(s) => assert_eq!(s.code(), code),
                other => panic!("expected Rejected for {code:?}, got {other:?}"),
            }
            assert_eq!(
                t.pull_calls, 1,
                "a permanent rejection must not be retried ({code:?})"
            );
        }
    }

    /// (g/continued) The same discrimination in the report loop: a
    /// permanent rejection gives up after exactly one call (nonzero
    /// exit; the establishment sweep stays the backstop), while the
    /// existing retryable behaviour is untouched.
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn report_fatal_rejection_gives_up_after_one_call() {
        for status in [
            tonic::Status::permission_denied("token bound to a different intent"),
            tonic::Status::unauthenticated("executor token expired"),
            tonic::Status::unimplemented("ReportOutcome not served"),
        ] {
            let code = status.code();
            let mut t = ScriptedTransport::new(vec![], vec![Err(status)]);
            let shutdown = token();
            let acked = report_until_acked(
                &mut t,
                "exec-1",
                CompletionReport::default(),
                REPORT_RETRY_BUDGET,
                &shutdown,
            )
            .await;
            assert!(!acked, "a permanently rejected report is not an ack");
            assert_eq!(
                t.report_calls, 1,
                "a permanent rejection must not burn the retry budget ({code:?})"
            );
        }
    }

    /// The production transport presents the executor identity token as
    /// call metadata on both unaries (the same header the stream open
    /// and heartbeats use); dev mode (empty token) sends no header.
    // r[verify sec.executor.identity-token+3]
    #[test]
    fn authed_request_injects_the_identity_header() {
        let req = authed_request(PullAssignmentRequest::default(), "tok-abc");
        assert_eq!(
            req.metadata()
                .get(rio_proto::EXECUTOR_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("tok-abc"),
            "the identity header rides every authed unary"
        );
        let req = authed_request(ReportOutcomeRequest::default(), "tok-abc");
        assert_eq!(
            req.metadata()
                .get(rio_proto::EXECUTOR_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("tok-abc"),
            "ReportOutcome carries the same header (it has no body fallback)"
        );
        let req = authed_request(PullAssignmentRequest::default(), "");
        assert!(
            req.metadata()
                .get(rio_proto::EXECUTOR_TOKEN_HEADER)
                .is_none(),
            "dev mode (no token) sends no header"
        );
    }

    /// A black-holed scheduler (in-flight report never answered) does
    /// not pin the process past SIGTERM: the in-flight RPC is raced
    /// against shutdown and the loop falls through to the bounded
    /// single-attempt arm.
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn report_in_flight_black_hole_yields_to_sigterm() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct BlackHoleTransport {
            calls: Arc<AtomicU32>,
        }
        impl PullTransport for BlackHoleTransport {
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("the black hole never answers")
            }
        }

        let calls = Arc::new(AtomicU32::new(0));
        let mut t = BlackHoleTransport {
            calls: Arc::clone(&calls),
        };
        let shutdown = token();
        let shutdown_for_task = shutdown.clone();
        let task = tokio::spawn(async move {
            report_until_acked(
                &mut t,
                "exec-bh",
                CompletionReport::default(),
                REPORT_RETRY_BUDGET,
                &shutdown_for_task,
            )
            .await
        });
        // Let the first report get in flight, then deliver SIGTERM.
        tokio::time::sleep(Duration::from_secs(1)).await;
        shutdown.cancel();
        let acked = tokio::time::timeout(Duration::from_secs(120), task)
            .await
            .expect("the loop must resolve within the SIGTERM report timeout, not hang")
            .expect("task not panicked");
        assert!(!acked, "nothing was ever acknowledged");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the abandoned in-flight call plus the bounded single attempt"
        );
    }

    /// merged_bug_167 (pull half, red-first): a black-holed PULL
    /// (accepted, never answered) yields to SIGTERM instead of pinning
    /// the pod past the pull-mode grace.
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn pull_in_flight_black_hole_yields_to_sigterm() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct BlackHolePull {
            calls: Arc<AtomicU32>,
        }
        impl PullTransport for BlackHolePull {
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("the black hole never answers")
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
        }

        let calls = Arc::new(AtomicU32::new(0));
        let mut t = BlackHolePull {
            calls: Arc::clone(&calls),
        };
        let shutdown = token();
        let shutdown_for_task = shutdown.clone();
        let task = tokio::spawn(async move {
            pull_until_resolved(&mut t, "intent-bh", "tok", IDLE, &shutdown_for_task).await
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        shutdown.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("an in-flight pull must yield to SIGTERM, not hang")
            .expect("task not panicked");
        assert!(matches!(outcome, PullPhaseOutcome::Shutdown));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// merged_bug_167 (report half, red-first): a black-holed report
    /// loop WITHOUT SIGTERM exhausts the retry budget — hung attempts
    /// spend the budget exactly like answered failures.
    // r[verify builder.pull.retry-loop+2]
    // r[verify builder.completion.exactly-once-or-death+2]
    #[tokio::test(start_paused = true)]
    async fn report_black_hole_exhausts_budget_without_sigterm() {
        struct BlackHoleReport;
        impl PullTransport for BlackHoleReport {
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                std::future::pending::<()>().await;
                unreachable!("the black hole never answers")
            }
        }

        let mut t = BlackHoleReport;
        let shutdown = token();
        let started = tokio::time::Instant::now();
        let acked = tokio::time::timeout(
            Duration::from_secs(3600),
            report_until_acked(
                &mut t,
                "exec-bh-budget",
                CompletionReport::default(),
                REPORT_RETRY_BUDGET,
                &shutdown,
            ),
        )
        .await
        .expect("hung attempts must exhaust the budget, not pend forever");
        assert!(!acked);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= REPORT_RETRY_BUDGET
                && elapsed < REPORT_RETRY_BUDGET + Duration::from_secs(120),
            "the budget bounds the hung-report phase (elapsed {elapsed:?})"
        );
    }

    /// merged_bug_209 (red-first): a 300 s scheduler outage between two
    /// NotYetReady answers does NOT mature the idle bound — only
    /// answered told-not-deliverable time counts; afterwards ~120 s of
    /// paced answers exits charge-free.
    // r[verify builder.pull.retry-loop+2]
    // r[verify builder.pull.exit-codes]
    #[tokio::test(start_paused = true)]
    async fn idle_outage_gap_does_not_count() {
        // Script: NYR(5), then ~300s of unavailable answers, then NYR(5)
        // forever. ScriptedTransport repeats the last entry once
        // exhausted, so: one NYR, 60 errors (~300s under the 1→30s
        // envelope... errors back off to 30s cap), then NYR forever.
        let mut pulls: Vec<Result<PullAssignmentResponse, tonic::Status>> =
            vec![Ok(not_yet_ready_resp(5))];
        // 14 errors ≈ 1+2+4+8+16+30*9 ≈ 300 s of outage.
        for _ in 0..14 {
            pulls.push(Err(tonic::Status::unavailable("leader failover")));
        }
        pulls.push(Ok(not_yet_ready_resp(5)));
        let mut t = ScriptedTransport::new(pulls, vec![]);
        let shutdown = token();
        let outcome = pull_until_resolved(&mut t, "intent-idle", "tok", IDLE, &shutdown).await;
        assert!(matches!(outcome, PullPhaseOutcome::IdleExit));
        // Structural assertion (jitter-proof): if the outage counted,
        // the FIRST post-outage answer would exit (1 NYR + 14 errors +
        // 1 NYR = 16 calls). Told-time accumulation requires ~120 s of
        // answered 5 s pacing AFTER the outage: ≥ 20 more answers.
        assert!(
            t.pull_calls >= 33,
            "the idle exit must NOT fire on outage time (exited after only {} pulls)",
            t.pull_calls
        );
    }
}

#[cfg(test)]
mod abort_tests {
    //! AD5 red-first battery (a): SIGTERM mid-build aborts via the
    //! cancel-honor path, the cancelled completion is collected, and
    //! the report phase makes exactly one bounded attempt under an
    //! already-cancelled shutdown token.

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn completion_with_status(
        drv_path: &str,
        status: rio_proto::types::BuildResultStatus,
    ) -> BuildTaskMessage {
        BuildTaskMessage::Completion(Box::new(CompletionReport {
            drv_path: drv_path.into(),
            result: Some(rio_proto::types::BuildResult {
                status: status.into(),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    // r[verify builder.cancel.cgroup-kill+2]
    // r[verify builder.shutdown.sigint+4]
    /// SIGTERM with a build in flight: the abort fires through the
    /// cancel-honor path (the slot's cancel flag is set — the same flag
    /// the cgroup-kill path keys on), the build task's `Cancelled`
    /// completion is collected, and nothing waited on the full report
    /// budget or the stream drain machinery.
    #[tokio::test(start_paused = true)]
    async fn sigterm_mid_build_aborts_and_collects_cancelled_report() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";
        let slot = Arc::new(super::super::BuildSlot::default());
        let guard = slot.try_claim(drv).expect("fresh slot claims");
        let cancel_flag = guard.cancelled();
        let (sink_tx, mut sink_rx) = mpsc::channel::<BuildTaskMessage>(8);
        let shutdown = rio_common::signal::Token::new();

        let slot_for_task = Arc::clone(&slot);
        let shutdown_for_task = shutdown.clone();
        let drv_owned = drv.to_string();
        let phase = tokio::spawn(async move {
            build_phase_with_abort(&slot_for_task, &drv_owned, &mut sink_rx, &shutdown_for_task)
                .await
        });

        // Deliver SIGTERM. The phase must abort the in-flight build
        // (set the cancel flag) rather than wait for it to finish.
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !cancel_flag.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the abort sets the cancel-honor flag promptly");

        // The killed build task emits its Cancelled completion exactly
        // as a scheduler-cancelled build does; the phase returns it.
        sink_tx
            .send(completion_with_status(
                drv,
                rio_proto::types::BuildResultStatus::Cancelled,
            ))
            .await
            .expect("sink open");
        let completion = tokio::time::timeout(Duration::from_secs(5), phase)
            .await
            .expect("phase resolves within the grace, not the build budget")
            .expect("phase task not panicked")
            .expect("completion collected");
        assert_eq!(
            completion.result.map(|r| r.status),
            Some(i32::from(rio_proto::types::BuildResultStatus::Cancelled)),
            "the abort surfaces the cancel-honor completion"
        );
        drop(guard);
    }

    /// Without SIGTERM the phase is a plain wait: the completion passes
    /// through untouched and the cancel flag is never set (no abort on
    /// the happy path).
    #[tokio::test(start_paused = true)]
    async fn build_phase_without_sigterm_never_aborts() {
        let drv = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y.drv";
        let slot = Arc::new(super::super::BuildSlot::default());
        let guard = slot.try_claim(drv).expect("fresh slot claims");
        let cancel_flag = guard.cancelled();
        let (sink_tx, mut sink_rx) = mpsc::channel::<BuildTaskMessage>(8);
        let shutdown = rio_common::signal::Token::new();

        sink_tx
            .send(completion_with_status(
                drv,
                rio_proto::types::BuildResultStatus::Built,
            ))
            .await
            .expect("sink open");
        let completion = build_phase_with_abort(&slot, drv, &mut sink_rx, &shutdown)
            .await
            .expect("completion collected");
        assert_eq!(
            completion.result.map(|r| r.status),
            Some(i32::from(rio_proto::types::BuildResultStatus::Built))
        );
        assert!(
            !cancel_flag.load(Ordering::Acquire),
            "no SIGTERM ⇒ no abort"
        );
        drop(guard);
    }

    // r[verify builder.cancel.cgroup-kill+2]
    /// The post-abort report is exactly one bounded best-effort
    /// attempt: with the shutdown token already cancelled, an unacked
    /// report is tried once (within the 10 s SIGTERM timeout) and the
    /// loop exits instead of burning the 600 s retry budget — the
    /// process leaves within the pull-mode grace.
    #[tokio::test(start_paused = true)]
    async fn sigterm_report_is_a_single_bounded_attempt() {
        use super::tests::ScriptedTransport;
        let mut t = ScriptedTransport::new(
            vec![],
            vec![Err(tonic::Status::unavailable("scheduler unreachable"))],
        );
        let shutdown = rio_common::signal::Token::new();
        shutdown.cancel();
        let started = tokio::time::Instant::now();
        let acked = report_until_acked(
            &mut t,
            "exec-abort",
            CompletionReport::default(),
            REPORT_RETRY_BUDGET,
            &shutdown,
        )
        .await;
        assert!(!acked, "an unacked best-effort attempt reports failure");
        assert_eq!(
            t.report_calls, 1,
            "exactly one report attempt after SIGTERM"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the bounded attempt returns well inside the pull-mode grace, \
             never the full retry budget"
        );

        // And when the single attempt succeeds, the report is acked.
        let mut ok = ScriptedTransport::new(vec![], vec![Ok(())]);
        let acked = report_until_acked(
            &mut ok,
            "exec-abort-2",
            CompletionReport::default(),
            REPORT_RETRY_BUDGET,
            &shutdown,
        )
        .await;
        assert!(acked);
        assert_eq!(ok.report_calls, 1);
    }

    /// bug_377 (red-first): SIGTERM with the build task parked where
    /// the cancel flag is not consulted (pre-cgroup RPC / wedged
    /// daemon) — the sink never yields. The phase must synthesize the
    /// Cancelled completion within the abort-drain slice so the report
    /// phase structurally runs inside the grace.
    // r[verify builder.cancel.cgroup-kill+2]
    // r[verify builder.shutdown.sigint+4]
    #[tokio::test(start_paused = true)]
    async fn sigterm_with_parked_build_synthesizes_cancelled_within_grace() {
        let drv = "/nix/store/cccccccccccccccccccccccccccccccc-z.drv";
        let slot = Arc::new(super::super::BuildSlot::default());
        let guard = slot.try_claim(drv).expect("fresh slot claims");
        let (_sink_tx, mut sink_rx) = mpsc::channel::<BuildTaskMessage>(8);
        let shutdown = rio_common::signal::Token::new();
        shutdown.cancel();

        let started = tokio::time::Instant::now();
        // The sink sender is held open and never yields a completion —
        // the parked-build shape.
        let completion = tokio::time::timeout(
            Duration::from_secs(44),
            build_phase_with_abort(&slot, drv, &mut sink_rx, &shutdown),
        )
        .await
        .expect("the phase must resolve within the abort-drain slice, never ride to SIGKILL")
        .expect("a synthesized completion is always produced");
        assert!(
            started.elapsed() <= Duration::from_secs(35),
            "resolution must fit the abort-drain slice (30 s), leaving the report reserve"
        );
        assert_eq!(
            completion.result.as_ref().map(|r| r.status),
            Some(i32::from(rio_proto::types::BuildResultStatus::Cancelled)),
        );
        assert!(
            completion
                .result
                .as_ref()
                .is_some_and(|r| r.error_msg.contains("abort-drain budget elapsed")),
            "the synthesized report names the parked-build cause"
        );
        assert_eq!(completion.drv_path, drv);
        drop(guard);
    }
}
