//! The intent-decided candidate axis set — ONE projection consumed by
//! render, gate, and pack.
//!
//! merged_bug_124/126/249's shared mechanism was three consumers each
//! re-deriving a *subset* of the axes the pod render actually stamps:
//! the spawn gate read exclusions against a Ready-only node list (over-
//! fire), FFD packing ignored exclusions entirely (under-mint), and the
//! drift fingerprint covered placement only (silent drift on exclusion/
//! resource/deadline re-solves). `RenderInputs` is the complete axis
//! set, constructed once per intent; every destructive verdict that
//! depends on "where can this intent run / what did we render" goes
//! through it. A future axis added to the render but not here fails
//! the field-sensitivity contract test (`fingerprint` covers every
//! field by construction).
// r[impl ctrl.pool.intent-candidate-set]

use std::collections::BTreeMap;

use rio_proto::types::SpawnIntent;

/// A node as seen by the spawn gate: name + labels + cordon state.
/// NotReady nodes are deliberately KEPT (merged_bug_124(a)): a
/// NotReady-but-schedulable node is a provisioning/boot transient that
/// will host pods shortly — counting it spawnable prevents the gate
/// from manufacturing fleet-exhaustion out of a boot window. Cordoned
/// (`spec.unschedulable`) nodes are real non-candidates: the
/// kube-scheduler will never bind there.
#[derive(Debug, Clone)]
pub(crate) struct CandidateNode {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub schedulable: bool,
}

/// The COMPLETE intent-decided axis set the pod render stamps.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RenderInputs {
    /// `apply_intent_resources`' nodeSelector merge input.
    pub node_selector: BTreeMap<String, String>,
    /// §13a OR-of-ANDs placement terms (proto form; render converts).
    pub node_affinity: Vec<rio_proto::types::NodeSelectorTerm>,
    /// AD2 exclusion set, sorted (render: `NotIn` anti-affinity).
    pub excluded_nodes: Vec<String>,
    pub cores: u32,
    pub mem_bytes: u64,
    pub disk_bytes: u64,
    /// `ephemeral_deadline(intent)` — the value `activeDeadlineSeconds`
    /// is rendered from (the floor-180 form, NOT the raw proto field).
    pub deadline_secs: i64,
}

impl RenderInputs {
    pub(crate) fn from_intent(intent: &SpawnIntent) -> Self {
        let mut excluded = intent.excluded_nodes.clone();
        excluded.sort_unstable();
        excluded.dedup();
        Self {
            node_selector: intent.node_selector.clone().into_iter().collect(),
            node_affinity: intent.node_affinity.clone(),
            excluded_nodes: excluded,
            cores: intent.cores,
            mem_bytes: intent.mem_bytes,
            disk_bytes: intent.disk_bytes,
            deadline_secs: super::jobs::ephemeral_deadline(intent),
        }
    }

    /// Stable fingerprint over EVERY field. Replaces the placement-only
    /// `selector_fingerprint` (merged_bug_249): exclusion growth,
    /// resource re-solves, and deadline re-solves now drift the stamped
    /// annotation, so `reap_stale_for_intents`' drift arm replaces the
    /// Pending Job instead of letting it bind under stale inputs.
    ///
    /// Format is versioned by construction (`v2;` prefix): every legacy
    /// `selector_fingerprint` annotation differs from every v2 value,
    /// so the first post-deploy tick drift-reaps Pending Jobs exactly
    /// once (foreground delete + NameCollision-safe respawn; Running
    /// Jobs untouched — documented one-time churn).
    pub(crate) fn fingerprint(&self) -> String {
        let sel: Vec<String> = self
            .node_selector
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let mut terms: Vec<String> = self
            .node_affinity
            .iter()
            .map(|t| {
                let mut kv: Vec<_> = t
                    .match_expressions
                    .iter()
                    .map(|r| format!("{}~{}={}", r.operator, r.key, r.values.join("+")))
                    .collect();
                kv.sort_unstable();
                kv.join("|")
            })
            .collect();
        terms.sort_unstable();
        format!(
            "v2;sel={};aff={};exc={};res={},{},{};dl={}",
            sel.join(","),
            terms.join(";"),
            self.excluded_nodes.join(","),
            self.cores,
            self.mem_bytes,
            self.disk_bytes,
            self.deadline_secs,
        )
    }

    /// Would the kube-scheduler consider `node` for this intent's pod,
    /// IGNORING the exclusion axis: nodeSelector ⊆ labels ∧ (affinity
    /// OR-of-ANDs admits). The gate's "pre" universe.
    ///
    /// Operator semantics: `In`/`NotIn`/`Exists`/`DoesNotExist` exact;
    /// `Gt`/`Lt`/unknown ADMIT (fail-open — the gate must never *prove*
    /// exhaustion through an operator it cannot evaluate).
    pub(crate) fn admits_ignoring_exclusion(&self, labels: &BTreeMap<String, String>) -> bool {
        if self
            .node_selector
            .iter()
            .any(|(k, v)| labels.get(k) != Some(v))
        {
            return false;
        }
        if self.node_affinity.is_empty() {
            return true;
        }
        self.node_affinity.iter().any(|term| {
            // Kubernetes contract (bug_156): a term with empty
            // matchExpressions matches NO objects — terms are OR'd, so
            // all-empty-terms admits nothing and only a NIL affinity
            // (no terms at all, the early return above) admits all.
            // Mixed lists OR over the non-empty terms.
            !term.match_expressions.is_empty()
                && term.match_expressions.iter().all(|r| {
                    let have = labels.get(&r.key);
                    match r.operator.as_str() {
                        "In" => have.is_some_and(|v| r.values.contains(v)),
                        "NotIn" => have.is_none_or(|v| !r.values.contains(v)),
                        "Exists" => have.is_some(),
                        "DoesNotExist" => have.is_none(),
                        _ => true,
                    }
                })
        })
    }

    /// Full admission: pre-universe ∧ name ∉ excluded.
    pub(crate) fn admits(&self, node_name: &str, labels: &BTreeMap<String, String>) -> bool {
        !self.excluded(node_name) && self.admits_ignoring_exclusion(labels)
    }

    pub(crate) fn excluded(&self, node_name: &str) -> bool {
        self.excluded_nodes.iter().any(|n| n == node_name)
    }
}

/// The ONE exclusion predicate (FFD + mint backstop + gate share it).
pub(crate) fn node_excluded(intent: &SpawnIntent, node_name: &str) -> bool {
    intent.excluded_nodes.iter().any(|n| n == node_name)
}

/// AD2 spawn-gate exhaustion over the candidate set (merged_bug_124):
/// `pre` = schedulable nodes the intent admits ignoring exclusion
/// (NotReady KEPT); `effective` = `pre` minus excluded. Exhausted iff
/// the intent carries exclusions, the pre-universe is non-empty (an
/// empty pre-universe is a provisioning transient — autoscaling may
/// mint an admitting node; mirrors `placeable()`'s empty-fleet defer),
/// and the exclusions consume it entirely. Intent affinity narrows the
/// universe (124(c)): a node the intent could never run on does not
/// veto exhaustion.
// r[impl sched.dispatch.fleet-exhaust+5]
pub(crate) fn no_eligible_source(intent: &SpawnIntent, candidates: &[CandidateNode]) -> bool {
    if intent.excluded_nodes.is_empty() {
        return false;
    }
    let ri = RenderInputs::from_intent(intent);
    let mut pre_nonempty = false;
    for c in candidates {
        if !c.schedulable || !ri.admits_ignoring_exclusion(&c.labels) {
            continue;
        }
        pre_nonempty = true;
        if ri.admits(&c.name, &c.labels) {
            // A fully-admitted (schedulable, matching, non-excluded)
            // node exists — the universe is not exhausted.
            return false;
        }
    }
    pre_nonempty
}

/// N-tick persistence for the exhaustion verdict (merged_bug_124(a)):
/// a single-tick observation (node restart, informer lag, autoscaler
/// churn) must not poison a derivation. The gate withholds the spawn
/// from tick 1 (the Job would sit unschedulable behind its own
/// anti-affinity anyway) but only REPORTS --- i.e. poisons --- once the
/// exhaustion has persisted `NO_ELIGIBLE_SOURCE_PERSIST_TICKS`
/// consecutive OBSERVED ticks AND `NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS`
/// of wall clock (merged_bug_073: reconciles are event-driven --- a
/// Job-event burst can deliver 3 ticks in under a second, so the count
/// alone never carried the documented ~30s persistence).
// r[impl ctrl.pool.no-eligible-persist+5]
pub(crate) const NO_ELIGIBLE_SOURCE_PERSIST_TICKS: u32 = 3;

/// Wall-clock floor for the irreversible report (merged_bug_073). At
/// the 10s steady-state requeue cadence three consecutive ticks span
/// 20s, so the floor adds no latency on the steady path; an event
/// burst (4 reconciles <1s) is structurally below it.
// r[impl ctrl.pool.no-eligible-persist+5]
pub(crate) const NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS: u64 = 20;

/// One streak-map update for one gated tick. Returns the new streak
/// and whether the COUNT half of the firing law is met.
/// [`PoolStreaks::step`] is the only caller in the reconcile path and
/// adds the wall-clock-floor conjunct (merged_bug_073).
pub(crate) fn exhausted_streak_step(prev: Option<u32>) -> (u32, bool) {
    let streak = prev.unwrap_or(0).saturating_add(1);
    (streak, streak >= NO_ELIGIBLE_SOURCE_PERSIST_TICKS)
}

