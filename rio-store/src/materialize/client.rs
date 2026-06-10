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

/// live_046 (R-B) — the pacing facts the claim loop's eager re-poll
/// reads off a completed pass (the structural EMPTY law:
/// `rio-store/src/materialize/mod.rs::pass_is_empty`). Carried beside
/// the claims so the pacing decision is derived from the pass that
/// actually ran, never re-probed.
#[derive(Debug, Clone, Copy, Default)]
pub struct PassPacing {
    /// The listing beat was withheld (honest-beat gate or futility
    /// latch — merged_bug_005). A withheld pass is ALWAYS paced: the
    /// no-spin composition law ("work fast, idle at the beat, never
    /// spin while wedged").
    pub beat_withheld: bool,
    /// The resume/fresh lanes changed the ledger (mint, resolution,
    /// or a standing transition).
    pub ledger_transitioned: bool,
    /// The listing answered with at least one descriptor.
    pub listed_any: bool,
}

/// One completed poll pass: the accrued claims plus the pacing facts.
/// Derefs to the claimed slice (and consumes into it) so claim
/// consumers read the pass exactly like the bare [`ClaimedSet`].
#[must_use = "claimed assignments must be executed or aborted — dropping them strands open attempts"]
pub struct PollPass {
    pub claimed: ClaimedSet,
    pub pacing: PassPacing,
}

impl std::ops::Deref for PollPass {
    type Target = [ClaimedJob];
    fn deref(&self) -> &[ClaimedJob] {
        &self.claimed
    }
}

impl IntoIterator for PollPass {
    type Item = ClaimedJob;
    type IntoIter = std::vec::IntoIter<ClaimedJob>;
    fn into_iter(self) -> Self::IntoIter {
        self.claimed.into_iter()
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
///
/// bug_034 — an entry plays TWO distinct roles, split as the private
/// `SlotStanding`: the rule-4b recovery CREDENTIAL (which every
/// live entry is, until authoritatively answered) and the claim-slot
/// CHARGE (which only an UNANSWERED entry holds — the scheduler may
/// have minted an attempt bound to this worker). The pass budget
/// derives from the private `charged_len` alone; [`Self::len`] keeps
/// the cap/diagnostic semantics (`RESUME_LEDGER_CAP` counts ENTRIES —
/// credential survival is unweakened by the split).
#[derive(Default)]
pub struct ResumeLedger {
    entries: VecDeque<ResumeEntry>,
    /// merged_bug_014 — the rotation cursor: the job last presented
    /// by the resume pass; the next pass's window starts after it
    /// (wrapping), so every live entry is presented within
    /// ⌈len/bound⌉ passes. `None` (or a resolved id) restarts at the
    /// front — recorded fairness coarsening on resolves.
    last_presented: Option<Uuid>,
}

/// One claim whose lifecycle is not settled: everything the resume
/// pull and the recovered [`ClaimedJob`] need from the original
/// listing descriptor, plus the minted nonce and the slot standing.
#[derive(Clone)]
struct ResumeEntry {
    job_id: Uuid,
    drv_hash: String,
    tenant_hint: Option<Uuid>,
    origin: String,
    nonce: Uuid,
    standing: SlotStanding,
}

/// bug_034 → merged_bug_014 — the typed split of a ledger entry's two
/// roles, now OUTCOME-DERIVED: the standing follows the entry's LAST
/// presentation outcome through the one transition law
/// ([`standing_effect`]), because the kernel mints on ANY claiming
/// presentation of a pending job (rio-evidence-kernel/src/pull.rs:
/// the (Materialization, Pending{parked:false}) arm answers
/// DeliverNew for Queued/Ready regardless of presented nonce; the
/// scheduler's confirm screen gates only `confirm_only` probes
/// before `mint_and_deliver` persists the mint). Therefore
/// `Charged` ⇔ "the entry's last claiming presentation is
/// unanswered", whichever lane presented:
///
///   - a fresh mint starts Charged (the mint authority stamps it);
///   - an ANSWERED NotYetReady refunds (→ `CredentialOnly`) — the
///     answer proves non-holdership for THIS presentation;
///   - a claiming presentation left UNANSWERED (either lane)
///     re-charges (→ `Charged`) — the scheduler may have committed a
///     mint bound to this worker behind the lost response;
///   - authoritative answers (Deliver / Gone / mint-disproving
///     rejection) remove the entry, ending both roles at once.
///
/// The pre-fix "monotone, never back" axiom claimed a storm requires
/// fresh mints "which always charge" — falsified by the kernel's own
/// admission table: a CredentialOnly entry's lost RESUME response
/// left a possibly-live mint with zero charge, and a 1-slot worker
/// over-bound while the orphan aged into the charged establishment
/// sweep. The anti-storm law survives in its true form: charges
/// arise only from presentations, and presentations are pass-bounded
/// ([`RESUME_PRESENTATIONS_PER_PASS`]).
///
/// Recorded residual (Q4/TOCTOU): a `CredentialOnly` entry whose
/// original pull DID mint server-side can later Deliver through the
/// resume lane while a fresh claim also Delivered this pass — bounded
/// by [`RESUME_LEDGER_CAP`], serialized by the worker's inline
/// execution loop, and strictly better than the pre-split shape where
/// the answered loser starved the worker for the winner's whole job
/// lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotStanding {
    /// Possibly minted, UNANSWERED: the entry consumes a claim slot —
    /// the mint may have committed server-side, bound to this worker
    /// (the bug_099/merged_bug_072 anti-storm law).
    Charged,
    /// The scheduler ANSWERED NotYetReady: the slot refunds (the pass
    /// may keep claiming), but the credential survives — NotYetReady
    /// is NOT proof of no-mint (the post-mint TOCTOU arm answers it
    /// after persisting this worker's nonce), so the entry stays
    /// nonce-resumable until Gone or delivery.
    CredentialOnly,
}

/// Ledger capacity. At cap the mint authority REFUSES fresh mints
/// (merged_bug_072 — live credentials are never evicted). What a full
/// ledger SIGNALS depends on its standing mix (bug_034/FS-4): 32
/// UNANSWERED claims on one worker is a scheduler-side outage the
/// establishment sweep owns (the warn); 32 answered-NotYetReady
/// credentials is the contested-remainder steady state (debug).
const RESUME_LEDGER_CAP: usize = 32;

/// FS-4 (bug_034 storm guard) — the per-pass fresh-mint allowance is
/// `available_slots + STEAL_SPECULATION_ALLOWANCE`. The allowance
/// exists because an answered raced loser REFUNDS its slot charge
/// (the SlotStanding split): without a per-pass mint bound, a fully
/// contested listing (every fresh pull answering NotYetReady — the
/// steady state whenever fleet > work) would let a 1-slot worker mint
/// the whole listing window of nonces per pass, accumulating
/// CredentialOnly entries toward [`RESUME_LEDGER_CAP`] and wedging
/// its own fresh mints.
///
/// Value: 1 — the production per-worker slot count (the claim loop
/// polls with `available_slots = 1`), giving 2 mints/pass there. The
/// resulting allowance MUST stay ≥ 2 at slots=1: an answered loser at
/// the head must leave room for one more fresh mint, or the refund
/// law is unreachable (the raced-loser red pins exactly that shape).
const STEAL_SPECULATION_ALLOWANCE: usize = 1;

/// merged_bug_014 (R17, violable + testable) — presentations per
/// resume pass. Derivation (const-asserted below): ≥ the production
/// `available_slots` (1) + [`STEAL_SPECULATION_ALLOWANCE`] (the
/// claiming lane must never starve — every slot can re-present, plus
/// the one speculative steal) + 2 probe slots at slots=1 (a
/// delivered resume flips the remainder to probes; two probes per
/// pass keep a small charged backlog settling within a handful of
/// beats). Pass wall-clock envelope: ≤ ONE timeout burn (the first
/// Unanswered ends the pass — a brownout answers nobody) plus
/// (bound − 1) answer latencies — vs the pre-fix cap-full worst case
/// of 32 sequential 30 s timeouts (~16 min). Starvation bound (the
/// rotation): every live entry is presented within ⌈len/bound⌉
/// passes.
const RESUME_PRESENTATIONS_PER_PASS: usize = 4;
const _: () = assert!(
    RESUME_PRESENTATIONS_PER_PASS >= 1 + STEAL_SPECULATION_ALLOWANCE + 2,
    "the pass bound must fund the claiming lane plus two probe slots at slots=1"
);

/// merged_bug_005 — what a pass may still do with fresh mints (the
/// [`ResumeLedger::fresh_mint_headroom`] answer): the typed reason a
/// pass cannot mint is what the honest-beat gate and the WO-side
/// wedge diagnostics key on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MintHeadroom {
    /// The pass may mint a fresh claim.
    Available,
    /// The claim budget is pinned: delivered claims plus UNANSWERED
    /// (Charged) mints fill every slot — a fresh mint would over-bind
    /// the worker past possibly-live server-side attempts.
    BudgetPinned { charged: usize },
    /// The ledger is at [`RESUME_LEDGER_CAP`]: the mint authority
    /// refuses every fresh mint (live credentials are never evicted).
    AtCap { charged: usize, entries: usize },
}

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
        // establishment window. The refusal stands regardless of the
        // population's standing mix (live credentials are never
        // evicted) — but WHAT the full ledger signals depends on it
        // (bug_034/FS-4): a Charged-dominated population is 32
        // UNANSWERED claims on one worker — a scheduler-side outage
        // the establishment sweep owns — while a CredentialOnly-
        // dominated one is the contested-remainder steady state
        // (answered raced losers waiting out the winners' jobs), not
        // an outage; warning "outage" there would be false.
        if self.entries.len() >= RESUME_LEDGER_CAP {
            if self.cap_refusal_is_outage() {
                warn!(job_id = %job_id,
                      charged = self.charged_len(),
                      entries = self.entries.len(),
                      "resume ledger at capacity with unanswered mints \
                       dominating; fresh mint refused (live credentials \
                       are never evicted) — scheduler-side outage?");
            } else {
                debug!(job_id = %job_id,
                       charged = self.charged_len(),
                       entries = self.entries.len(),
                       "resume ledger at capacity with answered \
                        credentials dominating (contested listing \
                        remainder); fresh mint refused");
            }
            return None;
        }
        let nonce = Uuid::new_v4();
        // The mint authority stamps the charge — a fresh mint ALWAYS
        // charges (bug_034: the budget's anti-storm half), whatever
        // the fill closure carried.
        let mut entry = fill(nonce);
        entry.standing = SlotStanding::Charged;
        self.entries.push_back(entry);
        Some(MintedClaim { nonce })
    }

    /// bug_034/FS-4 — the cap-refusal severity predicate: the outage
    /// warn fires only when the population is Charged-DOMINATED
    /// (charged ≥ credential-only; ties stay loud — the conservative
    /// direction). A CredentialOnly-dominated full ledger is the
    /// contested-remainder steady state and logs at debug.
    fn cap_refusal_is_outage(&self) -> bool {
        let charged = self.charged_len();
        charged * 2 >= self.entries.len()
    }

    /// bug_034 / merged_bug_014 — an answered NotYetReady refunds the
    /// slot: `Charged` → `CredentialOnly` (the answer proves
    /// non-holdership for this presentation; the credential
    /// survives). Idempotent on an already-refunded entry; a no-op
    /// when the job has no entry.
    fn note_answered_not_ready(&mut self, job_id: Uuid) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.job_id == job_id) {
            e.standing = SlotStanding::CredentialOnly;
        }
    }

    /// merged_bug_014 — a claiming presentation left UNANSWERED
    /// re-charges the slot: the scheduler may have committed a mint
    /// bound to this worker behind the lost response (the kernel
    /// mints on ANY claiming presentation of a pending job, whatever
    /// nonce it carries). Idempotent on an already-Charged entry; a
    /// no-op when the job has no entry.
    fn note_unanswered_presentation(&mut self, job_id: Uuid) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.job_id == job_id) {
            e.standing = SlotStanding::Charged;
        }
    }

    /// merged_bug_014 — apply one [`StandingEffect`] (the output of
    /// the [`standing_effect`] transition law) to the entry.
    fn apply_standing(&mut self, job_id: Uuid, effect: StandingEffect) {
        match effect {
            StandingEffect::Resolve => self.resolve(job_id),
            StandingEffect::Refund => self.note_answered_not_ready(job_id),
            StandingEffect::Recharge => self.note_unanswered_presentation(job_id),
            StandingEffect::Keep => {}
        }
    }

    /// The claim-slot charge count — what the pass budget reads
    /// (bug_034): entries whose mint is still UNANSWERED. Distinct
    /// from [`Self::len`] (the cap/diagnostic population, which
    /// counts every live credential).
    fn charged_len(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.standing == SlotStanding::Charged)
            .count()
    }

    // r[impl store.materialize.honest-beat]
    /// merged_bug_005 — THE fresh-mint capability predicate, single-
    /// sourced on the ledger: the conjunction the claim loop already
    /// enforced piecewise (the in-loop budget break and the
    /// [`Self::begin_fresh_claim`] cap refusal) as ONE typed answer.
    /// The pre-listing honest-beat gate, the in-loop break, and the
    /// mint authority all consume this fn — a pass whose headroom is
    /// not [`MintHeadroom::Available`] cannot convert a served job
    /// into a claim, so listing on it would be a false liveness beat
    /// (the scheduler's steal horizon keys on listing recency as the
    /// capability proxy; the beat must therefore be capability-
    /// bearing).
    fn fresh_mint_headroom(&self, claimed_len: usize, available_slots: usize) -> MintHeadroom {
        if self.entries.len() >= RESUME_LEDGER_CAP {
            MintHeadroom::AtCap {
                charged: self.charged_len(),
                entries: self.entries.len(),
            }
        } else if claimed_len + self.charged_len() >= available_slots {
            MintHeadroom::BudgetPinned {
                charged: self.charged_len(),
            }
        } else {
            MintHeadroom::Available
        }
    }

    /// Snapshot for the resume pass (entries are a handful of small
    /// strings; the pass mutates the ledger per answer).
    #[cfg(test)]
    fn snapshot(&self) -> Vec<ResumeEntry> {
        self.entries.iter().cloned().collect()
    }

    /// merged_bug_014 — the pass's presentation WINDOW: at most
    /// [`RESUME_PRESENTATIONS_PER_PASS`] entries, rotated to start
    /// after the previously-presented entry (the starvation bound),
    /// with Charged entries first WITHIN the window (the
    /// unanswered-orphan priority — window-scoped so the rotation
    /// fairness law survives a persistent Charged backlog; recorded
    /// divergence from the global-priority phrasing).
    fn presentation_window(&self) -> Vec<ResumeEntry> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let start = self
            .last_presented
            .and_then(|id| self.entries.iter().position(|e| e.job_id == id))
            .map(|i| (i + 1) % self.entries.len())
            .unwrap_or(0);
        let mut window: Vec<ResumeEntry> = self
            .entries
            .iter()
            .cycle()
            .skip(start)
            .take(self.entries.len().min(RESUME_PRESENTATIONS_PER_PASS))
            .cloned()
            .collect();
        // Stable: rotation order is preserved within each class.
        window.sort_by_key(|e| e.standing != SlotStanding::Charged);
        window
    }

    /// Advance the rotation cursor (called per presentation, before
    /// the answer settles the standing — a resolved entry still
    /// anchors the next pass's start until it leaves the ledger).
    fn note_presented(&mut self, job_id: Uuid) {
        self.last_presented = Some(job_id);
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

/// merged_bug_014 — which lane issued a presentation (the standing
/// law's second axis): a FRESH mint, a claiming RESUME of a live
/// credential, or a non-minting confirm PROBE past full slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresentationLane {
    Fresh,
    ResumeClaiming,
    Probe,
}

