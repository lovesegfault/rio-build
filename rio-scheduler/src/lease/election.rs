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
//!    Lease's `renewTime` (written by a *different* node's clock).
//!    Instead it records `(holderIdentity, leaseTransitions)` plus
//!    a local monotonic `Instant` when that pair was first seen.
//!    If the pair doesn't change for `ttl` of *local* time, the
//!    leader stopped renewing — steal. Cross-node clock skew is
//!    irrelevant; only our own `Instant` monotonicity matters.
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
/// monotonic clock) first see this exact (holder, transitions)
/// pair? If the pair changes, the leader is alive (it renewed, or
/// a new leader took over). If it doesn't change for `ttl`, the
/// leader is dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Observed {
    holder: String,
    transitions: i32,
    at: Instant,
}

/// Pure decision function. No I/O, no clock reads — `now` is
/// injected. Separated for table testing.
///
/// Updates `observed` in place: resets `at = now` if the (holder,
/// transitions) pair changed, leaves it alone if unchanged.
///
/// The `holder == our_id` branch doesn't consult `observed` at
/// all — if the apiserver says we hold it, we renew. The
/// resourceVersion guard on the replace() catches any staleness.
pub(super) fn decide(
    spec: &LeaseSpec,
    observed: &mut Option<Observed>,
    our_id: &str,
    ttl: Duration,
    now: Instant,
) -> Decision {
    let holder = spec.holder_identity.as_deref().unwrap_or("");
    let transitions = spec.lease_transitions.unwrap_or(0);

    // Empty holder: previous leader stepped down gracefully (set
    // holder_identity: None). No one to wait for — steal now.
    if holder.is_empty() {
        *observed = None;
        return Decision::Steal;
    }

    // We hold it. Renew. Don't touch `observed` — it tracks OTHER
    // holders, not ourselves. If we lose and re-observe ourselves
    // later (restart with same holder_id), the renew branch still
    // applies; observed-record staleness is irrelevant for renew.
    if holder == our_id {
        return Decision::Renew;
    }

    // Someone else holds it. Check the observed-record clock.
    match observed {
        Some(obs) if obs.holder == holder && obs.transitions == transitions => {
            // Same pair we saw before. Has it been ttl since we
            // FIRST saw it? (Not since the lease's renewTime — we
            // don't trust that clock.)
            if now.duration_since(obs.at) > ttl {
                Decision::Steal
            } else {
                Decision::Standby
            }
        }
        _ => {
            // New pair (first observation, or holder/transitions
            // changed). Reset the clock. This is the "first-tick
            // penalty": even if the lease is actually stale, we
            // can't know that without a prior observation, so we
            // wait one full ttl. client-go does the same.
            *observed = Some(Observed {
                holder: holder.to_string(),
                transitions,
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

        // 2. Decide.
        let spec = lease.spec.clone().unwrap_or_default();
        let decision = decide(
            &spec,
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

    fn spec(holder: Option<&str>, transitions: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: holder.map(String::from),
            lease_transitions: Some(transitions),
            ..Default::default()
        }
    }

    const TTL: Duration = Duration::from_secs(15);

    // ---- decide() table tests -------------------------------------

    /// We hold the lease → renew, regardless of observed state.
    /// The resourceVersion guard on replace() handles the case
    /// where someone stole between GET and PUT — decide() doesn't
    /// second-guess what the apiserver told us.
    #[test]
    fn we_hold_it_renews() {
        let mut obs = None;
        let d = decide(&spec(Some("us"), 3), &mut obs, "us", TTL, Instant::now());
        assert_eq!(d, Decision::Renew);
        assert_eq!(obs, None, "renew doesn't touch observed");
    }

    /// First time we see someone else → standby, start the clock.
    #[test]
    fn fresh_observation_is_standby() {
        let mut obs = None;
        let now = Instant::now();
        let d = decide(&spec(Some("other"), 5), &mut obs, "us", TTL, now);
        assert_eq!(d, Decision::Standby);
        assert_eq!(
            obs,
            Some(Observed {
                holder: "other".into(),
                transitions: 5,
                at: now
            })
        );
    }

    /// Same holder seen again, not yet ttl elapsed → still standby,
    /// clock NOT reset (we're measuring time since FIRST sight).
    #[test]
    fn not_yet_stale_stays_standby() {
        let t0 = Instant::now();
        let mut obs = Some(Observed {
            holder: "other".into(),
            transitions: 5,
            at: t0,
        });
        let d = decide(
            &spec(Some("other"), 5),
            &mut obs,
            "us",
            TTL,
            t0 + Duration::from_secs(5),
        );
        assert_eq!(d, Decision::Standby);
        assert_eq!(obs.as_ref().unwrap().at, t0, "clock preserved");
    }

    /// Same holder, ttl elapsed since first sight → steal.
    /// The lease's renewTime isn't consulted — only our local
    /// monotonic observation.
    #[test]
    fn stale_observation_steals() {
        let t0 = Instant::now();
        let mut obs = Some(Observed {
            holder: "other".into(),
            transitions: 5,
            at: t0,
        });
        let d = decide(
            &spec(Some("other"), 5),
            &mut obs,
            "us",
            TTL,
            t0 + Duration::from_secs(20),
        );
        assert_eq!(d, Decision::Steal);
    }

    /// Holder CHANGED (A→B) → reset the clock even though we'd
    /// been watching A for >ttl. B is a NEW leader; their renew
    /// cadence starts fresh.
    #[test]
    fn holder_changed_resets_clock() {
        let t0 = Instant::now();
        let mut obs = Some(Observed {
            holder: "A".into(),
            transitions: 5,
            at: t0,
        });
        let t1 = t0 + Duration::from_secs(20);
        let d = decide(&spec(Some("B"), 6), &mut obs, "us", TTL, t1);
        assert_eq!(d, Decision::Standby, "new holder → fresh observation");
        let obs = obs.unwrap();
        assert_eq!(obs.holder, "B");
        assert_eq!(obs.transitions, 6);
        assert_eq!(obs.at, t1);
    }

    /// Same holder string but transitions bumped → they re-acquired
    /// (e.g., crash + restart with same pod name). Still alive —
    /// reset the clock.
    #[test]
    fn transitions_bumped_resets_clock() {
        let t0 = Instant::now();
        let mut obs = Some(Observed {
            holder: "other".into(),
            transitions: 5,
            at: t0,
        });
        let t1 = t0 + Duration::from_secs(20);
        let d = decide(&spec(Some("other"), 6), &mut obs, "us", TTL, t1);
        assert_eq!(d, Decision::Standby);
        assert_eq!(obs.as_ref().unwrap().at, t1, "tx change = alive → reset");
    }

    /// holder_identity: None (graceful step_down) → steal
    /// immediately, no observed-record wait.
    #[test]
    fn empty_holder_steals_immediately() {
        let mut obs = Some(Observed {
            holder: "other".into(),
            transitions: 5,
            at: Instant::now(),
        });
        let d = decide(&spec(None, 5), &mut obs, "us", TTL, Instant::now());
        assert_eq!(d, Decision::Steal);
        assert_eq!(obs, None, "cleared — no one to observe");
    }

    /// holder_identity: Some("") — treat same as None. The old
    /// crate wrote empty string on step_down; be tolerant.
    #[test]
    fn empty_string_holder_steals_immediately() {
        let mut obs = None;
        let d = decide(&spec(Some(""), 0), &mut obs, "us", TTL, Instant::now());
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

    fn status_409() -> String {
        json!({
            "kind": "Status", "apiVersion": "v1",
            "status": "Failure", "reason": "Conflict", "code": 409,
            "message": "the object has been modified",
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
        let task = verifier.run(vec![
            // GET: we hold it (holder="us"). replace() will use rv=100.
            Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_json("us", 2, "100"),
            ),
            // PUT: 409 (rv is stale — someone else updated to rv=101).
            Scenario {
                method: http::Method::PUT,
                path_contains: "/leases/rio-sched",
                status: 409,
                body_json: status_409(),
            },
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(result, ElectionResult::Conflict);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("verifier done")
            .expect("no panic");
    }

    /// Steal race: two standbys tried to steal simultaneously, the
    /// other one's PUT landed first (rv bumped), ours gets 409.
    /// Also `Conflict` — next tick's GET will show the winner.
    #[tokio::test]
    async fn steal_409_is_conflict() {
        let (client, verifier) = ApiServerVerifier::new();
        let task = verifier.run(vec![
            Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_json("dead-leader", 2, "100"),
            ),
            Scenario {
                method: http::Method::PUT,
                path_contains: "/leases/rio-sched",
                status: 409,
                body_json: status_409(),
            },
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
        // Pre-seed observed so decide() chooses Steal (stale).
        // Without this, first observation → Standby (no PUT, test
        // hangs waiting for the PUT scenario).
        let stale = Instant::now() - Duration::from_secs(20);
        election.observed = Some(Observed {
            holder: "dead-leader".into(),
            transitions: 2,
            at: stale,
        });

        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(result, ElectionResult::Conflict);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("verifier done")
            .expect("no panic");
    }

    /// GET 404 → POST (create). The old crate's first-run path.
    /// POST 200 → Leading immediately (we created it, we own it).
    #[tokio::test]
    async fn create_on_404() {
        let (client, verifier) = ApiServerVerifier::new();
        let task = verifier.run(vec![
            Scenario {
                method: http::Method::GET,
                path_contains: "/leases/rio-sched",
                status: 404,
                body_json: json!({
                    "kind": "Status", "apiVersion": "v1",
                    "status": "Failure", "reason": "NotFound", "code": 404,
                })
                .to_string(),
            },
            Scenario::ok(http::Method::POST, "/leases", lease_json("us", 0, "1")),
        ]);

        let mut election =
            LeaderElection::new(client, "default", "rio-sched".into(), "us".into(), TTL);
        let result = election.try_acquire_or_renew().await.expect("not Err");
        assert_eq!(result, ElectionResult::Leading);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("verifier done")
            .expect("no panic");
    }
}
