//! In-house leader election on top of `kube::Api<Lease>`.
//!
//! Models client-go's `resourcelock` + `leaderelection` packages.
//! Two properties the previous crate (`kube-leader-election` 0.43)
//! lacked:
//!
//! 1. **Optimistic concurrency.** Acquire and renew go through
//!    `Api::replace()` (HTTP PUT), which requires
//!    `metadata.resourceVersion` from the preceding GET. If the
//!    object changed between GET and PUT, the apiserver returns
//!    409 Conflict. Exactly one of N racing writers succeeds. The
//!    old crate used `Patch::Merge` with no precondition — every
//!    racer got HTTP 200, last write won, every racer believed it
//!    had acquired.
//!
//! 2. **Observed-record expiry.** A standby doesn't trust the
//!    Lease's `renewTime` VALUE (written by a *different* node's
//!    clock). Instead it records the holder-authored spec content —
//!    `(holderIdentity, renewTime bytes)`, compared for CHANGE only —
//!    plus a local monotonic `Instant` when that content was first
//!    seen. A live leader renews every 5s, moving `renewTime` every
//!    5s. If the content doesn't change for the steal threshold of
//!    *local* time, the holder isn't writing —
//!    steal. (Raw `resourceVersion` movement deliberately does NOT
//!    reset the clock: the apiserver bumps rv on every object write,
//!    including non-protocol annotation patches — merged_bug_180.)
//!    The steal threshold (`STEAL_AFTER`, 19s) is deliberately
//!    LATER than the leader's own self-fence deadline
//!    (`SELF_FENCE_AFTER`, 11s): by the time we steal, the deposed
//!    leader has already stopped believing. Cross-node clock skew is
//!    irrelevant up to the margin; only our own `Instant`
//!    monotonicity matters.
//!
//! The split into a pure `decide()` function + an I/O shell is
//! deliberate: `decide()` is table-tested with no kube client.
//! The shell's 409-handling is integration-tested against a
//! mock apiserver.

use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff;
use kube::api::{Api, ObjectMeta, PostParams};
use rio_crds::KubeErrorExt;
use tracing::{debug, warn};

/// Result of one `try_acquire_or_renew()` call.
#[derive(Debug, PartialEq, Eq)]
pub enum ElectionResult {
    /// We hold the lease (acquired or renewed this tick).
    ///
    /// `transitions` is the lease's `leaseTransitions` count as of the
    /// write that produced this result: 0 from `create()` (the lease was
    /// born with us as holder — zero holder *changes* have happened),
    /// `old + 1` from a steal (the rv-guarded PUT bumped it atomically
    /// with the holder change), unchanged from a renew. The caller
    /// derives the leadership generation from it
    /// ([`LeaderState::on_acquire`](crate::LeaderState::on_acquire)) —
    /// because the apiserver's CAS admits exactly one writer per
    /// resourceVersion, two replicas that both believe they lead can
    /// never have acquired at the same transition count.
    Leading {
        /// `LeaseSpec.lease_transitions` as written by the PUT/POST that
        /// made us leader.
        transitions: u64,
    },
    /// Someone else holds it and our observed-record clock hasn't
    /// elapsed yet. Steady state for a standby — no log.
    Standby,
    /// We tried to `replace()` and the apiserver returned 409.
    /// Someone (or some write of OURS that we cancelled) mutated the
    /// lease between our GET and PUT.
    ///
    /// On a **renew** the 409 proves only that the rv moved — NOT that
    /// the holder changed: our own zombie commit from the
    /// cancelled-write ledger and a foreign metadata-only patch are
    /// both non-lose rv-movers. The loop defers one round and lets the
    /// next completed read resolve who holds
    /// (`sched.lease.holder-evidenced-lose`).
    ///
    /// On a **steal** this means another standby raced us and won.
    /// We were never leading; next tick's GET reveals the winner.
    ///
    /// Caller treats both as `now_leading = false`; the believing
    /// 409's defer-then-resolve is the loop's edge detection.
    Conflict,
}

/// What `decide()` wants the I/O shell to do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// We're the current holder — update `renew_time` only.
    Renew,
    /// Holder is stale or absent — take over. Sets `acquire_time`,
    /// bumps `lease_transitions`, changes `holder_identity` to us.
    Steal,
    /// Holder is fresh — do nothing, wait for next tick.
    Standby,
}

/// Facts from a completed GET+decide phase, surfaced to the lease loop
/// for the own-commit evidence rule (`sched.lease.cancelled-write`).
/// Facts only, never a capability: the loop cannot touch the lease
/// through these.
#[derive(Debug)]
pub(crate) struct FetchFacts {
    /// The fetched lease names this replica as holder.
    pub(crate) holder_is_us: bool,
    /// `spec.renewTime` of the fetched lease as authored bytes (None
    /// if absent). The own-commit evidence consumer compares this for
    /// CHANGE against the last completed read — a protocol write moves
    /// it, a foreign metadata patch does not (merged_bug_180).
    pub(crate) renew_time: Option<String>,
    /// `spec.leaseTransitions` clamped at 0 (the same clamp the act
    /// phase's `replace()` applies).
    pub(crate) transitions: u64,
}

/// Outcome of one phase-bounded election tick
/// ([`LeaderElection::renew_phased`]). Classifies WHICH phase failed —
/// the distinction the cancelled-write ledger keys on.
#[derive(Debug)]
pub(crate) enum RenewOutcome {
    /// Both phases completed: the apiserver answered the full
    /// round-trip. `facts` is `None` only on the 404→Create path
    /// (there was no lease to fetch).
    Completed {
        result: ElectionResult,
        facts: Option<FetchFacts>,
    },
    /// The read phase completed but the act phase failed or timed
    /// out. With `put_transmitted`, a mutating request may have left
    /// this process before the failure — the write may still commit
    /// server-side ("cancelled" is not "discarded"), which is the
    /// mint condition for the loop's unconfirmed-write ledger.
    FetchedActFailed {
        facts: Option<FetchFacts>,
        put_transmitted: bool,
        /// `None` = the phase deadline elapsed; `Some` = the apiserver
        /// (or transport) answered with an error.
        error: Option<kube::Error>,
    },
    /// The read phase itself failed or timed out: provably blind, and
    /// provably nothing was transmitted (the act never ran).
    FetchFailed { error: Option<kube::Error> },
}