/// Orphaned [`PoolStreaks`] entries (a pool removed from config stops
/// reconciling, so its entries are never pruned by its own tick)
/// expire after this long. Today's global wipe accidentally GC'd them;
/// the expiry replaces that accident with a law. Applies to the
/// per-pool sequence table too (merged_bug_073: `pool_seq` previously
/// grew forever).
pub(crate) const POOL_STREAK_ORPHAN_EXPIRY_SECS: u64 = 120;

/// Streak identity: `{namespace}/{name}` (merged_bug_073 --- bare pool
/// names collide across namespaces, pooling two pools' streaks). The
/// only constructor takes both halves, so a bare-name key is
/// untypeable at every consumer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolKey(String);

impl PoolKey {
    pub fn new(namespace: &str, name: &str) -> Self {
        Self(format!("{namespace}/{name}"))
    }
}

impl std::fmt::Display for PoolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Witness that [`PoolStreaks::begin_tick`] ran for this pool THIS
/// reconcile (bug_069). [`PoolStreaks::step`] consumes it, so stepping
/// a streak without entering through the per-reconcile protocol does
/// not typecheck. merged_bug_073 re-divides the labor: minting the
/// witness no longer mutates --- a reconcile whose gate fold is skipped
/// (no wanted intents, node LIST failure) simply drops the witness and
/// the pool's streaks RETAIN WITHOUT STEPPING (an unobserved tick is
/// not evidence in either direction); the documented ~30s persistence
/// is carried by the wall-clock floor. bug_028 narrows recovery to the
/// EVALUATED set: a completed fold breaks a streak only for an intent
/// it evaluated whose gated set no longer contains it --- the fold's
/// [`Observation`] carries evaluated and gated as one value, so
/// "absent from gated but never evaluated" (headroom-truncated,
/// job-pending, or absent from the stream) is unrepresentable as
/// recovery. Zero headroom no longer skips the fold: the gate
/// evaluates every wanted intent independent of the spawn window.
#[must_use = "the streak tick must reach the gate fold's step (or be dropped) --- never stored"]
pub struct StreakTick {
    key: PoolKey,
}

/// One completed gate fold's view of the pool's wanted intents
/// (bug_028): `evaluated = gated ∪ spawnable` BY CONSTRUCTION --- the
/// sole constructor takes the partition the fold just computed, so a
/// gated set outside the evaluated set is unrepresentable, and
/// [`PoolStreaks::step`]'s prune can only read "evaluated and no
/// longer gated" as recovery. An intent absent from BOTH halves was
/// not evaluated this fold and retains without stepping (the
/// per-intent extension of the witness-drop law). Every consumer
/// (the streak retain, the step loop, the respawn-record touch law)
/// reads the same instance.
pub struct Observation {
    gated: std::collections::HashSet<String>,
    spawnable: std::collections::HashSet<String>,
}

impl Observation {
    /// Mint from the gate partition over the FULL existing-names-
    /// filtered wanted set. The production fold is the only caller
    /// shape; tests construct through the same partition over real
    /// [`SpawnIntent`]s, never hand-assembled id sets.
    pub fn from_partition(gated: &[SpawnIntent], spawnable: &[SpawnIntent]) -> Self {
        Self {
            gated: gated.iter().map(|i| i.intent_id.clone()).collect(),
            spawnable: spawnable.iter().map(|i| i.intent_id.clone()).collect(),
        }
    }

    /// The fold looked at this intent (either half of the partition).
    pub fn evaluated(&self, id: &str) -> bool {
        self.gated.contains(id) || self.spawnable.contains(id)
    }

    /// The fold gated this intent (exhaustion verdict this tick).
    pub fn gated(&self, id: &str) -> bool {
        self.gated.contains(id)
    }
}

/// bug_028 futility breaker: the CLOSED alphabet of named resolutions
/// that reset an intent's verdict-free-respawn record --- nothing else
/// resets one (besides the orphan expiry). The step's per-cell law
/// RETAINS the record on every evaluated tick, gated or not: during
/// backoff the intent is wanted, evaluated, and un-gated EVERY tick
/// (the breaker's steady state), and spawnability is NOT evidence of
/// recovery --- only a named resolution is. The record fields are
/// private and the only mutators are
/// [`PoolStreaks::note_verdict_free_death`],
/// [`PoolStreaks::note_resolution`], and the step's expiry --- and
/// `note_resolution` requires a [`VerdictWitness`] (merged_bug_080(2b)),
/// so a reset path that bypasses this alphabet, OR names a member
/// without holding its evidence, does not typecheck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnResolution {
    /// A terminal `ReportAttemptOutcome` for the intent was acked with
    /// `attempt_resolved=true` (controller-synthesized at delete,
    /// pod-terminal fill, or deadline-exceeded fill) --- the scheduler
    /// RESOLVED an attempt with it. A charge-free ack
    /// (`attempt_resolved=false`: no matching attempt, exec-less
    /// synthesized refusal, materialization-kind refusal, stale cycle)
    /// is NOT this member --- an empty `Ok` cannot witness its own
    /// premise (merged_bug_080(2b)).
    TerminalReport,
    /// A `NoEligibleSource` poison verdict for the intent was ACKED.
    /// The ack itself is the premise: the spawn-gate verdict rides its
    /// own poison lane scheduler-side, never an attempt resolution, so
    /// every `NoEligibleSource` ack carries `attempt_resolved=false`
    /// BY DESIGN (the WO-S3-P arm census) --- the bit is deliberately
    /// not consulted here.
    NoEligibleSource,
    /// An open BUILD pull-mode attempt was observed covering the
    /// intent --- the pull established; the death cycle ended in a
    /// worker actually starting.
    Established,
    /// A recently-closed BUILD attempt for the intent was observed in
    /// the ledger view --- the scheduler holds an adjudicated verdict
    /// for a worker-closed death (the close outran the controller's
    /// terminal reap). merged_bug_036: the wire age is baked into the
    /// witness at mint, so every consumer can bind the chronology
    /// conjunct (adjudication evidence covers only events it
    /// postdates) at its own premise --- the conjunct is unskippable
    /// once the variant carries the age.
    ClosedBuild {
        /// `ClosedAttempt.closed_age_secs` at mint time.
        closed_age_secs: u64,
    },
}

// r[impl ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2b): the verdict-evidence witness
/// [`PoolStreaks::note_resolution`] requires. Constructible ONLY
/// through the four mint constructors below, each demanding its typed
/// premise (the field is private), so every reset call site is forced
/// to hold scheduler-verdict evidence at compile time and
/// `rg -n 'note_resolution' rio-controller/src` is the complete
/// reset-lane census --- the compiler generates it.
///
/// The four mints, one per [`SpawnResolution`] member:
/// 1. [`Self::from_resolved_ack`] --- an ack carrying
///    `attempt_resolved=true`.
/// 2. [`Self::from_acked_no_eligible_source`] --- a `NoEligibleSource`
///    ack (the response value proves the RPC completed; the bit is
///    false by design on every poison arm).
/// 3. [`Self::from_open_build_attempt`] --- an open attempt the minted
///    identity classifier (`MintedPullIdentity::of`, job.rs) says is
///    the intent's own BUILD pull.
/// 4. [`Self::from_recently_closed_build`] --- a recently-closed entry
///    whose `attempt_kind` is explicitly BUILD (raw-i32 compare;
///    UNSPECIFIED and MATERIALIZATION do not mint --- fail-closed for
///    a spend-enabling lane).
#[derive(Debug, Clone, Copy)]
pub struct VerdictWitness(SpawnResolution);

impl VerdictWitness {
    /// Mint 1: a `ReportAttemptOutcome` ack that RESOLVED an attempt.
    /// `None` on a charge-free ack --- the caller has nothing to reset
    /// with (merged_bug_080(2b): the same-tick wipe died here; a
    /// deadline report for a never-pulled Job acks
    /// `attempt_resolved=false` and resets nothing).
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn from_resolved_ack(
        resp: &rio_proto::types::ReportAttemptOutcomeResponse,
    ) -> Option<Self> {
        resp.attempt_resolved
            .then_some(Self(SpawnResolution::TerminalReport))
    }

    /// Mint 2: an acked `NoEligibleSource` report. Taking the response
    /// value (not consulting its bit) is deliberate: the premise is
    /// "the scheduler acked the poison verdict" --- every poison arm
    /// returns `attempt_resolved=false` by design (the verdict is not
    /// an attempt resolution), and requiring the response makes the
    /// mint unreachable before the RPC completes.
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn from_acked_no_eligible_source(
        _ack: &rio_proto::types::ReportAttemptOutcomeResponse,
    ) -> Self {
        Self(SpawnResolution::NoEligibleSource)
    }

    /// Mint 3: an open attempt that is the intent's own minted BUILD
    /// pull identity. `None` for materialization claims, foreign
    /// shapes, and unknown kinds --- a store replica's attempt is not
    /// build progress and never resets.
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn from_open_build_attempt(attempt: &rio_proto::types::OpenAttempt) -> Option<Self> {
        (super::job::MintedPullIdentity::of(attempt) == super::job::MintedPullIdentity::Build)
            .then_some(Self(SpawnResolution::Established))
    }

