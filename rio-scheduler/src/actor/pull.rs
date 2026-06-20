//! Pull-mode dispatch: the `PullAssignment` admission kernel and the
//! actor-side pull transaction.
//!
//! A pull-mode pod is born knowing its derivation (the HMAC-attested
//! `intent_id`); its first and only ask is `PullAssignment`, which
//! either delivers the existing `WorkAssignment` payload, says `Gone`
//! (no longer wanted, exit 0 charge-free), or says `NotYetReady`
//! (wanted but not currently deliverable to *this* pod — deps unbuilt,
//! or the drv is open on another executor). The decision is computed by
//! the pure [`admit_pull`] kernel from already-loaded state so a
//! Phase-2 Kani harness needs no refactor; the actor handler executes
//! the decision, and the durable mint runs as one generation-fenced
//! transaction (`SchedulerDb::mint_pull_attempt_fenced`).
//!
//! The attempt's executor identity is the attested intent id itself:
//! the pull request carries no pod name (the token is signed before the
//! controller picks one), so the binding key is the identity the token
//! attests. Pod/node attribution is carried separately by
//! `source_node` (the controller-authoritative binding) and by the
//! controller's `ReportAttemptOutcome`. A re-pull by the pod (or a
//! replacement pod of the same intent) therefore converges on the same
//! open attempt instead of minting a second one, and an attempt held by
//! a *different* identity is never re-delivered or re-pointed.

use tokio::sync::oneshot;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::{DerivationStatus, DrvHash, ExecutorId};

use super::DagActor;

/// Server-suggested re-pull delay carried by `NotYetReady` (decision
/// P4: 5 s; the pod adds jitter). The pod-side idle bound reuses the
/// existing builder `idle_timeout`, so no second number exists here.
pub(crate) const NOT_YET_READY_RETRY_AFTER_SECS: u32 = 5;

/// The kind-selected side-effect profile of one pull mint (A2.3 —
/// bug_075/091/106): every kind-divergent decision in
/// [`DagActor::mint_and_deliver`] is made HERE, in the single match on
/// the work class at the top of the mint. The body below the match
/// consumes profile fields only — adding a new kind-divergent side
/// effect to the mint without deciding it for BOTH classes is
/// unwritable (a missing struct field does not compile).
struct MintProfile {
    /// The persisted work class — data for the durable write and the
    /// kinded transition, never compared below the profile match.
    attempt_kind: crate::state::AttemptKind,
    /// Node attribution. Build: the controller-authoritative binding.
    /// Materialization: `None` BY PROFILE — node attribution is a
    /// build-lane concept (the 084 CHECK makes the regression
    /// unrepresentable; bug_075's primary effect was a builder-pod
    /// binding stamped onto a store execution, feeding wrong exclusion
    /// keys — and post-084, failing every dep-racing claim).
    source_node: Option<String>,
    /// The deadline the establishment window anchors to. Build:
    /// `max(carried rendered, mint-time re-solve)` — the carried
    /// rendered deadline (`BoundIntent.deadline_secs`) is what the
    /// pod's `activeDeadlineSeconds` was REALLY rendered from, so the
    /// persisted anchor must never sit below it (bug_106).
    /// Materialization: `materialization.attempt_deadline_secs` — a
    /// store walk never runs under a build pod's deadline (bug_075's
    /// establishment half).
    deadline_secs: Option<f64>,
    /// Whether this mint is the spawn-success edge that clears the
    /// single-cell ICE mask and consumes `dispatched_cells`. Build
    /// only: a store replica's claim says nothing about builder-pod
    /// scheduling (bug_091).
    clear_ice: bool,
    /// GC live-input pins — build-lane lifecycle only (pin-at-ingest
    /// is store-side for materialization).
    pin_live_inputs: bool,
    /// Whether the committed mint is noted in the in-memory
    /// materialization job view (the kernel's one-winner arbitration).
    note_claimed: bool,
    /// The display surface, via the kernel's single kind-to-surface
    /// projection (`r[sched.pull.kinded-running-surface]`).
    surface: rio_evidence_kernel::pull::DisplaySurface,
}

/// What the scheduler answers a `PullAssignment` with.
#[derive(Debug)]
pub enum PullOutcome {
    /// Deps Ready and the open attempt is bound to the pulling
    /// identity: the dispatch payload (identical on every re-pull
    /// while the attempt stays open).
    Deliver(Box<rio_proto::types::WorkAssignment>),
    /// The derivation is no longer wanted; the pod exits 0 charge-free.
    /// Carries the exit-0 license proof (merged_bug_011): a keyed Gone
    /// is constructible only from a durable fence witness.
    Gone(GoneLicense),
    /// Wanted but not currently deliverable to this pod; re-pull after
    /// the suggested delay.
    NotYetReady { retry_after_secs: u32 },
}

/// The exit-0 license a `Gone` answer carries (merged_bug_011,
/// keystone 1): on the keyed build lane the license IS the durable
/// fence witness — [`crate::db::confirm_fences::ConfirmFenceDurable`]
/// is mintable only by the fence write/read, so a keyed Gone with no
/// row on disk does not typecheck. The unfenced license takes the
/// [`NonKeyedLane`] proof, mintable only at the lane-classification
/// match where `FenceLane::Keyed` is structurally excluded — a Keyed
/// lane has no path to a `NonKeyedLane` value, so the bypass is a
/// compile error, not a runtime check (the debug_assert form was
/// REJECTED per FS-5: debug_assert compiles out of release builds and
/// is not a typecheck).
#[derive(Debug)]
pub enum GoneLicense {
    /// The fence row for this token is durable (written ahead of this
    /// answer, or read by the DeliverNew screen).
    Fenced(crate::db::confirm_fences::ConfirmFenceDurable),
    /// This pull runs on a lane with no fence key domain — proof of
    /// non-keyed-ness, not an exemption.
    ///
    /// The proof is spent by EXISTING (constructibility is the
    /// property; nothing downstream needs to re-read which lane —
    /// `allow(dead_code)` records that deliberately).
    Unfenced(#[allow(dead_code)] NonKeyedLane),
}

/// The typed resolution a `ReportAttemptOutcome` ack carries (the
/// merged_bug_080 C-2 contract carrier): whether THIS report was
/// applied to — or matched an already-recorded terminal classification
/// of — an actual attempt, i.e. the scheduler holds a verdict for it.
/// Both report-fold fns return `Result<AttemptResolution,
/// PullRejection>`, so rustc exhaustiveness over their return sites IS
/// the charge-free-arm census generator (R15): adding an arm without
/// stating its resolution does not compile, and the admin layer
/// consumes the witness into the wire `attempt_resolved` bit at its
/// single response-construction site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptResolution {
    /// An actual attempt was classified by this report, or the report
    /// matched an attempt whose terminal classification is already
    /// recorded (the idempotent-duplicate arm — the scheduler HOLDS a
    /// verdict either way).
    Resolved,
    /// Charge-free ack: nothing was resolved (no matching attempt,
    /// refused synthesized verdict, materialization-kind refusal,
    /// unclassified-open-attempt ack, and EVERY NoEligibleSource arm —
    /// the spawn-gate verdict rides its own poison lane, never an
    /// attempt resolution; the controller's NoEligibleSource reset
    /// rides its own mint, independent of this bit).
    Unresolved,
}

/// The ONE derivation of "the shape this attempt is dispatched under"
/// (bug_027): the mint-time solve reconciled with the CARRIED
/// `BoundIntent` deadline (the pod runs under the carried rendered
/// shape — bug_106), consumed by BOTH the persisted establishment
/// anchor and the `last_intent` stamp. Two ad-hoc derivations of this
/// fact in one function guaranteed sibling drift: the anchor lifted
/// `max(resolved, carried)` while the stamp wrote the raw solve, so
/// the D4 corroboration anchor undersized on refit-down — the new
/// floor could land at (or below) the deadline that provably failed,
/// with `promoted=true` riding the promotion-exempt lane (an uncounted
/// guaranteed-futile full-length retry) — and `set_dim`'s at_cap test
/// (`target ≥ cap`) had the symmetric carried-at-ceiling hole.
///
/// `reconcile` is the total match over the `(solve, carried)` presence
/// product; a future carried sizing dimension lands at THIS seam and
/// nowhere else. Disclosed residual (priced): the mem/disk axes have
/// NO carried source on today's wire (`PlannedBinding` carries
/// `deadline_secs` only) — a refit-down OOM at the spawn-rendered mem
/// limit still doubles from the mint value and can burn
/// promotion-exempt futile retries until the doubling ladder passes
/// the spawn limit (≥1 step; >1 only when spawn > 2× mint, i.e. a
/// multi-step refit between spawn and pull — seconds-to-minutes in
/// practice). Extending `BoundIntent` is a proto/wire change owned
/// elsewhere (recorded in the wave handoff, not here).
enum DispatchShape {
    /// A solve exists; its `deadline_secs` is ALREADY lifted to
    /// `max(resolved, carried)` — the shape actually dispatched.
    Solved(crate::state::SolvedIntent),
    /// No solve, but a carried deadline exists. NOT stamped onto
    /// `last_intent`: a deadline-only `SolvedIntent` would poison the
    /// sizing triple (cores/mem/disk) with zeros — refused by
    /// construction; the establishment anchor still consumes the
    /// deadline.
    CarriedOnly { deadline_secs: u32 },
    /// Neither source — and the materialization lane, which sizes
    /// nothing and runs under its own config deadline.
    Unsized,
}

impl DispatchShape {
    /// Total over the presence product: `(Some, Some)` lifts the
    /// deadline to the max, `(Some, None)` passes the solve through,
    /// `(None, Some)` → CarriedOnly, `(None, None)` → Unsized.
    fn reconcile(
        solve: Option<crate::state::SolvedIntent>,
        carried_deadline_secs: Option<u32>,
    ) -> Self {
        match (solve, carried_deadline_secs) {
            (Some(mut s), Some(c)) => {
                s.deadline_secs = s.deadline_secs.max(c);
                Self::Solved(s)
            }
            (Some(s), None) => Self::Solved(s),
            (None, Some(c)) => Self::CarriedOnly { deadline_secs: c },
            (None, None) => Self::Unsized,
        }
    }

    /// The establishment-anchor deadline — behavior-identical to the
    /// previous `max()/or` table (the anchor leg carries no red; the
    /// existing anchor tests pin it).
    fn anchor_deadline_secs(&self) -> Option<f64> {
        match self {
            Self::Solved(s) => Some(f64::from(s.deadline_secs)),
            Self::CarriedOnly { deadline_secs } => Some(f64::from(*deadline_secs)),
            Self::Unsized => None,
        }
    }

    /// The `last_intent` stamp: the dispatched shape when a solve
    /// exists (deadline LIFTED — floor.rs's `base = max(floor, last)`
    /// law now reads what was actually dispatched), nothing otherwise
    /// (see [`Self::CarriedOnly`]).
    fn stamp(self) -> Option<crate::state::SolvedIntent> {
        match self {
            Self::Solved(s) => Some(s),
            Self::CarriedOnly { .. } | Self::Unsized => None,
        }
    }
}

/// Why a `PullAssignment` was refused without an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRejection {
    /// The serving replica is not the leader (or lost the lease while
    /// handling the pull). Retryable — same class as `ensure_leader`.
    NotLeader,
    /// The serving generation is below the durable claims floor (the
    /// transaction-side fence). Retryable not-leader class.
    StaleGeneration,
    /// The HMAC-attested intent does not match the requested intent.
    TokenMismatch,
    /// A required durable write did not land (the NACK law, bug_182's
    /// consumption close; merged_bug_145's confirm fence): the caller
    /// is NOT answered — the retry re-presents the SAME request and
    /// the idempotent write retries. Retryable (UNAVAILABLE):
    /// strictly better than acking a lost close (a charged
    /// 'unreported' establishment an hour later) or licensing an
    /// unfenced exit-0 (an invisible open attempt against a
    /// Succeeded Job).
    ConsumptionNotDurable,
    /// Database failure while admitting or minting.
    Internal(String),
}

impl PullRejection {
    // r[impl sched.grpc.fence-retryable]
    /// The refusal class (the same law as [`ActorError::retry_class`]
    /// — `Retryable ⟺ code ∈ {UNAVAILABLE, RESOURCE_EXHAUSTED}`,
    /// pinned by `retry_class_code_consistency`).
    ///
    /// [`ActorError::retry_class`]: crate::actor::ActorError::retry_class
    pub(crate) fn retry_class(&self) -> super::command::RetryClass {
        use super::command::RetryClass;
        match self {
            // Leadership-class refusals: valid pulls another replica
            // serves. Non-durable consumption closes: the SAME replica
            // serves the redelivery once PG recovers.
            Self::NotLeader | Self::StaleGeneration | Self::ConsumptionNotDurable => {
                RetryClass::Retryable
            }
            // Mis-bound token / internal failure: unservable as posed.
            Self::TokenMismatch | Self::Internal(_) => RetryClass::Terminal,
        }
    }
}

/// The pure admission decision for one pull: the CBMC-verified
/// [`rio_evidence_kernel::pull::PullAdmission`] alphabet, instantiated
/// with the scheduler's exec-id type. The decision logic itself lives
/// in the kernel ([`rio_evidence_kernel::pull::admit_pull`]); the
/// scheduler's [`admit_pull`] is the projection shim over it.
pub(crate) type PullDecision = rio_evidence_kernel::pull::PullAdmission<Uuid>;

/// Everything [`admit_pull`] needs, already loaded by the caller.
pub(crate) struct PullInputs<'a> {
    /// The request's intent id (== drv hash, the DAG key).
    pub intent_id: &'a str,
    /// The HMAC-attested intent binding (`None` = dev mode, no key).
    pub auth_intent: Option<&'a str>,
    /// The derivation's current status; `None` if the DAG has no node.
    pub status: Option<DerivationStatus>,
    /// The open attempt bound to the derivation, if any:
    /// (executor identity, exec_id).
    pub open_attempt: Option<(&'a ExecutorId, Uuid)>,
    /// The identity this pull would bind a fresh attempt to.
    pub pulling_identity: &'a ExecutorId,
    /// The serving replica's lease generation.
    pub serving_generation: u64,
    /// The durable claims floor (`None` = fresh cluster, no rows).
    pub generation_floor: Option<i64>,
    /// The pull's claimed work class.
    pub pull_kind: rio_evidence_kernel::pull::PullKind,
    /// The node's materialization-job state, projected from the actor's
    /// in-memory job view.
    pub job_view: rio_evidence_kernel::pull::JobView,
    /// Whether the node's transient-retry backoff has expired (`true`
    /// when none is set) — bug_282: gates the fresh from-source mint
    /// in the kernel's `(Build, JobView::None)` arm.
    pub build_backoff_expired: bool,
    /// merged_bug_158: the materialization re-delivery resume token
    /// (`PullAssignmentRequest.resume_exec_id`, parsed at the gRPC
    /// boundary; unparseable ⇒ `None` = deny-by-default fresh claim).
    pub resume_exec_id: Option<Uuid>,
    /// bug_251 (rule-4b): the claim nonce PRESENTED on this pull
    /// (`PullAssignmentRequest.claim_nonce`) — the lost-response
    /// resume credential.
    pub presented_nonce: Option<Uuid>,
    /// The open attempt's PERSISTED claim nonce (the node mirror of
    /// `assignments.claim_nonce`; hydrated at mint and at recovery).
    pub attempt_nonce: Option<Uuid>,
}

