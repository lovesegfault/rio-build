//! Materialization-job actor logic — the substitution mechanism.
//! Unconditional since the substitution-replacement cutover.
//! Design: substitution-replacement-design.md §2; spec:
//! sched.materialize.{job,routing,pinning}.
// r[impl sched.materialize.job+2]

use tokio::sync::oneshot;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::db::materialization::FencedJobCreate;
use crate::state::{DerivationStatus, DrvHash, ExecutorId, JobOrigin};

use super::DagActor;

/// What `ListMaterializationJobs` returns per job (the proto
/// descriptor's actor-side source).
#[derive(Debug, Clone)]
pub struct JobDescriptor {
    /// `materialization_jobs.job_id`.
    pub job_id: Uuid,
    /// The derivation hash (the DAG key / claim intent).
    pub drv_hash: String,
    /// Creating build's tenant; `None` = no tenant context (the
    /// executor re-resolves at execution time — design §2.2 item 3).
    pub tenant_id: Option<Uuid>,
    /// Which classification demanded the job (observability).
    pub origin: crate::state::JobOrigin,
}

impl JobDescriptor {
    fn from_row(row: crate::db::materialization::MaterializationJobRow) -> Self {
        Self {
            job_id: row.job_id,
            drv_hash: row.drv_hash,
            tenant_id: row.tenant_id,
            origin: row.origin,
        }
    }
}

/// Transient-retry deferral bounds (merged_bug_178). The default
/// covers `raced` (no Retry-After exists) and a 429 with no header;
/// the cap bounds a hostile/buggy store's `retry_after_secs` — the
/// deferral is view-only pacing, never park semantics, so the cap is
/// a denial-of-pacing bound, not a correctness edge.
pub(super) const RETRY_LATER_DEFAULT_DEFER_SECS: u64 = 5;
pub(super) const RETRY_LATER_MAX_DEFER_SECS: u64 = 300;

/// live_041 — the steal horizon (OQ-6b-2): a worker that has not
/// listed within this window has missed its beat, and the jobs it
/// OWNS under the rendezvous partition are served to every other
/// caller until it returns (work stealing — duplication bounded by
/// owner staleness, never asserted away; claims still arbitrate
/// one-winner, and WO-S6b-1's standing refund + speculation bound
/// make the contested losses cheap).
///
/// Calibration (re-derived for live_046 eager re-poll): the beat is
/// the store worker's poll cadence — `poll_interval_secs` default
/// 1 s, ±20 % jitter — and the binding worst case is the IDLE/EMPTY
/// cadence: a healthy idle worker lists at least every ~1.2 s plus
/// RPC/actor slack, so 5 s is four missed beats (the store-side
/// `idle_beat_worst_gap_times_four_fits_the_steal_horizon` pin holds
/// 4 × interval × (1 + jitter) ≤ this horizon through the exported
/// mirror symbol). Eager re-poll (live_046) tightens only PRODUCTIVE
/// passes — freshness strictly improves — and leaves the idle
/// cadence byte-identical; the honest beat (merged_bug_005,
/// store.materialize.honest-beat) withholds beats from passes that
/// cannot convert, so a wedged worker trips this horizon ON PURPOSE
/// exactly like the mid-walk case below. A worker mid-walk stops
/// listing for the walk's duration and trips this ON PURPOSE: it
/// cannot claim while executing (inline-serial, slots=1), so
/// offering its unclaimed slice to idle workers is the intended
/// stealing trigger. A deployment that raises the store's poll
/// interval past this horizon degrades to broader, more-contested
/// listings (the pre-live_041 shape) — never to unlisted jobs.
pub(super) const LISTING_STEAL_HORIZON: std::time::Duration = std::time::Duration::from_secs(5);

/// live_041 — membership TTL (OQ-6b-1): a worker silent past this
/// bound leaves the contact map entirely — the rendezvous partition
/// re-keys over the survivors, permanently re-homing the departed
/// worker's slice (scale-down leaves no residue). Between
/// [`LISTING_STEAL_HORIZON`] and this bound the silent worker still
/// OWNS its slice but the steal horizon serves it broadly — the
/// degradation direction is always "served more broadly", never
/// "unlisted".
pub(super) const LISTING_MEMBER_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// bug_045 (`sched.materialize.listing-cost`) — how long one
/// head-window snapshot serves polls before the listing beat re-runs
/// the durable query. Derivation (R17, violable + testable): the
/// bound must sit AT OR BELOW the worker poll cadence
/// (`poll_interval_secs` default 1 s — a snapshot older than one beat
/// would serve a staler head than the pre-fix per-poll query ever
/// did) and FAR below the scales whose lapses feed the window
/// (park backoffs: minutes; steal horizon: 5 s; member TTL: 60 s).
/// Claimability is still filtered per poll from the LIVE view
/// (bug_170's listed ⇒ admittable law is untouched — claimed /
/// parked / deferred / resolved rows drop exactly as before); only
/// park-LAPSE entry into the window waits ≤ this TTL, against park
/// scales of minutes. Job CREATION does not wait it out at all: the
/// view's creation feed marks the snapshot dirty
/// ([`JobView::creations`]).
pub(super) const LISTING_SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_secs(1);

/// bug_045 — the head-window FLOOR (the pre-existing 512-row
/// partition domain, now named): partitioned callers always drew the
/// full bounded head so slices cover it; the snapshot subsumes the
/// unpartitioned lanes' former `min(2×limit, 512)` over-fetch with
/// the same superset (recorded delta — same per-poll view filter,
/// same fail-closed arms).
///
/// sh-002 advisory §6 row 2: at 46 ready members the fixed 512-row
/// head partitioned to ~11 jobs/replica — every replica's coordinator
/// drained its slice in one pass and starved on the rendezvous
/// boundary while 6,615 jobs sat claimable. The window now scales
/// with the live membership ([`listing_head_window`]):
/// `max(512, ready_members × 32)` so wide fleets get >11/bucket.
/// Throughput, not correctness; the effect is gated on the deferred
/// coalesce-outcomes work (the single-threaded actor binds at ~15/s
/// regardless of head depth) — lands now for forward-compat.
const LISTING_HEAD_WINDOW_FLOOR: i64 = 512;

/// sh-002 — per-member head-window budget (× live members, floored at
/// [`LISTING_HEAD_WINDOW_FLOOR`]). 32 derived: at the production
/// `executor_concurrency=25` per replica, each coordinator can hold
/// up to ~25 SlotTokens; 32 leaves headroom for the resume-lane
/// CredentialOnly population without re-fetching mid-beat.
const LISTING_HEAD_PER_MEMBER: i64 = 32;

/// The membership-scaled head-window (sh-002 — see
/// [`LISTING_HEAD_WINDOW_FLOOR`] for the derivation).
fn listing_head_window(ready_members: usize) -> i64 {
    LISTING_HEAD_WINDOW_FLOOR.max(
        i64::try_from(ready_members)
            .unwrap_or(i64::MAX)
            .saturating_mul(LISTING_HEAD_PER_MEMBER),
    )
}

/// live061-R1 (R17, violable + testable) — per-class row bound of one
/// moot-sweep tick. The sweep is one fenced transaction per class
/// regardless of N (the live_053 batching lesson — the bound is NOT a
/// round-trip cap); what it bounds is the single transaction's UPDATE
/// row count / RETURNING set and the actor-side disposition fold, so
/// one pathological tick cannot hold the fence transaction open over
/// an unbounded row set. Derivation: 8× the claimable head-window
/// (512) and within 1 tick of clearing most observed bursts — the
/// live_053 mass-cancel population (5,258, the largest moot burst on
/// record) drains in 2 ticks; the claim plane is already clean after
/// ZERO ticks regardless, because the listing predicate
/// (`sched.materialize.claimability-projection+1`) excludes every
/// moot row independent of sweep progress. The truncated remainder
/// re-collects next tick (level-triggered; view entries leave only
/// on settled dispositions). Witness:
/// `moot_sweep_is_bounded_per_tick`.
pub(super) const MOOT_SWEEP_TICK_BOUND: usize = 4096;

/// bug_045 (R17) — always-on operation counters for the listing
/// chokepoint's cost envelope. The complexity claims are structural
/// (count operations, never wall-clock — the repo's
/// structural-over-wall-clock rule), so the counters live INSIDE the
/// operations they count: every caller counts by construction, and a
/// poll-path scoring call that bypasses the maintainer is structurally
/// visible as a nonzero delta. Relaxed ordering: the actor is
/// single-threaded; the counters are monotone diagnostics, never
/// synchronization. nextest's process-per-test isolation makes the
/// deltas test-local.
static SCORES_COMPUTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Counted at the SOLE production head-window call site (the listing
/// arm's `list_claimable_materialization_jobs` query).
static SNAPSHOT_FETCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Counted per member element visited at the member-iteration choke
/// sites (contact-map prune scans, membership snapshots, owner-age
/// walks): the O(served slice) serving law's witness is "zero member
/// touches on a stable-epoch poll", not prose.
static MEMBER_TOUCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[inline]
fn member_touch() {
    MEMBER_TOUCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Test-side reader for the three cost-envelope counters (the
/// `sched.materialize.listing-cost` verify set takes deltas around
/// production listing calls).
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ListingCostSnapshot {
    pub scores_computed: u64,
    pub snapshot_fetches: u64,
    pub member_touches: u64,
}

#[cfg(test)]
pub(crate) fn listing_cost_snapshot() -> ListingCostSnapshot {
    use std::sync::atomic::Ordering::Relaxed;
    ListingCostSnapshot {
        scores_computed: SCORES_COMPUTED.load(Relaxed),
        snapshot_fetches: SNAPSHOT_FETCHES.load(Relaxed),
        member_touches: MEMBER_TOUCHES.load(Relaxed),
    }
}

// r[impl sched.materialize.listing-distribution]
/// live_041 — THE per-pair rendezvous score (highest-random-weight
/// hash over `(job_id, member)`): the SipHash with the member string
/// as the total-order tie key (ties are astronomically unlikely at
/// 64 bits, but the owner must be a function). The SINGLE scoring
/// source (Q1): the batch argmax oracle and the incremental owner-map
/// maintainer both consume this fn — one source, two composers,
/// parity-pinned. The score counter increments HERE so every caller
/// counts by construction (R17).
///
/// Member unit (RULED CF-2): the per-WORKER composite `{pod}-w{n}` —
/// the token-bound identity the claim path already asserts. Never the
/// pod: `with_worker`'s sanitize-salt fallback makes suffix-stripping
/// unreliable, so no pod aggregation exists anywhere.
///
/// Hashing: `DefaultHasher::new()` (SipHash-1-3, fixed keys) —
/// deterministic within a process, which is all HRW needs here: the
/// leader's listing plan is the only consumer, and a re-partition at
/// leader failover or a std hash change is benign (claims arbitrate;
/// the steal horizon covers any transient).
pub(crate) fn rendezvous_score(job_id: Uuid, member: &str) -> (u64, &str) {
    use std::hash::{Hash, Hasher};
    SCORES_COMPUTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    job_id.hash(&mut h);
    member.hash(&mut h);
    (h.finish(), member)
}

/// The batch-argmax composer over [`rendezvous_score`] — since
/// bug_045 the production serving path reads the incremental
/// [`ListingPlan`] cache instead of recomputing this per row, so the
/// batch form survives as the PARITY ORACLE: the proptest pins
/// cached-owner ≡ this argmax across churn (Q1: one scoring source,
/// two composers).
#[cfg(test)]
pub(crate) fn rendezvous_owner<'a, I>(job_id: Uuid, members: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    scored_owner(job_id, members).map(|(_, m)| m)
}

// r[impl sched.materialize.listing-distribution]
/// The argmax WITH its winning score — what the incremental owner map
/// caches so later membership events compare against the stored
/// winner instead of recomputing the field. THE production argmax
/// composer over the single scoring source; same `(hash, member)`
/// total tie order as the batch oracle (`Iterator::max` keeps the
/// greatest tuple, exactly `max_by_key`'s semantics — member strings
/// are unique, so tuples never tie).
fn scored_owner<'a, I>(job_id: Uuid, members: I) -> Option<(u64, &'a str)>
where
    I: IntoIterator<Item = &'a str>,
{
    members
        .into_iter()
        .map(|m| rendezvous_score(job_id, m))
        .max()
}

/// bug_045 — the leader-tenure-scoped incremental rendezvous owner
/// map (`sched.materialize.listing-cost`): the job→owner partition is
/// a pure function of `(job_id, membership SET)`, and the membership
/// set changes only at join/leave — never at contact refreshes — so
/// the owner of every cached head-window job is maintained PER
/// MEMBERSHIP EVENT instead of recomputed per poll:
///
///   - JOIN: one score per cached job (the joiner against the stored
///     winner) — O(window);
///   - LEAVE of a non-owner: free; LEAVE of an owner: re-argmax of
///     exactly the departed member's jobs over the survivors —
///     O(owned × members);
///   - contact refreshes of existing members: nothing.
///
/// Lives beside [`ListingContacts`] inside [`JobViewState::Hydrated`]
/// on purpose: leader-tenure-scoped soft state with exactly the
/// view's lifecycle — wiped with the tenure on LeaderLost, empty at
/// recovery, so stale owners across failover are unrepresentable.
///
/// Tie order parity: the cached comparison uses the same
/// `(hash, member)` tuple order as [`rendezvous_score`] — the cached
/// owner is ALWAYS the batch argmax over the live membership (the
/// parity proptest pins cached ≡ `rendezvous_owner` — the test-only
/// batch oracle — across churn).
/// The listing beat's pacing charge (merged_bug_066): minted ONLY by
/// [`ListingPlan::refresh_decision`], spent BY VALUE into exactly one
/// of [`ListingPlan::spend_install`] (the Ok arm) or
/// [`ListingPlan::spend_failed`] (the Err arm). `#[must_use]` +
/// move-only: a beat arm that neither installs nor fails-the-charge
/// does not compile — the closure set over outcome arms is the type,
/// so the pacing envelope binds ATTEMPTS, not successes, and its
/// clock anchors at attempt COMPLETION (both spends sample their
/// instant internally, post-await).
#[must_use = "a fired beat MUST spend its pacing charge (spend_install / spend_failed)"]
#[derive(Debug)]
pub(crate) struct BeatToken {
    /// [`JobView::creations`] at decision time — the dirty token both
    /// spends consume into `seen_creations`.
    creations_seen: u64,
}

// r[impl sched.materialize.listing-cost+2]
#[derive(Debug, Default)]
pub(crate) struct ListingPlan {
    /// `{job_id → (winning hash, owner member)}` over the live
    /// membership, for every job in the current head window.
    owners: std::collections::HashMap<Uuid, (u64, String)>,
    /// The beat-scoped head snapshot (raw head-window rows, SQL
    /// order; claimability re-filtered per poll from the LIVE view).
    snapshot: Vec<crate::db::materialization::MaterializationJobRow>,
    /// The pacing anchor (merged_bug_066): when the last beat ATTEMPT
    /// completed — stamped by BOTH spends ([`Self::spend_install`] and
    /// [`Self::spend_failed`]), sampled internally post-await, so
    /// neither failure nor query latency re-opens per-poll beating.
    /// `None` only at construction/tenure wipe (cold start beats
    /// immediately). Serving keys on snapshot CONTENTS, never on this
    /// field — a failed beat's cleared snapshot answers empty
    /// (fail-closed preserved) while the pacing stands.
    last_beat: Option<std::time::Instant>,
    /// [`JobView::creations`] at the last beat ATTEMPT — the dirty
    /// edge, consumed by both spends (a creation observed during a
    /// failed beat costs at most one extra attempt; the TTL leg is
    /// its retry floor).
    seen_creations: u64,
    /// Disclosed harness alignment (merged_bug_066 W2, the R13
    /// disclosed-alignment lane): an awaited delay the handler
    /// inserts between the head-window query await and the spend —
    /// exactly where production PG latency sits. `None` in
    /// production; settable only from tests.
    #[cfg(test)]
    pub(crate) test_beat_latency: Option<std::time::Duration>,
    /// Per-beat owner buckets: member → ascending snapshot indices.
    buckets: std::collections::HashMap<String, Vec<usize>>,
    /// Members past [`LISTING_STEAL_HORIZON`] at the last beat (or
    /// still-unrefreshed since); their buckets are served broadly.
    stale_owners: std::collections::HashSet<String>,
    /// The merged ascending indices of every stale owner's bucket —
    /// precomputed per beat so a poll never iterates members.
    stolen: Vec<usize>,
}

impl ListingPlan {
    /// The cached owner for a head-window job (present for every job
    /// the window reconcile has seen). Test-facing parity probe —
    /// production serving reads the per-beat buckets, never per-job
    /// lookups.
    #[cfg(test)]
    pub(crate) fn owner(&self, job_id: Uuid) -> Option<&str> {
        self.owners.get(&job_id).map(|(_, m)| m.as_str())
    }

    /// A member JOINED (its first identity-bearing listing): compare
    /// the joiner against each cached winner — one score per cached
    /// job, never a full re-partition.
    pub(crate) fn on_join(&mut self, joiner: &str) {
        for (job_id, winner) in &mut self.owners {
            let (h, m) = rendezvous_score(*job_id, joiner);
            if (h, m) > (winner.0, winner.1.as_str()) {
                *winner = (h, m.to_owned());
            }
        }
    }

    /// A member LEFT (TTL prune): non-owners are free; the departed
    /// member's jobs re-argmax over the survivors. An emptied
    /// membership clears the cache wholesale (the partition domain is
    /// gone; the next window reconcile re-seeds over whatever
    /// membership then exists).
    pub(crate) fn on_leave<'a>(
        &mut self,
        leaver: &str,
        survivors: impl Iterator<Item = &'a str> + Clone,
    ) {
        if survivors.clone().next().is_none() {
            self.owners.clear();
            return;
        }
        for (job_id, winner) in &mut self.owners {
            if winner.1 != leaver {
                continue;
            }
            if let Some((h, m)) = scored_owner(*job_id, survivors.clone()) {
                *winner = (h, m.to_owned());
            }
        }
    }

    /// Reconcile the cache to the current head window: score jobs
    /// ENTERING the window (over the live membership — the once-per-
    /// window-change cost), drop jobs that left it. Cached jobs cost
    /// nothing.
    pub(crate) fn reconcile_window<'a>(
        &mut self,
        window: impl Iterator<Item = Uuid>,
        members: impl Iterator<Item = &'a str> + Clone,
    ) {
        let mut seen = std::collections::HashSet::new();
        for job_id in window {
            seen.insert(job_id);
            if !self.owners.contains_key(&job_id)
                && let Some((h, m)) = scored_owner(job_id, members.clone())
            {
                self.owners.insert(job_id, (h, m.to_owned()));
            }
        }
        self.owners.retain(|job_id, _| seen.contains(job_id));
    }

    /// The refresh decision (merged_bug_066): whether this poll must
    /// run the listing beat (the head-window query) — no beat yet
    /// this tenure, the pacing TTL elapsed since the last ATTEMPT, or
    /// the view's creation feed moved (new jobs must not wait out the
    /// TTL). The `Some` arm mints the [`BeatToken`] the handler MUST
    /// spend by value into exactly one of [`Self::spend_install`] /
    /// [`Self::spend_failed`] — the pacing charge exists on every
    /// outcome arm by construction.
    fn refresh_decision(&self, creations: u64, now: std::time::Instant) -> Option<BeatToken> {
        let due = match self.last_beat {
            None => true,
            Some(at) => {
                now.duration_since(at) >= LISTING_SNAPSHOT_TTL || creations != self.seen_creations
            }
        };
        due.then_some(BeatToken {
            creations_seen: creations,
        })
    }

    /// Spend the beat charge on a SUCCESSFUL fetch: install the fresh
    /// head window, the owner-map reconcile (only ENTERING jobs are
    /// scored), the staleness partition, and the owner buckets — ALL
    /// the per-beat work, so a poll between beats does none of it.
    /// Samples the pacing/partition instant INTERNALLY (post-await —
    /// a caller cannot supply a backdated instant, so query latency
    /// ≥ TTL cannot birth the snapshot expired; merged_bug_066).
    fn spend_install(
        &mut self,
        token: BeatToken,
        rows: Vec<crate::db::materialization::MaterializationJobRow>,
        members: &[(String, std::time::Instant)],
    ) {
        let now = std::time::Instant::now();
        self.last_beat = Some(now);
        self.seen_creations = token.creations_seen;
        self.snapshot = rows;
        let window: Vec<Uuid> = self.snapshot.iter().map(|r| r.job_id).collect();
        self.reconcile_window(window.into_iter(), members.iter().map(|(m, _)| m.as_str()));
        self.stale_owners = members
            .iter()
            .filter(|(_, last)| {
                member_touch();
                now.duration_since(*last) > LISTING_STEAL_HORIZON
            })
            .map(|(m, _)| m.clone())
            .collect();
        self.rebuild_buckets();
    }

    /// Spend the beat charge on a FAILED fetch: never serve a
    /// stale-unusable snapshot — answer empty until a refresh
    /// succeeds (the pre-snapshot fail-closed arm, preserved) — but
    /// the attempt still charges the pacing envelope and consumes the
    /// dirty token (merged_bug_066: pre-fix the Err arm zeroed the
    /// TTL, so during a PG failure every worker poll re-ran the
    /// 512-row query serialized on the actor, M-fold amplification
    /// into the degraded PG; and unconsumed creations re-opened
    /// per-poll beating through the dirty leg even where the TTL leg
    /// was paced).
    fn spend_failed(&mut self, token: BeatToken) {
        self.last_beat = Some(std::time::Instant::now());
        self.seen_creations = token.creations_seen;
        self.snapshot.clear();
        self.buckets.clear();
        self.stolen.clear();
    }

    /// A previously-stale member listed again: un-stale it NOW (its
    /// slice stops being served broadly on this very poll — the
    /// partition-resumes edge the steal tests pin), rebuilding the
    /// stolen overlay. Event-scoped work; a fresh member's poll is a
    /// no-op O(1) lookup.
    fn note_member_fresh(&mut self, member: &str) {
        if self.stale_owners.remove(member) {
            self.rebuild_stolen();
        }
    }

    /// bug_045 mixed-mode re-seed (recorded delta): a join into an
    /// epoch whose snapshot was installed over an EMPTY membership
    /// (instance-less polls only) finds an empty owner cache — the
    /// join walk has nothing to compare against, and serving would go
    /// empty for ≤1 beat (the unsafe narrow direction). Re-seed the
    /// cache over the current membership at the join event itself
    /// (O(window × members) — membership-event work, exactly the
    /// rule's budget).
    fn reseed_if_unseeded<'a>(&mut self, members: impl Iterator<Item = &'a str> + Clone) {
        if self.owners.is_empty() && !self.snapshot.is_empty() {
            let window: Vec<Uuid> = self.snapshot.iter().map(|r| r.job_id).collect();
            self.reconcile_window(window.into_iter(), members);
        }
    }

    /// Rebuild the per-beat owner buckets and the stolen overlay from
    /// the current snapshot + owner cache. Runs at beats and at
    /// membership events (join/leave/un-stale) — never per poll.
    fn rebuild_buckets(&mut self) {
        self.buckets.clear();
        for (idx, row) in self.snapshot.iter().enumerate() {
            if let Some((_, owner)) = self.owners.get(&row.job_id) {
                self.buckets.entry(owner.clone()).or_default().push(idx);
            }
        }
        self.rebuild_stolen();
    }

    /// Merge the stale owners' buckets into one ascending index list.
    fn rebuild_stolen(&mut self) {
        let mut stolen: Vec<usize> = self
            .stale_owners
            .iter()
            .filter_map(|owner| {
                member_touch();
                self.buckets.get(owner)
            })
            .flatten()
            .copied()
            .collect();
        stolen.sort_unstable();
        self.stolen = stolen;
    }

    /// Serve one poll from the beat state: the caller's owner bucket
    /// united with the stolen overlay (ascending snapshot order — the
    /// SQL ORDER BY survives as fairness within the served set), or
    /// the whole snapshot for unpartitioned callers. O(served slice);
    /// zero scoring; zero member iteration.
    fn serve<'p>(
        &'p self,
        partitioned_caller: Option<&str>,
    ) -> Box<dyn Iterator<Item = &'p crate::db::materialization::MaterializationJobRow> + 'p> {
        match partitioned_caller {
            None => Box::new(self.snapshot.iter()),
            Some(me) => {
                let mine: &[usize] = self.buckets.get(me).map(Vec::as_slice).unwrap_or(&[]);
                let stolen: &[usize] = self.stolen.as_slice();
                Box::new(MergeAscending::new(mine, stolen).map(|i| &self.snapshot[i]))
            }
        }
    }
}

/// Two-pointer merge of two ascending, disjoint index slices (the
/// caller's bucket + the stolen overlay) — the served set stays in
/// snapshot (SQL) order without a sort or a member walk.
struct MergeAscending<'a> {
    a: &'a [usize],
    b: &'a [usize],
}