/// The result of [`LeaderElection::fetch_and_decide`]: everything the
/// act phase needs to finish the round-trip.
///
/// Carrying the fetched [`Lease`] (rather than re-fetching in the act
/// phase) is load-bearing: the PUT's optimistic-concurrency guard is the
/// `metadata.resourceVersion` of *this* GET, so a write that races in
/// between the two phases is rejected with 409 — the CAS the formal
/// model verifies as `casOk` / `atMostOneCASWinner`.
#[derive(Debug)]
pub(crate) enum FetchOutcome {
    /// The GET returned 404 — no lease exists. The act phase POSTs a
    /// fresh one (which itself races: the apiserver admits exactly one
    /// creator, the rest get 409).
    Create,
    /// A lease exists and [`decide`] chose what to do with it. The
    /// lease is carried for the act phase's rv-guarded PUT; on
    /// `Decision::Standby` it is simply dropped. Boxed so the
    /// payload-free `Create` variant doesn't pay for the largest
    /// variant's size (`clippy::large_enum_variant`).
    Decided {
        decision: Decision,
        lease: Box<Lease>,
    },
}

/// client-go's "observed record": when did WE (this process, our
/// monotonic clock) first see this holder-authored spec content? The
/// identity is `(holderIdentity, renewTime bytes)` — exactly the
/// fields a protocol write authors (renew moves `renewTime`; steal
/// moves both; graceful release clears the holder) — compared for
/// CHANGE only, never against any clock. If the content doesn't
/// change for the steal threshold of LOCAL time, the holder isn't
/// writing — it is dead.
///
/// Keying on spec content rather than `metadata.resourceVersion` is
/// load-bearing (merged_bug_180): the apiserver bumps rv on EVERY
/// object write, including annotation/label/ownerRef patches by
/// non-protocol tooling — a periodic foreign mutator under the steal
/// cadence would reset an rv-keyed clock forever, indefinitely
/// blocking the steal of a genuinely dead leader's lease. The
/// `renewTime` VALUE is still never compared to a local clock
/// (cross-node skew stays irrelevant); only its BYTES are compared
/// for change, which is what client-go's record comparison does.
/// Tracking `(holder, transitions)` instead would break the other
/// way: renew touches neither, so a live leader would look frozen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    holder: String,
    renew_time: Option<String>,
    at: Instant,
}

/// Projection of `(holder, our_id)` for the pure decision predicate.
/// `decide()` computes this from production data; `decide_pure()` and
/// the Kani harness operate on it directly. Collapsing the string
/// comparison into a small enum keeps `decide_pure()` CBMC-tractable —
/// CBMC can't symbolically execute over `&str` arguments.
///
/// The variants map onto `decide()`'s case structure and the formal
/// model's action partition (`docs/spec/models/leaderElection.qnt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum HolderKind {
    /// `holder` is `None` or `Some("")` — graceful step-down. Steal now.
    /// Model: a node whose snapshot shows no holder takes `steal(n)`.
    Empty,
    /// `holder == our_id` — we hold it. Renew.
    /// Model: a node whose snapshot shows itself as holder takes
    /// `renewLease(n)`.
    Us,
    /// `holder` is a non-empty string that isn't us. Standby or steal
    /// based on the observed-record clock.
    /// Model: a node whose snapshot shows another holder stays standby
    /// (no PUT action fires) or takes `steal(n)` once the observed-record
    /// clock expires.
    Other,
}

/// What `decide()` should do to its `observed` state after this tick.
/// Returned by `decide_pure()` so the side-effect can live in `decide()`
/// (which has `Instant` and `String`) while the decision logic stays in
/// `decide_pure()` (which doesn't, and is therefore Kani-verifiable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedUpdate {
    /// Don't touch `observed` — the lease state didn't change in a way
    /// that resets the staleness clock.
    Keep,
    /// Clear `observed` — there's no holder to observe.
    Clear,
    /// Reset the clock — the holder-authored spec content changed (the
    /// protocol wrote) so the holder is alive. Re-observe at the new
    /// content.
    StartObserving,
}

/// Pure decision predicate. No I/O, no `Instant`, no `String` — every
/// argument and return type is CBMC-tractable. `decide()` projects
/// production data into these types, calls this, and applies the
/// returned `ObservedUpdate`.
///
/// The case structure here is the formal contract: it parallels the
/// per-node action disjunction reachable from `step` in
/// `docs/spec/models/leaderElection.qnt` (`apiGet`, `steal`,
/// `renewLease`, `conflict`, …). When either changes, update the other.
///
/// `matched_observation_age_ms` collapses two production cases into
/// one: it's `Some(age)` only when there IS an observation AND its
/// holder-authored content matches the fetched lease's. `None` covers
/// both "no observation" and "content changed" — both reset the clock
/// (`StartObserving`).
// r[impl sched.lease.k8s-lease+2]
//
// ── Kani contracts ───────────────────────────────────────────────────
// Each `ensures` clause is one direction of an iff. The contract
// closures restate decide_pure's spec from a different angle than the
// implementation: instead of `if holder is X then return Y`, they say
// `return is Y iff holder is X`. A bug that flips a comparison
// (`>` to `>=`) or drops a case fails the contract.
//
// The case structure parallels the {steal, renewLease, standby} case
// partition across the per-node actions reachable from `step` in
// docs/spec/models/leaderElection.qnt.
// When either changes, update the other — `tracey bump` on the spec
// rule will flag both.
//
// Verified by check_decide_pure_contract in #[cfg(kani)] mod kani_proofs.
#[cfg_attr(kani, kani::ensures(|r: &(Decision, ObservedUpdate)| {
    // Steal iff holder is empty, OR (holder is Other AND the observed
    // content matched AND has been stale for > the steal threshold).
    (r.0 == Decision::Steal) == (
        matches!(holder, HolderKind::Empty)
        || (matches!(holder, HolderKind::Other)
            && matched_observation_age_ms.is_some_and(|age| age > steal_after_ms))
    )
}))]
#[cfg_attr(kani, kani::ensures(|r: &(Decision, ObservedUpdate)| {
    // Renew iff holder is us. Steal and Standby are never returned for
    // a holder we own — the rv guard on replace() catches staleness,
    // not decide().
    (r.0 == Decision::Renew) == matches!(holder, HolderKind::Us)
}))]
#[cfg_attr(kani, kani::ensures(|r: &(Decision, ObservedUpdate)| {
    // Observed cleared iff holder is empty (graceful step-down).
    (r.1 == ObservedUpdate::Clear) == matches!(holder, HolderKind::Empty)
}))]
#[cfg_attr(kani, kani::ensures(|r: &(Decision, ObservedUpdate)| {
    // StartObserving iff holder is Other AND no matching observation.
    (r.1 == ObservedUpdate::StartObserving) == (
        matches!(holder, HolderKind::Other)
        && matched_observation_age_ms.is_none()
    )
}))]
pub(crate) fn decide_pure(
    holder: HolderKind,
    matched_observation_age_ms: Option<u64>,
    steal_after_ms: u64,
) -> (Decision, ObservedUpdate) {
    match holder {
        // Empty holder: previous leader stepped down gracefully (set
        // holder_identity: None). No one to wait for — steal now.
        HolderKind::Empty => (Decision::Steal, ObservedUpdate::Clear),

        // We hold it. Renew. Don't touch `observed` — it tracks OTHER
        // holders' activity, not ours. If we restart with the same
        // holder_id, this branch still applies; the rv guard on
        // replace() catches any staleness.
        HolderKind::Us => (Decision::Renew, ObservedUpdate::Keep),

        // Someone else holds it. Check the observed-record clock.
        HolderKind::Other => match matched_observation_age_ms {
            // Same holder-authored content we saw before — the
            // protocol hasn't written since. It's been > the steal
            // threshold since we FIRST saw it (the renewTime VALUE is
            // never compared to a clock — only its bytes, for change).
            // The holder is dead — and it
            // self-fenced 2×FENCE_MARGIN ago, so it already stopped
            // believing. Steal.
            //
            // Strict `>` (not `>=`) is an implementation choice: steal
            // only when *strictly* past the threshold.
            // `sched.lease.k8s-lease` is silent on the boundary case;
            // the Kani contract above codifies this choice (the
            // mutation `>` → `>=` fails the contract — that
            // demonstrates non-tautology, not that the spec mandates
            // `>`).
            Some(age_ms) if age_ms > steal_after_ms => (Decision::Steal, ObservedUpdate::Keep),

            // Same content, but not yet stale. Wait.
            Some(_) => (Decision::Standby, ObservedUpdate::Keep),

            // New content (first observation, or the leader renewed
            // since our last look). Reset the clock. This is also the
            // "first-tick penalty": even if the lease is actually
            // stale, we can't know without a prior observation, so we
            // wait one full steal threshold.
            None => (Decision::Standby, ObservedUpdate::StartObserving),
        },
    }
}

