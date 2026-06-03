//! The materialization executor's scheduler client: poll → fenced
//! claim → report (substitution-replacement design §2.2 item 1).
//!
//! Transport is abstracted behind [`MaterializeTransport`] (the builder
//! runtime's `PullTransport` precedent — copied shape, not shared code)
//! so the claim/report state machines are unit-testable against a
//! scripted mock with no wire and no scheduler.
// r[impl store.materialize.executor+3]

use std::time::Duration;

use rio_common::grpc::DEFAULT_GRPC_TIMEOUT;
use rio_common::transport::{AttemptBudget, BoundedOutcome, SIGTERM_FINAL_ATTEMPT, bounded};
use tracing::{debug, warn};
use uuid::Uuid;

use rio_proto::types::{
    ListMaterializationJobsRequest, ListMaterializationJobsResponse, MaterializationOutcome,
    PullAssignmentRequest, PullAssignmentResponse, ReportMaterializationProgressRequest,
    ReportOutcomeRequest, pull_assignment_response,
};

/// `ServiceClaims.caller` this executor presents — the kind-attested
/// store credential (the scheduler's materialization-operations
/// allowlist accepts exactly this caller).
pub const STORE_SERVICE_CALLER: &str = "rio-store";

/// Retry envelope for unacked outcome reports: exponential 1 s → 30 s
/// cap, full jitter (the builder pull client's P5 discipline — copied
/// constants, same rationale: the scheduler may be mid-failover).
const REPORT_RETRY_ENVELOPE: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(30),
    jitter: rio_common::backoff::Jitter::Full,
};

/// The four unaries the executor speaks, abstracted for testing
/// (the builder runtime's `PullTransport` precedent).
pub trait MaterializeTransport {
    /// An attempt-level timeout was observed by the caller (the bounded
    /// await elapsed with no answer). The production transport treats
    /// this like an UNAVAILABLE answer and abandons the pinned
    /// connection — a black-holed connection is indistinguishable from
    /// the standby-pin (finding 18) at the caller. Default: no-op.
    fn note_timeout(&mut self) {}

    fn list_jobs(
        &mut self,
        req: ListMaterializationJobsRequest,
    ) -> impl Future<Output = Result<ListMaterializationJobsResponse, tonic::Status>> + Send;
    fn pull(
        &mut self,
        req: PullAssignmentRequest,
    ) -> impl Future<Output = Result<PullAssignmentResponse, tonic::Status>> + Send;
    fn report(
        &mut self,
        req: ReportOutcomeRequest,
    ) -> impl Future<Output = Result<(), tonic::Status>> + Send;
    fn report_progress(
        &mut self,
        req: ReportMaterializationProgressRequest,
    ) -> impl Future<Output = Result<(), tonic::Status>> + Send;
}

/// One claimed materialization job: the scheduler's job descriptor
/// joined with the delivered assignment (the open attempt this replica
/// now holds).
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    /// `materialization_jobs.job_id` (from the listing descriptor).
    /// Parse-don't-validate (bug_233): a descriptor whose job_id does
    /// not parse is REFUSED before the claim, so every held job is
    /// attributable — the pin-at-ingest write always binds a real job
    /// and the 093 CHECK's NULL-job pin class cannot be minted.
    pub job_id: Uuid,
    /// The derivation this job materializes (the pull's intent).
    pub drv_hash: String,
    /// Recorded creating-build tenant — a HINT only (PDQ-8): execution
    /// re-resolves against live interest; `None` = no recorded context.
    pub tenant_hint: Option<Uuid>,
    /// Job origin (`pruned` | `cache_opportunity` | …) — observability.
    pub origin: String,
    /// The open attempt's execution identity (UUIDv7 string) — what
    /// `ReportOutcome` is keyed by.
    pub exec_id: String,
    /// Store path of the .drv file (from the assignment payload).
    pub drv_path: String,
}