    /// Mint 4: a recently-closed BUILD attempt. Raw-i32 compare
    /// against the explicit BUILD value: UNSPECIFIED and
    /// MATERIALIZATION DO NOT mint --- fail-closed for a spend-enabling
    /// lane. This is the deliberate INVERSE of `MintedPullIdentity`'s
    /// UNSPECIFIED-reads-as-Build REPORT posture (RULED S2-OQ4,
    /// 2026-06-09): a report attribution must be total over the
    /// rolling-skew window, while enabling respawn spend on a
    /// pre-field scheduler's default would convert skew into money ---
    /// do not "fix" either site to match the other (the cross-reference
    /// lives at both).
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn from_recently_closed_build(closed: &rio_proto::types::ClosedAttempt) -> Option<Self> {
        (closed.attempt_kind == rio_proto::types::AttemptKind::Build as i32).then_some(Self(
            SpawnResolution::ClosedBuild {
                closed_age_secs: closed.closed_age_secs,
            },
        ))
    }

    /// Mint 4b (merged_bug_036): a recently-closed BUILD attempt that
    /// POSTDATES the reaped Job's creation --- the chronology conjunct
    /// lives inside the mint, where its premise (the Job in hand) is.
    /// A close minted before the Job existed categorically cannot be
    /// that Job's attempt's verdict, so it covers nothing. Same
    /// PG↔apiserver clock pair, same slack const, and the same
    /// fail-toward-counting inequality direction as
    /// `CancelTarget::bind` conjunct 3 (the sibling that always had
    /// this conjunct). The reap-mask consumes THIS mint; the
    /// windowed-reset lane keeps mint 4 and binds its chronology at
    /// the record ([`PoolStreaks::note_resolution`]).
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn covers_job_death(
        closed: &rio_proto::types::ClosedAttempt,
        job: &k8s_openapi::api::batch::v1::Job,
    ) -> Option<Self> {
        let kind_is_build = closed.attempt_kind == rio_proto::types::AttemptKind::Build as i32;
        let postdates_job = super::job::job_older_than(
            job,
            std::time::Duration::from_secs(
                closed.closed_age_secs + super::job::CANCEL_CLOSE_SKEW_SLACK_SECS,
            ),
        );
        (kind_is_build && postdates_job).then_some(Self(SpawnResolution::ClosedBuild {
            closed_age_secs: closed.closed_age_secs,
        }))
    }
}

/// bug_075: the per-intent evidence-state alphabet the spawn decision
/// is total over — the second axis of the (universe arm × evidence
/// state) decision table. The wave-5 bug_028 close typed the fold-skip
/// alphabet ([`super::jobs::GateUniverse`]) but never typed each arm's
/// SPAWN effect on evidence-carrying intents, so the ListFailed
/// fail-open spawned a mid-streak intent into a Job that made it
/// structurally unobservable past the orphan expiry — destroying by
/// spawning exactly what the retain law preserved. Every spawn arm now
/// consults this alphabet through [`PoolStreaks::evidence_state`]; the
/// census test walks the full product through the production fold.
///
/// Classification precedence (the states are derived from two
/// underlying records that can coexist): a respawn record inside its
/// backoff floor dominates (the backoff withholds on EVERY arm via the
/// universal post-arm filter), then streak liveness by the SAME
/// staleness bound as the retain law ([`POOL_STREAK_ORPHAN_EXPIRY_SECS`]
/// — one constant, no new time axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvidenceState {
    /// No streak entry and no in-backoff respawn record: fail-open
    /// spawn is safe — there is no evidence to destroy.
    NoEvidence,
    /// A streak entry younger than the orphan window: live exhaustion
    /// evidence — a fold-skip spawn would orphan it behind the Job's
    /// ≥180 s alive floor (> the 120 s expiry), so the skip arm
    /// withholds.
    LiveStreak,
    /// A streak entry at/past the orphan window: dead evidence by the
    /// merged_bug_073 staleness law — the intent re-qualifies for
    /// fail-open (persistent LIST failure cannot wedge spawn forever).
    StaleStreak,
    /// A respawn record inside its backoff floor: withheld on every
    /// arm by the universal [`PoolStreaks::respawn_blocked`] filter.
    InBackoff,
}

impl EvidenceState {
    /// Census alphabet (R15 generator: this array is pinned exhaustive
    /// by the same-file match in [`Self::label`] — a new variant fails
    /// that match until it is added HERE, and the census test walks
    /// this array so the new cells must be stated). Test-only consumer
    /// BY DESIGN; production code matches the closed enum exhaustively
    /// (the fold-skip arm in `jobs.rs`) instead of iterating it.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) const ALL: [EvidenceState; 4] = [
        EvidenceState::NoEvidence,
        EvidenceState::LiveStreak,
        EvidenceState::StaleStreak,
        EvidenceState::InBackoff,
    ];

    /// Census-cell label — and the [`Self::ALL`] compile pin: the
    /// exhaustive match makes a new variant a compile error at this
    /// line until the array above (and every census cell) names it.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn label(self) -> &'static str {
        match self {
            EvidenceState::NoEvidence => "no-evidence",
            EvidenceState::LiveStreak => "live-streak",
            EvidenceState::StaleStreak => "stale-streak",
            EvidenceState::InBackoff => "in-backoff",
        }
    }
}

/// Exponential floor cap for the verdict-free-respawn backoff
/// (bug_028 futility breaker). Schedule: `JOB_REQUEUE × 2^(deaths-1)`
/// seconds, capped here --- 10, 20, 40, 80, 80, ... The base is the
/// reconcile cadence (the as-built respawn rate: live exhibit
/// 2026-06-09, 759 intents respawned 2-8x with ZERO NoEligibleSource
/// verdicts over 41 min --- every reconcile re-spawned a same-named
/// Job for an intent whose every prior Job died without a verdict,
/// converting a TOTAL builder failure into EC2 spend at tick rate).
///
/// The cap MUST stay below [`POOL_STREAK_ORPHAN_EXPIRY_SECS`] (FS-6
/// clamp, asserted at compile time below): the record's `touched`
/// refreshes only on evaluated ticks, so a cap above the expiry would
/// let a record orphan-expire MID-BACKOFF and silently reset the
/// respawn cadence to ~the expiry regardless of this const. 80 s =
/// three doublings of the 10 s cadence --- an order-of-magnitude
/// spend reduction under total failure while a genuine verdict (the
/// reset lane) restores full cadence immediately.
// r[impl ctrl.pool.respawn-backoff+2]
pub(crate) const RESPAWN_BACKOFF_CAP_SECS: u64 = 80;
const _: () = assert!(
    RESPAWN_BACKOFF_CAP_SECS < POOL_STREAK_ORPHAN_EXPIRY_SECS,
    "FS-6: a backoff cap at/above the orphan expiry silently clamps at the expiry"
);

/// The backoff floor after `deaths` verdict-free Job deaths.
fn respawn_backoff(deaths: u32) -> std::time::Duration {
    let base = super::job::JOB_REQUEUE.as_secs();
    let shifted = base
        .checked_shl(deaths.saturating_sub(1).min(32))
        .unwrap_or(u64::MAX);
    std::time::Duration::from_secs(shifted.min(RESPAWN_BACKOFF_CAP_SECS))
}

/// merged_bug_117: pool-keyed exhaustion streaks. The retired shape was
/// a pool-SHARED `HashMap<intent, u32>` whose per-pool reconcile did
/// `retain(|id| this_pools_gated.contains(id))` --- pool B's tick wiped
/// pool A's streaks, and an intent gated in two overlapping pools
/// double-stepped per wall-clock tick. The key is `(PoolKey, intent)`
/// (merged_bug_073: namespace-qualified --- same-named pools in two
/// namespaces are distinct), and the only streak mutator is
/// [`Self::step`], scoped to one pool's completed fold --- a cross-pool
/// wipe remains INEXPRESSIBLE through the API.
///
/// bug_069 (amended by merged_bug_073, re-divided by bug_028): a
/// skipped fold (witness dropped) advances nothing and breaks nothing
/// --- the retired begin-tick prune conflated "we did not look" with
/// "we looked and it recovered". bug_028 extends that law PER INTENT:
/// the retired retain dropped any entry absent from the fold's gated
/// set, but the gated set was computed over the headroom-truncated
/// WINDOW, so an intent merely pushed past the cutoff by priority
/// churn read as recovered and the firing law never held on a
/// ceiling'd pool (the poison report that would un-wedge the
/// scheduler was livelocked). The prune input is now the fold's
/// [`Observation`] --- drop iff EVALUATED and no longer gated
/// (observed recovery) or expired; un-evaluated entries retain
/// without stepping. The retired `last_seq`/`pool_seq` adjacency
/// defense is REMOVED: legitimate per-intent evaluation gaps make
/// fold-sequence adjacency wrong (it would silently re-introduce the
/// wipe), and its staleness role is already carried by the
/// `touched`-based expiry plus the `first_gated` floor (m073's "two
/// stale observations plus one fresh blip" argument holds unchanged).
/// The firing law is `streak >= NO_ELIGIBLE_SOURCE_PERSIST_TICKS` AND
/// `now - first_gated >= NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS`: the
/// count proves repeated observation, the floor proves duration, and
/// observed recovery (or `POOL_STREAK_ORPHAN_EXPIRY_SECS` of silence)
/// is the only reset.
///
/// bug_028 futility breaker: `respawn` records (one per (pool,
/// intent) with at least one VERDICT-FREE Job death --- a terminal
/// Job reaped for a still-wanted intent with no acked synthesized
/// report) gate re-spawn behind the `respawn_backoff` schedule.
/// Reset alphabet:
/// [`SpawnResolution`] (closed) + the orphan expiry; the per-cell
/// retain law lives in [`Self::step`].
#[derive(Debug, Default)]
pub struct PoolStreaks {
    /// `(pool, intent) -> entry`; see [`StreakEntry`].
    entries: std::collections::HashMap<(PoolKey, String), StreakEntry>,
    /// `(pool, intent) -> verdict-free-respawn record` (the futility
    /// breaker); see [`RespawnEntry`].
    respawn: std::collections::HashMap<(PoolKey, String), RespawnEntry>,
}