/// Pure decision function. No I/O, no clock reads — `now` is
/// injected. Separated for table testing.
///
/// Updates `observed` in place: re-stamps the record if the
/// holder-authored content `(holder, renewTime bytes)` changed since
/// the last call, leaves it alone if unchanged.
///
/// `holder` and `renew_time` are passed separately rather
/// than as `&Lease` because the caller already has both and we
/// don't want decide() coupled to the full k8s type (simpler
/// table tests).
///
/// The decision logic itself lives in `decide_pure()` (crate-private,
/// Kani-verified); this function projects production data (`&str`,
/// `Instant`) into the CBMC-tractable types `decide_pure()` takes,
/// delegates, and applies the returned `ObservedUpdate`.
pub fn decide(
    holder: Option<&str>,
    renew_time: Option<&str>,
    observed: &mut Option<Observed>,
    our_id: &str,
    steal_after: Duration,
    sent_at: Instant,
    confirmed_at: Instant,
) -> Decision {
    // ── Project production data → predicate inputs ──────────────────
    // The projection is the only place `Instant` and `String` appear.
    // `decide_pure()` is the verified core; this is the I/O-shaped shim.
    //
    // Two-clock anchor discipline (`sched.lease.k8s-lease`): staleness
    // is MEASURED against `sent_at` (the GET's send instant — the
    // earliest bound on when this read's no-write evidence was
    // confirmed) while a fresh observation is STAMPED at `confirmed_at`
    // (the response instant — the latest bound on when the observed
    // state existed). Both directions UNDERSTATE the confirmed no-write
    // span, the conservative direction for steals. Taking the two
    // instants as separate parameters is the mechanism: a caller cannot
    // re-introduce the request-anchored stamp without visibly collapsing
    // the pair.
    let holder_kind = match holder {
        None => HolderKind::Empty,
        Some("") => HolderKind::Empty,
        Some(s) if s == our_id => HolderKind::Us,
        Some(_) => HolderKind::Other,
    };
    // `Some(age)` iff there's an observation whose holder-authored
    // content `(holder, renewTime bytes)` matches the fetched lease's.
    // Collapses "no observation" and "content changed" — both go to
    // `StartObserving` in decide_pure(). A bare rv movement (foreign
    // non-protocol write) does NOT reset the clock: it changes neither
    // field.
    let matched_observation_age_ms = observed
        .as_ref()
        .filter(|o| holder.is_some_and(|h| o.holder == h) && o.renew_time.as_deref() == renew_time)
        .map(|o| sent_at.duration_since(o.at).as_millis() as u64);
    let steal_after_ms = steal_after.as_millis() as u64;

    let (decision, update) = decide_pure(holder_kind, matched_observation_age_ms, steal_after_ms);

    // ── Apply the observed-record update ────────────────────────────
    match update {
        ObservedUpdate::Keep => {}
        ObservedUpdate::Clear => *observed = None,
        ObservedUpdate::StartObserving => {
            *observed = Some(Observed {
                // StartObserving is only returned for HolderKind::Other,
                // so a holder string is always present here.
                holder: holder.unwrap_or_default().to_string(),
                renew_time: renew_time.map(str::to_string),
                at: confirmed_at,
            });
        }
    }

    decision
}

/// Test-only constructor for a content-keyed observation record: crate
/// tests (the I/O boundary tests here and the composition tests in
/// lib.rs) pre-seed staleness while the fields stay private.
#[cfg(test)]
pub(crate) fn test_observed(
    holder: &str,
    renew_time: Option<&str>,
    at: Instant,
) -> Option<Observed> {
    Some(Observed {
        holder: holder.to_string(),
        renew_time: renew_time.map(str::to_string),
        at,
    })
}

pub struct LeaderElection {
    api: Api<Lease>,
    lease_name: String,
    holder_id: String,
    /// Written to the Lease's `leaseDurationSeconds` on every PUT/POST.
    /// Documentation for `kubectl describe lease` — NOT the threshold
    /// this replica acts on (that is `steal_after`).
    ttl: Duration,
    /// How long the same resourceVersion must be observed unchanged
    /// before this replica steals. Deliberately `2×FENCE_MARGIN` LATER
    /// than the holder's own self-fence deadline (`SELF_FENCE_AFTER`,
    /// i.e. `LEASE_TTL − FENCE_MARGIN`) so the deposed holder has
    /// already stopped believing by the time anyone steals.
    steal_after: Duration,
    /// `pub(crate)` (not private) for the model-based tests
    /// (`crate::mbt_tests`): the driver re-evaluates `decide()` against a
    /// stashed snapshot at the model's PUT-step clock, which needs `&mut`
    /// access to the observed record from a sibling module. Production
    /// code outside this module never touches it.
    pub(crate) observed: Option<Observed>,
}

