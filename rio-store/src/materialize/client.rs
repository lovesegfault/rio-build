//! The materialization executor's scheduler client: poll → fenced
//! claim → report (substitution-replacement design §2.2 item 1).
//!
//! Transport is abstracted behind [`MaterializeTransport`] (the builder
//! runtime's `PullTransport` precedent — copied shape, not shared code)
//! so the claim/report state machines are unit-testable against a
//! scripted mock with no wire and no scheduler.
// r[impl store.materialize.executor+5]

use std::collections::VecDeque;
use std::time::Duration;

use rio_common::grpc::DEFAULT_GRPC_TIMEOUT;
use rio_common::transport::{AttemptBudget, BoundedOutcome, SIGTERM_FINAL_ATTEMPT, bounded};
use tracing::{debug, info, warn};
use uuid::Uuid;

use rio_proto::types::{
    ListMaterializationJobsRequest, ListMaterializationJobsResponse, PullAssignmentRequest,
    PullAssignmentResponse, ReportMaterializationProgressRequest, ReportOutcomeRequest,
    pull_assignment_response,
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

/// The client-side identity a pull was issued under: the listing
/// descriptor (fresh claims) or the resume-ledger entry (nonce
/// resumes). [`ClaimedJob::bind`] joins it with the delivered
/// assignment — the only place the two views meet.
struct ExpectedJob {
    job_id: Uuid,
    drv_hash: String,
    tenant_hint: Option<Uuid>,
    origin: String,
}

impl ClaimedJob {
    /// merged_bug_026 — the ONE binding site joining a delivered
    /// assignment with the client-side identity view the pull was
    /// issued under. The kernel's Pending arm can lawfully answer a
    /// nonce-presenting pull with a delivery minted for the job's
    /// SUCCESSOR, so the assignment's wire-echoed `job_id` (the
    /// producer-asserted binding) is authoritative whenever present:
    ///
    /// - wire == expected: the normal case — keep the recorded hints.
    /// - wire != expected: REBIND — key by the wire job; the entry's
    ///   tenant_hint/origin belong to a different job and are dropped
    ///   (hint `None` is the documented "no recorded context" state;
    ///   execution re-resolves against live interest, PDQ-8).
    /// - wire empty (pre-field scheduler / build-kind payload) or
    ///   unparseable (producer bug — logged loudly): fall back to the
    ///   expected identity, the pre-field behavior.
    fn bind(expected: ExpectedJob, assignment: &rio_proto::types::WorkAssignment) -> Self {
        let (job_id, tenant_hint, origin) = match assignment.job_id.as_str() {
            "" => (expected.job_id, expected.tenant_hint, expected.origin),
            wire => match Uuid::parse_str(wire) {
                Ok(wire_job) if wire_job == expected.job_id => {
                    (expected.job_id, expected.tenant_hint, expected.origin)
                }
                Ok(wire_job) => {
                    info!(
                        expected_job = %expected.job_id, wire_job = %wire_job,
                        drv_path = %assignment.drv_path,
                        "delivery rebound to the wire-echoed job (successor \
                         delivered through the Pending arm); stale hints dropped"
                    );
                    (wire_job, None, REBOUND_ORIGIN.to_owned())
                }
                Err(err) => {
                    warn!(
                        job_id = %assignment.job_id, %err,
                        "malformed wire job_id on a delivered assignment \
                         (scheduler-side bug); keying by the client-side identity"
                    );
                    (expected.job_id, expected.tenant_hint, expected.origin)
                }
            },
        };
        ClaimedJob {
            job_id,
            drv_hash: expected.drv_hash,
            tenant_hint,
            origin,
            exec_id: assignment.exec_id.clone(),
            drv_path: assignment.drv_path.clone(),
        }
    }
}

/// bug_116 — the accrued-claims accumulator. `poll_and_claim` returns
/// ITS OWN accumulator (constructed exactly once at entry): every exit
/// arm — including the three listing-failure arms that previously
/// returned a fresh empty vec, discarding assignments the resume pass
/// had already claimed and ledger-resolved — returns the same set, so
/// the discard is structurally unwritable rather than
/// convention-protected (`return Vec::new()` no longer typechecks
/// against the return type).
#[must_use = "claimed assignments must be executed or aborted — dropping them strands open attempts"]
pub struct ClaimedSet(Vec<ClaimedJob>);

impl ClaimedSet {
    /// The ONE construction site, called at `poll_and_claim` entry.
    fn begin() -> Self {
        ClaimedSet(Vec::new())
    }

    fn push(&mut self, job: ClaimedJob) {
        self.0.push(job);
    }
}

impl std::ops::Deref for ClaimedSet {
    type Target = [ClaimedJob];
    fn deref(&self) -> &[ClaimedJob] {
        &self.0
    }
}

impl IntoIterator for ClaimedSet {
    type Item = ClaimedJob;
    type IntoIter = std::vec::IntoIter<ClaimedJob>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Origin sentinel stamped when a delivery's wire-echoed job binding
/// (`WorkAssignment.job_id`, merged_bug_026) names a DIFFERENT job
/// than the client-side identity the pull was issued under: the
/// successor job's true origin is unknown client-side, and carrying
/// the stale entry's origin would mis-attribute the execution.
pub const REBOUND_ORIGIN: &str = "resume_rebound";

// r[impl sched.materialize.claim-resume]
/// bug_251 (rule-4b, SIGNED 2026-06-04) — the bounded per-worker
/// resume ledger, the client half of the lost-response credential.
///
/// One entry per claim attempt whose ANSWER never arrived (timeout /
/// transport error): the v4 nonce was minted BEFORE the pull rode the
/// wire, so the scheduler may have committed an attempt that persisted
/// it (`assignments.claim_nonce`, migration 096) while the response —
/// and the `exec_id` resume token it carried — died in flight. A
/// minted attempt leaves the claimable listing (it is open, held by
/// THIS replica), so re-listing can never find it again: the next
/// pass instead issues DIRECT resume pulls for ledger entries,
/// presenting the nonce; the kernel's credential disjunction
/// re-delivers and the worker recovers the assignment.
///
/// Entries leave on every AUTHORITATIVE answer (Assignment — we hold
/// the job; Gone — resolved/absent; Rejected — MINT-DISPROVING
/// refusal (InvalidArgument/Unimplemented), no mint behind it;
/// auth-layer rejections file as Unanswered — merged_bug_074: the
/// rotation-skew codes judge the presentation, not the mint);
/// capacity REFUSES new mints instead
/// of evicting (merged_bug_072). `NotYetReady` KEEPS the entry on BOTH passes
/// (merged_bug_096): the job may be parked, raced to another replica
/// (their resolution answers Gone later), seen through a mid-recovery
/// stale view — or OUR OWN committed mint answered through the
/// scheduler's post-mint TOCTOU arm, which says NotYetReady after
/// persisting this worker's nonce. The retry costs one bounded RPC
/// per pass. A
/// process-lost entry settles through the charged establishment
/// window — the SIGNED residual (rule-4b): real crashes still pay,
/// lost responses no longer do.
#[derive(Default)]
pub struct ResumeLedger {
    entries: VecDeque<ResumeEntry>,
}

/// One unanswered claim: everything the resume pull and the recovered
/// [`ClaimedJob`] need from the original listing descriptor, plus the
/// minted nonce.
#[derive(Clone)]
struct ResumeEntry {
    job_id: Uuid,
    drv_hash: String,
    tenant_hint: Option<Uuid>,
    origin: String,
    nonce: Uuid,
}

/// Ledger capacity. At cap the mint authority REFUSES fresh mints
/// (merged_bug_072 — live credentials are never evicted); 32
/// unanswered claims on ONE worker already signals a scheduler-side
/// outage the establishment sweep owns.
const RESUME_LEDGER_CAP: usize = 32;

impl ResumeLedger {
    /// Record a claim attempt BEFORE its pull rides the wire (upsert
    /// by job: a re-claim re-mints, and exactly one nonce per job is
    /// ever live on this worker). TEST-ONLY since merged_bug_096:
    /// production fresh mints go through [`Self::begin_fresh_claim`]
    /// (the mint authority); tests use this raw insert to seed
    /// pre-existing entries.
    #[cfg(test)]
    fn note_pull(&mut self, entry: ResumeEntry) {
        self.entries.retain(|e| e.job_id != entry.job_id);
        // merged_bug_072: eviction is gone from the production API;
        // test seeding stays within capacity by construction.
        assert!(
            self.entries.len() < RESUME_LEDGER_CAP,
            "test seeded past RESUME_LEDGER_CAP"
        );
        self.entries.push_back(entry);
    }

    /// An ANSWERED outcome for the job — drop its entry.
    fn resolve(&mut self, job_id: Uuid) {
        self.entries.retain(|e| e.job_id != job_id);
    }

    /// merged_bug_096 — the SOLE fresh-mint authority. A claim pull
    /// for a job can only be issued through its ledger entry: one
    /// nonce per job lifecycle. Returns `None` when a LIVE entry
    /// exists — the fresh pass then SKIPS the job (the resume pass at
    /// the head of the same poll already presented the live
    /// credential; minting here would clobber the only proof of a
    /// possibly-committed server-side mint). The skip falls out of
    /// the API instead of being a remembered check.
    fn begin_fresh_claim(
        &mut self,
        job_id: Uuid,
        fill: impl FnOnce(Uuid) -> ResumeEntry,
    ) -> Option<MintedClaim> {
        if self.entries.iter().any(|e| e.job_id == job_id) {
            return None;
        }
        // merged_bug_072: at capacity the mint authority REFUSES —
        // every live entry is a possibly-committed rule-4b credential
        // (the only proof of a server-side mint bound to this
        // worker); evicting one to fund a NEW speculative mint
        // forfeited a possibly-committed attempt to the charged
        // establishment window. 32 unanswered claims on one worker
        // already signals a scheduler-side outage the establishment
        // sweep owns.
        if self.entries.len() >= RESUME_LEDGER_CAP {
            warn!(job_id = %job_id,
                  "resume ledger at capacity; fresh mint refused (live \
                   credentials are never evicted)");
            return None;
        }
        let nonce = Uuid::new_v4();
        self.entries.push_back(fill(nonce));
        Some(MintedClaim { nonce })
    }

    /// Snapshot for the resume pass (entries are a handful of small
    /// strings; the pass mutates the ledger per answer).
    fn snapshot(&self) -> Vec<ResumeEntry> {
        self.entries.iter().cloned().collect()
    }

    /// Test/diagnostic visibility: unanswered claims currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no unanswered claim is outstanding.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A fresh-claim credential minted THROUGH the ledger
/// ([`ResumeLedger::begin_fresh_claim`] is the only constructor): the
/// claim request's nonce field is filled from this token, so a fresh
/// pull that did not register its credential in the ledger does not
/// typecheck (merged_bug_096 — the ledger is the mint authority as an
/// API, not a convention).
struct MintedClaim {
    nonce: Uuid,
}

impl MintedClaim {
    fn nonce_string(&self) -> String {
        self.nonce.to_string()
    }
}

/// One claim attempt's wire verdict — shared by the listing pass and
/// the resume pass so the two cannot drift on outcome semantics.
enum PullAnswer {
    /// Boxed: the assignment dwarfs every other variant (~224 bytes vs
    /// zero) and the enum rides through match arms by value.
    Deliver(Box<rio_proto::types::WorkAssignment>),
    /// Answered: the job is resolved/absent — authoritative stop.
    Gone,
    /// Answered: refused for now (race lost, parked, not ready).
    NotYetReady,
    /// SIGTERM raced the pull — end the pass.
    Shutdown,
    /// The answer never arrived (timeout / transport error): a mint
    /// may have committed server-side — exactly the window the
    /// resume ledger exists for.
    Unanswered,
    /// ANSWERED with a MINT-DISPROVING rejection (merged_bug_074
    /// narrowing of bug_119: InvalidArgument / Unimplemented — the
    /// request shape itself can never mint, on this pull or the
    /// original one). The scheduler answered AND the answer disproves
    /// a pending mint: never filed as a lost response, and the
    /// callers resolve the ledger entry.
    RejectedDisproving,
    /// ANSWERED with an AUTH-LAYER rejection (PermissionDenied /
    /// Unauthenticated — the codes the scheduler's credential layer
    /// emits for transient HMAC rotation skew, without consulting
    /// attempt state). The rejection judges the credential
    /// PRESENTATION, not the mint: on a RESUME presentation the
    /// original unanswered pull may still have committed a mint, so
    /// the resume arm KEEPS the entry (Unanswered disposition — one
    /// bounded RPC per pass, self-resolving via Gone/delivery once
    /// the skew clears); the FRESH arm resolves (its gates run
    /// pre-mint, so nothing can be pending behind the refusal).
    RejectedAuth,
}

/// Issue one bounded `PullAssignment` and classify the outcome.
async fn pull_once<T: MaterializeTransport>(
    transport: &mut T,
    shutdown: &rio_common::signal::Token,
    req: PullAssignmentRequest,
    drv_hash: &str,
) -> PullAnswer {
    match bounded(shutdown, DEFAULT_GRPC_TIMEOUT, transport.pull(req)).await {
        BoundedOutcome::Shutdown => PullAnswer::Shutdown,
        BoundedOutcome::TimedOut { after } => {
            // bug_251: a minted attempt leaves the claimable listing,
            // so "the next poll re-lists" was FALSE for the timeout
            // case — the nonce in the resume ledger is what recovers
            // it now.
            warn!(drv_hash = %drv_hash, after_secs = after.as_secs(),
                  "materialization claim unanswered; nonce recorded — the next pass \
                   resumes it directly");
            transport.note_timeout();
            PullAnswer::Unanswered
        }
        BoundedOutcome::Resolved(Ok(resp)) => match resp.outcome {
            Some(pull_assignment_response::Outcome::Assignment(assignment)) => {
                PullAnswer::Deliver(Box::new(assignment))
            }
            Some(pull_assignment_response::Outcome::Gone(_)) => PullAnswer::Gone,
            Some(pull_assignment_response::Outcome::NotYetReady(_)) | None => {
                debug!(drv_hash = %drv_hash,
                       "materialization claim not delivered (race lost / not ready)");
                PullAnswer::NotYetReady
            }
        },
        // bug_119 + merged_bug_074: the ONE rpc-error classification
        // chokepoint — an ANSWERED refusal is typed by WHAT IT
        // DISPROVES, never laundered into the lost-response lane.
        // InvalidArgument/Unimplemented disprove a mint (the request
        // shape can never mint); PermissionDenied/Unauthenticated
        // judge only the credential presentation (the scheduler's
        // rotation-skew trace) and disprove nothing about a mint the
        // ORIGINAL pull may have committed. The two sets partition
        // is_fatal_rejection exactly, so the report leg's give-up
        // semantics are untouched.
        BoundedOutcome::Resolved(Err(status))
            if matches!(
                status.code(),
                tonic::Code::InvalidArgument | tonic::Code::Unimplemented
            ) =>
        {
            warn!(drv_hash = %drv_hash,
                  code = ?status.code(), msg = status.message(),
                  "materialization claim refused with a mint-disproving \
                   rejection; dropping the claim — no mint can be pending \
                   behind it");
            PullAnswer::RejectedDisproving
        }
        BoundedOutcome::Resolved(Err(status))
            if matches!(
                status.code(),
                tonic::Code::PermissionDenied | tonic::Code::Unauthenticated
            ) =>
        {
            warn!(drv_hash = %drv_hash,
                  code = ?status.code(), msg = status.message(),
                  "materialization claim refused at the auth layer \
                   (rotation skew?); the rejection judges the presentation, \
                   not the mint");
            PullAnswer::RejectedAuth
        }
        BoundedOutcome::Resolved(Err(status)) => {
            warn!(drv_hash = %drv_hash,
                  code = ?status.code(), msg = status.message(),
                  "materialization claim RPC failed; nonce recorded — the next pass \
                   resumes it directly");
            PullAnswer::Unanswered
        }
    }
}

/// Consecutive `ListMaterializationJobs` failures before the latch
/// escalates to `warn!`. One or two failures are routine (rollout,
/// standby re-roll, transient blip); three in a row is a persistently
/// dead store→scheduler edge — exactly the bug_257 outage shape that
/// previously surfaced at `debug!` only.
const LIST_FAILURE_WARN_THRESHOLD: u32 = 3;

/// Warn-once latch for persistent listing failures (bug_257 rider).
///
/// The per-pass failure logs stay at `debug!` (an empty poll pass is
/// routine); this latch owns the ESCALATION: when failures become
/// consecutive past `LIST_FAILURE_WARN_THRESHOLD` (private const,
/// currently 3) it emits ONE
/// `warn!`, then stays silent until a success resets it (recovery is
/// logged at `info!`), so a dead edge surfaces above debug without a
/// warn-per-pass flood.
#[derive(Default)]
pub struct ListFailureLatch {
    consecutive: u32,
    warned: bool,
}

impl ListFailureLatch {
    /// A failed (or unanswered) listing pass.
    fn note_failure(&mut self, detail: &str) {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= LIST_FAILURE_WARN_THRESHOLD && !self.warned {
            self.warned = true;
            warn!(
                consecutive = self.consecutive,
                last_error = detail,
                "ListMaterializationJobs failing persistently; store→scheduler edge \
                 down — no materialization jobs are being claimed by this worker"
            );
        }
    }

    /// A successful listing pass: reset and (if escalated) log recovery.
    fn note_success(&mut self) {
        if self.warned {
            info!(
                after_failures = self.consecutive,
                "ListMaterializationJobs recovered"
            );
        }
        self.consecutive = 0;
        self.warned = false;
    }

    /// Test visibility: has the latch escalated to `warn!`?
    #[cfg(test)]
    fn warned(&self) -> bool {
        self.warned
    }
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
/// are logged and skipped; a failed listing yields an empty pass (the
/// `list_health` latch escalates once the failures become persistent).
// r[impl store.materialize.executor+5]
pub async fn poll_and_claim<T: MaterializeTransport>(
    transport: &mut T,
    executor_instance: &rio_common::dns::Dns1123Label,
    available_slots: usize,
    ledger: &mut ResumeLedger,
    list_health: &mut ListFailureLatch,
    shutdown: &rio_common::signal::Token,
) -> ClaimedSet {
    // The accumulator is constructed ONCE; every exit below returns it
    // (bug_116 — the listing-failure arms can no longer fabricate an
    // empty result over accrued claims).
    let mut claimed = ClaimedSet::begin();
    if available_slots == 0 {
        return claimed;
    }

    // bug_251: the RESUME pass runs FIRST — unanswered claims from
    // prior passes are existing obligations (the scheduler may hold an
    // open attempt minted for this replica that no listing will ever
    // show again). Each resume pull presents the persisted nonce; the
    // kernel's credential disjunction re-delivers.
    for entry in ledger.snapshot() {
        if claimed.len() >= available_slots {
            break;
        }
        let req = PullAssignmentRequest {
            executor_token: String::new(),
            intent_id: entry.drv_hash.clone(),
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: executor_instance.as_str().to_owned(),
            // The resume credential: the original response never
            // arrived, so no exec_id token exists — the nonce IS the
            // proof of holdership (rule-4b).
            resume_exec_id: String::new(),
            claim_nonce: entry.nonce.to_string(),
            // A resume is a CLAIMING pull (the nonce credential may
            // re-deliver); never a confirm probe.
            confirm_only: false,
        };
        match pull_once(transport, shutdown, req, &entry.drv_hash).await {
            PullAnswer::Shutdown => return claimed,
            PullAnswer::Deliver(assignment) => {
                info!(drv_hash = %entry.drv_hash, exec_id = %assignment.exec_id,
                      "lost-response claim resumed via nonce (rule-4b)");
                ledger.resolve(entry.job_id);
                claimed.push(ClaimedJob::bind(
                    ExpectedJob {
                        job_id: entry.job_id,
                        drv_hash: entry.drv_hash,
                        tenant_hint: entry.tenant_hint,
                        origin: entry.origin,
                    },
                    &assignment,
                ));
            }
            // Authoritative: the job resolved without us.
            PullAnswer::Gone => ledger.resolve(entry.job_id),
            // Parked / raced / stale view: keep — one bounded RPC per
            // pass until Gone or delivery; capacity bounds the total.
            PullAnswer::NotYetReady => {}
            PullAnswer::Unanswered => {}
            // bug_119 (narrowed by merged_bug_074): a MINT-DISPROVING
            // refusal resolves the entry — the request shape can
            // never mint, so nothing is pending behind it.
            PullAnswer::RejectedDisproving => ledger.resolve(entry.job_id),
            // merged_bug_074: an auth-layer refusal judges the
            // PRESENTATION (rotation skew), not the mint the ORIGINAL
            // unanswered pull may have committed — the entry is the
            // only rule-4b recovery credential for that mint, so it
            // SURVIVES (Unanswered disposition: one bounded RPC per
            // pass; Gone or delivery resolves it once the skew
            // clears).
            PullAnswer::RejectedAuth => {}
        }
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
    // Budget already consumed by the resume pass → the listing RPC is
    // pure cost (the claim loop below could not claim anything).
    if claimed.len() >= available_slots {
        return claimed;
    }
    let listed = match bounded(
        shutdown,
        DEFAULT_GRPC_TIMEOUT,
        transport.list_jobs(list_req),
    )
    .await
    {
        BoundedOutcome::Shutdown => return claimed,
        BoundedOutcome::TimedOut { after } => {
            debug!(
                after_secs = after.as_secs(),
                "ListMaterializationJobs unanswered; empty poll pass"
            );
            list_health.note_failure("timed out (no answer)");
            transport.note_timeout();
            return claimed;
        }
        BoundedOutcome::Resolved(Ok(resp)) => {
            list_health.note_success();
            resp.jobs
        }
        BoundedOutcome::Resolved(Err(status)) => {
            debug!(code = ?status.code(), msg = status.message(),
                   "ListMaterializationJobs failed; empty poll pass");
            list_health.note_failure(&format!("{:?}: {}", status.code(), status.message()));
            return claimed;
        }
    };

    // bug_099: the walk's budget counts POTENTIAL server-side mints —
    // every nonce issued whose outcome the scheduler has not answered.
    // An ANSWERED refusal (Gone / NotYetReady / Rejected) refunds the
    // slot (the scheduler affirmatively did not mint — bug_385's
    // in-pass skip stays free); an UNANSWERED pull keeps it consumed
    // (the mint may have committed server-side, bound to this worker).
    // Pre-fix, a mailbox brownout let a 1-slot worker mint a nonce per
    // listed descriptor (16+ open attempts the resume lane drains at
    // one per pass).
    // merged_bug_072: the budget is DERIVED from the ledger
    // population — entries are created at mint and leave on every
    // authoritative answer, so `ledger.len()` IS the outstanding-mint
    // count, across passes as well as within one. The pre-fix
    // per-pass counter reset to 0 each pass, making prior-pass
    // Unanswered entries invisible: a list-ok/pull-lost brownout
    // minted one fresh nonce per pass up to RESUME_LEDGER_CAP (32x a
    // 1-slot worker), each eviction then forfeiting a live rule-4b
    // credential. No parallel counter exists to desync (banner a).
    for descriptor in listed {
        // The claim budget: delivered claims plus unanswered potential
        // mints (the surviving ledger population).
        if claimed.len() + ledger.len() >= available_slots {
            debug!(
                claimed = claimed.len(),
                outstanding = ledger.len(),
                slots = available_slots,
                "claim budget consumed by outstanding mints; fresh pass ends"
            );
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
        // bug_251 (rule-4b): mint the claim nonce and record it in the
        // resume ledger BEFORE the pull rides the wire. If the answer
        // never arrives, the scheduler may still have committed the
        // mint WITH this nonce persisted (`assignments.claim_nonce`) —
        // the ledger entry is then the credential that recovers the
        // attempt on the next pass. merged_bug_096: the mint goes
        // THROUGH the ledger — a job with a live entry is skipped
        // (its credential was already presented by the resume pass
        // this very poll; a second mint would clobber it).
        let Some(minted) = ledger.begin_fresh_claim(job_id, |nonce| ResumeEntry {
            job_id,
            drv_hash: descriptor.drv_hash.clone(),
            tenant_hint: Uuid::parse_str(&descriptor.tenant_id).ok(),
            origin: descriptor.origin.clone(),
            nonce,
        }) else {
            continue;
        };
        let req = PullAssignmentRequest {
            // No executor token: the store's credential is the
            // service token in metadata (the kind-attested credential).
            executor_token: String::new(),
            intent_id: descriptor.drv_hash.clone(),
            // BC-1: the work class + the per-replica identity ride
            // every claim.
            kind: rio_proto::types::AttemptKind::Materialization.into(),
            executor_instance: executor_instance.as_str().to_owned(),
            // Fresh claims NEVER carry a resume token (merged_bug_158:
            // re-delivery of a Claimed attempt requires a credential,
            // so a colliding identity cannot steal it). A
            // crashed-and-restarted worker has neither exec id nor
            // ledger: its credential-less re-pull answers NotYetReady
            // and the attempt settles through the establishment
            // window (the T-0e.6 rule-4 amendment — status at the
            // executor-invariant-map.md rule-4 anchor).
            resume_exec_id: String::new(),
            claim_nonce: minted.nonce_string(),
            confirm_only: false,
        };
        match pull_once(transport, shutdown, req, &descriptor.drv_hash).await {
            // SIGTERM mid-pass: return what was already claimed so the
            // caller can abort/report those attempts under the grace.
            PullAnswer::Shutdown => return claimed,
            PullAnswer::Deliver(assignment) => {
                ledger.resolve(job_id);
                // merged_bug_026 (fresh-claim sibling site): the same
                // Pending-arm race exists between list and claim — the
                // listed job can resolve and a successor mint answer
                // this pull. The wire binding is authoritative here too.
                claimed.push(ClaimedJob::bind(
                    ExpectedJob {
                        job_id,
                        drv_hash: descriptor.drv_hash,
                        tenant_hint: Uuid::parse_str(&descriptor.tenant_id).ok(),
                        origin: descriptor.origin,
                    },
                    &assignment,
                ));
            }
            // AUTHORITATIVE no-mint answers on a FRESH claim resolve
            // the credential: Gone (job settled without us) and BOTH
            // rejection flavors (bug_119 / merged_bug_074 — the fresh
            // pull's gates run before any mint, so even an auth-layer
            // refusal disproves a mint HERE: this very pull was the
            // only one that could have minted).
            PullAnswer::Gone | PullAnswer::RejectedDisproving | PullAnswer::RejectedAuth => {
                ledger.resolve(job_id)
            }
            // merged_bug_096: NotYetReady is NOT proof of no-mint —
            // the scheduler's post-mint TOCTOU arm answers it AFTER
            // the durable mint committed with this nonce. The
            // credential survives (one bounded resume RPC per pass
            // until Gone or delivery; cap bounds the total); the
            // PASS budget refunds — the possibly-minted attempt is
            // ledger-tracked now, and starving the walk behind a
            // raced head would undo bug_385's in-pass skip.
            // merged_bug_072: the credential survives in the ledger
            // and therefore KEEPS consuming budget — the possibly-
            // minted attempt holds a slot until answered (the
            // pre-fix per-pass refund let the next pass mint over
            // it).
            PullAnswer::NotYetReady => {}
            // The answer never arrived: the entry STAYS — the next
            // pass resumes it directly with the nonce.
            PullAnswer::Unanswered => {}
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
// r[impl store.materialize.executor+5]
pub async fn report_until_acked<T: MaterializeTransport>(
    transport: &mut T,
    exec_id: &str,
    outcome: super::executor::CountedOutcome,
    budget: Duration,
    shutdown: &rio_common::signal::Token,
) -> bool {
    let budget = AttemptBudget::new(budget);
    // Consume the witness once: the retry loop re-sends the SAME
    // counted outcome (one execution = one count, N report attempts).
    let outcome = outcome.into_outcome();
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
/// A bare `host:port` scheduler address, the ONLY currency
/// [`SchedulerTransport`] accepts (bug_257, parse-don't-validate).
///
/// Why this type exists: `rio_proto::client::build_endpoint` prepends
/// `http://` unconditionally, so a URL-form config value
/// (`http://rio-scheduler.rio-system:9001` — the same shape as the
/// sibling helm values `prometheusAddress`/`logPeerUrlTemplate`)
/// composed to `http://http://…`, which the http crate parses *Ok*
/// (host `http`). The executor booted "enabled", every
/// `ListMaterializationJobs` failed at `debug!` level only, and zero
/// jobs were claimed fleet-wide with no WARN anywhere. With the
/// constructor below as the sole way to mint one, the silent
/// double-prefix is unrepresentable in the transport layer.
///
/// The config field stays a `String` (PD-D2 never-fatal posture): a
/// bad value disables the executor loudly at the spawn boundary
/// instead of aborting the store's data plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPort(String);

impl HostPort {
    /// Accept exactly the `host:port` shape (IPv6 bracket form
    /// included); reject schemes, paths, queries, userinfo, and
    /// port-less authorities. The acceptance property (pinned by
    /// `hostport_accepts_iff_authority_roundtrips` below): composing
    /// `http://` onto an accepted value yields a URI whose authority
    /// is byte-identical to the input — i.e. the transport dials
    /// exactly what the operator wrote.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        if raw.contains("://") {
            anyhow::bail!(
                "scheme-bearing address (expected bare host:port — the transport \
                 prepends http:// itself; a scheme here composes the silent \
                 double-prefix http://http://…)"
            );
        }
        let uri: http::Uri = format!("http://{raw}")
            .parse()
            .map_err(|e| anyhow::anyhow!("not a valid host:port: {e}"))?;
        let authority = uri
            .authority()
            .ok_or_else(|| anyhow::anyhow!("no authority parsed from {raw:?}"))?;
        if authority.as_str() != raw {
            anyhow::bail!(
                "not a bare host:port: parsed authority {:?} differs from input \
                 (path/fragment/trailing content?)",
                authority.as_str()
            );
        }
        if uri.path() != "/" || uri.query().is_some() {
            anyhow::bail!("not a bare host:port: path/query present");
        }
        if raw.contains('@') {
            anyhow::bail!("userinfo not allowed in a host:port address");
        }
        if uri.port_u16().is_none() {
            anyhow::bail!(
                "missing port (every deployed form is host:port, e.g. \
                 rio-scheduler.rio-system:9001 — a port-less value is a config bug)"
            );
        }
        Ok(Self(raw.to_owned()))
    }

    /// The validated `host:port` string (what the channel dials, minus
    /// the scheme the endpoint builder adds).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone)]
pub struct SchedulerTransport {
    client: ExecutorClient,
    /// Constructor inputs, retained so [`Self::abandon_connection`] can
    /// rebuild the channel when the current connection is pinned to a
    /// peer that cannot serve (finding 18: the standby replica after a
    /// scheduler Deployment rollout).
    scheduler_addr: HostPort,
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
        scheduler_addr: &HostPort,
        signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
        instance: &rio_common::dns::Dns1123Label,
    ) -> anyhow::Result<Self> {
        let client = Self::build_client(scheduler_addr, signer.clone(), instance.as_str())?;
        Ok(Self {
            client,
            scheduler_addr: scheduler_addr.clone(),
            signer,
            instance: instance.as_str().to_owned(),
        })
    }

    /// The channel/interceptor/client stack shared by construction and
    /// [`Self::abandon_connection`]. Takes [`HostPort`] — the
    /// double-prefix composition bug_257 hit is unrepresentable here.
    fn build_client(
        scheduler_addr: &HostPort,
        signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
        instance: &str,
    ) -> anyhow::Result<ExecutorClient> {
        // Sanctioned channel construction: connect timeout, h2 window
        // tuning, and the hoisted keepalive all come from rio-proto's
        // chokepoint (h2-keepalive-single-source pins the knobs there —
        // this used to hand-chain the same values and drifted out of
        // the single-source set).
        let channel = rio_proto::client::connect_lazy_channel(scheduler_addr.as_str())?;
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

    /// Inspect one RPC outcome: a BARE `UNAVAILABLE` answer abandons
    /// the connection (see [`Self::abandon_connection`]) — that shape
    /// means wrong/standby peer (finding 18). An UNAVAILABLE carrying
    /// the leader-NACK marker (merged_bug_031:
    /// `x-rio-leader-nack`, e.g. the bug_182 consumption-not-durable
    /// NACK) KEEPS the connection: only the serving leader's
    /// consumption path emits it, so the refusal itself proves the
    /// pinned peer is the leader — abandoning would re-roll AWAY from
    /// it and halve leader-landing odds inside the report retry
    /// budget. Every other outcome — success or a different
    /// rejection — keeps the connection too.
    fn note_rpc_outcome<T>(&mut self, result: &Result<T, tonic::Status>) {
        if let Err(status) = result
            && Self::should_abandon(status)
        {
            debug!(
                msg = status.message(),
                "scheduler answered UNAVAILABLE; abandoning the pinned connection \
                 (rollout/standby recovery)"
            );
            self.abandon_connection();
        }
    }

    /// merged_bug_031 — the ONE abandon decision: retry-class
    /// (metadata) separated from peer-identity (status code), so a
    /// new leader-emitted NACK can never alias the
    /// abandon-connection trigger.
    fn should_abandon(status: &tonic::Status) -> bool {
        status.code() == tonic::Code::Unavailable && !rio_common::grpc::is_leader_nack(status)
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
    use rio_proto::types::MaterializationOutcome;

    /// merged_bug_243: poll_and_claim takes &Dns1123Label — tests mint
    /// theirs through the one sanitizer (valid raws pass through, so
    /// string assertions on the literal stay exact).
    fn instance(raw: &str) -> rio_common::dns::Dns1123Label {
        rio_common::dns::Dns1123Label::sanitize(
            raw,
            rio_common::dns::WORKER_SUFFIX_RESERVED,
            "rio-store-dev",
        )
    }
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
        /// bug_251: hang the next N pulls (request recorded, future
        /// never resolves) — with `start_paused` tokio time the
        /// bounded await elapses instantly, modeling a lost response.
        hang_next_pulls: u32,
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
                hang_next_pulls: 0,
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
            if self.hang_next_pulls > 0 {
                self.hang_next_pulls -= 1;
                // Lost response: the request reached the wire (it is
                // recorded above — the scheduler may act on it) but
                // no answer ever returns.
                std::future::pending::<()>().await;
            }
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

    /// merged_bug_072 RED 1: the fresh-claim budget must derive from
    /// the LEDGER population (outstanding = unanswered nonces), not a
    /// per-pass counter that resets — in a list-ok/pull-lost brownout
    /// a 1-slot worker must NOT mint a fresh nonce per pass.
    #[tokio::test(start_paused = true)]
    async fn brownout_mints_at_most_slot_budget_across_passes() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![
                Ok(ListMaterializationJobsResponse {
                    jobs: vec![job_a.clone()],
                }),
                Ok(ListMaterializationJobsResponse {
                    jobs: vec![job_b.clone()],
                }),
            ],
            vec![],
            vec![],
        );
        t.hang_next_pulls = 99;
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let tok = token();
        let inst = instance("brownout-w");
        // Pass 1: fresh mint for A rides the wire; the answer is lost.
        let c1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &tok).await;
        assert!(c1.is_empty());
        assert_eq!(ledger.len(), 1, "pass 1 minted A (unanswered)");
        // Pass 2: A's unanswered mint still consumes the only slot —
        // the fresh pass must not mint B (pre-fix: outstanding_mints
        // reset to 0 each pass, so every pass minted one more nonce,
        // accumulating to RESUME_LEDGER_CAP = 32x capacity).
        let c2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &tok).await;
        assert!(c2.is_empty());
        assert_eq!(
            ledger.len(),
            1,
            "the 1-slot budget is consumed by A's outstanding mint; B \
             must not be minted"
        );
    }

    /// merged_bug_072 RED 2: at capacity, begin_fresh_claim REFUSES
    /// (returns None) — it must never evict the OLDEST live rule-4b
    /// credential to fund a new speculative mint.
    #[test]
    fn fresh_claim_refuses_at_cap_never_evicts() {
        let mut ledger = ResumeLedger::default();
        let first_job = Uuid::now_v7();
        let fill = |job: Uuid| {
            move |nonce: Uuid| ResumeEntry {
                job_id: job,
                drv_hash: format!("drv-{job}"),
                tenant_hint: None,
                origin: "cache_opportunity".into(),
                nonce,
            }
        };
        assert!(
            ledger
                .begin_fresh_claim(first_job, fill(first_job))
                .is_some()
        );
        for _ in 1..RESUME_LEDGER_CAP {
            let j = Uuid::now_v7();
            assert!(ledger.begin_fresh_claim(j, fill(j)).is_some());
        }
        assert_eq!(ledger.len(), RESUME_LEDGER_CAP);
        // The 33rd mint: REFUSE, never evict.
        let extra = Uuid::now_v7();
        let got = ledger.begin_fresh_claim(extra, fill(extra));
        assert!(
            got.is_none(),
            "at cap the mint authority must refuse (got a minted claim — \
             pre-fix this evicted the oldest live credential)"
        );
        assert_eq!(ledger.len(), RESUME_LEDGER_CAP);
        assert!(
            ledger.snapshot().iter().any(|e| e.job_id == first_job),
            "the oldest credential must survive the refused mint"
        );
    }

    /// merged_bug_074 RED: an auth-layer rejection of a RESUME
    /// presentation (PermissionDenied — the scheduler's HMAC
    /// rotation-skew trace) judges the CREDENTIAL PRESENTATION, not
    /// the mint state: the ledger entry exists precisely because the
    /// original unanswered pull may have committed a mint, so the
    /// entry must SURVIVE the skew and the post-skew resume must
    /// recover the assignment. Pre-fix is_fatal_rejection folded
    /// auth codes into Rejected → ledger.resolve: the only rule-4b
    /// recovery credential evaporated exactly during fleet rotations
    /// and the attempt settled CHARGED through the establishment
    /// window.
    #[tokio::test(start_paused = true)]
    async fn auth_rejection_keeps_resume_credential_through_skew() {
        let job = descriptor(7);
        let mut t = MockTransport::new(
            vec![
                Ok(ListMaterializationJobsResponse {
                    jobs: vec![job.clone()],
                }),
                Ok(ListMaterializationJobsResponse { jobs: vec![] }),
                Ok(ListMaterializationJobsResponse { jobs: vec![] }),
            ],
            vec![
                // Pass 2's resume presentation: rotation skew.
                Err(tonic::Status::permission_denied(
                    "service token verified against no active key (rotation-skew trace)",
                )),
                // Pass 3's resume presentation: skew over, the
                // scheduler re-delivers the committed mint.
                Ok(deliver_for_job(
                    "exec-skew-recovered",
                    "/nix/store/skew-drv",
                    Uuid::nil(),
                )),
            ],
            vec![],
        );
        t.hang_next_pulls = 1; // pass 1's fresh mint: answer lost
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let tok = token();
        let inst = instance("skew-w");
        let c1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &tok).await;
        assert!(c1.is_empty());
        assert_eq!(ledger.len(), 1, "pass 1: unanswered mint recorded");
        // Pass 2: the resume presentation hits rotation skew.
        let c2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &tok).await;
        assert!(c2.is_empty());
        assert_eq!(
            ledger.len(),
            1,
            "an auth-layer rejection judges the presentation, not the \
             mint — the credential must survive rotation skew"
        );
        // Pass 3: skew over — the credential recovers the assignment.
        let c3 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &tok).await;
        assert_eq!(
            c3.len(),
            1,
            "post-skew resume re-delivers the committed mint"
        );
        assert!(ledger.is_empty(), "delivery resolves the entry");
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

    /// [`deliver`] with the merged_bug_026 producer-asserted job
    /// binding (`WorkAssignment.job_id`) populated.
    fn deliver_for_job(exec_id: &str, drv_path: &str, job_id: Uuid) -> PullAssignmentResponse {
        PullAssignmentResponse {
            outcome: Some(pull_assignment_response::Outcome::Assignment(
                rio_proto::types::WorkAssignment {
                    drv_path: drv_path.to_string(),
                    exec_id: exec_id.to_string(),
                    job_id: job_id.to_string(),
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
    // r[verify store.materialize.executor+5]
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
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            8,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
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

    /// merged_bug_031: the consumption-not-durable NACK is a
    /// LEADER-emitted refusal -- it must not tear down the healthy
    /// leader-pinned channel the way a bare standby UNAVAILABLE does.
    /// The typed signal is the x-rio-leader-nack metadata key; the
    /// abandon decision reads the marker, never the message text.
    #[test]
    fn leader_nack_keeps_the_pinned_connection() {
        // Bare UNAVAILABLE: standby/wrong-peer shape -- abandon.
        let bare = tonic::Status::unavailable("not leader (standby replica)");
        assert!(SchedulerTransport::should_abandon(&bare));

        // Marked UNAVAILABLE: the leader's own NACK -- keep.
        let mut nack = tonic::Status::unavailable("consumption close not durable");
        nack.metadata_mut().insert(
            rio_common::grpc::LEADER_NACK_METADATA_KEY,
            rio_common::grpc::LEADER_NACK_NOT_DURABLE.parse().unwrap(),
        );
        assert!(
            !SchedulerTransport::should_abandon(&nack),
            "a leader-emitted NACK proves the pinned peer IS the leader -- never abandon"
        );

        // Other codes never abandon (request-fault answers).
        assert!(!SchedulerTransport::should_abandon(
            &tonic::Status::permission_denied("nope")
        ));
    }

    /// merged_bug_096 (clobber half): the ledger is the SOLE mint
    /// authority — a fresh-claim pass cannot mint over a LIVE entry.
    /// Pre-fix, a job re-appearing in the listing (live PG anti-join +
    /// a brownout-delayed mint tx) had its persisted credential
    /// silently replaced by a fresh nonce (note_pull's upsert), which
    /// destroyed the only proof of the possibly-committed mint.
    #[tokio::test]
    async fn fresh_pass_cannot_mint_over_a_live_entry() {
        let job = Uuid::now_v7();
        let live_nonce = Uuid::new_v4();
        let mut ledger = ResumeLedger::default();
        ledger.note_pull(ResumeEntry {
            job_id: job,
            drv_hash: "drv-live".into(),
            tenant_hint: None,
            origin: "pruned".into(),
            nonce: live_nonce,
        });
        // The same job is STILL LISTED (pass-N mint tx not yet visible
        // to pass-N+1's listing snapshot). Resume pull answers
        // NotYetReady (entry kept); the fresh loop must SKIP the job.
        let d = rio_proto::types::MaterializationJobDescriptor {
            job_id: job.to_string(),
            drv_hash: "drv-live".into(),
            tenant_id: String::new(),
            origin: "pruned".into(),
        };
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d]))],
            vec![Ok(not_yet_ready())],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            2,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(
            t.pull_calls, 1,
            "one resume pull only — the fresh loop skips the live entry"
        );
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            t.seen_pull_requests[0].claim_nonce,
            live_nonce.to_string(),
            "the LIVE credential is presented, never clobbered by a fresh mint"
        );
    }

    /// merged_bug_096 (drop half): a fresh claim answered NotYetReady
    /// KEEPS its entry. The scheduler's post-mint TOCTOU arm answers
    /// NotYetReady AFTER the durable mint committed with this worker's
    /// nonce — dropping the nonce on that answer strands the committed
    /// mint into the charged establishment window, the exact wedge
    /// rule-4b was signed to close. (Gone and Rejected still resolve:
    /// both are authoritative no-mint answers.)
    #[tokio::test]
    async fn fresh_not_yet_ready_keeps_the_credential() {
        let d = descriptor(96);
        let mut ledger = ResumeLedger::default();
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d.clone()]))],
            vec![Ok(not_yet_ready())],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(
            ledger.len(),
            1,
            "NotYetReady is not proof of no-mint (post-mint TOCTOU answers it) — the credential survives"
        );
    }

    /// bug_099: the fresh-claim walk is budgeted by ISSUED claims
    /// (potential server-side mints, counted at nonce-mint time), not
    /// delivered claims. Pre-fix, Unanswered was a free skip over the
    /// full LISTING_WINDOW (16+): under a scheduler-mailbox brownout a
    /// 1-slot worker minted a nonce per descriptor — up to 16 open
    /// attempts committed server-side and bound to this worker, gone
    /// from every other worker's listing, while the resume lane drains
    /// at most available_slots per pass.
    #[tokio::test(start_paused = true)]
    async fn unanswered_pulls_consume_the_mint_budget() {
        let descriptors = vec![descriptor(991), descriptor(992), descriptor(993)];
        let mut ledger = ResumeLedger::default();
        let mut t = MockTransport::new(vec![Ok(listing(descriptors))], vec![], vec![]);
        t.hang_next_pulls = 8;
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(
            ledger.len(),
            1,
            "one slot = one potential mint per pass; an unanswered pull \
             consumes the budget instead of skipping to mint again"
        );
        assert_eq!(
            t.pull_calls, 1,
            "the walk stops at the budget — no further pulls (and no \
             further nonces) ride the wire this pass"
        );
    }

    /// bug_116: assignments the RESUME pass already claimed (and
    /// ledger-resolved) survive a failed listing. Pre-fix the three
    /// listing failure arms returned a fresh empty vec — the
    /// re-delivered attempt was then executed by nobody and
    /// unrecoverable by construction (nonce destroyed, claimed
    /// attempts never re-list, credential-less re-pulls answer
    /// NotYetReady), stranding it until the establishment sweep closed
    /// it CHARGED. The trigger is correlated: resume entries exist
    /// precisely because the same store→scheduler edge is flaky.
    #[tokio::test]
    async fn listing_failure_returns_accrued_resume_claims() {
        let mut ledger = ResumeLedger::default();
        ledger.note_pull(ResumeEntry {
            job_id: Uuid::now_v7(),
            drv_hash: "drv-keep".into(),
            tenant_hint: None,
            origin: "pruned".into(),
            nonce: Uuid::new_v4(),
        });
        let mut t = MockTransport::new(
            // The listing fails AFTER the resume pass delivered.
            vec![Err(tonic::Status::unavailable("leader rolling"))],
            vec![Ok(deliver("exec-keep", "/nix/store/fff-keep.drv"))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            2,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(
            claimed.len(),
            1,
            "the resume-claimed assignment survives a failed listing"
        );
        assert_eq!(claimed[0].exec_id, "exec-keep");
        assert!(ledger.is_empty(), "the delivered entry stays resolved");
    }

    /// bug_119: an ANSWERED permanent rejection (auth misconfig) is not
    /// a lost response. `pull_once` classifies it through the same
    /// `is_fatal_rejection` chokepoint the report leg uses, and BOTH
    /// claim passes resolve the ledger entry (the rule-4b contract:
    /// entries leave on every ANSWERED outcome). Pre-fix the entry was
    /// immortal: every pass burned a doomed resume pull plus a doomed
    /// fresh claim per job while the warn mis-narrated the credential
    /// refusal as a resumable lost response.
    #[tokio::test]
    async fn answered_permanent_rejection_resolves_ledger_entry() {
        // Fresh-claim half: the claim's nonce must not outlive the
        // answered refusal.
        let d = descriptor(119);
        let mut ledger = ResumeLedger::default();
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d.clone()]))],
            vec![Err(tonic::Status::permission_denied(
                "service token rejected",
            ))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert!(
            ledger.is_empty(),
            "an answered refusal is not a lost response — no immortal entry"
        );

        // Resume half (merged_bug_074 narrowing of the bug_119
        // letter): a held entry whose resume pull is REFUSED with a
        // MINT-DISPROVING answer (InvalidArgument/Unimplemented — the
        // request shape can never mint) resolves. Auth-layer codes no
        // longer resolve the RESUME arm — they judge the credential
        // presentation, not the mint the original unanswered pull may
        // have committed; their survival is pinned by
        // auth_rejection_keeps_resume_credential_through_skew.
        let mut ledger = ResumeLedger::default();
        ledger.note_pull(ResumeEntry {
            job_id: Uuid::now_v7(),
            drv_hash: "drv-fatal".into(),
            tenant_hint: None,
            origin: "pruned".into(),
            nonce: Uuid::new_v4(),
        });
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![]))],
            vec![Err(tonic::Status::invalid_argument(
                "malformed claim request",
            ))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert!(
            ledger.is_empty(),
            "resume credential refused with a mint-disproving answer — \
             entry resolves"
        );
    }

    /// merged_bug_026 (the resume half): a nonce-presenting resume pull
    /// can be answered through the kernel's nonce-blind Pending arm
    /// with a delivery minted for the job's SUCCESSOR. The assignment
    /// echoes the job it was minted under (`WorkAssignment.job_id`, the
    /// producer-asserted binding) and the client keys the claimed job
    /// by the WIRE value — the stale ledger entry's hints (recorded for
    /// a different job) are dropped on the rebind.
    #[tokio::test]
    async fn resume_deliver_binds_wire_job_identity() {
        let stale_job = Uuid::now_v7();
        let successor_job = Uuid::now_v7();
        let mut ledger = ResumeLedger::default();
        ledger.note_pull(ResumeEntry {
            job_id: stale_job,
            drv_hash: "drv-rebind".into(),
            tenant_hint: Some(Uuid::now_v7()),
            origin: "pruned".into(),
            nonce: Uuid::new_v4(),
        });
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![]))],
            vec![Ok(deliver_for_job(
                "exec-succ",
                "/nix/store/ccc-rebind.drv",
                successor_job,
            ))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].job_id, successor_job,
            "the wire-echoed job binding wins over the stale ledger entry"
        );
        assert_eq!(
            claimed[0].tenant_hint, None,
            "a different job's recorded hint is stale — dropped on rebind"
        );
        assert_eq!(
            claimed[0].origin, REBOUND_ORIGIN,
            "origin is the rebind sentinel, not the stale entry's"
        );
        assert!(
            ledger.is_empty(),
            "the nonce was ANSWERED (with a successor delivery) — entry resolves"
        );
    }

    /// merged_bug_026 (skew + agreement halves): an EMPTY wire job_id
    /// (pre-field scheduler / build-kind payload) falls back to the
    /// client-side identity — the pre-field behavior — and an AGREEING
    /// wire echo keeps the entry's recorded hints.
    #[tokio::test]
    async fn wire_job_identity_fallback_and_agreement() {
        // Skew half: empty wire field → ledger entry id + hints kept.
        let job = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let mut ledger = ResumeLedger::default();
        ledger.note_pull(ResumeEntry {
            job_id: job,
            drv_hash: "drv-skew".into(),
            tenant_hint: Some(tenant),
            origin: "pruned".into(),
            nonce: Uuid::new_v4(),
        });
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![]))],
            vec![Ok(deliver("exec-skew", "/nix/store/ddd-skew.drv"))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].job_id, job,
            "empty wire field: ledger identity holds"
        );
        assert_eq!(
            claimed[0].tenant_hint,
            Some(tenant),
            "hints kept on the pre-field path"
        );
        assert_eq!(claimed[0].origin, "pruned");

        // Agreement half (fresh-claim sibling site): the wire echoes
        // the SAME job the descriptor listed → descriptor hints kept.
        let d = descriptor(26);
        let listed_job = Uuid::parse_str(&d.job_id).expect("descriptor mints a uuid");
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d.clone()]))],
            vec![Ok(deliver_for_job(
                "exec-agree",
                "/nix/store/eee-agree.drv",
                listed_job,
            ))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].job_id, listed_job);
        assert_eq!(
            claimed[0].origin, d.origin,
            "agreeing echo keeps descriptor hints"
        );
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
    // r[verify store.materialize.executor+5]
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
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
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
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            2,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(claimed.len(), 2, "the budget caps successful claims");
        assert_eq!(t.pull_calls, 2, "no claim attempted past the budget");
    }

    /// bug_233 (bughunt wave): a descriptor whose job_id does not parse
    /// as a UUID is REFUSED before any claim is attempted — no
    /// ClaimedJob, no PullAssignment RPC (claiming an attempt we cannot
    /// attribute to a job would mint the immortal NULL-job pin class).
    // r[verify store.materialize.executor+5]
    #[tokio::test]
    async fn malformed_job_id_refuses_the_claim() {
        let mut bad = descriptor(9);
        bad.job_id = "not-a-uuid".to_string();
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![bad]))],
            vec![Ok(deliver("exec-9", "/nix/store/zzz-bad.drv"))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            8,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
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
    // r[verify store.materialize.executor+5]
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
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            8,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
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
    // r[verify store.materialize.executor+5]
    #[tokio::test]
    async fn poll_and_claim_respects_slots() {
        let mut t = MockTransport::new(
            vec![Ok(listing((1..=5).map(descriptor).collect::<Vec<_>>()))],
            vec![Ok(deliver("exec-x", "/nix/store/xxx.drv"))],
            vec![],
        );
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            2,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(claimed.len(), 2);
        assert_eq!(
            t.pull_calls, 2,
            "claims are bounded by available slots, not by the listing size"
        );

        // Zero slots → no RPCs at all.
        let mut idle = MockTransport::new(vec![], vec![], vec![]);
        let claimed = poll_and_claim(
            &mut idle,
            &instance("store-replica-0"),
            0,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(idle.list_calls, 0, "zero slots never even lists");
    }

    /// (d) The BC-1 wire obligation: every claim carries
    /// kind=MATERIALIZATION + the configured executor_instance, no
    /// executor token, and the listed job's drv hash as the intent.
    // r[verify store.materialize.executor+5]
    #[tokio::test]
    async fn claim_carries_kind_and_instance() {
        let d1 = descriptor(1);
        let d2 = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d1.clone(), d2.clone()]))],
            vec![Ok(deliver("exec-1", "/nix/store/aaa.drv"))],
            vec![],
        );
        let _ = poll_and_claim(
            &mut t,
            &instance("store-replica-7"),
            8,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
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
    // r[verify store.materialize.executor+5]
    #[tokio::test(start_paused = true)]
    async fn report_until_acked_retries() {
        let outcome = MaterializationOutcome {
            outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                rio_proto::types::materialization_outcome::Success {
                    ingested_paths: vec!["/nix/store/aaa-one".into()],
                    verified_paths: vec![],
                    verified_tenants: vec![],
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
            crate::materialize::executor::CountedOutcome::count(outcome.clone()),
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
            crate::materialize::executor::CountedOutcome::count(outcome.clone()),
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
        let acked = report_until_acked(
            &mut t,
            "exec-3",
            crate::materialize::executor::CountedOutcome::count(outcome),
            Duration::from_secs(60),
            &token(),
        )
        .await;
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
    // r[verify store.materialize.executor+5]
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
                crate::materialize::executor::CountedOutcome::count(MaterializationOutcome {
                    outcome: None,
                }),
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
    // r[verify store.materialize.executor+5]
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
            crate::materialize::executor::CountedOutcome::count(MaterializationOutcome {
                outcome: None,
            }),
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
    // r[verify store.materialize.executor+5]
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
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            8,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &shutdown,
        )
        .await;
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

    // r[verify store.materialize.executor+5]
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

        let mut transport = SchedulerTransport::connect_lazy(
            &HostPort::parse(&proxy_addr.to_string()).expect("proxy addr is host:port"),
            None,
            &instance("store-replica-0"),
        )
        .unwrap();

        // The executor's connection gets pinned to the standby: the
        // poll pass comes back empty (UNAVAILABLE answers).
        let claimed = poll_and_claim(
            &mut transport,
            &instance("store-replica-0"),
            1,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
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
        let mut claimed = ClaimedSet::begin();
        for _ in 0..5 {
            claimed = poll_and_claim(
                &mut transport,
                &instance("store-replica-0"),
                1,
                &mut ResumeLedger::default(),
                &mut ListFailureLatch::default(),
                &token(),
            )
            .await;
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
    // r[verify store.materialize.executor+5]
    #[tokio::test]
    async fn report_abandons_connection_pinned_to_standby_replica() {
        let standby_addr = spawn_executor_service(StandbyExecutorService).await;
        let leader_addr = spawn_executor_service(LeaderExecutorService).await;
        let (proxy_addr, backend) = spawn_switchable_proxy(standby_addr).await;

        let mut transport = SchedulerTransport::connect_lazy(
            &HostPort::parse(&proxy_addr.to_string()).expect("proxy addr is host:port"),
            None,
            &instance("store-replica-0"),
        )
        .unwrap();

        // Pin the connection to the standby with one failing pass.
        let _ = poll_and_claim(
            &mut transport,
            &instance("store-replica-0"),
            1,
            &mut ResumeLedger::default(),
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        // The rollout completes mid-execution.
        *backend.lock().unwrap() = leader_addr;

        let outcome = MaterializationOutcome {
            outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                rio_proto::types::materialization_outcome::Success {
                    ingested_paths: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-one".into()],
                    verified_paths: vec![],
                    verified_tenants: vec![],
                },
            )),
        };
        let acked = report_until_acked(
            &mut transport,
            "exec-rollout-1",
            crate::materialize::executor::CountedOutcome::count(outcome),
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
    // r[verify store.materialize.executor+5]
    #[tokio::test]
    async fn progress_abandons_connection_pinned_to_standby_replica() {
        let standby_addr = spawn_executor_service(StandbyExecutorService).await;
        let leader_addr = spawn_executor_service(LeaderExecutorService).await;
        let (proxy_addr, backend) = spawn_switchable_proxy(standby_addr).await;

        let mut transport = SchedulerTransport::connect_lazy(
            &HostPort::parse(&proxy_addr.to_string()).expect("proxy addr is host:port"),
            None,
            &instance("store-replica-0"),
        )
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

    /// bug_251 (rule-4b): timeout-then-resume — a lost-response claim
    /// is recovered by a DIRECT nonce-presenting resume pull on the
    /// next pass, never by re-listing (a minted attempt leaves the
    /// claimable listing forever; the pre-fix TimedOut log said "next
    /// poll re-lists", documenting a recovery path that did not
    /// exist). RED (recorded, kernel pin): the nonce-match leg
    /// refused pre-fix — `left: NotYetReady / right: DeliverExisting
    /// { exec_id: 9 }` (`lost_response_nonce_resumes_claim`); this
    /// test pins the client half end-to-end.
    // r[verify sched.materialize.claim-resume]
    #[tokio::test(start_paused = true)]
    async fn timeout_then_resume_recovers_lost_response() {
        let d = descriptor(1);
        let job_id = d.job_id.clone();
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![d])), Ok(listing(vec![]))],
            vec![Ok(deliver("exec-resume", "/nix/store/resume-x.drv"))],
            vec![],
        );
        t.hang_next_pulls = 1;
        let mut ledger = ResumeLedger::default();

        // Pass 1: the pull's answer never arrives — the claim is
        // recorded, nothing delivered.
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(
            ledger.len(),
            1,
            "the unanswered claim is held in the ledger"
        );
        let nonce_1 = t.seen_pull_requests[0].claim_nonce.clone();
        assert!(
            !nonce_1.is_empty(),
            "the nonce rides the FIRST pull — minted before the wire, not after the loss"
        );

        // Pass 2: the job no longer lists (the attempt is open,
        // held by us, server-side); the resume pull presents the
        // SAME nonce and recovers the assignment.
        let claimed = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(claimed.len(), 1, "the lost-response claim is recovered");
        assert_eq!(claimed[0].exec_id, "exec-resume");
        assert_eq!(claimed[0].job_id.to_string(), job_id);
        assert!(ledger.is_empty(), "delivery resolves the ledger entry");
        let resume_req = t.seen_pull_requests.last().expect("resume pull recorded");
        assert_eq!(
            resume_req.claim_nonce, nonce_1,
            "the resume presents the SAME nonce the mint persisted"
        );
        assert!(
            resume_req.resume_exec_id.is_empty(),
            "no exec_id token exists — the response that carried it was lost"
        );
    }

    /// bug_251: answered outcomes resolve ledger entries — Gone is
    /// authoritative on the resume pass; NotYetReady keeps the entry
    /// (parked / raced / stale view — one bounded RPC per pass); the
    /// capacity bound evicts oldest-first.
    #[tokio::test(start_paused = true)]
    async fn resume_ledger_lifecycle() {
        let d = descriptor(2);
        let mut t = MockTransport::new(
            vec![
                Ok(listing(vec![d])),
                Ok(listing(vec![])),
                Ok(listing(vec![])),
            ],
            vec![Ok(not_yet_ready()), Ok(gone())],
            vec![],
        );
        t.hang_next_pulls = 1;
        let mut ledger = ResumeLedger::default();
        // Pass 1: unanswered — entry held.
        let _ = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(ledger.len(), 1);
        // Pass 2: resume answered NotYetReady — entry KEPT.
        let _ = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(ledger.len(), 1, "NotYetReady keeps the resume obligation");
        // Pass 3: resume answered Gone — entry dropped.
        let _ = poll_and_claim(
            &mut t,
            &instance("store-replica-0"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &token(),
        )
        .await;
        assert!(ledger.is_empty(), "Gone is authoritative");

        // Capacity (merged_bug_072 re-aim): the mint authority
        // REFUSES at cap — eviction is gone (a live entry is a
        // possibly-committed rule-4b credential; the dedicated
        // fresh_claim_refuses_at_cap_never_evicts test pins the
        // refusal + oldest-survives properties).
        let mut full = ResumeLedger::default();
        for i in 0..RESUME_LEDGER_CAP {
            full.note_pull(ResumeEntry {
                job_id: Uuid::now_v7(),
                drv_hash: format!("drv-cap-{i}"),
                tenant_hint: None,
                origin: "cache_opportunity".into(),
                nonce: Uuid::new_v4(),
            });
        }
        assert_eq!(full.len(), RESUME_LEDGER_CAP);
        let extra = Uuid::now_v7();
        assert!(
            full.begin_fresh_claim(extra, |nonce| ResumeEntry {
                job_id: extra,
                drv_hash: "drv-cap-extra".into(),
                tenant_hint: None,
                origin: "cache_opportunity".into(),
                nonce,
            })
            .is_none(),
            "at cap the mint authority refuses"
        );
        assert_eq!(full.len(), RESUME_LEDGER_CAP);
    }

    /// bug_257: the adversarial input table. The URL form (row 1) is
    /// exactly the value that booted an "enabled" executor dialing
    /// `http://http://…` — host `http` — for a fleet-wide silent claim
    /// outage; every other row is a near-miss shape the parser must
    /// also refuse.
    #[test]
    fn hostport_rejects_every_non_hostport_shape() {
        for bad in [
            "http://127.0.0.1:9001",            // the outage shape
            "https://rio-scheduler:9001",       // sibling scheme
            "grpc://rio-scheduler:9001",        // any scheme at all
            "rio-scheduler.rio-system:9001/v1", // path-bearing
            "rio-scheduler:9001?tls=off",       // query-bearing
            "rio-scheduler:9001#frag",          // fragment-bearing
            "user@rio-scheduler:9001",          // userinfo
            "rio-scheduler.rio-system",         // port-less
            "127.0.0.1",                        // port-less IP
            "",                                 // empty
            "rio scheduler:9001",               // whitespace
            "rio-scheduler:port",               // non-numeric port
        ] {
            assert!(HostPort::parse(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in [
            "127.0.0.1:9001",
            "localhost:9001",
            "rio-scheduler.rio-system:9001",
            "[::1]:9001",
            "control:9001",
        ] {
            let parsed = HostPort::parse(good).expect("deployed shapes parse");
            assert_eq!(parsed.as_str(), good);
        }
    }

    proptest::proptest! {
        /// The bug_257 soundness property: for EVERY input, either the
        /// parser fails loudly, or composing the scheme the endpoint
        /// builder adds yields a URI whose authority is byte-identical
        /// to the input (the transport dials exactly what the operator
        /// wrote — the property the double-prefix violated, where the
        /// dialed authority became `http`).
        #[test]
        fn hostport_accepts_iff_authority_roundtrips(raw in ".{0,48}") {
            if let Ok(parsed) = HostPort::parse(&raw) {
                let uri: http::Uri = format!("http://{}", parsed.as_str())
                    .parse()
                    .expect("an accepted addr must compose with the endpoint scheme");
                proptest::prop_assert_eq!(
                    uri.authority().map(|a| a.as_str()),
                    Some(raw.as_str())
                );
                proptest::prop_assert_eq!(uri.path(), "/");
                proptest::prop_assert!(uri.query().is_none());
                proptest::prop_assert!(uri.port_u16().is_some());
            }
        }
    }

    /// bug_257 rider: the warn-once latch escalates at the threshold,
    /// stays escalated (no warn-per-pass flood), and a success re-arms
    /// it for the next persistent episode.
    #[test]
    fn list_failure_latch_warns_once_and_rearms_on_recovery() {
        let mut latch = ListFailureLatch::default();
        latch.note_failure("unavailable");
        latch.note_failure("unavailable");
        assert!(!latch.warned(), "two failures are routine (rollout blip)");
        latch.note_failure("unavailable");
        assert!(latch.warned(), "third consecutive failure escalates");
        latch.note_failure("unavailable");
        assert!(
            latch.warned(),
            "stays latched — exactly one warn per episode"
        );
        latch.note_success();
        assert!(!latch.warned(), "success resets the episode");
        latch.note_failure("timeout");
        latch.note_failure("timeout");
        assert!(!latch.warned(), "fresh episode counts from zero");
        latch.note_failure("timeout");
        assert!(latch.warned(), "and re-escalates at the threshold");
    }
}