impl<'a> MergeAscending<'a> {
    fn new(a: &'a [usize], b: &'a [usize]) -> Self {
        Self { a, b }
    }
}

impl Iterator for MergeAscending<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let take_a = match (self.a.first(), self.b.first()) {
            (Some(x), Some(y)) => x <= y,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        if take_a {
            let (x, rest) = self.a.split_first().expect("checked");
            self.a = rest;
            Some(*x)
        } else {
            let (y, rest) = self.b.split_first().expect("checked");
            self.b = rest;
            Some(*y)
        }
    }
}

/// live_041 — the leader's listing-contact map: `{worker member →
/// last identity-bearing listing}`. Fed exclusively by the listing
/// arm (the claim-path composites are the same identities; a
/// lease/endpoint view was REJECTED — it sees pods, not workers).
/// Lives inside [`JobViewState::Hydrated`] on purpose: it is
/// leader-tenure-scoped soft state with exactly the view's lifecycle
/// — wiped with the tenure on LeaderLost, empty at recovery (the
/// first post-failover beats serve broadly and converge within one
/// beat as workers list).
#[derive(Debug, Default)]
pub(crate) struct ListingContacts {
    contacts: std::collections::HashMap<String, std::time::Instant>,
}

impl ListingContacts {
    /// Record an identity-bearing listing call — an O(1) insert
    /// (bug_045: the TTL prune moved to the listing BEAT via
    /// [`Self::prune`]; per-poll contact recording walks nothing).
    /// Returns whether the member JOINED (its insert created the key)
    /// — the membership event the incremental owner map keys on;
    /// refreshes of existing members are not events.
    fn note(&mut self, member: &str, now: std::time::Instant) -> bool {
        self.contacts.insert(member.to_owned(), now).is_none()
    }

    /// Prune members silent past [`LISTING_MEMBER_TTL`], returning
    /// the leavers (the map stays bounded by live churn, not by
    /// all-time pod history). Runs once per listing BEAT — the leave
    /// granularity coarsens to ≤1 beat, far inside the 60 s TTL's
    /// slack (recorded delta).
    fn prune(&mut self, now: std::time::Instant) -> Vec<String> {
        let mut left = Vec::new();
        self.contacts.retain(|m, last| {
            member_touch();
            let live = now.duration_since(*last) <= LISTING_MEMBER_TTL;
            if !live {
                left.push(m.clone());
            }
            live
        });
        left
    }

    /// The live membership snapshot: members within
    /// [`LISTING_MEMBER_TTL`], with their last-listed instants (the
    /// steal horizon reads the ages). Beat-scoped — polls between
    /// beats never copy the membership.
    fn members(&self, now: std::time::Instant) -> Vec<(String, std::time::Instant)> {
        self.contacts
            .iter()
            .filter(|(_, last)| {
                member_touch();
                now.duration_since(**last) <= LISTING_MEMBER_TTL
            })
            .map(|(m, last)| (m.clone(), *last))
            .collect()
    }

    /// Contact-map size — the per-poll solo/partitioned check. May
    /// overcount by TTL-expired members for ≤1 beat (until the next
    /// beat prunes); the affected rows are stale-owner rows already
    /// served broadly, so the transient widens serving, never narrows
    /// it (recorded delta).
    fn len(&self) -> usize {
        self.contacts.len()
    }
}

/// The in-memory job view entry (droppable, never written back —
/// design handoff input 1's "derived droppable view"). Authority lives
/// in PG: creation dedup is the partial-unique index; consumption is
/// the fenced exec_id-keyed transaction. The view exists so pull
/// admission can answer from memory inside the actor turn; it is
/// populated only by the flag-gated creation paths (so flag-off it is
/// always empty) and rebuilt by query at recovery (Phase B).
#[derive(Debug, Clone)]
pub(crate) struct JobViewEntry {
    /// `materialization_jobs.job_id`.
    pub job_id: Uuid,
    /// Backoff expiry while parked; `None` = not parked. PRIVATE
    /// (bug_170): armament state is read ONLY through
    /// [`Self::claimability`] — no consumer hand-derives "claimable".
    parked_until: Option<std::time::Instant>,
    /// The claim episode (merged_bug_014): the claim holder and the
    /// episode-scoped skew-tripwire strike in ONE value, replaced
    /// WHOLESALE at every claim transition. PRIVATE (bug_170): mutated
    /// only by [`Self::mint_claim`] / [`Self::release_claim_if_held`]
    /// / the in-module companions, so every release is a
    /// compare-and-clear on the holder — and every strike dies with
    /// the episode it was observed in (see [`ClaimEpisode`]).
    episode: ClaimEpisode,
    /// Realized-path carrier (migration 082, the floating-CA
    /// stale-reset lane) — display copy for the claim-intake
    /// SUBSTITUTING event; the executor and the consumption coverage
    /// read the durable column directly.
    pub carried_realized_paths: Option<Vec<String>>,
    /// When the job's MOST RECENT park began (re-park overwrites — the
    /// dwell clock restarts). Mirror of the durable `park_began_at`
    /// (migration 083): set at park time, restored failover-exact by
    /// the recovery rebuild as a [`crate::state::RecoveredInstant`] —
    /// the recovered dwell keeps its true age on a freshly booted
    /// leader instead of silently restarting (merged_bug_300). `None`
    /// = never parked (or parked pre-083 — the dwell gate treats it as
    /// unmet, conservatively).
    parked_at: Option<crate::state::RecoveredInstant>,
    /// Transient-retry deferral (merged_bug_178): set by the
    /// RetryLater consumption arm (raced placeholder / upstream 429 —
    /// closed UNCHARGED), read by pull admission ONLY. Deliberately
    /// VIEW-ONLY: no durable column, no park semantics — PD-20's
    /// re-evaluation filter and the stalled gauge stay keyed on
    /// `parked_until`/`parked_at` exclusively, so a 429 wave can never
    /// walk a healthy job into park→PD-20→from-source, and a failover
    /// loses at most one uncharged deferral window (the new leader's
    /// view rebuild leaves it `None` — the job is immediately
    /// claimable again, which is the conservative direction for an
    /// uncharged transient).
    defer_until: Option<std::time::Instant>,
    /// Failover-exact mirror of `materialization_jobs.created_at`
    /// (migration 078): the phase-15 unclaimed age-out arm's
    /// `created_at.elapsed() > age_out_after` predicate reads this
    /// IN-MEMORY. The aged-out arm's per-entry PG await is the
    /// `classify_durable_evidence` read (capped at
    /// [`MAX_AGEOUT_PER_TICK`] so the 7725-row first-post-deploy
    /// backlog drains over `7725/256 ≈ 31` ticks instead of one
    /// serial-PG burst).
    created_at: crate::state::RecoveredInstant,
    /// Failover-exact, PG-authoritative mirror of
    /// `materialization_jobs.origin` (migration 078): both phase-15
    /// arms read this IN-MEMORY for the `from_source_viable`
    /// ChildlessLeaf gate and the `{origin}` counter label — neither
    /// arm issues a per-entry PG read for origin. Refreshed by
    /// [`DagActor::feed_job_view_entry`] on the dedup-UPGRADE edge
    /// only (`FencedJobCreate::Applied{upgraded:true}` — PG's
    /// upgrade-only `origin <> 'pruned'` UPDATE wrote the column), so
    /// the mirror is upward-monotone: a CacheOpportunity/Reprobe
    /// dedup onto an already-`Pruned` row leaves the mirror at
    /// `Pruned`, and the age-out arm's
    /// `from_source_viable(ChildlessLeaf, Pruned)=false` keeps the
    /// pruned root in the stalled gauge instead of evict-and-requeue
    /// (the bc84397f9 misroute).
    origin: crate::state::JobOrigin,
}

/// The one armament classification of a job-view entry — THE
/// derivation pull admission, the KEDA gauge, and the leader listing
/// all read (bug_170): no consumer combines the raw fields by hand, so
/// the three surfaces cannot disagree about what "claimable" means
/// (the listing advertising a job admission would refuse — the
/// NotYetReady busy-loop — is unrepresentable at the type).
///
/// Precedence: a held claim dominates everything; park (durable
/// backoff) dominates the view-only transient deferral; otherwise the
/// job is claimable right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claimability {
    /// Unclaimed, unparked, undeferred — a claim would be admitted.
    ClaimableNow,
    /// Transient-retry deferral active (merged_bug_178, view-only
    /// pacing): admission refuses NotYetReady until it lapses.
    Deferred,
    /// Durable park backoff unexpired: pacing, not claimable demand.
    Parked,
    /// An open materialization attempt holds the job.
    Claimed,
}

/// Disposition of one compare-and-clear claim release (bug_170/134):
/// a release names the holder it acts for, so a stale release (late
/// companion, ghost repair racing a fresh mint) can never clobber a
/// claim that now belongs to someone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimRelease {
    /// The named holder held the claim; it is now cleared.
    Released,
    /// A DIFFERENT identity holds a fresh claim — nothing cleared; the
    /// caller must not requeue the node (it belongs to the new
    /// attempt).
    StaleHolder,
    /// No claim was held (idempotent re-release).
    Unclaimed,
}

/// The claim episode (merged_bug_014): every per-claim-episode
/// observation flag lives WITH the episode it was observed in, and the
/// whole value is replaced WHOLESALE at every claim transition — so a
/// strike structurally cannot outlive its episode. The two skew
/// tripwires are per-arm fields BY CONSTRUCTION: the claimed-no-attempt
/// ghost strike (merged_bug_055 C) exists only while `Held`, and the
/// split-release wedge strike (merged_bug_285) exists only while
/// `Unclaimed` — the cross-episode strike survival that voided the
/// two-strike insurance (a wedge strike armed before a claim firing
/// the repair on the FIRST post-release wedged sweep) is
/// unrepresentable. Both strikes are the one-sweep insurance for the
/// documented snapshot race (a mint/attempt between the sweep's row
/// snapshot and the view iteration gets one full sweep to appear),
/// made structural and clock-free. In-memory only (never recovered as
/// set — recovery rebuilds from rows, so a recovered episode starts at
/// zero strikes by construction). The same pattern as the signed
/// episode-scoped-evidence resolution: evidence survives only
/// non-transitions, wiped wholesale at real edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimEpisode {
    /// An open materialization attempt holds the job.
    Held {
        /// The claiming identity (compare-and-clear key).
        holder: ExecutorId,
        /// Ghost tripwire: a sweep observed this claim UNBACKED (no
        /// open materialization assignment in the same snapshot); a
        /// second consecutive such observation is the
        /// claimed-no-attempt ghost and triggers the uncharged
        /// release repair.
        unbacked_strike: bool,
    },
    /// No open attempt holds the job.
    Unclaimed {
        /// Split-release tripwire: a sweep observed the node
        /// Assigned/Running with NO open assignment of either kind
        /// while this entry sat pending-unclaimed; a second
        /// consecutive such observation triggers the uncharged
        /// requeue repair.
        wedge_strike: bool,
    },
}

impl JobViewEntry {
    /// A fresh unclaimed, unparked, undeferred entry — the creation
    /// feed's shape (and the only construction tests get, so a
    /// fabricated pre-armed entry cannot appear outside this module).
    pub(crate) fn new_unclaimed(
        job_id: Uuid,
        carried_realized_paths: Option<Vec<String>>,
        origin: crate::state::JobOrigin,
    ) -> Self {
        Self {
            job_id,
            parked_until: None,
            episode: ClaimEpisode::Unclaimed {
                wedge_strike: false,
            },
            carried_realized_paths,
            parked_at: None,
            defer_until: None,
            created_at: crate::state::RecoveredInstant::fresh_now(),
            origin,
        }
    }

    /// THE armament classification (see [`Claimability`]).
    // r[impl sched.materialize.claimability-projection+1]
    pub(super) fn claimability(&self, now: std::time::Instant) -> Claimability {
        if matches!(self.episode, ClaimEpisode::Held { .. }) {
            Claimability::Claimed
        } else if self.parked_until.is_some_and(|until| until > now) {
            Claimability::Parked
        } else if self.defer_until.is_some_and(|until| until > now) {
            Claimability::Deferred
        } else {
            Claimability::ClaimableNow
        }
    }

    /// The claim holder, for identity comparison (`held_by_puller`).
    /// Read-only: armament DECISIONS go through
    /// [`Self::claimability`]; the housekeeping tripwires match the
    /// full [`Self::episode`].
    pub(super) fn holder(&self) -> Option<&ExecutorId> {
        match &self.episode {
            ClaimEpisode::Held { holder, .. } => Some(holder),
            ClaimEpisode::Unclaimed { .. } => None,
        }
    }

    /// The claim episode, for the housekeeping sweep's per-arm
    /// tripwire match (merged_bug_014): the sweep matches the episode
    /// itself, so each strike is structurally readable only in the arm
    /// that can act on it.
    pub(super) fn episode(&self) -> &ClaimEpisode {
        &self.episode
    }

    /// Mint the claim to `holder` (the pull mint, post-commit): a
    /// fresh `Held` episode, WHOLESALE (merged_bug_014) — the new
    /// claim is backed by definition (zero ghost strikes) and any
    /// wedge strike from the prior unclaimed episode dies with that
    /// episode.
    pub(super) fn mint_claim(&mut self, holder: ExecutorId) {
        self.episode = ClaimEpisode::Held {
            holder,
            unbacked_strike: false,
        };
    }

    /// Compare-and-clear the claim on behalf of `expected` (bug_170:
    /// `release_claim` names its holder). The Released arm replaces
    /// the episode WHOLESALE (merged_bug_014): the ghost strike dies
    /// with the held episode and the fresh unclaimed episode starts at
    /// zero wedge strikes — a post-release wedged sweep must observe
    /// two FRESH consecutive races before it may repair. A holder
    /// mismatch replaces NOTHING.
    // r[impl sched.materialize.claim-coherence]
    pub(super) fn release_claim_if_held(&mut self, expected: &ExecutorId) -> ClaimRelease {
        match &self.episode {
            ClaimEpisode::Held { holder, .. } if holder == expected => {
                self.episode = ClaimEpisode::Unclaimed {
                    wedge_strike: false,
                };
                ClaimRelease::Released
            }
            ClaimEpisode::Held { .. } => ClaimRelease::StaleHolder,
            ClaimEpisode::Unclaimed { .. } => ClaimRelease::Unclaimed,
        }
    }

    /// Compare-and-clear PLUS the deferral stamp in ONE
    /// disposition-gated mutation (bug_220): the defer rides the
    /// release disposition, so a stale holder's redelivered RetryLater
    /// cannot deface a job a different executor now holds. `Released`
    /// and `Unclaimed` stamp (the latter keeps idempotent redelivery
    /// after the holder's own release working); `StaleHolder` stamps
    /// NOTHING. This is the SINGLE production writer of `defer_until`
    /// — the field is private and every other path only reads it
    /// (`claimability`), so "a stale release cannot touch the deferral
    /// plane" is structural, not reviewed.
    // r[impl sched.materialize.claim-coherence]
    pub(super) fn release_claim_deferring(
        &mut self,
        expected: &ExecutorId,
        defer_until: Option<std::time::Instant>,
    ) -> ClaimRelease {
        let release = self.release_claim_if_held(expected);
        if release != ClaimRelease::StaleHolder
            && let Some(until) = defer_until
        {
            self.defer_until = Some(until);
        }
        release
    }

    /// Unconditional claim clear — the no-named-executor fallback of
    /// the park companions ONLY (no production lane reaches it today;
    /// every release that can race a fresh mint goes through
    /// [`Self::release_claim_if_held`]). Replaces the episode
    /// WHOLESALE like every transition (merged_bug_014).
    fn clear_claim_unconditional(&mut self) {
        self.episode = ClaimEpisode::Unclaimed {
            wedge_strike: false,
        };
    }

    /// Enter park: backoff expiry + the PD-20 dwell anchor (migration
    /// 083 mirror). Re-park restarts the dwell clock by design.
    fn park(&mut self, until: std::time::Instant) {
        self.parked_until = Some(until);
        self.parked_at = Some(crate::state::RecoveredInstant::fresh_now());
    }

    /// Test-only filler-free constructor (sh-044 r1): every test caller
    /// of [`Self::new_unclaimed`] passed the identical
    /// `(None, JobOrigin::CacheOpportunity)` filler — the next field
    /// add to the production constructor would otherwise force another
    /// 28-site mechanical edit. Tests that need a non-default carrier
    /// or origin go through [`Self::new_unclaimed`] directly.
    #[cfg(test)]
    pub(crate) fn test_unclaimed(job_id: Uuid) -> Self {
        Self::new_unclaimed(job_id, None, crate::state::JobOrigin::CacheOpportunity)
    }

    /// Test seeding of the park axis (the production writer is the
    /// park companion).
    #[cfg(test)]
    pub(super) fn test_set_parked_until(&mut self, until: Option<std::time::Instant>) {
        self.parked_until = until;
    }

    /// Test seeding of the age-out axis ([`Self::created_at`] is set
    /// at construction; the `DebugBackdate*` mechanism — tokio paused
    /// time cannot mock `std::time::Instant`, and the materialize
    /// harness's real sqlx pool `PoolTimedOut`s under `start_paused`).
    #[cfg(test)]
    pub(super) fn test_set_created_at(&mut self, at: crate::state::RecoveredInstant) {
        self.created_at = at;
    }

    /// Test seeding of the deferral axis (the production writer is the
    /// RetryLater consumption arm).
    #[cfg(test)]
    pub(super) fn test_set_defer_until(&mut self, until: Option<std::time::Instant>) {
        self.defer_until = until;
    }

    /// Test-visible wrapper over the recovery row conversion
    /// (merged_bug_262 totality pin).
    #[cfg(test)]
    pub(super) fn from_recovered_row_for_test(
        row: crate::db::open_attempts::RecoveredJobRow,
    ) -> JobViewEntry {
        DagActor::entry_from_recovered_row(row)
    }

    /// Arm/clear the two-strike ghost flag (merged_bug_055 C),
    /// housekeeping's write half. Episode-arm scoped (merged_bug_014):
    /// writable ONLY while `Held` — an unclaimed entry has no claim to
    /// call a ghost, so the write is a structural no-op there (the
    /// sweep matches the episode and only reaches this in the Held
    /// arm). The read half is the sweep's episode match.
    pub(super) fn set_strike(&mut self, armed: bool) {
        if let ClaimEpisode::Held {
            unbacked_strike, ..
        } = &mut self.episode
        {
            *unbacked_strike = armed;
        }
    }

    /// Arm/clear the split-release wedge strike (merged_bug_285),
    /// housekeeping's write half. Episode-arm scoped (merged_bug_014):
    /// writable ONLY while `Unclaimed` — a held entry's node is
    /// expected Assigned/Running, so there is no wedge to track and
    /// the write is a structural no-op there. The read half is the
    /// sweep's episode match.
    pub(super) fn set_wedge_strike(&mut self, armed: bool) {
        if let ClaimEpisode::Unclaimed { wedge_strike } = &mut self.episode {
            *wedge_strike = armed;
        }
    }
}

/// `#[must_use]` actor-side disposition of a fenced durable write —
/// the gate every job-view removal and companion action derives from
/// (`sched.materialize.view-settlement`). `Applied`/`AlreadyResolved`
/// (= settled) authorize the view mutation and the companions;
/// `Fenced` keeps the entry inert until the LeaderLost wipe (a deposed
/// believer mutates nothing it no longer owns); `Failed` keeps it for
/// the next tick's level-triggered retry (tick cadence bounds the
/// retry; the durable row is the authority either way).
// The disposition alphabet now lives WITH its settlement laws in the
// kernel (bug_182/merged_bug_055): `consumption_ack` decides the
// store-facing answer, `companion_follow_up` decides the claim's fate
// after a companion write — both CBMC-swept there, wired here.
pub(crate) use rio_evidence_kernel::settle::{
    CompanionFollowUp, ConsumptionAck, WriteDisposition, companion_follow_up, consumption_ack,
};

/// Proof that the consumption close for ONE report became durable
/// (`Applied`/`AlreadyResolved`). Linear and `#[must_use]`: produced
/// only by [`DagActor::close_for_consumption`] and its batched twin
/// [`DagActor::close_for_consumption_from_disposition`] (the sh-007c
/// S6 second sanctioned mint — same kernel `consumption_ack` law over
/// a `WriteDisposition` synthesized from the batch tx outcome),
/// consumed BY VALUE by exactly the five settled-close companions —
/// an arm that closes and drops the witness fails `--deny warnings`;
/// an arm that never closes has no witness to spend and therefore
/// cannot mint a [`MatAck`].
#[must_use = "a settled close must be spent on exactly one companion (bug_182)"]
#[derive(Debug)]
pub(crate) struct SettledClose(());

impl SettledClose {
    /// Test-only witness mint for unit tests of settled-gated
    /// observability (bug_086) — production construction stays
    /// exclusively inside [`DagActor::close_for_consumption`].
    #[cfg(test)]
    pub(crate) fn test_witness() -> Self {
        SettledClose(())
    }
}

/// Proof that one materialization report was CONSUMED to the point
/// where acknowledging the store is lawful (`sched.materialize.ack-law`):
/// either a settled close ran its companion, or the close was fenced
/// (deposed believer — ack and mutate nothing, the signed Q20 posture).
/// No other construction site exists, so "ack with the assignment
/// still open" no longer typechecks.
#[must_use = "the ack witness is the consumption's return value"]
pub(super) struct MatAck(());

/// One consumption close's outcome, per the kernel ack law.
#[must_use]
pub(super) enum CloseOutcome {
    /// The close settled durably: spend the witness on a companion.
    Settled(SettledClose),
    /// Fenced (deposed believer): acked, nothing further runs here —
    /// the successor's establishment owns the row (signed Q20).
    Deferred(MatAck),
    /// The close write failed: NACK retryably
    /// ([`super::pull::PullRejection::ConsumptionNotDurable`]) so the
    /// store's report redelivery retries the SAME outcome instead of
    /// the charged 'unreported' establishment settling it an hour
    /// later.
    NotDurable,
}

/// Fail-closed availability wrapper around the job view
/// (merged_bug_246). The view is either **Hydrated** — rebuilt from the
/// durable rows by recovery, the only constructor of trust — or
/// **Unavailable** — boot, post-wipe, or a term whose recovery failed.
///
/// `Unavailable` is NOT "no jobs": a populated DAG over an absent view
/// is exactly the 246 hole (mat claims answered `Gone`, the store
/// skips, the armed action strands; build pulls fall into the
/// `None→DeliverNew` kernel cell and race the job). Every consumer
/// reads through a per-question accessor whose Unavailable answer is
/// the conservative one:
/// pulls → `Pending{parked:true}` (every kind maps to NotYetReady,
/// token/fence checks still dominate); spawn exclusion → exclude;
/// KEDA backlog → 0; ticks → skip; creation feeds → dropped (the
/// durable row is the authority; the backstop sweep re-feeds once
/// Hydrated).
///
/// Transitions: [`Self::rebuild`] (recovery Ok) is the ONLY path to
/// `Hydrated` — a clear path that produced `Hydrated(empty)` over a
/// repopulated DAG would recreate the hole, so [`Self::wipe`] and the
/// default both land on `Unavailable`. One construction-time
/// exception, mirroring `dag_authoritative`: the always-leader
/// (non-K8s) actor starts hydrated-EMPTY via `rebuild`, because no
/// lease loop will ever send the `LeaderAcquired` that runs recovery
/// there, and at construction the DAG is empty too — the empty view
/// is faithful, not fabricated (the 246 hole requires a populated
/// DAG over an absent view). K8s mode is unchanged: `Unavailable`
/// until the first successful recovery.
#[derive(Debug, Default)]
pub(crate) enum JobViewState {
    /// No trustworthy view exists this term. Fail closed.
    #[default]
    Unavailable,
    /// Rebuilt from PG by recovery; live-maintained by the creation
    /// and consumption paths. Carries the [`ListingContacts`] and the
    /// [`ListingPlan`] beside the view (live_041 / bug_045): both are
    /// leader-tenure-scoped soft state with exactly the view's
    /// lifecycle (wiped with the tenure, empty at recovery), and the
    /// listing arm — their only consumer — already gates on this very
    /// arm.
    Hydrated {
        view: JobView,
        contacts: ListingContacts,
        /// Boxed: the beat state (snapshot + buckets) dwarfs the
        /// enum's other variant (stable clippy large_enum_variant);
        /// the listing arm reaches it through one indirection per
        /// poll.
        plan: Box<ListingPlan>,
    },
}

impl JobViewState {
    /// The hydrated view, if any. Consumers that read entry state for
    /// settled-write companions use this directly (their writes are
    /// fence-gated; a `None` here simply skips the in-memory mirror).
    pub(super) fn hydrated(&self) -> Option<&JobView> {
        match self {
            Self::Unavailable => None,
            Self::Hydrated { view, .. } => Some(view),
        }
    }

