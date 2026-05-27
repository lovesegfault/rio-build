//! Pull-mode client loop (`dispatch_mode = pull`).
//!
//! The pod is born knowing its derivation (`RIO_INTENT_ID` + the HMAC
//! executor token injected at Job spawn) and speaks exactly two
//! unaries: `ExecutorService.PullAssignment` (three outcomes —
//! `WorkAssignment` | `Gone` | `NotYetReady{retry_after}`) and
//! `ExecutorService.ReportOutcome`. There is no registration, no
//! heartbeat task, and no `BuildExecution` stream — the stream client
//! code is simply not started (see `setup.rs`).
//!
//! Build execution is the existing machinery, unchanged:
//! [`spawn_build_task`] runs the build and
//! sends today's `CompletionReport` into the permanent sink; this loop
//! consumes the sink directly (there is no relay) and forwards the
//! report through `ReportOutcome` until it is acknowledged. The
//! classification feed to the scheduler is therefore byte-identical to
//! the stream path's. Input prefetch is the executor's own
//! closure-compute + JIT/manifest warm path inside `execute_build`;
//! scheduler-pushed `PrefetchHint`s do not exist on the pull path.
//!
//! Exit codes (`builder.pull.exit-codes`): 0 only for `Gone`, for an
//! acknowledged `ReportOutcome`, and for the charge-free idle exit
//! after receiving only `NotYetReady` for the `idle_timeout` bound.
//! Every other termination exits nonzero so the Job goes Failed and
//! classification arrives via the controller's pod-terminal path.
//!
//! SIGTERM at this slice: the in-flight build keeps the stream-mode
//! semantics (no abort-on-SIGTERM yet); the pull/report retry loops
//! stop waiting and take the bounded best-effort path. The AD5 abort
//! semantics (cgroup-kill + one bounded `Cancelled` report inside the
//! pull-mode `terminationGracePeriodSeconds`) land with the 1b slice —
//! no pool template may flip to pull mode on a tree that predates
//! that, which is a deployment-time staging note (every deployable
//! image postdates the full workstream).

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use rio_proto::types::{
    CompletionReport, ExecutorMessage, PullAssignmentRequest, PullAssignmentResponse,
    ReportOutcomeRequest, WorkAssignment, executor_message, pull_assignment_response,
};

use super::{BuilderRuntime, run_teardown, spawn_build_task};

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

/// SIGTERM best-effort report: one final attempt with this timeout
/// (decision P5; inside the AD5 grace once 1b sets it).
const SIGTERM_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// The production transport: the same `ExecutorServiceClient` the
/// stream path uses (balanced channel in K8s, single channel in VM
/// tests).
impl PullTransport for super::setup::WorkerClient {
    async fn pull(
        &mut self,
        req: PullAssignmentRequest,
    ) -> Result<PullAssignmentResponse, tonic::Status> {
        self.pull_assignment(req).await.map(|r| r.into_inner())
    }

    async fn report(&mut self, req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
        self.report_outcome(req).await.map(|_| ())
    }
}