/// One poll→claim pass: list claimable jobs, then attempt to claim up
/// to `available_slots` of them via
/// `PullAssignment(kind=MATERIALIZATION, executor_instance=<pod>)`.
/// Returns the claimed assignments.
///
/// Race tolerance (design §2.2 item 1): `NotYetReady` answers are
/// NORMAL — another replica won the claim, or the job got resolved
/// between list and claim — never an error and never retried within
/// the pass (the next poll re-lists). `Gone` likewise. Per-RPC errors
/// are logged and skipped; a failed listing yields an empty pass.
// r[impl store.materialize.executor+3]
pub async fn poll_and_claim<T: MaterializeTransport>(
    transport: &mut T,
    executor_instance: &str,
    available_slots: usize,
    shutdown: &rio_common::signal::Token,
) -> Vec<ClaimedJob> {
    if available_slots == 0 {
        return Vec::new();
    }
    // bug_385: the listing window is DECOUPLED from the claim budget.
    // With limit == slots, a refused head — raced to another replica,
    // resolved between list and claim, or freshly parked — hides every
    // younger claimable job for the whole pass; at slots=1 the loop
    // starves behind one such head until it leaves the listing.
    // Listing is cheap (descriptors only); the claim loop below still
    // stops at the slot budget.
    const LISTING_WINDOW_MIN: usize = 16;
    const LISTING_WINDOW_PER_SLOT: usize = 8;
    let window = LISTING_WINDOW_MIN.max(available_slots.saturating_mul(LISTING_WINDOW_PER_SLOT));
    // Saturating u32 cast: slots are single-digit in practice.
    let limit = u32::try_from(window).unwrap_or(u32::MAX);
    let list_req = ListMaterializationJobsRequest {
        // The credential rides the x-rio-service-token metadata
        // (attached by the transport); the body field stays empty.
        service_token: String::new(),
        limit,
    };
    // Every RPC in the pass is bounded and raced against shutdown
    // (merged_bug_189): a black-holed leader connection becomes a
    // skipped pass instead of a parked claim loop, and SIGTERM ends
    // the pass promptly.
    let listed = match bounded(
        shutdown,
        DEFAULT_GRPC_TIMEOUT,
        transport.list_jobs(list_req),
    )
    .await
    {
        BoundedOutcome::Shutdown => return Vec::new(),
        BoundedOutcome::TimedOut { after } => {
            debug!(
                after_secs = after.as_secs(),
                "ListMaterializationJobs unanswered; empty poll pass"
            );
            transport.note_timeout();
            return Vec::new();
        }
        BoundedOutcome::Resolved(Ok(resp)) => resp.jobs,
        BoundedOutcome::Resolved(Err(status)) => {
            debug!(code = ?status.code(), msg = status.message(),
                   "ListMaterializationJobs failed; empty poll pass");
            return Vec::new();
        }
    };

    let mut claimed = Vec::new();
    for descriptor in listed {
        // The claim budget: stop once `available_slots` claims
        // SUCCEEDED — refusals (Gone/NotYetReady/timeout) do not
        // consume slots, they just advance past the head (bug_385's
        // in-pass skip).
        if claimed.len() >= available_slots {
            break;
        }
        // bug_233 (parse-don't-validate): refuse the claim BEFORE the
        // pull when the descriptor's job_id does not parse. Claiming an
        // attempt we cannot attribute to a job would strand it (the
        // resolve path is keyed by job) and the pin-at-ingest write
        // would mint the immortal NULL-job pin class the 093 CHECK now
        // forbids. A malformed descriptor is a scheduler-side bug —
        // surface it loudly and leave the attempt unclaimed.
        let job_id = match Uuid::parse_str(&descriptor.job_id) {
            Ok(id) => id,
            Err(err) => {
                warn!(drv_hash = %descriptor.drv_hash,
                      job_id = %descriptor.job_id, %err,
                      "malformed job_id in listing descriptor; claim refused");
                metrics::counter!("rio_store_materialization_claim_rejected_total",
                                  "reason" => "bad_job_id")
                .increment(1);
                continue;
            }
        };
        let req = PullAssignmentRequest {
            // No executor token: the store's credential is the
            // service token in metadata (the kind-attested credential).
            executor_token: String::new(),
            intent_id: descriptor.drv_hash.clone(),
            // BC-1: the work class + the per-replica identity ride
            // every claim.
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: executor_instance.to_string(),
            // Fresh claims NEVER carry a resume token (merged_bug_158:
            // re-delivery of a Claimed attempt requires the original
            // exec_id, so a colliding identity cannot steal it).
            resume_exec_id: String::new(),
        };
        match bounded(shutdown, DEFAULT_GRPC_TIMEOUT, transport.pull(req)).await {
            // SIGTERM mid-pass: return what was already claimed so the
            // caller can abort/report those attempts under the grace.
            BoundedOutcome::Shutdown => return claimed,
            BoundedOutcome::TimedOut { after } => {
                warn!(drv_hash = %descriptor.drv_hash, after_secs = after.as_secs(),
                      "materialization claim unanswered; skipping (next poll re-lists)");
                transport.note_timeout();
            }
            BoundedOutcome::Resolved(Ok(resp)) => match resp.outcome {
                Some(pull_assignment_response::Outcome::Assignment(assignment)) => {
                    claimed.push(ClaimedJob {
                        job_id,
                        drv_hash: descriptor.drv_hash,
                        tenant_hint: Uuid::parse_str(&descriptor.tenant_id).ok(),
                        origin: descriptor.origin,
                        exec_id: assignment.exec_id,
                        drv_path: assignment.drv_path,
                    });
                }
                // Lost the race (another replica holds the attempt) or
                // the job resolved between list and claim: normal.
                Some(pull_assignment_response::Outcome::NotYetReady(_))
                | Some(pull_assignment_response::Outcome::Gone(_))
                | None => {
                    debug!(drv_hash = %descriptor.drv_hash,
                           "materialization claim not delivered (race lost / job resolved)");
                }
            },
            BoundedOutcome::Resolved(Err(status)) => {
                warn!(drv_hash = %descriptor.drv_hash,
                      code = ?status.code(), msg = status.message(),
                      "materialization claim RPC failed; skipping (next poll re-lists)");
            }
        }
    }
    claimed
}

/// Forward a finished job's outcome until the scheduler acknowledges
/// it (the ack means the consumption transaction committed). Bounded
/// by `budget`; returns `true` on ack.
///
/// The builder's `report_until_acked` discipline (copied shape):
/// permanent rejections (auth / invalid-argument / unimplemented) give
/// up after one call — retrying cannot succeed and the establishment
/// sweep remains the scheduler-side backstop for the open attempt.
// r[impl store.materialize.executor+3]
pub async fn report_until_acked<T: MaterializeTransport>(
    transport: &mut T,
    exec_id: &str,
    outcome: MaterializationOutcome,
    budget: Duration,
    shutdown: &rio_common::signal::Token,
) -> bool {
    let budget = AttemptBudget::new(budget);
    let mut attempt: u32 = 0;
    loop {
        let req = ReportOutcomeRequest {
            exec_id: exec_id.to_owned(),
            report: None,
            materialization_outcome: Some(outcome.clone()),
        };
        if shutdown.is_cancelled() {
            // SIGTERM: one bounded best-effort attempt, then out (the
            // builder report loop's discipline; the establishment
            // sweep is the scheduler-side backstop).
            return matches!(
                tokio::time::timeout(SIGTERM_FINAL_ATTEMPT, transport.report(req)).await,
                Ok(Ok(()))
            );
        }
        // Bounded + raced against SIGTERM: hung attempts spend the
        // budget exactly like answered failures (merged_bug_189; the
        // pre-fix loop awaited the report bare and checked the budget
        // only on Err answers). Report acks are idempotent
        // scheduler-side, so per-attempt-cap retries are safe.
        let result = bounded(
            shutdown,
            budget.attempt_bound(DEFAULT_GRPC_TIMEOUT),
            transport.report(req),
        )
        .await;
        match result {
            // Loop back: the next iteration takes the SIGTERM
            // single-attempt arm.
            BoundedOutcome::Shutdown => continue,
            BoundedOutcome::Resolved(Ok(())) => return true,
            BoundedOutcome::Resolved(Err(status)) if is_fatal_rejection(status.code()) => {
                warn!(code = ?status.code(), msg = status.message(),
                      "materialization ReportOutcome permanently rejected; giving up \
                       (the establishment sweep is the scheduler-side backstop)");
                return false;
            }
            BoundedOutcome::TimedOut { after } => {
                transport.note_timeout();
                if budget.expired() {
                    warn!(
                        after_secs = after.as_secs(),
                        "materialization ReportOutcome never acknowledged within the budget \
                           (hung attempts)"
                    );
                    return false;
                }
                debug!(
                    after_secs = after.as_secs(),
                    "materialization ReportOutcome attempt unanswered; retrying"
                );
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {}
                    _ = tokio::time::sleep(REPORT_RETRY_ENVELOPE.duration(attempt - 1)) => {}
                }
            }
            BoundedOutcome::Resolved(Err(status)) => {
                if budget.expired() {
                    warn!(code = ?status.code(), msg = status.message(),
                          "materialization ReportOutcome never acknowledged within the budget");
                    return false;
                }
                debug!(code = ?status.code(), msg = status.message(),
                       "materialization ReportOutcome not acknowledged; retrying");
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {}
                    _ = tokio::time::sleep(REPORT_RETRY_ENVELOPE.duration(attempt - 1)) => {}
                }
            }
        }
    }
}