impl LeaderElection {
    pub fn new(
        client: kube::Client,
        namespace: &str,
        lease_name: String,
        holder_id: String,
        ttl: Duration,
        steal_after: Duration,
    ) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            lease_name,
            holder_id,
            ttl,
            steal_after,
            observed: None,
        }
    }

    /// One election tick: GET, decide, maybe PUT.
    ///
    /// `kube::Error` return is for transient apiserver failures
    /// (network, 5xx, timeout). 404 on GET and 409 on PUT are
    /// handled internally — they're expected racing outcomes, not
    /// errors. The caller retries on `Err` without flipping
    /// `is_leader`; see `run_lease_loop`'s error arm.
    ///
    /// Unbounded (no internal deadlines) — for tests and one-shot
    /// callers. The lease loop uses `Self::renew_phased` (private), whose
    /// per-phase budgets are what make an abandoned write classifiable
    /// (`sched.lease.cancelled-write`).
    pub async fn try_acquire_or_renew(&mut self) -> Result<ElectionResult, kube::Error> {
        let outcome = self.fetch_and_decide(Instant::now()).await?;
        self.act(outcome).await
    }

    /// One election tick with SEPARATE read- and write-phase budgets
    /// (`sched.lease.cancelled-write`): the GET+decide phase runs under
    /// `fetch_deadline`; only after it completes is a mutating request
    /// transmitted, under its own `act_deadline`. The split is
    /// load-bearing twice over: a truly-blind replica (failed read)
    /// transmits nothing — its rv freezes and stealing works — and a
    /// transmitted write always gets a full budget for its response, so
    /// "PUT sent but the response window was eaten by a slow GET" is
    /// unrepresentable.
    ///
    /// Failures are classified into the outcome rather than returned:
    /// the caller's ledger logic branches on WHICH phase died, and a
    /// `FetchedActFailed` whose decision was mutating means the write
    /// may have committed server-side ("cancelled" is not "discarded").
    /// The returned [`FetchFacts`] are facts, never a capability — the
    /// caller cannot act on the lease through them, so the GET/PUT
    /// fusion that prevents TOCTOU bugs is preserved.
    // r[impl sched.lease.cancelled-write+2]
    pub(crate) async fn renew_phased(
        &mut self,
        fetch_deadline: Duration,
        act_deadline: Duration,
    ) -> RenewOutcome {
        let outcome =
            match tokio::time::timeout(fetch_deadline, self.fetch_and_decide(Instant::now())).await
            {
                Err(_elapsed) => return RenewOutcome::FetchFailed { error: None },
                Ok(Err(e)) => return RenewOutcome::FetchFailed { error: Some(e) },
                Ok(Ok(outcome)) => outcome,
            };

        let facts = match &outcome {
            FetchOutcome::Create => None,
            FetchOutcome::Decided { lease, .. } => Some(FetchFacts {
                holder_is_us: lease
                    .spec
                    .as_ref()
                    .and_then(|s| s.holder_identity.as_deref())
                    == Some(&*self.holder_id),
                renew_time: lease
                    .spec
                    .as_ref()
                    .and_then(|s| s.renew_time.as_ref())
                    .map(|mt| mt.0.to_string()),
                transitions: lease
                    .spec
                    .as_ref()
                    .and_then(|s| s.lease_transitions)
                    .map_or(0, |t| u64::try_from(t).unwrap_or(0)),
            }),
        };
        // Whether the act phase will transmit a mutating request. The
        // Standby arm performs no I/O, so its act cannot fail at all;
        // Create POSTs, Renew/Steal PUT. Conservative on failure: any
        // mutating act that errors or times out counts as possibly
        // transmitted — over-recording is safe because the ledger is
        // only ever consumed by observed own-commit evidence.
        let mutating = matches!(
            &outcome,
            FetchOutcome::Create
                | FetchOutcome::Decided {
                    decision: Decision::Renew | Decision::Steal,
                    ..
                }
        );

        match tokio::time::timeout(act_deadline, self.act(outcome)).await {
            Ok(Ok(result)) => RenewOutcome::Completed { result, facts },
            Ok(Err(e)) => RenewOutcome::FetchedActFailed {
                facts,
                put_transmitted: mutating,
                error: Some(e),
            },
            Err(_elapsed) => RenewOutcome::FetchedActFailed {
                facts,
                put_transmitted: mutating,
                error: None,
            },
        }
    }

    /// The GET + decide half of [`Self::try_acquire_or_renew`].
    ///
    /// Exists so the model-based tests can drive the round-trip at the
    /// formal model's grain: `docs/spec/models/leaderElection.qnt` splits
    /// the apiserver GET and the subsequent PUT into separate actions so
    /// the two-replica CAS race (both GET the same resourceVersion, both
    /// PUT, exactly one wins) is explorable. Production code must only
    /// ever call a composition ([`Self::try_acquire_or_renew`] or the
    /// phase-bounded [`Self::renew_phased`]) — both fuse the fetch to an
    /// act attempt, so a fetch is never deliberately left un-acted-on.
    /// What a composition CAN produce is an act abandoned mid-flight by
    /// its phase deadline: the transmitted write may still commit
    /// server-side, which is exactly what the lease loop's
    /// unconfirmed-write ledger accounts for
    /// (`sched.lease.cancelled-write`).
    ///
    /// `sent_at` is the GET's send instant — the clock `decide()`
    /// measures observation staleness against; the composition passes
    /// `Instant::now()`, the model-based tests inject their mock clock.
    /// The response instant is derived as `sent_at` plus the GET's
    /// real elapsed latency, so an injected mock clock keeps a coherent
    /// timeline (latency ≈ 0 against an in-process mock) while
    /// production observations are stamped at the instant the fetched
    /// state was actually confirmed (`sched.lease.k8s-lease`'s
    /// two-clock anchor discipline; the `decide()` doc has the
    /// conservatism argument).
    pub(crate) async fn fetch_and_decide(
        &mut self,
        sent_at: Instant,
    ) -> Result<FetchOutcome, kube::Error> {
        // 1. GET. 404 → no lease exists; the act phase POSTs one.
        let fetch_started = Instant::now();
        let lease = match self.api.get_opt(&self.lease_name).await? {
            Some(l) => l,
            None => return Ok(FetchOutcome::Create),
        };
        let confirmed_at = sent_at + fetch_started.elapsed();

        // 2. Decide on the holder-authored spec content; the
        // resourceVersion stays the act phase's CAS guard and never
        // enters the decision.
        let holder = lease
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref());
        let renew_time = lease
            .spec
            .as_ref()
            .and_then(|s| s.renew_time.as_ref())
            .map(|mt| mt.0.to_string());
        let decision = decide(
            holder,
            renew_time.as_deref(),
            &mut self.observed,
            &self.holder_id,
            self.steal_after,
            sent_at,
            confirmed_at,
        );

        Ok(FetchOutcome::Decided {
            decision,
            lease: Box::new(lease),
        })
    }

    /// The act half of [`Self::try_acquire_or_renew`]: POST a fresh
    /// lease, PUT a renew/steal, or do nothing (standby). See
    /// [`Self::fetch_and_decide`] for why the round-trip is split.
    pub(crate) async fn act(
        &mut self,
        outcome: FetchOutcome,
    ) -> Result<ElectionResult, kube::Error> {
        // 3. Act.
        match outcome {
            FetchOutcome::Create => self.create().await,
            FetchOutcome::Decided { decision, lease } => match decision {
                Decision::Standby => Ok(ElectionResult::Standby),
                Decision::Renew => self.replace(*lease, false).await,
                Decision::Steal => self.replace(*lease, true).await,
            },
        }
    }

    /// Graceful release. Clears `holder_identity` so the next
    /// standby steals immediately (empty-holder branch in
    /// `decide()`) instead of waiting out the observed-record ttl.
    ///
    /// 409 on the replace() → someone already stole it → we're
    /// already not the leader → success from our perspective.
    /// Only propagates if the GET itself fails.
    pub async fn step_down(&self) -> Result<(), kube::Error> {
        let Some(mut lease) = self.api.get_opt(&self.lease_name).await? else {
            return Ok(());
        };
        if lease
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref())
            != Some(&*self.holder_id)
        {
            // Already not ours — nothing to release.
            return Ok(());
        }
        let mut spec = lease.spec.take().unwrap_or_default();
        spec.holder_identity = None;
        lease.spec = Some(spec);
        match self
            .api
            .replace(&self.lease_name, &PostParams::default(), &lease)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_conflict() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// No lease exists yet. Create it with us as holder.
    ///
    /// 409 here means another replica raced the create and won.
    /// They're the leader; we're standby. Next tick's GET will
    /// show their holderIdentity and set our observed-record.
    async fn create(&mut self) -> Result<ElectionResult, kube::Error> {
        let now = MicroTime(jiff::Timestamp::now());
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.lease_name.clone()),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(self.holder_id.clone()),
                lease_duration_seconds: Some(self.ttl.as_secs() as i32),
                acquire_time: Some(now.clone()),
                renew_time: Some(now),
                lease_transitions: Some(0),
                ..Default::default()
            }),
        };
        match self.api.create(&PostParams::default(), &lease).await {
            Ok(_) => {
                debug!(lease = %self.lease_name, "created lease");
                // We created the lease with `lease_transitions: 0` — the
                // lease was born with us as holder, no transition has
                // happened yet.
                Ok(ElectionResult::Leading { transitions: 0 })
            }
            Err(e) if e.is_conflict() => Ok(ElectionResult::Conflict),
            Err(e) => Err(e),
        }
    }

    /// Renew or steal via `replace()`. The `lease` argument is the
    /// result of the GET — it carries `metadata.resourceVersion`,
    /// which the apiserver checks. If the lease changed since the
    /// GET, we get 409.
    ///
    /// `steal`: also change holder, bump transitions, set
    /// acquire_time. Renew only touches renew_time.
    ///
    /// On successful steal, clear `observed` — we're the holder
    /// now, the observed-record tracks OTHER holders.
    ///
    /// Modeled as the rv-guarded PUT (`casOk` conjoined by `steal` and
    /// `renewLease`) in `docs/spec/models/leaderElection.qnt`.
    /// The rv-guarded CAS here is what keeps `atMostOneCASWinner` during
    /// the initial-acquisition race — the model checks it over all
    /// interleavings of N replicas, which neither the table tests nor
    /// the Kani contract on `decide_pure()` reach (both are
    /// single-replica).
    // r[impl sched.lease.at-most-one-leader+3]
    async fn replace(
        &mut self,
        mut lease: Lease,
        steal: bool,
    ) -> Result<ElectionResult, kube::Error> {
        let now = MicroTime(jiff::Timestamp::now());
        let mut spec = lease.spec.take().unwrap_or_default();
        spec.renew_time = Some(now.clone());
        spec.lease_duration_seconds = Some(self.ttl.as_secs() as i32);
        if steal {
            spec.holder_identity = Some(self.holder_id.clone());
            spec.acquire_time = Some(now);
            spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
        }
        // The transition count this PUT writes — post-bump on a steal,
        // unchanged on a renew. Captured before `spec` moves into `lease`.
        // `leaseTransitions` is i32 in the k8s API; a negative value (a
        // hand-edited or corrupt Lease object) clamps to 0 rather than
        // wrapping to a huge generation. Loud: nothing in this crate ever
        // writes a negative count, so seeing one means someone edited the
        // Lease by hand and the generation is about to restart from the
        // floor (the PG high-water seed is the backstop).
        let raw_transitions = spec.lease_transitions.unwrap_or(0);
        let transitions = u64::try_from(raw_transitions).unwrap_or_else(|_| {
            warn!(
                lease = %self.lease_name,
                lease_transitions = raw_transitions,
                "negative leaseTransitions on the Lease object (hand-edited?); \
                 clamping to 0 — the generation restarts from the floor and the \
                 PG high-water seed becomes the only collision defense"
            );
            0
        });
        lease.spec = Some(spec);

        match self
            .api
            .replace(&self.lease_name, &PostParams::default(), &lease)
            .await
        {
            Ok(_) => {
                if steal {
                    self.observed = None;
                }
                Ok(ElectionResult::Leading { transitions })
            }
            Err(e) if e.is_conflict() => {
                debug!(lease = %self.lease_name, steal, "replace 409 (raced)");
                Ok(ElectionResult::Conflict)
            }
            Err(e) => Err(e),
        }
    }
}