#[derive(Debug)]
struct StreakEntry {
    streak: u32,
    /// When this UNBROKEN streak first observed the exhaustion --- the
    /// wall-clock-floor anchor (merged_bug_073).
    first_gated: std::time::Instant,
    touched: std::time::Instant,
}

/// One intent's verdict-free-respawn record (bug_028 futility
/// breaker). Fields private: mutation only through the typed
/// [`PoolStreaks`] API.
#[derive(Debug)]
struct RespawnEntry {
    /// Verdict-free Job deaths observed for this (pool, intent).
    deaths: u32,
    /// When the latest verdict-free death was observed --- the backoff
    /// anchor (the respawn becomes possible at that reap; the floor
    /// runs from it).
    last_death: std::time::Instant,
    /// Refreshed on every EVALUATED tick (per-cell law in
    /// [`PoolStreaks::step`]) AND by every live/terminal same-named
    /// Job in a tick's listing ([`PoolStreaks::note_job_alive`],
    /// merged_bug_080(2a) --- the record must survive the job-held
    /// phases, whose alive floors all exceed the expiry); the shared
    /// orphan expiry bounds the map. Only genuinely JOBLESS fold-skip
    /// silence ages it out --- the breaker fails open with the spawn
    /// arm's documented posture.
    touched: std::time::Instant,
}

impl PoolStreaks {
    // r[impl ctrl.pool.no-eligible-persist+5]
    /// Mint the per-reconcile witness. NON-MUTATING (merged_bug_073):
    /// a reconcile that never reaches the gate fold drops the witness
    /// and leaves every streak exactly as it found it.
    pub fn begin_tick(&self, pool: &PoolKey) -> StreakTick {
        StreakTick { key: pool.clone() }
    }

    // r[impl ctrl.pool.no-eligible-persist+5]
    /// Fold one pool's COMPLETED gate evaluation: expire stale entries
    /// and respawn records, apply the per-cell retain laws against the
    /// fold's [`Observation`], step each gated intent, and return the
    /// intent ids whose exhaustion satisfied BOTH halves of the firing
    /// law (count + wall-clock floor). Consumes the [`StreakTick`]:
    /// one step per reconcile, none without `begin_tick`.
    ///
    /// Per-cell laws, stated per evidence kind (they deliberately
    /// differ --- see [`SpawnResolution`]):
    ///   - streak entries: evaluated ∧ gated → step; evaluated ∧
    ///     un-gated → DROP (observed recovery); un-evaluated → retain
    ///     without stepping.
    ///   - respawn records: evaluated (gated or not) → RETAIN and
    ///     refresh `touched` (spawnability is not recovery; only a
    ///     named resolution resets); un-evaluated → retain without
    ///     refreshing HERE --- the job-held phases refresh through
    ///     [`Self::note_job_alive`] (merged_bug_080(2a)), so the
    ///     expiry only ever consumes jobless silence.
    pub fn step(
        &mut self,
        tick: StreakTick,
        obs: &Observation,
        now: std::time::Instant,
    ) -> Vec<String> {
        let StreakTick { key } = tick;
        // Staleness bound (merged_bug_073): with retain-without-step,
        // an entry unstepped for the expiry window is dead evidence
        // REGARDLESS of pool --- otherwise two stale observations plus
        // one fresh blip hours later could complete the poison.
        self.entries.retain(|_, e| {
            now.saturating_duration_since(e.touched).as_secs() < POOL_STREAK_ORPHAN_EXPIRY_SECS
        });
        // Respawn records: touch this pool's evaluated cells FIRST
        // (an evaluated record at the expiry boundary survives), then
        // expire by the same staleness law.
        for ((p, id), r) in self.respawn.iter_mut() {
            if p == &key && obs.evaluated(id) {
                r.touched = now;
            }
        }
        self.respawn.retain(|_, r| {
            now.saturating_duration_since(r.touched).as_secs() < POOL_STREAK_ORPHAN_EXPIRY_SECS
        });
        // bug_028 retain law: drop this pool's entry iff the fold
        // EVALUATED the intent and its gated set no longer contains it
        // --- observed recovery. Un-evaluated entries (headroom has no
        // bearing now, but job-pending and absent-from-stream intents
        // are still un-evaluated) retain without stepping.
        self.entries.retain(|(p, id), _| {
            let observed_recovery = obs.evaluated(id) && !obs.gated(id);
            p != &key || !observed_recovery
        });
        let mut report = Vec::new();
        for id in &obs.gated {
            let k = (key.clone(), id.clone());
            let prev = self.entries.get(&k).map(|e| (e.streak, e.first_gated));
            let (streak, count_ok) = exhausted_streak_step(prev.map(|(s, _)| s));
            let first_gated = prev.map_or(now, |(_, f)| f);
            self.entries.insert(
                k,
                StreakEntry {
                    streak,
                    first_gated,
                    touched: now,
                },
            );
            if count_ok
                && now.saturating_duration_since(first_gated).as_secs()
                    >= NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS
            {
                report.push(id.clone());
            }
        }
        report.sort();
        report
    }

    /// bug_028 futility breaker: record one VERDICT-FREE Job death ---
    /// a terminal Job reaped for a still-wanted intent whose deletion
    /// carried NO acked synthesized report (never pulled, or the
    /// report RPC failed; either way the scheduler holds no verdict
    /// and the same-named respawn would otherwise fire at reconcile
    /// cadence). Steps the record (deaths+1) and re-anchors the
    /// backoff at this observation.
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn note_verdict_free_death(
        &mut self,
        pool: &PoolKey,
        intent: &str,
        now: std::time::Instant,
    ) {
        let e = self
            .respawn
            .entry((pool.clone(), intent.to_owned()))
            .or_insert(RespawnEntry {
                deaths: 0,
                last_death: now,
                touched: now,
            });
        e.deaths = e.deaths.saturating_add(1);
        e.last_death = now;
        e.touched = now;
    }

