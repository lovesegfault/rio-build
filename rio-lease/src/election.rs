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
//!    clock). Instead it records the lease's `resourceVersion`
//!    plus a local monotonic `Instant` when that rv was first
//!    seen. The apiserver bumps rv on every write, so a leader
//!    renewing every 5s produces a new rv every 5s. If rv doesn't
//!    change for `ttl` of *local* time, nobody wrote — steal.
//!    Cross-node clock skew is irrelevant; only our own `Instant`
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
    /// Someone else mutated the lease between our GET and PUT.
    ///
    /// On a **renew** this means we lost leadership — someone stole
    /// the lease since our GET. Unambiguous lose transition.
    ///
    /// On a **steal** this means another standby raced us and won.
    /// We were never leading; next tick's GET reveals the winner.
    ///
    /// Caller treats both as `now_leading = false`. The lose vs
    /// never-led distinction is handled by `was_leading` edge
    /// detection in the loop.
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

/// client-go's "observed record": when did WE (this process, our
/// monotonic clock) first see this `resourceVersion`? The
/// apiserver bumps rv on every write (including renew), so a
/// live leader produces a fresh rv every RENEW_INTERVAL. If rv
/// doesn't change for `ttl`, nobody is writing — the holder is
/// dead.
///
/// Tracking rv (not `(holder, transitions)`) is load-bearing:
/// renew only touches `renew_time`, leaving holder/transitions
/// unchanged — a standby watching only those would see a live
/// leader as frozen and steal it after ttl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    resource_version: String,
    at: Instant,
}

/// Projection of `(holder, our_id)` for the pure decision predicate.
/// `decide()` computes this from production data; `decide_pure()` and
/// the Kani harness operate on it directly. Collapsing the string
/// comparison into a small enum keeps `decide_pure()` CBMC-tractable —
/// CBMC can't symbolically execute over `&str` arguments.
///
/// The variants map onto `decide()`'s case structure and the TLA+
/// model's action partition (`docs/spec/models/LeaderElection.tla`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub(crate) enum HolderKind {
    /// `holder` is `None` or `Some("")` — graceful step-down. Steal now.
    /// TLA+: a node with `snap[n].holder = NULL` takes `Steal(n)`.
    Empty,
    /// `holder == our_id` — we hold it. Renew.
    /// TLA+: a node with `snap[n].holder = n` takes `Renew(n)`.
    Us,
    /// `holder` is a non-empty string that isn't us. Standby or steal
    /// based on the observed-record clock.
    /// TLA+: a node with `snap[n].holder /= n /\ snap[n].holder /= NULL`
    /// takes `Discard(n)` (spike scope: standby) or, in Phase-1, `Steal(n)`
    /// when the observed-record clock expires.
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
    /// Reset the clock — the lease's resourceVersion changed (someone
    /// wrote) so the holder is alive. Re-observe at the new rv.
    StartObserving,
}

/// Pure decision predicate. No I/O, no `Instant`, no `String` — every
/// argument and return type is CBMC-tractable. `decide()` projects
/// production data into these types, calls this, and applies the
/// returned `ObservedUpdate`.
///
/// The case structure here is the formal contract: it parallels the
/// action disjunction in `Next` in `docs/spec/models/LeaderElection.tla`
/// (`Get(n) \/ Steal(n) \/ Renew(n) \/ Observe(n) \/ Discard(n)`). When
/// either changes, update the other.
///
/// `matched_observation_age_ms` collapses two production cases into
/// one: it's `Some(age)` only when there IS an observation AND its rv
/// matches the current lease rv. `None` covers both "no observation"
/// and "rv changed" — both reset the clock (`StartObserving`).
// r[impl sched.lease.k8s-lease]
//
// ── Kani contracts ───────────────────────────────────────────────────
// Each `ensures` clause is one direction of an iff. The contract
// closures restate decide_pure's spec from a different angle than the
// implementation: instead of `if holder is X then return Y`, they say
// `return is Y iff holder is X`. A bug that flips a comparison
// (`>` to `>=`) or drops a case fails the contract.
//
// The case structure parallels the {Steal, Renew, Observe, Discard}
// action partition under `Next` in docs/spec/models/LeaderElection.tla.
// When either changes, update the other — `tracey bump` on the spec
// rule will flag both.
//
// Verified by check_decide_pure_contract in #[cfg(kani)] mod kani_proofs.
#[cfg_attr(kani, kani::ensures(|r: &(Decision, ObservedUpdate)| {
    // Steal iff holder is empty, OR (holder is Other AND the observed
    // rv matched AND has been stale for > ttl).
    (r.0 == Decision::Steal) == (
        matches!(holder, HolderKind::Empty)
        || (matches!(holder, HolderKind::Other)
            && matched_observation_age_ms.is_some_and(|age| age > ttl_ms))
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
    ttl_ms: u64,
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
            // Same rv we saw before — nobody has written since. It's
            // been > ttl since we FIRST saw it (not since the lease's
            // renewTime — we never read that value, only watch rv for
            // change). The holder is dead. Steal.
            //
            // Strict `>` (not `>=`) is an implementation choice: steal
            // only when *strictly* past the TTL. `sched.lease.k8s-lease`
            // is silent on the boundary case; the Kani contract above
            // codifies this choice (the mutation `>` → `>=` fails the
            // contract — that demonstrates non-tautology, not that the
            // spec mandates `>`).
            Some(age_ms) if age_ms > ttl_ms => (Decision::Steal, ObservedUpdate::Keep),

            // Same rv, but not yet stale. Wait.
            Some(_) => (Decision::Standby, ObservedUpdate::Keep),

            // New rv (first observation, or the leader renewed since
            // our last look). Reset the clock. This is also the
            // "first-tick penalty": even if the lease is actually
            // stale, we can't know without a prior observation, so we
            // wait one full ttl.
            None => (Decision::Standby, ObservedUpdate::StartObserving),
        },
    }
}