// r[verify sched.lease.k8s-lease+2]
#[cfg(test)]
mod tests {
    use super::*;

    fn obs(holder: &str, rt: Option<&str>, at: Instant) -> Option<Observed> {
        Some(Observed {
            holder: holder.to_string(),
            renew_time: rt.map(str::to_string),
            at,
        })
    }

    const TTL: Duration = Duration::from_secs(15);

    // ---- decide() table tests -------------------------------------

    /// We hold the lease → renew, regardless of observed state.
    /// The resourceVersion guard on replace() handles the case
    /// where someone stole between GET and PUT — decide() doesn't
    /// second-guess what the apiserver told us.
    #[test]
    fn we_hold_it_renews() {
        let mut o = None;
        let d = decide(
            Some("us"),
            Some("t1"),
            &mut o,
            "us",
            TTL,
            Instant::now(),
            Instant::now(),
        );
        assert_eq!(d, Decision::Renew);
        assert_eq!(o, None, "renew doesn't touch observed");
    }

    /// First time we see someone else → standby, start the clock.
    #[test]
    fn fresh_observation_is_standby() {
        let mut o = None;
        let now = Instant::now();
        let d = decide(Some("other"), Some("t1"), &mut o, "us", TTL, now, now);
        assert_eq!(d, Decision::Standby);
        assert_eq!(o, obs("other", Some("t1"), now));
    }