    pub(super) fn hydrated_mut(&mut self) -> Option<&mut JobView> {
        match self {
            Self::Unavailable => None,
            Self::Hydrated { view, .. } => Some(view),
        }
    }

    /// The hydrated view together with its listing-contact map and
    /// listing plan (live_041 / bug_045 — the listing arm's split
    /// borrow: contacts and plan mutate while the view is read).
    pub(super) fn hydrated_listing_mut(
        &mut self,
    ) -> Option<(&JobView, &mut ListingContacts, &mut ListingPlan)> {
        match self {
            Self::Unavailable => None,
            Self::Hydrated {
                view,
                contacts,
                plan,
            } => Some((view, contacts, plan.as_mut())),
        }
    }

    /// Per-entry read through the availability gate (`None` both for
    /// "no entry" and "no view" — callers needing to distinguish use
    /// [`Self::hydrated`]).
    pub(super) fn get<Q>(&self, k: &Q) -> Option<&JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.hydrated().and_then(|v| v.get(k))
    }

    pub(super) fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.hydrated_mut().and_then(|v| v.get_mut(k))
    }

    /// Iterate the hydrated entries; empty when Unavailable (the
    /// "ticks → skip" posture: periodic arms do nothing rather than
    /// acting on an absent cache).
    pub(super) fn iter(&self) -> impl Iterator<Item = (&DrvHash, &JobViewEntry)> {
        self.hydrated().into_iter().flat_map(|v| v.iter())
    }

    /// Settled-gated removal; `false` when Unavailable (nothing to
    /// remove — the durable row is the authority).
    pub(super) fn remove_settled<Q>(&mut self, k: &Q, d: WriteDisposition) -> bool
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        match self.hydrated_mut() {
            Some(v) => v.remove_settled(k, d),
            None => false,
        }
    }

    /// LeaderLost / pre-recovery clear: the cache is gone AND so is the
    /// trust — never `Hydrated(empty)` (merged_bug_246).
    pub(super) fn wipe(&mut self) {
        *self = Self::Unavailable;
    }

    /// Recovery: the only `Hydrated` constructor. The contact map
    /// starts EMPTY on purpose (live_041): a new tenure trusts no
    /// prior contact times — the first beats serve broadly
    /// (empty/sparse membership ⇒ unpartitioned-to-coarse listings)
    /// and converge within one beat as workers list.
    pub(super) fn rebuild(&mut self, entries: impl IntoIterator<Item = (DrvHash, JobViewEntry)>) {
        let mut v = JobView::default();
        v.rebuild(entries);
        *self = Self::Hydrated {
            view: v,
            contacts: ListingContacts::default(),
            plan: Box::default(),
        };
    }

    /// Test seeding: hydrate-if-needed then insert (tests model a
    /// healthy post-recovery leader).
    #[cfg(test)]
    pub(super) fn insert(&mut self, k: DrvHash, v: JobViewEntry) -> Option<JobViewEntry> {
        if matches!(self, Self::Unavailable) {
            *self = Self::Hydrated {
                view: JobView::default(),
                contacts: ListingContacts::default(),
                plan: Box::default(),
            };
        }
        match self {
            Self::Hydrated { view, .. } => view.insert(k, v),
            Self::Unavailable => unreachable!(),
        }
    }
}

// r[impl sched.materialize.view-settlement]
/// The in-memory materialization job view (a droppable cache of the
/// durable job table). The wrapper makes the removal discipline
/// STRUCTURAL: [`JobView::remove_settled`] is the only per-entry
/// removal and demands the durable write's [`WriteDisposition`] — an
/// unconditional `.remove()` no longer typechecks. Whole-view
/// transitions are `JobView::wipe` (LeaderLost — the cache drops
/// with the tenure) and [`JobView::rebuild`] (recovery — re-read from
/// the durable authority). Availability (hydrated vs absent) is the
/// enclosing [`JobViewState`]'s concern.
#[derive(Debug, Default)]
pub(crate) struct JobView {
    entries: std::collections::HashMap<DrvHash, JobViewEntry>,
    /// bug_045 — monotone count of NEW entries fed into the view: the
    /// listing snapshot's creation-dirty edge (new jobs must enter the
    /// head window without waiting out the snapshot TTL). Bumped by
    /// every insertion path that creates a key — the creation feed
    /// itself signals, so no call site carries a separate hook.
    creations: u64,
}

impl JobView {
    pub(crate) fn get<Q>(&self, k: &Q) -> Option<&JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.entries.get(k)
    }

    pub(super) fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut JobViewEntry>
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.entries.get_mut(k)
    }

    pub(crate) fn contains_key<Q>(&self, k: &Q) -> bool
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        self.entries.contains_key(k)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&DrvHash, &JobViewEntry)> {
        self.entries.iter()
    }

    /// bug_045 — the creation-feed cursor the listing snapshot's
    /// dirty edge compares against (a changed count = new jobs the
    /// snapshot has not seen).
    pub(super) fn creations(&self) -> u64 {
        self.creations
    }

    /// Recovery: rebuild the cache from the durable rows.
    pub(super) fn rebuild(&mut self, entries: impl IntoIterator<Item = (DrvHash, JobViewEntry)>) {
        self.entries.clear();
        self.entries.extend(entries);
        // Every rebuilt entry is new to THIS view generation — the
        // first post-recovery beat must refresh.
        self.creations = self.creations.wrapping_add(1);
    }

    /// Direct insertion (test seeding via [`JobViewState::insert`]).
    /// Additive only — the removal discipline is untouched.
    #[cfg(test)]
    fn insert(&mut self, k: DrvHash, v: JobViewEntry) -> Option<JobViewEntry> {
        let prior = self.entries.insert(k, v);
        if prior.is_none() {
            self.creations = self.creations.wrapping_add(1);
        }
        prior
    }

    /// Insert-or-keep for the creation paths: a pre-existing entry
    /// (the dedup found an unresolved row, or recovery rebuilt it)
    /// keeps its armament state (park backoff, claim holder); a new
    /// job gets the fresh entry. Additive only — never a removal.
    pub(super) fn entry_or_insert(
        &mut self,
        k: DrvHash,
        default: JobViewEntry,
    ) -> &mut JobViewEntry {
        match self.entries.entry(k) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                self.creations = self.creations.wrapping_add(1);
                e.insert(default)
            }
        }
    }

    // r[impl sched.materialize.view-settlement]
    /// THE per-entry removal: only a settled durable disposition may
    /// remove. Returns whether THIS call removed the entry — the gate
    /// every companion action (requeue, completion batch, fail-fast,
    /// conversion counter) hangs on. `Fenced`/`Failed` keep the entry:
    /// the armed action stays level-triggered instead of stranding the
    /// durable row behind an empty view.
    pub(super) fn remove_settled<Q>(&mut self, k: &Q, d: WriteDisposition) -> bool
    where
        DrvHash: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        if d.settled() {
            self.entries.remove(k).is_some()
        } else {
            false
        }
    }
}

/// One job the merge transaction created (or found via the dedup) —
/// what `persist_merge_to_db` returns to the post-commit phase so the
/// in-memory view is fed OUTSIDE the transaction (a rolled-back merge
/// must leave no view entry; the view is a cache, not an authority).
#[derive(Debug, Clone)]
pub(crate) struct CreatedJob {
    pub drv_hash: DrvHash,
    pub job_id: Uuid,
    /// False when the dedup found a pre-existing unresolved job.
    pub created: bool,
    /// The dedup upgraded the existing pending row's origin to
    /// `'pruned'` (pruned-wins, PD-D1 — counted separately from
    /// creations).
    pub upgraded: bool,
    pub origin: JobOrigin,
}

/// T3 (bug_110): ONE clock per logical listing pass. The handler
/// reads time ONLY through this type — `arm()` at pass start and the
/// single re-arm at the beat arm's completion (post-await) are the
/// only mint sites, so a second raw `Instant::now()` inside the
/// handler is structurally absent (pinned by the handler-region
/// census in the test battery). Wave-8's faac1261e re-pointed only
/// the prune/members reads to a post-await clock; the Phase-3
/// claimability serve filter and the caller's contact stamp stayed
/// on the pre-await one — on a slow beat a backoff lapsing during
/// the query was withheld a poll, and the contact stamp ran older
/// than the prune clock within the same pass.
pub(super) struct PassClock(std::time::Instant);

impl PassClock {
    /// Arm (or re-arm, at the beat completion) the pass clock.
    fn arm() -> Self {
        Self(std::time::Instant::now())
    }

    /// The single instant every read in the pass consumes.
    fn instant(&self) -> std::time::Instant {
        self.0
    }
}

impl DagActor {
    /// Leader-served job listing (the store's poll). Standby or no
    /// jobs → empty vec (never an error).
    ///
    /// bug_170: the durable query cannot see view-only armament (the
    /// merged_bug_178 transient deferral has no column; a fresh claim
    /// can commit between the query and the reply), so the rows are
    /// over-fetched and filtered through THE SAME [`Claimability`] law
    /// admission reads: only `view-exists ∧ ClaimableNow` survives.
    /// A listed job is therefore admittable by construction — the
    /// listing can no longer advertise work every claim of which the
    /// admission table refuses (the store-replica NotYetReady
    /// busy-loop). An Unavailable view answers EMPTY, preserving the
    /// merged_bug_246/155 fail-closed postures verbatim (advertising
    /// jobs whose armament we cannot see would hand out claims that
    /// race the durable holders).
    ///
    /// live_041 — the claimable head is rendezvous-PARTITIONED across
    /// the live store-worker membership: an identity-bearing caller
    /// (the verified `{pod}-w{n}` instance claim, threaded from the
    /// gRPC chokepoint) is recorded in the tenure-scoped
    /// [`ListingContacts`] and served the jobs the [`ListingPlan`]
    /// owner map assigns it — disjoint slices by construction, so N
    /// workers advance N slices instead of all racing the same
    /// `ORDER BY created_at` head (the convoy: one winner, N−1 burned
    /// passes, KEDA scale-out adding racers) — UNION the steal
    /// horizon: jobs whose owner has not listed within
    /// [`LISTING_STEAL_HORIZON`] (computed at this same site from the
    /// same membership map; no wire-visible segment distinction, no
    /// client steal lane — RULED CF-3). The SQL `ORDER BY` survives
    /// as fairness WITHIN a slice. An instance-less caller (full dev
    /// mode) contributes no member and is served the unpartitioned
    /// listing — with no identity-bearing callers the behavior is
    /// byte-for-byte the pre-partition shape.
    ///
    /// bug_045 — the partition is never recomputed per poll: polls
    /// serve the beat-scoped head snapshot through the incremental
    /// owner map, the head-window query runs at most once per
    /// [`LISTING_SNAPSHOT_TTL`] or creation-dirty event, and ALL
    /// membership-derived work (TTL prune, staleness partition,
    /// owner buckets) runs per beat / per membership event
    /// (`sched.materialize.listing-cost`; the envelope tests count
    /// the operations).
    // r[impl sched.materialize.job+2]
    // r[impl sched.materialize.claimability-projection+1]
    // r[impl sched.materialize.listing-distribution]
    // r[impl sched.materialize.listing-cost+2]
    pub(super) async fn handle_list_materialization_jobs(
        &mut self,
        limit: u32,
        instance: Option<String>,
        reply: oneshot::Sender<Vec<JobDescriptor>>,
    ) {
        let limit = limit.min(256);
        // Standby, zero limit, and an Unavailable view share one
        // fail-closed arm: answer empty.
        if !self.leader.is_leader() || limit == 0 || self.materialization_jobs.hydrated().is_none()
        {
            let _ = reply.send(Vec::new());
            return;
        }
        // bug_110 (T3): the pass clock — armed once here; re-armed
        // exactly once at the beat arm's completion. Every time read
        // below goes through it.
        let mut pass = PassClock::arm();
        // Phase 1 — contact + membership events + the refresh
        // decision (bug_045: O(1) on a stable-epoch poll). The
        // caller's contact records FIRST (its own membership entry
        // rides this very call) and records even when the beat's
        // query errors below — the caller IS live and capable; a DB
        // blip must not mass-stale the fleet.
        let beat_token = {
            let (view, contacts, plan) = self
                .materialization_jobs
                .hydrated_listing_mut()
                .expect("hydrated checked above");
            if let Some(me) = instance.as_deref() {
                if contacts.note(me, pass.instant()) {
                    // JOIN: rescore cached jobs against the joiner
                    // (one score each), re-seed if the cache was
                    // installed over an empty membership, and fold
                    // the new ownership into the beat buckets —
                    // membership-event work, never per-poll work.
                    plan.on_join(me);
                    let members = contacts.members(pass.instant());
                    plan.reseed_if_unseeded(members.iter().map(|(m, _)| m.as_str()));
                    plan.rebuild_buckets();
                }
                // A previously-stale member listing again un-stales
                // NOW (the partition-resumes edge): its slice stops
                // being served broadly on this very poll.
                plan.note_member_fresh(me);
            }
            plan.refresh_decision(view.creations(), pass.instant())
        };
        // Phase 2 — the listing BEAT: at most one head-window query
        // ATTEMPT per pacing TTL or consumed creation-dirty event
        // (R17, all axes: the charge is per attempt over the full
        // {Ok, Err} × {fast, ≥TTL} product — the BeatToken must be
        // spent into exactly one outcome arm, and both spends sample
        // the pacing clock at attempt COMPLETION; the fetch counter
        // at this sole production call site is the law's witness).
        // The TTL prune, the staleness partition, the owner-map
        // reconcile, and the bucket build all run HERE — polls
        // between beats do none of it.
        if let Some(token) = beat_token {
            SNAPSHOT_FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // sh-002: the head window scales with the live membership
            // sampled at the contacts beat above (the same membership
            // the rendezvous partition keys on).
            let head = {
                let (_v, contacts, _p) = self
                    .materialization_jobs
                    .hydrated_listing_mut()
                    .expect("hydrated checked above");
                listing_head_window(contacts.len())
            };
            let fetched = self.db.list_claimable_materialization_jobs(head).await;
            // Disclosed harness alignment (W2's latency hook): an
            // awaited delay exactly where production PG latency sits.
            #[cfg(test)]
            {
                let delay = self
                    .materialization_jobs
                    .hydrated_listing_mut()
                    .and_then(|(_, _, plan)| plan.test_beat_latency);
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
            }
            let (_view, contacts, plan) = self
                .materialization_jobs
                .hydrated_listing_mut()
                .expect("hydrated checked above");
            match fetched {
                Ok(rows) => {
                    // bug_110: the beat arm's SINGLE re-arm — the
                    // whole tail of the pass (caller re-note, prune,
                    // members, steal-horizon, the Phase-3 serve
                    // filter) reads this one post-await clock.
                    // faac1261e's correction covered only
                    // prune/members; the serve filter stayed
                    // pre-await (a backoff lapsing during the query
                    // was withheld a poll) and the contact stamp ran
                    // older than the prune clock.
                    pass = PassClock::arm();
                    if let Some(me) = instance.as_deref() {
                        // Re-stamp the caller at the pass clock
                        // BEFORE pruning: the caller is live AT beat
                        // completion (it is this very call), so its
                        // stamp can never run older than the prune
                        // clock within the pass — structural, both
                        // consume the same instant.
                        contacts.note(me, pass.instant());
                    }
                    let leavers = contacts.prune(pass.instant());
                    let members = contacts.members(pass.instant());
                    for leaver in &leavers {
                        plan.on_leave(leaver, members.iter().map(|(m, _)| m.as_str()));
                    }
                    plan.spend_install(token, rows, &members);
                }
                Err(e) => {
                    // Fail closed: never serve a stale-unusable
                    // snapshot (the pre-snapshot empty-answer arm,
                    // preserved; the contact note above stands) —
                    // but the ATTEMPT charges the envelope and
                    // consumes the dirty token.
                    warn!(error = %e, "ListMaterializationJobs query failed; answering empty");
                    plan.spend_failed(token);
                    let _ = reply.send(Vec::new());
                    return;
                }
            }
        }
        // Phase 3 — serve from the beat state: the caller's owner
        // bucket united with the stolen overlay (or the whole
        // snapshot for instance-less / solo callers — the dev-mode
        // fallback, byte-identical), with claimability re-filtered
        // per row from the LIVE view (bug_170: a listed job is
        // admittable by construction — claimed / parked / deferred /
        // resolved rows drop exactly as before) AND the node face
        // re-read from the LIVE DAG (live_061: a node that completed
        // by other means since the beat snapshot makes every claim
        // answer Gone — the kernel base table's terminal arm — so
        // serving its job advertises a doomed mint; the in-memory
        // status is AHEAD of the durable predicate during the
        // transition→persist window, so this filter closes the race
        // the beat query structurally cannot see). A DAG-ABSENT entry
        // is still served: absence is the zero-interest sweep's
        // one-tick transient (the `None => true` cancel arm), and
        // refusing to serve it here would also refuse the
        // recovery-rebuilt view's legitimate rows while the DAG
        // hydrates. O(served slice) map lookups; zero scoring; zero
        // member iteration (R17: member_touches == 0 on this path).
        let dag = &self.dag;
        let (view, contacts, plan) = self
            .materialization_jobs
            .hydrated_listing_mut()
            .expect("hydrated checked above");
        let plan = &*plan;
        let caller = instance.as_deref().filter(|_| contacts.len() > 1);
        let jobs = plan
            .serve(caller)
            .filter(|row| {
                view.get(row.drv_hash.as_str()).is_some_and(|entry| {
                    entry.claimability(pass.instant()) == Claimability::ClaimableNow
                }) && !dag
                    .node(row.drv_hash.as_str())
                    .is_some_and(|s| s.status().is_terminal())
            })
            .take(limit as usize)
            .cloned()
            .map(JobDescriptor::from_row)
            .collect();
        let _ = reply.send(jobs);
    }