/// Ask for the assignment until the question is resolved.
///
/// - Unservable (RPC error: not-leader, recovery-gated, timeout) →
///   retry with the P5 envelope for as long as the pod lives; the pod
///   never exits merely because the pull cannot land
///   (`activeDeadlineSeconds` bounds the wait).
/// - `NotYetReady{retry_after}` → re-pull after the suggested delay
///   (±20 % jitter); after receiving only `NotYetReady` for
///   `idle_timeout`, exit charge-free (the I-116 successor).
/// - `Gone` / `WorkAssignment` → resolved.
/// - Shutdown → stop waiting (nothing started yet).
// r[impl builder.pull.retry-loop]
pub(super) async fn pull_until_resolved<T: PullTransport>(
    transport: &mut T,
    intent_id: &str,
    executor_token: &str,
    idle_timeout: Duration,
    shutdown: &rio_common::signal::Token,
) -> PullPhaseOutcome {
    let mut attempt: u32 = 0;
    // Set at the FIRST NotYetReady; the idle bound measures how long
    // the pod has been told "wanted but not deliverable" — transport
    // errors neither start nor reset it (they are not an answer).
    let mut not_ready_since: Option<tokio::time::Instant> = None;
    loop {
        if shutdown.is_cancelled() {
            return PullPhaseOutcome::Shutdown;
        }
        let req = PullAssignmentRequest {
            executor_token: executor_token.to_owned(),
            intent_id: intent_id.to_owned(),
        };
        let delay = match transport.pull(req).await {
            Ok(resp) => match resp.outcome {
                Some(pull_assignment_response::Outcome::Assignment(a)) => {
                    return PullPhaseOutcome::Assigned(Box::new(a));
                }
                Some(pull_assignment_response::Outcome::Gone(_)) => {
                    return PullPhaseOutcome::Gone;
                }
                Some(pull_assignment_response::Outcome::NotYetReady(nyr)) => {
                    let started = *not_ready_since.get_or_insert_with(tokio::time::Instant::now);
                    if started.elapsed() >= idle_timeout {
                        return PullPhaseOutcome::IdleExit;
                    }
                    // A NotYetReady answer is contact with the leader:
                    // reset the unservable backoff curve.
                    attempt = 0;
                    let suggested = if nyr.retry_after_seconds == 0 {
                        DEFAULT_RETRY_AFTER
                    } else {
                        Duration::from_secs(u64::from(nyr.retry_after_seconds))
                    };
                    RETRY_AFTER_JITTER.apply(suggested)
                }
                // Defensive: an empty oneof from a newer/older server is
                // not an answer — treat like an unservable pull.
                None => {
                    warn!("PullAssignment returned an empty outcome; retrying");
                    attempt = attempt.saturating_add(1);
                    RETRY_ENVELOPE.duration(attempt - 1)
                }
            },
            Err(status) => {
                tracing::debug!(code = ?status.code(), msg = status.message(),
                    "PullAssignment unservable; retrying");
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
/// Returns `true` on ack. `false` when the budget is exhausted or when
/// shutdown fired and the single best-effort attempt also failed — the
/// caller exits nonzero so the Job goes Failed and the controller's
/// pod-terminal path classifies it.
// r[impl builder.pull.retry-loop]
pub(super) async fn report_until_acked<T: PullTransport>(
    transport: &mut T,
    exec_id: &str,
    report: CompletionReport,
    budget: Duration,
    shutdown: &rio_common::signal::Token,
) -> bool {
    let started = tokio::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        let req = ReportOutcomeRequest {
            exec_id: exec_id.to_owned(),
            report: Some(report.clone()),
        };
        if shutdown.is_cancelled() {
            // SIGTERM: one bounded best-effort attempt, then out.
            return matches!(
                tokio::time::timeout(SIGTERM_REPORT_TIMEOUT, transport.report(req)).await,
                Ok(Ok(()))
            );
        }
        match transport.report(req).await {
            Ok(()) => return true,
            Err(status) => {
                if started.elapsed() >= budget {
                    warn!(code = ?status.code(), msg = status.message(),
                        "ReportOutcome never acknowledged within the retry budget");
                    return false;
                }
                tracing::debug!(code = ?status.code(), msg = status.message(),
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

/// Wait for the single build's `CompletionReport` on the permanent
/// sink. Everything else the build machinery emits on the sink (the
/// `WorkAssignmentAck`, log batches, prefetch ACKs) has no consumer in
/// pull mode and is discarded; draining it here also keeps the build
/// task from ever blocking on a full channel.
async fn wait_for_completion(
    sink_rx: &mut mpsc::Receiver<ExecutorMessage>,
) -> Option<CompletionReport> {
    while let Some(msg) = sink_rx.recv().await {
        if let Some(executor_message::Msg::Completion(report)) = msg.msg {
            return Some(report);
        }
    }
    None
}

/// The pull-mode lifecycle: pull → build (existing machinery) → report
/// → exit. Replaces the `'reconnect` stream loop when
/// `dispatch_mode = pull`.
// r[impl builder.pull.exit-codes]
pub(super) async fn run_pull(mut rt: BuilderRuntime) -> anyhow::Result<()> {
    anyhow::ensure!(
        !rt.intent_id.is_empty(),
        "dispatch_mode=pull requires RIO_INTENT_ID (the controller injects it at Job spawn)"
    );
    let mut transport = rt.scheduler_client.clone();
    let executor_token = rt.executor_token.clone();
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
    let Some(completion) = wait_for_completion(&mut sink_rx).await else {
        // Cannot happen while build_ctx holds a sender; treat as a
        // builder bug and let the pod-terminal path classify it.
        anyhow::bail!("completion channel closed before the build reported");
    };
    // The sink has been consumed (no relay in pull mode): the
    // completion is now owned by the report loop below.
    rt.completion_pending.store(false, Ordering::Release);

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
    struct ScriptedTransport {
        pulls: VecDeque<Result<PullAssignmentResponse, tonic::Status>>,
        reports: VecDeque<Result<(), tonic::Status>>,
        pull_calls: u32,
        report_calls: u32,
    }

    impl ScriptedTransport {
        fn new(
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
    // r[verify builder.pull.retry-loop]
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
    // r[verify builder.pull.retry-loop]
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
    // r[verify builder.pull.retry-loop]
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
    // r[verify builder.pull.retry-loop]
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
    // r[verify builder.pull.retry-loop]
    // r[verify builder.pull.exit-codes]
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
}