// r[impl sched.executor.pull-gone+1]
// r[impl sched.executor.pull-not-ready+2]
/// Decide one pull from already-loaded state. Projection shim over the
/// CBMC-verified [`rio_evidence_kernel::pull::admit_pull`] (decision
/// P10 — the decision was kept pure from its introduction precisely so
/// it could be lifted into a kani kernel without refactoring; the
/// closure-evidence campaign's Phase 2 did the lift): this function
/// maps the scheduler vocabulary (`DerivationStatus`, `ExecutorId`,
/// `Uuid`) onto the kernel's mirrored alphabet and returns the kernel's
/// decision unchanged.
///
/// Kani-coverage arc (relocated from the retired executor invariant
/// map): the executor campaign's close-out evaluated and DECLINED an
/// executor-side harness for `admit_pull`/`fold_report` — the charging
/// arithmetic already lived in the proven retry kernel, `fold_report`
/// is a two-boolean truth table fronting durable guards no Rust-level
/// proof reaches, `admit_pull` was a single total match whose input
/// partition the unit tables enumerate, and hosting contracts inside
/// rio-scheduler had already failed to converge in the gate budget.
/// Its recorded reconsideration triggers: kernels growing
/// loops/collections/counter arithmetic, an extraction into a
/// dependency-light crate, or a changed floor-comparison shape. The
/// extraction trigger FIRED: the closure-evidence campaign's Phase 2
/// lifted the kernels into rio-evidence-kernel, and the pull-admission
/// proofs now run in the gated kani set (nix/kani.nix, the
/// rio-evidence-kernel pull-admission harnesses) — the omission is
/// discharged, not standing.
///
/// Check order (proven in the kernel): identity first (a mis-bound
/// token never learns anything about the drv), then the generation
/// fence (a deposed believer answers nothing), then
/// wantedness/deliverability — including the advisory generation-fence
/// half (`r[sched.lease.generation-fence+3]`) at the kernel's marked
/// arms.
/// The fence identity of one pull, decided exactly once at the entry
/// of the decision path (merged_bug_145 fail-closed hoist): `Option`
/// never reaches the fence enforcement sites — a build-lane pull
/// either carries its token hash or was already refused above.
#[derive(Clone, Copy)]
enum FenceLane<'a> {
    /// Build lane with a pod credential: the confirm-exit fence
    /// governs this pull.
    Keyed(&'a str),
    /// A lane with no fence key domain, carrying its [`NonKeyedLane`]
    /// proof (minted ONLY at the lane-classification match in
    /// [`DagActor::pull_assignment_inner`] — the single site where
    /// `FenceLane::Keyed` is structurally excluded).
    NonKeyed(NonKeyedLane),
}

/// Proof that one pull runs on a lane with no fence key domain
/// (merged_bug_011 / FS-5). The class is private and there is no pub
/// constructor: the ONLY minting site is the
/// `(kind, executor_token_sha256)` lane-classification match in
/// [`DagActor::pull_assignment_inner`] — the match whose `Keyed` arm
/// is structurally excluded from producing this value. A value of
/// this type therefore IS the proof that the pull was classified
/// non-keyed; [`GoneLicense::Unfenced`] consumes it, so an unfenced
/// keyed `Gone` is a compile error, not a runtime check (the
/// `debug_assert` form was REJECTED per FS-5 — it compiles out of
/// release builds and is not a typecheck).
///
/// The two classes:
///
/// - `Unfenced` — build lane in an IDENTITY-DISABLED deployment (no
///   HMAC key configured anywhere — dev mode, the standalone/keyless
///   VM fixtures): there is no pod credential in the system, so the
///   fence has no key domain. The m145 threat model is the k8s Job
///   lifecycle (a Succeeded Job invisible to the establishment
///   sweep); an identity-disabled deployment opted out of executor
///   identity entirely. Production cannot reach this lane: with a
///   key configured the credential layer rejects token-less pulls
///   Unauthenticated before the actor, and the gRPC dispatch carries
///   a second fail-closed arm behind it (defense in depth). First
///   enforced as an unconditional refusal — the 13-VM-check red
///   proved the lane is a deployment CLASS, not dead code.
/// - `Materialization` — no confirm-exit protocol, no fence (the
///   (intent, instance) composite + the kernel's one-winner arm
///   arbitrate replica identity instead).
#[derive(Debug, Clone, Copy)]
pub struct NonKeyedLane(NonKeyedLaneClass);

/// Which non-keyed lane a [`NonKeyedLane`] proof attests (private —
/// see the wrapper's doc; consumers project through
/// [`NonKeyedLane::kernel_kind`]).
#[derive(Debug, Clone, Copy)]
enum NonKeyedLaneClass {
    /// Identity-disabled deployment class (see the wrapper doc).
    Unfenced,
    /// Materialization lane (see the wrapper doc).
    Materialization,
}

impl NonKeyedLane {
    /// Project onto the kernel's lane alphabet for the
    /// [`rio_evidence_kernel::pull::fence_obligation`] law.
    fn kernel_kind(self) -> rio_evidence_kernel::pull::FenceLaneKind {
        match self.0 {
            NonKeyedLaneClass::Unfenced => rio_evidence_kernel::pull::FenceLaneKind::Unfenced,
            NonKeyedLaneClass::Materialization => {
                rio_evidence_kernel::pull::FenceLaneKind::Materialization
            }
        }
    }
}

/// live061-R5 — the pull-answer DEBUG flood limiter. The leader
/// answers every fleet claim attempt; during live_061 the two
/// per-answer debug lines ran at ~260 lines/s (21,337 refusals/78s),
/// rolling the scheduler pod's kubelet log retention (10MB x 5 files)
/// down to ~60-90s — the incident's ONSET evidence was gone before
/// any responder looked, and the forensic census had to be rebuilt
/// from store-side logs. The limiter bounds each answer arm to
/// [`Self::MAX_PER_WINDOW`] lines per [`Self::WINDOW_SECS`] window
/// (<=2.2 lines/s/arm worst-case vs ~260/s) and discloses the
/// suppressed count when a window rolls — counting is total (the
/// `rio_store_materialization_claim_answers_total` fleet counter and
/// the per-window suppressed disclosure carry the volume; the log
/// lane carries bounded samples). Window state is two atomics per
/// arm: lock-free, monotonic-coarse (a racing roll double-discloses
/// at worst, never under-counts).
struct AnswerLogLimiter {
    /// Epoch-seconds of the current window's start.
    window_start: std::sync::atomic::AtomicU64,
    /// Lines emitted in the current window.
    emitted: std::sync::atomic::AtomicU64,
    /// Lines suppressed in the current window.
    suppressed: std::sync::atomic::AtomicU64,
}

impl AnswerLogLimiter {
    const MAX_PER_WINDOW: u64 = 20;
    const WINDOW_SECS: u64 = 10;

