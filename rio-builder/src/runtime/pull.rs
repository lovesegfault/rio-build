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
//! Exit codes (`builder.pull.exit-codes+1`): 0 only for `Gone`, for an
//! acknowledged `ReportOutcome`, and for the charge-free idle exit
//! after receiving only `NotYetReady` for the `idle_timeout` bound.
//! Every other termination exits nonzero so the Job goes Failed and
//! classification arrives via the controller's pod-terminal path.
//!
//! SIGTERM in pull mode is an **abort**, not a drain (AD5): an
//! in-flight build is cgroup-killed through the cancel-honor path
//! (`try_cancel_build`: slot cancel flag + cgroup.kill — the surviving
//! machinery of the stream-era `CancelSignal`), the resulting `Cancelled` completion gets exactly one
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
    AttemptBudget, BoundedOutcome, EffectfulOutcome, GraceBudget, SIGTERM_FINAL_ATTEMPT, bounded,
    bounded_effectful,
};

use super::idle::IdleClock;
use super::{BuilderRuntime, run_teardown, spawn_build_task, try_cancel_build};
use crate::executor::BuildTaskMessage;

/// Default re-pull delay when `NotYetReady.retry_after_seconds` is 0
/// (defensive — the scheduler always suggests one; decision P4 = 5 s).
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);

/// R17 domain ceiling for wire-supplied retry hints (merged_bug_156):
/// the pull loop's sleep wakes only on shutdown and `IdleClock`
/// credits only between answers, so an unbounded hint parks the loop
/// past every exit. VIOLABLE, with the derivation: every in-tree
/// producer is a bounded const (the scheduler's
/// `NOT_YET_READY_RETRY_AFTER_SECS` = 5 s; the store's materialize
/// hint clamps at 300 s), and `GRPC_STREAM_TIMEOUT` = 300 s is the
/// longest the transport itself holds a single call — so 300 s is the
/// largest hint a non-skewed producer can mean. A larger landed value
/// trades parked-loop wedge depth for re-pull chatter under a future
/// slow-poll protocol; measure before moving it.
const RETRY_AFTER_PACING_CEILING: Duration = Duration::from_secs(300);

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
// merged_bug_270: the maybe-minted shutdown resolution spends at most
// one bounded confirm pull plus one bounded report attempt — both must
// fit inside the pod's termination grace.
const _: () = assert!(
    2 * SIGTERM_FINAL_ATTEMPT.as_secs() <= rio_common::limits::PULL_MODE_TERMINATION_GRACE_SECS
);

/// merged_bug_083 — the wire-effect evidence, typed. THE single truth
/// site for the latch's set/clear alphabet (merged_bug_012 house
/// rule, encoded here: use sites MAY only doc-link to this type doc,
/// never restate the alphabet — a semantics change has exactly one
/// place to edit):
///
/// - SET by any post-send uncertainty: a timed-out pull, a post-send
///   shutdown abandonment, a post-send transport error
///   ([`MintEvidence::latch_send`]), or an answered-UNDECODABLE
///   outcome — an unknown oneof variant from a newer/older server is
///   an ANSWER whose content may be a mint (bug_089, the fourth
///   letter; latched structurally by
///   [`MintEvidence::witness_answer`], the only consumer of an
///   answered pull).
/// - CLEARED only by a LOOP-TERMINATING answer: a delivered
///   `Assignment` or `Gone`
///   ([`MintEvidence::clear_loop_terminating`]). `NotYetReady`
///   deliberately does NOT clear, and there is NO method that clears
///   on an interleaved per-request answer (`NotYetReady` to request N
///   proves nothing about abandoned request N-1 — no
///   requester-liveness gate exists on the durable mint), so the
///   pre-fix bug shape (an answer laundering an earlier maybe-mint)
///   is unrepresentable in the API.
///
/// The Gone clear's "nothing is or will be held" claim is made
/// literally true by the scheduler's durable confirm fence
/// (merged_bug_011, `sched.executor.confirm-fence`): every keyed Gone
/// is fence-written ahead of the reply and a straggler pull is
/// screened, so clearing the whole send history on Gone is sound —
/// not merely optimistic.
#[derive(Debug, Default)]
struct MintEvidence {
    unconfirmed_send: bool,
}

impl MintEvidence {
    /// A pull reached (or may have reached) the wire and was never
    /// answered: timeout, post-send shutdown abandonment, or a
    /// post-send transport error (`Resolved(Err)` — transport.rs
    /// documents Resolved as "answer OR transport error"; a
    /// connection reset after send is the same epistemic state as a
    /// timeout).
    fn latch_send(&mut self) {
        self.unconfirmed_send = true;
    }

    /// A LOOP-TERMINATING protocol resolution that is authoritative
    /// for this pod's whole send history: Gone (the scheduler says
    /// nothing is or will be held for this intent) or a delivered
    /// assignment (we hold the work — the report path owns it now).
    fn clear_loop_terminating(&mut self) {
        self.unconfirmed_send = false;
    }

    /// One chokepoint deciding exit-0 from send history
    /// (merged_bug_083's keystone): true = every pull this process
    /// ever sent was answered (or none was sent).
    fn may_exit_zero(&self) -> bool {
        !self.unconfirmed_send
    }

    /// The ONE decode seam for an ANSWERED pull (bug_089): yields the
    /// typed witness, and the latch decision is the CONSTRUCTOR'S
    /// effect, not a per-arm call — so a latch omission at a new arm
    /// is unwritable. The match below is exhaustive over the wire
    /// alphabet: a new oneof variant fails compilation HERE, forcing
    /// its latch decision and its witness arm in the same edit (the
    /// confirm path types this state the same way —
    /// `resolve_maybe_minted`'s None arm refuses exit 0).
    ///
    /// Latch effects per letter: Assignment/Gone CLEAR (loop-
    /// terminating, authoritative for the whole send history);
    /// NotYetReady PRESERVES (authoritative only about attempts held
    /// at its answer time); Undecodable LATCHES (an answer whose
    /// content may be a mint we cannot read).
    fn witness_answer(
        &mut self,
        outcome: Option<pull_assignment_response::Outcome>,
    ) -> DecodedAnswer {
        match outcome {
            Some(pull_assignment_response::Outcome::Assignment(a)) => {
                self.clear_loop_terminating();
                DecodedAnswer::Assignment(Box::new(a))
            }
            Some(pull_assignment_response::Outcome::Gone(_)) => {
                self.clear_loop_terminating();
                DecodedAnswer::Gone
            }
            Some(pull_assignment_response::Outcome::NotYetReady(nyr)) => {
                DecodedAnswer::NotYetReady(nyr)
            }
            None => {
                self.latch_send();
                DecodedAnswer::Undecodable
            }
        }
    }
}

/// The decoded-outcome witness (bug_089): what an ANSWERED pull
/// resolved to, with the undecodable case a first-class letter.
/// Mintable only through [`MintEvidence::witness_answer`], so holding
/// a value proves the latch decision for it already happened.
#[derive(Debug)]
enum DecodedAnswer {
    /// The dispatch payload — build it.
    Assignment(Box<WorkAssignment>),
    /// No longer wanted (fence-written ahead of the reply).
    Gone,
    /// Wanted but not ready; carries the pacing hint.
    NotYetReady(rio_proto::types::NotYetReady),
    /// Answered, but the oneof decoded to no known variant (version
    /// skew): post-send uncertainty — the latch is already set.
    Undecodable,
}