/// merged_bug_014 — what one presentation outcome does to the
/// entry's standing/lifecycle (applied via
/// [`ResumeLedger::apply_standing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandingEffect {
    /// Authoritative settle: the entry leaves the ledger.
    Resolve,
    /// Answered non-holdership: `Charged` → `CredentialOnly`.
    Refund,
    /// A claiming presentation went unanswered: → `Charged`.
    Recharge,
    /// No standing movement (auth skew on a surviving credential,
    /// shutdown, a probe's inconclusive outcomes).
    Keep,
}

// r[impl sched.materialize.claim-resume]
/// merged_bug_014 — THE standing-transition law: ONE total function
/// over (presentation lane × [`PullAnswer`]) — the closure-set
/// carrier (R14/R15: rustc's exhaustiveness is the census; a new
/// answer variant or lane fails this build). Both claiming lanes and
/// the probe lane dispatch through it; the per-arm rationale:
///
///   - `Deliver` resolves on claiming lanes (the claim IS the
///     settle); on a PROBE it PROVES my mint is live — the entry
///     (re-)charges (a proven-live mint holds a slot in truth, even
///     if a prior answer had refunded it) and the payload is
///     DISCARDED (the probe is a standing oracle, never an execution
///     source; the next claiming presentation re-delivers);
///   - `Gone` resolves everywhere (the job settled without us);
///   - `NotYetReady` refunds everywhere — screened (the confirm
///     screen converting a would-be mint) or genuine, both prove
///     non-holdership for this presentation;
///   - `Unanswered` re-charges CLAIMING lanes (a mint may have
///     committed behind the lost response); a probe's loss proves
///     nothing either way (the screen blocks probe mints) — Keep;
///   - `RejectedDisproving` resolves everywhere (the request shape
///     can never mint — nothing is pending behind it);
///   - `RejectedAuth` resolves only the FRESH lane (its gates run
///     pre-mint, so THIS pull was the only one that could have
///     minted — merged_bug_074); on resume/probe it judges the
///     PRESENTATION, not the original mint — Keep;
///   - `Shutdown` keeps (the pass is abandoned unobserved).
fn standing_effect(lane: PresentationLane, answer: &PullAnswer) -> StandingEffect {
    match (lane, answer) {
        (PresentationLane::Fresh | PresentationLane::ResumeClaiming, PullAnswer::Deliver(_)) => {
            StandingEffect::Resolve
        }
        (PresentationLane::Probe, PullAnswer::Deliver(_)) => StandingEffect::Recharge,
        (_, PullAnswer::Gone) => StandingEffect::Resolve,
        (_, PullAnswer::NotYetReady) => StandingEffect::Refund,
        (PresentationLane::Fresh | PresentationLane::ResumeClaiming, PullAnswer::Unanswered) => {
            StandingEffect::Recharge
        }
        (PresentationLane::Probe, PullAnswer::Unanswered) => StandingEffect::Keep,
        (_, PullAnswer::RejectedDisproving) => StandingEffect::Resolve,
        (PresentationLane::Fresh, PullAnswer::RejectedAuth) => StandingEffect::Resolve,
        (PresentationLane::ResumeClaiming | PresentationLane::Probe, PullAnswer::RejectedAuth) => {
            StandingEffect::Keep
        }
        (_, PullAnswer::Shutdown) => StandingEffect::Keep,
    }
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

/// merged_bug_005 — consecutive FUTILE passes before the conversion-
/// futility latch withholds the listing beat. One or two
/// all-rejected passes can be transient (a rotation mid-skew, a
/// scheduler hiccup); three in a row is a worker that lists but can
/// never convert.
const FUTILE_PASS_THRESHOLD: u32 = 3;

/// merged_bug_005 (R17, violable + testable) — how many passes the
/// listing stays withheld after a futile streak before ONE probe
/// pass re-lists. Derivation: at the 1 s production beat
/// (`poll_interval_secs` floor — `claim_loop` clamps with
/// `.max(1)`), 64 withheld passes ≈ 64 s ≥ the scheduler's 60 s
/// listing-membership TTL ([`SCHEDULER_LISTING_MEMBER_TTL_SECS`]):
/// the wedged worker leaves the membership ENTIRELY and its
/// rendezvous slice re-homes permanently until a probe pass
/// converts again. The residual is one ≤5 s re-pin (the steal
/// horizon) per probe interval — recorded, accepted. The
/// `futile_latch_probe_interval_exceeds_member_ttl` const-relation
/// test pins the inequality.
const FUTILE_RELIST_INTERVAL_PASSES: u32 = 64;

/// Mirrored scheduler constant (rio-scheduler
/// `LISTING_MEMBER_TTL` = 60 s): rio-store cannot import
/// rio-scheduler (the dependency runs the other way), so the
/// honest-beat interval derivation mirrors the value; the
/// scheduler-side parity pin asserts equality THROUGH this exported
/// symbol (R14 — never a second hand-typed literal there).
pub const SCHEDULER_LISTING_MEMBER_TTL_SECS: u64 = 60;

/// Mirrored scheduler constant (rio-scheduler
/// `LISTING_STEAL_HORIZON` = 5 s) — same mirroring discipline as
/// [`SCHEDULER_LISTING_MEMBER_TTL_SECS`].
pub const SCHEDULER_LISTING_STEAL_HORIZON_SECS: u64 = 5;

/// merged_bug_005 — one pass's conversion evidence: the futility
/// latch's input, folded from the FULL [`PullAnswer`] alphabet at
/// the fresh-claim dispatch through [`futility_evidence`] (an
/// exhaustive match — a new answer variant forces a futility
/// classification at compile time, the R15 compiler census).
#[derive(Debug, Default)]
struct PassConversion {
    /// The listing served at least one descriptor.
    listed: bool,
    /// Fresh mints issued this pass.
    fresh_mints: usize,
    /// Deliveries on EITHER lane (resume or fresh).
    deliveries: usize,
    /// Fresh outcomes that were answered conversion-disproving
    /// rejections (`RejectedDisproving` / `RejectedAuth`).
    futile_rejections: usize,
    /// Any fresh outcome that proves the pass was NOT futile-only
    /// (delivery, Gone, a NotYetReady contest, an unanswered mint).
    non_futile_outcome: bool,
    /// The last conversion-disproving rejection's job (the latch
    /// warn names it).
    last_rejected_drv: Option<String>,
}

/// merged_bug_005 — the futility classification of ONE fresh-lane
/// outcome, TOTAL over the [`PullAnswer`] alphabet (the latch's
/// closure set is the enum itself; rustc is the census generator).
enum FutilityEvidence {
    /// Delivery or an authoritative clean settle (Gone): the pass
    /// CAN convert — the streak resets.
    Conversion,
    /// An answered conversion-disproving rejection
    /// (`RejectedDisproving` / `RejectedAuth`): futile evidence.
    ConversionDisproved,
    /// A NotYetReady contest loss (the healthy fleet>work steady
    /// state — NEVER futile evidence) or an unanswered mint (no
    /// evidence either way; the budget gate owns that lane).
    NotFutile,
    /// SIGTERM raced the pull — the pass is abandoned unobserved.
    Inconclusive,
}

fn futility_evidence(answer: &PullAnswer) -> FutilityEvidence {
    match answer {
        PullAnswer::Deliver(_) | PullAnswer::Gone => FutilityEvidence::Conversion,
        PullAnswer::RejectedDisproving | PullAnswer::RejectedAuth => {
            FutilityEvidence::ConversionDisproved
        }
        PullAnswer::NotYetReady | PullAnswer::Unanswered => FutilityEvidence::NotFutile,
        PullAnswer::Shutdown => FutilityEvidence::Inconclusive,
    }
}

// r[impl store.materialize.honest-beat]
/// merged_bug_005 — the conversion-futility latch (the in-file
/// [`ListFailureLatch`] pattern): a worker whose every fresh mint is
/// answered with a conversion-disproving rejection cannot convert a
/// served job into a claim, yet pre-fix it kept listing — staying
/// eternally fresh under the scheduler's steal horizon and pinning
/// its rendezvous slice fleet-wide (the wedged-but-polling worker).
/// After [`FUTILE_PASS_THRESHOLD`] consecutive futile passes the
/// LISTING is withheld for [`FUTILE_RELIST_INTERVAL_PASSES`] passes
/// (long enough to leave the scheduler's membership — the slice
/// re-homes permanently), then ONE probe pass re-lists; any
/// delivery or clean outcome resets. The RESUME presentation lane
/// is NEVER withheld (presentations are answer-gathering, not
/// mint-gated).
#[derive(Default)]
pub struct ConversionFutilityLatch {
    consecutive_futile: u32,
    withhold_remaining: u32,
    engaged: bool,
}

impl ConversionFutilityLatch {
    /// Whether THIS pass's listing is withheld (counts down the
    /// re-probe interval; the pass that reaches zero is the probe).
    fn withholds_this_pass(&mut self) -> bool {
        if self.withhold_remaining > 0 {
            self.withhold_remaining -= 1;
            true
        } else {
            false
        }
    }

    /// Fold one completed pass's conversion evidence.
    fn observe_pass(&mut self, pass: &PassConversion) {
        if pass.deliveries > 0 || (!pass.listed && pass.fresh_mints == 0) {
            // A converting pass resets outright; a pass that served
            // nothing (empty listing) carries no conversion evidence
            // — it neither extends nor breaks the streak.
            if pass.deliveries > 0 {
                self.reset_on_conversion();
            }
            return;
        }
        let futile = pass.listed
            && pass.fresh_mints > 0
            && pass.futile_rejections == pass.fresh_mints
            && !pass.non_futile_outcome;
        if futile {
            self.consecutive_futile = self.consecutive_futile.saturating_add(1);
            if self.consecutive_futile >= FUTILE_PASS_THRESHOLD && self.withhold_remaining == 0 {
                self.withhold_remaining = FUTILE_RELIST_INTERVAL_PASSES;
                if !self.engaged {
                    self.engaged = true;
                    warn!(
                        streak = self.consecutive_futile,
                        withheld_passes = FUTILE_RELIST_INTERVAL_PASSES,
                        last_rejected = pass.last_rejected_drv.as_deref().unwrap_or("<none>"),
                        "every fresh mint this streak was answered with a                          conversion-disproving rejection; withholding the                          listing beat so the rendezvous slice re-homes                          (the resume lane keeps presenting)"
                    );
                }
            }
        } else if pass.non_futile_outcome || pass.fresh_mints > 0 {
            // Conversion-capable evidence without a delivery (a
            // contest loss, an unanswered mint, a Gone): the streak
            // is broken — futile passes must be CONSECUTIVE.
            self.consecutive_futile = 0;
        }
    }

    fn reset_on_conversion(&mut self) {
        if self.engaged {
            info!(
                "conversion recovered; listing beat resumes (futility                  latch reset)"
            );
        }
        self.consecutive_futile = 0;
        self.withhold_remaining = 0;
        self.engaged = false;
    }

    /// Test visibility: is the listing currently withheld?
    #[cfg(test)]
    fn withholding(&self) -> bool {
        self.withhold_remaining > 0
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
    futility: &mut ConversionFutilityLatch,
    shutdown: &rio_common::signal::Token,
) -> PollPass {
    // The accumulator is constructed ONCE; every exit below returns it
    // (bug_116 — the listing-failure arms can no longer fabricate an
    // empty result over accrued claims).
    let mut claimed = ClaimedSet::begin();
    // merged_bug_005 — the pass's conversion evidence (folded from the
    // full PullAnswer alphabet at the fresh dispatch); observed by the
    // futility latch at pass end.
    let mut pass = PassConversion::default();
    // live_046 (R-B) — the pacing facts: ledger transitions are
    // derived from the (len, charged) pair around the pass (any mint,
    // resolution, or standing transition moves at least one).
    let mut pacing = PassPacing::default();
    let ledger_shape_at_entry = (ledger.len(), ledger.charged_len());
    macro_rules! finish {
        () => {{
            pacing.ledger_transitioned =
                (ledger.len(), ledger.charged_len()) != ledger_shape_at_entry;
            return PollPass { claimed, pacing };
        }};
    }
    if available_slots == 0 {
        finish!();
    }

    // bug_251: the RESUME pass runs FIRST — unanswered claims from
    // prior passes are existing obligations (the scheduler may hold an
    // open attempt minted for this replica that no listing will ever
    // show again). Each resume pull presents the persisted nonce; the
    // kernel's credential disjunction re-delivers.
    //
    // merged_bug_014: the pass presents a bounded, rotated WINDOW
    // (Charged first within it — the unanswered-orphan priority);
    // once delivered claims fill the slots, remaining presentations
    // switch to confirm_only PROBES (the landed confirm screen
    // converts would-be-DeliverNew to NotYetReady, so no mint can
    // occur; DeliverExisting passes through with the payload
    // DISCARDED — the probe is a standing oracle, never an execution
    // source). The FIRST Unanswered presentation ends the pass: a
    // brownout answers nobody — one timeout burn per pass, not one
    // per entry.
    for entry in ledger.presentation_window() {
        let probing = claimed.len() >= available_slots;
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
            // A resume is a CLAIMING pull while slots remain; past
            // full slots it presents as a confirm PROBE
            // (merged_bug_014 — the standing oracle).
            confirm_only: probing,
        };
        let answer = pull_once(transport, shutdown, req, &entry.drv_hash).await;
        if matches!(answer, PullAnswer::Shutdown) {
            finish!();
        }
        ledger.note_presented(entry.job_id);
        // merged_bug_014 — the ONE transition law settles the
        // standing for every arm (per-arm rationale at
        // [`standing_effect`]); the resume highlights:
        //   Gone / RejectedDisproving resolve (authoritative);
        //   NotYetReady refunds — screened (the confirm screen
        //     converting a probe's would-be mint) or genuine, both
        //     prove non-holdership; the credential rides on (one
        //     bounded RPC per pass until Gone or delivery);
        //   Unanswered RE-CHARGES the claiming lane (the lost
        //     response may hide a committed mint — the refuted
        //     monotone axiom's edge) and Keeps a probe (the screen
        //     blocks probe mints);
        //   RejectedAuth keeps (merged_bug_074: rotation skew judges
        //     the presentation, not the original mint).
        let lane = if probing {
            PresentationLane::Probe
        } else {
            PresentationLane::ResumeClaiming
        };
        ledger.apply_standing(entry.job_id, standing_effect(lane, &answer));
        match (probing, answer) {
            (false, PullAnswer::Deliver(assignment)) => {
                info!(drv_hash = %entry.drv_hash, exec_id = %assignment.exec_id,
                      "lost-response claim resumed via nonce (rule-4b)");
                pass.deliveries += 1;
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
            (true, PullAnswer::Deliver(assignment)) => {
                // DeliverExisting through the screen: my mint is
                // PROVEN live — the entry (re-)charged above. The
                // payload is DISCARDED; the next pass's CLAIMING
                // presentation re-delivers it for execution.
                debug!(drv_hash = %entry.drv_hash, exec_id = %assignment.exec_id,
                       "confirm probe answered with my live mint; payload \
                        discarded (the probe is a standing oracle)");
            }
            (_, PullAnswer::Unanswered) => {
                // Brownout short-circuit: the first lost answer ends
                // the pass — one timeout burn, not one per entry.
                debug!(drv_hash = %entry.drv_hash,
                       "resume presentation unanswered; ending the pass \
                        (one timeout burn per pass)");
                break;
            }
            _ => {}
        }
    }
    // bug_385: the listing window is DECOUPLED from the claim budget.
    // With limit == slots, a refused head — raced to another replica,
    // resolved between list and claim, or freshly parked — hides every
    // younger claimable job for the whole pass; at slots=1 the loop
    // starves behind one such head until it leaves the listing.
    // Listing is cheap (descriptors only); the claim loop below still
    // stops at the slot budget.
    // live_041: this window is no longer a FLEET-wide throughput
    // ceiling — the scheduler rendezvous-partitions the claimable
    // head per live worker (identity from the verified service-token
    // claims; zero wire change here), so N workers' windows cover N
    // disjoint slices instead of all fighting the same oldest-first
    // head.
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
    // r[impl store.materialize.honest-beat]
    // merged_bug_005 — the honest-beat gate: the listing call doubles
    // as this worker's liveness/capability beat for the scheduler's
    // rendezvous partition (the steal horizon keys on listing recency
    // as the capability proxy), so a pass that CANNOT convert a
    // served job into a claim must not list — mint headroom exhausted
    // (delivered claims, the budget pinned by Charged entries, or the
    // ledger at cap: the SAME predicate the claim loop enforces) or a
    // conversion-futility streak. The RESUME lane above already ran:
    // presentations are answer-gathering and are never withheld.
    let headroom = ledger.fresh_mint_headroom(claimed.len(), available_slots);
    if headroom != MintHeadroom::Available {
        debug!(
            ?headroom,
            "pass cannot mint a fresh claim; listing beat withheld (honest beat)"
        );
        pacing.beat_withheld = true;
        finish!();
    }
    if futility.withholds_this_pass() {
        debug!(
            "conversion-futility latch engaged; listing beat withheld              until the re-probe interval elapses"
        );
        pacing.beat_withheld = true;
        finish!();
    }
    let listed = match bounded(
        shutdown,
        DEFAULT_GRPC_TIMEOUT,
        transport.list_jobs(list_req),
    )
    .await
    {
        BoundedOutcome::Shutdown => finish!(),
        BoundedOutcome::TimedOut { after } => {
            debug!(
                after_secs = after.as_secs(),
                "ListMaterializationJobs unanswered; empty poll pass"
            );
            list_health.note_failure("timed out (no answer)");
            transport.note_timeout();
            finish!();
        }
        BoundedOutcome::Resolved(Ok(resp)) => {
            list_health.note_success();
            pass.listed = !resp.jobs.is_empty();
            resp.jobs
        }
        BoundedOutcome::Resolved(Err(status)) => {
            debug!(code = ?status.code(), msg = status.message(),
                   "ListMaterializationJobs failed; empty poll pass");
            list_health.note_failure(&format!("{:?}: {}", status.code(), status.message()));
            finish!();
        }
    };
    pacing.listed_any = pass.listed;

    // bug_099: the walk's budget counts POTENTIAL server-side mints —
    // every nonce issued whose outcome the scheduler has not answered.
    // An ANSWERED refusal refunds the slot; an UNANSWERED pull keeps
    // it consumed (the mint may have committed server-side, bound to
    // this worker). Pre-fix, a mailbox brownout let a 1-slot worker
    // mint a nonce per listed descriptor (16+ open attempts the
    // resume lane drains at one per pass).
    // merged_bug_072: the budget is DERIVED from the ledger
    // population — entries are created at mint and leave on every
    // authoritative answer — across passes as well as within one. The
    // pre-fix per-pass counter reset to 0 each pass, making
    // prior-pass Unanswered entries invisible: a list-ok/pull-lost
    // brownout minted one fresh nonce per pass up to RESUME_LEDGER_CAP
    // (32x a 1-slot worker), each eviction then forfeiting a live
    // rule-4b credential. No parallel counter exists to desync
    // (banner a).
    // bug_034: the two laws above collided on the answered-
    // NotYetReady cell — bug_099's refund law says the slot frees,
    // merged_bug_072's derived budget said the surviving entry keeps
    // consuming it. The SlotStanding split dissolves the
    // contradiction: the CREDENTIAL survives (rule-4b — NotYetReady
    // is not proof of no-mint), the CHARGE refunds (the budget reads
    // `charged_len()`, the unanswered subset only). At slots=1 the
    // raced loser no longer idles the worker for the winner's whole
    // job lifetime.
    //
    // FS-4 storm guard: the refund alone licenses a fresh-mint storm
    // on a fully-contested remainder (every fresh pull answering
    // NotYetReady — the common state whenever fleet > work): each
    // answered loser refunds, the pass walks on, and a 1-slot worker
    // would mint up to the full listing window of nonces per pass,
    // accumulating CredentialOnly entries toward RESUME_LEDGER_CAP
    // (entries clear only when the winners' jobs settle) — wedging
    // its OWN fresh mints behind the cap. The per-pass speculation
    // bound below caps fresh MINTS per pass; refunds restore the
    // cross-pass budget, never the per-pass mint allowance.
    let fresh_mint_allowance = available_slots.saturating_add(STEAL_SPECULATION_ALLOWANCE);
    let mut fresh_mints_this_pass: usize = 0;
    for descriptor in listed {
        // FS-4: the per-pass speculation bound — counted at the mint,
        // never restored by a refund.
        if fresh_mints_this_pass >= fresh_mint_allowance {
            debug!(
                fresh_mints = fresh_mints_this_pass,
                allowance = fresh_mint_allowance,
                "per-pass speculation bound reached; fresh pass ends"
            );
            break;
        }
        // The claim budget: delivered claims plus unanswered potential
        // mints (the CHARGED ledger population — bug_034).
        if claimed.len() + ledger.charged_len() >= available_slots {
            debug!(
                claimed = claimed.len(),
                charged = ledger.charged_len(),
                entries = ledger.len(),
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
            standing: SlotStanding::Charged,
        }) else {
            continue;
        };
        // FS-4: count the mint the moment it exists — an answered
        // refusal refunds the BUDGET above, never this counter.
        fresh_mints_this_pass += 1;
        pass.fresh_mints = fresh_mints_this_pass;
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
        let answer = pull_once(transport, shutdown, req, &descriptor.drv_hash).await;
        // merged_bug_005 — fold the outcome's futility evidence
        // (total over the PullAnswer alphabet; rustc is the census).
        match futility_evidence(&answer) {
            FutilityEvidence::Conversion | FutilityEvidence::NotFutile => {
                pass.non_futile_outcome = true;
            }
            FutilityEvidence::ConversionDisproved => {
                pass.futile_rejections += 1;
                pass.last_rejected_drv = Some(descriptor.drv_hash.clone());
            }
            FutilityEvidence::Inconclusive => {}
        }
        // SIGTERM mid-pass: return what was already claimed so the
        // caller can abort/report those attempts under the grace.
        if matches!(answer, PullAnswer::Shutdown) {
            finish!();
        }
        // merged_bug_014 — the ONE transition law settles the
        // standing (per-arm rationale at [`standing_effect`]); the
        // fresh-lane highlights:
        //   Gone AND BOTH rejection flavors resolve (bug_119 /
        //     merged_bug_074 — the fresh pull's gates run before any
        //     mint, so even an auth-layer refusal disproves a mint
        //     HERE: this very pull was the only one that could have
        //     minted);
        //   NotYetReady refunds, the credential survives
        //     (merged_bug_096: the post-mint TOCTOU arm answers it
        //     AFTER the durable mint committed with this nonce —
        //     under no refactor may this transition drop or revert
        //     the entry, the Q4/TOCTOU rider; the FS-4 speculation
        //     bound, not the charge, keeps the refunded pass from
        //     storming the ledger);
        //   Unanswered keeps the mint Charged (the next pass resumes
        //     it directly with the nonce).
        ledger.apply_standing(job_id, standing_effect(PresentationLane::Fresh, &answer));
        if let PullAnswer::Deliver(assignment) = answer {
            pass.deliveries += 1;
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
    }
    // merged_bug_005 — the latch observes the completed pass (the
    // SIGTERM exits above deliberately do not: an abandoned pass is
    // inconclusive evidence).
    futility.observe_pass(&pass);
    finish!();
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
        /// merged_bug_014 R2: hang exactly the Nth pull (1-based; the
        /// mixed answered-then-lost pass shape). 0 = disabled.
        hang_at_pull: u32,
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
                hang_at_pull: 0,
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
            if self.hang_at_pull != 0 && self.pull_calls == self.hang_at_pull {
                // Exactly this pull's answer is lost.
                std::future::pending::<()>().await;
            }
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
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("brownout-w");
        // Pass 1: fresh mint for A rides the wire; the answer is lost.
        let c1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(c1.is_empty());
        assert_eq!(ledger.len(), 1, "pass 1 minted A (unanswered)");
        // Pass 2: A's unanswered mint still consumes the only slot —
        // the fresh pass must not mint B (pre-fix: outstanding_mints
        // reset to 0 each pass, so every pass minted one more nonce,
        // accumulating to RESUME_LEDGER_CAP = 32x capacity).
        let c2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
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
                standing: SlotStanding::Charged,
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
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("skew-w");
        let c1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(c1.is_empty());
        assert_eq!(ledger.len(), 1, "pass 1: unanswered mint recorded");
        // Pass 2: the resume presentation hits rotation skew.
        let c2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(c2.is_empty());
        assert_eq!(
            ledger.len(),
            1,
            "an auth-layer rejection judges the presentation, not the \
             mint — the credential must survive rotation skew"
        );
        // Pass 3: skew over — the credential recovers the assignment.
        let c3 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
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
            &mut ConversionFutilityLatch::default(),
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
            standing: SlotStanding::Charged,
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            standing: SlotStanding::Charged,
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            standing: SlotStanding::Charged,
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
            &mut ConversionFutilityLatch::default(),
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
            standing: SlotStanding::Charged,
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
            &mut ConversionFutilityLatch::default(),
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
            standing: SlotStanding::Charged,
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(idle.list_calls, 0, "zero slots never even lists");
    }

    /// bug_034 RED: an ANSWERED NotYetReady on a fresh claim — the
    /// raced LOSER lane (the kernel's one-winner arbiter answers the
    /// loser NotYetReady and keeps its entry alive until the winner's
    /// job settles) — must REFUND the pass budget: the credential
    /// survives in the ledger (rule-4b: NotYetReady is NOT proof of
    /// no-mint), but the slot charge it held is released the moment
    /// the scheduler ANSWERS. At slots=1 a raced loser must not idle
    /// the worker for the winner's whole job lifetime.
    #[tokio::test]
    async fn raced_loser_not_yet_ready_refunds_the_claim_budget() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![job_a.clone(), job_b.clone()]))],
            vec![
                Ok(not_yet_ready()),
                Ok(deliver("exec-b", "/nix/store/bbb-two.drv")),
            ],
            vec![],
        );
        let mut ledger = ResumeLedger::default();
        let claimed = poll_and_claim(
            &mut t,
            &instance("raced-loser-w"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &mut ConversionFutilityLatch::default(),
            &token(),
        )
        .await;
        assert_eq!(
            claimed.len(),
            1,
            "left: claimed=[] (budget consumed by the answered loser; the pass \
             breaks at the head) / right: claimed=[B], ledger holds A as \
             CredentialOnly"
        );
        assert_eq!(claimed[0].drv_hash, job_b.drv_hash);
        assert_eq!(
            ledger.len(),
            1,
            "the loser's credential SURVIVES (Q4/TOCTOU: NotYetReady is not \
             proof of no-mint — the entry stays nonce-resumable)"
        );
        assert_eq!(
            ledger.charged_len(),
            0,
            "the answered loser holds NO slot charge (CredentialOnly)"
        );
    }

    /// bug_034 conservation twin (the bug_099/072 anti-storm half
    /// SURVIVES the split): an UNANSWERED mint keeps its charge — the
    /// scheduler may have committed the attempt, so the pass must not
    /// mint past it. Slots=1, A's pull is lost → B is NOT claimed
    /// this pass.
    #[tokio::test(start_paused = true)]
    async fn unanswered_mint_keeps_the_charge() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![job_a.clone(), job_b.clone()]))],
            vec![Ok(deliver("exec-b", "/nix/store/bbb-two.drv"))],
            vec![],
        );
        t.hang_next_pulls = 1;
        let mut ledger = ResumeLedger::default();
        let claimed = poll_and_claim(
            &mut t,
            &instance("unanswered-w"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &mut ConversionFutilityLatch::default(),
            &token(),
        )
        .await;
        assert!(
            claimed.is_empty(),
            "an UNANSWERED mint keeps the slot consumed — B must not be \
             claimed this pass"
        );
        assert_eq!(t.pull_calls, 1, "only A's (lost) pull rode the wire");
        assert_eq!(ledger.charged_len(), 1, "the lost mint stays Charged");
    }

    /// FS-4 RED: the fully-contested pass (every fresh pull answers
    /// NotYetReady — the live 173-replica/16-head regime's steady
    /// state) must NOT storm the ledger. The refund alone would let a
    /// 1-slot worker walk the whole 16-job listing minting a nonce per
    /// descriptor, accumulating CredentialOnly entries toward
    /// RESUME_LEDGER_CAP (they clear only when the winners' jobs
    /// settle) and wedging its own future fresh mints. The per-pass
    /// speculation bound caps fresh mints at
    /// `available_slots + STEAL_SPECULATION_ALLOWANCE` (= 2 at
    /// slots=1).
    ///
    /// Strawman disclosure: pre-split the pass breaks at the FIRST
    /// answered head (the bug_034 defect) and cannot storm, so this
    /// red is recorded against the refund-WITHOUT-bound strawman (the
    /// split alone), not against the pre-fix tree — left: 16 fresh
    /// mints, 16 CredentialOnly entries this pass / right: fresh
    /// mints <= 2, no cap pressure, no outage warn.
    #[tokio::test]
    async fn contested_remainder_does_not_storm_the_ledger() {
        let listed: Vec<_> = (1..=16).map(descriptor).collect();
        let mut t =
            MockTransport::new(vec![Ok(listing(listed))], vec![Ok(not_yet_ready())], vec![]);
        let mut ledger = ResumeLedger::default();
        let claimed = poll_and_claim(
            &mut t,
            &instance("contested-w"),
            1,
            &mut ledger,
            &mut ListFailureLatch::default(),
            &mut ConversionFutilityLatch::default(),
            &token(),
        )
        .await;
        assert!(claimed.is_empty(), "every claim lost the race");
        assert!(
            t.pull_calls <= 2,
            "left: 16 fresh mints (one per listed descriptor) / right: fresh \
             mints <= available_slots + STEAL_SPECULATION_ALLOWANCE (= 2); \
             got {} pulls",
            t.pull_calls
        );
        assert!(
            ledger.len() <= 2,
            "no ledger storm: {} entries accumulated toward the cap",
            ledger.len()
        );
        assert_eq!(
            ledger.charged_len(),
            0,
            "every minted entry was answered (CredentialOnly)"
        );
    }

    /// FS-4 companion green: the cap-refusal severity keys on the
    /// standing MIX — a Charged-dominated full ledger (unanswered
    /// mints) still reads as the scheduler-side outage; a
    /// CredentialOnly-dominated one is the contested-remainder steady
    /// state. The refusal itself stands either way (live credentials
    /// are never evicted).
    #[test]
    fn cap_refusal_severity_keys_on_the_standing_mix() {
        let entry = |standing: SlotStanding| {
            let job = Uuid::now_v7();
            ResumeEntry {
                job_id: job,
                drv_hash: format!("drv-{job}"),
                tenant_hint: None,
                origin: "cache_opportunity".into(),
                nonce: Uuid::new_v4(),
                standing,
            }
        };

        // Charged-dominated: the outage warn shape.
        let mut charged_heavy = ResumeLedger::default();
        for _ in 0..RESUME_LEDGER_CAP {
            charged_heavy.note_pull(entry(SlotStanding::Charged));
        }
        assert!(
            charged_heavy.cap_refusal_is_outage(),
            "32/32 Charged: outage"
        );
        assert!(
            charged_heavy
                .begin_fresh_claim(Uuid::now_v7(), |nonce| {
                    let mut e = entry(SlotStanding::Charged);
                    e.nonce = nonce;
                    e
                })
                .is_none(),
            "the refusal stands"
        );

        // CredentialOnly-dominated: the contested steady state.
        let mut contested = ResumeLedger::default();
        for i in 0..RESUME_LEDGER_CAP {
            contested.note_pull(entry(if i < 2 {
                SlotStanding::Charged
            } else {
                SlotStanding::CredentialOnly
            }));
        }
        assert!(
            !contested.cap_refusal_is_outage(),
            "2 Charged / 30 CredentialOnly: contested remainder, not an outage"
        );
        assert!(
            contested
                .begin_fresh_claim(Uuid::now_v7(), |nonce| {
                    let mut e = entry(SlotStanding::Charged);
                    e.nonce = nonce;
                    e
                })
                .is_none(),
            "the refusal stands either way (credentials are never evicted)"
        );

        // The tie stays loud (conservative direction).
        let mut tie = ResumeLedger::default();
        for i in 0..RESUME_LEDGER_CAP {
            tie.note_pull(entry(if i < RESUME_LEDGER_CAP / 2 {
                SlotStanding::Charged
            } else {
                SlotStanding::CredentialOnly
            }));
        }
        assert!(tie.cap_refusal_is_outage(), "16/16 tie: keep the warn");
    }

    /// merged_bug_014 — the OUTCOME-DERIVED standing (the bug_034
    /// monotone-axiom pin, inverted with the law): pass 1's answered
    /// NotYetReady refunds (the credential survives); pass 2's LOST
    /// resume presentation RE-CHARGES — the kernel may have minted
    /// behind the lost response, so the fresh budget is pinned and B
    /// is NOT minted (pre-merged_bug_014 this very flow over-bound:
    /// the lost resume left zero charge and B claimed alongside the
    /// possibly-live orphan). The retired assertion lives in this
    /// test's history; the rationale is in the introducing commit.
    #[tokio::test(start_paused = true)]
    async fn lost_resume_response_recharges_and_pins_the_budget() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![
                Ok(listing(vec![job_a.clone()])),
                Ok(listing(vec![job_a.clone(), job_b.clone()])),
            ],
            vec![Ok(not_yet_ready())],
            vec![],
        );
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("recharge-pin-w");
        let c1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(c1.is_empty());
        assert_eq!(ledger.charged_len(), 0, "pass 1: answered → refunded");

        // Pass 2: the resume pull for A is lost — the slot re-charges
        // and the pass cannot mint B (the honest-beat gate withholds
        // the listing outright once the budget is pinned mid-pass...
        // here the re-charge lands DURING the resume lane, so the
        // gate sees it before any listing).
        t.hang_next_pulls = 1;
        t.pulls = vec![Ok(deliver("exec-b", "/nix/store/bbb-two.drv"))].into();
        let c2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(
            c2.is_empty(),
            "the lost resume re-charged the slot — B must NOT be minted \
             over the possibly-live orphan"
        );
        assert_eq!(ledger.len(), 1, "A's credential still rides");
        assert_eq!(
            ledger.charged_len(),
            1,
            "Charged again: the LAST claiming presentation is unanswered"
        );
    }

    /// merged_bug_014 — THE transition table, total over
    /// (lane × PullAnswer) by compiler census (R15): the alphabet
    /// vector's completeness is FORCED — `pull_answer_variant_index`
    /// is a wildcard-free match (a new variant fails the build), and
    /// the bijection assertion below fails if the vector omits one.
    /// Each row's expected effect is the law's reviewable table.
    #[test]
    fn standing_transition_total_over_pull_answers() {
        const PULL_ANSWER_VARIANTS: usize = 7;
        fn pull_answer_variant_index(a: &PullAnswer) -> usize {
            match a {
                PullAnswer::Deliver(_) => 0,
                PullAnswer::Gone => 1,
                PullAnswer::NotYetReady => 2,
                PullAnswer::Shutdown => 3,
                PullAnswer::Unanswered => 4,
                PullAnswer::RejectedDisproving => 5,
                PullAnswer::RejectedAuth => 6,
            }
        }
        let alphabet: Vec<PullAnswer> = vec![
            PullAnswer::Deliver(Box::default()),
            PullAnswer::Gone,
            PullAnswer::NotYetReady,
            PullAnswer::Shutdown,
            PullAnswer::Unanswered,
            PullAnswer::RejectedDisproving,
            PullAnswer::RejectedAuth,
        ];
        let mut seen = [false; PULL_ANSWER_VARIANTS];
        for a in &alphabet {
            seen[pull_answer_variant_index(a)] = true;
        }
        assert!(
            seen.iter().all(|s| *s) && alphabet.len() == PULL_ANSWER_VARIANTS,
            "the census vector must cover every PullAnswer variant exactly"
        );

        use PresentationLane as L;
        use StandingEffect as E;
        for a in &alphabet {
            let idx = pull_answer_variant_index(a);
            let expected: [(L, E); 3] = match idx {
                // Deliver: settle on claiming lanes; a probe delivery
                // proves the mint is live (re-charge + discard).
                0 => [
                    (L::Fresh, E::Resolve),
                    (L::ResumeClaiming, E::Resolve),
                    (L::Probe, E::Recharge),
                ],
                1 => [
                    (L::Fresh, E::Resolve),
                    (L::ResumeClaiming, E::Resolve),
                    (L::Probe, E::Resolve),
                ],
                2 => [
                    (L::Fresh, E::Refund),
                    (L::ResumeClaiming, E::Refund),
                    (L::Probe, E::Refund),
                ],
                3 => [
                    (L::Fresh, E::Keep),
                    (L::ResumeClaiming, E::Keep),
                    (L::Probe, E::Keep),
                ],
                4 => [
                    (L::Fresh, E::Recharge),
                    (L::ResumeClaiming, E::Recharge),
                    (L::Probe, E::Keep),
                ],
                5 => [
                    (L::Fresh, E::Resolve),
                    (L::ResumeClaiming, E::Resolve),
                    (L::Probe, E::Resolve),
                ],
                6 => [
                    (L::Fresh, E::Resolve),
                    (L::ResumeClaiming, E::Keep),
                    (L::Probe, E::Keep),
                ],
                _ => unreachable!("variant census bound above"),
            };
            for (lane, want) in expected {
                assert_eq!(
                    standing_effect(lane, a),
                    want,
                    "cell ({lane:?}, variant {idx})"
                );
            }
        }
    }

    /// merged_bug_014 green pin: an auth-layer rejection of a RESUME
    /// presentation leaves the standing UNTOUCHED — the rejection is
    /// pre-mint by construction (merged_bug_074's law), so THIS
    /// presentation provably did not mint: no re-charge, no refund,
    /// the credential survives the skew.
    #[tokio::test(start_paused = true)]
    async fn rejected_auth_presentation_leaves_standing_untouched() {
        let job_a = descriptor(1);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![job_a.clone()]))],
            vec![
                Ok(not_yet_ready()),
                Err(tonic::Status::permission_denied("rotation skew")),
            ],
            vec![],
        );
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("skew-standing-w");
        let p1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p1.is_empty());
        assert_eq!(ledger.charged_len(), 0, "answered: CredentialOnly");
        let p2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p2.is_empty());
        assert_eq!(ledger.len(), 1, "the credential survives the skew");
        assert_eq!(
            ledger.charged_len(),
            0,
            "auth skew judges the presentation, not the mint: standing \
             untouched (no re-charge)"
        );
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
        let mut claimed: Vec<ClaimedJob> = Vec::new();
        for _ in 0..5 {
            claimed = poll_and_claim(
                &mut transport,
                &instance("store-replica-0"),
                1,
                &mut ResumeLedger::default(),
                &mut ListFailureLatch::default(),
                &mut ConversionFutilityLatch::default(),
                &token(),
            )
            .await
            .into_iter()
            .collect();
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
            &mut ConversionFutilityLatch::default(),
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
                standing: SlotStanding::Charged,
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
                standing: SlotStanding::Charged,
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

    // -----------------------------------------------------------------
    // merged_bug_005 — the honest beat (store.materialize.honest-beat)
    // -----------------------------------------------------------------

    // r[verify store.materialize.honest-beat]
    /// merged_bug_005 RED 1 + the resume-lane pin. Proposition
    /// certified (R16): a pass whose claim budget is pinned by a
    /// Charged orphan issues NO listing RPC (the beat is the
    /// scheduler's capability proxy; a pass that cannot mint must not
    /// claim freshness) while the RESUME presentation still rides
    /// (answer-gathering is never withheld). Witness: the scripted
    /// transport's call census — the steal horizon's own input —
    /// never log text.
    #[tokio::test(start_paused = true)]
    async fn budget_pinned_pass_withholds_the_listing_beat() {
        let job_a = descriptor(1);
        let mut t = MockTransport::new(vec![Ok(listing(vec![job_a.clone()]))], vec![], vec![]);
        t.hang_next_pulls = 99; // every pull lost (fresh, then resume)
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("gate-budget-w");
        // Pass 1 (production flow): lists, fresh-mints A, the answer
        // is lost — A becomes the Charged orphan.
        let c1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(c1.is_empty());
        assert_eq!(ledger.charged_len(), 1, "precondition: A is Charged");
        assert_eq!(t.list_calls, 1, "precondition: pass 1 listed");
        let pulls_before = t.pull_calls;
        // Pass 2: budget pinned by the orphan — the beat is withheld;
        // the resume presentation still rides.
        let c2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(c2.is_empty());
        assert_eq!(
            t.pull_calls,
            pulls_before + 1,
            "the gated pass still runs the resume lane (presentations \
             are answer-gathering, never withheld)"
        );
        assert_eq!(
            t.list_calls, 1,
            "left: 1 ListMaterializationJobs RPC issued on a pass that \
             cannot mint (the false liveness beat) / right: 0 — the beat \
             is withheld; the resume presentation still rode"
        );
    }

    // r[verify store.materialize.honest-beat]
    /// merged_bug_005 RED 2. Proposition certified (R16): a pass whose
    /// ledger sits at RESUME_LEDGER_CAP issues NO listing RPC — every
    /// begin_fresh_claim would refuse, so the listed slice could never
    /// convert. The resume lane still presents all 32 credentials.
    ///
    /// Cap-shape seeding via the #[cfg(test)] note_pull raw insert
    /// (disclosed: a 32-entry CredentialOnly population arises in
    /// production only across many contested passes; the raw insert is
    /// the documented seeding lane for cap shapes — the standings and
    /// the gate decision under test are production code).
    #[tokio::test]
    async fn cap_full_ledger_withholds_the_listing_beat() {
        let mut ledger = ResumeLedger::default();
        for _ in 0..RESUME_LEDGER_CAP {
            let job = Uuid::now_v7();
            ledger.note_pull(ResumeEntry {
                job_id: job,
                drv_hash: format!("drv-{job}"),
                tenant_hint: None,
                origin: "cache_opportunity".into(),
                nonce: Uuid::new_v4(),
                standing: SlotStanding::CredentialOnly,
            });
        }
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![descriptor(1)]))],
            vec![Ok(not_yet_ready())],
            vec![],
        );
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let claimed = poll_and_claim(
            &mut t,
            &instance("gate-cap-w"),
            1,
            &mut ledger,
            &mut latch,
            &mut fut,
            &token(),
        )
        .await;
        assert!(claimed.is_empty());
        assert_eq!(
            t.pull_calls, RESUME_PRESENTATIONS_PER_PASS as u32,
            "the resume lane still presented its bounded window \
             (merged_bug_014: presentations are pass-bounded; the \
             rotation covers the rest across passes)"
        );
        assert_eq!(
            t.list_calls, 0,
            "left: listing issued then every begin_fresh_claim refused \
             / right: no listing"
        );
    }

    // r[verify store.materialize.honest-beat]
    /// merged_bug_005 RED 3 — the conversion-futility latch.
    /// Proposition certified (R16): a streak of passes whose EVERY
    /// fresh mint is answered with a conversion-disproving rejection
    /// stops beating at the threshold — the rendezvous slice re-homes
    /// instead of staying pinned to a worker that lists but can never
    /// convert. NotYetReady contest losses never count (pinned by the
    /// existing FS-4 battery staying green).
    #[tokio::test]
    async fn futile_conversion_streak_backs_off_listing() {
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![descriptor(7)]))],
            vec![Err(tonic::Status::invalid_argument(
                "request shape can never mint",
            ))],
            vec![],
        );
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("gate-futile-w");
        for _ in 0..5 {
            let c = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
            assert!(c.is_empty(), "every mint is refused");
        }
        assert_eq!(
            t.list_calls, 3,
            "left: 5 listing RPCs (the slice stays pinned) / right: 3 \
             (the threshold), then withheld until the probe interval"
        );
        assert!(
            fut.withholding(),
            "the latch holds the beat through the re-probe interval"
        );
    }

    // r[verify store.materialize.honest-beat]
    /// R17 const-relation pin: the futility re-probe interval at the
    /// production beat floor (1 s — claim_loop clamps
    /// `poll_interval_secs.max(1)`) covers the scheduler's
    /// listing-membership TTL, so a withheld worker leaves the
    /// membership ENTIRELY and its slice re-homes permanently (not
    /// just via the 5 s steal horizon).
    #[test]
    fn futile_latch_probe_interval_exceeds_member_ttl() {
        const MIN_BEAT_SECS: u64 = 1;
        // Compile-anchored (R17): the relations are const blocks — a
        // constant change that breaks the law fails the BUILD, not a
        // test run.
        const {
            assert!(
                (FUTILE_RELIST_INTERVAL_PASSES as u64) * MIN_BEAT_SECS
                    >= SCHEDULER_LISTING_MEMBER_TTL_SECS,
                "the withhold interval must outlast the scheduler's membership TTL"
            );
        }
        const {
            assert!(
                SCHEDULER_LISTING_STEAL_HORIZON_SECS < SCHEDULER_LISTING_MEMBER_TTL_SECS,
                "mirrored-constant sanity: horizon below TTL"
            );
        }
    }

    // -----------------------------------------------------------------
    // merged_bug_014 — outcome-derived charge + bounded presentations
    // -----------------------------------------------------------------

    /// merged_bug_014 RED 1 (hole 1 — the resume-lane re-charge edge).
    /// Proposition certified (R16): a claiming presentation left
    /// UNANSWERED re-charges the slot — Charged derives from "the
    /// entry's LAST claiming presentation is unanswered", regardless
    /// of which lane presented (the kernel mints on ANY claiming
    /// presentation of a pending job; the scheduler's confirm screen
    /// gates only confirm probes). Pre-fix the monotone axiom left
    /// the entry CredentialOnly with zero charge: a 1-slot worker
    /// over-bound while a possibly-live orphan attempt existed, and
    /// the orphan aged into the charged establishment sweep against a
    /// healthy worker.
    #[tokio::test(start_paused = true)]
    async fn resume_unanswered_presentation_recharges_the_slot() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![
                Ok(listing(vec![job_a.clone()])),
                Ok(listing(vec![job_b.clone()])),
            ],
            vec![
                // Pass 1: A's fresh mint answers NotYetReady — the
                // production CredentialOnly path (contest lost; the
                // TOCTOU arm may still have minted server-side).
                Ok(not_yet_ready()),
                // Pass 2 (pre-fix only): B's fresh mint delivers — the
                // over-bind this red exists to kill.
                Ok(deliver("exec-overbind", "/nix/store/overbind.drv")),
            ],
            vec![],
        );
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("recharge-w");
        let p1 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p1.is_empty());
        assert_eq!(ledger.len(), 1, "A holds a credential");
        assert_eq!(ledger.charged_len(), 0, "answered contest: refunded");

        // Pass 2: A's resume presentation TIMES OUT (a mint may have
        // committed server-side, bound to this worker).
        t.hang_next_pulls = 1;
        let p2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert_eq!(
            ledger.charged_len(),
            1,
            "left: charged_len()==0 and the fresh lane mints another job \
             this pass (over-bind at slots=1, orphan abandoned to the \
             establishment sweep) / right: charged_len()==1, fresh lane \
             blocked until the orphan answers"
        );
        assert!(
            p2.is_empty(),
            "the re-charged orphan pins the budget — no fresh claim may \
             ride this pass (got {:?})",
            p2.iter().map(|j| j.drv_hash.as_str()).collect::<Vec<_>>()
        );
    }
    /// merged_bug_014 RED 2 (hole 2 — the stranding break). Proposition
    /// certified (R16): once delivered claims fill the slots, remaining
    /// presented entries switch to confirm_only PROBES (the screen
    /// converts would-be-DeliverNew to NotYetReady; DeliverExisting
    /// passes through with the payload DISCARDED) — a Charged sibling
    /// is never stranded unpresented behind a delivered resume for the
    /// whole inline walk.
    #[tokio::test(start_paused = true)]
    async fn delivered_resume_does_not_strand_charged_sibling() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![job_a.clone(), job_b.clone()]))],
            vec![
                // Pass 1 fresh lane: A answers NotYetReady
                // (CredentialOnly); B's mint (pull #2) HANGS — the
                // Charged orphan.
                Ok(not_yet_ready()),
                // Pass 2: A's resume DELIVERS; B's probe answers
                // NotYetReady (non-holdership — refund).
                Ok(deliver_for_job(
                    "exec-resumed-a",
                    "/nix/store/resumed-a.drv",
                    Uuid::nil(),
                )),
                Ok(not_yet_ready()),
            ],
            vec![],
        );
        t.hang_at_pull = 2;
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("strand-w");
        // Pass 1 at slots=2: A minted (answered NotYetReady —
        // CredentialOnly); B minted, answer LOST (Charged).
        let p1 = poll_and_claim(&mut t, &inst, 2, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p1.is_empty(), "pass 1: A contested, B lost");
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.charged_len(), 1, "B is the Charged orphan");

        // Pass 2 at slots=1: A's resume delivers (slots full), then B
        // MUST still be presented — as a confirm probe.
        let pulls_before = t.pull_calls;
        let p2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert_eq!(p2.len(), 1, "A's resume delivered");
        assert_eq!(
            t.pull_calls - pulls_before,
            2,
            "left: zero presentations for B this pass (strands for the \
             walk) / right: B presented confirm_only this pass; its \
             standing settles from the probe answer"
        );
        let probe_req = t.seen_pull_requests.last().expect("B's probe rode");
        assert!(
            probe_req.confirm_only,
            "past full slots the presentation is a PROBE (no mint can \
             occur behind the screen)"
        );
        assert_eq!(
            ledger.charged_len(),
            0,
            "B's probe answered NotYetReady — non-holdership refunds"
        );
    }

    /// merged_bug_014 RED 3 (hole 3 — the unbounded brownout pass).
    /// Proposition certified (R16): the FIRST Unanswered presentation
    /// ends the pass (a brownout answers nobody — one timeout burn per
    /// pass, not one per entry) and presentations are bounded per pass.
    #[tokio::test(start_paused = true)]
    async fn resume_pass_short_circuits_on_first_timeout() {
        let listed: Vec<_> = (1..=8).map(descriptor).collect();
        let mut t = MockTransport::new(vec![Ok(listing(listed))], vec![], vec![]);
        t.hang_next_pulls = 99;
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("shortcircuit-w");
        // Pass 1 at slots=8: eight fresh mints, every answer lost —
        // eight Charged entries through the production mint authority.
        let p1 = poll_and_claim(&mut t, &inst, 8, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p1.is_empty());
        assert_eq!(ledger.charged_len(), 8, "eight lost mints");

        // Pass 2: the resume lane hits the first timeout and stops.
        let pulls_before = t.pull_calls;
        let p2 = poll_and_claim(&mut t, &inst, 8, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p2.is_empty());
        assert_eq!(
            t.pull_calls - pulls_before,
            1,
            "left: 8 presentations issued (8 sequential timeout burns, \
             ~16 min at cap against a browned-out scheduler) / right: 1 \
             — the first Unanswered ends the pass"
        );
    }

    /// merged_bug_014 green pin: a confirm probe answered with MY
    /// live mint (DeliverExisting through the screen) keeps the
    /// charge AND discards the payload — the probe is a standing
    /// oracle, never an execution source; the next pass's CLAIMING
    /// presentation re-delivers it for execution.
    #[tokio::test(start_paused = true)]
    async fn confirm_probe_deliver_existing_keeps_charge_and_discards_payload() {
        let job_a = descriptor(1);
        let job_b = descriptor(2);
        let mut t = MockTransport::new(
            vec![Ok(listing(vec![job_a.clone(), job_b.clone()]))],
            vec![
                Ok(not_yet_ready()),
                Ok(deliver_for_job(
                    "exec-resumed-a2",
                    "/nix/store/resumed-a2.drv",
                    Uuid::nil(),
                )),
                Ok(deliver_for_job(
                    "exec-live-b",
                    "/nix/store/live-b.drv",
                    Uuid::nil(),
                )),
            ],
            vec![],
        );
        t.hang_at_pull = 2; // B's mint is the lost one
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("probe-deliver-w");
        let p1 = poll_and_claim(&mut t, &inst, 2, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p1.is_empty());
        assert_eq!(ledger.charged_len(), 1, "B Charged (lost mint)");

        // Pass 2 at slots=1: the Charged orphan B presents FIRST (the
        // window's unanswered-orphan priority) and DELIVERS the
        // claiming slot; A's PROBE then answers DeliverExisting — a
        // PROVEN-live mint: payload discarded, A re-charges.
        let p2 = poll_and_claim(&mut t, &inst, 1, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert_eq!(p2.len(), 1, "only the claiming delivery executes");
        assert_eq!(
            p2[0].drv_hash, job_b.drv_hash,
            "the Charged orphan won the claiming slot; A's probe \
             payload was discarded"
        );
        let probe_req = t.seen_pull_requests.last().expect("A probed");
        assert!(probe_req.confirm_only);
        assert_eq!(
            ledger.charged_len(),
            1,
            "A re-charged: its probe PROVED a live mint (budget honesty \
             — a proven-live mint holds a slot); the next claiming pass \
             re-delivers it for execution"
        );
    }

    /// merged_bug_014 green pin (the rotation/starvation bound): with
    /// 8 live entries and a 4-presentation window, two passes present
    /// ALL EIGHT distinct entries — every live entry within
    /// ceil(len/bound) passes.
    #[tokio::test(start_paused = true)]
    async fn presentation_rotation_covers_all_entries() {
        let listed: Vec<_> = (1..=8).map(descriptor).collect();
        let mut t = MockTransport::new(vec![Ok(listing(listed))], vec![], vec![]);
        t.hang_next_pulls = 99;
        let mut ledger = ResumeLedger::default();
        let mut latch = ListFailureLatch::default();
        let mut fut = ConversionFutilityLatch::default();
        let tok = token();
        let inst = instance("rotation-w");
        // Pass 1 at slots=8: eight Charged entries (production mints).
        let p1 = poll_and_claim(&mut t, &inst, 8, &mut ledger, &mut latch, &mut fut, &tok).await;
        assert!(p1.is_empty());
        assert_eq!(ledger.charged_len(), 8);

        // Passes 2-3: answers now flow (NotYetReady) — each pass
        // presents a 4-entry window; rotation must cover all eight.
        t.hang_next_pulls = 0;
        t.pulls = vec![Ok(not_yet_ready())].into();
        let presented_before = t.seen_pull_requests.len();
        let _p2 = poll_and_claim(&mut t, &inst, 8, &mut ledger, &mut latch, &mut fut, &tok).await;
        let _p3 = poll_and_claim(&mut t, &inst, 8, &mut ledger, &mut latch, &mut fut, &tok).await;
        let resumed: std::collections::HashSet<String> = t.seen_pull_requests[presented_before..]
            .iter()
            .filter(|r| !r.claim_nonce.is_empty() && r.resume_exec_id.is_empty())
            .map(|r| r.intent_id.clone())
            .collect();
        assert_eq!(
            resumed.len(),
            8,
            "two 4-entry windows must cover all eight live entries \
             (ceil(8/4) = 2 passes — the starvation bound)"
        );
    }
}