    /// THE single job-creation helper for callers with NO enclosing
    /// transaction (every §2.1 probe-partition site calls this one fn —
    /// the "one helper" the design's B7 disposition requires; the merge
    /// sites use the in-tx core inside `persist_merge_to_db` instead).
    /// No-op on standby. Creates the job row fenced + dedup'd, updates
    /// the in-memory view, and records the wanted relation for the
    /// creating build when one is named.
    ///
    /// Returns whether an unresolved job exists for the node after the
    /// call (created now or found by the dedup).
    // r[impl sched.materialize.job+2]
    #[must_use = "a false return means the job row did NOT apply — a caller holding a \
                  realized-path carrier must stash it for the housekeeping retry"]
    pub(super) async fn create_materialization_job(
        &mut self,
        drv_hash: &DrvHash,
        origin: JobOrigin,
        creating_build: Option<Uuid>,
        carried_realized_paths: Option<Vec<String>>,
    ) -> bool {
        if !self.leader.is_leader() {
            return false;
        }
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        let Some(db_id) = state.db_id else {
            return false;
        };
        // Tenant: any live interested build's tenant — a HINT only
        // (bug_139: upstream config AND sig trust are per-tenant, so
        // whose view a probe runs under is NOT irrelevant to the
        // result; ownership stamping is therefore witness-gated via
        // StampProvenance and the executor re-resolves the FULL live
        // tenant set per walk iteration). NULL = no tenant context;
        // the executor re-resolves at execution time (design §2.2
        // item 3 / PDQ-8).
        let tenant: Option<Uuid> = state
            .interested_builds
            .iter()
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        // sched.materialize.listing-priority+2: post-merge callers
        // (the dispatch-probe partition, housekeeping) HAVE the
        // critical-path priority + unblocks at job-create time — 6b
        // already ran. Band-packed via mat_listing_priority.
        let priority = crate::db::materialization::mat_listing_priority(
            state.sched.unblocks,
            state.sched.priority,
        );
        let serving_generation = self.serving_generation();
        match self
            .db
            .create_materialization_job_fenced(
                db_id,
                drv_hash.as_str(),
                tenant,
                origin,
                carried_realized_paths.as_deref(),
                priority,
                serving_generation,
            )
            .await
        {
            Ok(FencedJobCreate::Applied {
                job_id,
                created,
                upgraded,
            }) => {
                // The view feed (merged_bug_246's re-feed half): a
                // genuinely new job (created == true) cannot have a
                // view entry (the partial-unique index guarantees no
                // unresolved job existed) — insert the fresh entry. The
                // dedup arm (created == false) re-encounters jobs the
                // view may already track — the dispatch probe
                // re-probing a PARKED node every tick, the
                // post-recovery probe pass — and must NOT reset their
                // armament state. When the dedup finds a row the view
                // does NOT track (the unhydrated-entry case), the entry
                // is REHYDRATED FROM PG — never fabricated as
                // unparked/unclaimed: a fabricated entry would let a
                // second claim race the durable holder or deliver a
                // parked job early. On a load failure the entry stays
                // absent and the BACKSTOP SWEEP is the level-triggered
                // repair (tick_backstop_materialization_jobs).
                self.feed_job_view_entry(
                    drv_hash,
                    job_id,
                    created,
                    upgraded,
                    carried_realized_paths.clone(),
                    origin,
                )
                .await;
                if created {
                    metrics::counter!(
                        "rio_scheduler_materialization_jobs_created_total",
                        "origin" => origin.as_str()
                    )
                    .increment(1);
                    self.mirror_job_creation_reset(drv_hash);
                }
                if upgraded {
                    metrics::counter!("rio_scheduler_materialization_jobs_origin_upgraded_total")
                        .increment(1);
                }
                // Interest: ensure the durable wanted relation reflects
                // EVERY live interested build of this node — not just a
                // named creating build. Builds that merged flag-on
                // already have their rows (their merge wrote them; the
                // record is an idempotent fenced upsert), but builds
                // that merged FLAG-OFF have none, and the §6 joins the
                // store executor runs (tenant resolution + wanted-set
                // resolution, both through build_wanted_outputs) come up
                // empty for them: every execution of the job then fails
                // instantly as InfraFailure("no tenant context") and the
                // build never completes — the FP-4(b) absorption gap
                // observed by the flag-transition scenario (a flag-off
                // era build's nodes get jobs after the flip that can
                // never execute).
                let live_interested: Vec<Uuid> = {
                    use crate::state::BuildStateExt;
                    self.dag
                        .node(drv_hash)
                        .map(|s| {
                            s.interested_builds
                                .iter()
                                .filter(|bid| {
                                    self.builds
                                        .get(bid)
                                        .is_some_and(|b| !b.state().is_terminal())
                                })
                                .copied()
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for build_id in live_interested {
                    self.record_wanted_for_build_node(build_id, drv_hash).await;
                }
                // The named creating build (the reprobe lane's re-merging
                // build) is covered by the loop above — it is among the
                // node's live interested builds by the time this runs.
                let _ = creating_build;
                true
            }
            Ok(FencedJobCreate::Fenced) => {
                self.note_fenced_evidence_write("materialization job create");
                false
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, error = %e, "materialization job create failed");
                false
            }
        }
    }

    /// Batched [`Self::create_materialization_job`] for the §2.1
    /// probe-partition lane (sh-007c S5): one fenced
    /// [`SchedulerDb::create_materialization_jobs_batch_fenced`] over
    /// N drvs instead of N serial `begin_fenced` round-trips. Uniform
    /// `origin`, no `creating_build` / no `carried_realized_paths` —
    /// the dispatch-probe partition (`batch_probe_cached_ready`)
    /// passes `CacheOpportunity, None, None` for every row, so the
    /// in-tx core's carried-paths inner loop is a no-op. Per-result
    /// post-processing (view feed, reset mirror, wanted-relation
    /// backfill) is unchanged from the singular — the batching gain is
    /// the ONE fenced create.
    ///
    /// `housekeeping.rs` and `merge.rs` still call the per-drv
    /// singular (out of the phase-17 hot path; sibling-sweep
    /// candidates).
    ///
    /// [`SchedulerDb::create_materialization_jobs_batch_fenced`]: crate::db::SchedulerDb::create_materialization_jobs_batch_fenced
    pub(super) async fn create_materialization_jobs_batch(
        &mut self,
        drv_hashes: &[DrvHash],
        origin: JobOrigin,
    ) {
        if !self.leader.is_leader() || drv_hashes.is_empty() {
            return;
        }
        struct Prep {
            hash: DrvHash,
            db_id: Uuid,
            tenant: Option<Uuid>,
            priority: f64,
        }
        let prep: Vec<Prep> = drv_hashes
            .iter()
            .filter_map(|h| {
                let state = self.dag.node(h)?;
                let db_id = state.db_id?;
                let tenant = state
                    .interested_builds
                    .iter()
                    .filter_map(|bid| self.builds.get(bid))
                    .find_map(|b| b.tenant_id);
                Some(Prep {
                    hash: h.clone(),
                    db_id,
                    tenant,
                    // sched.materialize.listing-priority+2: the
                    // dispatch-probe partition runs post-merge, so 6b
                    // has set sched.priority + sched.unblocks — read
                    // them directly. Band-packed via the encoder.
                    priority: crate::db::materialization::mat_listing_priority(
                        state.sched.unblocks,
                        state.sched.priority,
                    ),
                })
            })
            .collect();
        if prep.is_empty() {
            return;
        }
        let rows: Vec<crate::db::materialization::NewJobRow<'_>> = prep
            .iter()
            .map(|p| crate::db::materialization::NewJobRow {
                derivation_id: p.db_id,
                drv_hash: p.hash.as_str(),
                tenant_id: p.tenant,
                origin,
                priority: p.priority,
                carried_realized_paths: None,
            })
            .collect();
        let serving_generation = self.serving_generation();
        match self
            .db
            .create_materialization_jobs_batch_fenced(&rows, serving_generation)
            .await
        {
            Ok(crate::db::materialization::FencedBatchJobCreate::Applied(results)) => {
                for (p, r) in prep.iter().zip(results.iter()) {
                    self.feed_job_view_entry(
                        &p.hash, r.job_id, r.created, r.upgraded, None, origin,
                    )
                    .await;
                    if r.created {
                        metrics::counter!(
                            "rio_scheduler_materialization_jobs_created_total",
                            "origin" => origin.as_str()
                        )
                        .increment(1);
                        self.mirror_job_creation_reset(&p.hash);
                    }
                    if r.upgraded {
                        metrics::counter!(
                            "rio_scheduler_materialization_jobs_origin_upgraded_total"
                        )
                        .increment(1);
                    }
                    let live_interested: Vec<Uuid> = {
                        use crate::state::BuildStateExt;
                        self.dag
                            .node(&p.hash)
                            .map(|s| {
                                s.interested_builds
                                    .iter()
                                    .filter(|bid| {
                                        self.builds
                                            .get(bid)
                                            .is_some_and(|b| !b.state().is_terminal())
                                    })
                                    .copied()
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    for build_id in live_interested {
                        self.record_wanted_for_build_node(build_id, &p.hash).await;
                    }
                }
            }
            Ok(crate::db::materialization::FencedBatchJobCreate::Fenced) => {
                self.note_fenced_evidence_write("materialization job create");
            }
            Err(e) => {
                // No carrier at stake on the dispatch-probe lane: a
                // fenced/failed create is re-probed by the next
                // dispatch pass (self-healing).
                warn!(n = prep.len(), error = %e,
                      "materialization job batch create failed (best-effort; \
                       re-probed by the next dispatch pass)");
            }
        }
    }

    /// Standalone fenced wanted-relation write for one (build, node)
    /// pair — the probe-partition path's interest registration (the
    /// merge path writes the relation for all nodes inside its own tx).
    /// Best-effort: a failure leaves the durable relation behind the
    /// in-memory interest, which the next merge of the build repairs.
    pub(super) async fn record_wanted_for_build_node(
        &mut self,
        build_id: Uuid,
        drv_hash: &DrvHash,
    ) {
        let Some(db_id) = self.dag.node(drv_hash).and_then(|s| s.db_id) else {
            return;
        };
        // The backfill row is the SATURATING '{}' (all-declared) row —
        // matching the model's backfill encoding (materializationJob.qnt
        // puts OUTPUTS): a legacy build's true narrow wants are unknown
        // here, and the relation must never under-state interest width
        // (T-D2.3 step 5; widening-only divergence). GAP-FILLING ONLY
        // (ON CONFLICT DO NOTHING): a build that merged flag-on already
        // has its EXACT row and the backfill must never widen it.
        match self
            .db
            .backfill_wanted_fenced(self.serving_generation(), build_id, db_id)
            .await
        {
            Ok(
                crate::db::FencedOutcome::Applied(_) | crate::db::FencedOutcome::AlreadyResolved,
            ) => {}
            Ok(crate::db::FencedOutcome::Fenced) => {
                self.note_fenced_evidence_write("wanted relation record");
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, build_id = %build_id, error = %e,
                      "wanted-relation record failed (best-effort)");
            }
        }
    }

    /// Convert a durable pending-job row into its view entry (the
    /// recovery rebuild's per-row shape, shared by the dedup re-feed
    /// and the backstop sweep).
    fn entry_from_recovered_row(row: crate::db::open_attempts::RecoveredJobRow) -> JobViewEntry {
        JobViewEntry {
            job_id: row.job_id,
            // merged_bug_262: PG EXTRACT(EPOCH ...) can carry +inf
            // ('infinity'::timestamptz) and the > 0.0 filter passes it
            // — the clamped constructor is total where raw
            // from_secs_f64 panics (one poisoned row crash-looped
            // every leader candidate's recovery rebuild). The sibling
            // parked_at lane below was already clamped.
            parked_until: row
                .park_remaining_secs
                .map(rio_common::clamped::ClampedSecs::from_f64)
                .filter(|c| !c.is_zero())
                .map(|c| std::time::Instant::now() + c.duration()),
            // A recovered episode starts at zero strikes by
            // construction (merged_bug_014: strikes are episode-scoped
            // in-memory observations — rows carry none).
            episode: match row.claimed_by {
                Some(holder) => ClaimEpisode::Held {
                    holder: crate::state::ExecutorId::from(holder),
                    unbacked_strike: false,
                },
                None => ClaimEpisode::Unclaimed {
                    wedge_strike: false,
                },
            },
            carried_realized_paths: row.carried_realized_paths,
            parked_at: row
                .park_began_secs_ago
                .filter(|secs| *secs >= 0.0)
                .map(crate::state::RecoveredInstant::from_age_secs),
            // View-only by design (merged_bug_178): a rebuild loses at
            // most one uncharged transient deferral window.
            defer_until: None,
            // Failover-exact age (merged_bug_262: from_age_secs is
            // total — +inf clamps, never panics).
            created_at: crate::state::RecoveredInstant::from_age_secs(row.age_secs),
            // db_str_enum! emits no sqlx::Decode — TEXT decodes as
            // String and parses here (the RawJobRow precedent at
            // db/materialization.rs). A drift from the 078 CHECK
            // alphabet is a deploy bug; `CacheOpportunity` is the
            // conservative default — `from_source_viable(ChildlessLeaf,
            // CacheOpportunity)` is `true`, so an unknown origin can
            // never strand a leaf in the stalled gauge on the age-out
            // arm's gate.
            origin: crate::state::db_str::parse_or_warn_default(
                "materialization_jobs.origin",
                &row.origin,
                crate::state::JobOrigin::CacheOpportunity,
            ),
        }
    }

    /// The single view-feed discipline for every creation path
    /// (merged_bug_246): fresh rows insert fresh entries; dedup'd rows
    /// the view already tracks keep their armament state; dedup'd rows
    /// the view does NOT track are rehydrated from PG — never
    /// fabricated as unparked/unclaimed (a fabricated entry would race
    /// the durable holder or early-deliver a parked job). Under an
    /// Unavailable view the feed is dropped: the durable row is the
    /// authority and the next recovery hydrates it.
    async fn feed_job_view_entry(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Uuid,
        created: bool,
        upgraded: bool,
        carried_realized_paths: Option<Vec<String>>,
        origin: crate::state::JobOrigin,
    ) {
        if self.materialization_jobs.hydrated().is_none() {
            debug!(
                drv_hash = %drv_hash, %job_id,
                "job view unavailable: creation feed dropped (durable row is authoritative; the next recovery hydrates)"
            );
            return;
        }
        let have_entry = self.materialization_jobs.get(drv_hash).is_some();
        if created {
            // A genuinely new job cannot have a view entry (the
            // partial-unique index); insert the fresh shape.
            if let Some(view) = self.materialization_jobs.hydrated_mut() {
                view.entry_or_insert(
                    drv_hash.clone(),
                    JobViewEntry::new_unclaimed(job_id, None, origin),
                );
            }
        } else if !have_entry {
            // Dedup'd row, untracked entry: rehydrate from PG.
            let db_id = self.dag.node(drv_hash.as_str()).and_then(|s| s.db_id);
            match db_id {
                Some(db_id) => match self.db.load_unresolved_job_row(db_id).await {
                    Ok(Some(row)) => {
                        let entry = Self::entry_from_recovered_row(row);
                        if let Some(view) = self.materialization_jobs.hydrated_mut() {
                            view.entry_or_insert(drv_hash.clone(), entry);
                        }
                    }
                    Ok(None) => {
                        // Resolved between the dedup and this read —
                        // nothing unresolved to track.
                    }
                    Err(e) => {
                        warn!(
                            drv_hash = %drv_hash, %job_id, error = %e,
                            "dedup re-feed load failed; entry stays absent (backstop sweep re-feeds)"
                        );
                    }
                },
                None => {
                    debug!(drv_hash = %drv_hash, %job_id,
                           "dedup re-feed skipped: node has no db_id yet");
                }
            }
        }
        // `!created && have_entry` (dedup onto a tracked entry) and the
        // two arms above all converge here: one post-branch lookup
        // mirrors the durable set-if-null carrier semantics on the view
        // copy (display only) AND the dedup-upgrade origin write.
        //
        // Origin refresh: gated on `upgraded` — PG's dedup-upgrade is
        // upward-only (`WHERE origin <> 'pruned'`; the kept entry's
        // armament state is preserved either way). Threading the
        // `upgraded` bit (rather than writing the REQUESTED `origin`
        // unconditionally) keeps the mirror PG-authoritative: a
        // `CacheOpportunity`/`Reprobe` dedup onto an already-`Pruned`
        // row reports `upgraded=false` and the in-memory `Pruned`
        // survives, so both phase-15 arms'
        // `from_source_viable(ChildlessLeaf, origin)` gate and
        // `{origin}` label cannot misroute a pruned root through
        // evict-and-requeue (the bc84397f9 hazard the gate exists to
        // prevent). When `upgraded=true` PG wrote `origin` exactly as
        // requested, so the unguarded `= origin` is the authoritative
        // value.
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            if upgraded {
                entry.origin = origin;
            }
            if entry.carried_realized_paths.is_none()
                && carried_realized_paths
                    .as_ref()
                    .is_some_and(|c| !c.is_empty())
            {
                entry.carried_realized_paths = carried_realized_paths;
            }
        }
    }

    /// Post-commit feed of the in-memory job view from the merge
    /// transaction's created-jobs list. Called only AFTER the merge tx
    /// committed (never inside it — a rolled-back merge must leave no
    /// view entry), in the same post-commit phase that seeds states.
    ///
    /// `entry().or_insert()` (DQ-1 armament preservation, T-D2.1): the
    /// dedup arm (`created == false`) re-encounters jobs the view may
    /// already track — a pruned merge dedup-upgrading a PARKED
    /// `cache_opportunity` job, a re-merge over a claimed one — and
    /// must NOT reset their armament state (park backoff, claim
    /// holder) to a fresh unparked/unclaimed entry. A genuinely new
    /// job (`created == true`) cannot have a view entry (the
    /// partial-unique index guarantees no unresolved job existed), so
    /// or_insert inserts it.
    pub(super) async fn note_created_materialization_jobs(&mut self, created: &[CreatedJob]) {
        for job in created {
            // Same feed discipline as the probe-partition helper
            // (merged_bug_246): fresh insert for created rows, PG
            // rehydration for dedup'd rows the view does not track —
            // never a fabricated unparked/unclaimed default. (The
            // merge in-tx creation lanes never carry realized paths —
            // only the stale_reset post-tx site does; recovery and the
            // rehydration read the column.)
            self.feed_job_view_entry(
                &job.drv_hash,
                job.job_id,
                job.created,
                job.upgraded,
                None,
                job.origin,
            )
            .await;
            if job.created {
                metrics::counter!(
                    "rio_scheduler_materialization_jobs_created_total",
                    "origin" => job.origin.as_str()
                )
                .increment(1);
                self.mirror_job_creation_reset(&job.drv_hash);
            }
            if job.upgraded {
                metrics::counter!("rio_scheduler_materialization_jobs_origin_upgraded_total")
                    .increment(1);
            }
        }
    }

    /// Mirror the 085_materialization_reset_class job-creation reset row into the node's
    /// in-memory history (the durable row was written inside the
    /// creating transaction): the in-memory [`Self::mat_counters`]
    /// re-window immediately, identically to a post-failover suffix
    /// reload. The mirrored record's attempt_id differs from the
    /// committed row's (both are independent mints); no fold reads it.
    fn mirror_job_creation_reset(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        let Some(db_id) = state.db_id else {
            return;
        };
        let record = crate::db::attempts::AttemptRow::new_reset(
            db_id,
            crate::state::OutcomeClass::MaterializationReset,
            crate::state::ReportingParty::Scheduler,
            0,
            crate::state::AttemptKind::Materialization,
        )
        .to_record();
        state.push_attempt_record(record);
        self.refresh_retry_view(drv_hash);
    }

    /// Project the node's materialization-job state for pull admission
    /// (the kernel's `JobView` input).
    pub(super) fn materialization_job_view(
        &self,
        drv_hash: &DrvHash,
        pulling_identity: &ExecutorId,
    ) -> rio_evidence_kernel::pull::JobView {
        use rio_evidence_kernel::pull::JobView;
        let Some(view) = self.materialization_jobs.hydrated() else {
            // Fail-closed projection (merged_bug_246): an Unavailable
            // view must never answer `None` — a build pull would fall
            // into the kernel's None→DeliverNew cell and race a job we
            // cannot see, and a materialization claim would get `Gone`
            // (the store treats Gone as authoritative: it resolves the
            // claim credential and bars re-mints for its remint
            // cooldown — a degraded-term Gone strands the armed
            // action for at least that window, per pass, fleet-wide).
            // `Pending { parked: true }` maps EVERY kind to
            // NotYetReady while keeping the kernel's token/fence
            // rejections dominant (check order is load-bearing).
            return JobView::Pending { parked: true };
        };
        match view.get(drv_hash) {
            None => JobView::None,
            // One law, three surfaces (bug_170): admission projects
            // the SAME `claimability` the KEDA gauge and the leader
            // listing read, so a listed job is admittable by
            // construction. Park and the view-only transient deferral
            // (merged_bug_178) both project `parked: true` — the claim
            // is refused with NotYetReady until they lapse; PD-20 and
            // the stalled gauge do NOT read the deferral — defer is
            // pacing for the NEXT claim, never park evidence.
            Some(entry) => match entry.claimability(std::time::Instant::now()) {
                Claimability::Claimed => JobView::Claimed {
                    held_by_puller: entry.holder().is_some_and(|h| h == pulling_identity),
                },
                Claimability::Parked | Claimability::Deferred => JobView::Pending { parked: true },
                Claimability::ClaimableNow => JobView::Pending { parked: false },
            },
        }
    }

    /// Note a materialization claim in the in-memory view (called by
    /// the pull mint after the fenced transaction committed for a
    /// materialization-kind delivery). Reachable only flag-on.
    // r[impl obs.metric.scheduler]
    pub(super) fn note_materialization_claimed(&mut self, drv_hash: &DrvHash, holder: &ExecutorId) {
        if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
            // A fresh mint resets the ghost strike (merged_bug_055 C).
            entry.mint_claim(holder.clone());
        }
        // T-6.2 lifecycle counter: one increment per delivered claim
        // (the open attempt's mint committed — this is called only on
        // that path). Pairs with jobs_created_total (supply) and
        // jobs_resolved_total (drain) for the dashboard rates.
        metrics::counter!("rio_scheduler_materialization_claims_total").increment(1);
    }

    // r[impl sched.materialize.job+2]
    /// BC-4 (Phase B): emit the SUBSTITUTING DerivationEvent at
    /// materialization-claim intake. The event KIND is wire-retained;
    /// its emission site moved here from the walk spawn (deleted with
    /// Phase D'). The gateway's
    /// actSubstitute/actCopyPath pair creation keys on this kind
    /// (rio-gateway handler/build.rs `relay_derivation_status`,
    /// the SUBSTITUTING arm) and is untouched (BC-4's contract). Keeps
    /// the walk-era emission shape on the wire: one event
    /// per interested build + a progress snapshot so the queued/running
    /// flip is visible.
    ///
    /// Called by the pull mint INSTEAD of `emit_assignment_started` for
    /// materialization-kind mints — STARTED is one of the gateway's
    /// pair-STOP triggers, so emitting it here would close the pair the
    /// instant it opened (and misrepresent substitution work as a
    /// builder dispatch).
    ///
    /// Reachable only flag-on (no materialization mint exists flag-off),
    /// so flag-off event streams are byte-identical to as-built.
    pub(super) fn emit_materialization_claimed(&mut self, drv_hash: &DrvHash) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let drv_path = state.drv_path().to_string();
        // The same payload the walk-spawn site sends: the paths the
        // store will fetch. `output_paths` is set by completion only;
        // pre-completion the expected paths are the fetch targets —
        // except the floating-CA stale-reset lane, where the expected
        // slots are [""] placeholders and the job's carried realized
        // paths (migration 082) ARE the fetch targets (the walk-era
        // emission carried them too; display copy of the carrier).
        let carried = self
            .materialization_jobs
            .get(drv_hash)
            .and_then(|e| e.carried_realized_paths.clone())
            .filter(|c| !c.is_empty());
        let output_paths = match carried {
            Some(c) if state.expected_output_paths.iter().all(|p| p.is_empty()) => c,
            _ => state.expected_output_paths.clone(),
        };
        let event = rio_proto::types::build_event::Event::Derivation(
            rio_proto::types::DerivationEvent::substituting(drv_path, output_paths),
        );
        for build_id in self.get_interested_builds(drv_hash) {
            self.events.emit(build_id, event.clone());
            // build_summary counts the now-Assigned/Running node as
            // running — emit a progress snapshot so the queued/running
            // flip is visible (matches the walk site and
            // `emit_assignment_started`).
            self.emit_progress(build_id);
        }
    }

    /// Whether the node carries an unresolved, UNCLAIMED materialization
    /// job — the §2.6 "substitution backlog" predicate read by the
    /// snapshot bucket re-sourcing and the spawn-intent filter.
    ///
    /// Pending AND parked jobs both count: the consumers' question is
    /// "does store-side substitution work exist for this node", and a
    /// parked job is exactly that (work the store will resume once its
    /// backoff expires). Claimed jobs do NOT count — their nodes are
    /// Assigned/Running and surface through `running_derivations`.
    ///
    /// Reads only the in-memory view (a droppable cache of the durable
    /// job table, rebuilt at recovery).
    pub(super) fn has_pending_unclaimed_job(&self, drv_hash: &str) -> bool {
        match self.materialization_jobs.hydrated() {
            // Fail closed (merged_bug_246): with no trustworthy view,
            // assume store-side work MAY exist — the exclusion
            // consumers (spawn-intent filter, queued-bucket
            // disjointness) must not spawn builders for nodes whose
            // jobs we cannot see. Heals at the next successful
            // recovery.
            None => true,
            Some(view) => view
                .get(drv_hash)
                .is_some_and(|entry| entry.holder().is_none()),
        }
    }

    /// Whether the node carries a job the store could claim RIGHT NOW
    /// (unclaimed, not parked, not deferred — `claimability()`'s three
    /// axes) — the KEDA "substituting backlog" question (bug_252):
    /// non-claimable jobs are pacing, not claimable demand, so they
    /// must not hold store replicas up. Parked jobs stay visible via
    /// `rio_scheduler_materialization_stalled`; a deferred job
    /// (`defer_until`, bounded <=300s) is counted in neither gauge for
    /// that window (m032 — stated in the substituting_derivations
    /// HELP).
    ///
    /// Unavailable view → `false` (an honest zero: the gauge advertises
    /// claimable work to autoscalers; advertising unverifiable work
    /// would scale the store against a view we don't have).
    pub(super) fn has_claimable_job(&self, drv_hash: &str, now: std::time::Instant) -> bool {
        // The SAME law admission and the listing read (bug_170): an
        // active park OR transient deferral (merged_bug_178) is not
        // claimable demand — admission refuses it, so advertising it
        // would scale the store against refusals.
        self.materialization_jobs
            .get(drv_hash)
            .is_some_and(|entry| entry.claimability(now) == Claimability::ClaimableNow)
    }

    /// Whether ANY unresolved job exists for the node (pending,
    /// claimed, or parked) — the reap-survivor armament predicate
    /// (T-4.3): a survivor carrying an unresolved job needs nothing
    /// from the removal-survivor settlement, whatever the job's
    /// sub-state, because every job state has its own armed action
    /// (the §3.3 settlement totality).
    pub(super) fn has_unresolved_job(&self, drv_hash: &str) -> bool {
        match self.materialization_jobs.hydrated() {
            // Fail closed: treat the node as armed-elsewhere so the
            // reap-survivor settlement does not run destructive
            // promotion/fail-fast against jobs we cannot see
            // (merged_bug_246; the backstop sweep + next recovery are
            // the level-triggered repair).
            None => true,
            Some(view) => view.contains_key(drv_hash),
        }
    }

    // r[impl sched.materialize.job+2]
    /// T-4.3 (Phase B): rebuild the in-memory job view from PG at
    /// recovery. Without this, a failed-over leader's empty view
    /// answers `JobView::None` to every materialization claim → the
    /// kernel's kinded table answers `Gone` → the store executor
    /// (which treats Gone as authoritative — the credential resolves
    /// and the job enters the remint cooldown) re-claims at the
    /// earliest one cooldown later, and the armed action is stranded
    /// until a dispatch-probe tick happens to lazily re-feed the view
    /// (the F10/L1 class).
    ///
    /// The rebuild mirrors ALL unresolved state: claim holders (so the
    /// open attempt re-delivers to its holder and refuses everyone
    /// else) and park expiries (so parked jobs keep answering
    /// NotYetReady until their durable backoff lapses).
    pub(super) fn rebuild_materialization_job_view(
        &mut self,
        rows: Vec<crate::db::open_attempts::RecoveredJobRow>,
    ) {
        // Per-row conversion shared with the dedup re-feed and the
        // backstop sweep; the dwell anchor rides as RecoveredInstant
        // age-data — exact even when the park pre-dates this leader's
        // boot (merged_bug_300: no silent dwell restart).
        let entries = rows.into_iter().map(|row| {
            (
                DrvHash::from(row.drv_hash.as_str()),
                Self::entry_from_recovered_row(row),
            )
        });
        self.materialization_jobs.rebuild(entries);
    }
}

// ──────────────────────────────────────────────────────────────────────
// The consumption transaction (design §2.4): Success coverage + the
// four-arm Unobtainable routing. The routing core is PURE (no IO, no
// clocks — kani-liftable per design §9.4); the consumption handler
// wires it to the fenced db operations.
// ──────────────────────────────────────────────────────────────────────

// The pure routing core lives in rio-evidence-kernel (bughunt wave,
// A4): rio-scheduler is a bin crate and the kani gate is lib-only —
// the kernel sweep proves the cells; this module keeps the wiring.
pub(crate) use rio_evidence_kernel::routing::{
    LiveWanted, Refusal, ReprobeAnswer, RoutingInputs, UnobtainableRouting, route_unobtainable,
    success_covers_live_wanted,
};

/// THE decode chokepoint of the wire refusal (bug_084): raw
/// `Unobtainable.refusal` (field 6) → the kernel's closed [`Refusal`]
/// alphabet. MUST consume the RAW wire value via `try_from` — NEVER
/// the prost accessor, whose unknown-value default (UNSPECIFIED)
/// would launder a FUTURE refusal axis into the clean lane, repeating
/// bug_084's shape at the next alphabet evolution. Decode law: a
/// known nonzero value wins; 0/absent is `Refusal::None` (field 5,
/// `trust_refused`, is NOT consulted — it is a decode-ignored
/// coherence echo per SIGNED Q6, bughunt-5 §5-S 2026-06-09: --wipe
/// rollout, no old-store skew lane); an unknown nonzero value is
/// `Refusal::Unrecognized` — conservative refusal routing, kept for
/// future-evolution robustness, not as a rollout hedge.
pub(super) fn refusal_from_wire(refusal: i32) -> Refusal {
    use rio_proto::types::UnobtainableRefusal as Wire;
    match Wire::try_from(refusal) {
        Ok(Wire::Unspecified) => Refusal::None,
        Ok(Wire::Trust) => Refusal::Trust,
        Ok(Wire::Content) => Refusal::Content,
        Ok(Wire::TrustAndContent) => Refusal::TrustAndContent,
        Err(_) => Refusal::Unrecognized,
    }
}

/// The PD-20 from-source-viable predicate (T-D2.2/PD-D4): a parked or
/// aged-out materialization job may resolve `ResolvedFromSource` iff
/// the node's durable closure evidence is `Vouched`/`Pending` (a
/// buildable dependency closure exists), or `ChildlessLeaf` with a
/// non-`Pruned` origin (merged_bug_301: a structural leaf has no
/// closure to be missing — from-source is viable; a pruned root's
/// closure was deliberately dropped and must NOT requeue). `Holed`
/// evidence and `ChildlessLeaf+Pruned` stay in the view (the stalled
/// gauge counts them). Shared by both phase-15 arms so the gate is one
/// edit point — a refactor that drops the `ChildlessLeaf` origin
/// conjunct cannot drift between the parked-conversion and age-out
/// settlements.
fn from_source_viable(
    evidence: rio_evidence_kernel::ClosureEvidence,
    origin: Option<crate::state::JobOrigin>,
) -> bool {
    use rio_evidence_kernel::ClosureEvidence;
    match evidence {
        ClosureEvidence::Vouched | ClosureEvidence::Pending => true,
        ClosureEvidence::ChildlessLeaf => {
            origin.is_some_and(|o| o != crate::state::JobOrigin::Pruned)
        }
        ClosureEvidence::Holed => false,
    }
}

/// Per-tick quota on the aged-out arm's serial
/// `classify_durable_evidence` PG awaits (sh-044 r2): the aged-out
/// arm's per-entry evidence read is unbounded over the
/// first-post-deploy/post-outage backlog (the project's own comments
/// cite "hundreds in one pass" and a 7725-row backlog); at ~3 ms RTT
/// a 3500+ backlog exceeds 11 s inside `handle_tick` phase-15 —
/// guard-isolated, so `/readyz` shed rather than self-fence, but
/// still a multi-second actor stall. The cap drains the backlog over
/// `⌈backlog/256⌉` ticks; over-quota entries stay in the view
/// unchanged (the predicate matches again next tick).
const MAX_AGEOUT_PER_TICK: usize = 256;

/// One report's batched-consumption intent (sh-007c S6 phase B
/// product): the per-item routing decision over PREFETCHED inputs,
/// with no PG await. The flush body collects these, runs ONE
/// `close_and_resolve_materialization_batch_fenced`, then applies
/// each companion in phase D.
#[derive(Debug)]
pub(super) struct BatchIntent {
    /// The report's exec_id (close key + per-item disposition key).
    pub(super) exec_id: Uuid,
    /// The DAG key (for the in-mem ledger sync + companion).
    pub(super) drv_hash: DrvHash,
    /// Executor identity the attempt is bound to.
    pub(super) executor: ExecutorId,
    /// The companion to run on a settled close.
    pub(super) companion: BatchedCompanion,
}

/// The per-item companion the batched flush runs after the settled
/// close (sh-007c S6 phase D). Covers exactly the UNCHARGED arms
/// (Success / RetryLater / Aborted / zero-width) — every charged or
/// probe-bearing arm routes to the per-item slow path instead.
#[derive(Debug)]
pub(super) enum BatchedCompanion {
    /// `complete_materialization_for_live_interest`: resolve the job
    /// `ResolvedSuccess`, settle the view, stamp carried paths, push
    /// to `pending_walk_completed`. Carries everything the companion
    /// needs so phase D is PG-free (the resolve rode the batch tx).
    Complete {
        /// Durable job row to resolve (None = no row → AlreadyResolved).
        job_id: Option<Uuid>,
        /// Migration-082 floating-CA realized paths.
        carried_paths: Vec<String>,
        /// Signed Q2 per-path verified-tenant sets.
        walk_verified: std::collections::HashMap<Vec<u8>, Vec<Uuid>>,
    },
    /// `companion_release` (the ReArm / RetryLater / Aborted /
    /// zero-width / coverage-miss composition): release the claim
    /// atomically, optionally deferring the next claim.
    Release {
        /// View-only deferral; `None` = re-arm immediately.
        defer: Option<std::time::Duration>,
        /// `Some(_)` only for the zero-width arm — counted post-settle
        /// (bug_086: the witness-gated event).
        zero_width_exec: Option<Uuid>,
    },
}

/// Phase-D apply result (sh-027 §3 phase-D batch): either an
/// immediate ack (`Complete` arm — its companion already ran) or a
/// release the phase-D loop COLLECTS for one
/// [`DagActor::companion_release_batch`] after the loop. The reply is
/// `Ok(())` for both — the close was settled either way; only the
/// release's requeue is deferred.
pub(super) enum CompanionResult {
    /// The companion ran inline (the `Complete` arm). Ack now.
    Ack(MatAck),
    /// The release deferred to the post-loop batch. Ack now (the close
    /// IS settled); the requeue rides one slice-wide
    /// `requeue_after_attempt`.
    DeferredRelease(DeferredRelease),
}

/// One phase-D `BatchedCompanion::Release` intent the phase-D loop
/// collected for [`DagActor::companion_release_batch`]: the same
/// inputs the per-item [`DagActor::companion_release`] takes (the
/// settled-close witness was consumed at the construction site —
/// `apply_batched_companion`'s `Settled` arm — so this carrying it
/// would be redundant).
#[derive(Debug)]
pub(super) struct DeferredRelease {
    /// The DAG key (in-mem release + requeue).
    pub(super) drv_hash: DrvHash,
    /// The expected holder (the compare-and-clear key — bug_170).
    pub(super) executor: ExecutorId,
    /// View-only deferral; `None` = re-arm immediately.
    pub(super) defer: Option<std::time::Duration>,
}

impl BatchIntent {
    /// The `(job_id, to_state, resolution_exec_id)` triple this intent
    /// contributes to the batch resolve (only `Complete` resolves).
    pub(super) fn resolve(&self) -> Option<(Uuid, crate::state::JobState, Option<Uuid>)> {
        match &self.companion {
            BatchedCompanion::Complete {
                job_id: Some(job_id),
                ..
            } => Some((
                *job_id,
                crate::state::JobState::ResolvedSuccess,
                Some(self.exec_id),
            )),
            _ => None,
        }
    }
}

impl DagActor {
    // r[impl sched.materialize.routing+7]
    /// Consume one materialization outcome (the §2.4 consumption
    /// transaction). Reachable only flag-on in practice (no
    /// materialization attempt can exist otherwise) — but ALWAYS wired
    /// (design §4 "always-on regardless of flags": reports for existing
    /// attempts must drain after an ON→OFF flip).
    /// Takes the MATERIALIZATION witness: a build attempt cannot reach
    /// the consumption transaction — the cross-kind call no longer
    /// typechecks (the witness twin of `close_pull_attempt_uncharged`'s
    /// `&BuildAttempt`).
    /// Returns the [`MatAck`] witness — constructible only by the five
    /// settled-close companions and the fenced arm of
    /// [`Self::close_for_consumption`], so an ack with the assignment
    /// still open no longer typechecks (bug_182). A close that fails
    /// to become durable propagates
    /// [`super::pull::PullRejection::ConsumptionNotDurable`] (the NACK
    /// law) so the store re-delivers instead of the charged
    /// 'unreported' establishment settling the row an hour later.
    /// Takes the INNER outcome: the None-payload intake arm stays at
    /// the report intake (`Result<(), _>` — no consumption happened,
    /// no witness to mint).
    pub(super) async fn consume_materialization_outcome(
        &mut self,
        exec_id: Uuid,
        attempt: &crate::db::open_attempts::MatAttempt,
        outcome: rio_proto::types::materialization_outcome::Outcome,
        // The kind-uniform admission witness (bug_134): minted ONLY by
        // the kernel's `fold_report`, so a consumption call that did
        // not pass the open-attempt/not-yet-classified fold does not
        // typecheck. Spent here by value — one admission, one
        // consumption pass.
        _admission: rio_evidence_kernel::pull::ProcessAdmission,
    ) -> Result<MatAck, super::pull::PullRejection> {
        use rio_proto::types::materialization_outcome::Outcome;
        let drv_hash = DrvHash::from(attempt.core.drv_hash.as_str());
        let serving_generation = self.serving_generation();
        let executor = ExecutorId::from(attempt.core.executor_id.as_str());

        // The unresolved job this attempt executes (PG is the
        // authority; the in-memory view is a cache). The row carries
        // the ORIGIN — `'pruned'` is the durable arm-3 settlement
        // discriminator (T-D2.1; the walk-era pruned column's
        // successor).
        let job = self
            .db
            .unresolved_job_for_derivation(attempt.core.derivation_id)
            .await
            .map_err(|e| {
                super::pull::PullRejection::Internal(format!("materialization job lookup: {e}"))
            })?;
        let job_id = job.as_ref().map(|(id, _, _)| *id);
        let pruned_origin = job
            .as_ref()
            .is_some_and(|(_, origin, _)| matches!(origin, JobOrigin::Pruned));
        // Realized-path carrier (migration 082): the floating-CA
        // stale-reset lane's fetch targets. Unioned into the wanted
        // set below, so coverage is non-vacuous exactly when a carrier
        // is present (the conservative-absent saturation arm and every
        // non-carried shape are untouched — the scope the records'
        // collision analysis settled).
        let carried_paths: Vec<String> = job
            .as_ref()
            .and_then(|(_, _, carried)| carried.clone())
            .unwrap_or_default();

        // 1. The live effective wanted set (the §6 join), resolved to
        //    store paths — the presence-re-check half of D7's closure.
        let mut wanted_union = self
            .live_wanted_paths_for(attempt.core.derivation_id, &drv_hash)
            .await
            .map_err(|e| super::pull::PullRejection::Internal(format!("wanted-union read: {e}")))?
            .unwrap_or_default();
        // r[impl sched.merge.stale-substitutable+3]
        for p in &carried_paths {
            if !p.is_empty() && !wanted_union.contains(p) {
                wanted_union.push(p.clone());
            }
        }
        // The non-empty witness (merged_bug_194): no verifiable wanted
        // set even after the carrier union means NOTHING can be
        // verified for live interest — every completion would be
        // vacuous. Conservative branch for BOTH Success and
        // Unobtainable: the job stays pending and the node leaves the
        // mint's Running state (the ReArm posture; the next claim or
        // the zero-interest closer settles it).
        let Some(live_wanted_paths) = LiveWanted::new(wanted_union) else {
            // bug_182 zero-width reshape: this arm used to release the
            // claim with the assignment still OPEN and ack — the open
            // attempt then hid the job from every listing (anti-join)
            // fleet-wide until the establishment sweep. Now it composes
            // exactly like RetryLater: close UNCHARGED first, then the
            // release companion with a defer, so a persistent
            // zero-width condition is re-listable yet cannot hot-loop
            // claim/close (decision recorded at this arm).
            return match self
                .close_for_consumption(exec_id, &drv_hash, None, serving_generation)
                .await
            {
                CloseOutcome::Settled(close) => {
                    // bug_282: the width-ZERO event class — its own
                    // counter and warn latch. bug_086: counted ONLY
                    // from the settled close (the witness is demanded
                    // by the event constructor): a Deferred close is
                    // the successor leader's event, and a NotDurable
                    // NACK redelivers the SAME outcome — neither may
                    // tick a consumption counter.
                    crate::state::note_width_event(crate::state::WidthEvent::NoVerifiableSet {
                        exec_id,
                        settled: &close,
                    });
                    warn!(
                        %exec_id, drv_hash = %drv_hash,
                        "no verifiable live-wanted path set; closed uncharged and deferred"
                    );
                    Ok(self
                        .companion_release(
                            &drv_hash,
                            &executor,
                            Some(std::time::Duration::from_secs(
                                RETRY_LATER_DEFAULT_DEFER_SECS,
                            )),
                            close,
                        )
                        .await)
                }
                CloseOutcome::Deferred(ack) => Ok(ack),
                CloseOutcome::NotDurable => Err(super::pull::PullRejection::ConsumptionNotDurable),
            };
        };

        match outcome {
            Outcome::Success(s) => {
                // Success appends NOTHING to the ledger (design §2.4 —
                // success is not a fold event). Coverage decides
                // Complete vs ReArm (the CE-17 class). The close runs
                // through the ack-law chokepoint: Fenced acks inert
                // (deposed believer, signed Q20), Failed NACKs.
                // Signed Q2: parse the wire's per-path verified-tenant
                // sets ONCE (sha256(path) -> tenant uuids); malformed
                // uuids are dropped (stamping is conservative).
                let walk_verified: std::collections::HashMap<Vec<u8>, Vec<Uuid>> = {
                    use sha2::Digest;
                    s.verified_tenants
                        .iter()
                        .map(|pt| {
                            (
                                sha2::Sha256::digest(pt.store_path.as_bytes()).to_vec(),
                                pt.verified_tenant_ids
                                    .iter()
                                    .filter_map(|t| t.parse::<Uuid>().ok())
                                    .collect::<Vec<Uuid>>(),
                            )
                        })
                        .collect()
                };
                match self
                    .close_for_consumption(exec_id, &drv_hash, None, serving_generation)
                    .await
                {
                    CloseOutcome::Deferred(ack) => Ok(ack),
                    CloseOutcome::NotDurable => {
                        Err(super::pull::PullRejection::ConsumptionNotDurable)
                    }
                    CloseOutcome::Settled(close) => {
                        if success_covers_live_wanted(
                            &s.ingested_paths,
                            &s.verified_paths,
                            &live_wanted_paths,
                        ) {
                            // The build-success path: outputs are
                            // present and verified in the store; one
                            // chokepoint resolves, settles the view,
                            // stamps the carrier, and completes for
                            // live interest.
                            Ok(self
                                .complete_materialization_for_live_interest(
                                    &drv_hash,
                                    job_id,
                                    exec_id,
                                    &executor,
                                    &carried_paths,
                                    serving_generation,
                                    close,
                                    walk_verified,
                                )
                                .await)
                        } else {
                            // Coverage failed — interest grew between
                            // execution and consumption, or the report
                            // did not cover the carried realized paths
                            // (the floating-CA stale-reset shape): the
                            // job stays pending; the next claim covers
                            // it. The release companion re-arms
                            // atomically (re-arm without the reassign
                            // is the documented wedge).
                            Ok(self
                                .companion_release(&drv_hash, &executor, None, close)
                                .await)
                        }
                    }
                }
            }
            Outcome::Unobtainable(u) => {
                // The charge row: kind=materialization — visible only to
                // the materialization budget, never to build budgets.
                let mut row = crate::db::attempts::AttemptRow::new(
                    attempt.core.derivation_id,
                    crate::state::OutcomeClass::MaterializationUnobtainable,
                    crate::state::ReportingParty::Worker,
                    crate::state::AttemptKind::Materialization,
                );
                row.exec_id = Some(exec_id);
                row.executor_id = Some(executor.clone());
                row.error_msg = (!u.cause.is_empty()).then(|| u.cause.clone());
                let prior_unobtainable = self.mat_counters(&drv_hash).unobtainable_since_reset;
                let close = match self
                    .close_for_consumption(exec_id, &drv_hash, Some(row), serving_generation)
                    .await
                {
                    CloseOutcome::Settled(close) => close,
                    // view-settlement gate: a deposed close runs no
                    // routing — ack inert, the successor's sweep owns
                    // the row (signed Q20).
                    CloseOutcome::Deferred(ack) => return Ok(ack),
                    // A failed close NACKs: the store re-delivers the
                    // SAME outcome and the idempotent close retries.
                    CloseOutcome::NotDurable => {
                        return Err(super::pull::PullRejection::ConsumptionNotDurable);
                    }
                };

                // 2. The four-arm routing. Arms 0–2 decide without the
                //    re-probe; the probe is fetched only for arm 3. The
                //    evidence is classified over the DURABLE relation
                //    (T-D2.2/PD-D4: pg.edges + pg.status + live
                //    co-owning build links — the three-part strict
                //    criterion), never the in-memory child set, so a
                //    reap-truncated or post-failover view cannot
                //    launder a verdict (the F9 hazard).
                let durable_evidence = self
                    .db
                    .classify_durable_evidence(attempt.core.derivation_id)
                    .await
                    .map_err(|e| {
                        super::pull::PullRejection::Internal(format!(
                            "durable evidence classification: {e}"
                        ))
                    })?;
                // The arm-3 discriminator (finding 11) is the consumed
                // job's origin — `pruned_origin` was read from the job
                // row above (T-D2.1: the durable fact, not the
                // in-memory column).
                // merged_bug_193: the wanted-miss vs reference-miss
                // partition. New stores report references in their own
                // cell; an OLD store (bounded skew, scheduler-before-
                // store rollout) lumps them into missing_paths — the
                // consumer partitions: a missing entry outside
                // expected ∪ carried ∪ live-wanted cannot be a wanted
                // miss (wanted paths are drawn from exactly those
                // sets), so it is a reference miss.
                let expected_paths: Vec<String> = self
                    .dag
                    .node(&drv_hash)
                    .map(|st| st.expected_output_paths.clone())
                    .unwrap_or_default();
                let (missing_wanted, skew_references): (Vec<String>, Vec<String>) =
                    u.missing_paths.iter().cloned().partition(|p| {
                        live_wanted_paths.contains(p.as_str())
                            || carried_paths.contains(p)
                            || expected_paths.contains(p)
                    });
                let mut missing_references = u.missing_reference_paths.clone();
                missing_references.extend(skew_references);
                // bug_084: decode the typed refusal at THE chokepoint
                // before any probe decision — the routing consumes the
                // closed alphabet, not the raw wire value.
                let refusal = refusal_from_wire(u.refusal);
                // The arm-3 probe is needed exactly when arm 0 cannot
                // apply (a live-wanted miss OR a confirmed closure
                // hole), the evidence is leaf/holed (arms 1/2 decide
                // without it), AND no typed refusal rode the outcome
                // (bug_084): under any refusal the presence-only FMP
                // round-trip is doomed — it cannot answer the trust/
                // content question the refusal already settled — and
                // its failure path's bare re-arm below would re-meet
                // the same refusal; the kernel's arm-3 refusal match
                // owns the verdict without it.
                let needs_probe = (missing_wanted
                    .iter()
                    .any(|p| live_wanted_paths.contains(p.as_str()))
                    || !missing_references.is_empty())
                    && matches!(
                        durable_evidence,
                        rio_evidence_kernel::ClosureEvidence::ChildlessLeaf
                            | rio_evidence_kernel::ClosureEvidence::Holed
                    )
                    && !refusal.is_refused();
                let reprobe = if needs_probe {
                    match self
                        .reprobe_live_wanted_paths(&drv_hash, live_wanted_paths.paths())
                        .await
                    {
                        Some(answer) => Some(answer),
                        None => {
                            // B3: the re-probe RPC itself failed — an
                            // indeterminate answer never fail-fasts.
                            // Atomic release (merged_bug_015): the
                            // bare re-arm here was the wedge.
                            return Ok(self
                                .companion_release(&drv_hash, &executor, None, close)
                                .await);
                        }
                    }
                } else {
                    None
                };
                let routing = route_unobtainable(&RoutingInputs {
                    missing_paths: &missing_wanted,
                    missing_references: &missing_references,
                    verified_paths: &u.verified_paths,
                    live_wanted_paths: &live_wanted_paths,
                    durable_evidence,
                    prior_unobtainable_count: prior_unobtainable,
                    reprobe,
                    pruned_origin,
                    // bug_084 (supersedes the merged_bug_263 bool):
                    // the typed refusal alphabet rides the wire into
                    // the settlement through the one decode
                    // chokepoint above.
                    refusal,
                });
                // 3. Execute the routing — every arm spends the
                //    settled-close witness on exactly one companion.
                let ack = match routing {
                    UnobtainableRouting::CompleteForLiveInterest => {
                        // The moot arm completes through the SAME
                        // chokepoint as Success — the carrier stamp
                        // cannot be skipped by arm choice
                        // (merged_bug_055: this arm completed with the
                        // [""] placeholder pre-fix).
                        self.complete_materialization_for_live_interest(
                            &drv_hash,
                            job_id,
                            exec_id,
                            &executor,
                            &carried_paths,
                            serving_generation,
                            close,
                            // Signed Q2: the Unobtainable wire carries
                            // no per-path verified-tenant sets — the
                            // moot completion stamps NOTHING; interest
                            // stays open and a later walk's Success
                            // (which does carry sets) stamps lawfully.
                            std::collections::HashMap::new(),
                        )
                        .await
                    }
                    UnobtainableRouting::ReArm => {
                        // Atomic release (merged_bug_015): re-arm +
                        // requeue in ONE step — the bare re-arm here
                        // held the node Running with no armed action.
                        self.companion_release(&drv_hash, &executor, None, close)
                            .await
                    }
                    UnobtainableRouting::ResolveFromSource => {
                        self.companion_resolve_from_source(
                            &drv_hash,
                            job_id,
                            exec_id,
                            &executor,
                            serving_generation,
                            close,
                        )
                        .await
                    }
                    UnobtainableRouting::FailFast => {
                        self.companion_fail_fast(
                            &drv_hash,
                            job_id,
                            exec_id,
                            &executor,
                            serving_generation,
                            close,
                        )
                        .await
                    }
                };
                Ok(ack)
            }
            Outcome::InfraFailure(f) => {
                // The infra charge: counts toward the materialization
                // budget and toward NOTHING else. Never fail-fasts,
                // never routes from source (B3). Charge + park verdict
                // are ONE chokepoint (close, verdict, and requeue all
                // inside) — it mints the ack itself and a non-durable
                // close propagates as the NACK.
                self.charge_materialization_infra(
                    exec_id,
                    attempt.core.derivation_id,
                    &drv_hash,
                    &executor,
                    job_id,
                    crate::state::ReportingParty::Worker,
                    (!f.detail.is_empty()).then(|| f.detail.clone()),
                    None,
                    None,
                    serving_generation,
                )
                .await
                .map(|(ack, _settled)| ack)
            }
            Outcome::RetryLater(r) => {
                // merged_bug_178: transient by the substituter's own
                // contract (raced placeholder slot / upstream 429) —
                // the attempt proves nothing about upstream content or
                // store health. Close UNCHARGED (no ledger row of any
                // class: a 429 wave must never burn the park budget),
                // defer the job's next claim (VIEW-ONLY — PD-20 and
                // the stalled gauge never read it), and re-arm
                // atomically (the InfraFailure posture: re-arm without
                // the reassign is the documented wedge).
                let retry_after = std::time::Duration::from_secs(
                    r.retry_after_secs
                        .clamp(0, RETRY_LATER_MAX_DEFER_SECS)
                        .max(RETRY_LATER_DEFAULT_DEFER_SECS),
                );
                tracing::info!(
                    %exec_id,
                    drv_hash = %drv_hash,
                    class = %r.class,
                    detail = %r.detail,
                    defer_secs = retry_after.as_secs(),
                    "transient materialization failure; closing uncharged and deferring"
                );
                match self
                    .close_for_consumption(exec_id, &drv_hash, None, serving_generation)
                    .await
                {
                    CloseOutcome::Settled(close) => Ok(self
                        .companion_release(&drv_hash, &executor, Some(retry_after), close)
                        .await),
                    CloseOutcome::Deferred(ack) => Ok(ack),
                    CloseOutcome::NotDurable => {
                        Err(super::pull::PullRejection::ConsumptionNotDurable)
                    }
                }
            }
            Outcome::Aborted(a) => {
                // Charge-free close (owner default Q3, 2026-06-03 — AD5
                // parity for the materialization kind): the worker was
                // told to stop (SIGTERM during a store rollout/drain),
                // which is evidence about the WORKER's lifecycle, not
                // about the upstream or the job. NO ledger row of any
                // class — routine store rollouts must not burn the
                // 3-attempt park budget (merged_bug_189) — and the
                // budget keeps its flapping-replica blindness by
                // decision. The job returns to pending claimable and
                // the node leaves the mint's Running state (re-arm
                // without the reassign is the documented wedge).
                tracing::info!(
                    %exec_id,
                    drv_hash = %drv_hash,
                    detail = %a.detail,
                    "materialization walk aborted by the worker; closing charge-free"
                );
                // view-settlement gate (sched.materialize.view-settlement):
                // the rearm + requeue companions run only when the
                // charge-free close SETTLED — a deposed believer's fenced
                // Aborted close mutates nothing it no longer owns, and a
                // failed close leaves the establishment sweep as the
                // armed action (the same composition as every other
                // consumption arm).
                match self
                    .close_for_consumption(exec_id, &drv_hash, None, serving_generation)
                    .await
                {
                    CloseOutcome::Settled(close) => Ok(self
                        .companion_release(&drv_hash, &executor, None, close)
                        .await),
                    CloseOutcome::Deferred(ack) => Ok(ack),
                    CloseOutcome::NotDurable => {
                        Err(super::pull::PullRejection::ConsumptionNotDurable)
                    }
                }
            }
        }
    }