/// What the pull phase resolved to.
#[derive(Debug)]
pub(super) enum PullPhaseOutcome {
    /// The scheduler delivered the dispatch payload — build it.
    Assigned(Box<WorkAssignment>),
    /// No longer wanted: exit 0 without building (charge-free).
    Gone,
    /// Received only `NotYetReady` for the idle bound: exit 0
    /// charge-free (the OA6 pod-side bounded retry loop) — UNLESS
    /// `maybe_minted` is set (merged_bug_083: an earlier abandoned
    /// send may have minted; the caller confirms before any exit 0).
    IdleExit { maybe_minted: bool },
    /// Shutdown fired while still waiting for work. `maybe_minted` is
    /// the sticky wire-effect latch: false = every pull this process
    /// ever sent was answered (or none was sent) — provably nothing is
    /// held for this pod, exit 0 with zero further RPCs; true = some
    /// pull reached the wire and was never answered (timed out, or
    /// abandoned mid-flight by the shutdown race) — the scheduler MAY
    /// have minted an assignment, so the caller must confirm before it
    /// may exit 0 (merged_bug_270).
    Shutdown { maybe_minted: bool },
    /// The scheduler rejected the pull with a permanent,
    /// non-retryable status (identity/auth rejection, unimplemented
    /// RPC, invalid request): exit nonzero promptly instead of holding
    /// the node for the full `activeDeadlineSeconds`. `maybe_minted`
    /// (merged_bug_145): the rejection answers THIS request only — an
    /// EARLIER abandoned send may still have minted; the caller makes
    /// one best-effort resolution pass before the nonzero exit (the
    /// exit code is the rejection's regardless — resolution only
    /// narrows the orphan window the establishment sweep would
    /// otherwise carry).
    Rejected {
        status: tonic::Status,
        maybe_minted: bool,
    },
}

// r[impl sec.authz.refusal-adjudication]
/// Permanent, non-retryable rejection codes, sourced from the ONE
/// exported adjudication authority (merged_bug_059 — this consumer
/// was the re-point the authority's own AttemptBound doc named but
/// never received): `rio_proto::refusal::judge_refusal` under the
/// ATTEMPT-BOUND regime. The builder's executor token is fixed for
/// the pod's lifetime, so re-presentation is byte-identical and an
/// auth refusal is stable for the whole attempt — the regime split
/// that keeps the store's fresh-mint lanes retrying the same pair.
/// Retrying these holds a node for the full `activeDeadlineSeconds`
/// with no chance of progress; the loops terminate promptly and
/// loudly instead (`builder.pull.retry-loop+2`).
///
/// Exhaustive over [`RefusalJudgment`](rio_proto::refusal::RefusalJudgment)
/// — never `matches!` (which desugars to `_ => false`): a future
/// judgment variant must decide its fatality HERE at compile time,
/// not land in silent-retry. The
/// agreement census (`fatal_set_agrees_with_the_authority`) pins the
/// behavioral set: identical to the pre-re-point hand-coded
/// {PermissionDenied, Unauthenticated, Unimplemented,
/// InvalidArgument} — zero behavior change, asserted cell-by-cell.
fn is_fatal_rejection(code: tonic::Code) -> bool {
    use rio_proto::refusal::{CredentialRegime, RefusalJudgment, judge_refusal};
    match judge_refusal(CredentialRegime::AttemptBound, code) {
        RefusalJudgment::DisprovesRequest => true,
        RefusalJudgment::JudgesPresentation | RefusalJudgment::Undecided => false,
    }
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

/// sh-045: the running-telemetry ticker cadence. 5s (NOT the snapshot
/// loop's 10s) so the worst-case `cpu_seconds` under-read at SIGKILL is
/// ≤5s/60s ≈ 8.3% against `compute_bound_min_wall_secs=60` (a 20s
/// under-read at the old cadence is gate-defeating: a true-0.90 cpu_util
/// reads ≈0.60 and fails the 0.80 threshold). Each tick reads `cpu.stat`
/// FRESH via `final_sample` (HF-1: not the cached snapshot) — eliminates
/// the snapshot-staleness term.
const TELEMETRY_TICK: std::time::Duration = std::time::Duration::from_secs(5);

// r[impl sched.executor.running-telemetry]
/// sh-045: spawn the periodic running-telemetry heartbeat. Each tick
/// reads `(memory.peak, final_sample(cpu.stat fresh))` from the parent
/// cgroup and best-effort ships it via `ReportRunningTelemetry`. RPC
/// error → `debug!` + continue (a dropped heartbeat degrades to the
/// witnessed-axis-only fallback). The returned `JoinHandle` is aborted
/// by the caller after `build_phase_with_abort` returns; the caller
/// also does ONE final fresh-read+ship at that point so the last
/// heartbeat is ≤0s stale at process exit.
fn spawn_running_telemetry_ticker(
    mut client: super::setup::WorkerClient,
    executor_token: String,
    exec_id: String,
    cgroup_parent: std::path::PathBuf,
    resources: crate::cgroup::ResourceSnapshotHandle,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TELEMETRY_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let (peak_mem, ru) = sample_running_telemetry(&cgroup_parent, &resources);
            let req = rio_proto::types::ReportRunningTelemetryRequest {
                exec_id: exec_id.clone(),
                peak_memory_bytes: peak_mem,
                resources: Some(ru),
            };
            if let Err(e) = client
                .report_running_telemetry(authed_request(req, &executor_token))
                .await
            {
                tracing::debug!(error = %e, "ReportRunningTelemetry dropped (best-effort)");
            }
        }
    })
}