    /// Same holder-authored content seen again, not yet ttl elapsed →
    /// still standby, clock NOT reset (measuring time since FIRST
    /// sight).
    #[test]
    fn same_content_not_yet_stale_stays_standby() {
        let t0 = Instant::now();
        let mut o = obs("other", Some("t1"), t0);
        let d = decide(
            Some("other"),
            Some("t1"),
            &mut o,
            "us",
            TTL,
            t0 + Duration::from_secs(5),
            t0 + Duration::from_secs(5),
        );
        assert_eq!(d, Decision::Standby);
        assert_eq!(o.as_ref().unwrap().at, t0, "clock preserved");
    }

    /// Same content, ttl elapsed since first sight → steal. The
    /// renewTime VALUE is never compared to a clock — only its BYTES
    /// are compared for change against our local monotonic
    /// observation.
    #[test]
    fn same_content_stale_steals() {
        let t0 = Instant::now();
        let mut o = obs("other", Some("t1"), t0);
        let d = decide(
            Some("other"),
            Some("t1"),
            &mut o,
            "us",
            TTL,
            t0 + Duration::from_secs(20),
            t0 + Duration::from_secs(20),
        );
        assert_eq!(d, Decision::Steal);
    }

    /// Holder-authored content CHANGED (renewTime moved) → reset the
    /// clock even though we'd been watching for >ttl. The protocol
    /// wrote (renew or steal) — the holder is live.
    ///
    /// This is the case that was BROKEN when we tracked (holder,
    /// transitions): a renew bumps renewTime
    /// but NOT holder or transitions, so a standby would see a
    /// live leader as frozen and steal it after ttl. Flip-flop.
    #[test]
    fn renew_time_changed_resets_clock() {
        let t0 = Instant::now();
        let mut o = obs("other", Some("t1"), t0);
        let t1 = t0 + Duration::from_secs(20);
        let d = decide(Some("other"), Some("t2"), &mut o, "us", TTL, t1, t1);
        assert_eq!(
            d,
            Decision::Standby,
            "renewTime moved → leader alive → reset"
        );
        assert_eq!(o, obs("other", Some("t2"), t1));
    }

    /// A foreign metadata write (annotation/label patch) bumps the
    /// apiserver's resourceVersion WITHOUT touching the holder-authored
    /// spec content — the identity decide() keys on has no rv input at
    /// all, so the clock keeps aging and a dead leader is stolen on
    /// schedule (merged_bug_180's table half; the loop half drives the
    /// real composition through the mock apiserver).
    #[test]
    fn foreign_rv_churn_does_not_reset_the_clock() {
        let t0 = Instant::now();
        let mut o = obs("other", Some("t1"), t0);
        // 20s later the content is byte-identical — whatever the rv
        // did in between is invisible to the identity.
        let t1 = t0 + Duration::from_secs(20);
        let d = decide(Some("other"), Some("t1"), &mut o, "us", TTL, t1, t1);
        assert_eq!(
            d,
            Decision::Steal,
            "content-frozen for >ttl steals — rv churn is not protocol activity"
        );
    }

    /// Two-clock anchor discipline (`sched.lease.k8s-lease`): a fresh
    /// observation is STAMPED at the response instant (`confirmed_at`),
    /// not the request's send instant — the observed state is only
    /// known to have existed by the time the response arrived, and
    /// stamping earlier backdates the staleness anchor by one GET
    /// latency, letting a standby pass `age > steal_after` up to that
    /// latency BEFORE the no-write window is truly confirmed (the
    /// anti-conservative direction; the margin derivation budgets
    /// 1.5s per side, which a request-anchored stamp silently spends).
    /// Staleness is still MEASURED against the deciding read's send
    /// instant — the earliest bound on when its same-content evidence
    /// held.
    #[test]
    fn observation_stamp_anchors_at_the_response_instant() {
        let t0 = Instant::now();
        let fetch_latency = Duration::from_millis(1500);
        // First sighting: GET sent at t0, response (and rv
        // confirmation) at t0 + 1.5s. The observation must carry the
        // response instant.
        let mut o = None;
        let d = decide(
            Some("other"),
            Some("t1"),
            &mut o,
            "us",
            TTL,
            t0,
            t0 + fetch_latency,
        );
        assert_eq!(d, Decision::Standby);

        // Deciding read sent at t0 + TTL + 1s (zero latency this time):
        // measured against the RESPONSE-anchored stamp the same-rv
        // window is TTL - 0.5s — not yet stale. A request-anchored
        // stamp would claim TTL + 1s and steal 1.5s early.
        let t_decide = t0 + TTL + Duration::from_secs(1);
        let d = decide(
            Some("other"),
            Some("t1"),
            &mut o,
            "us",
            TTL,
            t_decide,
            t_decide,
        );
        assert_eq!(
            d,
            Decision::Standby,
            "the confirmed no-write window is TTL - 0.5s, short of the threshold — \
             a request-anchored stamp overstates it by the fetch latency"
        );
    }

    /// holder_identity: None (graceful step_down) → steal
    /// immediately, no observed-record wait.
    #[test]
    fn empty_holder_steals_immediately() {
        let mut o = obs("other", Some("t1"), Instant::now());
        let d = decide(
            None,
            Some("t1"),
            &mut o,
            "us",
            TTL,
            Instant::now(),
            Instant::now(),
        );
        assert_eq!(d, Decision::Steal);
        assert_eq!(o, None, "cleared — no one to observe");
    }

    /// holder_identity: Some("") — treat same as None. Be tolerant
    /// of code that clears via empty string.
    #[test]
    fn empty_string_holder_steals_immediately() {
        let mut o = None;
        let d = decide(
            Some(""),
            Some("t1"),
            &mut o,
            "us",
            TTL,
            Instant::now(),
            Instant::now(),
        );
        assert_eq!(d, Decision::Steal);
    }