    /// The live effective wanted PATHS for a node: the §6 wanted-union
    /// (the 086 membership-derived view), resolved to store paths
    /// against the node's declared outputs through THE single guard
    /// (`rio_common::wanted_outputs::verifiable_wanted_paths`,
    /// merged_bug_194 — the open-coded zip-filter copies are deleted).
    ///
    /// `None` means "no verifiable live wanted set exists": the node
    /// is missing from the DAG, no live build is interested, or every
    /// resolved path is empty (the floating-CA placeholder shape
    /// before the carrier union). The CONSUMER decides the
    /// conservative branch — re-arm, never a completion.
    async fn live_wanted_paths_for(
        &self,
        derivation_id: Uuid,
        drv_hash: &DrvHash,
    ) -> Result<Option<Vec<String>>, sqlx::Error> {
        let union = self.db.effective_wanted_union(derivation_id).await?;
        let Some(state) = self.dag.node(drv_hash) else {
            // Missing DAG node: no verifiable wanted set (the
            // conservative-absent arm — never a vacuous verdict).
            return Ok(None);
        };
        let wanted_names: Vec<String> = match union {
            // Zero live interest rows: nothing wants this node.
            None => return Ok(None),
            // '{}' saturation = all declared outputs (the
            // membership-derived default rows saturate here too).
            Some(v) => v,
        };
        Ok(rio_common::wanted_outputs::verifiable_wanted_paths(
            &state.output_names,
            &state.expected_output_paths,
            &wanted_names,
        )
        .map(|paths| paths.into_iter().map(str::to_string).collect()))
    }