/// One fresh `(memory.peak, ResourceUsage)` read from the parent cgroup.
/// `final_sample` reads `cpu.stat` directly (HF-1: NOT the ≤10s-stale
/// cached snapshot); `memory.peak` is the kernel's own running max
/// (one-build-per-pod: the parent cgroup includes small rio-builder
/// overhead, conservative).
fn sample_running_telemetry(
    cgroup_parent: &std::path::Path,
    resources: &crate::cgroup::ResourceSnapshotHandle,
) -> (u64, rio_proto::types::ResourceUsage) {
    let peak_mem = crate::cgroup::read_single_u64(&cgroup_parent.join("memory.peak")).unwrap_or(0);
    let prev = *resources.read().unwrap_or_else(|e| e.into_inner());
    let ru = crate::cgroup::final_sample(cgroup_parent, None, prev);
    (peak_mem, ru)
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
    // The sticky wire-effect latch; the set/clear alphabet is
    // documented at [`MintEvidence`] (the one truth site — use sites
    // doc-link, never restate; merged_bug_012).
    let mut evidence = MintEvidence::default();
    loop {
        if shutdown.is_cancelled() {
            return PullPhaseOutcome::Shutdown {
                maybe_minted: !evidence.may_exit_zero(),
            };
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
        let delay = match bounded_effectful(shutdown, DEFAULT_GRPC_TIMEOUT, transport.pull(req))
            .await
        {
            // Never polled: this request provably did nothing; the
            // latch keeps whatever earlier attempts established.
            EffectfulOutcome::ShutdownBeforeSend => {
                return PullPhaseOutcome::Shutdown {
                    maybe_minted: !evidence.may_exit_zero(),
                };
            }
            // Polled then abandoned: the request may be on the wire.
            EffectfulOutcome::ShutdownAfterSend => {
                evidence.latch_send();
                return PullPhaseOutcome::Shutdown { maybe_minted: true };
            }
            EffectfulOutcome::TimedOut { after } => {
                warn!(
                    after_secs = after.as_secs(),
                    "PullAssignment unanswered; retrying"
                );
                evidence.latch_send();
                attempt = attempt.saturating_add(1);
                RETRY_ENVELOPE.duration(attempt - 1)
            }
            // The answered seam: every Resolved(Ok) flows
            // through the witness constructor — decode and latch
            // decision are one effect (bug_089).
            EffectfulOutcome::Resolved(Ok(resp)) => match evidence.witness_answer(resp.outcome) {
                DecodedAnswer::Assignment(a) => {
                    return PullPhaseOutcome::Assigned(a);
                }
                DecodedAnswer::Gone => {
                    return PullPhaseOutcome::Gone;
                }
                DecodedAnswer::NotYetReady(nyr) => {
                    // A NotYetReady answer is contact with the leader:
                    // reset the unservable backoff curve. It does NOT
                    // clear the wire-effect latch (merged_bug_083: the
                    // answer is authoritative only about attempts held
                    // at ITS answer time — an earlier abandoned send
                    // may still mint; only a loop-terminating answer
                    // or the confirm-only pull clears).
                    attempt = 0;
                    // Proto→sleep seam (merged_bug_156): mint
                    // through the pacing constructor — the hint
                    // is bounded by the domain ceiling BY
                    // CONSTRUCTION, so a skewed producer cannot
                    // park this loop past the idle exit.
                    let suggested = rio_common::clamped::WireSecs::from_wire(u64::from(
                        nyr.retry_after_seconds,
                    ))
                    .pacing(RETRY_AFTER_PACING_CEILING)
                    .unwrap_or(DEFAULT_RETRY_AFTER);
                    // r[impl builder.pull.idle-undroppable]
                    // Errors and empty outcomes between answers are
                    // structurally invisible to the clock — the
                    // pair-discard API no longer exists, so the
                    // armed pair survives to be credited (capped)
                    // here.
                    idle.on_answer(tokio::time::Instant::now(), suggested);
                    if idle.idle_for() >= idle_timeout {
                        return PullPhaseOutcome::IdleExit {
                            maybe_minted: !evidence.may_exit_zero(),
                        };
                    }
                    RETRY_AFTER_JITTER.apply(suggested)
                }
                // An empty oneof from a newer/older server is an
                // ANSWER whose content may be a mint we cannot
                // read — the witness constructor already latched
                // send-uncertainty (the fourth letter); retry
                // like an unservable pull.
                DecodedAnswer::Undecodable => {
                    warn!(
                        "PullAssignment answered with an undecodable outcome \
                             (version skew?); retrying"
                    );
                    attempt = attempt.saturating_add(1);
                    RETRY_ENVELOPE.duration(attempt - 1)
                }
            },
            EffectfulOutcome::Resolved(Err(status)) if is_fatal_rejection(status.code()) => {
                tracing::error!(code = ?status.code(), msg = status.message(),
                    "PullAssignment permanently rejected; exiting instead of holding the node");
                // merged_bug_145: the rejection answered THIS
                // request; earlier abandoned sends keep their
                // wire-effect latch.
                return PullPhaseOutcome::Rejected {
                    status,
                    maybe_minted: !evidence.may_exit_zero(),
                };
            }
            EffectfulOutcome::Resolved(Err(status)) => {
                warn!(code = ?status.code(), msg = status.message(),
                    "PullAssignment unservable; retrying");
                // merged_bug_083 residual (1): a post-send transport
                // error is the same epistemic state as a timeout —
                // the request may have been processed.
                evidence.latch_send();
                attempt = attempt.saturating_add(1);
                RETRY_ENVELOPE.duration(attempt - 1)
            }
        };
        // Sleep, but wake immediately on SIGTERM (nothing is running
        // yet — exiting promptly is strictly better than waiting out a
        // backoff under a deletion grace period).
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return PullPhaseOutcome::Shutdown {
                maybe_minted: !evidence.may_exit_zero(),
            },
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
// r[impl builder.shutdown.sigint+5]
/// AD5 abort phase: wait for the single build's completion, aborting
/// the build if SIGTERM arrives first. The abort is the same
/// cancel-honor path the stream-era `CancelSignal` used (`try_cancel_build`: slot
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

/// The deadline regime a maybe-minted confirm runs under
/// (merged_bug_145): the confirm's patience is a function of WHY the
/// process is exiting, not a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmRegime {
    /// Idle exit: the pod is healthy and in no hurry — the confirm
    /// rides [`RETRY_ENVELOPE`] across transient blips (up to
    /// [`IDLE_CONFIRM_ATTEMPTS`] pulls) so leader churn cannot
    /// convert a charge-free idle exit into a Failed Job. A SIGTERM
    /// during the pacing collapses the remaining budget to one final
    /// attempt (the shutdown semantics take over) — which also keeps
    /// the 600 s Idle REPORT budget safe under a late SIGTERM:
    /// `report_until_acked` collapses to the single bounded SIGTERM
    /// attempt the moment the token fires.
    Idle,
    /// SIGTERM: exactly ONE confirm pull bounded by
    /// `SIGTERM_FINAL_ATTEMPT` — the grace window is burning.
    Shutdown,
}

/// ALL phase budgets of the maybe-minted resolution protocol, as data
/// per regime (bug_068): no phase inside the protocol references a
/// raw grace/budget constant — a third regime or a third phase forces
/// an exhaustive-match budget decision at compile time.
struct ConfirmBudgets {
    /// Confirm pulls the regime may spend (first + retries), paced by
    /// [`RETRY_ENVELOPE`].
    confirm_attempts: u32,
    /// The budget for pushing the synthesized `Cancelled` report
    /// through [`report_until_acked`] when the confirm answers
    /// `Assignment`.
    report_budget: Duration,
}

impl ConfirmRegime {
    /// The regime's total budget table (bug_068: the pre-fix
    /// resolution parameterized only the confirm attempt count and
    /// hardcoded the SIGTERM report slice for BOTH regimes, so a
    /// healthy idle exit's report died inside exactly the leader
    /// churn that set its maybe-minted latch — rio-lease's
    /// `STEAL_AFTER` alone exceeds the 15 s slice).
    const fn budgets(self) -> ConfirmBudgets {
        match self {
            // Healthy pod, no deadline pressure: full attempt count,
            // the same report patience a finished build gets.
            ConfirmRegime::Idle => ConfirmBudgets {
                confirm_attempts: IDLE_CONFIRM_ATTEMPTS,
                report_budget: REPORT_RETRY_BUDGET,
            },
            // The grace window is burning: one confirm pull, the
            // reserved report slice (see the grace-partition asserts
            // at the top of the file).
            ConfirmRegime::Shutdown => ConfirmBudgets {
                confirm_attempts: 1,
                report_budget: PULL_GRACE.report(),
            },
        }
    }
}

// bug_068: the Shutdown row IS the grace partition — tying it to
// PULL_GRACE.report() keeps the merged_bug_270/bug_377 asserts above
// (`report slice covers the SIGTERM final attempt`, `confirm + report
// fit inside termination grace`) sound for the regime table.
const _: () = assert!(
    ConfirmRegime::Shutdown.budgets().report_budget.as_secs() == PULL_GRACE.report().as_secs()
);

/// Confirm pulls the Idle regime may spend (first + retries). The
/// envelope pacing between them is `RETRY_ENVELOPE` — the same curve
/// the live pull loop rides.
const IDLE_CONFIRM_ATTEMPTS: u32 = 3;

/// merged_bug_270: resolve a maybe-minted state before the process
/// may exit 0.
///
/// Confirm pulls bounded by `SIGTERM_FINAL_ATTEMPT` each, attempt
/// count and pacing per [`ConfirmRegime`] (merged_bug_145: the
/// pre-regime single shot applied the dying-pod budget to healthy
/// idle exits). Pull idempotency makes the answer authoritative:
/// - `NotYetReady`/`Gone`: nothing is held for this pod → clean (the
///   timed-out request either never minted or was already released).
///   The scheduler durably FENCES this answer (migration 097): a
///   late abandoned pull can no longer mint after it.
/// - `Assignment`: the abandoned pull DID mint. The build never
///   started; synthesize a `Cancelled` completion and push it through
///   [`report_until_acked`]. Acked → clean; the scheduler closes
///   the attempt charge-free (AD5 semantics, bounded scheduler-side by
///   the worker-abort admission).
/// - Timeout / transport error / empty outcome (after the regime's
///   attempts): UNRESOLVED → the caller exits nonzero so the Job goes
///   Failed and the establishment sweep reaps the open attempt
///   against a Failed pod, not a lying `Succeeded` one.
///
/// Returns true iff the maybe-minted state was resolved clean.
async fn resolve_maybe_minted<T: PullTransport>(
    transport: &mut T,
    intent_id: &str,
    executor_token: &str,
    shutdown: &rio_common::signal::Token,
    regime: ConfirmRegime,
) -> bool {
    // bug_068: the resolution protocol consumes ONLY the regime's
    // budget table — no raw grace/budget const appears in the body.
    // (`SIGTERM_FINAL_ATTEMPT` below is the shared per-RPC unary
    // timeout, not a phase budget — considered and kept as-is.)
    let budgets = regime.budgets();
    let budget = budgets.confirm_attempts;
    let mut attempt: u32 = 0;
    let resp = loop {
        attempt += 1;
        let req = PullAssignmentRequest {
            executor_token: executor_token.to_owned(),
            intent_id: intent_id.to_owned(),
            // merged_bug_083 residual (3): the confirm probe is a READ —
            // the wire discriminator screens the DeliverNew admission, so
            // the probe itself can never mint fresh work for a dying or
            // idle-exiting pod (pre-field it could: kernel
            // `Ready => DeliverNew`).
            confirm_only: true,
            ..Default::default()
        };
        let failure = match tokio::time::timeout(SIGTERM_FINAL_ATTEMPT, transport.pull(req)).await {
            Ok(Ok(resp)) => break resp,
            // bug_153: the fatal arm the two sibling loops always had
            // — a permanent rejection is adjudicated ONCE (the
            // authority-derived set, `is_fatal_rejection`); retrying a
            // byte-identical request under the idle envelope only
            // delays the same nonzero exit and mis-signals the logs.
            // UNRESOLVED posture is unchanged: the caller exits
            // nonzero and the establishment sweep reaps the open
            // attempt against a Failed pod.
            Ok(Err(status)) if is_fatal_rejection(status.code()) => {
                tracing::error!(code = ?status.code(), msg = status.message(),
                    "maybe-minted confirm pull permanently rejected; exiting \
                     nonzero (establishment sweep remains the backstop)");
                return false;
            }
            Ok(Err(status)) => format!("failed: {:?} {}", status.code(), status.message()),
            Err(_elapsed) => "unanswered".to_owned(),
        };
        if attempt >= budget {
            warn!(%failure, attempt, "maybe-minted confirm pull exhausted; exiting nonzero");
            return false;
        }
        // Idle pacing: ride the envelope; a SIGTERM mid-pace hands the
        // last attempt to the shutdown semantics immediately.
        warn!(%failure, attempt, "maybe-minted confirm pull failed; retrying under the idle envelope");
        tokio::select! {
            _ = tokio::time::sleep(RETRY_ENVELOPE.duration(attempt - 1)) => {}
            _ = shutdown.cancelled() => {
                if attempt + 1 < budget {
                    // One final attempt, then the regime is spent.
                    attempt = budget - 1;
                }
            }
        }
    };
    match resp.outcome {
        Some(pull_assignment_response::Outcome::NotYetReady(_))
        | Some(pull_assignment_response::Outcome::Gone(_)) => {
            info!("maybe-minted shutdown resolved: nothing held for this pod");
            true
        }
        Some(pull_assignment_response::Outcome::Assignment(a)) => {
            info!(
                drv_path = %a.drv_path,
                exec_id = %a.exec_id,
                "abandoned pull HAD minted; reporting synthesized Cancelled"
            );
            let report = CompletionReport {
                drv_path: a.drv_path.clone(),
                result: Some(rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Cancelled.into(),
                    error_msg: "SIGTERM before build start; assignment minted by an abandoned pull (merged_bug_270 confirm path)"
                        .into(),
                    ..Default::default()
                }),
                assignment_token: a.assignment_token.clone(),
                ..Default::default()
            };
            report_until_acked(
                transport,
                &a.exec_id,
                report,
                budgets.report_budget,
                shutdown,
            )
            .await
        }
        None => {
            // Version skew, not a transient: the RPC was ANSWERED with
            // an oneof this build cannot decode — the regime's retries
            // would re-fetch the same skew.
            warn!("maybe-minted confirm pull returned an empty outcome; exiting nonzero");
            false
        }
    }
}

/// Every way `run_pull` ends, as data (merged_bug_011 keystone 1,
/// builder half): the closed exit alphabet. A future "nothing held"
/// answer arm cannot license exit 0 without adding a variant here and
/// deciding its license in [`finish`] — the exhaustive match is the
/// machine witness for "every exit path decided its exit code".
#[derive(Debug)]
enum ExitDisposition {
    /// The scheduler answered `Gone`: nothing wanted, exit 0
    /// charge-free. Sound because the scheduler durably fences every
    /// keyed Gone BEFORE answering (`sched.executor.confirm-fence`) —
    /// a straggler pull after this exit is screened, never minted.
    GoneAnswered,
    /// Only `NotYetReady` for the idle bound with a clean send
    /// history: exit 0 charge-free (the OA6 bounded idle exit).
    IdleClean,
    /// Shutdown while waiting with a provably-unminted send history:
    /// exit 0, zero further RPCs (the named fourth exit-0 case).
    ShutdownClean,
    /// A maybe-minted history was resolved clean by the confirm
    /// protocol (nothing held, or the synthesized Cancelled report
    /// was acked): exit 0.
    ConfirmResolvedClean,
    /// The build ran and its `CompletionReport` was acknowledged:
    /// exit 0 — the scheduler committed the attempt row before
    /// answering.
    ReportAcked,
    /// The exit-0 license could not be established (unresolved
    /// confirm, unacked report, internal wiring failure): exit
    /// nonzero so the Job goes Failed and the establishment sweep
    /// reaps the open attempt against a Failed pod, never a lying
    /// `Succeeded` one.
    Unresolved(String),
    /// The scheduler permanently rejected the pull
    /// (identity/auth/unimplemented/invalid): exit nonzero promptly
    /// instead of holding the node for the full
    /// `activeDeadlineSeconds`.
    Rejected(tonic::Status),
}

/// THE one exit chokepoint of [`run_pull`]: teardown, then the
/// per-variant exit-code decision. House policy (machine-checked by
/// the commit-body grep, re-assertable any time): zero `return
/// Ok(())` / `bail!` in `run_pull` outside this function — every exit
/// names its [`ExitDisposition`] variant.
fn finish(rt: BuilderRuntime, disposition: ExitDisposition) -> anyhow::Result<()> {
    run_teardown(rt);
    match disposition {
        // The five licensed clean exits — each variant's license
        // rationale lives on the variant doc; a new variant lands
        // RED here until this match decides it.
        ExitDisposition::GoneAnswered
        | ExitDisposition::IdleClean
        | ExitDisposition::ShutdownClean
        | ExitDisposition::ConfirmResolvedClean
        | ExitDisposition::ReportAcked => Ok(()),
        ExitDisposition::Unresolved(why) => Err(anyhow::anyhow!(why)),
        ExitDisposition::Rejected(status) => Err(anyhow::anyhow!(
            "PullAssignment permanently rejected ({:?}): {}",
            status.code(),
            status.message()
        )),
    }
}

/// The pull lifecycle: pull → build (existing machinery) → report →
/// exit. The builder runtime's only delivery path; every exit routes
/// through [`finish`].
// r[impl builder.pull.exit-codes+1]
// r[impl sched.executor.one-shot+2]
pub(super) async fn run_pull(mut rt: BuilderRuntime) -> anyhow::Result<()> {
    if rt.intent_id.is_empty() {
        return finish(
            rt,
            ExitDisposition::Unresolved(
                "the pull runtime requires RIO_INTENT_ID (the controller injects it at Job spawn)"
                    .into(),
            ),
        );
    }
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
            return finish(rt, ExitDisposition::GoneAnswered);
        }
        PullPhaseOutcome::IdleExit {
            maybe_minted: false,
        } => {
            info!(
                intent_id = %rt.intent_id,
                idle_secs = rt.idle_timeout.as_secs(),
                "only NotYetReady for the idle bound; exiting 0 (charge-free)"
            );
            return finish(rt, ExitDisposition::IdleClean);
        }
        PullPhaseOutcome::IdleExit { maybe_minted: true } => {
            // merged_bug_083: an exit-0 with unconfirmed sends in the
            // history must CONFIRM first — same resolution protocol as
            // the SIGTERM path (the confirm-only pull cannot mint).
            info!(
                intent_id = %rt.intent_id,
                "idle bound reached with unconfirmed sends; confirming before exit"
            );
            let resolved = resolve_maybe_minted(
                &mut transport,
                &rt.intent_id,
                &executor_token,
                &rt.shutdown,
                ConfirmRegime::Idle,
            )
            .await;
            return finish(
                rt,
                if resolved {
                    ExitDisposition::ConfirmResolvedClean
                } else {
                    ExitDisposition::Unresolved(
                        "idle-exit confirm could not resolve the maybe-minted state; exiting \
                         nonzero so the establishment sweep reaps against a Failed pod"
                            .into(),
                    )
                },
            );
        }
        PullPhaseOutcome::Shutdown {
            maybe_minted: false,
        } => {
            // Provably nothing minted (builder.pull.exit-codes+1).
            info!(intent_id = %rt.intent_id, "shutdown while waiting for work; exiting 0");
            return finish(rt, ExitDisposition::ShutdownClean);
        }
        PullPhaseOutcome::Shutdown { maybe_minted: true } => {
            let resolved = resolve_maybe_minted(
                &mut transport,
                &rt.intent_id,
                &executor_token,
                &rt.shutdown,
                ConfirmRegime::Shutdown,
            )
            .await;
            return finish(
                rt,
                if resolved {
                    ExitDisposition::ConfirmResolvedClean
                } else {
                    ExitDisposition::Unresolved(
                        "shutdown with a maybe-minted pull left unresolved; exiting nonzero so \
                         the Job goes Failed (the establishment sweep reaps the attempt)"
                            .into(),
                    )
                },
            );
        }
        PullPhaseOutcome::Rejected {
            status,
            maybe_minted,
        } => {
            // Permanent rejection: exit nonzero promptly so the Job
            // goes Failed and the pod-terminal path (charge-free, no
            // attempt row) surfaces the misconfiguration instead of
            // the node idling until activeDeadlineSeconds.
            // merged_bug_145: an EARLIER abandoned send may have
            // minted — one best-effort resolution narrows the orphan
            // window (an InvalidArgument rejection does not predict
            // the confirm's fate; an auth rejection makes it fail
            // fast). The exit stays nonzero either way: the rejection
            // is the exit cause.
            if maybe_minted {
                let resolved = resolve_maybe_minted(
                    &mut transport,
                    &rt.intent_id,
                    &executor_token,
                    &rt.shutdown,
                    ConfirmRegime::Shutdown,
                )
                .await;
                info!(resolved, "rejected-exit maybe-minted resolution attempted");
            }
            return finish(rt, ExitDisposition::Rejected(status));
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
        // fail loudly rather than build twice. (Routing through
        // finish adds an orderly teardown this documented-unreachable
        // arm previously skipped — disclosed micro-delta; the exit
        // code is unchanged.)
        return finish(
            rt,
            ExitDisposition::Unresolved("build slot unexpectedly busy at first pull".into()),
        );
    };
    spawn_build_task(*assignment, guard, &rt.build_ctx).await;

    // sh-045: the running-telemetry heartbeat. Spawned AFTER the build
    // task (so the cgroup is populated) with the SAME authed transport
    // as the pull/report unaries; aborted after `build_phase_with_abort`
    // returns (ticker lifetime is bounded by the assignment).
    let telemetry_ticker = spawn_running_telemetry_ticker(
        rt.scheduler_client.clone(),
        executor_token.clone(),
        exec_id.clone(),
        rt.build_ctx.cgroup_parent.clone(),
        rt.build_ctx.resources.clone(),
    );

    let mut sink_rx = rt
        .pull_sink_rx
        .take()
        .expect("pull mode always carries the sink receiver");
    let completion = build_phase_with_abort(&rt.slot, &drv_path, &mut sink_rx, &rt.shutdown).await;
    // One final fresh-read + ship BEFORE aborting the ticker, so the
    // last heartbeat is ≤0s stale at process exit (not ≤5s).
    {
        let (peak_mem, ru) =
            sample_running_telemetry(&rt.build_ctx.cgroup_parent, &rt.build_ctx.resources);
        let req = rio_proto::types::ReportRunningTelemetryRequest {
            exec_id: exec_id.clone(),
            peak_memory_bytes: peak_mem,
            resources: Some(ru),
        };
        if let Err(e) = rt
            .scheduler_client
            .clone()
            .report_running_telemetry(authed_request(req, &executor_token))
            .await
        {
            tracing::debug!(error = %e, "final ReportRunningTelemetry dropped (best-effort)");
        }
    }
    telemetry_ticker.abort();
    let Some(completion) = completion else {
        // Cannot happen while build_ctx holds a sender; treat as a
        // builder bug and let the pod-terminal path classify it.
        // (Same teardown micro-delta as the slot arm above.)
        return finish(
            rt,
            ExitDisposition::Unresolved(
                "completion channel closed before the build reported".into(),
            ),
        );
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
    if acked {
        info!(exec_id = %exec_id, "outcome reported and acknowledged; exiting 0");
        finish(rt, ExitDisposition::ReportAcked)
    } else {
        // Nonzero exit → Job goes Failed → the controller's
        // pod-terminal path reports it; the scheduler's establishment
        // sweep is the final backstop.
        finish(
            rt,
            ExitDisposition::Unresolved(
                "ReportOutcome was never acknowledged; exiting nonzero".into(),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // r[verify sec.authz.refusal-adjudication]
    /// The agreement census (merged_bug_059): the builder's fatal set,
    /// now derived from the authority, equals the pre-re-point
    /// hand-coded set on EVERY tonic code — zero behavior change is
    /// asserted by the census itself, cell-by-cell over all 17 codes,
    /// with the expectation table hand-transcribed (the independent
    /// oracle), never computed from the implementation.
    #[test]
    fn fatal_set_agrees_with_the_authority() {
        use tonic::Code;
        // The pre-re-point hand-coded set, transcribed verbatim from
        // the replaced matches! body.
        let hand_coded = [
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::Unimplemented,
            Code::InvalidArgument,
        ];
        let all = [
            Code::Ok,
            Code::Cancelled,
            Code::Unknown,
            Code::InvalidArgument,
            Code::DeadlineExceeded,
            Code::NotFound,
            Code::AlreadyExists,
            Code::PermissionDenied,
            Code::ResourceExhausted,
            Code::FailedPrecondition,
            Code::Aborted,
            Code::OutOfRange,
            Code::Unimplemented,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
            Code::Unauthenticated,
        ];
        assert_eq!(all.len(), 17, "the tonic code census is total");
        for code in all {
            assert_eq!(
                is_fatal_rejection(code),
                hand_coded.contains(&code),
                "code {code:?}: the authority-derived fatal set must \
                 agree with the pre-re-point hand-coded set"
            );
        }
    }

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

    /// merged_bug_270 (R7 quadruple). Pre-fix red recorded by the
    /// probe this battery superseded: a SENT pull abandoned by SIGTERM
    /// returned plain `Shutdown` with zero follow-up RPCs and run_pull
    /// exited 0 (the forbidden fourth exit-0 case before the law named
    /// it confirm-then-{0|nonzero}).
    ///
    /// (1) shutdown mid-flight (sent, unanswered) → maybe_minted.
    // r[verify builder.pull.exit-codes+1]
    // r[verify builder.shutdown.sigint+5]
    #[tokio::test(start_paused = true)]
    async fn shutdown_mid_flight_is_maybe_minted() {
        struct Hang;
        impl PullTransport for Hang {
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                std::future::pending().await
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
        }
        let mut t = Hang;
        let shutdown = token();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            sd.cancel();
        });
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(matches!(
            outcome,
            PullPhaseOutcome::Shutdown { maybe_minted: true }
        ));
    }

    /// merged_bug_083 residuals (1)+(2): the wire-effect latch is
    /// TOTAL over post-send uncertainty. A non-fatal transport error
    /// after send is the same epistemic state as a timeout (the
    /// request may have been processed; transport.rs documents
    /// Resolved as "answer OR transport error"), so it LATCHES; and an
    /// interleaved NotYetReady to a LATER request is not proof the
    /// EARLIER abandoned request never minted (no requester-liveness
    /// gate exists on the durable mint), so it must NOT clear the
    /// latch. Pre-fix: the error arm never latched and NotYetReady
    /// cleared -- a SIGTERM after [err, nyr] exited 0 with a possibly
    /// durable open attempt (the Succeeded-Job-with-charged-crash
    /// state).
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn post_send_error_then_nyr_keeps_the_latch() {
        let mut t = ScriptedTransport::new(
            vec![
                // Post-send transport fault: epistemically maybe-sent.
                Err(tonic::Status::unknown("h2 connection reset post-send")),
                // A LATER request answered NotYetReady: authoritative
                // only about attempts held at ITS answer time.
                Ok(not_yet_ready_resp(1)),
            ],
            vec![],
        );
        let shutdown = token();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            // Past the error backoff and the NYR retry_after.
            tokio::time::sleep(Duration::from_secs(40)).await;
            sd.cancel();
        });
        let outcome = pull_until_resolved(&mut t, "intent-mm", "tok", IDLE, &shutdown).await;
        assert!(
            matches!(outcome, PullPhaseOutcome::Shutdown { maybe_minted: true }),
            "post-send error latches and an interleaved NotYetReady cannot launder it, got {outcome:?}"
        );
    }

    /// (2) timed-out pull, then SIGTERM in the backoff → maybe_minted
    /// (the sticky latch survives the non-answer gap).
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn timeout_then_sigterm_in_backoff_is_maybe_minted() {
        struct HangForever;
        impl PullTransport for HangForever {
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                std::future::pending().await
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
        }
        let mut t = HangForever;
        let shutdown = token();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            // Past the pull bound (timeout latches maybe_minted) and a
            // beat into the unservable backoff.
            tokio::time::sleep(DEFAULT_GRPC_TIMEOUT + Duration::from_millis(50)).await;
            sd.cancel();
        });
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(matches!(
            outcome,
            PullPhaseOutcome::Shutdown { maybe_minted: true }
        ));
    }

    /// W10-BK (bug_089): the FOURTH uncertainty letter. An ANSWERED
    /// pull whose oneof decoded to None (an unknown variant from a
    /// newer/older server) is post-send uncertainty — the scheduler
    /// may have minted under the new variant. Pre-fix the None arm
    /// was the unique post-send arm that never latched, so persistent
    /// version skew + SIGTERM routed Shutdown{maybe_minted:false} →
    /// exit 0 over a possibly-minted attempt — the exact evidence
    /// resolve_maybe_minted's None arm refuses exit 0 for.
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn undecodable_answer_then_sigterm_is_maybe_minted() {
        let mut t =
            ScriptedTransport::new(vec![Ok(PullAssignmentResponse { outcome: None })], vec![]);
        let shutdown = token();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            // Past several answered-undecodable responses, into a
            // retry backoff.
            tokio::time::sleep(Duration::from_secs(40)).await;
            sd.cancel();
        });
        let outcome = pull_until_resolved(&mut t, "intent-skew", "tok", IDLE, &shutdown).await;
        assert!(
            matches!(outcome, PullPhaseOutcome::Shutdown { maybe_minted: true }),
            "an answered-undecodable outcome is post-send uncertainty \
             and must latch; got {outcome:?}"
        );
    }

    /// W10-BK (the all-arms quantifier): the answered-seam product —
    /// every decoded letter × every prior latch state, with the
    /// expected latch effect per cell. The witness consumption below
    /// is an EXHAUSTIVE match, so a new oneof variant cannot land
    /// without (a) a `witness_answer` arm (its match over the wire
    /// alphabet is exhaustive) and (b) a row here.
    #[test]
    fn answered_seam_latch_product_is_total() {
        use pull_assignment_response::Outcome;
        for pre_latched in [false, true] {
            // (cell name, answer, expected latch AFTER the witness)
            let cells: [(&str, Option<Outcome>, bool); 4] = [
                (
                    "assignment-clears",
                    Some(Outcome::Assignment(WorkAssignment::default())),
                    false,
                ),
                (
                    "gone-clears",
                    Some(Outcome::Gone(rio_proto::types::Gone {})),
                    false,
                ),
                (
                    "nyr-preserves",
                    Some(Outcome::NotYetReady(rio_proto::types::NotYetReady {
                        retry_after_seconds: 1,
                    })),
                    pre_latched,
                ),
                ("undecodable-latches", None, true),
            ];
            for (name, outcome, expect_latched) in cells {
                let mut ev = MintEvidence::default();
                if pre_latched {
                    ev.latch_send();
                }
                let witness = ev.witness_answer(outcome);
                assert_eq!(
                    !ev.may_exit_zero(),
                    expect_latched,
                    "cell {name} (pre_latched={pre_latched}): latch effect wrong"
                );
                // Exhaustive consumption: a new witness letter fails
                // compilation at this match.
                match witness {
                    DecodedAnswer::Assignment(_)
                    | DecodedAnswer::Gone
                    | DecodedAnswer::NotYetReady(_)
                    | DecodedAnswer::Undecodable => {}
                }
            }
        }
    }

    /// (3) shutdown before any pull was sent → provably nothing minted.
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn shutdown_before_send_is_clean() {
        let mut t = ScriptedTransport::new(vec![], vec![]);
        let shutdown = token();
        shutdown.cancel();
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(matches!(
            outcome,
            PullPhaseOutcome::Shutdown {
                maybe_minted: false
            }
        ));
        assert_eq!(t.pull_calls, 0, "nothing may be sent after shutdown");
    }

    /// (4) RE-AIMED by merged_bug_083: a timed-out pull followed by a
    /// NotYetReady answer to a LATER request keeps the latch — the
    /// signed rule text defines maybe-minted PER PULL ("a pull that
    /// reached the wire and was never answered"), and pull #1 was
    /// never answered; pull #2's answer is authoritative only about
    /// holdings at ITS answer time (no requester-liveness gate exists
    /// on the durable mint). The pre-fix clear was the laundering this
    /// test used to assert; exit 0 is still reached, via the blessed
    /// bounded confirm pull instead of by assumption.
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn answer_after_timeout_keeps_the_latch() {
        struct HangThenAnswer {
            calls: u32,
        }
        impl PullTransport for HangThenAnswer {
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                self.calls += 1;
                if self.calls == 1 {
                    std::future::pending().await
                } else {
                    Ok(not_yet_ready_resp(600))
                }
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
        }
        let mut t = HangThenAnswer { calls: 0 };
        let shutdown = token();
        let sd = shutdown.clone();
        tokio::spawn(async move {
            // Past the first pull's timeout + its backoff + the second
            // pull's answer, into the NotYetReady-paced sleep.
            tokio::time::sleep(DEFAULT_GRPC_TIMEOUT + Duration::from_secs(40)).await;
            sd.cancel();
        });
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(
            matches!(outcome, PullPhaseOutcome::Shutdown { maybe_minted: true }),
            "the timed-out pull was never answered -- an interleaved answer to a \
             later request must not launder it, got {outcome:?}"
        );
    }

    /// merged_bug_145 (banner a): every PullPhaseOutcome variant
    /// names its mint evidence — the exhaustive match (no wildcard)
    /// is the machine witness: a future variant cannot compile
    /// without taking a position on whether an unconfirmed send may
    /// be behind it.
    fn mint_evidence(o: &PullPhaseOutcome) -> Option<bool> {
        match o {
            // The mint itself rides the variant — the build phase
            // consumes it.
            PullPhaseOutcome::Assigned(_) => None,
            // Authoritative not-wanted answer for the intent: any
            // straggler mint reports into a Gone consumption.
            PullPhaseOutcome::Gone => None,
            PullPhaseOutcome::IdleExit { maybe_minted }
            | PullPhaseOutcome::Shutdown { maybe_minted }
            | PullPhaseOutcome::Rejected { maybe_minted, .. } => Some(*maybe_minted),
        }
    }

    /// merged_bug_145: a fatal rejection after an earlier abandoned
    /// (timed-out) send carries maybe_minted=true — pre-fix the
    /// variant could not express it (compile-level red, the
    /// result.rs:447 precedent) and the orphan waited for the
    /// establishment sweep.
    #[tokio::test(start_paused = true)]
    async fn rejection_after_abandoned_send_carries_evidence() {
        let shutdown = token();
        // First pull times out (the send is abandoned mid-flight);
        // the second is permanently rejected.
        let mut t = ScriptedTransport::new(
            vec![
                Err(tonic::Status::deadline_exceeded("pull timed out")),
                Err(tonic::Status::permission_denied("token mismatch")),
            ],
            vec![],
        );
        let outcome =
            pull_until_resolved(&mut t, "intent", "tok", Duration::from_secs(300), &shutdown).await;
        match &outcome {
            PullPhaseOutcome::Rejected {
                status,
                maybe_minted,
            } => {
                assert_eq!(status.code(), tonic::Code::PermissionDenied);
                assert!(
                    *maybe_minted,
                    "the abandoned first send must keep its wire-effect latch"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(mint_evidence(&outcome), Some(true));
    }

    /// merged_bug_145 (bughunt-4): the IDLE-EXIT confirm rides the
    /// retry envelope — the pod is healthy and in no hurry; a single
    /// transient blip (leader churn) must not convert a charge-free
    /// idle exit into a Failed Job. (The SIGTERM regime keeps its
    /// single bounded attempt.)
    #[tokio::test(start_paused = true)]
    async fn idle_confirm_retries_transient_failure() {
        let shutdown = token(); // NOT fired: idle exit, healthy pod.
        let mut t = ScriptedTransport::new(
            vec![
                Err(tonic::Status::unavailable("leader churn")),
                Ok(not_yet_ready_resp(5)),
            ],
            vec![],
        );
        assert!(
            resolve_maybe_minted(&mut t, "i", "tok", &shutdown, ConfirmRegime::Idle).await,
            "a healthy idle-exiting pod must ride the retry envelope over a \
             transient confirm blip"
        );
        assert_eq!((t.pull_calls, t.report_calls), (2, 0));
    }

    /// bug_068: the Idle regime's Assignment arm pushes the
    /// synthesized Cancelled report through the regime's OWN report
    /// budget — the healthy 600 s `REPORT_RETRY_BUDGET`, not the
    /// burning-grace SIGTERM slice (15 s). The trigger is exactly the
    /// leader churn that set the maybe_minted latch: rio-lease's
    /// `STEAL_AFTER` alone exceeds 15 s, so the pre-fix Idle pod's
    /// report died inside the failover it was idling through →
    /// `IdleExit{maybe_minted:true}` bailed nonzero → Failed Job →
    /// establishment-sweep charge, violating the Idle regime's own
    /// "leader churn cannot convert a charge-free idle exit into a
    /// Failed Job" guarantee.
    ///
    /// Ten unavailable answers ride the retry envelope (nominal 181 s
    /// of virtual pacing — full jitter draws below 15 s across ten
    /// delays are practically impossible, so the pre-fix budget
    /// expires deterministically) before the ack lands; the healthy
    /// budget never expires (jittered sum ≤ 181 s < 600 s).
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn idle_confirm_assignment_report_rides_the_full_report_budget() {
        let shutdown = token(); // NOT fired: idle exit, healthy pod.
        let mut reports: Vec<Result<(), tonic::Status>> = (0..10)
            .map(|i| Err(tonic::Status::unavailable(format!("leader churn {i}"))))
            .collect();
        reports.push(Ok(()));
        let mut t = ScriptedTransport::new(vec![Ok(assignment_resp("exec-idle"))], reports);
        assert!(
            resolve_maybe_minted(&mut t, "i", "tok", &shutdown, ConfirmRegime::Idle).await,
            "a healthy idle-exiting pod's synthesized Cancelled report must ride the \
             full healthy report budget across leader churn (pre-fix: the hardcoded \
             15 s SIGTERM slice expired mid-churn and the pod exited nonzero)"
        );
        assert_eq!((t.pull_calls, t.report_calls), (1, 11));
    }

    /// resolve_shutdown: confirm answers map to the law — NotYetReady →
    /// clean; minted Assignment → synthesized Cancelled acked → clean;
    /// confirm failure → unresolved (nonzero at the caller).
    // r[verify builder.pull.exit-codes+1]
    // r[verify builder.shutdown.sigint+5]
    #[tokio::test(start_paused = true)]
    async fn resolve_shutdown_confirm_table() {
        let shutdown = token();
        shutdown.cancel();
        // NotYetReady → clean, exactly one RPC.
        let mut t = ScriptedTransport::new(vec![Ok(not_yet_ready_resp(5))], vec![]);
        assert!(resolve_maybe_minted(&mut t, "i", "tok", &shutdown, ConfirmRegime::Shutdown).await);
        assert_eq!((t.pull_calls, t.report_calls), (1, 0));
        // Minted → synthesized Cancelled, acked → clean.
        let mut t = ScriptedTransport::new(vec![Ok(assignment_resp("exec-9"))], vec![Ok(())]);
        assert!(resolve_maybe_minted(&mut t, "i", "tok", &shutdown, ConfirmRegime::Shutdown).await);
        assert_eq!((t.pull_calls, t.report_calls), (1, 1));
        // Confirm pull errors → unresolved.
        let mut t = ScriptedTransport::new(vec![Err(tonic::Status::unavailable("gone"))], vec![]);
        assert!(
            !resolve_maybe_minted(&mut t, "i", "tok", &shutdown, ConfirmRegime::Shutdown).await
        );
        // Minted but the report is never acked → unresolved.
        let mut t = ScriptedTransport::new(
            vec![Ok(assignment_resp("exec-9"))],
            vec![Err(tonic::Status::unavailable("no leader"))],
        );
        assert!(
            !resolve_maybe_minted(&mut t, "i", "tok", &shutdown, ConfirmRegime::Shutdown).await
        );
    }

    /// (b) `Gone` resolves immediately: exit 0 without building, no
    /// further pulls.
    // r[verify builder.pull.exit-codes+1]
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
    // r[verify builder.pull.exit-codes+1]
    #[tokio::test(start_paused = true)]
    async fn not_yet_ready_repulls_and_idle_bounds() {
        let mut t = ScriptedTransport::new(vec![Ok(not_yet_ready_resp(5))], vec![]);
        let shutdown = token();
        let started = tokio::time::Instant::now();
        let outcome = pull_until_resolved(&mut t, "intent-a", "tok", IDLE, &shutdown).await;
        assert!(
            matches!(
                outcome,
                PullPhaseOutcome::IdleExit {
                    maybe_minted: false
                }
            ),
            "pure answered NYRs: idle exit is provably clean, got {outcome:?}"
        );
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
    // r[verify builder.pull.exit-codes+1]
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
    // r[verify builder.pull.exit-codes+1]
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
                PullPhaseOutcome::Rejected { status: s, .. } => assert_eq!(s.code(), code),
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

    /// **W12-N (bug_153)** — *proposition: a permanent rejection is
    /// adjudicated once, identically, in EVERY consulting loop — quantifier: census(transport_retry_loop_census_consults_the_fatal_authority) — the
    /// maybe-minted resolve included; population: the [GEN-SET]
    /// transport-loop census below.* Pre-fix the Ok(Err) arm never
    /// consulted `is_fatal_rejection`, so permanent rejections
    /// (Unauthenticated/PermissionDenied/Unimplemented/
    /// InvalidArgument) were re-presented byte-identically up to the
    /// idle budget (3) with "retrying" warns while both siblings
    /// fast-exit on the same authority. Bounded impact (identical
    /// terminal disposition, ~2 extra envelope sleeps, mis-signaled
    /// logs) — the close is the sibling-identical fatal arm + the
    /// loop-set census (the full transport-seam hoist recorded
    /// REJECTED for this round: a transport-layer refactor out of
    /// proportion to a low; the census closes the class).
    // r[verify builder.pull.retry-loop+2]
    #[tokio::test(start_paused = true)]
    async fn maybe_minted_fatal_rejection_resolves_after_one_call() {
        for status in [
            tonic::Status::permission_denied("token bound to a different intent"),
            tonic::Status::unauthenticated("executor token expired"),
            tonic::Status::unimplemented("PullAssignment not served"),
            tonic::Status::invalid_argument("intent_id is required"),
        ] {
            let code = status.code();
            let mut t = ScriptedTransport::new(vec![Err(status)], vec![]);
            let shutdown = token();
            let resolved =
                resolve_maybe_minted(&mut t, "intent-a", "tok", &shutdown, ConfirmRegime::Idle)
                    .await;
            assert!(
                !resolved,
                "a permanently rejected confirm is UNRESOLVED (nonzero \
                 exit; the establishment sweep reaps against a Failed \
                 pod) for {code:?}"
            );
            assert_eq!(
                t.pull_calls, 1,
                "left (pre-fix): the maybe-minted loop re-presented the \
                 permanent rejection byte-identically under the idle \
                 envelope (3 calls, retry warns) / right: adjudicated \
                 ONCE, sibling-identical with pull_until_resolved and \
                 report_until_acked ({code:?})"
            );
        }
    }

    /// W12-N's census ([GEN-SET], the bug_153 class-closer): the SET
    /// of transport-consuming retry loops is DERIVED from this file
    /// (the `PullTransport` trait is `pub(super)` — every consumer is
    /// structurally in-file; widening the trait is a reviewable
    /// event), and EVERY derived member — quantifier: census(transport_retry_loop_census_consults_the_fatal_authority) — must consult the fatal
    /// authority — a fourth loop born without the consult reds here
    /// (per-call-site opt-in was the enumeration trap; the fatal SET
    /// itself is authority-derived and pinned by
    /// `fatal_set_agrees_with_the_authority`).
    ///
    /// POPULATION face (census riders (a)): the universe is the
    /// compile-embedded self (resolve face compile-discharged), the
    /// floor is the three expected members VERIFIED in the derived
    /// set, and the walk's non-vacuity is asserted. Plant battery
    /// (riders (b)): the strawman below drives an unconsulting loop
    /// through the same walk (enrollment face); the walk derives
    /// membership from the transport-call grammar, not a name list
    /// (jurisdiction face: `PullTransport` is `pub(super)`, so the
    /// scanned universe is every `runtime/` module file — a sibling
    /// could host a transport loop and the trait's visibility would
    /// admit it); aliased transport bindings still contain the dotted
    /// call text (the receiver name is not anchored — overscan
    /// posture).
    #[test]
    fn transport_retry_loop_census_consults_the_fatal_authority() {
        const MODULE_SRC: &[(&str, &str)] = &[
            ("pull.rs", include_str!("pull.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("idle.rs", include_str!("idle.rs")),
            ("result.rs", include_str!("result.rs")),
            ("setup.rs", include_str!("setup.rs")),
            ("slot.rs", include_str!("slot.rs")),
        ];
        let src: String = MODULE_SRC
            .iter()
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join("\n");
        let members = derive_transport_loops(&src);
        assert!(
            members.len() >= 3,
            "VACUOUS WALK: expected at least the three known loops, \
             derived {}",
            members.len()
        );
        for expected in [
            "pull_until_resolved",
            "report_until_acked",
            "resolve_maybe_minted",
        ] {
            assert!(
                members.iter().any(|(name, _)| name == expected),
                "expected member {expected} missing from the derived \
                 loop set (the walk grammar broke)"
            );
        }
        for (name, body) in &members {
            assert!(
                body.contains(concat!("is_fatal_", "rejection(")),
                "transport loop `{name}` never consults the fatal \
                 authority — the bug_153 shape (adjudicate the \
                 rejection once, identically, in every loop)"
            );
        }
    }

    /// The census's planted red (enrollment face): a NEW
    /// transport-consuming loop WITHOUT the fatal consult, driven
    /// through the same walk, MUST be derived and MUST fail the
    /// consult law NAMING the loop.
    #[test]
    fn loop_census_plants_red_on_unconsulting_loop() {
        let strawman = format!(
            "async fn rogue_confirm<T: PullTransport>(t: &mut T) {{\n\
             loop {{\n\
             let _ = t{}(req).await;\n\
             }}\n\
             }}\n",
            ".pull"
        );
        // The walk needs the transport-call grammar: receiver-dotted
        // pull/report calls. Use the production grammar form.
        let planted = format!(
            "async fn rogue_confirm<T: PullTransport>(t: &mut T) {{\n\
             loop {{\n\
             let _ = transport{}(req).await;\n\
             }}\n\
             }}\n",
            ".pull"
        );
        let _ = strawman;
        let members = derive_transport_loops(&planted);
        assert_eq!(
            members.len(),
            1,
            "plant premise: the walk derives the rogue loop"
        );
        let (name, body) = &members[0];
        assert_eq!(name, "rogue_confirm");
        assert!(
            !body.contains(concat!("is_fatal_", "rejection(")),
            "plant premise: the rogue loop never consults"
        );
        // The production law (the same predicate the census asserts)
        // refuses this member.
        let consults = body.contains(concat!("is_fatal_", "rejection("));
        assert!(!consults, "the plant is the state the census refuses");
        // Empty-walk red (rider (a)): the same walk on an empty
        // universe derives nothing — the floor's refused state.
        assert!(derive_transport_loops("").is_empty());
    }

    /// The walk: column-0 `async fn` heads (pub or private); a body
    /// ends at the first column-0 `}` (rust item close), so test-
    /// module text never attributes to the last production fn.
    /// Membership = the body consumes the transport (a dotted
    /// `.pull(`/`.report(` call on any receiver — overscan: aliasing
    /// the transport binding does not change the call text).
    fn derive_transport_loops(src: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut current: Option<(String, String)> = None;
        for line in src.lines() {
            let head = line
                .strip_prefix("pub(super) async fn ")
                .or_else(|| line.strip_prefix("pub async fn "))
                .or_else(|| line.strip_prefix("async fn "));
            if let Some(rest) = head {
                if let Some((name, body)) = current.take() {
                    out.push((name, body));
                }
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                current = Some((name, String::new()));
                continue;
            }
            if line == "}" {
                if let Some((name, body)) = current.take() {
                    out.push((name, body));
                }
                continue;
            }
            if let Some((_, body)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
        if let Some((name, body)) = current.take() {
            out.push((name, body));
        }
        out.into_iter()
            .filter(|(_, body)| {
                body.contains(concat!("transport", ".pull("))
                    || body.contains(concat!("transport", ".report("))
                    || body.contains(concat!(".pull", "(req)"))
                    || body.contains(concat!(".report", "(req)"))
            })
            .collect()
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
        // merged_bug_270 re-pin: the abandoned pull was SENT — the
        // outcome must carry the maybe-minted latch.
        assert!(matches!(
            outcome,
            PullPhaseOutcome::Shutdown { maybe_minted: true }
        ));
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
    // r[verify builder.pull.exit-codes+1]
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
        assert!(
            matches!(outcome, PullPhaseOutcome::IdleExit { maybe_minted: true }),
            "the 14 post-send transport errors are unconfirmed sends — the idle \
             exit must carry the latch into the confirm pass, got {outcome:?}"
        );
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
    // r[verify builder.shutdown.sigint+5]
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
    // r[verify builder.shutdown.sigint+5]
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

#[cfg(test)]
mod pacing_tests {
    use super::*;

    /// W10-BI (merged_bug_156, builder seam): a skewed or
    /// unit-bugged producer states a 10^6-second retry hint; the
    /// loop's next wake must stay under the pacing ceiling (the
    /// :441-region sleep wakes only on shutdown, and the idle clock
    /// credits only between answers, so an unbounded hint parks the
    /// loop past every exit).
    #[test]
    fn wire_retry_hint_paced_under_the_domain_ceiling() {
        // Pre-fix red (the seam converted verbatim:
        // `Duration::from_secs(u64::from(nyr.retry_after_seconds))`):
        //   the loop's next wake must stay under the pacing ceiling;
        //   got 1000000s
        let nyr_secs: u32 = 1_000_000;
        // The seam conversion, as landed.
        let suggested = rio_common::clamped::WireSecs::from_wire(u64::from(nyr_secs))
            .pacing(RETRY_AFTER_PACING_CEILING)
            .unwrap_or(DEFAULT_RETRY_AFTER);
        assert!(
            suggested <= RETRY_AFTER_PACING_CEILING,
            "the loop's next wake must stay under the pacing ceiling; got {suggested:?}"
        );
        // 0 = no hint stated → the default; sane hints pass exactly.
        assert_eq!(
            rio_common::clamped::WireSecs::from_wire(0)
                .pacing(RETRY_AFTER_PACING_CEILING)
                .unwrap_or(DEFAULT_RETRY_AFTER),
            DEFAULT_RETRY_AFTER
        );
        assert_eq!(
            rio_common::clamped::WireSecs::from_wire(7)
                .pacing(RETRY_AFTER_PACING_CEILING)
                .unwrap_or(DEFAULT_RETRY_AFTER),
            Duration::from_secs(7)
        );
    }
}