    /// merged_bug_080(2a) structural refresh: a live or terminal
    /// same-named Job in this tick's listing is the observable
    /// artifact of the job-held cycle phases --- refresh the intents'
    /// respawn-record `touched` so the orphan expiry cannot destroy a
    /// record during a phase in which the intent is structurally
    /// unobservable to the gate fold (a job-held intent leaves
    /// `wanted` via the existing-name filter, so `step`'s
    /// evaluated-tick touch never reaches it, while every Job-alive
    /// floor --- the >=180 s deadline floor, the 600 s terminal TTL
    /// --- exceeds the 120 s expiry). Refreshes ONLY respawn records,
    /// NEVER streak entries: the merged_bug_073 staleness law for
    /// streaks is a different proposition --- a live Job is not an
    /// exhaustion observation, and a streak kept alive by its own
    /// spawn could complete a poison from stale evidence. With this
    /// lane the expiry reverts to its documented orphan semantics:
    /// genuinely JOBLESS fold-skip silence.
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn note_job_alive<'a>(
        &mut self,
        pool: &PoolKey,
        intents: impl Iterator<Item = &'a str>,
        now: std::time::Instant,
    ) {
        for intent in intents {
            if let Some(r) = self.respawn.get_mut(&(pool.clone(), intent.to_owned())) {
                r.touched = now;
            }
        }
    }

    /// bug_028 futility breaker: apply one NAMED resolution --- the
    /// ONLY reset lane (see [`SpawnResolution`]). Removes the record:
    /// the scheduler holds (or just produced) a verdict, so the next
    /// death cycle starts fresh and a healthy retry is never taxed.
    /// merged_bug_080(2b): the [`VerdictWitness`] parameter makes a
    /// premise-free reset a compile error --- every caller must mint
    /// the witness at one of the four evidence sites first.
    // r[impl ctrl.pool.respawn-backoff+2]
    /// merged_bug_036: the `ClosedBuild` arm binds the chronology
    /// conjunct at the record (the only place `last_death` lives) ---
    /// the record is removed IFF the close provably postdates the
    /// latest recorded death (`close_age + skew slack <
    /// now - last_death`), else RETAINED wholesale (deaths are not
    /// time-splittable; the over-caution residual is bounded by the
    /// backoff cap + orphan expiry, the same posture as the
    /// reap-mask's window residual). The other three variants reset
    /// unconditionally: their premises are current by construction (a
    /// just-completed ack RPC; a live open attempt). Remove-or-retain
    /// on chronology subsumes per-close idempotency: a consumed
    /// close's record is gone, and re-noting no-ops on the absent
    /// key.
    pub fn note_resolution(
        &mut self,
        pool: &PoolKey,
        intent: &str,
        witness: VerdictWitness,
        now: std::time::Instant,
    ) {
        let VerdictWitness(what) = witness;
        let key = (pool.clone(), intent.to_owned());
        if let SpawnResolution::ClosedBuild { closed_age_secs } = what
            && let Some(e) = self.respawn.get(&key)
        {
            let close_age = std::time::Duration::from_secs(
                closed_age_secs + super::job::CANCEL_CLOSE_SKEW_SLACK_SECS,
            );
            if close_age >= now.saturating_duration_since(e.last_death) {
                tracing::debug!(pool = %pool, intent, closed_age_secs,
                    "recently-closed verdict predates the latest recorded death; \
                     respawn record retained (merged_bug_036)");
                return;
            }
        }
        if self.respawn.remove(&key).is_some() {
            tracing::debug!(pool = %pool, intent, resolution = ?what,
                "verdict-free-respawn backoff reset by named resolution");
        }
    }

    /// bug_075: classify one intent's evidence state for the spawn
    /// decision — the typed query the fold-skip arm consults (and the
    /// census walks). Pure read. Streak liveness uses the SAME
    /// staleness bound as the retain law
    /// ([`POOL_STREAK_ORPHAN_EXPIRY_SECS`]): evidence the next `step`
    /// would expire is already dead for the spawn decision, so the
    /// withhold is self-limiting — a permanently failing node LIST
    /// restores fail-open once the entry goes stale.
    // r[impl ctrl.pool.no-eligible-persist+5]
    pub(crate) fn evidence_state(
        &self,
        pool: &PoolKey,
        intent: &str,
        now: std::time::Instant,
    ) -> EvidenceState {
        if self.respawn_blocked(pool, intent, now) {
            return EvidenceState::InBackoff;
        }
        match self.entries.get(&(pool.clone(), intent.to_owned())) {
            Some(e)
                if now.saturating_duration_since(e.touched).as_secs()
                    < POOL_STREAK_ORPHAN_EXPIRY_SECS =>
            {
                EvidenceState::LiveStreak
            }
            Some(_) => EvidenceState::StaleStreak,
            None => EvidenceState::NoEvidence,
        }
    }

    /// bug_028 futility breaker: is this intent's re-spawn currently
    /// gated behind the verdict-free backoff floor? Pure read --- the
    /// spawn arm consults it on EVERY tick (including fold-skip
    /// ticks, where deaths noted by the reap still gate immediately).
    // r[impl ctrl.pool.respawn-backoff+2]
    pub fn respawn_blocked(&self, pool: &PoolKey, intent: &str, now: std::time::Instant) -> bool {
        self.respawn
            .get(&(pool.clone(), intent.to_owned()))
            .is_some_and(|r| {
                now.saturating_duration_since(r.last_death) < respawn_backoff(r.deaths)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, labels: &[(&str, &str)], schedulable: bool) -> CandidateNode {
        CandidateNode {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            schedulable,
        }
    }

    fn intent(excluded: &[&str]) -> SpawnIntent {
        SpawnIntent {
            intent_id: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            excluded_nodes: excluded.iter().map(|s| (*s).to_owned()).collect(),
            cores: 4,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 31,
            ..Default::default()
        }
    }

    /// Same production shape as [`intent`], with a caller-chosen id
    /// (the streak tests need distinct intents; bug_028: observations
    /// are minted from real `SpawnIntent`s via `from_partition`, never
    /// hand-assembled id sets).
    fn intent_for(id: &str, excluded: &[&str]) -> SpawnIntent {
        SpawnIntent {
            intent_id: id.into(),
            ..intent(excluded)
        }
    }

    /// merged_bug_124(a): a schedulable-but-NotReady node counts toward
    /// the pre-universe, so excluding the only Ready node is NOT
    /// exhaustion. (Pre-fix: the Ready-only spawnable set made this
    /// fire — the recorded red.)
    #[test]
    fn gate_counts_schedulable_not_ready_nodes() {
        let i = intent(&["n1"]);
        let candidates = vec![
            node("n1", &[], true),
            node("n2", &[], true), // NotReady in k8s terms — still a candidate
        ];
        assert!(!no_eligible_source(&i, &candidates));
    }

    /// Cordoned nodes are NOT candidates: with the only other node
    /// excluded, exhaustion holds.
    #[test]
    fn gate_ignores_cordoned_nodes() {
        let i = intent(&["n1"]);
        let candidates = vec![node("n1", &[], true), node("n2", &[], false)];
        assert!(no_eligible_source(&i, &candidates));
    }

    /// merged_bug_124(c): intent affinity narrows the universe — a node
    /// the intent can never run on does not veto exhaustion.
    #[test]
    fn gate_universe_narrowed_by_affinity() {
        let mut i = intent(&["n1"]);
        i.node_affinity = vec![rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "hw".into(),
                operator: "In".into(),
                values: vec!["metal".into()],
            }],
        }];
        // n1 admits (hw=metal) but is excluded; n2 never admits.
        let candidates = vec![
            node("n1", &[("hw", "metal")], true),
            node("n2", &[("hw", "cloud")], true),
        ];
        assert!(no_eligible_source(&i, &candidates));
        // A second admitting node un-exhausts.
        let candidates = vec![
            node("n1", &[("hw", "metal")], true),
            node("n3", &[("hw", "metal")], true),
        ];
        assert!(!no_eligible_source(&i, &candidates));
    }

    /// Empty pre-universe defers (provisioning transient), and an
    /// exclusion-free intent is never exhausted.
    #[test]
    fn gate_defers_on_empty_universe_and_no_exclusions() {
        let i = intent(&["n1"]);
        assert!(!no_eligible_source(&i, &[]));
        let mut sel = intent(&["n1"]);
        sel.node_selector = [("zone".to_owned(), "z1".to_owned())].into_iter().collect();
        // Only node lacks the selector label: pre-universe empty.
        assert!(!no_eligible_source(&sel, &[node("n1", &[], true)]));
        assert!(!no_eligible_source(&intent(&[]), &[node("n1", &[], true)]));
    }

    /// Persistence: ticks 1-2 withhold silently, tick 3 reports.
    #[test]
    fn gate_reports_only_after_persistence() {
        let (s1, r1) = exhausted_streak_step(None);
        let (s2, r2) = exhausted_streak_step(Some(s1));
        let (s3, r3) = exhausted_streak_step(Some(s2));
        assert_eq!((s1, r1), (1, false));
        assert_eq!((s2, r2), (2, false));
        assert_eq!((s3, r3), (3, true));
    }

    /// merged_bug_249: every render-stamped axis drifts the
    /// fingerprint (the placement-only legacy form let exclusion/
    /// resource/deadline re-solves bind stale Pending Jobs).
    #[test]
    fn fingerprint_is_sensitive_to_every_field() {
        let base = RenderInputs::from_intent(&intent(&["n1"]));
        let fp = base.fingerprint();
        let mut variants = Vec::new();
        let mut v = base.clone();
        v.node_selector.insert("k".into(), "v".into());
        variants.push(v);
        let mut v = base.clone();
        v.node_affinity = vec![rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "hw".into(),
                operator: "In".into(),
                values: vec!["m".into()],
            }],
        }];
        variants.push(v);
        let mut v = base.clone();
        v.excluded_nodes.push("n9".into());
        variants.push(v);
        let mut v = base.clone();
        v.cores += 1;
        variants.push(v);
        let mut v = base.clone();
        v.mem_bytes += 1;
        variants.push(v);
        let mut v = base.clone();
        v.disk_bytes += 1;
        variants.push(v);
        let mut v = base.clone();
        v.deadline_secs += 1;
        variants.push(v);
        for variant in variants {
            assert_ne!(variant.fingerprint(), fp, "axis change must drift");
        }
        // Determinism over input ordering.
        let mut a = intent(&["b", "a"]);
        let mut b = intent(&["a", "b"]);
        a.node_selector = [
            ("x".to_owned(), "1".to_owned()),
            ("y".to_owned(), "2".to_owned()),
        ]
        .into_iter()
        .collect();
        b.node_selector = [
            ("y".to_owned(), "2".to_owned()),
            ("x".to_owned(), "1".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            RenderInputs::from_intent(&a).fingerprint(),
            RenderInputs::from_intent(&b).fingerprint()
        );
    }

    /// Three-way agreement: the gate's pre-universe and FFD's exclusion
    /// predicate are projections of the same admits() — an excluded
    /// node is never admitted, an admitted node is in the pre-universe.
    #[test]
    fn admit_agreement() {
        let i = intent(&["n1"]);
        let ri = RenderInputs::from_intent(&i);
        let labels = BTreeMap::new();
        assert!(ri.admits_ignoring_exclusion(&labels));
        assert!(!ri.admits("n1", &labels));
        assert!(ri.admits("n2", &labels));
        assert_eq!(node_excluded(&i, "n1"), ri.excluded("n1"));
        assert_eq!(node_excluded(&i, "n2"), ri.excluded("n2"));
    }

    /// merged_bug_073 red: a Job-event burst (4 reconciles inside one
    /// second) must NOT fire the irreversible report — the persistence
    /// contract is wall-clock (~30s documented), not reconcile-count.
    #[test]
    fn streak_fire_requires_wall_clock_floor() {
        let mut streaks = PoolStreaks::default();
        let key = PoolKey::new("rio-system", "metal");
        let now = std::time::Instant::now();
        let gated = [intent_for("i1", &["n1"])];
        let mut fired = false;
        for _ in 0..4 {
            let tick = streaks.begin_tick(&key);
            let obs = Observation::from_partition(&gated, &[]);
            fired |= !streaks.step(tick, &obs, now).is_empty();
        }
        assert!(!fired, "a sub-second reconcile burst must not fire");
        // The SAME streak, aged past the floor: fires on its next
        // observed tick (count long satisfied, floor now too).
        let aged = now + std::time::Duration::from_secs(NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS);
        let tick = streaks.begin_tick(&key);
        let obs = Observation::from_partition(&gated, &[]);
        assert_eq!(
            streaks.step(tick, &obs, aged),
            vec!["i1".to_owned()],
            "wall-clock-aged persistence must fire"
        );
    }

    /// merged_bug_073: a skipped fold (witness dropped) retains the
    /// streak without stepping --- the wall-clock floor, not fold
    /// reachability, carries the persistence guarantee.
    #[test]
    fn streak_retains_without_step_on_fold_skip() {
        let mut streaks = PoolStreaks::default();
        let key = PoolKey::new("rio-system", "metal");
        let t0 = std::time::Instant::now();
        let gated = [intent_for("i1", &["n1"])];
        for i in 0..2 {
            let tick = streaks.begin_tick(&key);
            let obs = Observation::from_partition(&gated, &[]);
            let _ = streaks.step(tick, &obs, t0 + std::time::Duration::from_secs(i * 10));
        }
        // Fold skipped this reconcile: witness minted then dropped.
        let dropped = streaks.begin_tick(&key);
        drop(dropped);
        // Next completed fold continues the streak (3rd observation)
        // and the floor (20s elapsed) is met: fires.
        let t3 = t0 + std::time::Duration::from_secs(20);
        let tick = streaks.begin_tick(&key);
        let obs = Observation::from_partition(&gated, &[]);
        assert_eq!(
            streaks.step(tick, &obs, t3),
            vec!["i1".to_owned()],
            "a skipped fold must not break an aged streak"
        );
    }

    /// merged_bug_073 red: same-named pools in two namespaces must not
    /// share streak state (bare-name keying collides cross-namespace).
    #[test]
    fn streak_keying_is_namespace_scoped() {
        let mut streaks = PoolStreaks::default();
        let now = std::time::Instant::now();
        let gated = [intent_for("i1", &["n1"])];
        // ns-a/metal ticks twice, ns-b/metal once: if keys collide the
        // single logical pool sees interleaved sequences.
        let ka = PoolKey::new("ns-a", "metal");
        let kb = PoolKey::new("ns-b", "metal");
        let t = streaks.begin_tick(&ka); // ns-a tick 1
        streaks.step(t, &Observation::from_partition(&gated, &[]), now);
        let t = streaks.begin_tick(&kb); // ns-b tick 1 (distinct key)
        let r = streaks.step(t, &Observation::from_partition(&gated, &[]), now);
        // Under colliding keys the second pool CONTINUES ns-a's streak
        // (streak 2 of a pool that observed once). Distinct keys would
        // leave both at streak 1 and this stays empty at tick 3 below
        // only if pools are independent.
        let t = streaks.begin_tick(&ka);
        let r3 = streaks.step(t, &Observation::from_partition(&gated, &[]), now);
        assert!(
            r.is_empty() && r3.is_empty(),
            "two pools' interleaved ticks must not pool one streak: r={r:?} r3={r3:?}"
        );
    }
}

