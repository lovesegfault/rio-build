//! D4: the lease generation, CONSUMED (WO-S3-3, C4 repriced by Q5).
//!
//! Pre-round-9 the controller's generation had no readers — "its
//! lease is pure mutual exclusion". The lease premise that mutual
//! exclusion rides (fence-check latency ≤ one 5s tick) is exactly
//! what an admitted-load stall violates (live_054: 2.5–3×), and a
//! stall past STEAL_AFTER re-opens DUAL ACTORS on the lease-gated
//! loop with nothing downstream able to tell the actors apart. Q5
//! repricing (signed): this is NOT an HA enabler — the singleton
//! stays a singleton; the token closes the RESTART-OVERLAP and
//! STALL dual-actor windows.
//!
//! Two carriers, both consuming [`rio_lease::LeaderState`] (which the
//! guard-domain lease loop now keeps truthful through main-domain
//! stalls — the WO-S3-1 composition):
//!
//! 1. **Mutation seam ([`MutationFence`])** — minted per reconcile
//!    pass; the THREE NodeClaim mutation writers (create in
//!    nodeclaim_pool/mod.rs, the unhealthy reap delete in health.rs,
//!    the idle consolidate delete in consolidate.rs — a two-site pin
//!    would leave an unstamped shadow mutation path) check it
//!    immediately before the apiserver call and REFUSE when the pass
//!    generation is no longer the live one. The create site also
//!    stamps the generation as an object label
//!    (`rio.build/controller-generation`) for forensics and future
//!    apiserver-side consumers.
//! 2. **Evidence-Ack seam ([`GenerationStamp`])** — every AdminService
//!    RPC carries `x-rio-controller-generation` request metadata (the
//!    shared [`rio_proto::interceptor::CONTROLLER_GENERATION_KEY`]
//!    contract); the scheduler's `AckSpawnedIntents` keeps a monotonic
//!    watermark and refuses anything below it, so a deposed actor's
//!    late ack cannot land after the live generation has spoken.
//!
//! Residual (named, RULED — trigger: the next dual-actor incident
//! class): other evidence RPCs (`ReportAttemptOutcome`,
//! `AppendInterruptSample`) CARRY the stamp through the same
//! interceptor but are not yet validated scheduler-side; the granted
//! consumer this round is the Ack plane.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::warn;

/// A reconcile pass's claim on the lease generation. Minted at pass
/// start from the live [`rio_lease::LeaderState`]; checked at every
/// mutation seam. The check consults the SAME LeaderState, so a lease
/// loss or steal observed by the (guard-domain) lease loop between
/// mint and mutation flips the verdict.
// r[impl sys.guard.scheduling-premise]
pub struct MutationFence {
    generation: u64,
    leader: rio_lease::LeaderState,
}

impl MutationFence {
    /// Capture the pass generation. Call once per reconcile pass,
    /// after the leadership gate.
    pub fn mint(leader: &rio_lease::LeaderState) -> Self {
        Self {
            generation: leader.generation(),
            leader: leader.clone(),
        }
    }

    /// The generation this pass acts under (the create-site label
    /// value and any future wire carriers).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The seam check: is this pass's generation still the live one,
    /// held by a current leader? On refusal: one WARN + the
    /// `rio_controller_fenced_mutations_refused_total{surface}`
    /// counter, at THIS chokepoint (every refusal looks the same).
    pub fn check(&self, surface: &'static str) -> Result<(), StaleGeneration> {
        let live = self.leader.generation();
        if self.leader.is_leader() && live == self.generation {
            return Ok(());
        }
        warn!(
            surface,
            pass_generation = self.generation,
            live_generation = live,
            is_leader = self.leader.is_leader(),
            "mutation refused: pass generation is no longer the live lease \
             generation (dual-actor window closed at the seam)"
        );
        metrics::counter!(
            "rio_controller_fenced_mutations_refused_total",
            "surface" => surface,
        )
        .increment(1);
        Err(StaleGeneration {
            pass: self.generation,
            live,
        })
    }
}

/// The typed refusal: the pass acted under `pass`, the lease says
/// `live` (or leadership is gone entirely).
#[derive(Debug)]
pub struct StaleGeneration {
    pub pass: u64,
    pub live: u64,
}

/// NodeClaim object label carrying the creating generation (the
/// create-site object carrier — forensics + future consumers).
pub const GENERATION_LABEL: &str = "rio.build/controller-generation";