/// Permanent, non-retryable rejection codes (the builder pull client's
/// `is_fatal_rejection`, same set): retrying these burns the budget
/// with no chance of progress.
fn is_fatal_rejection(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
            | tonic::Code::Unimplemented
            | tonic::Code::InvalidArgument
    )
}

// ---------------------------------------------------------------------------
// Production transport
// ---------------------------------------------------------------------------

/// The concrete client type the transport drives (lazy channel + the
/// service-token interceptor).
type ExecutorClient = rio_proto::ExecutorServiceClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        rio_auth::hmac::ServiceTokenInterceptor,
    >,
>;

/// The store-service-authenticated `ExecutorServiceClient`: a lazy
/// channel to the scheduler with
/// [`rio_auth::hmac::ServiceTokenInterceptor`] minting a fresh
/// `ServiceClaims { caller: "rio-store", instance: Some(<pod>) }` token
/// (60 s expiry) onto every request's `x-rio-service-token` metadata —
/// the kind-attested, **instance-bound** credential the scheduler's
/// materialization operations require (T-5.1: the scheduler verifies
/// the claimed replica identity against the request's
/// `executor_instance` instead of trusting the request field).
/// Signer `None` = dev mode: no header, only meaningful against a
/// keyless scheduler.
///
/// `Clone` is cheap (tonic channels are reference-counted): the claim
/// loop clones one copy per job execution for the BC-4 progress relay
/// task, so display traffic never contends with the claim/report
/// transport.
#[derive(Clone)]
pub struct SchedulerTransport {
    client: ExecutorClient,
    /// Constructor inputs, retained so [`Self::abandon_connection`] can
    /// rebuild the channel when the current connection is pinned to a
    /// peer that cannot serve (finding 18: the standby replica after a
    /// scheduler Deployment rollout).
    scheduler_addr: String,
    signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    /// The replica identity bound into every minted token (T-5.1) —
    /// the same [`super::executor_instance`] value the claim loop
    /// asserts as `executor_instance`, so claim and credential always
    /// agree.
    instance: String,
}

impl SchedulerTransport {
    /// Build the lazy channel + interceptor stack.
    ///
    /// Lazy + h2 keepalive for the same reason the scheduler's store
    /// client is lazy (`connect_store_lazy`,
    /// rio-proto/src/client/mod.rs — the cross-reference): the peer
    /// Deployment rolls, DNS re-resolves, and the channel must follow
    /// the Service's current endpoint instead of pinning the boot-time
    /// pod IP. Never fails on connection (only on a malformed addr).
    ///
    /// `instance` is this replica's pod identity
    /// ([`super::executor_instance`]); it is bound into every minted
    /// service token so the scheduler can verify (not trust) the
    /// `executor_instance` field of every claim.
    pub fn connect_lazy(
        scheduler_addr: &str,
        signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
        instance: &str,
    ) -> anyhow::Result<Self> {
        let client = Self::build_client(scheduler_addr, signer.clone(), instance)?;
        Ok(Self {
            client,
            scheduler_addr: scheduler_addr.to_owned(),
            signer,
            instance: instance.to_owned(),
        })
    }

    /// The channel/interceptor/client stack shared by construction and
    /// [`Self::abandon_connection`].
    fn build_client(
        scheduler_addr: &str,
        signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
        instance: &str,
    ) -> anyhow::Result<ExecutorClient> {
        let endpoint = tonic::transport::Channel::from_shared(format!("http://{scheduler_addr}"))?
            .connect_timeout(Duration::from_secs(10))
            .initial_stream_window_size(Some(rio_common::grpc::H2_INITIAL_STREAM_WINDOW))
            .initial_connection_window_size(Some(rio_common::grpc::H2_INITIAL_CONN_WINDOW))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true);
        let channel = endpoint.connect_lazy();
        let interceptor = rio_auth::hmac::ServiceTokenInterceptor::with_instance(
            signer,
            STORE_SERVICE_CALLER,
            instance.to_owned(),
        );
        let max = rio_common::grpc::max_message_size();
        Ok(
            rio_proto::ExecutorServiceClient::with_interceptor(channel, interceptor)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        )
    }

    /// Drop the current channel and lazily dial a fresh one.
    ///
    /// Why this exists (finding 18 — the scheduler-rollout claim
    /// stall): the executor dials the scheduler's ClusterIP Service;
    /// kube-proxy pins each TCP connection to ONE backend pod; gRPC
    /// multiplexes every RPC onto that connection; and h2 keepalive
    /// keeps it healthy indefinitely. Only the LEADER replica serves —
    /// the standby answers `UNAVAILABLE "not leader (standby replica)"`
    /// on every RPC over a perfectly healthy connection. So a
    /// connection pinned to the standby (a 50/50 outcome after a
    /// Deployment rollout replaces both pods) never breaks and never
    /// recovers on its own: the lazy channel only re-dials on
    /// connection-level failure, which never comes. Abandoning the
    /// channel is the only way out; the fresh connection re-rolls the
    /// kube-proxy backend choice, so repeated polls converge on the
    /// leader (geometrically, ~2 passes expected with 2 replicas).
    ///
    /// Failure to rebuild keeps the old client (the addr parsed at
    /// construction, so this is unreachable in practice).
    fn abandon_connection(&mut self) {
        match Self::build_client(&self.scheduler_addr, self.signer.clone(), &self.instance) {
            Ok(client) => self.client = client,
            Err(e) => warn!(
                scheduler_addr = %self.scheduler_addr, error = %e,
                "scheduler channel rebuild failed; keeping the existing connection"
            ),
        }
    }

    /// Inspect one RPC outcome: an `UNAVAILABLE` answer abandons the
    /// connection (see [`Self::abandon_connection`]). Every other
    /// outcome — success or a different rejection — keeps it: those
    /// answers prove the connected peer is the serving leader (or that
    /// the request itself is at fault), so connection churn would only
    /// cost throughput.
    fn note_rpc_outcome<T>(&mut self, result: &Result<T, tonic::Status>) {
        if let Err(status) = result
            && status.code() == tonic::Code::Unavailable
        {
            debug!(
                msg = status.message(),
                "scheduler answered UNAVAILABLE; abandoning the pinned connection \
                 (rollout/standby recovery)"
            );
            self.abandon_connection();
        }
    }
}