    const fn new() -> Self {
        Self {
            window_start: std::sync::atomic::AtomicU64::new(0),
            emitted: std::sync::atomic::AtomicU64::new(0),
            suppressed: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Whether this line may emit; returns the rolled window's
    /// suppressed count (Some on the first emit after a roll, so the
    /// caller can disclose it) alongside the verdict.
    fn admit(&self) -> (bool, Option<u64>) {
        use std::sync::atomic::Ordering;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let start = self.window_start.load(Ordering::Relaxed);
        let mut rolled = None;
        if now.saturating_sub(start) >= Self::WINDOW_SECS
            && self
                .window_start
                .compare_exchange(start, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let dropped = self.suppressed.swap(0, Ordering::Relaxed);
            self.emitted.store(0, Ordering::Relaxed);
            if dropped > 0 {
                rolled = Some(dropped);
            }
        }
        if self.emitted.fetch_add(1, Ordering::Relaxed) < Self::MAX_PER_WINDOW {
            (true, rolled)
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            (false, rolled)
        }
    }
}

static GONE_ANSWER_LOG: AnswerLogLimiter = AnswerLogLimiter::new();
static NYR_ANSWER_LOG: AnswerLogLimiter = AnswerLogLimiter::new();

pub(crate) fn admit_pull(inputs: &PullInputs<'_>) -> PullDecision {
    rio_evidence_kernel::pull::admit_pull(
        rio_evidence_kernel::pull::PullRequest {
            intent_id: inputs.intent_id,
            auth_intent: inputs.auth_intent,
            serving_generation: inputs.serving_generation,
            generation_floor: inputs.generation_floor,
            status: inputs.status.map(pull_node_status),
            open_attempt: inputs.open_attempt,
            pulling_identity: inputs.pulling_identity,
            build_backoff_expired: inputs.build_backoff_expired,
        },
        rio_evidence_kernel::pull::MaterializationInputs {
            kind: inputs.pull_kind,
            job: inputs.job_view,
            resume_exec_id: inputs.resume_exec_id,
            // The kernel's credential scalar is the UUID's u128 form
            // (type-distinct from ExecId, so a nonce can never be
            // compared against an exec id by accident).
            presented_nonce: inputs.presented_nonce.map(|n| n.as_u128()),
            attempt_nonce: inputs.attempt_nonce.map(|n| n.as_u128()),
        },
    )
}

/// sh-045 r1: per-field max-merge of one heartbeat into the cache.
/// `wall_seconds`, `peak_memory_bytes`, `cpu_seconds_total`,
/// `cpu_throttled_usec`, `peak_disk_bytes` are all builder-monotone
/// across one attempt; max-merging makes RPC ordering irrelevant (a
/// stale ticker RPC landing after the final ship cannot regress them).
fn merge_heartbeat_peaks(
    slot: &mut Option<(f64, u64, rio_proto::types::ResourceUsage)>,
    wall_seconds: f64,
    peak_memory_bytes: u64,
    mut resources: rio_proto::types::ResourceUsage,
) {
    if let Some((w, m, prev)) = slot.as_ref() {
        resources.cpu_seconds_total = match (resources.cpu_seconds_total, prev.cpu_seconds_total) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        resources.cpu_throttled_usec = resources.cpu_throttled_usec.max(prev.cpu_throttled_usec);
        resources.peak_disk_bytes = resources.peak_disk_bytes.max(prev.peak_disk_bytes);
        *slot = Some((wall_seconds.max(*w), peak_memory_bytes.max(*m), resources));
    } else {
        *slot = Some((wall_seconds, peak_memory_bytes, resources));
    }
}

/// `DerivationStatus` → kernel [`PullNodeStatus`]. The exhaustive
/// `match` (no wildcard arm) pins the alphabets in lockstep: adding a
/// scheduler variant the kernel lacks fails this compile.
///
/// [`PullNodeStatus`]: rio_evidence_kernel::pull::PullNodeStatus
fn pull_node_status(status: DerivationStatus) -> rio_evidence_kernel::pull::PullNodeStatus {
    use rio_evidence_kernel::pull::PullNodeStatus as K;
    match status {
        DerivationStatus::Created => K::Created,
        DerivationStatus::Queued => K::Queued,
        DerivationStatus::Ready => K::Ready,
        DerivationStatus::Assigned => K::Assigned,
        DerivationStatus::Running => K::Running,
        DerivationStatus::Completed => K::Completed,
        DerivationStatus::Failed => K::Failed,
        DerivationStatus::Poisoned => K::Poisoned,
        DerivationStatus::DependencyFailed => K::DependencyFailed,
        DerivationStatus::Cancelled => K::Cancelled,
        DerivationStatus::Skipped => K::Skipped,
    }
}

impl DagActor {
    /// Handle one `PullAssignment` (the actor turn). Computes the
    /// admission via [`admit_pull`], executes the decision, and replies.
    #[allow(clippy::too_many_arguments)]
    // The pull's wire surface is the argument list (one field per
    // PullAssignmentRequest input); bundling into a struct would just
    // restate the proto. Grew to 8 with confirm_only (merged_bug_083),
    // 9 with the confirm-fence token hash (merged_bug_145).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_pull_assignment(
        &mut self,
        intent_id: String,
        auth_intent: Option<String>,
        kind: rio_evidence_kernel::pull::PullKind,
        executor_instance: Option<String>,
        resume_exec_id: Option<Uuid>,
        claim_nonce: Option<Uuid>,
        confirm_only: bool,
        executor_token_sha256: Option<String>,
        reply: oneshot::Sender<Result<PullOutcome, PullRejection>>,
    ) {
        let result = self
            .pull_assignment_inner(
                &intent_id,
                auth_intent.as_deref(),
                kind,
                executor_instance.as_deref(),
                resume_exec_id,
                claim_nonce,
                confirm_only,
                executor_token_sha256.as_deref(),
            )
            .await;
        let _ = reply.send(result);
    }

    // r[impl sched.executor.pull-transaction+2]
    #[allow(clippy::too_many_arguments)] // mirrors handle_pull_assignment's wire surface
    async fn pull_assignment_inner(
        &mut self,
        intent_id: &str,
        auth_intent: Option<&str>,
        kind: rio_evidence_kernel::pull::PullKind,
        executor_instance: Option<&str>,
        resume_exec_id: Option<Uuid>,
        claim_nonce: Option<Uuid>,
        confirm_only: bool,
        executor_token_sha256: Option<&str>,
    ) -> Result<PullOutcome, PullRejection> {
        // merged_bug_145 fail-closed hoist (found by automated review
        // of the chain): token ABSENCE is decided exactly ONCE, here —
        // below this point the fence logic consumes a typed lane with
        // no `None` to reach, so a future caller cannot reintroduce
        // the bypass by guard omission. The fail-closed boundary for
        // KEY-CONFIGURED deployments lives where the configuration is
        // known: the credential layer rejects token-less build pulls
        // Unauthenticated (grpc/mod.rs require_executor), and the
        // gRPC dispatch carries a second rejection arm behind it —
        // `None` reaching this actor therefore MEANS the
        // identity-disabled deployment class (see FenceLane::Unfenced;
        // the unconditional-refusal first cut turned every keyless VM
        // scenario red — 13 checks — which is the machine evidence
        // this lane is a deployment class, not dead code).
        // THE lane-classification match (merged_bug_011 / FS-5): the
        // single site minting `NonKeyedLane` proofs — the `Keyed` arm
        // structurally cannot produce one, so an unfenced keyed Gone
        // cannot be assembled anywhere downstream.
        let fence_lane = match (kind, executor_token_sha256) {
            // Materialization lane: replica identity is the
            // (intent, instance) composite under fleet-level service
            // credentials; the confirm-EXIT protocol (and so the
            // fence) does not exist here — only the builder's keyed
            // build lane bears it. The store's client DOES send
            // `confirm_only` on this lane (bug_060 census correction):
            // the only call sites passing `confirm_only: true` are the store's resume PROBE lane and the builder's confirm-exit — census[test: materialization_probe_is_screened_not_minted] census[test: delivered_resume_does_not_strand_charged_sibling]
            // (resume presentations past full slots probe with
            // `confirm_only: probing`, merged_bug_014's standing
            // oracle; the mint path passes false), and the kind-blind
            // confirm screen below is LOAD-BEARING for exactly that
            // shape — it converts a probe's would-be-DeliverNew to
            // NotYetReady so no mint can occur. Absent on this lane:
            // the fence. Present and required: the screen.
            (rio_evidence_kernel::pull::PullKind::Materialization, _) => {
                FenceLane::NonKeyed(NonKeyedLane(NonKeyedLaneClass::Materialization))
            }
            (rio_evidence_kernel::pull::PullKind::Build, Some(hash)) => FenceLane::Keyed(hash),
            (rio_evidence_kernel::pull::PullKind::Build, None) => {
                FenceLane::NonKeyed(NonKeyedLane(NonKeyedLaneClass::Unfenced))
            }
        };
        // Standby replicas answer nothing (the gRPC layer already
        // gates; this closes the in-flight-deposed window).
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        let serving_generation = self.serving_generation;
        // sh-007 row 1: the per-pull floor read is the cached field —
        // refreshed at LeaderAcquired and at every Tick head — instead
        // of a per-pull claims-floor PG round-trip (iter1: 149k reads,
        // 41% of actor busy time). The cached
        // value is at most one tick interval stale; the PG-side fence
        // at `mint_pull_attempt_fenced` is the hard gate, so staleness
        // here delays the advisory self-reject by at most one tick and
        // admits no stale write.
        let generation_floor = self.generation_floor_cached;

        let drv_hash = DrvHash::from(intent_id);
        // The attempt's executor identity (substitution-replacement
        // BC-1): build pulls bind to the attested intent itself,
        // exactly as-built (the request carries no pod name the token
        // could attest). Materialization pulls bind to the
        // `(intent, replica)` pair — distinct per store replica, so the
        // kernel's open-attempt arm (same-identity re-delivery,
        // different-identity NotYetReady) is the one-winner arbiter.
        // The kind is NEVER parsed back out of this string: it is
        // carried by the request and persisted as
        // `drv_executions.attempt_kind`.
        let pulling_identity = match (kind, executor_instance) {
            (rio_evidence_kernel::pull::PullKind::Materialization, Some(instance))
                if !instance.is_empty() =>
            {
                ExecutorId::from(format!("{intent_id}@{instance}"))
            }
            // Build pulls: the attested intent, exactly as-built.
            _ => ExecutorId::from(intent_id),
        };

        let (status, open_attempt, attempt_nonce, build_backoff_expired) =
            match self.dag.node(&drv_hash) {
                None => (None, None, None, true),
                Some(state) => (
                    Some(state.status()),
                    state
                        .assigned_executor
                        .as_ref()
                        .zip(state.exec_id)
                        .map(|(executor, exec_id)| (executor.clone(), exec_id)),
                    // The open attempt's persisted claim nonce (rule-4b):
                    // meaningful only alongside exec_id (set at mint,
                    // cleared in lockstep, recovered off the same
                    // assignments join).
                    state.claim_nonce,
                    // bug_282: the transient-retry backoff produced by
                    // handle_transient_failure, finally consumed — the
                    // kernel's (Build, None) arm holds the fresh mint
                    // until the window lapses.
                    state
                        .retry
                        .backoff_until
                        .is_none_or(|t| t <= std::time::Instant::now()),
                ),
            };
        let decision = admit_pull(&PullInputs {
            intent_id,
            auth_intent,
            status,
            open_attempt: open_attempt.as_ref().map(|(e, x)| (e, *x)),
            pulling_identity: &pulling_identity,
            serving_generation: serving_generation.to_kernel_u64(),
            generation_floor,
            pull_kind: kind,
            build_backoff_expired,
            // The job view: projected from the actor's in-memory job map
            // (Unavailable projects Pending{parked:true} — fail-closed).
            job_view: self.materialization_job_view(&drv_hash, &pulling_identity),
            resume_exec_id,
            presented_nonce: claim_nonce,
            attempt_nonce,
        });

        // merged_bug_246, the Gone half of the fail-closed posture: a
        // term with no trustworthy job view must never tell a
        // materialization claimant `Gone` — the store treats Gone as
        // "job resolved, skip" and NEVER claims again, stranding any
        // real durable job behind a degraded term. The kernel's Gone
        // here comes from the base table's unknown/terminal-node arm
        // (a degraded term's DAG is empty, so every status reads
        // None); token/fence rejections passed through above it and
        // are unaffected. Build pulls keep base semantics (an empty
        // DAG answers Gone to builders exactly as before — the
        // job-view hole for builds is the DeliverNew cell, closed by
        // the parked projection).
        let decision = if matches!(decision, PullDecision::Gone)
            && kind == rio_evidence_kernel::pull::PullKind::Materialization
            && self.materialization_jobs.hydrated().is_none()
        {
            debug!(
                intent_id = %intent_id,
                "materialization claim answered NotYetReady: job view unavailable (degraded term — never Gone for a job we cannot see)"
            );
            PullDecision::NotYetReady
        } else {
            decision
        };

        // merged_bug_083 (the confirm screen): a confirm-only pull is a
        // READ of this puller's holdings — DeliverNew (the only
        // minting admission) is screened to NotYetReady BEFORE the
        // decision is consumed, so the mint arm below is structurally
        // unreachable on a confirm probe. Exhaustive match: a future
        // PullDecision variant forces a screening decision here.
        let decision = if confirm_only {
            match decision {
                PullDecision::DeliverNew => {
                    debug!(intent_id = %intent_id,
                           "confirm-only pull screened a DeliverNew admission to NotYetReady");
                    PullDecision::NotYetReady
                }
                d @ (PullDecision::RejectToken
                | PullDecision::RejectStaleGeneration
                | PullDecision::Gone
                | PullDecision::NotYetReady
                | PullDecision::DeliverExisting { .. }) => d,
            }
        } else {
            decision
        };

        // r[impl sched.executor.confirm-fence]
        // merged_bug_145 + merged_bug_011: the confirm-exit fence's
        // WRITE-AHEAD half, now TOTAL over the kernel's
        // which-pulls-must-fence law instead of gated on
        // `confirm_only`. An answer that licenses the builder's
        // exit 0 — EVERY keyed Gone (live or confirm; the pre-fix gate
        // left the live-loop Gone unfenced, so a straggler pull could
        // mint after a content-addressed resubmit re-readied the drv)
        // and the confirm-only NotYetReady — must have the fence row
        // durable BEFORE the reply, or a late abandoned pull (still in
        // the mailbox/network) mints an open attempt against a
        // Succeeded Job — invisible to the establishment sweep, which
        // reaps against FAILED pods. Fail-closed: if the fence write
        // fails, the license is NOT issued (retryable; the builder
        // retries or exits nonzero → Failed → the sweep reaps).
        let obligation = match fence_lane {
            FenceLane::Keyed(_) => rio_evidence_kernel::pull::fence_obligation(
                &decision,
                confirm_only,
                rio_evidence_kernel::pull::FenceLaneKind::Keyed,
            ),
            FenceLane::NonKeyed(lane) => rio_evidence_kernel::pull::fence_obligation(
                &decision,
                confirm_only,
                lane.kernel_kind(),
            ),
        };
        let fence_witness: Option<crate::db::confirm_fences::ConfirmFenceDurable> =
            match (obligation, fence_lane) {
                (
                    rio_evidence_kernel::pull::FenceObligation::WriteAhead,
                    FenceLane::Keyed(hash),
                ) => {
                    #[cfg(test)]
                    if std::mem::take(&mut self.bump_claims_floor_before_fence_write) {
                        // r13-allow(injection): the bug_015 TOCTOU
                        // window made deterministic — a successor
                        // stamps a higher claim AFTER this handler's
                        // floor read, BEFORE the fence write (the
                        // `fail_next_*` hook family's lane).
                        self.db
                            .claim_generation(
                                self.serving_generation.as_i64() + 1,
                                "test-interloper",
                            )
                            .await
                            .expect("test interloper claim");
                    }
                    match self
                        .db
                        .insert_confirm_fence(hash, intent_id, serving_generation)
                        .await
                    {
                        Ok(crate::db::confirm_fences::ConfirmFenceWrite::Durable(witness)) => {
                            Some(witness)
                        }
                        // bug_015 (SIGNED Q2): the write transaction's
                        // OWN floor check refused — this replica is
                        // below the durable claims floor. The precise
                        // truth is the SAME rejection the kernel's
                        // floor check mints; uncounted leader-churn
                        // class at the gRPC table. Nothing was
                        // written; the license is withheld.
                        Ok(crate::db::confirm_fences::ConfirmFenceWrite::Fenced { floor }) => {
                            warn!(intent_id = %intent_id,
                                  serving_generation = serving_generation.as_i64(),
                                  floor,
                                  "fence write refused below the durable claims floor; \
                                   withholding the exit-0 license");
                            return Err(PullRejection::StaleGeneration);
                        }
                        Err(e) => {
                            warn!(intent_id = %intent_id, error = %e,
                                  "fence write-ahead failed; withholding the exit-0 license");
                            // Retryable class: the builder re-pulls
                            // (live loop / Idle confirm) or exits
                            // nonzero (Shutdown) — the pod never exits
                            // 0 on an unfenced licensing answer.
                            return Err(PullRejection::ConsumptionNotDurable);
                        }
                    }
                }
                // Kernel law (kani-pinned): WriteAhead is emitted only
                // on the keyed lane — this cell is structurally dead;
                // total and harmless (writes nothing, licenses
                // nothing: the Gone arm below licenses non-keyed lanes
                // through their own proof, never through a witness).
                (
                    rio_evidence_kernel::pull::FenceObligation::WriteAhead,
                    FenceLane::NonKeyed(_),
                ) => None,
                // ScreenRead is consumed inside the DeliverNew arm
                // (the read needs to interleave with the mint); None
                // obliges nothing.
                (
                    rio_evidence_kernel::pull::FenceObligation::ScreenRead
                    | rio_evidence_kernel::pull::FenceObligation::None,
                    _,
                ) => None,
            };

        match decision {
            PullDecision::RejectToken => {
                // r[impl sec.executor.identity-token+3]
                warn!(
                    intent_id = %intent_id,
                    "pull rejected: executor token bound to a different intent"
                );
                Err(PullRejection::TokenMismatch)
            }
            PullDecision::RejectStaleGeneration => {
                info!(
                    intent_id = %intent_id,
                    serving_generation = serving_generation.as_i64(),
                    ?generation_floor,
                    "pull rejected: serving generation below the durable claims floor"
                );
                Err(PullRejection::StaleGeneration)
            }
            PullDecision::Gone => {
                // r[impl sched.executor.pull-gone+1]
                // The license: keyed lanes spend the write-ahead
                // witness (the fence IS durable — the block above
                // returned early otherwise); non-keyed lanes spend
                // their classification proof. A keyed Gone with no
                // witness is structurally dead by the kernel law
                // (Gone on Keyed ⇒ WriteAhead, kani-pinned) — the arm
                // refuses fail-closed rather than license unfenced.
                let license = match (fence_lane, fence_witness) {
                    (FenceLane::NonKeyed(lane), _) => GoneLicense::Unfenced(lane),
                    (FenceLane::Keyed(_), Some(witness)) => GoneLicense::Fenced(witness),
                    (FenceLane::Keyed(_), None) => {
                        return Err(PullRejection::Internal(
                            "keyed Gone reached the answer with no fence witness \
                             (fence_obligation law violated)"
                                .into(),
                        ));
                    }
                };
                let (emit, rolled) = GONE_ANSWER_LOG.admit();
                if let Some(suppressed) = rolled {
                    debug!(
                        suppressed,
                        "pull-answered-Gone lines suppressed in the last window"
                    );
                }
                if emit {
                    debug!(intent_id = %intent_id, ?status, "pull answered Gone");
                }
                Ok(PullOutcome::Gone(license))
            }
            PullDecision::NotYetReady => {
                let (emit, rolled) = NYR_ANSWER_LOG.admit();
                if let Some(suppressed) = rolled {
                    debug!(
                        suppressed,
                        "pull-answered-NotYetReady lines suppressed in the last window"
                    );
                }
                if emit {
                    debug!(intent_id = %intent_id, ?status, "pull answered NotYetReady");
                }
                Ok(PullOutcome::NotYetReady {
                    retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
                })
            }
            PullDecision::DeliverExisting { exec_id } => {
                // Idempotent re-pull: read, never write. The payload is
                // rebuilt from the same inputs (drv, identity), so the
                // load-bearing fields — drv path, ATerm, exec_id,
                // resources — are identical.
                debug_assert_eq!(
                    self.dag.node(&drv_hash).and_then(|s| s.exec_id),
                    Some(exec_id)
                );
                let assignment = self
                    .build_assignment_proto(&drv_hash, &pulling_identity, kind)
                    .await
                    .ok_or_else(|| {
                        PullRejection::Internal("derivation vanished during re-pull".into())
                    })?;
                Ok(PullOutcome::Deliver(Box::new(assignment)))
            }
            PullDecision::DeliverNew => {
                // r[impl sched.executor.confirm-fence]
                // merged_bug_145: the confirm-exit fence's READ half —
                // the kernel law's ScreenRead obligation (DeliverNew
                // on the keyed lane; the `if let Keyed` IS that
                // conjunction). A fenced token already received its
                // exit-0 license — the pod is gone (or exiting);
                // minting would open an attempt no sweep can see.
                // Screen to Gone (terminal for any straggler loop),
                // licensed by the witness the read returned.
                // Fail-closed on a read error: refusing a mint costs
                // one NotYetReady retry; a false mint costs an
                // invisible open attempt — and the mint transaction
                // needs PG anyway.
                if let FenceLane::Keyed(hash) = fence_lane {
                    // Q2 scope: the screen READ stays unfenced by
                    // design (which ANSWERS are fenced, not reads) —
                    // the row it observes has transitively fenced
                    // provenance (only a fenced write creates one).
                    match self.db.confirm_fence_exists(hash).await {
                        Ok(Some(witness)) => {
                            info!(intent_id = %intent_id,
                                  "DeliverNew screened to Gone: executor token is confirm-fenced");
                            return Ok(PullOutcome::Gone(GoneLicense::Fenced(witness)));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            warn!(intent_id = %intent_id, error = %e,
                                  "confirm fence read failed; withholding the mint");
                            return Ok(PullOutcome::NotYetReady {
                                retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
                            });
                        }
                    }
                }
                self.mint_and_deliver(
                    &drv_hash,
                    &pulling_identity,
                    serving_generation,
                    kind,
                    claim_nonce,
                )
                .await
            }
        }
    }

    /// The one pull transaction: mint `exec_id`, write the fenced
    /// `assignments` + `drv_executions` rows, transition the
    /// derivation out of Ready, pin GC live-inputs, and build the
    /// payload. The durable half commits only at-or-above the claims
    /// floor; on a fence abort nothing is written and nothing in
    /// memory changes.
    async fn mint_and_deliver(
        &mut self,
        drv_hash: &DrvHash,
        pulling_identity: &ExecutorId,
        serving_generation: crate::db::ServingGeneration,
        kind: rio_evidence_kernel::pull::PullKind,
        claim_nonce: Option<Uuid>,
    ) -> Result<PullOutcome, PullRejection> {
        let Some(db_id) = self.dag.node(drv_hash).and_then(|s| s.db_id) else {
            // Merged but not yet persisted — deliverable on a later
            // pull once the merge commit lands.
            return Ok(PullOutcome::NotYetReady {
                retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
            });
        };
        let exec_id = Uuid::now_v7();
        let log_hash = self
            .dag
            .path_for_hash(drv_hash)
            .map(rio_nix::store_path::drv_log_hash)
            .unwrap_or_default();
        // live_040 + bug_027: THE mint-time dispatch shape, derived
        // ONCE and consumed by BOTH siblings — the persisted
        // establishment anchor and the last_intent stamp after the
        // durable mint commits. The mint IS the dispatch decision in
        // the pull architecture; `DispatchShape::reconcile` lifts the
        // solve's deadline to max(resolved, carried) because the pod
        // runs under the CARRIED rendered shape (bug_106) — so the D4
        // floor doubling base, §13b running-dep ETA, SLA misprediction
        // scoring, cpu_limit_cores min, and the delivered assignment's
        // sizing triple all read what was actually dispatched. The
        // kube-authoritative binding row is read ONCE here (bug_027:
        // the node and the carried deadline were two ad-hoc lookups of
        // one fact). Build lane only: a materialization claim runs
        // under its own deadline and sizes nothing.
        let bound_node = self
            .authoritative_binding
            .get(drv_hash)
            .map(|b| b.node.clone());
        let dispatch: DispatchShape = match kind {
            rio_evidence_kernel::pull::PullKind::Build => {
                let carried_deadline = self
                    .authoritative_binding
                    .get(drv_hash)
                    .and_then(|b| b.deadline_secs);
                let (hw, cost, inputs_gen) = self.solve_inputs();
                DispatchShape::reconcile(
                    self.dag
                        .node(drv_hash)
                        .map(|state| self.solve_intent_for(state, &hw, &cost, inputs_gen)),
                    carried_deadline,
                )
            }
            rio_evidence_kernel::pull::PullKind::Materialization => DispatchShape::Unsized,
        };
        // THE one kind match (A2.3): every kind-divergent decision of
        // this mint is selected here; the body below consumes profile
        // fields only. The work class is keyed on the request's claimed
        // kind, never derived from an identity prefix; build pulls
        // write 'build' (value-identical to the as-built rows).
        let profile = match kind {
            rio_evidence_kernel::pull::PullKind::Build => MintProfile {
                attempt_kind: crate::state::AttemptKind::Build,
                // Controller-authoritative pod→node binding, when
                // known (projected from the SAME binding read as the
                // carried deadline above — one fact, one lookup).
                source_node: bound_node,
                // The deadline this attempt is dispatched under,
                // persisted so the establishment window is anchored to
                // it and can never shrink below it while the attempt
                // is open. bug_027: this is `dispatch`'s OWN deadline
                // — `DispatchShape::reconcile` already lifted the
                // solve to max(resolved, carried) (the bug_106 law),
                // and the SAME reconciled value is stamped onto
                // `last_intent` once the durable mint commits, so the
                // anchor and the doubling base can no longer drift
                // (pre-fix the stamp wrote the raw solve while the
                // anchor lifted — a refit-down DeadlineExceeded then
                // doubled from the smaller mint value into an
                // uncounted futile retry at the limit that provably
                // failed). The debug.rs operator seed remains a second
                // writer with documented precedence — the mint
                // overwrites it on the next pull, the intended
                // freshness law.
                deadline_secs: dispatch.anchor_deadline_secs(),
                clear_ice: true,
                pin_live_inputs: true,
                note_claimed: false,
                surface: rio_evidence_kernel::pull::display_class(kind),
            },
            rio_evidence_kernel::pull::PullKind::Materialization => MintProfile {
                attempt_kind: crate::state::AttemptKind::Materialization,
                source_node: None,
                deadline_secs: Some(self.materialization_cfg.attempt_deadline_secs as f64),
                clear_ice: false,
                pin_live_inputs: false,
                note_claimed: true,
                surface: rio_evidence_kernel::pull::display_class(kind),
            },
        };

        // 249 rider (mint backstop): never deliver onto a node the
        // derivation has itself excluded. The binding is the OLD
        // Pending pod's (stamped before the failure that excluded the
        // node); the controller's drift reap replaces that Job next
        // tick — the pod polling from it idle-exits on NotYetReady.
        // r[impl sched.dispatch.fleet-exhaust+5]
        if let Some(node) = profile.source_node.as_deref()
            && self
                .dag
                .node(drv_hash)
                .is_some_and(|s| s.excluded_source_nodes().iter().any(|n| n == node))
        {
            debug!(
                drv_hash = %drv_hash,
                node,
                "pull refused: the bound node is in the derivation's exclusion set \
                 (stale Pending pod; the drift reap replaces it)"
            );
            return Ok(PullOutcome::NotYetReady {
                retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
            });
        }
        let minted = self
            .db
            .mint_pull_attempt_fenced(
                db_id,
                pulling_identity,
                serving_generation,
                exec_id,
                &log_hash,
                profile.source_node.as_deref(),
                profile.deadline_secs,
                profile.attempt_kind,
                // rule-4b: persist the presented claim nonce with the
                // assignment — the lost-response credential. None for
                // build pulls and nonceless (old-store) claims.
                claim_nonce,
            )
            .await
            .map_err(|e| PullRejection::Internal(format!("pull mint transaction failed: {e}")))?;
        if !minted.settled() {
            info!(
                drv_hash = %drv_hash,
                serving_generation = serving_generation.as_i64(),
                "pull mint aborted by the generation fence; no row written"
            );
            return Err(PullRejection::StaleGeneration);
        }

        // Substitution-replacement: a committed materialization mint is
        // the claim — note it in the in-memory job view so the kernel's
        // one-winner arbitration (Claimed{held_by_puller}) answers
        // re-pulls and competing claims correctly. Reachable only
        // flag-on (no materialization pull is delivered flag-off).
        if profile.note_claimed {
            self.note_materialization_claimed(drv_hash, pulling_identity);
        }

        // r[impl sched.sla.hw-class.ice-mask]
        // Mechanism #22's clear half, pull-mode trigger: the first
        // successful pull is the success edge for the pull path — the
        // pod is scheduled and has taken the work — exactly what the
        // stream path's registration edge signals. Same |A'| = 1
        // discipline as that edge (the pod's affinity is OR-of-A', so a
        // multi-cell intent identifies no single cell; over-clearing
        // would defeat `ice_step_doubles`); `registered_cells` (A18)
        // remains the per-cell signal. NotYetReady / Gone / rejected
        // pulls never reach here, so they never clear. Arming
        // (`AckSpawnedIntents`) and the DAG-state sweep are untouched.
        // Build-profile only (bug_091): a materialization claim says
        // nothing about builder-pod scheduling — it must neither clear
        // the mask nor consume the arming.
        if profile.clear_ice
            && let Some((_, cells)) = self.dispatched_cells.remove(drv_hash.as_str())
            && let [cell] = cells.as_slice()
        {
            self.ice.clear(cell);
        }

        // Durable mint committed — now the in-memory bookkeeping, the
        // same shape the stream path's record phase keeps (transition,
        // exec_id, assigned executor, status persist, GC pins).
        // r[impl sched.state.machine+2]
        // The transition uses the KINDED validation (PD-6): build mints
        // take the as-built Ready→Assigned edge byte-identically;
        // materialization mints may additionally take Queued→Assigned
        // (the kernel's Queued admission and this edge are two halves of
        // one decision — a rejection here for an admitted claim would
        // re-open the PDQ-6 stranded-mint window).
        if let Some(state) = self.dag.node_mut(drv_hash) {
            if let Err(e) =
                state.transition_for_mint(DerivationStatus::Assigned, profile.attempt_kind)
            {
                // TOCTOU vs a concurrent cancel between the admit and
                // the commit: the durable rows exist but the node left
                // Ready/Queued. Never deliver. The committed mint is
                // NOT stranded into a charged settlement
                // (merged_bug_096 recovery contract, both branches):
                //  - job still live: the client KEEPS its nonce on
                //    this NotYetReady (the credential survives), the
                //    job view already says Claimed-by-this-puller
                //    (note_claimed ran above), so the next resume
                //    pull re-delivers through the Claimed arm's
                //    credential disjunction;
                //  - job cancelled (the race that rejected the
                //    transition): the flag-gated housekeeping closer
                //    sweeps the cancelled job's open attempt
                //    CHARGE-FREE from a PG snapshot taken after this
                //    commit, and the client's next resume answers
                //    Gone, dropping the entry cleanly.
                warn!(drv_hash = %drv_hash, error = %e,
                      "pull minted but the mint transition was rejected; answering NotYetReady \
                       (credential kept client-side; cancelled-job mints settle charge-free \
                       via the housekeeping closer)");
                return Ok(PullOutcome::NotYetReady {
                    retry_after_secs: NOT_YET_READY_RETRY_AFTER_SECS,
                });
            }
            state.retry.backoff_until = None;
            state.assigned_executor = Some(pulling_identity.clone());
            state.exec_id = Some(exec_id);
            // live_040 + bug_027: stamp the RECONCILED dispatch shape
            // (the same value the establishment anchor consumed, with
            // the deadline lifted to max(resolved, carried)) — BEFORE
            // build_assignment_proto reads it for the assignment's
            // sizing triple. One derivation, two consumers.
            if let Some(solve) = dispatch.stamp() {
                state.sched.last_intent = Some(solve);
            }
            // sh-045: the heartbeat cache shares lifecycle with
            // `last_intent` — cleared at the dispatch-mint restamp so
            // a stale prior-attempt's `cpu_seconds` cannot leak into a
            // new attempt's witnessed close.
            state.sched.last_reported_peaks = None;
            // rule-4b: mirror the persisted nonce (cleared in lockstep
            // with exec_id at every clear site).
            state.claim_nonce = claim_nonce;
            // r[impl sched.pull.kinded-running-surface]
            // Captured at the single mint site for BOTH work classes,
            // cleared in lockstep with exec_id (bug_144).
            state.open_attempt_kind = Some(profile.attempt_kind);
            if let Err(e) = state.transition(DerivationStatus::Running) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "pull-minted attempt could not enter Running (left Assigned)");
            }
        }
        let new_status = self
            .dag
            .node(drv_hash)
            .map(|s| s.status())
            .unwrap_or(DerivationStatus::Running);
        self.persist_status(drv_hash, new_status, Some(pulling_identity))
            .await;