    /// The node's [`rio_retry_kernel::MatCounters`] over the in-memory
    /// history (the loaded per-lane view) — THE single
    /// budget/one-shot/strictness counter (merged_bug_020). All three
    /// counts share the kernel's mat-lane reset window; party survives
    /// recovery (the suffix load parses `reporting_party` into every
    /// rebuilt record), so the worker-only Item-T recount is identical
    /// live and post-failover.
    fn mat_counters(&self, drv_hash: &DrvHash) -> rio_retry_kernel::MatCounters {
        self.dag
            .node(drv_hash)
            .map(|s| crate::retry_policy::materialization_counters(s.attempt_history()))
            .unwrap_or_default()
    }

    /// THE charge→verdict chokepoint (bug_067 + merged_bug_020): every
    /// `materialization_infra` charge — worker-reported AND
    /// establishment-written — closes through here, and the park
    /// decision runs UNCONDITIONALLY on the post-append kernel
    /// counters. Party-blind by the config contract
    /// (`max_attempts`: "worker-reported AND establishment-written —
    /// both channels charge the same budget"); the owner-signed Q5
    /// reversal (2026-06-03) of counter-signed residual (a)
    /// "establishment never parks" is exactly this fusion — a charging
    /// channel without the decision no longer exists. Park backs off
    /// the job durably (and the node is requeued either way: claimable
    /// again / from-source dispatchable per the admission table).
    // r[impl sched.materialize.routing+7]
    /// Settled-close companion #5 of 5 — it owns its OWN close (the
    /// charge row rides the close transaction), then runs the park
    /// verdict. Returns the ack witness plus whether the close
    /// SETTLED (false = fenced; the establishment caller keys its
    /// logging on this); a non-durable close propagates as the NACK.
    #[allow(clippy::too_many_arguments)]
    async fn charge_materialization_infra(
        &mut self,
        exec_id: Uuid,
        derivation_id: Uuid,
        drv_hash: &DrvHash,
        executor: &ExecutorId,
        job_id: Option<Uuid>,
        party: crate::state::ReportingParty,
        error_detail: Option<String>,
        source_node: Option<String>,
        termination_reason: Option<&'static str>,
        serving_generation: crate::db::ServingGeneration,
    ) -> Result<(MatAck, bool), super::pull::PullRejection> {
        let mut row = crate::db::attempts::AttemptRow::new(
            derivation_id,
            crate::state::OutcomeClass::MaterializationInfra,
            party,
            crate::state::AttemptKind::Materialization,
        );
        row.exec_id = Some(exec_id);
        row.executor_id = Some(executor.clone());
        row.error_msg = error_detail;
        row.source_node = source_node;
        row.termination_reason = termination_reason.map(Into::into);
        let close = match self
            .close_for_consumption(exec_id, drv_hash, Some(row), serving_generation)
            .await
        {
            CloseOutcome::Settled(close) => close,
            // Deposed believer: ack inert — the successor's own sweep
            // owns this attempt now (signed Q20).
            CloseOutcome::Deferred(ack) => return Ok((ack, false)),
            CloseOutcome::NotDurable => {
                return Err(super::pull::PullRejection::ConsumptionNotDurable);
            }
        };
        let SettledClose(()) = close;
        // The verdict, post-append: park at budget exhaustion, rearm
        // claimable under it. The job_id falls back to the view entry
        // (the establishment path has no report-context job id).
        let counters = self.mat_counters(drv_hash);
        let job_id = job_id.or_else(|| self.materialization_jobs.get(drv_hash).map(|e| e.job_id));
        if counters.infra_since_reset >= self.materialization_cfg.max_attempts {
            // Park ends with its own requeue companion — the node
            // returns from-source dispatchable per the admission table.
            self.park_materialization_job(
                drv_hash,
                job_id,
                counters.infra_since_reset,
                Some(executor),
                serving_generation,
            )
            .await;
        } else {
            // Under budget: the atomic claim release (re-arm + requeue
            // in ONE step — the node is claimable again immediately).
            self.release_claim(drv_hash, executor).await;
        }
        Ok((MatAck(()), true))
    }

    /// THE success-for-live-interest completion chokepoint
    /// (merged_bug_055): resolve the job, settle the view, stamp the
    /// carried realized paths, and complete the node — in ONE helper,
    /// so no consuming arm (Success coverage, the Unobtainable moot
    /// arm, or any future arm) can skip the carrier stamp. The stamp
    /// runs BEFORE `complete_ready_from_store_batch`: its
    /// non-destructive guard keeps a known path, so the floating-CA
    /// node re-completes with the realized path instead of the `[""]`
    /// placeholder (GC retention + the client-visible path restored).
    /// Settled-close companion #1 of 5. Consumes the linear
    /// [`SettledClose`] witness and mints the [`MatAck`]; a failed
    /// resolve follows the kernel companion law — release the claim
    /// uncharged instead of wedging it (merged_bug_055).
    #[allow(clippy::too_many_arguments)]
    async fn complete_materialization_for_live_interest(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        exec_id: Uuid,
        executor: &ExecutorId,
        carried_paths: &[String],
        serving_generation: crate::db::ServingGeneration,
        close: SettledClose,
        walk_verified: std::collections::HashMap<Vec<u8>, Vec<Uuid>>,
    ) -> MatAck {
        let SettledClose(()) = close;
        let d = match job_id {
            Some(job_id) => {
                self.resolve_materialization_job(
                    job_id,
                    Some(exec_id),
                    crate::state::JobState::ResolvedSuccess,
                    serving_generation,
                )
                .await
            }
            // No durable row: the job settled earlier (PG is the
            // authority) — removing the stale view entry is
            // reconciliation, not a decision.
            None => WriteDisposition::AlreadyResolved,
        };
        match companion_follow_up(d) {
            CompanionFollowUp::Settled => {
                if self.materialization_jobs.remove_settled(drv_hash, d) {
                    if !carried_paths.is_empty()
                        && let Some(state) = self.dag.node_mut(drv_hash)
                        && state.output_paths.is_empty()
                    {
                        state.output_paths = carried_paths.to_vec();
                    }
                    // Signed Q2: the walk's per-path verified-tenant
                    // sets came over the wire — ownership stamps
                    // INTERSECT them (a path is stamped only for
                    // tenants whose view validated it; absent entries
                    // stamp nothing). sh-002 row 4: pushed to the
                    // flush-scoped accumulator instead of calling
                    // `complete_ready_from_store_batch(len=1)` inline
                    // — `flush_pending_pull_outcomes` drains it into
                    // ONE batched call after every queued report's
                    // consumption ran. The carried-paths
                    // `output_paths` stamp ABOVE runs per-item before
                    // this push (Hazard Q — `dispatch.rs`'s
                    // `output_paths.is_empty()` back-fill must see
                    // the realized floating-CA path).
                    self.pending_walk_completed.push((
                        drv_hash.clone(),
                        crate::db::live_pins::StampProvenance::WalkVerified(walk_verified),
                    ));
                }
            }
            // Deposed believer: mutate nothing the successor owns.
            CompanionFollowUp::Inert => {}
            // The resolve write failed: claimable-but-unparked
            // dominates wedged-claimed-forever — the durable job row
            // is still pending and the next consumer re-decides.
            CompanionFollowUp::ReleaseClaimFallback => {
                warn!(drv_hash = %drv_hash, %exec_id,
                      "job resolve failed after a settled close; releasing the claim \
                       uncharged (companion law)");
                self.release_claim(drv_hash, executor).await;
            }
        }
        MatAck(())
    }

    /// Prefetched-routing twin of
    /// [`Self::consume_materialization_outcome`] for the UNCHARGED
    /// outcome arms (Success / RetryLater / Aborted; sh-007c S6 phase
    /// B). Takes the prefetched job + wanted-union and returns a
    /// [`BatchIntent`] with NO PG await; the close + resolve ride the
    /// batched fenced tx, the companion runs in phase D. Returns `Err`
    /// for arms the batched path does NOT cover (Unobtainable —
    /// durable-evidence + reprobe IO; InfraFailure — charge-row +
    /// park PG write); the caller routes those to the per-item
    /// `report_outcome_inner`. The kind/admission gates already
    /// passed in the caller's partition.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn consume_materialization_outcome_prefetched(
        &self,
        exec_id: Uuid,
        attempt: &crate::db::open_attempts::MatAttempt,
        outcome: &rio_proto::types::materialization_outcome::Outcome,
        job: Option<&(Uuid, JobOrigin, Option<Vec<String>>)>,
        wanted_union: Option<&Vec<String>>,
        // bug_134: the kind-uniform admission witness — minted by
        // `fold_report` in the caller's partition. Spent here on the
        // fast-path arms (one admission, one consumption pass);
        // RETURNED unspent on the slow-path arms (`Err(admission)`)
        // so the caller forwards it to `report_outcome_inner` — the
        // at-most-once spend holds either way (the witness is linear:
        // exactly one of the two lanes consumes it).
        admission: rio_evidence_kernel::pull::ProcessAdmission,
    ) -> Result<BatchIntent, rio_evidence_kernel::pull::ProcessAdmission> {
        use rio_proto::types::materialization_outcome::Outcome;
        let drv_hash = DrvHash::from(attempt.core.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.core.executor_id.as_str());
        let job_id = job.map(|(id, _, _)| *id);
        let carried_paths: Vec<String> = job
            .and_then(|(_, _, carried)| carried.clone())
            .unwrap_or_default();
        // 1. The live wanted set, resolved to paths via the same
        //    single guard the per-item path uses (merged_bug_194).
        let mut wanted_union: Vec<String> = match (wanted_union, self.dag.node(&drv_hash)) {
            (Some(names), Some(state)) => rio_common::wanted_outputs::verifiable_wanted_paths(
                &state.output_names,
                &state.expected_output_paths,
                names,
            )
            .map(|paths| paths.into_iter().map(str::to_string).collect())
            .unwrap_or_default(),
            // No DAG node OR no live interest row: no verifiable set
            // (the conservative-absent arm — never a vacuous verdict).
            _ => Vec::new(),
        };
        for p in &carried_paths {
            if !p.is_empty() && !wanted_union.contains(p) {
                wanted_union.push(p.clone());
            }
        }
        // The non-empty witness (merged_bug_194): zero-width → close
        // uncharged + deferred release (the same composition as the
        // per-item path). The width event is counted in phase D
        // POST-settle (bug_086).
        let Some(live_wanted_paths) = LiveWanted::new(wanted_union) else {
            return Ok(BatchIntent {
                exec_id,
                drv_hash,
                executor,
                companion: BatchedCompanion::Release {
                    defer: Some(std::time::Duration::from_secs(
                        RETRY_LATER_DEFAULT_DEFER_SECS,
                    )),
                    zero_width_exec: Some(exec_id),
                },
            });
        };
        match outcome {
            Outcome::Success(s) => {
                let walk_verified: std::collections::HashMap<Vec<u8>, Vec<Uuid>> = {
                    use sha2::Digest;
                    s.verified_tenants
                        .iter()
                        .map(|pt| {
                            (
                                sha2::Sha256::digest(pt.store_path.as_bytes()).to_vec(),
                                pt.verified_tenant_ids
                                    .iter()
                                    .filter_map(|t| t.parse::<Uuid>().ok())
                                    .collect::<Vec<Uuid>>(),
                            )
                        })
                        .collect()
                };
                let companion = if success_covers_live_wanted(
                    &s.ingested_paths,
                    &s.verified_paths,
                    &live_wanted_paths,
                ) {
                    BatchedCompanion::Complete {
                        job_id,
                        carried_paths,
                        walk_verified,
                    }
                } else {
                    BatchedCompanion::Release {
                        defer: None,
                        zero_width_exec: None,
                    }
                };
                Ok(BatchIntent {
                    exec_id,
                    drv_hash,
                    executor,
                    companion,
                })
            }
            Outcome::RetryLater(r) => {
                let retry_after = std::time::Duration::from_secs(
                    r.retry_after_secs
                        .clamp(0, RETRY_LATER_MAX_DEFER_SECS)
                        .max(RETRY_LATER_DEFAULT_DEFER_SECS),
                );
                tracing::info!(
                    %exec_id, drv_hash = %drv_hash, class = %r.class, detail = %r.detail,
                    defer_secs = retry_after.as_secs(),
                    "transient materialization failure; closing uncharged and deferring"
                );
                Ok(BatchIntent {
                    exec_id,
                    drv_hash,
                    executor,
                    companion: BatchedCompanion::Release {
                        defer: Some(retry_after),
                        zero_width_exec: None,
                    },
                })
            }
            Outcome::Aborted(a) => {
                tracing::info!(
                    %exec_id, drv_hash = %drv_hash, detail = %a.detail,
                    "materialization walk aborted by the worker; closing charge-free"
                );
                Ok(BatchIntent {
                    exec_id,
                    drv_hash,
                    executor,
                    companion: BatchedCompanion::Release {
                        defer: None,
                        zero_width_exec: None,
                    },
                })
            }
            // Unobtainable (durable-evidence read + reprobe IO) and
            // InfraFailure (charge row + park-on-budget PG write) are
            // the per-item slow path: the admission witness returns
            // UNSPENT (bug_134's at-most-once law — exactly one lane
            // consumes it). The zero-width short-circuit above already
            // covered the no-verifiable-set case for these variants.
            Outcome::Unobtainable(_) | Outcome::InfraFailure(_) => Err(admission),
        }
    }

    /// Phase-D apply for one batched intent (sh-007c S6): synthesize
    /// the close `WriteDisposition` from the batch tx outcome, mint
    /// the witness via [`Self::close_for_consumption_from_disposition`],
    /// then run the companion. The resolve `WriteDisposition` is
    /// likewise synthesized from `resolved_set` (the batch tx already
    /// committed it). PG-free for the `Release` arm (sh-027 §3: it
    /// returns a [`DeferredRelease`] the caller batches into ONE
    /// `requeue_after_attempt(slice)`); the `Complete` arm's
    /// `ReleaseClaimFallback` rare-path is the only residual await.
    pub(super) async fn apply_batched_companion(
        &mut self,
        intent: BatchIntent,
        close_d: WriteDisposition,
        resolved_set: &std::collections::HashSet<Uuid>,
    ) -> Result<CompanionResult, super::pull::PullRejection> {
        let BatchIntent {
            exec_id,
            drv_hash,
            executor,
            companion,
        } = intent;
        let close = match Self::close_for_consumption_from_disposition(close_d) {
            CloseOutcome::Settled(close) => close,
            CloseOutcome::Deferred(ack) => return Ok(CompanionResult::Ack(ack)),
            CloseOutcome::NotDurable => {
                return Err(super::pull::PullRejection::ConsumptionNotDurable);
            }
        };
        match companion {
            BatchedCompanion::Release {
                defer,
                zero_width_exec,
            } => {
                if let Some(exec_id) = zero_width_exec {
                    crate::state::note_width_event(crate::state::WidthEvent::NoVerifiableSet {
                        exec_id,
                        settled: &close,
                    });
                    warn!(
                        %exec_id, drv_hash = %drv_hash,
                        "no verifiable live-wanted path set; closed uncharged and deferred"
                    );
                }
                // sh-027 §3: defer the release to the post-loop
                // batch — the per-item `companion_release` await here
                // was the residual N×PG chain in the otherwise-O(1)
                // phased flush. The settled-close witness is consumed
                // HERE (the gate); the DeferredRelease is constructed
                // only in this Settled arm, so the witness law holds.
                let SettledClose(()) = close;
                Ok(CompanionResult::DeferredRelease(DeferredRelease {
                    drv_hash,
                    executor,
                    defer,
                }))
            }
            BatchedCompanion::Complete {
                job_id,
                carried_paths,
                walk_verified,
            } => {
                let SettledClose(()) = close;
                // The resolve already rode the batch tx — synthesize
                // its disposition from the batch outcome (Fenced /
                // Failed cannot reach here: those map to Deferred /
                // NotDurable above; close_d.settled() ⇒ committed).
                let d = match job_id {
                    Some(job_id) if resolved_set.contains(&job_id) => {
                        metrics::counter!(
                            "rio_scheduler_materialization_jobs_resolved_total",
                            "outcome" => "success"
                        )
                        .increment(1);
                        WriteDisposition::Applied
                    }
                    Some(_) | None => WriteDisposition::AlreadyResolved,
                };
                match companion_follow_up(d) {
                    CompanionFollowUp::Settled => {
                        if self.materialization_jobs.remove_settled(&drv_hash, d) {
                            if !carried_paths.is_empty()
                                && let Some(state) = self.dag.node_mut(&drv_hash)
                                && state.output_paths.is_empty()
                            {
                                state.output_paths = carried_paths;
                            }
                            self.pending_walk_completed.push((
                                drv_hash,
                                crate::db::live_pins::StampProvenance::WalkVerified(walk_verified),
                            ));
                        }
                    }
                    CompanionFollowUp::Inert => {}
                    CompanionFollowUp::ReleaseClaimFallback => {
                        warn!(drv_hash = %drv_hash, %exec_id,
                              "job resolve failed after a settled close; releasing the claim \
                               uncharged (companion law)");
                        self.release_claim(&drv_hash, &executor).await;
                    }
                }
                Ok(CompanionResult::Ack(MatAck(())))
            }
        }
    }

    /// Phase-D batched release (sh-027 §3): the per-item in-mem
    /// `release_claim_deferring` (the bug_220 compare-and-clear; cheap,
    /// no PG) for every collected [`DeferredRelease`], then ONE
    /// [`Self::requeue_after_attempt`] over the non-stale slice. The
    /// `lost_worker` hint is `None` for the batch — the Materialization
    /// arm of `requeue_after_attempt` does not consult it (its only
    /// reader is the Build arm's poison-threshold log line); per-item
    /// executor identity is fully consumed by the compare-and-clear
    /// here. Semantically identical to N × [`Self::companion_release`]
    /// (every release the per-item path would issue is issued; the
    /// `StaleHolder` arm is skipped exactly the same way), with one
    /// requeue chokepoint instead of N.
    pub(super) async fn companion_release_batch(&mut self, releases: Vec<DeferredRelease>) {
        if releases.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let mut requeue: Vec<DrvHash> = Vec::with_capacity(releases.len());
        for DeferredRelease {
            drv_hash,
            executor,
            defer,
        } in releases
        {
            let defer_until = defer.map(|d| now + d);
            let release = match self.materialization_jobs.get_mut(&drv_hash) {
                Some(entry) => entry.release_claim_deferring(&executor, defer_until),
                None => ClaimRelease::Unclaimed,
            };
            if release == ClaimRelease::StaleHolder {
                warn!(
                    drv_hash = %drv_hash, executor = %executor,
                    "stale claim release ignored: a different executor holds a fresh claim"
                );
                continue;
            }
            requeue.push(drv_hash);
        }
        self.requeue_after_attempt(&requeue, crate::state::AttemptKind::Materialization, None)
            .await;
    }

    /// THE consumption-close chokepoint (bug_182): every report arm
    /// closes through here and receives the settled-close witness, the
    /// fenced ack, or the NACK marker — per the kernel ack law
    /// (`consumption_ack`). This and
    /// [`Self::close_for_consumption_from_disposition`] are the only
    /// two sites that construct [`SettledClose`]; both feed the same
    /// `consumption_ack` over a `WriteDisposition`, so the witness law
    /// is identical batched or per-item.
    async fn close_for_consumption(
        &mut self,
        exec_id: Uuid,
        drv_hash: &DrvHash,
        charge_row: Option<crate::db::attempts::AttemptRow>,
        serving_generation: crate::db::ServingGeneration,
    ) -> CloseOutcome {
        let d = self
            .close_materialization_attempt(exec_id, drv_hash, charge_row, serving_generation)
            .await;
        match consumption_ack(d) {
            ConsumptionAck::Ack if d.settled() => CloseOutcome::Settled(SettledClose(())),
            // Fenced: ack inert (signed Q20 — deposed believers ack;
            // the successor's establishment owns the row).
            ConsumptionAck::Ack => CloseOutcome::Deferred(MatAck(())),
            ConsumptionAck::NackRetryable => CloseOutcome::NotDurable,
        }
    }

    /// Batched twin of [`Self::close_for_consumption`] (sh-007c S6
    /// second sanctioned [`SettledClose`] mint): the
    /// `WriteDisposition` was synthesized by the caller from ONE
    /// `close_and_resolve_materialization_batch_fenced` transaction
    /// (Applied iff committed AND `exec_id ∈ closed_set`;
    /// `AlreadyResolved` iff committed AND not-in-set; `Fenced` /
    /// `Failed` from the batch outcome). Same kernel `consumption_ack`
    /// law — the witness/ack/NACK semantics are IDENTICAL to the
    /// per-item path; only the `begin_fenced` count differs (1 per
    /// flush, not 1 per item).
    pub(super) fn close_for_consumption_from_disposition(d: WriteDisposition) -> CloseOutcome {
        match consumption_ack(d) {
            ConsumptionAck::Ack if d.settled() => CloseOutcome::Settled(SettledClose(())),
            ConsumptionAck::Ack => CloseOutcome::Deferred(MatAck(())),
            ConsumptionAck::NackRetryable => CloseOutcome::NotDurable,
        }
    }

    /// Settled-close companion #2 of 5: defer (optionally) and release
    /// the claim atomically — the ReArm / RetryLater / Aborted /
    /// zero-width / coverage-miss composition. bug_220 + bug_095 (the
    /// stamp law, righted): `StaleHolder` — a DIFFERENT live executor
    /// holds a fresh claim — is the ONLY no-stamp/no-requeue arm;
    /// `Released` AND `Unclaimed` (former holder / no view entry)
    /// stamp and requeue BY DESIGN (idempotent redelivery after the
    /// holder's own release + the level-triggered wedge repair,
    /// merged_bug_015/307 — see `release_claim_deferring`'s arm
    /// table). The reason a former holder's REDELIVERED classified
    /// report cannot reach this code at all is the report-intake
    /// shield, NOT this disposition: `fold_report`
    /// (rio-evidence-kernel/src/pull.rs) AckIgnores any report for an
    /// inactive or already-classified attempt — pinned end-to-end by
    /// `redelivered_retry_later_after_classification_is_inert`.
    pub(super) async fn companion_release(
        &mut self,
        drv_hash: &DrvHash,
        executor: &ExecutorId,
        defer: Option<std::time::Duration>,
        close: SettledClose,
    ) -> MatAck {
        #[cfg(test)]
        self.test_counters
            .companion_release_awaits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let SettledClose(()) = close;
        self.release_claim_with_defer(drv_hash, executor, defer)
            .await;
        MatAck(())
    }

    /// Settled-close companion #3 of 5: resolve the job from source
    /// and requeue; a failed resolve releases the claim (companion
    /// law) instead of leaving it wedged.
    async fn companion_resolve_from_source(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        exec_id: Uuid,
        executor: &ExecutorId,
        serving_generation: crate::db::ServingGeneration,
        close: SettledClose,
    ) -> MatAck {
        let SettledClose(()) = close;
        let d = match job_id {
            Some(job_id) => {
                self.resolve_materialization_job(
                    job_id,
                    Some(exec_id),
                    crate::state::JobState::ResolvedFromSource,
                    serving_generation,
                )
                .await
            }
            None => WriteDisposition::AlreadyResolved,
        };
        match companion_follow_up(d) {
            CompanionFollowUp::Settled => {
                if self.materialization_jobs.remove_settled(drv_hash, d) {
                    // The node returns to its dep-derived status
                    // (the normal Ready path) — requeue it.
                    self.requeue_after_attempt(
                        std::slice::from_ref(drv_hash),
                        crate::state::AttemptKind::Materialization,
                        Some(executor),
                    )
                    .await;
                }
            }
            CompanionFollowUp::Inert => {}
            CompanionFollowUp::ReleaseClaimFallback => {
                warn!(drv_hash = %drv_hash, %exec_id,
                      "from-source resolve failed after a settled close; releasing the \
                       claim uncharged (companion law)");
                self.release_claim(drv_hash, executor).await;
            }
        }
        MatAck(())
    }

