//! The materialization executor's scheduler client: poll → fenced
//! claim → report (substitution-replacement design §2.2 item 1).
//!
//! Transport is abstracted behind [`MaterializeTransport`] (the builder
//! runtime's `PullTransport` precedent — copied shape, not shared code)
//! so the claim/report state machines are unit-testable against a
//! scripted mock with no wire and no scheduler.
// r[impl store.materialize.executor+3]

use std::time::Duration;

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
) -> Vec<ClaimedJob> {
    if available_slots == 0 {
        return Vec::new();
    }
    // Saturating u32 cast: slots are single-digit in practice.
    let limit = u32::try_from(available_slots).unwrap_or(u32::MAX);
    let listed = match transport
        .list_jobs(ListMaterializationJobsRequest {
            // The credential rides the x-rio-service-token metadata
            // (attached by the transport); the body field stays empty.
            service_token: String::new(),
            limit,
        })
        .await
    {
        Ok(resp) => resp.jobs,
        Err(status) => {
            debug!(code = ?status.code(), msg = status.message(),
                   "ListMaterializationJobs failed; empty poll pass");
            return Vec::new();
        }
    };

    let mut claimed = Vec::new();
    for descriptor in listed.into_iter().take(available_slots) {
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
        };
        match transport.pull(req).await {
            Ok(resp) => match resp.outcome {
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
            Err(status) => {
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
) -> bool {
    let started = tokio::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        let req = ReportOutcomeRequest {
            exec_id: exec_id.to_owned(),
            report: None,
            materialization_outcome: Some(outcome.clone()),
        };
        match transport.report(req).await {
            Ok(()) => return true,
            Err(status) if is_fatal_rejection(status.code()) => {
                warn!(code = ?status.code(), msg = status.message(),
                      "materialization ReportOutcome permanently rejected; giving up \
                       (the establishment sweep is the scheduler-side backstop)");
                return false;
            }
            Err(status) => {
                if started.elapsed() >= budget {
                    warn!(code = ?status.code(), msg = status.message(),
                          "materialization ReportOutcome never acknowledged within the budget");
                    return false;
                }
                debug!(code = ?status.code(), msg = status.message(),
                       "materialization ReportOutcome not acknowledged; retrying");
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(REPORT_RETRY_ENVELOPE.duration(attempt - 1)).await;
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
            }
        }
    }

    impl MaterializeTransport for MockTransport {
        async fn list_jobs(
            &mut self,
            _req: ListMaterializationJobsRequest,
        ) -> Result<ListMaterializationJobsResponse, tonic::Status> {
            self.list_calls += 1;
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
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8).await;
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
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8).await;
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
        let claimed = poll_and_claim(&mut t, "store-replica-0", 8).await;
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
        let claimed = poll_and_claim(&mut t, "store-replica-0", 2).await;
        assert_eq!(claimed.len(), 2);
        assert_eq!(
            t.pull_calls, 2,
            "claims are bounded by available slots, not by the listing size"
        );

        // Zero slots → no RPCs at all.
        let mut idle = MockTransport::new(vec![], vec![], vec![]);
        let claimed = poll_and_claim(&mut idle, "store-replica-0", 0).await;
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
        let _ = poll_and_claim(&mut t, "store-replica-7", 8).await;
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
        let acked =
            report_until_acked(&mut t, "exec-1", outcome.clone(), Duration::from_secs(600)).await;
        assert!(acked);
        assert_eq!(t.report_calls, 3);

        // Permanent rejection → exactly one call, false.
        let mut t = MockTransport::new(
            vec![],
            vec![],
            vec![Err(tonic::Status::permission_denied("bad credential"))],
        );
        let acked =
            report_until_acked(&mut t, "exec-2", outcome.clone(), Duration::from_secs(600)).await;
        assert!(!acked);
        assert_eq!(t.report_calls, 1, "permanent rejections are never retried");

        // Budget exhaustion → false after spending the window.
        let mut t = MockTransport::new(
            vec![],
            vec![],
            vec![Err(tonic::Status::unavailable("scheduler gone"))],
        );
        let acked = report_until_acked(&mut t, "exec-3", outcome, Duration::from_secs(60)).await;
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
        let claimed = poll_and_claim(&mut transport, "store-replica-0", 1).await;
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
            claimed = poll_and_claim(&mut transport, "store-replica-0", 1).await;
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
        let _ = poll_and_claim(&mut transport, "store-replica-0", 1).await;
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
        )
        .await;
        assert!(
            acked,
            "an outcome report must converge on the leader after a rollout \
             instead of burning its budget against the pinned standby"
        );
    }
}