        // GC live-input pins — same best-effort discipline as the
        // stream path's record phase. Materialization mints skip this:
        // build-input pins are not materialization's lifecycle (design
        // §5 — pin-at-ingest is store-side, and a materialization
        // attempt never reads build inputs).
        if profile.pin_live_inputs {
            let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
            if !input_paths.is_empty()
                && let Err(e) = self.db.pin_live_inputs(drv_hash, &input_paths).await
            {
                debug!(drv_hash = %drv_hash, error = %e,
                       "failed to pin live inputs for pull attempt (best-effort)");
            }
        }

        let assignment = self
            .build_assignment_proto(drv_hash, pulling_identity, kind)
            .await
            .ok_or_else(|| {
                PullRejection::Internal("derivation vanished while building the payload".into())
            })?;
        // Display events, per work class: build mints emit STARTED (the
        // as-built path, byte-identical); materialization mints emit
        // SUBSTITUTING (BC-4 — the wire-retained kind whose emission
        // moves from walk-spawn to claim intake; STARTED would stop the
        // gateway's actSubstitute/actCopyPath pair the instant it opened).
        match profile.surface {
            rio_evidence_kernel::pull::DisplaySurface::Build => {
                self.emit_assignment_started(drv_hash, pulling_identity);
            }
            // r[impl sched.materialize.job+2]
            rio_evidence_kernel::pull::DisplaySurface::Substitution => {
                self.emit_materialization_claimed(drv_hash);
            }
        }
        info!(
            drv_hash = %drv_hash,
            exec_id = %exec_id,
            executor_id = %pulling_identity,
            "pull-mode attempt opened"
        );
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        Ok(PullOutcome::Deliver(Box::new(assignment)))
    }
}

#[cfg(test)]
mod kernel_tests {
    use super::*;

    fn base_inputs<'a>(
        status: Option<DerivationStatus>,
        pulling: &'a ExecutorId,
        open: Option<(&'a ExecutorId, Uuid)>,
    ) -> PullInputs<'a> {
        PullInputs {
            intent_id: "drv-x",
            auth_intent: Some("drv-x"),
            status,
            open_attempt: open,
            pulling_identity: pulling,
            serving_generation: 3,
            generation_floor: Some(3),
            pull_kind: rio_evidence_kernel::pull::PullKind::Build,
            job_view: rio_evidence_kernel::pull::JobView::None,
            build_backoff_expired: true,
            resume_exec_id: None,
            presented_nonce: None,
            attempt_nonce: None,
        }
    }

    /// bug_282 kernel rows: an unexpired backoff downgrades ONLY the
    /// fresh DeliverNew; re-delivery, refusals, and rejections pass
    /// through.
    #[test]
    fn admit_pull_backoff_table() {
        let pulling = ExecutorId::from("intent-a");
        // Ready + unexpired backoff → NotYetReady (RED pre-fix:
        // DeliverNew — the produced-but-unenforced window).
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &pulling, None);
        inputs.build_backoff_expired = false;
        assert_eq!(admit_pull(&inputs), PullDecision::NotYetReady);
        // Ready + expired → DeliverNew (the normal mint).
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &pulling, None);
        inputs.build_backoff_expired = true;
        assert_eq!(admit_pull(&inputs), PullDecision::DeliverNew);
        // Re-delivery to the bound identity is untouched by the
        // backoff (the attempt already exists).
        let exec = Uuid::new_v4();
        let mut inputs = base_inputs(
            Some(DerivationStatus::Assigned),
            &pulling,
            Some((&pulling, exec)),
        );
        inputs.build_backoff_expired = false;
        assert_eq!(
            admit_pull(&inputs),
            PullDecision::DeliverExisting { exec_id: exec }
        );
        // Refusals/rejections unaffected: a queued node refuses either
        // way; a mis-bound token rejects either way.
        let mut inputs = base_inputs(Some(DerivationStatus::Queued), &pulling, None);
        inputs.build_backoff_expired = false;
        assert_eq!(admit_pull(&inputs), PullDecision::NotYetReady);
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &pulling, None);
        inputs.auth_intent = Some("other-drv");
        inputs.build_backoff_expired = false;
        assert_eq!(admit_pull(&inputs), PullDecision::RejectToken);
    }

    /// Exhaustive status → decision table for the no-open-attempt case.
    #[test]
    fn admit_pull_status_table() {
        use DerivationStatus as S;
        let me = ExecutorId::from("drv-x");
        for (status, want) in [
            (None, PullDecision::Gone),
            (Some(S::Created), PullDecision::NotYetReady),
            (Some(S::Queued), PullDecision::NotYetReady),
            (Some(S::Failed), PullDecision::NotYetReady),
            (Some(S::Ready), PullDecision::DeliverNew),
            (Some(S::Completed), PullDecision::Gone),
            (Some(S::Cancelled), PullDecision::Gone),
            (Some(S::Skipped), PullDecision::Gone),
            (Some(S::Poisoned), PullDecision::Gone),
            (Some(S::DependencyFailed), PullDecision::Gone),
        ] {
            let got = admit_pull(&base_inputs(status, &me, None));
            assert_eq!(got, want, "status {status:?}");
        }
        // Assigned/Running with no recorded open attempt: never deliver.
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_pull(&base_inputs(Some(status), &me, None)),
                PullDecision::NotYetReady,
                "in-flight without exec bookkeeping must wait"
            );
        }
    }

    /// Open-attempt identity match decides re-delivery vs wait.
    #[test]
    fn admit_pull_open_attempt_identity() {
        use DerivationStatus as S;
        let me = ExecutorId::from("drv-x");
        let other = ExecutorId::from("pool-pod-7");
        let exec = Uuid::now_v7();
        for status in [S::Assigned, S::Running] {
            assert_eq!(
                admit_pull(&base_inputs(Some(status), &me, Some((&me, exec)))),
                PullDecision::DeliverExisting { exec_id: exec },
            );
            assert_eq!(
                admit_pull(&base_inputs(Some(status), &me, Some((&other, exec)))),
                PullDecision::NotYetReady,
                "an attempt open on another executor is never re-delivered or re-pointed"
            );
        }
    }

    /// The token binding and the generation fence dominate everything.
    #[test]
    fn admit_pull_rejections_dominate() {
        let me = ExecutorId::from("drv-x");
        // Token mismatch wins even for a Ready drv.
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &me, None);
        inputs.auth_intent = Some("drv-other");
        assert_eq!(admit_pull(&inputs), PullDecision::RejectToken);
        // Below-floor serving generation answers nothing.
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &me, None);
        inputs.serving_generation = 2;
        inputs.generation_floor = Some(3);
        assert_eq!(admit_pull(&inputs), PullDecision::RejectStaleGeneration);
        // Dev mode (no token) and fresh cluster (no floor) both admit.
        let mut inputs = base_inputs(Some(DerivationStatus::Ready), &me, None);
        inputs.auth_intent = None;
        inputs.generation_floor = None;
        assert_eq!(admit_pull(&inputs), PullDecision::DeliverNew);
    }
}

// The report-admission law lives in the kernel (bug_134): BOTH kinds
// fold through `rio_evidence_kernel::pull::fold_report`, and the
// materialization consumption REQUIRES the `ProcessAdmission` witness
// it mints — a consumption call that bypassed the fold no longer
// typechecks. The kernel harness
// `check_report_admission_requires_active_assignment` sweeps the full
// 2×2 input table, so the materialization arm's old hand gate (which
// ignored `assignment_active`) cannot reappear without a red proof.
pub(crate) use rio_evidence_kernel::pull::{ReportAdmission, fold_report};

/// The report payload fields forwarded from the gRPC layer — the same
/// set the stream `ProcessCompletion` arm carries, so the intake can
/// funnel into the identical internal entry point.
#[derive(Debug)]
pub struct PullReportPayload {
    pub result: rio_proto::types::BuildResult,
    pub peak_memory_bytes: u64,
    pub peak_cpu_cores: f64,
    pub node_name: Option<String>,
    pub hw_class: Option<String>,
    pub final_resources: Option<rio_proto::types::ResourceUsage>,
    pub final_line_count: u64,
    /// Substitution-replacement: set INSTEAD of `result` for
    /// materialization attempts (the gRPC layer rejects requests
    /// carrying both). `None` for every build report — the as-built
    /// shape, bit-identical.
    pub materialization_outcome: Option<rio_proto::types::MaterializationOutcome>,
}

/// Eager-flush trigger (i): the actor coalesces at most this many
/// `ReportPullOutcome` commands per [`flush_pending_pull_outcomes`]
/// pass. Chosen as a power of two well above the expected per-replica
/// burst (S1's coordinator offers ~46/s × ~1s actor-turn budget) and
/// well below where the per-item residual chain (~5 PG awaits/item)
/// would push a single flush past the per-phase budget warn.
///
/// [`flush_pending_pull_outcomes`]: DagActor::flush_pending_pull_outcomes
pub(super) const REPORT_OUTCOME_BATCH_MAX: usize = 64;