    // ---- I/O shell integration tests (mock apiserver) -------------

    use k8s_openapi::serde_json::json;
    use rio_test_support::kube_mock::{ApiServerVerifier, Scenario};

    fn lease_json(holder: &str, tx: i32, rv: &str) -> String {
        json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": "rio-sched",
                "namespace": "default",
                "resourceVersion": rv,
            },
            "spec": {
                "holderIdentity": holder,
                "leaseTransitions": tx,
                "leaseDurationSeconds": 15,
            },
        })
        .to_string()
    }

    /// Renew hits 409 → `Conflict`, not `Err`. Proves
    /// [`KubeErrorExt::is_conflict`] matches what kube-rs actually
    /// returns for a failed PUT.
    ///
    /// This is the load-bearing case: our GET said we're holder,
    /// but the PUT bounced — someone stole since the GET. The
    /// caller flips `now_leading = false` immediately.
    #[tokio::test]
    async fn renew_409_is_conflict() {
        let (client, verifier) = ApiServerVerifier::new();
        let guard = verifier.run(vec![
            // GET: we hold it (holder="us"). replace() will use rv=100.
            Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_json("us", 2, "100"),
            ),
            // PUT: 409 (rv is stale — someone else updated to rv=101).
            Scenario::k8s_error(
                http::Method::PUT,
                "/leases/rio-sched",
                409,
                "Conflict",
                "the object has been modified",
            ),
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL, TTL);
        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(result, ElectionResult::Conflict);

        guard.verified().await;
    }

    /// Steal race: two standbys tried to steal simultaneously, the
    /// other one's PUT landed first (rv bumped), ours gets 409.
    /// Also `Conflict` — next tick's GET will show the winner.
    #[tokio::test]
    async fn steal_409_is_conflict() {
        let (client, verifier) = ApiServerVerifier::new();
        let guard = verifier.run(vec![
            Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_json("dead-leader", 2, "100"),
            ),
            Scenario::k8s_error(
                http::Method::PUT,
                "/leases/rio-sched",
                409,
                "Conflict",
                "the object has been modified",
            ),
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL, TTL);
        // Pre-seed observed so decide() chooses Steal (stale).
        // Without this, first observation → Standby (no PUT, test
        // hangs waiting for the PUT scenario).
        let stale = Instant::now() - Duration::from_secs(20);
        election.observed = Some(Observed {
            holder: "dead-leader".into(),
            renew_time: None,
            at: stale,
        });

        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(result, ElectionResult::Conflict);

        guard.verified().await;
    }

    /// GET 404 → POST (create). The old crate's first-run path.
    /// POST 200 → Leading immediately (we created it, we own it).
    /// The created lease has `leaseTransitions: 0` — the creator is
    /// transition zero, so its generation is the floor (1).
    #[tokio::test]
    async fn create_on_404() {
        let (client, verifier) = ApiServerVerifier::new();
        let guard = verifier.run(vec![
            Scenario::k8s_error(http::Method::GET, "/leases/rio-sched", 404, "NotFound", ""),
            Scenario::ok(http::Method::POST, "/leases", lease_json("us", 0, "1")),
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL, TTL);
        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(result, ElectionResult::Leading { transitions: 0 });

        guard.verified().await;
    }

    /// A successful steal carries the POST-bump transition count. The
    /// generation the caller derives from it is therefore distinct from
    /// the deposed holder's even if that holder never wrote anything to
    /// PG — the apiserver bumped `leaseTransitions` atomically with the
    /// holder change inside the rv-guarded PUT. This is the production
    /// half of the fix for the StaleLeaderHasStaleGeneration
    /// counterexample documented in `docs/spec/models/leaderElection.qnt`.
    // r[verify sched.lease.generation-fence+3]
    #[tokio::test]
    async fn successful_steal_carries_bumped_transitions() {
        let (client, verifier) = ApiServerVerifier::new();
        let guard = verifier.run(vec![
            // GET: dead-leader holds it at transitions=2.
            Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_json("dead-leader", 2, "100"),
            ),
            // PUT succeeds: we are now the holder at transitions=3.
            Scenario::ok(
                http::Method::PUT,
                "/leases/rio-sched",
                lease_json("us", 3, "101"),
            ),
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL, TTL);
        // Pre-seed observed so decide() chooses Steal (stale).
        let stale = Instant::now() - Duration::from_secs(20);
        election.observed = Some(Observed {
            holder: "dead-leader".into(),
            renew_time: None,
            at: stale,
        });

        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(
            result,
            ElectionResult::Leading { transitions: 3 },
            "steal of a transitions=2 lease yields transitions=3"
        );
        assert_eq!(
            election.observed, None,
            "successful steal clears the observed record"
        );

        guard.verified().await;
    }

    /// The staleness threshold `try_acquire_or_renew()` consults is
    /// `steal_after`, NOT the lease-duration `ttl` written to the Lease
    /// object. With `ttl = 15s` and `steal_after = 19s`, an observation
    /// that has been stale for 17s — past the advertised TTL but short
    /// of the steal threshold — must NOT trigger a steal. If the struct
    /// wrongly consulted `ttl`, this would PUT (and panic the mock,
    /// which has no PUT scenario queued).
    ///
    /// This is the asymmetric-TTL boundary: the deposed leader's
    /// self-fence fires at 11s, so by the time anyone reaches 19s of
    /// observed staleness the old leader has long stopped believing.
    #[tokio::test]
    async fn stale_past_ttl_but_within_steal_after_stays_standby() {
        let (client, verifier) = ApiServerVerifier::new();
        let guard = verifier.run(vec![
            // GET only — no PUT may happen.
            Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_json("other-leader", 2, "100"),
            ),
        ]);

        let mut election = LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            "us".into(),
            Duration::from_secs(15),
            Duration::from_secs(19),
        );
        // 17s: past ttl (15s), short of steal_after (19s).
        let aged = Instant::now() - Duration::from_secs(17);
        election.observed = Some(Observed {
            holder: "other-leader".into(),
            renew_time: None,
            at: aged,
        });

        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(
            result,
            ElectionResult::Standby,
            "17s stale is past ttl but within steal_after — no steal"
        );
        assert!(
            election.observed.is_some(),
            "the observation keeps aging toward steal_after"
        );

        guard.verified().await;
    }

    // ---- Interleaved CAS races (stateful mock apiserver) ----------
    //
    // These are the races the formal model explores over all
    // interleavings (`docs/spec/models/leaderElection.qnt`'s `casOk` /
    // `atMostOneCASWinner`) and the scripted `ApiServerVerifier` can
    // never express: two replicas' GET and PUT phases interleave, and
    // the apiserver's optimistic concurrency admits exactly one writer.
    // Expressible only because `try_acquire_or_renew()` is split into
    // `fetch_and_decide()` + `act()` at the model's grain.

    use rio_test_support::kube_mock::MockApiServer;

    fn election_against(client: kube::Client, holder_id: &str) -> LeaderElection {
        LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            holder_id.into(),
            Duration::from_secs(15),
            Duration::from_secs(19),
        )
    }

    /// The create race: no lease exists, both replicas GET a 404, both
    /// decide to create, exactly one POST wins.
    // r[verify sched.lease.at-most-one-leader+3]
    #[tokio::test]
    async fn interleaved_create_race_admits_one_winner() {
        let (client, mock) = MockApiServer::new();
        let mut n1 = election_against(client.clone(), "n1");
        let mut n2 = election_against(client, "n2");
        let now = Instant::now();

        // Both fetch before either acts: both see 404, both decide to
        // create.
        let o1 = n1.fetch_and_decide(now).await.expect("n1 fetch");
        let o2 = n2.fetch_and_decide(now).await.expect("n2 fetch");
        assert!(matches!(o1, FetchOutcome::Create), "n1 sees no lease");
        assert!(matches!(o2, FetchOutcome::Create), "n2 sees no lease");

        // n1's POST lands first and wins.
        let r1 = n1.act(o1).await.expect("n1 act");
        assert_eq!(r1, ElectionResult::Leading { transitions: 0 });

        // n2's POST bounces off the existing object.
        let r2 = n2.act(o2).await.expect("n2 act");
        assert_eq!(r2, ElectionResult::Conflict, "exactly one creator wins");

        assert_eq!(mock.holder().as_deref(), Some("n1"), "the winner holds it");
        assert_eq!(mock.resource_version().as_deref(), Some("1"));
    }

    /// The steal race: a lease held by a dead third party, both replicas
    /// snapshot the same resourceVersion, both decide to steal, exactly
    /// one rv-guarded PUT wins and the loser's 409 leaves it untouched.
    // r[verify sched.lease.at-most-one-leader+3]
    #[tokio::test]
    async fn interleaved_steal_race_admits_one_winner() {
        let (client, mock) = MockApiServer::new();
        mock.seed(
            k8s_openapi::serde_json::from_str(&lease_json("dead-leader", 2, "100"))
                .expect("seed lease json"),
        );

        let mut n1 = election_against(client.clone(), "n1");
        let mut n2 = election_against(client, "n2");
        // Both have watched rv 100 sit unchanged past the steal
        // threshold.
        let stale = Instant::now() - Duration::from_secs(20);
        n1.observed = Some(Observed {
            holder: "dead-leader".into(),
            renew_time: None,
            at: stale,
        });
        n2.observed = Some(Observed {
            holder: "dead-leader".into(),
            renew_time: None,
            at: stale,
        });
        let now = Instant::now();

        // Both fetch before either acts: both snapshot rv 100, both
        // decide Steal.
        let o1 = n1.fetch_and_decide(now).await.expect("n1 fetch");
        let o2 = n2.fetch_and_decide(now).await.expect("n2 fetch");
        assert!(
            matches!(
                &o1,
                FetchOutcome::Decided {
                    decision: Decision::Steal,
                    ..
                }
            ),
            "n1 decides to steal the stale lease"
        );
        assert!(
            matches!(
                &o2,
                FetchOutcome::Decided {
                    decision: Decision::Steal,
                    ..
                }
            ),
            "n2 decides to steal the stale lease"
        );

        // n1's PUT at rv 100 lands first: wins, bumps transitions 2→3,
        // bumps the rv.
        let r1 = n1.act(o1).await.expect("n1 act");
        assert_eq!(r1, ElectionResult::Leading { transitions: 3 });
        assert_eq!(n1.observed, None, "the winner clears its observed record");

        // n2's PUT still carries rv 100 — the CAS rejects it.
        let r2 = n2.act(o2).await.expect("n2 act");
        assert_eq!(r2, ElectionResult::Conflict, "exactly one thief wins");
        assert!(
            n2.observed.is_some(),
            "the loser's observed record is untouched — it keeps watching"
        );

        assert_eq!(mock.holder().as_deref(), Some("n1"), "the winner holds it");
        assert_eq!(
            mock.resource_version().as_deref(),
            Some("101"),
            "the winning PUT bumped the rv exactly once"
        );
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Verify decide_pure() against its `kani::ensures` contracts for all
    /// (holder, matched_observation_age_ms, steal_after_ms) triples. `HolderKind`,
    /// `Option<u64>`, and `u64` all impl `kani::Arbitrary`, so the harness
    /// exercises the full type domain — a strict superset of the inputs
    /// reachable from production (e.g. `Empty ∧ Some(age)` never occurs).
    /// Proving over the superset is sound: it implies the property over
    /// the reachable subset.
    ///
    /// CBMC verifies the `ensures` closures hold for every input. Since
    /// decide_pure() has no loops, no allocation, no recursion, the proof
    /// is exhaustive over the actual domain (not bounded).
    ///
    /// This is a *necessary* condition for `sched.lease.at-most-one-leader`:
    /// `decide_pure()` never returns Steal for a holder we own, never Renew
    /// for a holder we don't. The contract proves `decide_pure()` correct
    /// *given a correct projection* — the string→`HolderKind` and
    /// `Instant`→`u64` projection in `decide()` is outside the verified
    /// core and is covered by the `decide()` table tests in `mod tests`
    /// (which exercise the full `decide()` → projection → `decide_pure()`
    /// path).
    /// The *sufficient* condition (the CAS prevents dual leadership when
    /// both racers' `decide()` returns Steal) is verified by the formal
    /// model in `docs/spec/models/leaderElection.qnt`.
    ///
    /// The verification stack:
    ///   - table tests (`mod tests`)   → projection: `decide()` end-to-end
    ///   - Kani contracts (this file)  → pure decision: `decide_pure()`
    ///   - the model (`leaderElection.qnt`) → protocol: the actions that call `decide()`
    #[kani::proof_for_contract(decide_pure)]
    fn check_decide_pure_contract() {
        let holder: HolderKind = kani::any();
        let matched_observation_age_ms: Option<u64> = kani::any();
        let steal_after_ms: u64 = kani::any();
        let _ = decide_pure(holder, matched_observation_age_ms, steal_after_ms);
    }
}
