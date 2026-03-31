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
use tracing::debug;

/// Result of one `try_acquire_or_renew()` call.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ElectionResult {
    /// We hold the lease (acquired or renewed this tick).
    Leading,
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
pub(super) enum Decision {
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
pub(super) struct Observed {
    resource_version: String,
    at: Instant,
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
pub(super) fn decide(
    holder: Option<&str>,
    resource_version: &str,
    observed: &mut Option<Observed>,
    our_id: &str,
    ttl: Duration,
    now: Instant,
) -> Decision {
    let holder = holder.unwrap_or("");

    // Empty holder: previous leader stepped down gracefully (set
    // holder_identity: None). No one to wait for — steal now.
    if holder.is_empty() {
        *observed = None;
        return Decision::Steal;
    }

    // We hold it. Renew. Don't touch `observed` — it tracks OTHER
    // holders' activity, not ours. If we restart with the same
    // holder_id, this branch still applies; the rv guard on
    // replace() catches any staleness.
    if holder == our_id {
        return Decision::Renew;
    }

    // Someone else holds it. Check the observed-record clock.
    match observed {
        Some(obs) if obs.resource_version == resource_version => {
            // Same rv we saw before — nobody has written since.
            // Has it been ttl since we FIRST saw it? (Not since
            // the lease's renewTime — we never read that value,
            // only watch rv for change.)
            if now.duration_since(obs.at) > ttl {
                Decision::Steal
            } else {
                Decision::Standby
            }
        }
        _ => {
            // New rv (first observation, or the leader renewed
            // since our last look). Reset the clock. This is
            // also the "first-tick penalty": even if the lease
            // is actually stale, we can't know without a prior
            // observation, so we wait one full ttl.
            *observed = Some(Observed {
                resource_version: resource_version.to_string(),
                at: now,
            });
            Decision::Standby
        }
    }
}

pub(super) struct LeaderElection {
    api: Api<Lease>,
    lease_name: String,
    holder_id: String,
    ttl: Duration,
    observed: Option<Observed>,
}

impl LeaderElection {
    pub(super) fn new(
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
    pub(super) async fn try_acquire_or_renew(&mut self) -> Result<ElectionResult, kube::Error> {
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
    pub(super) async fn step_down(&self) -> Result<(), kube::Error> {
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
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(()),
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
                Ok(ElectionResult::Leading)
            }
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(ElectionResult::Conflict),
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
                Ok(ElectionResult::Leading)
            }
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
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

    /// Renew hits 409 → `Conflict`, not `Err`. Proves the
    /// `kube::Error::Api(ae) if ae.code == 409` pattern matches
    /// what kube-rs actually returns for a failed PUT.
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
        assert_eq!(result, ElectionResult::Leading);

        guard.verified().await;
    }
}