impl MaterializeTransport for SchedulerTransport {
    /// A bounded await elapsed with no answer: indistinguishable at
    /// this layer from the standby-pinned connection (finding 18) —
    /// abandon the channel so the next RPC re-rolls the kube-proxy
    /// backend choice.
    fn note_timeout(&mut self) {
        debug!(
            "scheduler RPC timed out; abandoning the pinned connection \
             (rollout/standby/black-hole recovery)"
        );
        self.abandon_connection();
    }

    async fn list_jobs(
        &mut self,
        req: ListMaterializationJobsRequest,
    ) -> Result<ListMaterializationJobsResponse, tonic::Status> {
        let result = self
            .client
            .list_materialization_jobs(req)
            .await
            .map(|r| r.into_inner());
        self.note_rpc_outcome(&result);
        result
    }

    async fn pull(
        &mut self,
        req: PullAssignmentRequest,
    ) -> Result<PullAssignmentResponse, tonic::Status> {
        let result = self
            .client
            .pull_assignment(req)
            .await
            .map(|r| r.into_inner());
        self.note_rpc_outcome(&result);
        result
    }

    async fn report(&mut self, req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
        let result = self.client.report_outcome(req).await.map(|_| ());
        self.note_rpc_outcome(&result);
        result
    }

    async fn report_progress(
        &mut self,
        req: ReportMaterializationProgressRequest,
    ) -> Result<(), tonic::Status> {
        let result = self
            .client
            .report_materialization_progress(req)
            .await
            .map(|_| ());
        self.note_rpc_outcome(&result);
        result
    }
}