    /// Settled-close companion #4 of 5: resolve the job unobtainable
    /// and fail-fast the pruned root; a failed resolve releases the
    /// claim (companion law).
    async fn companion_fail_fast(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        exec_id: Uuid,
        executor: &ExecutorId,
        serving_generation: crate::db::ServingGeneration,
        close: SettledClose,
    ) -> MatAck {
        let SettledClose(()) = close;
        let d = match job_id {
            Some(job_id) => {
                self.resolve_materialization_job(
                    job_id,
                    Some(exec_id),
                    crate::state::JobState::ResolvedUnobtainable,
                    serving_generation,
                )
                .await
            }
            None => WriteDisposition::AlreadyResolved,
        };
        match companion_follow_up(d) {
            CompanionFollowUp::Settled => {
                if self.materialization_jobs.remove_settled(drv_hash, d) {
                    self.fail_fast_pruned_root(
                        drv_hash,
                        "materialization confirmed a live-wanted output missing upstream \
                         and not substitutable",
                    )
                    .await;
                }
            }
            CompanionFollowUp::Inert => {}
            CompanionFollowUp::ReleaseClaimFallback => {
                warn!(drv_hash = %drv_hash, %exec_id,
                      "unobtainable resolve failed after a settled close; releasing the \
                       claim uncharged (companion law)");
                self.release_claim(drv_hash, executor).await;
            }
        }
        MatAck(())
    }

    /// Close the open materialization attempt (assignment row) and
    /// append the charge row when one is given, in ONE transaction
    /// carrying the same claims-floor fence as every other attempt
    /// closer. Mirrors `close_pull_attempt_uncharged`'s shape WITH an
    /// optional charge. Idempotent: a row already present for the exec
    /// makes the append a no-op (terminal-row-wins).
    async fn close_materialization_attempt(
        &mut self,
        exec_id: Uuid,
        drv_hash: &DrvHash,
        charge_row: Option<crate::db::attempts::AttemptRow>,
        serving_generation: crate::db::ServingGeneration,
    ) -> WriteDisposition {
        #[cfg(test)]
        self.test_counters
            .begin_fenced_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result: Result<Option<(u64, bool)>, sqlx::Error> = async {
            let mut tx = match self.db.begin_fenced(serving_generation).await? {
                crate::db::FencedBegin::Fenced { .. } => return Ok(None),
                crate::db::FencedBegin::Open(ftx) => ftx,
            };
            let mut inserted = false;
            if let Some(row) = &charge_row {
                inserted = crate::db::SchedulerDb::append_attempt(tx.conn(), row).await?;
            }
            let closed = tx
                .close_assignment(exec_id, crate::db::AssignmentCloseStatus::Completed)
                .await?;
            tx.commit().await?;
            Ok(Some((closed, inserted)))
        }
        .await;
        match result {
            Ok(Some((closed, inserted))) => {
                if inserted && let Some(row) = charge_row {
                    if let Some(state) = self.dag.node_mut(drv_hash) {
                        state.push_attempt_record(row.to_record());
                    }
                    self.refresh_retry_view(drv_hash);
                }
                if closed > 0 {
                    WriteDisposition::Applied
                } else {
                    // Already closed (idempotent re-report or a prior
                    // settlement): the durable state is settled either
                    // way — terminal-row-wins.
                    WriteDisposition::AlreadyResolved
                }
            }
            Ok(None) => {
                self.note_fenced_evidence_write("materialization attempt close");
                WriteDisposition::Fenced
            }
            Err(e) => {
                warn!(drv_hash = %drv_hash, %exec_id, error = %e,
                      "materialization attempt close failed; the establishment sweep remains \
                       the backstop");
                WriteDisposition::Failed
            }
        }
    }

    /// Resolve the job terminally (fenced, exec_id-keyed, at-most-once)
    /// and note a fence refusal. Returns whether the resolution was
    /// APPLIED (rows > 0) — the at-most-once edge callers may hang
    /// further accounting on (the PD-20 conversion counter).
    // r[impl obs.metric.scheduler]
    async fn resolve_materialization_job(
        &mut self,
        job_id: Uuid,
        exec_id: Option<Uuid>,
        to_state: crate::state::JobState,
        serving_generation: crate::db::ServingGeneration,
    ) -> WriteDisposition {
        match self
            .db
            .resolve_materialization_job_fenced(job_id, exec_id, to_state, serving_generation)
            .await
        {
            Ok(crate::db::FencedOutcome::Applied(rows)) => {
                // T-6.2 lifecycle counter: one increment per APPLIED
                // terminal resolution, labeled by outcome. rows == 0 is
                // the at-most-once no-op (already resolved) and never
                // double-counts.
                if rows > 0 {
                    metrics::counter!(
                        "rio_scheduler_materialization_jobs_resolved_total",
                        "outcome" => Self::resolution_outcome_label(to_state)
                    )
                    .increment(1);
                }
                // r[impl sched.materialize.pinning]
                // §5.3 release site (i): the resolution may have made
                // this job's pins releasable (resolved AND no live
                // interest — the single-build case where the interest
                // already went terminal before the report landed). The
                // release query self-scopes; calling it after every
                // resolution is a no-op when interest is still live.
                self.release_materialization_pins_best_effort("job resolution")
                    .await;
                if rows > 0 {
                    WriteDisposition::Applied
                } else {
                    WriteDisposition::AlreadyResolved
                }
            }
            Ok(crate::db::FencedOutcome::AlreadyResolved) => {
                // Settled by an earlier resolution: not the at-most-once
                // edge (no counter), but the pin-release re-check is the
                // same self-scoping no-op-if-live call.
                self.release_materialization_pins_best_effort("job resolution")
                    .await;
                WriteDisposition::AlreadyResolved
            }
            Ok(crate::db::FencedOutcome::Fenced) => {
                self.note_fenced_evidence_write("materialization job resolve");
                WriteDisposition::Fenced
            }
            Err(e) => {
                warn!(%job_id, error = %e, "materialization job resolve failed");
                WriteDisposition::Failed
            }
        }
    }

    /// The `outcome` label vocabulary for
    /// `rio_scheduler_materialization_jobs_resolved_total` — the
    /// JobState terminal alphabet minus its `resolved_`/state prefixes
    /// (Pending is unreachable here; resolve targets are terminal by
    /// the caller's debug_assert).
    fn resolution_outcome_label(state: crate::state::JobState) -> &'static str {
        use crate::state::JobState;
        match state {
            JobState::ResolvedSuccess => "success",
            JobState::ResolvedFromSource => "from_source",
            JobState::ResolvedUnobtainable => "unobtainable",
            JobState::Cancelled => "cancelled",
            JobState::Obsolete => "obsolete",
            JobState::Pending => "pending",
        }
    }

    // r[impl sched.materialize.pinning]
    /// The §5.3 pin-release call shared by the three wiring sites
    /// (consumption resolution, build-terminal transition, recovery
    /// sweep arm): delete materialization pins whose job is resolved
    /// AND whose derivation has no live interested build left. The
    /// query self-scopes (pins of unresolved jobs and pins with live
    /// interest always survive — the B2-strong holds-window).
    ///
    /// ALWAYS-ON, never flag-gated (PD-B17): flag-gating the release
    /// would make flag-on-era pins permanently GC-immune after an
    /// ON→OFF rollback — a store-state divergence no equivalence
    /// criterion would catch. Flag-off, no materialization pins exist
    /// for fresh work and the call is a cheap no-op over flag-on-era
    /// leftovers (which is exactly the rollback drain §5.3 wants).
    /// Event-driven (not per-tick), so this does not reproduce the
    /// dormancy-7 flag-off-PG-query concern.
    ///
    /// Best-effort: a failed release is retried by the next event-driven
    /// site or the recovery sweep arm (the orphan backstop).
    pub(super) async fn release_materialization_pins_best_effort(&mut self, trigger: &str) {
        match self
            .db
            .release_materialization_pins_for_resolved_jobs()
            .await
        {
            Ok(0) => {}
            Ok(released) => {
                tracing::info!(
                    released,
                    trigger,
                    "materialization pins released (job resolved + all interest terminal)"
                );
            }
            Err(e) => {
                warn!(error = %e, trigger,
                      "materialization pin release failed (best-effort; the recovery \
                       sweep arm is the backstop)");
            }
        }
    }

    /// THE claim-drop chokepoint (merged_bug_015 / merged_bug_307 legs
    /// a+b): re-arm the job — the in-memory view drops the claim so
    /// the next pull's one-winner arbitration sees Pending — AND
    /// requeue the node off the mint's Assigned/Running bookkeeping in
    /// the SAME call, the code twin of the model's single-step
    /// `consumeUnobtainable`. A claim drop without the requeue is the
    /// documented wedge: pending-unclaimed job + node held Running ⇒
    /// the admission table answers NotYetReady to EVERY identity and
    /// no armed action remains (the skew the housekeeping tripwire
    /// counts). B1's Aborted intake routes through here too.
    /// bug_170/134 rider: the release is a COMPARE-AND-CLEAR on the
    /// holder — every caller names the executor it acts for, so a
    /// stale release (a late companion or the two-strike ghost repair
    /// racing a fresh mint across actor turns) clears nothing and,
    /// crucially, does NOT requeue a node that now belongs to the new
    /// attempt's Assigned/Running bookkeeping.
    // r[impl sched.materialize.claim-coherence]
    pub(super) async fn release_claim(&mut self, drv_hash: &DrvHash, executor: &ExecutorId) {
        self.release_claim_with_defer(drv_hash, executor, None)
            .await;
    }

    /// [`Self::release_claim`] with the optional deferral folded into
    /// the SAME entry-level disposition-gated mutation (bug_220 +
    /// bug_095): the companion's RetryLater window stamps on every
    /// disposition EXCEPT `StaleHolder` — `Unclaimed` (former holder /
    /// no entry) deliberately stamps and falls through to the
    /// level-triggered requeue (the merged_bug_015/307 wedge-repair
    /// law); only a DIFFERENT live holder blocks the stamp.
    /// Redeliveries of already-classified reports never reach here:
    /// `fold_report`'s intake gate (rio-evidence-kernel) is the
    /// load-bearing shield.
    async fn release_claim_with_defer(
        &mut self,
        drv_hash: &DrvHash,
        executor: &ExecutorId,
        defer: Option<std::time::Duration>,
    ) {
        let defer_until = defer.map(|d| std::time::Instant::now() + d);
        let release = match self.materialization_jobs.get_mut(drv_hash) {
            Some(entry) => entry.release_claim_deferring(executor, defer_until),
            // No view entry (resolved/wiped/unhydrated): nothing to
            // clear; the requeue below stays level-triggered — the
            // node bookkeeping is keyed on the durable attempt, not
            // the view.
            None => ClaimRelease::Unclaimed,
        };
        if release == ClaimRelease::StaleHolder {
            warn!(
                drv_hash = %drv_hash, executor = %executor,
                "stale claim release ignored: a different executor holds a fresh claim"
            );
            return;
        }
        self.requeue_after_attempt(
            std::slice::from_ref(drv_hash),
            crate::state::AttemptKind::Materialization,
            Some(executor),
        )
        .await;
    }

    /// Park the job (infra-budget exhaustion, design §2.5): durable
    /// `park_until` + the in-memory view. Never a fail-fast.
    async fn park_materialization_job(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Option<Uuid>,
        infra_count: u32,
        executor: Option<&ExecutorId>,
        serving_generation: crate::db::ServingGeneration,
    ) {
        let base = self.materialization_cfg.park_backoff_base_secs;
        let cap = self.materialization_cfg.park_backoff_cap_secs;
        let exp = infra_count.saturating_sub(self.materialization_cfg.max_attempts);
        let backoff_secs = base.saturating_mul(2u64.saturating_pow(exp)).min(cap);
        // The view mutation gates on the durable park's disposition
        // (sched.materialize.view-settlement): a deposed believer's
        // refused park must not project a parked view over a durable
        // row the successor owns. A view-only entry (no durable job
        // row) has nothing to settle against — it parks in memory and
        // the next consumption/cancel pass reconciles it.
        let durable = if let Some(job_id) = job_id {
            let park_until_epoch = crate::db::attempts::epoch_now() + backoff_secs as f64;
            match self
                .db
                .park_materialization_job_fenced(job_id, park_until_epoch, serving_generation)
                .await
            {
                Ok(
                    crate::db::FencedOutcome::Applied(_)
                    | crate::db::FencedOutcome::AlreadyResolved,
                ) => WriteDisposition::Applied,
                Ok(crate::db::FencedOutcome::Fenced) => {
                    self.note_fenced_evidence_write("materialization job park");
                    WriteDisposition::Fenced
                }
                Err(e) => {
                    warn!(%job_id, error = %e, "materialization job park failed");
                    WriteDisposition::Failed
                }
            }
        } else {
            WriteDisposition::Applied
        };
        match companion_follow_up(durable) {
            CompanionFollowUp::Settled => {
                if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
                    // Compare-and-clear when the consuming executor is
                    // named (bug_170 rider): a stale park companion
                    // must not strip a fresh foreign claim. The
                    // unnamed fallback (no caller today) keeps the
                    // park's original unconditional clear.
                    match executor {
                        Some(e) => {
                            let _ = entry.release_claim_if_held(e);
                        }
                        None => entry.clear_claim_unconditional(),
                    }
                    entry.park(
                        std::time::Instant::now() + std::time::Duration::from_secs(backoff_secs),
                    );
                }
            }
            // Deposed believer: project nothing over rows the
            // successor owns.
            CompanionFollowUp::Inert => {}
            // The park write failed (merged_bug_055): clear the holder
            // anyway — claimable-but-unparked dominates
            // wedged-claimed-forever (the attempt is already closed;
            // a kept holder is exactly the claimed-no-attempt ghost).
            CompanionFollowUp::ReleaseClaimFallback => {
                if let Some(entry) = self.materialization_jobs.get_mut(drv_hash) {
                    match executor {
                        Some(e) => {
                            let _ = entry.release_claim_if_held(e);
                        }
                        None => entry.clear_claim_unconditional(),
                    }
                }
            }
        }
        // The park's requeue companion (merged_bug_015's park half):
        // the node leaves the mint's Assigned/Running bookkeeping
        // either way — parked means "not claimable until the backoff
        // lapses", never "wedged Running with no armed action".
        self.requeue_after_attempt(
            std::slice::from_ref(drv_hash),
            crate::state::AttemptKind::Materialization,
            executor,
        )
        .await;
    }

    /// The arm-3 FMP re-probe over the live wanted paths — once per
    /// LIVE TENANT, folded through the kernel's per-(tenant, path)
    /// quantifier (merged_bug_028 / owner Q2, re-granulated by
    /// bug_299): `ConfirmedMissing` — the verdict that can fail-fast
    /// a pruned root — requires SOME path missing under EVERY
    /// interested tenant (∃ path ∀ tenant). This caller does ONLY
    /// mechanical membership mapping into raw cells; the quantifier
    /// decision lives in the kernel, where complementary coverage
    /// (tenant A has X, tenant B has Y) folds Obtainable. `None` =
    /// the probe could not answer (no store client / ANY tenant's RPC
    /// failure / timeout) — the caller maps that to ReArm (B3: an
    /// indeterminate answer never fail-fasts).
    ///
    /// Without a service signer the store cannot run its upstream
    /// substitution check (no `x-rio-probe-tenant-id`), so a missing
    /// path is indeterminate, never confirmed-missing — the probe then
    /// cannot produce the fail-fast conjunct (B3's conservative
    /// direction; the kernel's can_confirm row). A node with NO live
    /// tenant folds over the empty set → `Obtainable` (the kernel's
    /// empty-never-confirms row).
    // r[impl sched.materialize.reprobe-per-path]
    async fn reprobe_live_wanted_paths(
        &mut self,
        drv_hash: &DrvHash,
        live_wanted: &[String],
    ) -> Option<ReprobeAnswer> {
        use rio_evidence_kernel::outcome::{PathProbeCell, TenantPathAnswers};
        if live_wanted.is_empty() {
            return Some(ReprobeAnswer::Obtainable);
        }
        let store = self.store_client.clone()?;
        let tenants = self.live_tenants_of(drv_hash);
        let mut rows: Vec<(Vec<PathProbeCell>, bool)> = Vec::with_capacity(tenants.len());
        // One AttemptBudget prices the whole reprobe (bug_127, the
        // same law as the dispatch sweep): T tenants share a single
        // grpc_timeout instead of paying it each; an expired budget
        // poisons the answer exactly like a per-tenant failure (B3: a
        // partial view must not confirm). NOT capped at
        // `DISPATCH_PROBE_SWEEP_BUDGET`: this is per-RPC settlement
        // (not the per-tick hot path the cap defends), and a 5.5 s
        // shared serial budget over 3 tenants × 2.5 s each makes
        // tenant 3 hit `budget.expired()` → `return None` →
        // `companion_release` bare re-arm — converts a one-cycle
        // ConfirmedMissing/Obtainable settlement into a ~3 h stall
        // ending in the age-out arm's unconditional ResolvedFromSource
        // (wrong terminal outcome for a pruned-root that should
        // fail-fast).
        // r[impl sched.dispatch.probe-budget]
        let budget = rio_common::transport::AttemptBudget::new(self.grpc_timeout);
        for tenant in tenants {
            if budget.expired() {
                return None;
            }
            let probe = self.probe_service_meta_for(Some(tenant));
            let mut req = tonic::Request::new(rio_proto::types::FindMissingPathsRequest {
                store_paths: live_wanted.to_vec(),
            });
            for (k, v) in probe {
                if let Ok(mv) = tonic::metadata::MetadataValue::try_from(v.as_str()) {
                    req.metadata_mut().insert(k, mv);
                }
            }
            // ANY tenant's RPC failure poisons the whole answer (B3:
            // a partial view must not confirm). A store that REJECTS
            // the probe (UNAUTHENTICATED under the Q3 law — rotated
            // service HMAC) lands here too: conservative ReArm.
            let resp = match tokio::time::timeout(
                budget.attempt_bound(self.grpc_timeout),
                store.clone().find_missing_paths(req),
            )
            .await
            {
                Ok(Ok(resp)) => resp.into_inner(),
                Ok(Err(e)) => {
                    // merged_bug_179: an issued FMP failure is
                    // store-health evidence on this surface too — the
                    // arm-3 settlement reprobe carries the same
                    // probe-budget marker as the dispatch fold and
                    // "poisons the answer exactly like a per-tenant
                    // failure".
                    self.note_issued_store_rpc_failure("settlement-reprobe");
                    tracing::debug!(error = %e, "settlement reprobe: FindMissingPaths failed");
                    return None;
                }
                Err(_elapsed) => {
                    self.note_issued_store_rpc_failure("settlement-reprobe");
                    tracing::debug!("settlement reprobe: FindMissingPaths timed out");
                    return None;
                }
            };
            // merged_bug_003 (Q3): confirmed-missing authority derives
            // from the store's ECHO — the probe actually ran tenant-
            // scoped — never from `!probe.is_empty()` (the scheduler's
            // belief about its own request). A pre-Q3 store that
            // silently downgraded to anonymous answered missing with
            // empty substitutable/indeterminate, wire-identical to
            // confirmed 404s; the echo (absent = false on old stores)
            // makes that answer non-confirming: conservative, never
            // fail-fast.
            let can_confirm = resp.probe_ran_tenant_scoped;
            let missing: std::collections::HashSet<String> =
                resp.missing_paths.into_iter().collect();
            let substitutable: std::collections::HashSet<String> =
                resp.substitutable_paths.into_iter().collect();
            let indeterminate: std::collections::HashSet<String> =
                resp.indeterminate_paths.into_iter().collect();
            // Mechanical membership mapping — nothing else is
            // decidable here.
            let cells: Vec<PathProbeCell> = live_wanted
                .iter()
                .map(|p| {
                    if !missing.contains(p) {
                        PathProbeCell::Present
                    } else if substitutable.contains(p) {
                        PathProbeCell::Substitutable
                    } else if indeterminate.contains(p) {
                        PathProbeCell::Indeterminate
                    } else {
                        PathProbeCell::Missing
                    }
                })
                .collect();
            rows.push((cells, can_confirm));
        }
        let answers: Vec<TenantPathAnswers<'_>> = rows
            .iter()
            .map(|(cells, can_confirm)| TenantPathAnswers {
                cells,
                can_confirm: *can_confirm,
            })
            .collect();
        Some(rio_evidence_kernel::outcome::fold_path_reprobes(&answers))
    }
}

impl DagActor {
    /// Establish one expired open materialization attempt (the
    /// dead-store-replica case): the `materialization_infra` charge
    /// (kind=materialization, party=Scheduler, "unreported") routes
    /// through [`Self::charge_materialization_infra`] — close, charge,
    /// and the PARK DECISION in one chokepoint. Establishment-only
    /// crash-loops therefore park at `max_attempts` like every other
    /// charge channel (the owner-signed Q5 reversal of counter-signed
    /// residual (a), 2026-06-03: party-blind parking; the parked
    /// population is MD-D1's stalled gauge). NO adopt arm (BC-3: a
    /// mid-walk crash leaves outputs present but the closure
    /// incomplete) and never `executor_crash` (BC-2: the charge feeds
    /// the materialization budget and nothing else).
    // r[impl sched.materialize.routing+7]
    pub(super) async fn establish_materialization_attempt(
        &mut self,
        attempt: &crate::db::open_attempts::OpenAttemptRow,
    ) {
        let drv_hash = DrvHash::from(attempt.drv_hash.as_str());
        let executor = ExecutorId::from(attempt.executor_id.as_str());
        let serving_generation = self.serving_generation();
        // A deposed close runs no verdict and no requeue — the
        // successor's own sweep owns this attempt now; a non-durable
        // close re-runs next tick (the sweep is idempotent; there is
        // no store to NACK on this path).
        match self
            .charge_materialization_infra(
                attempt.exec_id,
                attempt.derivation_id,
                &drv_hash,
                &executor,
                None,
                crate::state::ReportingParty::Scheduler,
                None,
                attempt.source_node.clone(),
                Some("unreported"),
                serving_generation,
            )
            .await
        {
            Ok((_ack, true)) => {}
            Ok((_ack, false)) => return,
            Err(_not_durable) => return,
        }
        tracing::info!(
            drv_hash = %drv_hash,
            exec_id = %attempt.exec_id,
            executor_id = %executor,
            age_secs = attempt.age_secs,
            "establishment sweep: open materialization attempt established as \
             materialization_infra (no adopt arm; park decision applied — the \
             job re-armed claimable or parked at budget exhaustion)"
        );
    }

    /// The phase-15 settlement core shared by both
    /// [`Self::tick_reevaluate_materialization_jobs`] arms (sh-044 r1):
    /// `resolve_materialization_job(.., ResolvedFromSource, ..)` →
    /// `remove_settled` gate. Returns `Some(disposition)` iff the view
    /// entry was evicted (the at-most-once edge each arm's counter
    /// keys on); `None` on a Fenced or Failed resolve — the job stays
    /// in the view and is re-evaluated next tick. Each arm keeps only
    /// its distinct counter and log fields; the requeue is batched
    /// once over both arms' settled set after the loop, and the
    /// stalled gauge is a post-loop view recount (no manual ±1).
    async fn settle_resolved_from_source(
        &mut self,
        drv_hash: &DrvHash,
        job_id: Uuid,
    ) -> Option<WriteDisposition> {
        let serving_generation = self.serving_generation();
        let d = self
            .resolve_materialization_job(
                job_id,
                None,
                crate::state::JobState::ResolvedFromSource,
                serving_generation,
            )
            .await;
        self.materialization_jobs
            .remove_settled(drv_hash, d)
            .then_some(d)
    }