/// Pure decision function. No I/O, no clock reads — `now` is
/// injected. Separated for table testing.
///
/// Updates `observed` in place: resets `at = now` if the
/// resourceVersion changed since the last call, leaves it alone
/// if unchanged.
///
/// `holder` and `resource_version` are passed separately rather
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
    resource_version: &str,
    observed: &mut Option<Observed>,
    our_id: &str,
    ttl: Duration,
    now: Instant,
) -> Decision {
    // ── Project production data → predicate inputs ──────────────────
    // The projection is the only place `Instant` and `String` appear.
    // `decide_pure()` is the verified core; this is the I/O-shaped shim.
    let holder_kind = match holder {
        None => HolderKind::Empty,
        Some("") => HolderKind::Empty,
        Some(s) if s == our_id => HolderKind::Us,
        Some(_) => HolderKind::Other,
    };
    // `Some(age)` iff there's an observation whose rv matches the
    // current lease rv. Collapses "no observation" and "rv changed" —
    // both go to `StartObserving` in decide_pure().
    let matched_observation_age_ms = observed
        .as_ref()
        .filter(|o| o.resource_version == resource_version)
        .map(|o| now.duration_since(o.at).as_millis() as u64);
    let ttl_ms = ttl.as_millis() as u64;

    let (decision, update) = decide_pure(holder_kind, matched_observation_age_ms, ttl_ms);

    // ── Apply the observed-record update ────────────────────────────
    match update {
        ObservedUpdate::Keep => {}
        ObservedUpdate::Clear => *observed = None,
        ObservedUpdate::StartObserving => {
            *observed = Some(Observed {
                resource_version: resource_version.to_string(),
                at: now,
            });
        }
    }

    decision
}

pub struct LeaderElection {
    api: Api<Lease>,
    lease_name: String,
    holder_id: String,
    ttl: Duration,
    observed: Option<Observed>,
}

impl LeaderElection {
    pub fn new(
        client: kube::Client,
        namespace: &str,
        lease_name: String,
        holder_id: String,
        ttl: Duration,
    ) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
            lease_name,
            holder_id,
            ttl,
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
    pub async fn try_acquire_or_renew(&mut self) -> Result<ElectionResult, kube::Error> {
        // 1. GET. 404 → create and done.
        let lease = match self.api.get_opt(&self.lease_name).await? {
            Some(l) => l,
            None => return self.create().await,
        };

        // 2. Decide. resource_version is always set on objects
        // that came from the apiserver (unwrap_or_default is
        // defensive, not expected).
        let holder = lease
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref());
        let rv = lease
            .metadata
            .resource_version
            .as_deref()
            .unwrap_or_default();
        let decision = decide(
            holder,
            rv,
            &mut self.observed,
            &self.holder_id,
            self.ttl,
            Instant::now(),
        );