/// Eager-flush trigger (iv) — the deadline backstop (sh-027 §3): a
/// queued report flushes at most this long after the FIRST push of
/// its batch, decoupling ack latency from `tick_interval` (the
/// `tickIntervalSecs=600` materialization VM fixture relies on
/// "never tick-driven", and store-side `report_until_acked`'s
/// per-attempt cap is `DEFAULT_GRPC_TIMEOUT=30s`). 250ms in
/// production: well under the 30s cap, and at sh-027 §3's measured
/// ~46 reports/s a 250ms window coalesces ~11 — vs the retired
/// mailbox-empty trigger's N̄≈5.5 (interleaving with
/// `ListMaterializationJobs` / `SubstituteProgress` made the
/// mailbox-empty signal fire per-item ~80% of the time). Short in
/// tests (precedent: [`POISON_TTL`](crate::state::POISON_TTL)) so
/// the ~113 lone-report test sites do not each wait 250ms.
#[cfg(not(test))]
pub(super) const REPORT_OUTCOME_FLUSH_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(250);
#[cfg(test)]
pub(super) const REPORT_OUTCOME_FLUSH_DEADLINE: std::time::Duration =
    std::time::Duration::from_millis(25);

/// One queued `ActorCommand::ReportPullOutcome` — the EXACT command
/// field set, reply channel INCLUDED. Held by
/// [`DagActor::pending_pull_outcomes`] between intake and the flush
/// that sends every reply only after the batched completion has
/// committed (the ack-after-durable contract).
#[derive(Debug)]
pub(super) struct PendingReport {
    pub(super) exec_id: Uuid,
    pub(super) auth_intent: Option<String>,
    pub(super) payload: PullReportPayload,
    pub(super) reply: oneshot::Sender<Result<(), PullRejection>>,
}

/// One held `(reply, result)` pair inside
/// [`DagActor::flush_pending_pull_outcomes`] — replies are sent only
/// after the batched completion commits.
type PendingAck = (
    oneshot::Sender<Result<(), PullRejection>>,
    Result<(), PullRejection>,
);

impl DagActor {
    /// Handle one `ReportOutcome` (the actor turn): queue it on
    /// [`pending_pull_outcomes`](Self::pending_pull_outcomes) and —
    /// when there is nothing left to coalesce with — drain the queue
    /// via [`Self::flush_pending_pull_outcomes`]. The reply is sent only
    /// after the flush's batched `complete_ready_from_store_batch`
    /// call returns (its appending transaction has committed by
    /// then), which is what the pod's exit-0 waits for. The flush
    /// resolves the exec_id to its attempt, decides via
    /// [`fold_report`], and on Process runs the existing completion
    /// path per item — the same `handle_completion` entry point the
    /// stream arm calls, so the worker-report→fold feed is identical
    /// in classification terms.
    ///
    /// Flush triggers: (i) `len ≥ REPORT_OUTCOME_BATCH_MAX` here;
    /// (ii) `handle_tick` head; (iii) `handle_leader_lost` (drains
    /// with `NotLeader`); (iv) the [`REPORT_OUTCOME_FLUSH_DEADLINE`]
    /// select! arm — a 250ms-bounded ack latency, decoupled from
    /// `tick_interval`. The retired mailbox-empty signal (sh-002
    /// trigger iv) interleaved with `ListMaterializationJobs` /
    /// `SubstituteProgress` and degraded N̄ to ~5.5 (sh-027 §3); the
    /// deadline arm coalesces up to `min(64, reports_per_250ms)` and
    /// is observable via `rio_scheduler_pull_outcome_flush_batch_size`.
    // r[impl sched.executor.report-idempotent]
    pub(super) async fn handle_report_outcome(
        &mut self,
        exec_id: Uuid,
        auth_intent: Option<String>,
        payload: PullReportPayload,
        reply: oneshot::Sender<Result<(), PullRejection>>,
    ) {
        // r[sched.lease.standby-drops-writes+4]: the only no-PG check
        // — keep it inline so a standby replies immediately. The
        // auth-intent binding needs the `find_attempt_by_exec_id`
        // row, so it stays inside the per-item flush body.
        if !self.leader.is_leader() {
            let _ = reply.send(Err(PullRejection::NotLeader));
            return;
        }
        // sh-027 §3 (s3-interval-reset): the deadline arm's guard is
        // `!pending.is_empty()`, so while idle the Interval is
        // unpolled and its internal deadline goes stale. Without this
        // reset, the FIRST report after any idle gap >250ms would
        // re-enable the guard with an immediately-Ready tick →
        // flushes at N=1 — the exact N̄ degradation this slot exists
        // to fix. `MissedTickBehavior` does not help (it governs the
        // NEXT tick after a late fire; the first late tick still
        // fires immediately). The `biased;` arm placement (AFTER
        // `rx.recv()`) is the second half: it lets the queued burst
        // dequeue before the deadline arm is even considered.
        if self.pending_pull_outcomes.is_empty() {
            self.pull_flush_deadline.reset();
        }
        self.pending_pull_outcomes.push(PendingReport {
            exec_id,
            auth_intent,
            payload,
            reply,
        });
        // Flush trigger (i): eager at the batch cap. Triggers
        // (ii)/(iii)/(iv) are handle_tick head / handle_leader_lost /
        // the REPORT_OUTCOME_FLUSH_DEADLINE select! arm.
        if self.pending_pull_outcomes.len() >= REPORT_OUTCOME_BATCH_MAX {
            self.flush_pending_pull_outcomes().await;
        }
    }