    // r[impl obs.metric.materialization-stalled+2]
    // r[impl sched.materialize.routing+7]
    /// PD-20 (design §2.5, Phase B T-6.1): the materialization-job
    /// housekeeping arm — both the parked re-evaluation AND the
    /// unclaimed age-out (the partition covers all `holder()=None`
    /// entries). Every tick, flag-on, leader-only:
    ///
    ///   1. **Parked re-evaluation**: every parked job is re-read against its
    ///      node's durable closure evidence. Vouched/Pending — a
    ///      buildable dependency closure exists (produced by other
    ///      builds, or normally dep-gated) — resolves the job
    ///      `resolved_from_source` NOW (the same arm-1/arm-2 disposition
    ///      the consumption routing takes) and requeues the node for
    ///      normal dispatch: the park can never outlive from-source
    ///      viability. A CHILDLESS LEAF with a non-pruned origin
    ///      converts too (merged_bug_301: a structural leaf has no
    ///      closure to be missing — from-source is viable; pre-fix the
    ///      conflated Broken cell stranded leaves parked forever).
    ///      Pruned-origin and holed evidence stay parked, with the
    ///      backoff-expiry re-claim as the armed action.
    ///   2. **Unclaimed age-out** — the sh-044 backstop (jobs with
    ///      `parked_until.is_none_or(|u| u <= now)` past `max_attempts
    ///      × attempt_deadline_secs`); see the
    ///      `r[impl sched.materialize.unclaimed-age-out]` block.
    ///   3. **Visibility**: `rio_scheduler_materialization_stalled`
    ///      (gauge) is set to the ground-truth count of jobs the
    ///      partition would collect that this pass left in the view
    ///      — both populations, recounted post-loop from the view
    ///      itself (no manual ±1) — the §2.5 operator signal ("a
    ///      genuinely dead upstream makes builds wait visibly"). Set
    ///      from truth every tick (the handle_tick snapshot-sourced
    ///      re-emit self-healing discipline), so resolutions, re-arms, and
    ///      cancellations are never missed decrements.
    ///
    /// Leader-only by construction (`handle_tick` returns early on
    /// standby).
    // r[impl sched.materialize.settlement]
    pub(super) async fn tick_reevaluate_materialization_jobs(
        &mut self,
        _authority: &super::DagAuthority,
    ) {
        let now = std::time::Instant::now();
        // sh-044: the unclaimed age-out threshold. With the phase-17
        // candidate filter, a Ready node carrying an unresolved job is
        // skipped until `remove_settled` evicts the entry; for a job
        // NO executor ever lists/claims (Pending-unclaimed,
        // `parked_until=None`, no open attempt row), neither phase-12
        // nor the parked-conversion arm below reaches it. The age-out
        // arm closes this residual: every Ready node carrying an
        // unresolved job is re-admitted to phase-17 once `created_at`
        // exceeds `max_attempts × attempt_deadline_secs`, on the next
        // tick the entry is unclaimed and not currently parked —
        // bounded above by `(max_attempts+1) × attempt_deadline_secs +
        // park_backoff_cap_secs` (default ≈ 4h15m), unconditionally
        // (`holder()=Some` is itself bounded by phase-12 attempt-expiry
        // / the merged_bug_055 ghost two-strike repair;
        // `parked_until>now` by `park_backoff_cap_secs`).
        let age_out_after = std::time::Duration::from_secs(
            u64::from(self.materialization_cfg.max_attempts)
                .saturating_mul(self.materialization_cfg.attempt_deadline_secs),
        );
        let mut parked: Vec<(DrvHash, Uuid, crate::state::JobOrigin)> = Vec::new();
        let mut aged_out: Vec<(DrvHash, Uuid, crate::state::JobOrigin)> = Vec::new();
        // One `requeue_after_attempt` over the union of both arms'
        // settled entries (sh-044 r1): per-entry calls pay a per-call
        // leader check + fresh `affected` HashSet + per-call
        // `emit_progress`, so cross-entry build-id dedup is lost (N
        // settled drvs sharing one build → N progress emits); the
        // first-post-deploy backlog scenario can age out hundreds in
        // one pass.
        let mut requeue: Vec<DrvHash> = Vec::new();
        for (h, e) in self.materialization_jobs.iter() {
            if e.holder().is_some() {
                continue;
            }
            // The two arms PARTITION `holder()=None` on the park axis:
            // `is_none_or(|u| u <= now)` is the exact Boolean
            // complement of `is_some_and(|u| u > now)` — no gap, no
            // overlap. `parked_until` is written only at
            // `{new_unclaimed, park, entry_from_recovered_row}` and no
            // live-process path resets `Some→None` (the four
            // claim-lifecycle mutators write `episode`/`defer_until`
            // only), so a once-parked-then-abandoned entry sits at
            // `Some(past)` indefinitely; `is_none()` would miss it.
            if e.parked_until.is_some_and(|until| until > now) {
                parked.push((h.clone(), e.job_id, e.origin));
            } else if e.created_at.elapsed() > age_out_after {
                // Never-parked OR park-expired, unclaimed, past the
                // age-out threshold. A RetryLater-deferred job
                // (`defer_until=Some`, `parked_until=None`,
                // `holder=None` — merged_bug_178 view-only pacing)
                // MATCHES intentionally: a job that has only ever been
                // deferred for `max_attempts × attempt_deadline_secs`
                // (default 3 h) without one settled close IS the
                // never-touched case this arm targets.
                aged_out.push((h.clone(), e.job_id, e.origin));
            }
        }
        // sh-044 r2: per-tick PG-await quota on the aged-out arm.
        // Over-quota entries stay in the view unchanged; the predicate
        // matches again next tick (the cap drains the backlog over
        // ⌈backlog/MAX_AGEOUT_PER_TICK⌉ ticks instead of one
        // serial-PG burst).
        if aged_out.len() > MAX_AGEOUT_PER_TICK {
            tracing::warn!(
                aged_out = aged_out.len(),
                cap = MAX_AGEOUT_PER_TICK,
                "phase-15 aged-out backlog over MAX_AGEOUT_PER_TICK; the \
                 over-quota tail stays in the view (re-evaluated next tick)"
            );
            aged_out.truncate(MAX_AGEOUT_PER_TICK);
        }
        for (drv_hash, job_id, origin) in parked {
            // Classify over the DURABLE relation (T-D2.2/PD-D4 — the
            // same three-part criterion the consumption routing uses;
            // a stale in-memory view must neither strand a buildable
            // closure nor auto-resolve on dead previous-generation
            // evidence). One query per parked job: bounded by the
            // stalled population, which is small by construction
            // (alerted at >0 for 15m).
            let Some(db_id) = self.dag.node(drv_hash.as_str()).and_then(|s| s.db_id) else {
                continue;
            };
            let evidence = match self.db.classify_durable_evidence(db_id).await {
                Ok(ev) => ev,
                Err(e) => {
                    // Conservative: an unanswerable classification
                    // keeps the job parked (the armed action remains
                    // park-expiry re-claim).
                    warn!(drv_hash = %drv_hash, error = %e,
                          "park re-evaluation evidence query failed; job stays parked");
                    continue;
                }
            };
            // The origin read is the IN-MEMORY mirror (merged_bug_301
            // / sh-044 r2): `entry.origin` is PG-authoritative
            // (upward-only refresh on the dedup-upgrade edge in
            // `feed_job_view_entry`), so both arms read the same
            // single source of truth and the second per-entry PG
            // round-trip (`unresolved_job_for_derivation` solely for
            // origin) is dead. The ChildlessLeaf cell is
            // from-source-viable exactly when the origin is not
            // `pruned` — a structural leaf has no closure to be
            // missing, while a pruned root's closure was deliberately
            // dropped.
            if !from_source_viable(evidence, Some(origin)) {
                continue;
            }
            // r[impl sched.materialize.conversion-strictness]
            // Item T strictness gate (default-off; whole-arm scope —
            // every origin's Vouched/Pending conversion). Both halves
            // gate the CONVERSION ACT only: the park predicate, the
            // party-blind budget fold, and the stalled-gauge
            // definition are untouched (OQ1 amendment 1 forecloses
            // re-keying parking), so default-off is byte-identical
            // and knob-ON extends the gauge population by exactly the
            // deferred-conversion class. A refused job stays parked:
            // counted by the gauge below, armed via park-expiry
            // re-claim, accruing further worker charges across cycles.
            // Post-reversal population note (2026-06-03, Q5):
            // establishment-only-parked jobs (zero worker charges —
            // the never-reporting-replica crash-loop, which the
            // reversal sends HERE instead of re-listing forever) can
            // never satisfy the worker-charge gate; with the knob ON
            // they stay parked until a worker charge lands or the job
            // is cancelled. Deliberate — see the
            // conversion_requires_worker_charge config doc.
            let strict_worker = self.materialization_cfg.conversion_requires_worker_charge;
            let dwell_secs = self.materialization_cfg.conversion_min_park_dwell_secs;
            let max_attempts = self.materialization_cfg.max_attempts;
            if strict_worker {
                let worker_only = self.mat_counters(&drv_hash).worker_infra_since_reset;
                if worker_only < max_attempts {
                    debug!(
                        drv_hash = %drv_hash, %job_id, worker_only, max_attempts,
                        "conversion deferred: worker-reported charges alone do not \
                         exhaust the budget (conversion_requires_worker_charge)"
                    );
                    continue;
                }
            }
            if dwell_secs > 0 {
                // Boundary: dwell_secs ≤ max_satisfiable_dwell_secs()
                // (= cap - 1) by config validation — a visited job's
                // clock truncates below the cap, so the gate is
                // reachable for every accepted dwell (bug_088).
                let dwell_met = self
                    .materialization_jobs
                    .get(&drv_hash)
                    .and_then(|e| e.parked_at)
                    .is_some_and(|began| began.elapsed().as_secs() >= dwell_secs);
                if !dwell_met {
                    debug!(
                        drv_hash = %drv_hash, %job_id, dwell_secs,
                        "conversion deferred: minimum park dwell not yet elapsed \
                         (conversion_min_park_dwell_secs)"
                    );
                    continue;
                }
            }
            // Item T conversion visibility: the origin label reuses
            // the in-memory PG-authoritative mirror (the upward-only
            // refresh keeps it equal to the durable column).
            let origin_label = origin.as_str();
            // From-source is viable: resolve the job (no exec_id — the
            // re-evaluation, not an execution, resolved it) and requeue
            // the node. The spawn-intent filter and the admission table
            // stop excluding the node the moment the job row is
            // terminal.
            //
            // Item T (harden-store reconciliation memo §6.2): every
            // PD-20 conversion — a TIME-driven from-source disposition
            // of a job whose park budget exhausted while from-source
            // stayed viable — is counted, discriminated by origin.
            // `origin="cache_opportunity"` conversions are upstream-
            // available content converting to builds (the incident's
            // outcome class re-entering through exhaustion): the
            // RioSchedulerMaterializationConversions alert watches
            // their sustained rate. Applied-only (the at-most-once
            // edge), so a deposed leader's fenced resolve never counts.
            // The view removal + every companion gate on the resolve
            // disposition (sched.materialize.view-settlement): a Fenced
            // or Failed resolve keeps the job parked — counted by the
            // gauge below, re-evaluated next tick.
            if let Some(d) = self.settle_resolved_from_source(&drv_hash, job_id).await {
                if d == WriteDisposition::Applied {
                    metrics::counter!(
                        "rio_scheduler_materialization_converted_total",
                        "origin" => origin_label
                    )
                    .increment(1);
                }
                tracing::info!(
                    drv_hash = %drv_hash,
                    %job_id,
                    ?evidence,
                    origin = origin_label,
                    "parked materialization job re-evaluated: from-source is viable; \
                     resolved from_source and requeued (PD-20)"
                );
                requeue.push(drv_hash);
            }
        }
        // sh-044: the unclaimed age-out arm. Shares the
        // `from_source_viable` gate and the `resolve_materialization_job
        // → remove_settled → requeue_after_attempt + counter` body with
        // the parked-conversion arm above, but does NOT consult the
        // Item-T strictness knobs (`conversion_requires_worker_charge`
        // / `conversion_min_park_dwell_secs`): a never-touched job has
        // zero worker charges and zero park dwell by construction, so
        // both knobs would refuse forever, and a
        // once-parked-then-abandoned job (establishment-only crash-loop
        // → park → executor gone) likewise has zero worker charges and
        // an EXPIRED dwell window the parked-conversion arm already
        // dropped — the age-out is the executor-liveness backstop, not
        // a budget-exhaustion conversion. The asymmetry is recorded in
        // the SEPARATE counter (NOT a `reason` label on
        // `…_converted_total`: `SeededSeries` is single-axis-only, so a
        // `{origin,reason}` live series would desync from the seeded
        // `{origin}`-only series — birth-gap protection lost).
        //
        // The `from_source_viable` gate IS consulted (mirroring the
        // parked arm): a `ChildlessLeaf+Pruned` node — closure
        // deliberately dropped — must NOT be evicted-and-requeued (it
        // would re-probe cached → new mat job → 3h loop, or dispatch a
        // build whose closure was dropped) and must stay in the stalled
        // gauge. The evidence read is the per-entry
        // `classify_durable_evidence` PG round-trip (the same
        // serial-await shape the parked arm already exhibits, capped at
        // `MAX_AGEOUT_PER_TICK` so the first-post-deploy backlog cannot
        // serial-drain in one tick); the ORIGIN read is in-memory
        // `entry.origin` (the PG-authoritative mirror — same source as
        // the parked arm).
        // r[impl sched.materialize.unclaimed-age-out]
        for (drv_hash, job_id, origin) in aged_out {
            let Some(db_id) = self.dag.node(drv_hash.as_str()).and_then(|s| s.db_id) else {
                // No durable identity → no evidence to classify; the
                // job stays in the view (the backstop sweep folds moot
                // rows). Counted as stalled by the post-loop recount.
                continue;
            };
            let evidence = match self.db.classify_durable_evidence(db_id).await {
                Ok(ev) => ev,
                Err(e) => {
                    warn!(drv_hash = %drv_hash, error = %e,
                          "age-out evidence query failed; job stays unclaimed (stalled)");
                    continue;
                }
            };
            if !from_source_viable(evidence, Some(origin)) {
                // Same as the parked arm's `continue`: not viable →
                // stays in the view, counted as stalled.
                continue;
            }
            if let Some(d) = self.settle_resolved_from_source(&drv_hash, job_id).await {
                if d == WriteDisposition::Applied {
                    metrics::counter!(
                        "rio_scheduler_materialization_aged_out_total",
                        "origin" => origin.as_str()
                    )
                    .increment(1);
                }
                tracing::info!(
                    drv_hash = %drv_hash,
                    %job_id,
                    ?evidence,
                    origin = origin.as_str(),
                    age_out_after_secs = age_out_after.as_secs(),
                    "unclaimed materialization job aged out: resolved from_source \
                     and requeued (no executor reached it within max_attempts × \
                     attempt_deadline_secs; the next phase-17 probe re-creates the \
                     job if the cache fact still holds)"
                );
                requeue.push(drv_hash);
            }
        }
        self.requeue_after_attempt(&requeue, crate::state::AttemptKind::Materialization, None)
            .await;
        // The stalled gauge: ground truth after the re-evaluation pass
        // — derived STRUCTURALLY by re-counting the view (sh-044 r2:
        // manual `still_parked ±= 1` across six sites missed the
        // aged-out arm's Fenced/Failed-resolve `None` path —
        // `settle_resolved_from_source` returns `None`, the entry
        // survives, no `+= 1` ran, and the gauge undercounted by
        // exactly that population). The recount is the same
        // `holder()=None` partition predicate the loop above used:
        // parked entries the conversion did not resolve PLUS aged-out
        // entries the `from_source_viable` gate (or the resolve, or
        // the per-tick quota) refused — both are genuinely stuck; the
        // §2.5 operator signal.
        let still_parked = self
            .materialization_jobs
            .iter()
            .filter(|(_, e)| {
                e.holder().is_none()
                    && (e.parked_until.is_some_and(|u| u > now)
                        || e.created_at.elapsed() > age_out_after)
            })
            .count();
        crate::observability::LeaderGauge::MaterializationStalled.set(still_parked as f64);
    }

    /// The PG-backed view backstop (merged_bug_246's sweep half +
    /// bug_385's scheduler leg): low-frequency reconciliation of the
    /// in-memory view against the durable pending rows. Rows the view
    /// does not track are RE-FED from PG (a live node's job becomes
    /// claimable again instead of answering Gone-by-absence; a moot
    /// row — node terminal/absent — gains the entry the zero-interest
    /// canceler needs and is closed charge-free on the same tick,
    /// converging the refusal-producing rows). Skips when Unavailable
    /// (ticks → skip; the next recovery hydrates wholesale).
    pub(super) async fn tick_backstop_materialization_jobs(
        &mut self,
        _authority: &super::DagAuthority,
    ) {
        const BACKSTOP_EVERY: u64 = 30;
        if !self.tick_count.is_multiple_of(BACKSTOP_EVERY) {
            return;
        }
        if self.materialization_jobs.hydrated().is_none() {
            return;
        }
        let rows = match self.db.load_unresolved_materialization_jobs().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "materialization view backstop load failed; retried next pass");
                return;
            }
        };
        let mut refed = 0usize;
        for row in rows {
            let drv_hash = DrvHash::from(row.drv_hash.as_str());
            if self.materialization_jobs.get(&drv_hash).is_none() {
                let entry = Self::entry_from_recovered_row(row);
                if let Some(view) = self.materialization_jobs.hydrated_mut() {
                    view.entry_or_insert(drv_hash, entry);
                    refed += 1;
                }
            }
        }
        if refed > 0 {
            tracing::info!(
                refed,
                "materialization view backstop re-fed untracked pending rows from PG (moot rows are closed by the zero-interest pass this tick)"
            );
        }
    }

    /// The flag-gated housekeeping backstop — the MOOT sweep: resolve
    /// jobs whose node can never consume them — quantifier: census(node_completed_by_other_means_resolves_obsolete) —
    /// each face under its own alphabet letter (live_061 —
    /// `JobState::Obsolete` finally has its writer):
    ///
    ///   - **Obsolete**: the node COMPLETED by other means while the
    ///     job was open (store probe found the outputs, a sibling
    ///     build produced them, CA cutoff) — the enum's exact letter.
    ///     Every claim against the completed node answers `Gone`, so
    ///     pre-fix these rows were the zombie heads that pinned the
    ///     oldest-first listing window — and they resolved under the
    ///     WRONG letter (`cancelled`), leaving
    ///     `resolved_total{outcome="obsolete"}` zero-forever and the
    ///     by-other-means class forensically invisible.
    ///   - **Cancelled**: zero live interest remains — node gone,
    ///     node doomed-terminal (failed/poisoned/dep-failed/
    ///     cancelled/skipped), or every interested build terminal —
    ///     BC-2's no-controller closer, semantics unchanged. The
    ///     doomed-terminal faces stay under `cancelled` BY DERIVATION:
    ///     "produced by other means" is false for them, and their
    ///     interest dies in the same cascade that doomed the node.
    ///
    /// The partition is disjoint by construction (Completed takes the
    /// obsolete arm; every other moot face takes cancelled). Each
    /// class is bounded per tick by [`MOOT_SWEEP_TICK_BOUND`] —
    /// level-triggered: the truncated remainder is re-collected next
    /// tick. Phase B's build-terminal hooks will call the batch
    /// closer directly; in Phase A it is reached through this tick
    /// backstop. census[test: zero_interest_cancel_closes_attempt_without_dag_node]
    ///
    /// ONE fenced sweep per class (the `cancel_build_derivations`
    /// batched-persist precedent — `build.rs` documents the N+1 actor
    /// stall this avoids). live_053 measured the per-job form: 5,258
    /// sequential fenced cancels at 3.16ms each = 16.6s inside a
    /// single 134.65s Tick, head-of-line blocking every queued RPC.
    /// The sweep resolves the job rows AND closes the kind-guarded
    /// assignment rows keyed entirely on durable state: the close is
    /// TOTAL over the DAG-absent arm (its own trigger — the
    /// `None => cancelled` arm — guarantees the node may be gone, so
    /// no in-memory exec_id is ever read). Per-job view removal gates
    /// on the folded disposition; a Fenced or Failed sweep keeps
    /// every entry and re-attempts next tick (level-triggered).
    ///
    /// (The fn keeps its wired name: the housekeeping call site and
    /// its phase label are outside this close's surface — this doc
    /// carries the widened law.)
    // r[impl sched.materialize.settlement]
    // r[impl sched.materialize.job+2]
    // r[impl sched.materialize.view-settlement]
    // r[impl sched.materialize.obsolescence]
    // r[impl sched.admission.work-per-turn]
    pub(super) async fn tick_cancel_zero_interest_materialization(
        &mut self,
        _authority: &super::DagAuthority,
    ) {
        use crate::state::BuildStateExt;
        let mut obsolete: Vec<(DrvHash, Uuid)> = Vec::new();
        let mut zero_interest: Vec<(DrvHash, Uuid)> = Vec::new();
        for (h, entry) in self.materialization_jobs.iter() {
            match self.dag.node(h.as_str()) {
                None => zero_interest.push((h.clone(), entry.job_id)),
                Some(state) if state.status() == DerivationStatus::Completed => {
                    // The dead detection edge live_061 exposed, now an
                    // edge: the skew detector quantified only over
                    // Assigned/Running nodes (split_release,
                    // claimed_no_attempt) while the zombie face —
                    // terminal node, pending job — had no polarity and
                    // never fired through the whole incident. Counted
                    // at OBSERVATION (this sweep SAW the skew); the
                    // lifecycle counter below counts at the APPLIED
                    // resolve, so the two stay independently honest.
                    // No two-strike insurance needed: node-Completed
                    // and job-pending are both read in this same actor
                    // turn and neither can revert.
                    metrics::counter!(
                        "rio_scheduler_materialization_view_node_skew_total",
                        "polarity" => "node_terminal_job_pending"
                    )
                    .increment(1);
                    obsolete.push((h.clone(), entry.job_id));
                }
                Some(state) => {
                    if state.status().is_terminal()
                        || !state.interested_builds.iter().any(|bid| {
                            self.builds
                                .get(bid)
                                .is_some_and(|b| !b.state().is_terminal())
                        })
                    {
                        zero_interest.push((h.clone(), entry.job_id));
                    }
                }
            }
        }
        // R17 per-tick bound, per class: the truncated tail keeps its
        // view entries (removal gates on settled dispositions), so the
        // next tick re-collects it — level-triggered.
        obsolete.truncate(MOOT_SWEEP_TICK_BOUND);
        zero_interest.truncate(MOOT_SWEEP_TICK_BOUND);
        let mut settled = 0usize;
        settled += self
            .sweep_moot_class(obsolete, crate::state::JobState::Obsolete)
            .await;
        settled += self
            .sweep_moot_class(zero_interest, crate::state::JobState::Cancelled)
            .await;
        if settled > 0 {
            // r[impl sched.materialize.pinning]
            // §5.3 release site: the sweep resolves jobs, so pins may
            // be releasable (self-scoping no-op when live interest
            // remains elsewhere). ONCE per tick over both classes: the
            // release is a global resolved-jobs query, so the per-job
            // form ran N identical statements for one effect.
            self.release_materialization_pins_best_effort("job moot sweep")
                .await;
        }
    }

    /// One moot class through the batched fenced closer: resolve the
    /// rows to `to_state`, fold per-job dispositions (the T-6.2
    /// lifecycle counter on each APPLIED row), settle the view.
    /// Returns the settled count; the caller runs the pin release
    /// once over both classes.
    // r[impl sched.materialize.obsolescence]
    async fn sweep_moot_class(
        &mut self,
        class: Vec<(DrvHash, Uuid)>,
        to_state: crate::state::JobState,
    ) -> usize {
        if class.is_empty() {
            return 0;
        }
        let job_ids: Vec<Uuid> = class.iter().map(|(_, job_id)| *job_id).collect();
        let serving_generation = self.serving_generation();
        let resolved = match self
            .db
            .resolve_moot_jobs_and_close_attempts_fenced(&job_ids, to_state, serving_generation)
            .await
        {
            Ok(crate::db::materialization::FencedCancelSweep::Applied { resolved }) => resolved,
            Ok(crate::db::materialization::FencedCancelSweep::Fenced) => {
                // Sweep-level fence: every view entry survives (the
                // durable rows are a successor's to settle); one note
                // per sweep, not per job.
                self.note_fenced_evidence_write("materialization job moot sweep");
                return 0;
            }
            Err(e) => {
                warn!(jobs = job_ids.len(), to_state = to_state.as_str(), error = %e,
                      "materialization moot sweep failed; retried next tick");
                return 0;
            }
        };
        let mut settled = 0usize;
        for (drv_hash, job_id) in class {
            let d = if resolved.contains(&job_id) {
                // The at-most-once edge: the same lifecycle counter the
                // exec-keyed resolver increments (T-6.2) — once per
                // APPLIED row, exactly as the per-job form counted.
                metrics::counter!(
                    "rio_scheduler_materialization_jobs_resolved_total",
                    "outcome" => Self::resolution_outcome_label(to_state)
                )
                .increment(1);
                WriteDisposition::Applied
            } else {
                WriteDisposition::AlreadyResolved
            };
            if self.materialization_jobs.remove_settled(&drv_hash, d) {
                settled += 1;
                if to_state == crate::state::JobState::Obsolete {
                    tracing::info!(
                        drv_hash = %drv_hash,
                        %job_id,
                        "materialization job obsolete: the node completed by other \
                         means while the job was open (attempt closed in the same \
                         fenced sweep; the row leaves the claimable plane)"
                    );
                } else {
                    tracing::info!(
                        drv_hash = %drv_hash,
                        %job_id,
                        "materialization job cancelled: no live interested build remains \
                         (attempt closed in the same fenced sweep)"
                    );
                }
            }
        }
        settled
    }
}