        // 3. Act.
        match decision {
            Decision::Standby => Ok(ElectionResult::Standby),
            Decision::Renew => self.replace(lease, false).await,
            Decision::Steal => self.replace(lease, true).await,
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
    /// Modeled as `Replace(n)` in `docs/spec/models/LeaderElection.tla`.
    /// The rv-guarded CAS here is what keeps `AtMostOneLeader` during the
    /// initial-acquisition race — the model checks it over all
    /// interleavings of N replicas, which neither the table tests nor
    /// the Kani contract on `decide_pure()` reach (both are
    /// single-replica).
    // r[impl sched.lease.at-most-one-leader+2]
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

// r[verify sched.lease.k8s-lease]
#[cfg(test)]
mod tests {
    use super::*;

    fn obs(rv: &str, at: Instant) -> Option<Observed> {
        Some(Observed {
            resource_version: rv.to_string(),
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
        let d = decide(Some("us"), "42", &mut o, "us", TTL, Instant::now());
        assert_eq!(d, Decision::Renew);
        assert_eq!(o, None, "renew doesn't touch observed");
    }

    /// First time we see someone else → standby, start the clock.
    #[test]
    fn fresh_observation_is_standby() {
        let mut o = None;
        let now = Instant::now();
        let d = decide(Some("other"), "42", &mut o, "us", TTL, now);
        assert_eq!(d, Decision::Standby);
        assert_eq!(o, obs("42", now));
    }

    /// Same rv seen again, not yet ttl elapsed → still standby,
    /// clock NOT reset (measuring time since FIRST sight).
    #[test]
    fn same_rv_not_yet_stale_stays_standby() {
        let t0 = Instant::now();
        let mut o = obs("42", t0);
        let d = decide(
            Some("other"),
            "42",
            &mut o,
            "us",
            TTL,
            t0 + Duration::from_secs(5),
        );
        assert_eq!(d, Decision::Standby);
        assert_eq!(o.as_ref().unwrap().at, t0, "clock preserved");
    }

    /// Same rv, ttl elapsed since first sight → steal. The
    /// lease's renewTime isn't consulted — only our local
    /// monotonic observation of whether rv MOVED.
    #[test]
    fn same_rv_stale_steals() {
        let t0 = Instant::now();
        let mut o = obs("42", t0);
        let d = decide(
            Some("other"),
            "42",
            &mut o,
            "us",
            TTL,
            t0 + Duration::from_secs(20),
        );
        assert_eq!(d, Decision::Steal);
    }

    /// rv CHANGED → reset the clock even though we'd been watching
    /// for >ttl. Something wrote (renew or steal) — holder is live.
    ///
    /// This is the case that was BROKEN when we tracked (holder,
    /// transitions): a renew bumps renewTime and resourceVersion
    /// but NOT holder or transitions, so a standby would see a
    /// live leader as frozen and steal it after ttl. Flip-flop.
    #[test]
    fn rv_changed_resets_clock() {
        let t0 = Instant::now();
        let mut o = obs("42", t0);
        let t1 = t0 + Duration::from_secs(20);
        let d = decide(Some("other"), "43", &mut o, "us", TTL, t1);
        assert_eq!(d, Decision::Standby, "rv moved → leader alive → reset");
        assert_eq!(o, obs("43", t1));
    }

    /// holder_identity: None (graceful step_down) → steal
    /// immediately, no observed-record wait.
    #[test]
    fn empty_holder_steals_immediately() {
        let mut o = obs("42", Instant::now());
        let d = decide(None, "42", &mut o, "us", TTL, Instant::now());
        assert_eq!(d, Decision::Steal);
        assert_eq!(o, None, "cleared — no one to observe");
    }

    /// holder_identity: Some("") — treat same as None. Be tolerant
    /// of code that clears via empty string.
    #[test]
    fn empty_string_holder_steals_immediately() {
        let mut o = None;
        let d = decide(Some(""), "42", &mut o, "us", TTL, Instant::now());
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
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
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
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
        // Pre-seed observed so decide() chooses Steal (stale).
        // Without this, first observation → Standby (no PUT, test
        // hangs waiting for the PUT scenario).
        let stale = Instant::now() - Duration::from_secs(20);
        election.observed = Some(Observed {
            resource_version: "100".into(),
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
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
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
    /// counterexample in `docs/spec/models/LeaderElection.tla`.
    // r[verify sched.lease.generation-fence]
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
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
        // Pre-seed observed so decide() chooses Steal (stale).
        let stale = Instant::now() - Duration::from_secs(20);
        election.observed = Some(Observed {
            resource_version: "100".into(),
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
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Verify decide_pure() against its `kani::ensures` contracts for all
    /// (holder, matched_observation_age_ms, ttl_ms) triples. `HolderKind`,
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
    /// both racers' `decide()` returns Steal) is verified by the TLA+ model
    /// in `docs/spec/models/LeaderElection.tla`.
    ///
    /// The verification stack:
    ///   - table tests (`mod tests`)   → projection: `decide()` end-to-end
    ///   - Kani contracts (this file)  → pure decision: `decide_pure()`
    ///   - TLA+ (`LeaderElection.tla`) → protocol: the actions that call `decide()`
    #[kani::proof_for_contract(decide_pure)]
    fn check_decide_pure_contract() {
        let holder: HolderKind = kani::any();
        let matched_observation_age_ms: Option<u64> = kani::any();
        let ttl_ms: u64 = kani::any();
        let _ = decide_pure(holder, matched_observation_age_ms, ttl_ms);
    }
}