/// tonic interceptor stamping the live lease generation onto every
/// controller→scheduler AdminService request, layered over the
/// service-token interceptor (rio-auth's type is wrapped, not
/// edited). The generation Arc is the SAME one
/// [`rio_lease::LeaderState`] mutates, so the header always carries
/// the freshest acquire/rebound-derived value with no lock.
#[derive(Clone)]
pub struct GenerationStamp {
    inner: rio_auth::hmac::ServiceTokenInterceptor,
    generation: Arc<AtomicU64>,
}

impl GenerationStamp {
    pub fn new(inner: rio_auth::hmac::ServiceTokenInterceptor, generation: Arc<AtomicU64>) -> Self {
        Self { inner, generation }
    }
}

impl tonic::service::Interceptor for GenerationStamp {
    fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let mut req = self.inner.call(req)?;
        let generation = self.generation.load(Ordering::SeqCst);
        // u64 decimal is always a valid ASCII metadata value.
        if let Ok(v) = generation.to_string().parse() {
            req.metadata_mut()
                .insert(rio_proto::interceptor::CONTROLLER_GENERATION_KEY, v);
        }
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W9-AM (controller half): the dual-actor window driven with two
    /// LeaderStates over the REAL lease state machinery (production
    /// constructors — R13): actor A's fence goes stale the moment the
    /// lease moves; actor B's stays live. Both directions.
    // r[verify sys.guard.scheduling-premise]
    #[test]
    fn w9_am_stale_fence_refuses_live_fence_proceeds() {
        // Actor A: leader after acquire at lease-transitions 5
        // (generation = transitions+1 = 6, exactly the lease loop's
        // derivation).
        let gen_a = Arc::new(AtomicU64::new(1));
        let a = rio_lease::LeaderState::pending(Arc::clone(&gen_a));
        a.on_acquire(5);
        let fence_a = MutationFence::mint(&a);
        assert!(fence_a.check("test-create").is_ok(), "live fence must pass");

        // The lease moves: A's (guard-domain) lease loop observes the
        // loss; B acquires with the bumped transition count.
        a.on_lose();
        let gen_b = Arc::new(AtomicU64::new(1));
        let b = rio_lease::LeaderState::pending(Arc::clone(&gen_b));
        b.on_acquire(6);

        // A's outstanding pass is now a deposed actor: REFUSED.
        let err = fence_a
            .check("test-create")
            .expect_err("stale fence must refuse");
        assert_eq!(err.pass, 6);

        // B's fresh pass proceeds.
        let fence_b = MutationFence::mint(&b);
        assert!(fence_b.check("test-create").is_ok());
        assert_eq!(fence_b.generation(), 7);
    }

    /// The rebound edge (holder change inside the observation gap)
    /// also stales an outstanding fence: generation moves without a
    /// lose edge.
    #[test]
    fn rebound_stales_an_outstanding_fence() {
        let generation = Arc::new(AtomicU64::new(1));
        let state = rio_lease::LeaderState::pending(Arc::clone(&generation));
        state.on_acquire(7);
        let fence = MutationFence::mint(&state);
        assert!(fence.check("test-delete").is_ok());
        state.on_rebound(9);
        assert!(
            fence.check("test-delete").is_err(),
            "a rebound-bumped generation must stale the pass fence"
        );
        // A fence minted AFTER the rebound is live again.
        assert!(MutationFence::mint(&state).check("test-delete").is_ok());
    }

    /// The interceptor stamps the CURRENT generation per call (not the
    /// mint-time one) — the header tracks acquire/rebound updates.
    #[test]
    fn generation_stamp_tracks_the_live_atomic() {
        use tonic::service::Interceptor as _;
        let generation = Arc::new(AtomicU64::new(3));
        let inner = rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller");
        let mut stamp = GenerationStamp::new(inner, Arc::clone(&generation));
        let req = stamp.call(tonic::Request::new(())).expect("stamp");
        let v = req
            .metadata()
            .get(rio_proto::interceptor::CONTROLLER_GENERATION_KEY)
            .expect("header present");
        assert_eq!(v.to_str().unwrap(), "3");
        generation.store(8, Ordering::SeqCst);
        let req = stamp.call(tonic::Request::new(())).expect("stamp");
        assert_eq!(
            req.metadata()
                .get(rio_proto::interceptor::CONTROLLER_GENERATION_KEY)
                .unwrap()
                .to_str()
                .unwrap(),
            "8"
        );
    }
}