    /// Drain [`pending_pull_outcomes`](Self::pending_pull_outcomes) as
    /// a phased pipeline (sh-007c S6 — O(1) PG round-trips per flush
    /// for the materialization-kind subset, replacing ~12 RTTs/item):
    ///
    /// - **(A)** Prefetch — three batched readers
    ///   ([`SchedulerDb::find_attempts_by_exec_ids`],
    ///   [`SchedulerDb::unresolved_jobs_for_derivations`],
    ///   [`SchedulerDb::effective_wanted_unions_for`]).
    /// - **(B)** Per-item in-memory routing — each materialization
    ///   report's UNCHARGED arm (Success / RetryLater / Aborted /
    ///   zero-width) becomes a [`BatchIntent`]; build-kind, charged,
    ///   and probe-bearing arms route to the per-item slow path.
    /// - **(C)** ONE
    ///   [`SchedulerDb::close_and_resolve_materialization_batch_fenced`]
    ///   fenced tx (close + append + resolve; the
    ///   `BatchCloseResult` sets feed phase D).
    /// - **(D)** Per-item apply — synthesize a `WriteDisposition` from
    ///   `{tx outcome, exec_id ∈ closed_set}`, mint the witness via
    ///   [`DagActor::close_for_consumption_from_disposition`], run the
    ///   companion. The in-mem `push_attempt_record` /
    ///   `refresh_retry_view` ledger sync gates on `inserted_set`
    ///   (orthogonal to `closed_set`); on batch-`Fenced`, ONE
    ///   `note_fenced_evidence_write`.
    /// - **(E)** Once-per-flush tail —
    ///   `release_materialization_pins_best_effort` (hoisted from the
    ///   per-resolve site), drain `pending_walk_completed` into ONE
    ///   [`Self::complete_ready_from_store_batch`], then — and only
    ///   then — send every held `(reply, result)` pair.
    ///
    /// Ack-after-durable: every reply is sent strictly after the
    /// batched close+resolve AND the batched
    /// `persist_status_batch(Completed)` commit. An actor panic
    /// between push and flush drops every held reply →
    /// `oneshot::Canceled` → store-side `report_until_acked` retries.
    ///
    /// [`SchedulerDb::find_attempts_by_exec_ids`]: crate::db::SchedulerDb::find_attempts_by_exec_ids
    /// [`SchedulerDb::unresolved_jobs_for_derivations`]: crate::db::SchedulerDb::unresolved_jobs_for_derivations
    /// [`SchedulerDb::effective_wanted_unions_for`]: crate::db::SchedulerDb::effective_wanted_unions_for
    /// [`SchedulerDb::close_and_resolve_materialization_batch_fenced`]: crate::db::SchedulerDb::close_and_resolve_materialization_batch_fenced
    /// [`BatchIntent`]: super::materialize::BatchIntent
    pub(super) async fn flush_pending_pull_outcomes(&mut self) {
        use super::materialize::{BatchIntent, WriteDisposition};
        use crate::db::open_attempts::AttemptRef;
        use rio_evidence_kernel::pull::ReportAdmission;

        if self.pending_pull_outcomes.is_empty() {
            return;
        }
        debug_assert!(
            self.pending_walk_completed.is_empty(),
            "pending_walk_completed is flush-scoped — drained at the \
             tail of every flush and nowhere else"
        );
        let pending = std::mem::take(&mut self.pending_pull_outcomes);
        // sh-027 §3: the prod N̄ signal — `begin_fenced_calls` is
        // `#[cfg(test)]` only, so the sh-007c S6 design's core
        // assumption (N̄≥20) was unverifiable in prod. Buckets at
        // [1,2,5,10,20,32,64] so the design target is a bucket edge.
        metrics::histogram!("rio_scheduler_pull_outcome_flush_batch_size")
            .record(pending.len() as f64);
        let mut acks: Vec<PendingAck> = Vec::with_capacity(pending.len());

        // ─── Phase A: prefetch ────────────────────────────────────
        let exec_ids: Vec<Uuid> = pending.iter().map(|p| p.exec_id).collect();
        let attempts = match self.db.find_attempts_by_exec_ids(&exec_ids).await {
            Ok(m) => m,
            Err(e) => {
                // Batch read failed: NACK every report retryably (the
                // store re-delivers; the next flush re-reads).
                let msg = format!("attempt lookup failed: {e}");
                for PendingReport { reply, .. } in pending {
                    let _ = reply.send(Err(PullRejection::Internal(msg.clone())));
                }
                return;
            }
        };

        // ─── Phase B: per-item partition + routing ────────────────
        // Q8: between entry and first close,
        // `consume_materialization_outcome` only does `&self`/
        // `&self.db` reads — batch-wide prefetch before any in-mem
        // mutation is sound.
        let mut intents: Vec<(BatchIntent, oneshot::Sender<Result<(), PullRejection>>)> =
            Vec::new();
        let mut mat_drv_ids: Vec<Uuid> = Vec::new();
        let mut staged: Vec<(
            PendingReport,
            crate::db::open_attempts::MatAttempt,
            rio_evidence_kernel::pull::ProcessAdmission,
        )> = Vec::new();
        for p in pending {
            let attempt = attempts.get(&p.exec_id);
            // Partition: only materialization-kind reports with a
            // Process admission and a payload reach the prefetched
            // routing. Everything else — unknown-exec, build-kind,
            // kind-mismatch, AckIgnore (duplicate/late), no-payload —
            // routes to today's per-item `report_outcome_inner` (the
            // unknown-exec arm preserves
            // `sh002_leader_lost_drains_pending_reports_not_leader`:
            // LeaderLost drains BEFORE this body runs).
            let Some(AttemptRef::Materialization(m)) = attempt else {
                let result = self
                    .report_outcome_inner(p.exec_id, p.auth_intent.as_deref(), p.payload)
                    .await;
                acks.push((p.reply, result));
                continue;
            };
            // r[impl sec.executor.identity-token+3]
            if let Some(auth) = &p.auth_intent
                && auth != &m.core.drv_hash
            {
                warn!(exec_id = %p.exec_id,
                      "ReportOutcome rejected: executor token bound to a different intent");
                acks.push((p.reply, Err(PullRejection::TokenMismatch)));
                continue;
            }
            let Some(outcome) = &p.payload.materialization_outcome else {
                warn!(exec_id = %p.exec_id,
                      "build-report payload for a materialization attempt; ignoring");
                acks.push((p.reply, Ok(())));
                continue;
            };
            let admission = match fold_report(
                m.core.assignment_active,
                m.core.attempt_recorded || m.core.attempt_terminal,
            ) {
                ReportAdmission::AckIgnore => {
                    debug!(exec_id = %p.exec_id,
                           "duplicate/late materialization report acknowledged-and-ignored");
                    acks.push((p.reply, Ok(())));
                    continue;
                }
                ReportAdmission::Process(a) => a,
            };
            if outcome.outcome.is_none() {
                warn!(exec_id = %p.exec_id,
                      "materialization outcome with no payload; acknowledged-and-ignored");
                acks.push((p.reply, Ok(())));
                continue;
            }
            mat_drv_ids.push(m.core.derivation_id);
            staged.push((p, m.clone(), admission));
        }

        if !staged.is_empty() {
            // Phase A continued: prefetch jobs + wanted unions for
            // the materialization subset (one RTT each; on read
            // failure, fall back to the per-item slow path so the
            // ack-after-durable contract holds).
            let (jobs, wanted) = match tokio::try_join!(
                self.db.unresolved_jobs_for_derivations(&mat_drv_ids),
                self.db.effective_wanted_unions_for(&mat_drv_ids),
            ) {
                Ok(jw) => jw,
                Err(e) => {
                    warn!(error = %e,
                          "batched prefetch (jobs/wanted) failed; falling back to per-item");
                    for (p, _, _) in staged {
                        let result = self
                            .report_outcome_inner(p.exec_id, p.auth_intent.as_deref(), p.payload)
                            .await;
                        acks.push((p.reply, result));
                    }
                    return self.flush_tail(acks).await;
                }
            };
            for (p, m, admission) in staged {
                let inner = p
                    .payload
                    .materialization_outcome
                    .as_ref()
                    .and_then(|o| o.outcome.as_ref())
                    .expect("filtered above");
                match self.consume_materialization_outcome_prefetched(
                    p.exec_id,
                    &m,
                    inner,
                    jobs.get(&m.core.derivation_id),
                    wanted.get(&m.core.derivation_id),
                    admission,
                ) {
                    Ok(intent) => intents.push((intent, p.reply)),
                    Err(_admission) => {
                        // Unobtainable / InfraFailure: per-item slow
                        // path (the `report_outcome_inner` body re-
                        // runs the fold + admission mint over the
                        // same row state, so the unspent admission
                        // returned here is dropped and the slow-path
                        // mints its own — at-most-once still holds:
                        // exactly one consumption pass per report).
                        let result = self
                            .report_outcome_inner(p.exec_id, p.auth_intent.as_deref(), p.payload)
                            .await;
                        acks.push((p.reply, result));
                    }
                }
            }
        }

        // ─── Phase C: ONE fenced close+resolve tx ─────────────────
        // Ack-after-durable hard gate: structurally guarded by
        // control flow — every reply is held in `acks` / `intents`
        // and sent only by `flush_tail` after phase E. The slow-path
        // arms above MAY have pushed to `pending_walk_completed`
        // (their per-item consumption ran); the batched arms push in
        // phase D below; the tail drains both into ONE batched
        // completion before any reply fires.
        if !intents.is_empty() {
            #[cfg(test)]
            self.test_counters
                .begin_fenced_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let serving_generation = self.serving_generation();
            let close_exec_ids: Vec<Uuid> = intents.iter().map(|(i, _)| i.exec_id).collect();
            let resolves: Vec<_> = intents.iter().filter_map(|(i, _)| i.resolve()).collect();
            // No charge rows on the batched lane: every batched arm
            // (Success / RetryLater / Aborted / zero-width) closes
            // UNCHARGED. `inserted_set` is therefore empty by
            // construction; the in-mem `push_attempt_record` /
            // `refresh_retry_view` gate (which keys on `inserted_set`)
            // is a no-op for this lane and stays the per-item slow
            // path's responsibility for charged arms.
            let batch = self
                .db
                .close_and_resolve_materialization_batch_fenced(
                    serving_generation,
                    &close_exec_ids,
                    &[],
                    &resolves,
                )
                .await;

            // ─── Phase D: per-item apply ──────────────────────────
            // The in-mem ledger sync (`push_attempt_record` /
            // `refresh_retry_view`) gates on `inserted_set`, NOT
            // `closed_set` (orthogonal: drv_attempts ON CONFLICT vs
            // assignment-close). With no charge rows on the batched
            // lane, the gate is empty by construction — asserted here
            // so a future charged-arm widening cannot silently skip
            // the sync (`failoverPreservesHistory`'s live==ledger
            // precondition would otherwise weaken silently).
            let (closed_set, resolved_set, batch_d) = match &batch {
                Ok(r) if r.fenced => {
                    self.note_fenced_evidence_write("materialization batch close");
                    (
                        &r.closed_set,
                        &r.resolved_set,
                        Some(WriteDisposition::Fenced),
                    )
                }
                Ok(r) => {
                    assert!(
                        r.inserted_set.is_empty(),
                        "the batched lane is uncharged-only — a non-empty inserted_set \
                         means a charge row reached phase C without the phase-D \
                         push_attempt_record/refresh_retry_view ledger sync"
                    );
                    (&r.closed_set, &r.resolved_set, None)
                }
                Err(e) => {
                    warn!(error = %e,
                          "materialization batch close failed; NACKing the batch retryably");
                    for (_, reply) in intents {
                        acks.push((reply, Err(PullRejection::ConsumptionNotDurable)));
                    }
                    return self.flush_tail(acks).await;
                }
            };
            let mut releases: Vec<super::materialize::DeferredRelease> = Vec::new();
            for (intent, reply) in intents {
                let close_d = batch_d.unwrap_or_else(|| {
                    if closed_set.contains(&intent.exec_id) {
                        WriteDisposition::Applied
                    } else {
                        WriteDisposition::AlreadyResolved
                    }
                });
                let result = match self
                    .apply_batched_companion(intent, close_d, resolved_set)
                    .await
                {
                    Ok(super::materialize::CompanionResult::Ack(_ack)) => Ok(()),
                    Ok(super::materialize::CompanionResult::DeferredRelease(d)) => {
                        releases.push(d);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                acks.push((reply, result));
            }
            // sh-027 §3: ONE batched release-and-requeue chokepoint
            // for every Release-arm intent the loop deferred (was N
            // per-item `companion_release` awaits inline).
            self.companion_release_batch(releases).await;
        }

        // ─── Phase E: once-per-flush tail ─────────────────────────
        self.flush_tail(acks).await;
    }

    /// Phase-E tail of [`Self::flush_pending_pull_outcomes`] (sh-007c
    /// S6): hoisted `release_materialization_pins_best_effort` (was
    /// per-resolve), ONE batched `complete_ready_from_store_batch`,
    /// then — and only then — every held reply. Factored so the
    /// fall-back arms above share the same ack-after-durable epilogue.
    async fn flush_tail(&mut self, acks: Vec<PendingAck>) {
        // The second-level accumulator: every Success consumption
        // pushed `(drv_hash, WalkVerified(..))` here. ONE batched
        // call (the I-139 amortization — 3 PG awaits total).
        let completed = std::mem::take(&mut self.pending_walk_completed);
        self.complete_ready_from_store_batch(&completed).await;
        // r[impl sched.materialize.pinning]
        // §5.3 release site (i), hoisted: the per-resolve pins
        // release is self-scoping and idempotent, so once per flush
        // (after every batched resolve committed) is equivalent.
        self.release_materialization_pins_best_effort("report flush")
            .await;
        // Ack-after-durable: replies fire only now.
        for (reply, result) in acks {
            let _ = reply.send(result);
        }
    }

    // r[impl sec.executor.identity-token+3]
    /// sh-045 r1 — the shared executor-report prologue: leader-gate →
    /// `find_attempt_by_exec_id` → ack-unknown → token↔intent binding.
    /// `Ok(None)` = unknown/superseded exec (acknowledged-and-ignored;
    /// the caller returns `Ok(())`). Both `report_outcome_inner` and
    /// the heartbeat cold-path call it so a future hardening (e.g.
    /// reject when `auth_intent` is `None` under a non-dev key) edits
    /// ONE body.
    async fn resolve_authed_attempt(
        &self,
        exec_id: Uuid,
        auth_intent: Option<&str>,
        rpc: &'static str,
    ) -> Result<Option<crate::db::open_attempts::AttemptRef>, PullRejection> {
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        let attempt = self
            .db
            .find_attempt_by_exec_id(exec_id)
            .await
            .map_err(|e| PullRejection::Internal(format!("attempt lookup failed: {e}")))?;
        let Some(attempt) = attempt else {
            // Never-pulled (or superseded) exec: acknowledged, nothing
            // written — the no-open-attempt arm of report idempotency.
            debug!(%exec_id, "{rpc} for unknown/superseded exec acknowledged-and-ignored");
            return Ok(None);
        };
        // Token↔intent binding (per-unary, same as the pull): the
        // attested intent must be the attempt's derivation.
        if let Some(auth) = auth_intent
            && auth != attempt.core().drv_hash
        {
            warn!(%exec_id, "{rpc} rejected: executor token bound to a different intent");
            return Err(PullRejection::TokenMismatch);
        }
        Ok(Some(attempt))
    }

    async fn report_outcome_inner(
        &mut self,
        exec_id: Uuid,
        auth_intent: Option<&str>,
        payload: PullReportPayload,
    ) -> Result<(), PullRejection> {
        let Some(attempt) = self
            .resolve_authed_attempt(exec_id, auth_intent, "ReportOutcome")
            .await?
        else {
            return Ok(());
        };
        // The kind witness (A2): materialization attempts route to
        // their own consumption transaction; the build arms below bind
        // `&BuildAttempt`, so a cross-kind close no longer typechecks.
        // Kind mismatch between witness and payload is acknowledged-
        // and-ignored (the kindMatchesWorker rule at the report
        // intake). Both arms are reachable FLAG-OFF from a
        // buggy/hostile reporter, so the warn+ack posture is a dormancy
        // guarantee, not just a flag-on routing rule.
        let b = match &attempt {
            crate::db::open_attempts::AttemptRef::Materialization(m) => {
                return match payload.materialization_outcome {
                    Some(outcome) => {
                        // Kind-uniform admission (bug_134): the SAME
                        // kernel fold the build arm uses — the old
                        // hand gate here read only the recorded bits
                        // and ignored `assignment_active`, so a
                        // closed-but-unrecorded attempt's late report
                        // could still reach consumption.
                        match fold_report(
                            m.core.assignment_active,
                            m.core.attempt_recorded || m.core.attempt_terminal,
                        ) {
                            ReportAdmission::AckIgnore => {
                                // Terminal-row-wins: a duplicate/late
                                // report for a consumed or closed
                                // attempt is acknowledged.
                                debug!(%exec_id, "duplicate/late materialization report acknowledged-and-ignored");
                                Ok(())
                            }
                            ReportAdmission::Process(admission) => match outcome.outcome {
                                // Intake-level arm: no payload means no
                                // consumption ran — acknowledged without
                                // a witness (nothing closed, nothing to
                                // ack lawfully; same family as
                                // AckIgnore; the admission is dropped
                                // unspent by design).
                                None => {
                                    warn!(%exec_id,
                                          "materialization outcome with no payload; \
                                           acknowledged-and-ignored");
                                    Ok(())
                                }
                                // The consumption proper: spends the
                                // admission witness (the fold is the
                                // only mint), and the MatAck witness
                                // proves an ack-lawful state was
                                // reached (settled close + companion,
                                // or fenced); a non-durable close
                                // surfaces as the retryable NACK.
                                Some(inner) => self
                                    .consume_materialization_outcome(exec_id, m, inner, admission)
                                    .await
                                    .map(|_ack: super::materialize::MatAck| ()),
                            },
                        }
                    }
                    None => {
                        warn!(%exec_id, "build-report payload for a materialization attempt; ignoring");
                        Ok(())
                    }
                };
            }
            crate::db::open_attempts::AttemptRef::Build(b) => b,
        };
        if payload.materialization_outcome.is_some() {
            warn!(%exec_id, "materialization payload for a build attempt; acknowledged-and-ignored");
            return Ok(());
        }
        match fold_report(
            b.core.assignment_active,
            b.core.attempt_recorded || b.core.attempt_terminal,
        ) {
            ReportAdmission::AckIgnore => {
                debug!(
                    %exec_id,
                    drv_hash = %b.core.drv_hash,
                    assignment_active = b.core.assignment_active,
                    "duplicate/late ReportOutcome acknowledged-and-ignored"
                );
                // bug_077: the late-report lane — the typed effect a
                // non-admitted report may still have (the cancelled
                // count gap-fill rides HERE, where late reports
                // actually arrive). bug_098: the effect carries the
                // REPORT'S OWN exec — the find key this intake
                // resolved the attempt by — so the fill can never
                // stamp a successor attempt through the node's
                // mutable carrier. Round-9 WO-S1-1: a late
                // success-class report on a cancelled/evicted drv
                // classifies `Register` here — THE production lane
                // the 1,735 lost run-1 registrations arrived on.
                let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
                let (ctx, declared) = self.late_node_context(drv_hash.as_str());
                let outputs = Self::validated_late_outputs(
                    b.core.executor_id.as_str(),
                    drv_hash.as_str(),
                    declared.as_deref(),
                    &payload.result.built_outputs,
                );
                let effect = super::completion::late_report_effect(
                    Some(super::completion::ReportingExec(exec_id)),
                    ctx,
                    payload.result.status(),
                    payload.final_line_count,
                    outputs,
                );
                self.apply_late_report_effect(
                    b.core.executor_id.as_str(),
                    drv_hash.as_str(),
                    effect,
                )
                .await;
                Ok(())
            }
            ReportAdmission::Process(admission) => {
                let executor_id = ExecutorId::from(b.core.executor_id.as_str());
                // r[impl sched.attempt.synthesized-verdict+4]
                // AD5 abort charge class: a pod reporting `Cancelled`
                // for a derivation the scheduler still wants is the
                // SIGTERM-abort report (preemption, scale-down,
                // controller delete) — a platform termination, not a
                // worker fault. It closes the attempt charge-free and
                // requeues at this fold; it is never charged as an
                // infrastructure failure. A genuinely-cancelled
                // (no-longer-wanted) derivation falls through to the
                // completion path's Cancelled early-return and stays
                // exactly as the cancel arm leaves it (no row, no
                // requeue).
                let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
                let abort_of_still_wanted = payload.result.status()
                    == rio_proto::types::BuildResultStatus::Cancelled
                    && self.dag.node(&drv_hash).is_some_and(|s| {
                        matches!(
                            s.status(),
                            DerivationStatus::Assigned | DerivationStatus::Running
                        ) && s.exec_id == Some(exec_id)
                    });
                if abort_of_still_wanted {
                    // sh-041u chokepoint #3: the AD5 short-circuit
                    // never reaches `handle_admitted_completion`'s
                    // chokepoint #2, so observe peaks HERE. Placed
                    // BEFORE `admit_worker_abort`; the at-bound
                    // fall-through ALSO reaches chokepoint #2 — the
                    // double-observe is idempotent under `set_dim`'s
                    // max, and #2's per-handler `floor_at_cap` stamp
                    // narrows by axis, so the redundant observe is
                    // harmless.
                    let peaks = super::floor::ObservedPeaks::from_report(
                        payload.peak_memory_bytes,
                        payload
                            .final_resources
                            .as_ref()
                            .and_then(|r| r.cpu_seconds_total),
                        payload
                            .final_resources
                            .as_ref()
                            .and_then(|r| r.peak_disk_bytes),
                    );
                    let _ = self
                        .observe_resource_floor(
                            &drv_hash,
                            peaks,
                            super::floor::AttemptCloseReason::WorkerAbort,
                        )
                        .await;
                    // r[impl sched.attempt.worker-abort-bounded+2]
                    // bug_279: the charge-free admission is LEDGER-
                    // BOUNDED — the worker-supplied Cancelled
                    // discriminator is trusted only while the trailing
                    // run of build-lane worker-abort closures is below
                    // the kernel bound. At the bound the report falls
                    // through to the charged unsolicited-Cancelled→
                    // infrastructure arm of the completion path,
                    // consuming the attempt WITH budget (exclusion and
                    // poison advance; the loop is finite).
                    let admission = self
                        .dag
                        .node(&drv_hash)
                        .map(|s| crate::retry_policy::admit_worker_abort(s.attempt_history()))
                        .unwrap_or(rio_retry_kernel::WorkerAbortAdmission::Uncharged);
                    if admission == rio_retry_kernel::WorkerAbortAdmission::Uncharged {
                        info!(
                            %exec_id,
                            drv_hash = %b.core.drv_hash,
                            "pull-mode SIGTERM-abort report for still-wanted work: closing the \
                             attempt charge-free and requeueing (AD5)"
                        );
                        // AD2c: node attribution comes from the
                        // controller-authoritative binding only — never the
                        // worker-supplied report fields.
                        let source_node = self.pull_attempt_source_node(&drv_hash);
                        self.close_pull_attempt_uncharged(
                            b,
                            exec_id,
                            "worker_abort",
                            crate::state::ReportingParty::Worker,
                            source_node,
                            "worker-abort",
                        )
                        .await;
                        return Ok(());
                    }
                    warn!(
                        %exec_id,
                        drv_hash = %b.core.drv_hash,
                        bound = rio_retry_kernel::WORKER_ABORT_FREE_CLOSES,
                        "worker-abort free-close run at the bound; consuming the report as a \
                         charged infrastructure failure (worker-protocol loop)"
                    );
                }
                // Same internal entry point as the stream Completion
                // arm — classification, verdict, attempt-row append,
                // status persist, realisations, SLA samples all happen
                // exactly as they do for stream-reported outcomes. The
                // drv is addressed by the attempt's own derivation (the
                // exec_id is the key; the report's drv_path is not
                // trusted to re-route it).
                self.handle_admitted_completion(
                    admission,
                    &executor_id,
                    &b.core.drv_path,
                    payload.result,
                    (payload.peak_memory_bytes, payload.peak_cpu_cores),
                    (payload.node_name, payload.hw_class),
                    (payload.final_resources, payload.final_line_count),
                )
                .await;
                Ok(())
            }
        }
    }

    /// Close one open pull-mode attempt charge-free (the AD5 abort /
    /// synthesized-verdict closure): append exactly one uncharged
    /// terminal row (`disconnected`, `termination_reason` filled — the
    /// fold treats it as a no-charge event and the open-attempt view
    /// drops it) and close the assignment row in ONE transaction
    /// carrying the same claims-floor fence as the pull mint and the
    /// establishment sweep, then mirror the row in memory and requeue
    /// the derivation if this attempt was still the in-flight one.
    /// Idempotent: a row already present for the exec (any classifier
    /// won first) makes the append a no-op and nothing else changes.
    // r[impl sched.attempt.synthesized-verdict+4]
    /// Takes the BUILD witness: a materialization attempt cannot be
    /// closed by this path — the cross-kind call no longer typechecks
    /// (merged_bug_146's structural half).
    async fn close_pull_attempt_uncharged(
        &mut self,
        attempt: &crate::db::open_attempts::BuildAttempt,
        exec_id: Uuid,
        termination_reason: &str,
        reporting_party: crate::state::ReportingParty,
        source_node: Option<String>,
        requeue_cause: &'static str,
    ) {
        if !self.leader.is_leader() {
            return;
        }
        let drv_hash = &DrvHash::from(attempt.core.drv_hash.as_str());
        let executor_id = &ExecutorId::from(attempt.core.executor_id.as_str());
        let serving_generation = self.serving_generation;
        let mut row = crate::db::attempts::AttemptRow::new(
            attempt.core.derivation_id,
            crate::state::OutcomeClass::Disconnected,
            reporting_party,
            crate::state::AttemptKind::Build,
        );
        row.exec_id = Some(exec_id);
        row.executor_id = Some(executor_id.clone());
        row.source_node = source_node;
        row.termination_reason = Some(termination_reason.to_owned());
        if let Some(state) = self.dag.node(drv_hash) {
            row.resubmit_cycle = i32::try_from(state.retry.resubmit_cycles).unwrap_or(i32::MAX);
        }
        let result: Result<Option<bool>, sqlx::Error> = async {
            // The same generation fence the pull mint and the
            // establishment sweep apply: a below-floor serving
            // generation writes nothing.
            let mut tx = match self.db.begin_fenced(serving_generation).await? {
                crate::db::FencedBegin::Fenced { .. } => return Ok(None),
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let inserted = crate::db::SchedulerDb::append_attempt(tx.conn(), &row).await?;
            tx.close_assignment(exec_id, crate::db::AssignmentCloseStatus::Failed)
                .await?;
            tx.commit().await?;
            Ok(Some(inserted))
        }
        .await;
        let inserted = match result {
            Ok(Some(inserted)) => inserted,
            Ok(None) => {
                info!(drv_hash = %drv_hash, serving_generation = serving_generation.as_i64(),
                      "uncharged close: serving generation below the claims floor; nothing written");
                return;
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %exec_id, error = %e,
                      "uncharged close failed; the attempt stays open (the establishment \
                       sweep remains the backstop)");
                return;
            }
        };
        if !inserted {
            // Another classifier won the row; its verdict stands.
            return;
        }
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.push_attempt_record(row.to_record());
        }
        self.refresh_retry_view(drv_hash);
        // OA1 interval: the closing observation and the requeue happen
        // in this same actor turn, so the recorded latency is the
        // in-turn processing time (the same shape as the worker-report
        // cause).
        metrics::histogram!(
            "rio_scheduler_attempt_requeue_seconds",
            "cause" => requeue_cause
        )
        .record((crate::db::attempts::epoch_now() - row.occurred_at_epoch_secs).max(0.0));
        let still_in_flight = self.dag.node(drv_hash).is_some_and(|s| {
            matches!(
                s.status(),
                DerivationStatus::Assigned | DerivationStatus::Running
            ) && s.exec_id == Some(exec_id)
        });
        if still_in_flight {
            self.reassign_derivations(std::slice::from_ref(drv_hash), Some(executor_id))
                .await;
        }
    }

    /// sh-039 (revised): SYNCHRONOUS witnessed-clock establishment for
    /// one open pull-mode attempt — the controller-synthesized verdict
    /// arrived over a recorded witnessed-terminal mark, so the
    /// witnessed reason is known NOW and the controller is about to
    /// delete the Job (`delete_job_with_synthesized_report` deletes
    /// unconditionally). Runs the establishment sweep's C2 charge arm
    /// (`housekeeping.rs`) inline — `append_and_decide_in_tx` +
    /// `witnessed_disposition` + `observe_resource_floor` — so the durable
    /// row commits BEFORE the ack returns and the controller deletes
    /// the Job. The earlier defer-to-mark shape (age `witnessed_at` to
    /// 0.0, return `Unresolved`) left a ~5s window where the Job was
    /// gone, the attempt was open, and the only establishment trigger
    /// was the in-memory mark — a leader restart in that window
    /// re-created the deadline-anchor pathology (sh-039 wall B
    /// re-formed one failover over).
    ///
    /// Consumes the mark on commit; on tx failure or fence the mark
    /// stays in place and the establishment sweep backstops (same
    /// posture as [`Self::close_pull_attempt_uncharged`]).
    // r[impl sched.attempt.witnessed-terminal+3]
    async fn establish_from_witnessed(
        &mut self,
        attempt: &crate::db::open_attempts::BuildAttempt,
        exec_id: Uuid,
        witnessed_reason: rio_proto::types::AttemptTerminalReason,
        node_name: Option<String>,
    ) {
        let drv_hash = DrvHash::from(attempt.core.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.core.executor_id.as_str());
        let serving_generation = self.serving_generation;
        let verdict_eligible = self.dag.node(&drv_hash).is_some_and(|s| {
            matches!(
                s.status(),
                DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
            )
        });
        let mut row = crate::db::attempts::AttemptRow::new(
            attempt.core.derivation_id,
            crate::state::OutcomeClass::ExecutorCrash,
            crate::state::ReportingParty::Scheduler,
            crate::state::AttemptKind::Build,
        );
        row.exec_id = Some(exec_id);
        row.executor_id = Some(executor.clone());
        // AD2c: prefer the controller-reported node from this very
        // report, else the in-memory spawn-ack binding.
        row.source_node = node_name.or_else(|| self.pull_attempt_source_node(&drv_hash));
        // The witnessed reason's label — the row carries the
        // controller-witnessed letter (the report this path stands in
        // for), never the synthesized handshake's `reaped`/`cancelled`.
        row.termination_reason = Some(attempt_terminal_reason_label(witnessed_reason).to_owned());
        type ChargeOutcome = Option<(bool, crate::retry_policy::Decision)>;
        let result: Result<ChargeOutcome, sqlx::Error> = async {
            // r[impl sched.lease.generation-fence+3]
            let mut tx = match self.db.begin_fenced(serving_generation).await? {
                crate::db::FencedBegin::Fenced { .. } => return Ok(None),
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let (won, decision) = self.append_and_decide_in_tx(tx.conn(), &row).await?;
            if won
                && verdict_eligible
                && matches!(decision.verdict, crate::retry_policy::Verdict::Poison(_))
            {
                crate::db::SchedulerDb::persist_poisoned_in_tx(tx.conn(), &drv_hash).await?;
            }
            tx.close_assignment(exec_id, crate::db::AssignmentCloseStatus::Failed)
                .await?;
            tx.commit().await?;
            Ok(Some((won, decision)))
        }
        .await;
        let (won, decision) = match result {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                info!(drv_hash = %drv_hash, serving_generation = serving_generation.as_i64(),
                      "witnessed establishment: serving generation below the claims floor; \
                       nothing written");
                return;
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %exec_id, error = %e,
                      "witnessed establishment failed; the mark stays in place \
                       (the establishment sweep backstops)");
                return;
            }
        };
        // The charging transaction committed: the witnessed mark is
        // consumed with the attempt.
        self.witnessed_terminal.remove(&exec_id);
        if !won {
            return;
        }
        // sh-041u chokepoint #4: the witnessed reason feeds the
        // per-reason disposition table (the same `witnessed_disposition`
        // + `observe_resource_floor` path the establishment sweep's
        // witnessed arm runs; `observe_resource_floor_caller_census`
        // files this row). Sits AFTER the `if !won { return }` guard —
        // the won flag remains the once-per-attempt cap (live_058-b).
        self.observe_witnessed_floor(&drv_hash, witnessed_reason)
            .await;
        metrics::histogram!(
            "rio_scheduler_attempt_requeue_seconds",
            "cause" => "establishment"
        )
        .record((crate::db::attempts::epoch_now() - row.occurred_at_epoch_secs).max(0.0));
        metrics::counter!("rio_scheduler_pull_establishments_total").increment(1);
        if let Some(state) = self.dag.node_mut(&drv_hash) {
            state.push_attempt_record(row.to_record());
        }
        self.refresh_retry_view(&drv_hash);
        info!(
            drv_hash = %drv_hash,
            %exec_id,
            executor_id = %executor,
            witnessed_reason = attempt_terminal_reason_label(witnessed_reason),
            "synthesized verdict over a witnessed mark: established synchronously \
             (charged; the controller may now delete the Job)"
        );
        if !verdict_eligible {
            return;
        }
        match decision.verdict {
            crate::retry_policy::Verdict::Poison(_) => {
                self.poison_already_recorded(
                    &drv_hash,
                    "poison threshold reached after unreported executor crashes",
                    None,
                    rio_proto::VerdictBacking::FreshExecution,
                )
                .await;
            }
            _ => {
                self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
                    .await;
            }
        }
    }
}