// ---------------------------------------------------------------------------
// Mock-transport battery
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted transport (the builder runtime's `ScriptedTransport`
    /// precedent): pops one scripted answer per call; repeats the last
    /// entry once the script is exhausted. Records every
    /// `PullAssignmentRequest` so the BC-1 wire-obligation test can
    /// assert kind/instance on each claim.
    struct MockTransport {
        listings: VecDeque<Result<ListMaterializationJobsResponse, tonic::Status>>,
        pulls: VecDeque<Result<PullAssignmentResponse, tonic::Status>>,
        reports: VecDeque<Result<(), tonic::Status>>,
        list_calls: u32,
        pull_calls: u32,
        report_calls: u32,
        seen_pull_requests: Vec<PullAssignmentRequest>,
        seen_list_limits: Vec<u32>,
    }

    impl MockTransport {
        fn new(
            listings: Vec<Result<ListMaterializationJobsResponse, tonic::Status>>,
            pulls: Vec<Result<PullAssignmentResponse, tonic::Status>>,
            reports: Vec<Result<(), tonic::Status>>,
        ) -> Self {
            Self {
                listings: listings.into(),
                pulls: pulls.into(),
                reports: reports.into(),
                list_calls: 0,
                pull_calls: 0,
                report_calls: 0,
                seen_pull_requests: Vec::new(),
                seen_list_limits: Vec::new(),
            }
        }
    }

    impl MaterializeTransport for MockTransport {
        async fn list_jobs(
            &mut self,
            req: ListMaterializationJobsRequest,
        ) -> Result<ListMaterializationJobsResponse, tonic::Status> {
            self.list_calls += 1;
            self.seen_list_limits.push(req.limit);
            match self.listings.len() {
                0 => Err(tonic::Status::unavailable("script exhausted")),
                1 => self.listings[0].clone(),
                _ => self.listings.pop_front().expect("non-empty"),
            }
        }

        async fn pull(
            &mut self,
            req: PullAssignmentRequest,
        ) -> Result<PullAssignmentResponse, tonic::Status> {
            self.pull_calls += 1;
            self.seen_pull_requests.push(req);
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

        async fn report_progress(
            &mut self,
            _req: ReportMaterializationProgressRequest,
        ) -> Result<(), tonic::Status> {
            Ok(())
        }
    }

    fn token() -> rio_common::signal::Token {
        rio_common::signal::Token::new()
    }

    fn descriptor(n: u32) -> rio_proto::types::MaterializationJobDescriptor {
        rio_proto::types::MaterializationJobDescriptor {
            job_id: Uuid::now_v7().to_string(),
            drv_hash: format!("drv-claim-{n}"),
            tenant_id: String::new(),
            origin: "cache_opportunity".to_string(),
        }
    }

    fn listing(
        jobs: Vec<rio_proto::types::MaterializationJobDescriptor>,
    ) -> ListMaterializationJobsResponse {
        ListMaterializationJobsResponse { jobs }
    }

    fn deliver(exec_id: &str, drv_path: &str) -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::Assignment(
                rio_proto::types::WorkAssignment {
                    drv_path: drv_path.to_string(),
                    exec_id: exec_id.to_string(),
                    ..Default::default()
                },
            )),
        }
    }

    fn gone() -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::Gone(
                rio_proto::types::Gone {},
            )),
        }
    }

    fn not_yet_ready() -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::NotYetReady(
                rio_proto::types::NotYetReady {
                    retry_after_seconds: 5,
                },
            )),
        }
    }

    /// (a) The happy path: 2 listed jobs, both claims deliver → 2
    /// ClaimedJobs carrying the descriptors' identity joined with the
    /// assignments' exec ids.
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn poll_and_claim_claims_listed_jobs() {
        let d1 = descriptor(1);
        let d2 = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d1.clone(), d2.clone()]))],
            vec![
                Ok(deliver("exec-1", "/nix/store/aaa-one.drv")),
                Ok(deliver("exec-2", "/nix/store/bbb-two.drv")),
            ],
            vec![],
        );
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8, &token()).await;
        assert_eq!(claimed.len(), 2, "both listed jobs are claimed");
        assert_eq!(claimed[0].drv_hash, d1.drv_hash);
        assert_eq!(claimed[0].exec_id, "exec-1");
        assert_eq!(claimed[1].drv_hash, d2.drv_hash);
        assert_eq!(claimed[1].exec_id, "exec-2");
        assert_ne!(
            claimed[0].exec_id, claimed[1].exec_id,
            "distinct attempts get distinct exec ids"
        );
        assert_eq!(t.list_calls, 1);
        assert_eq!(t.pull_calls, 2);
    }

    /// bug_385 (the head-starvation fix): a refused head — Gone, the
    /// job resolved or was raced to another replica — must NOT hide
    /// younger claimable jobs in the same pass. With budget 1 and the
    /// head refusing, the SECOND listed job is claimed in the SAME
    /// pass.
    ///
    /// RED (pre-fix): `limit = available_slots` listed only the head
    /// (LIMIT-1 oldest-first); the pass claimed nothing, every pass —
    /// the younger job starved behind the refused head until the head
    /// left the listing.
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn refused_head_does_not_hide_younger_jobs() {
        let head = descriptor(1);
        let younger = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![head.clone(), younger.clone()]))],
            vec![Ok(gone()), Ok(deliver("exec-2", "/nix/store/bbb-two.drv"))],
            vec![],
        );
        // Budget 1: the head refuses, the younger job must fill the
        // slot in the same pass.
        let claimed = poll_and_claim(&mut t, "store-replica-0", 1, &token()).await;
        assert_eq!(
            claimed.len(),
            1,
            "the refused head does not consume the slot; the younger job claims"
        );
        assert_eq!(claimed[0].drv_hash, younger.drv_hash);
        assert_eq!(claimed[0].exec_id, "exec-2");
        assert_eq!(t.list_calls, 1, "one pass");
        assert_eq!(t.pull_calls, 2, "head attempted, then the younger job");
        // The listing window is decoupled from the budget: even at
        // budget 1 the request asks for at least the minimum window.
        assert!(
            t.seen_list_limits[0] >= 16,
            "the listing window is independent of the claim budget; got limit {}",
            t.seen_list_limits[0]
        );
    }

    /// bug_385, the budget side: refusals do not consume slots, but
    /// successful claims do — the pass stops at the budget even with
    /// more claimable jobs listed.
    #[tokio::test]
    async fn claim_budget_stops_at_slots() {
        let d1 = descriptor(1);
        let d2 = descriptor(2);
        let d3 = descriptor(3);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d1.clone(), d2.clone(), d3.clone()]))],
            vec![
                Ok(deliver("exec-1", "/nix/store/aaa-one.drv")),
                Ok(deliver("exec-2", "/nix/store/bbb-two.drv")),
            ],
            vec![],
        );
        let claimed = poll_and_claim(&mut t, "store-replica-0", 2, &token()).await;
        assert_eq!(claimed.len(), 2, "the budget caps successful claims");
        assert_eq!(t.pull_calls, 2, "no claim attempted past the budget");
    }

    /// bug_233 (bughunt wave): a descriptor whose job_id does not parse
    /// as a UUID is REFUSED before any claim is attempted — no
    /// ClaimedJob, no PullAssignment RPC (claiming an attempt we cannot
    /// attribute to a job would mint the immortal NULL-job pin class).
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn malformed_job_id_refuses_the_claim() {
        let mut bad = descriptor(9);
        bad.job_id = "not-a-uuid".to_string();
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![bad]))],
            vec![Ok(deliver("exec-9", "/nix/store/zzz-bad.drv"))],
            vec![],
        );
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8, &token()).await;
        assert!(
            claimed.is_empty(),
            "a malformed job_id must refuse the claim (pre-fix RED: claimed with job_id=None)"
        );
        assert_eq!(
            t.pull_calls, 0,
            "the refusal happens BEFORE the pull — the attempt is never claimed"
        );
    }

    /// (b) NotYetReady on a claim is race tolerance, not an error: the
    /// pass returns the claims that DID deliver and never retries the
    /// lost one (the next poll re-lists).
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn poll_and_claim_tolerates_not_yet_ready() {
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![descriptor(1), descriptor(2)]))],
            vec![
                Ok(not_yet_ready()),
                Ok(deliver("exec-2", "/nix/store/bbb-two.drv")),
            ],
            vec![],
        );
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8, &token()).await;
        assert_eq!(
            claimed.len(),
            1,
            "the lost race is tolerated; the won claim is returned"
        );
        assert_eq!(claimed[0].exec_id, "exec-2");
        assert_eq!(
            t.pull_calls, 2,
            "exactly one pull per listed job — the lost claim is NOT retried in-pass"
        );
    }

    /// (c) The slot bound: 5 listed, 2 slots → exactly 2 pulls.
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn poll_and_claim_respects_slots() {
        let mut t = MockTransport::new(
            vec![Ok(listing((1..=5).map(descriptor).collect::<Vec<_>>()))],
            vec![Ok(deliver("exec-x", "/nix/store/xxx.drv"))],
            vec![],
        );
        let claimed = poll_and_claim(&mut t, "store-replica-0", 2, &token()).await;
        assert_eq!(claimed.len(), 2);
        assert_eq!(
            t.pull_calls, 2,
            "claims are bounded by available slots, not by the listing size"
        );

        // Zero slots → no RPCs at all.
        let mut idle = MockTransport::new(vec![], vec![], vec![]);
        let claimed = poll_and_claim(&mut idle, "store-replica-0", 0, &token()).await;
        assert!(claimed.is_empty());
        assert_eq!(idle.list_calls, 0, "zero slots never even lists");
    }

    /// (d) The BC-1 wire obligation: every claim carries
    /// kind=MATERIALIZATION + the configured executor_instance, no
    /// executor token, and the listed job's drv hash as the intent.
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn claim_carries_kind_and_instance() {
        let d1 = descriptor(1);
        let d2 = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d1.clone(), d2.clone()]))],
            vec![Ok(deliver("exec-1", "/nix/store/aaa.drv"))],
            vec![],
        );
        let _ = poll_and_claim(&mut t, "store-replica-7", 8, &token()).await;
        assert_eq!(t.seen_pull_requests.len(), 2);
        for (req, descriptor) in t.seen_pull_requests.iter().zip([&d1, &d2]) {
            assert_eq!(
                req.kind(),
                rio_proto::types::AttemptKind::Materialization,
                "every claim carries the materialization kind"
            );
            assert_eq!(
                req.executor_instance, "store-replica-7",
                "every claim carries the per-replica identity (BC-1)"
            );
            assert_eq!(
                req.intent_id, descriptor.drv_hash,
                "the claim's intent is the listed job's derivation"
            );
            assert!(
                req.executor_token.is_empty(),
                "the store presents no executor token (the service token \
                 rides the metadata, attached by the transport)"
            );
        }
    }

    /// (e) The report loop: two transient failures then an ack → 3
    /// calls, returns true. A permanent rejection gives up after one
    /// call. Budget exhaustion gives up.
    // r[verify store.materialize.executor+3]
    #[tokio::test(start_paused = true)]
    async fn report_until_acked_retries() {
        let outcome = MaterializationOutcome {
            outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                rio_proto::types::materialization_outcome::Success {
                    ingested_paths: vec!["/nix/store/aaa-one".into()],
                    verified_paths: vec![],
                },
            )),
        };

        // Transient → retried until acked.
        let mut t = MockTransport::new(
            vec![],
            vec![],
            vec![
                Err(tonic::Status::unavailable("not leader")),
                Err(tonic::Status::unavailable("still settling")),
                Ok(()),
            ],
        );
        let acked = report_until_acked(
            &mut t,
            "exec-1",
            outcome.clone(),
            Duration::from_secs(600),
            &token(),
        )
        .await;
        assert!(acked);
        assert_eq!(t.report_calls, 3);

        // Permanent rejection → exactly one call, false.
        let mut t = MockTransport::new(
            vec![],
            vec![],
            vec![Err(tonic::Status::permission_denied("bad credential"))],
        );
        let acked = report_until_acked(
            &mut t,
            "exec-2",
            outcome.clone(),
            Duration::from_secs(600),
            &token(),
        )
        .await;
        assert!(!acked);
        assert_eq!(t.report_calls, 1, "permanent rejections are never retried");

        // Budget exhaustion → false after spending the window.
        let mut t = MockTransport::new(
            vec![],
            vec![],
            vec![Err(tonic::Status::unavailable("scheduler gone"))],
        );
        let acked =
            report_until_acked(&mut t, "exec-3", outcome, Duration::from_secs(60), &token()).await;
        assert!(
            !acked,
            "an unacked report inside the budget is not a success"
        );
        assert!(
            t.report_calls >= 3,
            "the budget window is spent retrying, saw {}",
            t.report_calls
        );
    }

    /// merged_bug_189: a black-holed report (accepted, never answered)
    /// exhausts the budget instead of pending forever — hung attempts
    /// spend the budget like answered failures. (The pre-fix loop
    /// awaited the report bare; this shape was unexpressible — the
    /// signature change is the compile-level red, and the runtime red
    /// for the identical loop shape is recorded in the builder's
    /// report_black_hole_exhausts_budget_without_sigterm.)
    // r[verify store.materialize.executor+3]
    #[tokio::test(start_paused = true)]
    async fn report_black_hole_times_out_within_budget() {
        struct BlackHole {
            calls: u32,
        }
        impl MaterializeTransport for BlackHole {
            async fn list_jobs(
                &mut self,
                _req: ListMaterializationJobsRequest,
            ) -> Result<ListMaterializationJobsResponse, tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
            async fn pull(
                &mut self,
                _req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                Err(tonic::Status::unavailable("unused"))
            }
            async fn report(&mut self, _req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                self.calls += 1;
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn report_progress(
                &mut self,
                _req: ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                Ok(())
            }
        }
        let mut t = BlackHole { calls: 0 };
        let shutdown = token();
        let started = tokio::time::Instant::now();
        let acked = tokio::time::timeout(
            Duration::from_secs(3600),
            report_until_acked(
                &mut t,
                "exec-bh",
                MaterializationOutcome { outcome: None },
                Duration::from_secs(120),
                &shutdown,
            ),
        )
        .await
        .expect("hung report attempts must exhaust the budget, not pend forever");
        assert!(!acked);
        assert!(
            started.elapsed() >= Duration::from_secs(120)
                && started.elapsed() < Duration::from_secs(400),
            "the budget bounds the phase (elapsed {:?})",
            started.elapsed()
        );
        assert!(t.calls >= 2, "multiple bounded attempts were made");
    }

    /// SIGTERM mid-report: exactly one bounded best-effort attempt.
    // r[verify store.materialize.executor+3]
    #[tokio::test(start_paused = true)]
    async fn report_after_sigterm_is_a_single_bounded_attempt() {
        let mut t = MockTransport::new(
            vec![],
            vec![],
            vec![Err(tonic::Status::unavailable("scheduler unreachable"))],
        );
        let shutdown = token();
        shutdown.cancel();
        let started = tokio::time::Instant::now();
        let acked = report_until_acked(
            &mut t,
            "exec-sig",
            MaterializationOutcome { outcome: None },
            Duration::from_secs(600),
            &shutdown,
        )
        .await;
        assert!(!acked);
        assert_eq!(t.report_calls, 1, "exactly one attempt after SIGTERM");
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the bounded attempt fits the grace"
        );
    }

    /// SIGTERM mid-pass: poll_and_claim returns the claims already won
    /// so the caller can settle them under the grace, instead of
    /// continuing the pass.
    // r[verify store.materialize.executor+3]
    #[tokio::test(start_paused = true)]
    async fn poll_and_claim_sigterm_ends_pass_with_claims_so_far() {
        struct CancelOnFirstPull {
            inner: MockTransport,
            shutdown: rio_common::signal::Token,
        }
        impl MaterializeTransport for CancelOnFirstPull {
            async fn list_jobs(
                &mut self,
                req: ListMaterializationJobsRequest,
            ) -> Result<ListMaterializationJobsResponse, tonic::Status> {
                self.inner.list_jobs(req).await
            }
            async fn pull(
                &mut self,
                req: PullAssignmentRequest,
            ) -> Result<PullAssignmentResponse, tonic::Status> {
                let resp = self.inner.pull(req).await;
                // SIGTERM lands right after the first claim delivers.
                self.shutdown.cancel();
                resp
            }
            async fn report(&mut self, req: ReportOutcomeRequest) -> Result<(), tonic::Status> {
                self.inner.report(req).await
            }
            async fn report_progress(
                &mut self,
                req: ReportMaterializationProgressRequest,
            ) -> Result<(), tonic::Status> {
                self.inner.report_progress(req).await
            }
        }
        let shutdown = token();
        let mut t = CancelOnFirstPull {
            inner: MockTransport::new(
                vec![Ok(listing(vec![descriptor(1), descriptor(2)]))],
                vec![Ok(deliver("exec-1", "/nix/store/aaa.drv"))],
                vec![],
            ),
            shutdown: shutdown.clone(),
        };
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8, &shutdown).await;
        assert_eq!(claimed.len(), 1, "the won claim is returned");
        assert_eq!(
            t.inner.pull_calls, 1,
            "the pass ends at SIGTERM instead of claiming more work it cannot run"
        );
    }

    // -----------------------------------------------------------------------
    // Scheduler-rollout survivability (finding 18: the transition claim
    // stall) — the production SchedulerTransport against real tonic
    // servers behind a kube-proxy stand-in.
    // -----------------------------------------------------------------------

    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;
    use tonic::{Request, Response, Status};

    /// A scheduler STANDBY replica: every RPC answers
    /// `UNAVAILABLE "not leader (standby replica)"` on a perfectly
    /// healthy connection — byte-identical to what the scheduler's
    /// `ensure_leader` guard produces.
    struct StandbyExecutorService;

    #[tonic::async_trait]
    impl rio_proto::ExecutorService for StandbyExecutorService {
        async fn pull_assignment(
            &self,
            _: Request<PullAssignmentRequest>,
        ) -> Result<Response<PullAssignmentResponse>, Status> {
            Err(Status::unavailable("not leader (standby replica)"))
        }
        async fn report_outcome(
            &self,
            _: Request<ReportOutcomeRequest>,
        ) -> Result<Response<rio_proto::types::ReportOutcomeResponse>, Status> {
            Err(Status::unavailable("not leader (standby replica)"))
        }
        async fn list_materialization_jobs(
            &self,
            _: Request<ListMaterializationJobsRequest>,
        ) -> Result<Response<ListMaterializationJobsResponse>, Status> {
            Err(Status::unavailable("not leader (standby replica)"))
        }
        async fn report_materialization_progress(
            &self,
            _: Request<ReportMaterializationProgressRequest>,
        ) -> Result<Response<rio_proto::types::ReportMaterializationProgressResponse>, Status>
        {
            Err(Status::unavailable("not leader (standby replica)"))
        }
    }

    /// The scheduler LEADER: lists one claimable job and delivers its
    /// claim.
    struct LeaderExecutorService;

    #[tonic::async_trait]
    impl rio_proto::ExecutorService for LeaderExecutorService {
        async fn pull_assignment(
            &self,
            _: Request<PullAssignmentRequest>,
        ) -> Result<Response<PullAssignmentResponse>, Status> {
            Ok(Response::new(deliver(
                "exec-rollout-1",
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-rollout.drv",
            )))
        }
        async fn report_outcome(
            &self,
            _: Request<ReportOutcomeRequest>,
        ) -> Result<Response<rio_proto::types::ReportOutcomeResponse>, Status> {
            Ok(Response::new(rio_proto::types::ReportOutcomeResponse {}))
        }
        async fn list_materialization_jobs(
            &self,
            _: Request<ListMaterializationJobsRequest>,
        ) -> Result<Response<ListMaterializationJobsResponse>, Status> {
            let mut d = descriptor(1);
            d.drv_hash = "rollout-drv".to_string();
            Ok(Response::new(listing(vec![d])))
        }
        async fn report_materialization_progress(
            &self,
            _: Request<ReportMaterializationProgressRequest>,
        ) -> Result<Response<rio_proto::types::ReportMaterializationProgressResponse>, Status>
        {
            Ok(Response::new(
                rio_proto::types::ReportMaterializationProgressResponse {},
            ))
        }
    }

    /// Spawn an in-process ExecutorService server on a random port.
    async fn spawn_executor_service<S>(svc: S) -> SocketAddr
    where
        S: rio_proto::ExecutorService,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            Server::builder()
                .add_service(rio_proto::ExecutorServiceServer::new(svc))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        addr
    }

    /// A kube-proxy stand-in: every NEW TCP connection accepted on the
    /// proxy port is forwarded to whatever backend is CURRENT at accept
    /// time; established flows stay pinned to the backend they started
    /// with (exactly the per-connection DNAT semantics of a k8s
    /// ClusterIP Service).
    async fn spawn_switchable_proxy(initial: SocketAddr) -> (SocketAddr, Arc<Mutex<SocketAddr>>) {
        let backend = Arc::new(Mutex::new(initial));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend_for_task = Arc::clone(&backend);
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    break;
                };
                let target = *backend_for_task.lock().unwrap();
                tokio::spawn(async move {
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                });
            }
        });
        (addr, backend)
    }

    // r[verify store.materialize.executor+3]
    /// FINDING 18 (the transition claim stall; red-first): the executor
    /// transport must abandon a connection pinned to a standby scheduler
    /// replica and reach the leader within a bounded number of poll
    /// passes — scheduler-Deployment-rollout survivability.
    ///
    /// The k8s mechanics reproduced here: the executor dials the
    /// scheduler ClusterIP Service; kube-proxy pins each TCP connection
    /// to one backend pod; gRPC multiplexes every RPC onto that single
    /// connection; h2 keepalive keeps it healthy indefinitely. After a
    /// scheduler Deployment rollout replaces both pods, the executor's
    /// reconnect lands on EITHER new pod — landing on the standby means
    /// every subsequent RPC answers UNAVAILABLE "not leader (standby
    /// replica)" on a connection that never breaks, so the executor
    /// polls a dead end forever while claimable jobs sit pending (the
    /// vm-materialization-transition flip-on stall: jobs created, never
    /// claimed within 300 s).
    #[tokio::test]
    async fn poll_abandons_connection_pinned_to_standby_replica() {
        let standby_addr = spawn_executor_service(StandbyExecutorService).await;
        let leader_addr = spawn_executor_service(LeaderExecutorService).await;
        // The "kube-proxy": initially fronts the standby.
        let (proxy_addr, backend) = spawn_switchable_proxy(standby_addr).await;

        let mut transport =
            SchedulerTransport::connect_lazy(&proxy_addr.to_string(), None, "store-replica-0")
                .unwrap();

        // The executor's connection gets pinned to the standby: the
        // poll pass comes back empty (UNAVAILABLE answers).
        let claimed = poll_and_claim(&mut transport, "store-replica-0", 1, &token()).await;
        assert!(
            claimed.is_empty(),
            "the standby answers UNAVAILABLE — nothing claimable on this pass"
        );

        // The rollout completes: the Service now fronts the leader. The
        // pinned connection still goes to the (still healthy) standby —
        // only a NEW connection can reach the leader.
        *backend.lock().unwrap() = leader_addr;

        // The executor must reach the leader within a bounded number of
        // poll passes. Without reconnect-on-UNAVAILABLE the transport
        // reuses the pinned connection forever and every pass stays
        // empty.
        let mut claimed = Vec::new();
        for _ in 0..5 {
            claimed = poll_and_claim(&mut transport, "store-replica-0", 1, &token()).await;
            if !claimed.is_empty() {
                break;
            }
        }
        assert!(
            !claimed.is_empty(),
            "the executor transport must abandon a connection pinned to a \
             standby replica and reach the leader within a bounded number of \
             poll passes (scheduler-Deployment-rollout survivability); every \
             pass kept polling the standby"
        );
        assert_eq!(claimed[0].drv_hash, "rollout-drv");
        assert_eq!(claimed[0].exec_id, "exec-rollout-1");
    }

    /// The report path has the same pinning hazard: an outcome report
    /// retried against a standby-pinned connection burns its whole
    /// budget without ever landing. With reconnect-on-UNAVAILABLE the
    /// retry envelope converges on the leader and the report acks.
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn report_abandons_connection_pinned_to_standby_replica() {
        let standby_addr = spawn_executor_service(StandbyExecutorService).await;
        let leader_addr = spawn_executor_service(LeaderExecutorService).await;
        let (proxy_addr, backend) = spawn_switchable_proxy(standby_addr).await;

        let mut transport =
            SchedulerTransport::connect_lazy(&proxy_addr.to_string(), None, "store-replica-0")
                .unwrap();

        // Pin the connection to the standby with one failing pass.
        let _ = poll_and_claim(&mut transport, "store-replica-0", 1, &token()).await;
        // The rollout completes mid-execution.
        *backend.lock().unwrap() = leader_addr;

        let outcome = MaterializationOutcome {
            outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                rio_proto::types::materialization_outcome::Success {
                    ingested_paths: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-one".into()],
                    verified_paths: vec![],
                },
            )),
        };
        let acked = report_until_acked(
            &mut transport,
            "exec-rollout-1",
            outcome,
            Duration::from_secs(20),
            &token(),
        )
        .await;
        assert!(
            acked,
            "an outcome report must converge on the leader after a rollout \
             instead of burning its budget against the pinned standby"
        );
    }

    /// bug_362's client half (A2 rider): the display-only progress
    /// relay rides the same UNAVAILABLE-abandon discipline. The
    /// scheduler-side fix gives the standby an `ensure_leader` answer
    /// on `ReportMaterializationProgress`; this pins the client
    /// reaction — an UNAVAILABLE progress ack abandons the pinned
    /// connection so the NEXT progress tick (and every weightier RPC
    /// sharing the transport) redials toward the leader. Pre-fix the
    /// standby ACKed progress (Ok), the connection stayed pinned, and
    /// the client never learned it was talking to a wall.
    // r[verify store.materialize.executor+3]
    #[tokio::test]
    async fn progress_abandons_connection_pinned_to_standby_replica() {
        let standby_addr = spawn_executor_service(StandbyExecutorService).await;
        let leader_addr = spawn_executor_service(LeaderExecutorService).await;
        let (proxy_addr, backend) = spawn_switchable_proxy(standby_addr).await;

        let mut transport =
            SchedulerTransport::connect_lazy(&proxy_addr.to_string(), None, "store-replica-0")
                .unwrap();

        // Pin to the standby: the progress tick answers UNAVAILABLE
        // (the scheduler-side 362 fix) and the transport abandons.
        let req = ReportMaterializationProgressRequest {
            exec_id: "exec-rollout-1".into(),
            upstream_uri: "https://cache.example".into(),
            bytes_done: 1,
            bytes_expected: 2,
        };
        let first = transport.report_progress(req.clone()).await;
        assert!(
            first.is_err(),
            "a standby must answer the progress tick UNAVAILABLE, not ack it"
        );

        // The rollout completes; the abandoned connection redials and
        // the next tick reaches the leader.
        *backend.lock().unwrap() = leader_addr;
        let mut acked = false;
        for _ in 0..5 {
            if transport.report_progress(req.clone()).await.is_ok() {
                acked = true;
                break;
            }
        }
        assert!(
            acked,
            "the progress relay must converge on the leader after a rollout \
             (abandon-on-UNAVAILABLE redial)"
        );
    }
}