/// C2 formal-delta proptest plane 2: the candidate-set equivalence
/// laws. The keystone claim of the area-A refactor is that the gate,
/// the render, and the pack all project from ONE `RenderInputs`
/// universe — these properties pin the algebra of that universe
/// against a mirror specification, over arbitrary intents and fleets.
// r[verify ctrl.pool.intent-candidate-set]
// r[verify ctrl.pool.no-eligible-persist+5]
#[cfg(test)]
mod proptests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};

    use super::*;

    const KEYS: [&str; 3] = ["arch", "cap", "zone"];
    const VALS: [&str; 2] = ["x", "y"];
    const NODES: [&str; 4] = ["n0", "n1", "n2", "n3"];
    const OPS: [&str; 5] = ["In", "NotIn", "Exists", "DoesNotExist", "Gt"];

    fn arb_labels() -> impl Strategy<Value = BTreeMap<String, String>> {
        proptest::collection::btree_map(
            proptest::sample::select(&KEYS[..]).prop_map(str::to_owned),
            proptest::sample::select(&VALS[..]).prop_map(str::to_owned),
            0..=3,
        )
    }

    fn arb_nodes() -> impl Strategy<Value = Vec<CandidateNode>> {
        proptest::collection::vec(
            (
                proptest::sample::select(&NODES[..]),
                arb_labels(),
                any::<bool>(),
            )
                .prop_map(|(name, labels, schedulable)| CandidateNode {
                    name: name.to_owned(),
                    labels,
                    schedulable,
                }),
            0..=4,
        )
    }

    fn arb_term() -> impl Strategy<Value = NodeSelectorTerm> {
        proptest::collection::vec(
            (
                proptest::sample::select(&KEYS[..]),
                proptest::sample::select(&OPS[..]),
                proptest::collection::vec(
                    proptest::sample::select(&VALS[..]).prop_map(str::to_owned),
                    0..=2,
                ),
            )
                .prop_map(|(key, operator, values)| NodeSelectorRequirement {
                    key: key.to_owned(),
                    operator: operator.to_owned(),
                    values,
                }),
            0..=2,
        )
        .prop_map(|match_expressions| NodeSelectorTerm { match_expressions })
    }

    fn arb_intent() -> impl Strategy<Value = SpawnIntent> {
        (
            proptest::collection::btree_map(
                proptest::sample::select(&KEYS[..]).prop_map(str::to_owned),
                proptest::sample::select(&VALS[..]).prop_map(str::to_owned),
                0..=2,
            ),
            proptest::collection::vec(arb_term(), 0..=2),
            proptest::collection::vec(
                proptest::sample::select(&NODES[..]).prop_map(str::to_owned),
                0..=4,
            ),
            1u32..=8,
            1u64..=1u64 << 33,
            0u32..=7200,
        )
            .prop_map(
                |(node_selector, node_affinity, excluded_nodes, cores, mem_bytes, deadline)| {
                    SpawnIntent {
                        intent_id: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
                        node_selector: node_selector.into_iter().collect(),
                        node_affinity,
                        excluded_nodes,
                        cores,
                        mem_bytes,
                        disk_bytes: mem_bytes / 2,
                        deadline_secs: deadline,
                        ..Default::default()
                    }
                },
            )
    }

    proptest! {
        /// Law 1 (the single-universe conjunction): full admission is
        /// EXACTLY pre-universe membership minus the exclusion axis —
        /// the gate, the render's anti-affinity, and the pack can never
        /// disagree about a node.
        #[test]
        fn admits_is_pre_minus_exclusion(intent in arb_intent(), nodes in arb_nodes()) {
            let ri = RenderInputs::from_intent(&intent);
            for n in &nodes {
                prop_assert_eq!(
                    ri.admits(&n.name, &n.labels),
                    ri.admits_ignoring_exclusion(&n.labels) && !ri.excluded(&n.name)
                );
                prop_assert_eq!(node_excluded(&intent, &n.name), ri.excluded(&n.name));
            }
        }

        /// Law 2 (exhaustion ≡ excluded-consumed-pre-universe): the AD2
        /// verdict is true iff exclusions exist, some schedulable node
        /// matches ignoring exclusion, and NO node is fully admitted —
        /// pinned against an independent fold over the same universe.
        #[test]
        fn exhaustion_matches_mirror(intent in arb_intent(), nodes in arb_nodes()) {
            let ri = RenderInputs::from_intent(&intent);
            let pre: Vec<&CandidateNode> = nodes
                .iter()
                .filter(|n| n.schedulable && ri.admits_ignoring_exclusion(&n.labels))
                .collect();
            let expected = !intent.excluded_nodes.is_empty()
                && !pre.is_empty()
                && pre.iter().all(|n| ri.excluded(&n.name));
            prop_assert_eq!(no_eligible_source(&intent, &nodes), expected);
        }

        /// Law 3 (fingerprint determinism + axis sensitivity): equal
        /// inputs render equal fingerprints; growing the exclusion set
        /// or re-solving the deadline drifts the fingerprint (the
        /// drift-reap trigger merged_bug_249 depends on). The deadline
        /// axis is sensitive in its RENDERED form: `ephemeral_deadline`
        /// floors at 180 s, so sub-floor re-solves correctly do NOT
        /// drift (this property originally asserted raw-field
        /// sensitivity and the plane falsified it — the floor is the
        /// law).
        #[test]
        fn fingerprint_deterministic_and_axis_sensitive(intent in arb_intent()) {
            let a = RenderInputs::from_intent(&intent);
            let b = RenderInputs::from_intent(&intent);
            prop_assert_eq!(a.fingerprint(), b.fingerprint());

            let mut grown = intent.clone();
            grown.excluded_nodes.push("zz-not-a-node".into());
            prop_assert_ne!(
                RenderInputs::from_intent(&grown).fingerprint(),
                a.fingerprint()
            );

            // Above the floor every re-solve drifts: 7300 exceeds both
            // the 180 floor and the strategy's 7200 cap, so the
            // rendered deadline always differs from the base's.
            let mut redl = intent.clone();
            redl.deadline_secs = 7300;
            prop_assert_ne!(
                RenderInputs::from_intent(&redl).fingerprint(),
                a.fingerprint()
            );

            // And BELOW the floor, re-solves are render-equivalent:
            // the fingerprint is the rendered tuple, not the raw wire.
            let mut sub_a = intent.clone();
            sub_a.deadline_secs = 0;
            let mut sub_b = intent.clone();
            sub_b.deadline_secs = 179;
            prop_assert_eq!(
                RenderInputs::from_intent(&sub_a).fingerprint(),
                RenderInputs::from_intent(&sub_b).fingerprint()
            );
        }

        /// Law 4 (persistence threshold): the streak fires iff it has
        /// reached NO_ELIGIBLE_SOURCE_PERSIST_TICKS, and a reset (None)
        /// always restarts at one-not-firing.
        #[test]
        fn streak_fires_only_at_threshold(prev in proptest::option::of(0u32..=10)) {
            let (streak, fire) = exhausted_streak_step(prev);
            prop_assert_eq!(streak, prev.unwrap_or(0).saturating_add(1));
            prop_assert_eq!(fire, streak >= NO_ELIGIBLE_SOURCE_PERSIST_TICKS);
            let (s1, f1) = exhausted_streak_step(None);
            prop_assert_eq!((s1, f1), (1, false));
        }
    }

    /// One OBSERVED pool tick under the bug_069 witness API: begin +
    /// step over an [`Observation`] minted from the fold's partition
    /// (bug_028: real `SpawnIntent`s, never hand-assembled id sets).
    fn observed_tick(
        s: &mut PoolStreaks,
        pool: &PoolKey,
        gated: &[SpawnIntent],
        spawnable: &[SpawnIntent],
        t: std::time::Instant,
    ) -> Vec<String> {
        let tick = s.begin_tick(pool);
        let obs = Observation::from_partition(gated, spawnable);
        s.step(tick, &obs, t)
    }

    /// A gated production-shaped intent with a caller-chosen id.
    fn gated_intent(id: &str) -> SpawnIntent {
        SpawnIntent {
            intent_id: id.into(),
            excluded_nodes: vec!["n1".into()],
            cores: 4,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 31,
            ..Default::default()
        }
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// merged_bug_117 law 1 (recorded red: the retired pool-shared map's
    /// `retain(|id| gated_B.contains(id))` wiped pool A's streaks every
    /// B tick — a persistently exhausted intent in a multi-pool config
    /// NEVER reported): interleaved pool ticks each keep their own
    /// persistence count.
    #[test]
    fn cross_pool_ticks_do_not_wipe_each_other() {
        let mut s = PoolStreaks::default();
        let ka = PoolKey::new("ns", "pool-a");
        let kb = PoolKey::new("ns", "pool-b");
        let t = std::time::Instant::now();
        let a_gated = [gated_intent("drv-a")];
        let b_gated = [gated_intent("drv-b")];
        // A and B alternate at the 10s cadence; A's third own tick
        // (20s elapsed: count AND floor met) must report.
        assert!(observed_tick(&mut s, &ka, &a_gated, &[], t).is_empty());
        assert!(observed_tick(&mut s, &kb, &b_gated, &[], t).is_empty());
        let t10 = t + std::time::Duration::from_secs(10);
        assert!(observed_tick(&mut s, &ka, &a_gated, &[], t10).is_empty());
        assert!(observed_tick(&mut s, &kb, &b_gated, &[], t10).is_empty());
        let t20 = t + std::time::Duration::from_secs(20);
        assert_eq!(
            observed_tick(&mut s, &ka, &a_gated, &[], t20),
            vec!["drv-a".to_string()],
            "pool A's third consecutive own observation must report"
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// merged_bug_117 law 2 (recorded red: an intent gated in two
    /// overlapping pools double-stepped the shared entry — reported
    /// after 2 wall-clock ticks instead of each pool's own 3): each
    /// pool counts its OWN observations.
    #[test]
    fn overlapping_pools_count_their_own_observations() {
        let mut s = PoolStreaks::default();
        let ka = PoolKey::new("ns", "pool-a");
        let kb = PoolKey::new("ns", "pool-b");
        let t = std::time::Instant::now();
        let gated = [gated_intent("drv-x")];
        // Two wall-clock rounds of both pools at the 10s cadence:
        // 4 steps total, but no single pool has seen 3 yet.
        for i in 0..2u64 {
            let ti = t + std::time::Duration::from_secs(i * 10);
            assert!(observed_tick(&mut s, &ka, &gated, &[], ti).is_empty());
            assert!(observed_tick(&mut s, &kb, &gated, &[], ti).is_empty());
        }
        // Each pool's own third observation (20s elapsed) reports
        // independently.
        let t20 = t + std::time::Duration::from_secs(20);
        assert_eq!(
            observed_tick(&mut s, &ka, &gated, &[], t20),
            vec!["drv-x".to_string()]
        );
        assert_eq!(
            observed_tick(&mut s, &kb, &gated, &[], t20),
            vec!["drv-x".to_string()]
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// Orphan expiry: a pool removed from config never ticks again —
    /// its entries expire at POOL_STREAK_ORPHAN_EXPIRY_SECS instead of
    /// living forever (the retired global wipe GC'd them by accident;
    /// the law replaces the accident). Observable: A's streak restarts
    /// after the gap instead of resuming.
    #[test]
    fn orphaned_pool_entries_expire() {
        let mut s = PoolStreaks::default();
        let ka = PoolKey::new("ns", "pool-a");
        let kb = PoolKey::new("ns", "pool-b");
        let t0 = std::time::Instant::now();
        let gated = [gated_intent("drv-a")];
        let other = [gated_intent("drv-b")];
        assert!(observed_tick(&mut s, &ka, &gated, &[], t0).is_empty());
        assert!(
            observed_tick(
                &mut s,
                &ka,
                &gated,
                &[],
                t0 + std::time::Duration::from_secs(10)
            )
            .is_empty()
        ); // streak 2
        // pool-a removed from config; only pool-b ticks, past the expiry.
        let late = t0 + std::time::Duration::from_secs(POOL_STREAK_ORPHAN_EXPIRY_SECS + 11);
        assert!(observed_tick(&mut s, &kb, &other, &[], late).is_empty());
        // pool-a re-added: its old streak expired, so this is tick 1, not 3.
        assert!(
            observed_tick(&mut s, &ka, &gated, &[], late).is_empty(),
            "an expired orphan streak must restart, not resume at 3"
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// merged_bug_073 (amends bug_069): a skipped fold retains the
    /// streak WITHOUT stepping --- but the wall-clock floor still
    /// blocks a burst that interleaves skips at the same instant. The
    /// retired law broke the streak on every skip, which conflated
    /// "did not look" with "looked and it recovered".
    #[test]
    fn skipped_fold_retains_streak_floor_blocks_burst() {
        let mut s = PoolStreaks::default();
        let key = PoolKey::new("rio-system", "pool-a");
        let gated = [gated_intent("drv-x")];
        let t = std::time::Instant::now();
        assert!(observed_tick(&mut s, &key, &gated, &[], t).is_empty()); // observed 1
        assert!(observed_tick(&mut s, &key, &gated, &[], t).is_empty()); // observed 2
        // Reconcile 3: the gate fold is skipped --- the witness is
        // minted (top of reconcile, unconditional) and dropped. The
        // streak RETAINS at 2.
        drop(s.begin_tick(&key));
        // Observed again at the same instant: count reaches 3 but the
        // wall-clock floor is unmet --- no fire.
        let fired = observed_tick(&mut s, &key, &gated, &[], t);
        assert!(
            fired.is_empty(),
            "count satisfied inside the floor fired the irreversible poison: {fired:?}"
        );
        // Aged past the floor, the retained streak fires on its next
        // observation --- the skip did not destroy real persistence.
        let aged = t + std::time::Duration::from_secs(NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS);
        assert_eq!(
            observed_tick(&mut s, &key, &gated, &[], aged),
            vec!["drv-x".to_owned()]
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// merged_bug_073, staleness bound: a streak frozen for hours (no
    /// completed folds) is dead evidence --- the orphan expiry applies
    /// to the OWN pool's unstepped entries too, so one isolated blip
    /// hours later starts over instead of completing the poison. Fresh
    /// consecutive observations spanning the floor still fire.
    #[test]
    fn single_pool_frozen_streak_never_resumes() {
        let mut s = PoolStreaks::default();
        let key = PoolKey::new("rio-system", "pool-a");
        let gated = [gated_intent("drv-x")];
        let t = std::time::Instant::now();
        assert!(observed_tick(&mut s, &key, &gated, &[], t).is_empty());
        assert!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                t + std::time::Duration::from_secs(10)
            )
            .is_empty()
        ); // streak 2
        // Hours where the fold never runs (node LIST failures ---
        // bug_028: a pool at ceiling now folds; only no-wanted and
        // LIST-failure ticks skip): witnesses minted and dropped.
        for _ in 0..360 {
            drop(s.begin_tick(&key));
        }
        // One isolated re-observation hours later: the frozen entry
        // expired (unstepped past POOL_STREAK_ORPHAN_EXPIRY_SECS), so
        // this starts a FRESH streak --- no fire.
        let late = t + std::time::Duration::from_secs(3600);
        assert!(
            observed_tick(&mut s, &key, &gated, &[], late).is_empty(),
            "an isolated observation after a frozen streak must not poison"
        );
        // Genuine persistence from here: two more observations, the
        // third spanning the floor --- fires.
        assert!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                late + std::time::Duration::from_secs(10)
            )
            .is_empty()
        );
        assert_eq!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                late + std::time::Duration::from_secs(NO_ELIGIBLE_SOURCE_PERSIST_FLOOR_SECS)
            ),
            vec!["drv-x".to_owned()]
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// bug_028 red (recorded against the retired gated-ids API, where
    /// truncation absence was only expressible as absence-from-gated
    /// --- that inexpressibility WAS the bug; the API migration is the
    /// disclosed strawman): an intent a completed fold NEVER EVALUATED
    /// must retain its streak. Folds 1-2 gate i1; fold 3 completes
    /// WITHOUT i1 in its evaluated set (the churn tick: a higher-
    /// priority intent filled the window pre-fix --- post-fix, e.g.
    /// the intent's Job is briefly Pending); fold 4 gates i1 at
    /// t0+20s: count (3 observations) and floor (20s) met --- fires.
    /// Recorded red (pre-fix): `left: [] right: ["i1"] --- a fold that
    /// never evaluated the intent reset its streak`.
    #[test]
    fn truncated_intent_retains_streak_across_folds() {
        let mut s = PoolStreaks::default();
        let key = PoolKey::new("rio-system", "pool-a");
        let t0 = std::time::Instant::now();
        let gated = [gated_intent("i1")];
        // Folds 1-2: i1 gated (t0, t0+10s).
        assert!(observed_tick(&mut s, &key, &gated, &[], t0).is_empty());
        assert!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                t0 + std::time::Duration::from_secs(10)
            )
            .is_empty()
        );
        // Fold 3 COMPLETES without i1 evaluated: i2 is gated, i1 is in
        // NEITHER half of the partition --- unobserved this fold.
        let churn = [gated_intent("i2")];
        assert!(
            observed_tick(
                &mut s,
                &key,
                &churn,
                &[],
                t0 + std::time::Duration::from_secs(11)
            )
            .is_empty()
        );
        // Fold 4 gates i1 again at t0+20s: count (3 observations) and
        // floor (20s) are both met --- must fire.
        assert_eq!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                t0 + std::time::Duration::from_secs(20)
            ),
            vec!["i1".to_owned()],
            "a fold that never evaluated the intent reset its streak"
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+5]
    /// bug_028 companion pin: OBSERVED recovery still resets --- a
    /// completed fold that EVALUATED the intent into its spawnable
    /// half (universe un-exhausted) drops the streak, so the next
    /// gated run starts at 1 and cannot fire inside the count window.
    #[test]
    fn evaluated_ungated_intent_resets_streak() {
        let mut s = PoolStreaks::default();
        let key = PoolKey::new("rio-system", "pool-a");
        let t0 = std::time::Instant::now();
        let gated = [gated_intent("i1")];
        let recovered = [gated_intent("i1")];
        assert!(observed_tick(&mut s, &key, &gated, &[], t0).is_empty());
        assert!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                t0 + std::time::Duration::from_secs(10)
            )
            .is_empty()
        );
        // Fold 3: i1 EVALUATED and spawnable --- observed recovery.
        assert!(
            observed_tick(
                &mut s,
                &key,
                &[],
                &recovered,
                t0 + std::time::Duration::from_secs(20)
            )
            .is_empty()
        );
        // Folds 4-5: gated again --- a fresh streak (1, 2); even past
        // the original floor, the count restarts and must not fire.
        assert!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                t0 + std::time::Duration::from_secs(30)
            )
            .is_empty()
        );
        assert!(
            observed_tick(
                &mut s,
                &key,
                &gated,
                &[],
                t0 + std::time::Duration::from_secs(40)
            )
            .is_empty(),
            "observed recovery must reset the count --- two post-recovery \
             observations are not three"
        );
    }

    /// bug_156 red: a NodeSelectorTerm with empty matchExpressions
    /// matches NO objects under the Kubernetes contract (terms are
    /// ORed; an empty term is "matches nothing", only a NIL affinity
    /// admits all). Today the empty `iter().all()` makes it admit ALL.
    #[test]
    fn empty_term_admits_nothing_per_the_kube_contract() {
        let mut intent = rio_proto::types::SpawnIntent {
            node_affinity: vec![rio_proto::types::NodeSelectorTerm {
                match_expressions: vec![],
            }],
            ..Default::default()
        };
        let ri = RenderInputs::from_intent(&intent);
        let labels: std::collections::BTreeMap<String, String> =
            [("zone".to_string(), "a".to_string())].into();
        assert!(
            !ri.admits_ignoring_exclusion(&labels),
            "an all-empty-terms affinity admits nothing (kube semantics); \
             only NIL affinity admits all"
        );
        // Mixed: one empty term + one matching term = OR over the
        // non-empty term only -> admits.
        intent
            .node_affinity
            .push(rio_proto::types::NodeSelectorTerm {
                match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                    key: "zone".into(),
                    operator: "In".into(),
                    values: vec!["a".into()],
                }],
            });
        let ri = RenderInputs::from_intent(&intent);
        assert!(ri.admits_ignoring_exclusion(&labels));
        // NIL affinity (no terms at all) admits all - unchanged.
        let nil = RenderInputs::from_intent(&rio_proto::types::SpawnIntent::default());
        assert!(nil.admits_ignoring_exclusion(&labels));
    }

    /// bug_156: the Kubernetes nodeAffinity conformance fixture table —
    /// expected verdicts TRANSCRIBED from the upstream contract
    /// (`requiredDuringSchedulingIgnoredDuringExecution` /
    /// `NodeSelectorTerm`: terms are OR'd; matchExpressions within a
    /// term are AND'd; an empty/nil term matches no objects; a nil
    /// affinity matches all; NotIn/DoesNotExist match when the key is
    /// absent), NOT derived from the implementation (the retired
    /// `mirror_pre` restated the impl — both wrong together on the
    /// empty-term row). The one deliberate divergence is an explicit
    /// EXPECTED-DIVERGENCE row.
    #[test]
    fn k8s_nodeaffinity_conformance() {
        use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};
        fn term(reqs: &[(&str, &str, &[&str])]) -> NodeSelectorTerm {
            NodeSelectorTerm {
                match_expressions: reqs
                    .iter()
                    .map(|(k, op, vs)| NodeSelectorRequirement {
                        key: (*k).into(),
                        operator: (*op).into(),
                        values: vs.iter().map(|v| (*v).into()).collect(),
                    })
                    .collect(),
            }
        }
        let labels: std::collections::BTreeMap<String, String> = [
            ("zone".to_string(), "a".to_string()),
            ("disk".to_string(), "ssd".to_string()),
        ]
        .into();

        // (name, terms, expected-admits, kube-divergence?)
        struct Row {
            name: &'static str,
            terms: Vec<NodeSelectorTerm>,
            admits: bool,
            expected_divergence: Option<&'static str>,
        }
        let rows = vec![
            Row {
                name: "nil affinity admits all",
                terms: vec![],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "single empty term matches nothing",
                terms: vec![term(&[])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "all-empty terms match nothing",
                terms: vec![term(&[]), term(&[])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "mixed: OR over the non-empty term (matching)",
                terms: vec![term(&[]), term(&[("zone", "In", &["a"])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "mixed: OR over the non-empty term (non-matching)",
                terms: vec![term(&[]), term(&[("zone", "In", &["b"])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "In: value present",
                terms: vec![term(&[("zone", "In", &["a", "b"])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "In: value absent",
                terms: vec![term(&[("zone", "In", &["b"])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "In: key missing",
                terms: vec![term(&[("region", "In", &["a"])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "NotIn: value not in set",
                terms: vec![term(&[("zone", "NotIn", &["b"])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "NotIn: value in set",
                terms: vec![term(&[("zone", "NotIn", &["a"])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "NotIn: key missing matches",
                terms: vec![term(&[("region", "NotIn", &["a"])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "Exists: key present",
                terms: vec![term(&[("disk", "Exists", &[])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "Exists: key missing",
                terms: vec![term(&[("gpu", "Exists", &[])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "DoesNotExist: key missing",
                terms: vec![term(&[("gpu", "DoesNotExist", &[])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "DoesNotExist: key present",
                terms: vec![term(&[("disk", "DoesNotExist", &[])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "AND within a term: both hold",
                terms: vec![term(&[("zone", "In", &["a"]), ("disk", "In", &["ssd"])])],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "AND within a term: one fails",
                terms: vec![term(&[("zone", "In", &["a"]), ("disk", "In", &["hdd"])])],
                admits: false,
                expected_divergence: None,
            },
            Row {
                name: "OR across terms: second holds",
                terms: vec![
                    term(&[("zone", "In", &["b"])]),
                    term(&[("disk", "In", &["ssd"])]),
                ],
                admits: true,
                expected_divergence: None,
            },
            Row {
                name: "unknown operator fails open",
                terms: vec![term(&[("zone", "Gt", &["0"])])],
                admits: true,
                expected_divergence: Some(
                    "kube evaluates Gt/Lt numerically (here: 'a' > '0' would error → not admitted); \
                     the gate fails OPEN by design — it must never PROVE exhaustion through an \
                     operator it cannot evaluate (admitting keeps the spawn, the safe direction)",
                ),
            },
        ];
        for row in rows {
            let intent = SpawnIntent {
                node_affinity: row.terms.clone(),
                ..Default::default()
            };
            let ri = RenderInputs::from_intent(&intent);
            assert_eq!(
                ri.admits_ignoring_exclusion(&labels),
                row.admits,
                "conformance row failed: {} (expected divergence: {:?})",
                row.name,
                row.expected_divergence,
            );
        }
    }
}