/// 124(d): how long after an `AckSpawnedIntents{spawned}` covering an
/// intent the scheduler defers `NoEligibleSource` verdicts for it (the
/// verdict raced its own spawn). ~3 controller ticks: long enough for
/// the new Job's pod to bind or surface as genuinely unschedulable,
/// short enough that a real exhaustion still poisons promptly (the
/// controller's persistence gate already adds ~3 ticks in front).
pub(crate) const ACKED_SPAWNED_DEFER_SECS: f64 = 30.0;

/// The attempt identity carried by `ReportAttemptOutcome` (exec_id
/// preferred; intent id accepted; the Job name is recorded for
/// diagnostics — its full resolution arrives with the controller-side
/// re-point, which always knows the exec/intent from the open-attempt
/// view).
#[derive(Debug, Default)]
pub struct AttemptIdentity {
    pub intent_id: Option<String>,
    pub job_name: Option<String>,
    pub exec_id: Option<Uuid>,
}

/// Map the wire reason to the `termination_reason` label the second
/// installment records. ONE mapping for both planes (bug_255): the
/// vocabulary lives in `rio_common::classify`; the controller's OA1
/// histogram routes through the same fn, so the planes' series line
/// up by construction.
pub(crate) fn attempt_terminal_reason_label(
    reason: rio_proto::types::AttemptTerminalReason,
) -> &'static str {
    rio_common::classify::attempt_terminal_reason_label(reason.into())
}

impl DagActor {
    /// Handle one `ReportAttemptOutcome` (the unified pod-terminal
    /// intake, scheduler half). Idempotent: a pod-terminal letter for
    /// an unclassified open attempt records the in-memory
    /// witnessed-terminal MARK (first-witnessed-wins — a
    /// level-triggered re-report re-creates an absent mark and
    /// otherwise changes nothing); a worker-reported row's
    /// second-installment reason-only fill writes once. The intake
    /// MARKS; the establishment sweep ACTS — on the witnessed clock
    /// (`witnessed_at + establishment_report_slack`) for marked
    /// attempts, on the dispatch deadline for everything else. The
    /// only intake arm that inserts a row, charges budget, or bumps a
    /// floor is the synthesized verdict OVER a recorded witnessed
    /// mark, which establishes synchronously
    /// ([`Self::establish_from_witnessed`] — the durable write must
    /// commit before the controller deletes the Job).
    ///
    /// The no-attempt arm (a pod that died without ever completing a
    /// pull) acknowledges and charges nothing; its only permitted side
    /// effects are clearing the intent's ICE cell (the
    /// `dispatched_cells` arm for that intent) and re-arming the spawn
    /// intent — which, for a still-wanted drv, simply means leaving it
    /// Ready so the next `GetSpawnIntents` re-emits it.
    // r[impl ctrl.report.attempt-outcome]
    // r[impl sched.attempt.no-attempt-no-op]
    /// sh-045: cache the builder's periodic running-telemetry on
    /// `SchedHint.last_reported_peaks`. The witnessed-close
    /// establishment (`observe_witnessed_floor`) reads it so a
    /// kubelet-SIGKILLed attempt's `cpu_seconds` / `peak_disk_bytes`
    /// reach `observe_peaks`. Auth-before-side-effect: the token↔intent
    /// binding (the same `auth != attempt.core().drv_hash` check
    /// `report_outcome_inner` applies) runs BEFORE the cache write — a
    /// builder holding a valid token for drv X that learns drv Y's
    /// `exec_id` is rejected without touching Y's cache.
    pub(super) async fn handle_report_running_telemetry(
        &mut self,
        exec_id: Uuid,
        auth_intent: Option<String>,
        wall_seconds: f64,
        peak_memory_bytes: u64,
        resources: rio_proto::types::ResourceUsage,
        reply: oneshot::Sender<Result<(), PullRejection>>,
    ) {
        let result = self
            .report_running_telemetry_inner(
                exec_id,
                auth_intent.as_deref(),
                wall_seconds,
                peak_memory_bytes,
                resources,
            )
            .await;
        let _ = reply.send(result);
    }

    async fn report_running_telemetry_inner(
        &mut self,
        exec_id: Uuid,
        auth_intent: Option<&str>,
        wall_seconds: f64,
        peak_memory_bytes: u64,
        resources: rio_proto::types::ResourceUsage,
    ) -> Result<(), PullRejection> {
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        // r[impl sec.executor.identity-token+3]
        // sh-045 r1 hot path: `auth_intent` IS the dispatch intent's
        // drv_hash (server-minted at pull, [`ExecutorClaims::intent_id`]).
        // Resolve in-memory; the per-heartbeat 3-table PG JOIN
        // (~200 qps at 1000 builders × 5 s tick, serialized on the
        // actor loop) only runs on the cold path (dev-mode `None`, or
        // the in-memory key disagrees — defensive). The token↔intent
        // binding holds by construction: the cache write is keyed on
        // the ATTESTED intent and gated on `state.exec_id == exec_id`,
        // so a builder holding a token for drv X that learns drv Y's
        // `exec_id` cannot touch Y.
        if let Some(auth) = auth_intent
            && let Some(state) = self.dag.node_mut(auth)
            && state.exec_id == Some(exec_id)
        {
            merge_heartbeat_peaks(
                &mut state.sched.last_reported_peaks,
                wall_seconds,
                peak_memory_bytes,
                resources,
            );
            return Ok(());
        }
        // Cold path: same prologue as `report_outcome_inner` (PG
        // resolve + token-binding); preserves `Err(TokenMismatch)` for
        // a forged intent (test (i)).
        let Some(attempt) = self
            .resolve_authed_attempt(exec_id, auth_intent, "ReportRunningTelemetry")
            .await?
        else {
            return Ok(());
        };
        // The cache write: in-memory only, per-field max-merge (the
        // builder's ticker monotonically refreshes, so a stale RPC
        // landing after the final ship — `abort()` is fire-and-forget
        // — cannot regress monotone fields). A heartbeat for a closed
        // attempt's exec_id finds no live node and no-ops.
        if let Some(state) = self.dag.node_mut(attempt.core().drv_hash.as_str())
            && state.exec_id == Some(exec_id)
        {
            merge_heartbeat_peaks(
                &mut state.sched.last_reported_peaks,
                wall_seconds,
                peak_memory_bytes,
                resources,
            );
        }
        Ok(())
    }

    pub(super) async fn handle_report_attempt_outcome(
        &mut self,
        identity: AttemptIdentity,
        reason: rio_proto::types::AttemptTerminalReason,
        node_name: Option<String>,
        resubmit_cycle: u32,
        reply: oneshot::Sender<Result<AttemptResolution, PullRejection>>,
    ) {
        let result = self
            .report_attempt_outcome_inner(identity, reason, node_name, resubmit_cycle)
            .await;
        let _ = reply.send(result);
    }

    async fn report_attempt_outcome_inner(
        &mut self,
        identity: AttemptIdentity,
        reason: rio_proto::types::AttemptTerminalReason,
        node_name: Option<String>,
        resubmit_cycle: u32,
    ) -> Result<AttemptResolution, PullRejection> {
        if !self.leader.is_leader() {
            return Err(PullRejection::NotLeader);
        }
        // AD2(a): the spawn-gate exhaustion verdict. Not a pod-terminal
        // classification — there is no pod and no attempt — so it is
        // handled before attempt resolution: the controller (which owns
        // the spawnable-source universe) detected excluded ⊇ spawnable
        // for this intent and the fold maps that to the fleet-exhaust
        // poison arm, exactly like the dispatch-time E9 backstop.
        if reason == rio_proto::types::AttemptTerminalReason::NoEligibleSource {
            return self
                .handle_no_eligible_source(&identity, resubmit_cycle)
                .await;
        }
        // r[impl sched.attempt.synthesized-verdict+4]
        // Synthesized verdicts (cancelled / preempted / reaped) REQUIRE
        // the attempt's exec_id (merged_bug_135): the controller holds
        // it from the same ListOpenAttempts read that justified the
        // verdict, and the intent fallback below is newest-open-wins —
        // a sticky DisruptionTarget re-fire or a stale verdict would
        // close a NEWER attempt it never observed. Refused = ack +
        // warn, nothing resolved; the establishment sweep remains the
        // bounded fallback classifier for the (skewed-controller) case.
        let synthesized = matches!(
            reason,
            rio_proto::types::AttemptTerminalReason::Cancelled
                | rio_proto::types::AttemptTerminalReason::Preempted
                | rio_proto::types::AttemptTerminalReason::Reaped
        );
        if synthesized && identity.exec_id.is_none() {
            // Same stale-watch hygiene as the no-attempt arm: a
            // refused synthesized report still proves the pod is gone.
            if let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) {
                self.dispatched_cells.remove(intent);
            }
            warn!(
                intent_id = ?identity.intent_id,
                job_name = ?identity.job_name,
                ?reason,
                "synthesized verdict without exec_id refused (acked charge-free; synthesized closes are exec-pinned)"
            );
            return Ok(AttemptResolution::Unresolved);
        }
        // Whether the report named the execution directly — the AD2c
        // fill below is gated on it (R4: fill iff Build witness AND
        // exec_id-resolved).
        let exec_resolved = identity.exec_id.is_some();
        // Resolve the attempt: exec_id first, then the intent's open
        // pull-mode attempt. A Job-name-only report cannot be resolved
        // here yet (the deterministic name embeds only a derived
        // suffix); the controller-side callers always know the
        // exec/intent from ListOpenAttempts.
        let resolved = if let Some(exec_id) = identity.exec_id {
            self.db
                .find_attempt_by_exec_id(exec_id)
                .await
                .map_err(|e| PullRejection::Internal(format!("attempt lookup failed: {e}")))?
                .map(|row| (exec_id, row))
        } else if let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) {
            self.db
                .find_open_pull_attempt_by_drv_hash(intent)
                .await
                .map_err(|e| PullRejection::Internal(format!("attempt lookup failed: {e}")))?
        } else {
            None
        };

        let Some((exec_id, attempt)) = resolved else {
            // Pull-side no-attempt side effect first (the never-pulled
            // pod-death case): drop the intent's ICE-clear arm so a
            // death before the first pull cannot leave a stale
            // Pending-watch entry behind, regardless of whether the
            // report also carries a job name that routes below.
            if let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) {
                self.dispatched_cells.remove(intent);
            }
            // The no-attempt no-op arm: acknowledge, charge nothing,
            // and leave the (still-wanted) drv exactly as it is so the
            // spawn intent re-arms on the next controller poll.
            debug!(
                intent_id = ?identity.intent_id,
                job_name = ?identity.job_name,
                exec_id = ?identity.exec_id,
                ?reason,
                "ReportAttemptOutcome with no matching attempt acknowledged charge-free"
            );
            return Ok(AttemptResolution::Unresolved);
        };

        // The kind witness FIRST (merged_bug_146): a controller verdict
        // names a BUILD-lifecycle event — the controller deleting a
        // builder Job — and never consumes a store replica's open
        // materialization attempt. Acknowledged charge-free BEFORE the
        // AD2c fill below, so the fill can never stamp a builder node
        // onto a materialization exec row (the 084 CHECK makes that
        // unrepresentable; this arm makes it unreachable) and the
        // store's own outcome report stays the attempt's only consumer.
        let b = match &attempt {
            crate::db::open_attempts::AttemptRef::Materialization(_) => {
                debug!(
                    %exec_id,
                    drv_hash = %attempt.core().drv_hash,
                    ?reason,
                    "ReportAttemptOutcome for a materialization attempt acknowledged \
                     charge-free (controller verdicts never consume store attempts)"
                );
                return Ok(AttemptResolution::Unresolved);
            }
            crate::db::open_attempts::AttemptRef::Build(b) => b,
        };

        if b.core.attempt_terminal {
            // Duplicate / already established: idempotent no-op — but
            // the attempt HAS a recorded terminal classification, so
            // the scheduler holds a verdict (matched-already-recorded
            // is Resolved per the C-2 iff).
            return Ok(AttemptResolution::Resolved);
        }

        // AD2c: persist the controller-reported node onto the open
        // execution row when the mint lost the binding-ack race
        // (NULL-only fill, best-effort), so a later establishment
        // charge carries the node key even when this report itself
        // classifies nothing. Never a worker-supplied value — the
        // node here comes from the controller's informer view.
        // §4.R4 conjunction (second toucher): the fill runs iff the
        // report resolved as a BUILD witness (the match above) AND
        // named the execution directly (exec_id-resolved) — an
        // intent-resolved report is newest-open-wins and could stamp
        // the node onto an execution the controller never observed.
        if exec_resolved
            && let Some(node) = node_name.as_deref().filter(|s| !s.is_empty())
            && let Err(e) = self.db.fill_open_execution_source_node(exec_id, node).await
        {
            debug!(%exec_id, error = %e,
                   "open-execution source_node fill failed (best-effort)");
        }

        let label = attempt_terminal_reason_label(reason);
        if b.core.attempt_recorded {
            // Second installment on the worker-reported row: fill the
            // termination reason only — never a reclassification, never
            // a new row, never a budget or floor change.
            let derivation_id = b.core.derivation_id;
            let won = self
                .db
                .fill_termination_reason_only(
                    derivation_id,
                    exec_id,
                    label,
                    node_name.as_deref(),
                    self.serving_generation(),
                )
                .await
                .map_err(|e| PullRejection::Internal(format!("installment fill failed: {e}")))?
                .applied();
            let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
            if won {
                if let Some(state) = self.dag.node_mut(&drv_hash) {
                    state.fill_attempt_termination_reason(exec_id, label, node_name.as_deref());
                }
                // A won fill on an attempt whose derivation is still
                // in-flight on this attempt means no other observer has
                // resolved it yet — requeue now (the pod is gone), and
                // record the pod-terminal requeue interval.
                let still_in_flight = self.dag.node(&drv_hash).is_some_and(|s| {
                    matches!(
                        s.status(),
                        DerivationStatus::Assigned | DerivationStatus::Running
                    ) && s.exec_id == Some(exec_id)
                });
                if still_in_flight {
                    // OA1 interval, pod-terminal cause: the attempt's
                    // classifying observation -> this requeue.
                    let observed_at = self
                        .dag
                        .node(&drv_hash)
                        .and_then(|s| {
                            s.attempt_history()
                                .iter()
                                .rev()
                                .find(|r| r.exec_id == Some(exec_id))
                                .map(|r| r.occurred_at_epoch_secs)
                        })
                        .unwrap_or_else(crate::db::attempts::epoch_now);
                    metrics::histogram!(
                        "rio_scheduler_attempt_requeue_seconds",
                        "cause" => "pod-terminal"
                    )
                    .record((crate::db::attempts::epoch_now() - observed_at).max(0.0));
                    let executor = ExecutorId::from(b.core.executor_id.as_str());
                    self.reassign_derivations(std::slice::from_ref(&drv_hash), Some(&executor))
                        .await;
                }
            }
            // attempt_recorded: a worker-reported classification row
            // exists (this report filled — or another report already
            // filled — the termination reason on it). The scheduler
            // holds a verdict for the attempt either way: Resolved.
            return Ok(AttemptResolution::Resolved);
        }

        // Open attempt with no classification row yet (pulled, never
        // worker-reported, pod now terminal). The split:
        //
        //   - Controller-synthesized verdicts (cancelled / preempted /
        //     reaped — the AD5/C5/C6 synthesize-on-delete and
        //     DisruptionTarget arms) classify HERE: the controller is
        //     deleting (or has deleted) the Job, so no other observer
        //     will ever report this attempt — EXCEPT the controller's
        //     own PRIOR pod-terminal classification, recorded as a
        //     witnessed mark below. A synthesized verdict over a
        //     recorded witnessed mark is a Job-gone handshake, never
        //     new evidence: it ESTABLISHES the attempt via the
        //     witnessed reason synchronously (sh-039 wall B — the
        //     terminal-absent reap at strikes=2 ≈ 10s otherwise
        //     pre-empted the witnessed slack=120s and closed
        //     charge-free as `disconnected`/`reaped`, so the floor
        //     never moved and the budget never charged). Absent a
        //     mark (genuine spot-kill / node-loss), the close is
        //     charge-free and the still-wanted derivation requeues at
        //     this fold, never at the establishment sweep.
        //   - Pod-terminal reasons without a worker row (OOM, eviction,
        //     deadline, plain error) keep waiting: the establishment
        //     sweep stays their classifier (the 1b gate text), because
        //     the worker's own classifying report may still arrive.
        // r[impl sched.attempt.synthesized-verdict+4]
        use rio_proto::types::AttemptTerminalReason as R;
        if b.core.assignment_active && matches!(reason, R::Cancelled | R::Preempted | R::Reaped) {
            // sh-039 (revised): a recorded witnessed mark means the
            // controller already classified this attempt (the
            // pod-terminal letter); the synthesized verdict is the
            // Job-gone handshake, and the controller deletes the Job
            // UNCONDITIONALLY on return
            // (`delete_job_with_synthesized_report`). The earlier
            // defer-to-mark shape (age the mark, return `Unresolved`)
            // left the only establishment trigger in volatile memory
            // while the Job was being deleted — a leader restart in
            // the ~5s window re-created the deadline-anchor pathology
            // sh-039 set out to fix. Establish synchronously instead:
            // the durable row commits BEFORE the controller deletes
            // the Job (`establish_from_witnessed` runs the
            // establishment sweep's C2 charge arm + the
            // witnessed-disposition floor bump inline), the mark is
            // consumed, and `Resolved` returns truthfully.
            if let Some(mark) = self.witnessed_terminal.get(&exec_id).copied() {
                self.establish_from_witnessed(b, exec_id, mark.reason, node_name)
                    .await;
                return Ok(AttemptResolution::Resolved);
            }
            let drv_hash = DrvHash::from(b.core.drv_hash.as_str());
            // AD2c: prefer the controller-reported node from this very
            // report, fall back to the spawn-ack binding; never a
            // worker-supplied value.
            let source_node = node_name
                .clone()
                .or_else(|| self.pull_attempt_source_node(&drv_hash));
            info!(
                %exec_id,
                drv_hash = %b.core.drv_hash,
                ?reason,
                "controller-synthesized verdict for an open pull attempt: closing charge-free"
            );
            self.close_pull_attempt_uncharged(
                b,
                exec_id,
                label,
                crate::state::ReportingParty::Controller,
                source_node,
                "synthesized",
            )
            .await;
            // Charge-free in the BUDGET sense, but it closes an actual
            // attempt: the scheduler now holds the verdict — Resolved.
            return Ok(AttemptResolution::Resolved);
        }
        // r[impl sched.attempt.witnessed-terminal+3]
        // live_058-c: the witnessed-terminal mark. The pod is GONE
        // (controller-witnessed terminal) while the attempt holds no
        // classification row — the worker's own report can only still
        // be IN FLIGHT, never future — so the establishment sweep may
        // act at `witnessed_at + establishment_report_slack` instead
        // of dead-waiting the dispatch deadline. First-witnessed-wins:
        // the controller re-reports level-triggered every tick while
        // the pod stays listable, and advancing the clock on each
        // re-report would defer establishment indefinitely; a
        // re-report re-creates an absent mark (the post-failover
        // re-arm) and otherwise changes nothing. The mark bumps,
        // charges, and classifies NOTHING here — the sweep owns the
        // action.
        let mark =
            self.witnessed_terminal
                .entry(exec_id)
                .or_insert_with(|| super::WitnessedTerminal {
                    witnessed_at: crate::db::attempts::epoch_now(),
                    reason,
                });
        debug!(
            %exec_id,
            drv_hash = %b.core.drv_hash,
            ?reason,
            witnessed_at = mark.witnessed_at,
            "ReportAttemptOutcome for an unclassified open attempt acknowledged (no fill \
             target; witnessed-terminal mark recorded — the establishment sweep acts at \
             witnessed_at + slack)"
        );
        Ok(AttemptResolution::Unresolved)
    }

    // r[impl sched.dispatch.fleet-exhaust+5]
    /// The spawn-gate exhaustion arm of `ReportAttemptOutcome`
    /// (`reason = NoEligibleSource`, AD2a): the controller holds the
    /// node informers, so it is the party that can observe "every
    /// source this intent could be scheduled onto is already excluded".
    /// The scheduler maps that observation to the same fleet-exhaust
    /// poison the dispatch-time E9 backstop produces: a `fleet_exhaust`
    /// marker row (no charge — the fold treats it as a no-op event)
    /// appended in the same transaction as the poison persist, then the
    /// cascade. Idempotent: only a currently-Ready derivation is acted
    /// on — an already-poisoned (or in-flight, or terminal, or unknown)
    /// drv acknowledges and changes nothing, so controller re-ticks and
    /// duplicate reports are no-ops.
    /// Resolution disposition (C-2 contract): every arm of this fn —
    /// including the poison arm — answers `Unresolved`/false. There is
    /// no pod and no attempt (the spawn-gate verdict maps to the
    /// fleet-exhaust poison, never an attempt classification), and the
    /// controller's NoEligibleSource reset lane rides its own mint,
    /// independent of the `attempt_resolved` bit.
    async fn handle_no_eligible_source(
        &mut self,
        identity: &AttemptIdentity,
        resubmit_cycle: u32,
    ) -> Result<AttemptResolution, PullRejection> {
        let Some(intent) = identity.intent_id.as_deref().filter(|s| !s.is_empty()) else {
            debug!(
                job_name = ?identity.job_name,
                "NoEligibleSource report without an intent id acknowledged (nothing to act on)"
            );
            return Ok(AttemptResolution::Unresolved);
        };
        let drv_hash = DrvHash::from(intent);
        let status = self.dag.node(&drv_hash).map(|s| s.status());
        if status != Some(DerivationStatus::Ready) {
            debug!(
                intent_id = %intent,
                ?status,
                "NoEligibleSource for a non-Ready derivation acknowledged (already resolved, \
                 in flight, or unknown)"
            );
            return Ok(AttemptResolution::Unresolved);
        }
        // 124(d) verdict guards — every miss acknowledges WITHOUT
        // poisoning (the controller re-evaluates next tick; a genuine
        // exhaustion re-reports and passes).
        if let Some(state) = self.dag.node(&drv_hash) {
            // (i) Scope: no failed builders ⇒ the wire carried no
            // exclusions ⇒ nothing can be exhausted. The verdict raced
            // an exclusion clear (resubmit reset, recovery rebuild).
            if state.retry.failed_builders.is_empty() {
                debug!(
                    intent_id = %intent,
                    "NoEligibleSource for a derivation with no failed builders acknowledged \
                     (nothing is excluded — stale or raced verdict)"
                );
                return Ok(AttemptResolution::Unresolved);
            }
            // (ii) Staleness: the echoed cycle must match the cycle the
            // verdict was computed against. A mismatch means the
            // derivation re-entered Ready (resubmit) since the
            // controller polled — its exclusion set was rebuilt.
            if resubmit_cycle != state.retry.resubmit_cycles {
                debug!(
                    intent_id = %intent,
                    echoed = resubmit_cycle,
                    current = state.retry.resubmit_cycles,
                    "NoEligibleSource with a stale resubmit-cycle echo acknowledged"
                );
                return Ok(AttemptResolution::Unresolved);
            }
        }
        // (iii) Spawn race: the controller acked creating a Job for
        // this intent within the defer window — the gate evaluated a
        // tick where that Job did not exist yet. Defer; if the spawn
        // genuinely cannot place, the gate re-fires after the window.
        if let Some(acked_at) = self.acked_spawned.get(&drv_hash)
            && crate::db::attempts::epoch_now() - acked_at < ACKED_SPAWNED_DEFER_SECS
        {
            debug!(
                intent_id = %intent,
                "NoEligibleSource within the spawn-ack defer window acknowledged (verdict \
                 raced its own spawn)"
            );
            return Ok(AttemptResolution::Unresolved);
        }
        if let Some(state) = self.dag.node(&drv_hash) {
            warn!(
                intent_id = %intent,
                system = %state.system,
                excluded = state.retry.failed_builders.len(),
                "controller reported NoEligibleSource: every spawnable source for this \
                 derivation is excluded; poisoning (AD2 spawn-gate fleet exhaust)"
            );
        }
        metrics::counter!("rio_scheduler_poison_fleet_exhausted_total").increment(1);
        // Same marker-row discipline as the dispatch-time arm: a verdict
        // marker is not an execution, so the execution/executor/node
        // attribution is cleared before the append.
        let marker = self
            .attempt_row_for(
                &drv_hash,
                crate::state::OutcomeClass::FleetExhaust,
                crate::state::ReportingParty::Controller,
            )
            .map(|mut row| {
                row.exec_id = None;
                row.executor_id = None;
                row.source_node = None;
                row
            });
        self.poison_and_cascade(
            &drv_hash,
            "no eligible source: every spawnable node is excluded for this derivation \
             (controller spawn gate)",
            None,
            marker,
            // THE defect lane (bug_080): \"no pod and no attempt\" — any
            // log a drv-named lookup resolves is a PRIOR execution's;
            // state NoExecution so the gateway never prints it.
            rio_proto::VerdictBacking::NoExecution,
        )
        .await;
        // The poison is a derivation-level verdict, not an attempt
        // resolution (the marker row carries exec_id=None) — the bit
        // stays false on this arm too.
        Ok(AttemptResolution::Unresolved)
    }
}
