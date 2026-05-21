//! Model-based testing: replay traces generated from the leader-election
//! Quint model (`docs/spec/models/leaderElection.qnt`) against the real
//! election machinery, diffing the implementation's projected state
//! against the model's after every step.
//!
//! This is layer 4 of the verification stack (VM tests > **MBT** > the
//! Quint model > Kani > unit tests): the model proves the protocol is
//! correct; MBT proves the code is that protocol. Without it the model's
//! correspondence to the code is a hand-maintained convention that
//! decays — every one of the model-fidelity bugs found during the
//! formal-verification campaign was a case of "the model and the code
//! disagree about what happens after action A in state S", which is
//! exactly the per-step diff this module mechanizes.
//!
//! # Architecture
//!
//! [`MbtSystem`] owns a [`MockApiServer`] (the stateful in-memory Lease
//! store with real optimistic-concurrency semantics) and one
//! [`NodeHarness`] per model node, each holding a real [`LeaderElection`]
//! + [`LeaderState`] pair. The [`Driver`] impl maps each named model
//! action onto implementation calls; the [`State`] impl projects the
//! implementation's observable state into the model's variable shape.
//! quint-connect (the `#[quint_run]` simulation) and the hand-rolled
//! named-run replay both drive the same [`MbtSystem`] and diff the same
//! [`Projection`].
//!
//! # The model↔implementation time mapping
//!
//! The model's clocks are integer ticks; the implementation's thresholds
//! are wall-clock `Duration`s. One model tick = [`TICK`] = 7s, chosen so
//! that the model's symmetric base-regime deadline (`FENCE_AFTER =
//! STEAL_AFTER = 3` ticks, i.e. an action enabled once 4 ticks have
//! elapsed) lands strictly past BOTH implementation thresholds:
//!
//! ```text
//!   3 ticks = 21s > STEAL_AFTER (19s) > SELF_FENCE_AFTER (11s)
//! ```
//!
//! The `3 ticks > 19s` direction (not just `4 ticks > 19s`) matters
//! because the implementation evaluates the steal threshold inside
//! `decide()` at GET time while the model evaluates it at PUT time, and
//! the two can be one tick apart (see the PUT-arm comment). There is no
//! upper constraint: the driver only invokes a threshold check at the
//! instants the trace chooses, and the trace only chooses instants where
//! the model's (coarser) threshold has passed.
//!
//! # Findings from building this driver (each verified empirically)
//!
//! 1. **`quint test` cannot drive quint-connect's `#[quint_test]`.**
//!    quint-connect's default config reads the action name from the
//!    `mbt::actionTaken` trace variable, which only `quint run --mbt`
//!    emits — `quint test` (the named-run runner) does not accept
//!    `--mbt` at all. The named runs are
//!    therefore replayed by hand: `quint test --out-itf` still emits the
//!    per-step *states*, and [`replay_named_run`] zips them with the
//!    run's action sequence (mirrored from the model) and diffs after
//!    every step. A model-side edit that changes any projected variable
//!    at any step makes the replay diverge there, and the trace-length
//!    check catches insertions and deletions; only edits invisible to
//!    the projection (e.g. reordering ticks within the skew constraint)
//!    escape, which is acceptable because tick touches none of
//!    lease/leading/gen.
//! 2. **The model's initial state is "a Lease exists with no holder at
//!    rv 0", not "no Lease object".** The driver's `init` seeds the mock
//!    store accordingly. The implementation's 404→POST create path is
//!    therefore *outside the modeled state space* (the model's first
//!    acquisition is a steal of the born-empty lease, which bumps the
//!    transition count to 1 and derives generation 2; a real first
//!    deployment creates the lease at transition count 0 and derives
//!    generation 1). The create path stays covered by the unit tests
//!    (`create_on_404`, `interleaved_create_race_admits_one_winner`);
//!    closing the gap requires the model to grow a distinct "no lease"
//!    state, which changes the verified state space and is deferred.
//! 3. **The model checks the steal threshold at PUT time; the
//!    implementation checks it at GET time.** Production's GET and PUT
//!    are microseconds apart so the distinction is invisible there, but
//!    the model elides the stuttering re-GETs a standby performs every
//!    renew interval, so a model trace can hold a snapshot across the
//!    threshold crossing. The driver's PUT arms re-evaluate `decide()`
//!    against the *stashed* snapshot at the PUT step's clock — the
//!    production round the model's PUT action abstracts is "the GET
//!    whose decide() returned Steal, immediately followed by its PUT".
//!    The CAS token (the snapshot's resourceVersion) is NOT refreshed;
//!    that is the part that must stay stale for the conflict path to be
//!    reachable.
//! 4. **`maybe_self_fence` measures `last_successful_renew.elapsed()`
//!    against the real clock**, so the driver hands it a synthetic
//!    *past* instant computed from the tick delta at call time rather
//!    than a future instant derived from `base + ticks×TICK`.
//!
//! # Determinism policy
//!
//! The simulation test pins its seed in the `#[quint_run]` attribute (an
//! input, not a measurement). Unseeded exploration is a local activity:
//! delete the `seed` parameter, run until a divergence appears, then add
//! the offending seed to the pinned set. The named-run replays are fully
//! deterministic (no nondeterminism in a `run` definition).
//!
//! All tests are `#[ignore]`d: they shell out to `quint`, which the
//! default `nextest-rio-lease` sandbox does not provide. The dedicated
//! MBT check runs them with `--run-ignored`. Locally:
//!
//! ```text
//! cargo nextest run -p rio-lease -E 'test(/mbt_/)' --run-ignored all
//! ```

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use quint_connect::{Driver, Result, State, Step, switch};
use rio_test_support::kube_mock::MockApiServer;
use serde::Deserialize;

use crate::election::{ElectionResult, FetchOutcome, LeaderElection};
use crate::{LEASE_TTL, LeaderState, STEAL_AFTER, decide, maybe_self_fence};

/// One model tick, in implementation time. See the module header for the
/// derivation; the `const` assertions pin the constraint so neither the
/// implementation thresholds nor this mapping can move without the other.
const TICK: Duration = Duration::from_secs(7);

/// The model's `FENCE_AFTER` / `STEAL_AFTER` in the base regime
/// (`leaderElectionBase` imports the core module with both at 3).
const MODEL_DEADLINE_TICKS: u32 = 3;

const _: () = {
    // A deadline the model considers expired (> MODEL_DEADLINE_TICKS
    // ticks elapsed) must also be expired under the implementation's
    // thresholds — including when the implementation evaluated it one
    // tick earlier, at GET time (finding 3 in the module header).
    assert!(
        MODEL_DEADLINE_TICKS as u64 * TICK.as_secs() > crate::STEAL_AFTER.as_secs(),
        "3 model ticks must exceed the implementation's steal threshold"
    );
    assert!(
        MODEL_DEADLINE_TICKS as u64 * TICK.as_secs() > crate::SELF_FENCE_AFTER.as_secs(),
        "3 model ticks must exceed the implementation's self-fence threshold"
    );
};

/// The model's node set (`NODES = Set("n1", "n2")` in
/// `leaderElectionBase`). The holder identities the harnesses register
/// with the mock apiserver are the model's node names, so the projected
/// `lease.holder` compares directly against the model's `Held(n)`.
const NODES: [&str; 2] = ["n1", "n2"];

/// The spec path for the hand-rolled named-run replay (absolute via the
/// manifest dir, so it resolves regardless of the test CWD).
const SPEC_ABS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/spec/models/leaderElection.qnt"
);

// =======================================================================
// The projection (the abstraction function)
// =======================================================================

/// The subset of the model's state the implementation observably
/// realizes. Field names are the model's fully-qualified variable names
/// (quint namespaces them as `<instance module>::<core module>::<var>`
/// in ITF traces); fields absent from this struct are not compared.
///
/// Omitted, and why:
/// - `clocks`, `alive`, `fence`, `acquiredAt`, `casRace`, `delVictims`,
///   `deletes`, `claimFailures`, `restores`: model-only history
///   bookkeeping (they exist to express the invariants) or pure driver
///   bookkeeping (diffing the driver's tick counter against the model's
///   clock proves nothing about the implementation).
/// - `genHW`: lives in rio-scheduler's claims ledger — phase 2. Sound to
///   omit in the base regime because the lease-derived and PG-derived
///   epoch sources stay in lockstep when no fault separates them (the
///   design doc's lockstep argument).
/// - `snap`, `obs`: projectable (the driver's stash and the election's
///   observed record) but deferred until the core projection is proven
///   stable — every added field is another way to get the projection
///   itself wrong.
#[derive(Debug, PartialEq, Deserialize)]
struct Projection {
    #[serde(rename = "leaderElectionBase::leaderElection::lease")]
    lease: ModelLease,
    #[serde(rename = "leaderElectionBase::leaderElection::leading")]
    leading: BTreeMap<String, bool>,
    #[serde(rename = "leaderElectionBase::leaderElection::gen")]
    r#gen: BTreeMap<String, u64>,
}

/// The model's `LeaseRec`: `{ holder: Holder, rv: int, gen: int }`.
/// `gen` is the Lease object's `leaseTransitions` count (the model named
/// it `gen` because the node generation derives from it), NOT the node's
/// in-memory generation.
#[derive(Debug, PartialEq, Deserialize)]
struct ModelLease {
    holder: ModelHolder,
    rv: u64,
    r#gen: u64,
}

/// The model's `Holder` sum type. Quint sum types serialize as
/// `{ "tag": <variant>, "value": <payload> }` in ITF traces.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelHolder {
    NoHolder,
    Held(String),
}

// =======================================================================
// The system under test
// =======================================================================

/// One model node's implementation half: the real election + state
/// machinery plus the loop-local bookkeeping `run_lease_loop` would
/// keep (`was_leading`, `last_successful_renew`) and the model-time
/// bookkeeping the driver needs (`ticks`, `fence_tick`).
struct NodeHarness {
    election: LeaderElection,
    state: LeaderState,
    /// The model's `snap[n]`: the outcome of the last `apiGet(n)` not
    /// yet consumed by a PUT action. `None` ⇔ the model's `NoSnap`.
    stash: Option<FetchOutcome>,
    /// The model's `clocks[n]`.
    ticks: u64,
    /// The model's `fence[n]`: the tick of the last completed write
    /// round-trip. Feeds `maybe_self_fence`'s `last_successful_renew`.
    fence_tick: u64,
    /// `run_lease_loop`'s edge-detection state. Mirrors
    /// `state.is_leader()` exactly (as it does in the production loop);
    /// kept separate because `maybe_self_fence` takes `&mut bool`.
    was_leading: bool,
}

/// The whole replicated system: the shared mock apiserver plus one
/// harness per model node.
struct MbtSystem {
    mock: MockApiServer,
    client: kube::Client,
    nodes: BTreeMap<String, NodeHarness>,
    /// The instant corresponding to model tick 0. Synthetic "now" values
    /// are `base + ticks×TICK`; `Instant` arithmetic between two such
    /// values is exact, so `decide()`'s observation ages come out as
    /// exact multiples of [`TICK`].
    base: Instant,
}

impl MbtSystem {
    /// The model's `init` action: a born-empty Lease at resourceVersion
    /// 0 with transition count 0 (finding 2 in the module header), two
    /// fresh follower harnesses at generation 1.
    fn init() -> Self {
        let (client, mock) = MockApiServer::new();
        mock.seed(serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": "rio-mbt",
                "namespace": "default",
                "resourceVersion": "0",
            },
            "spec": {
                "holderIdentity": null,
                "leaseTransitions": 0,
                "leaseDurationSeconds": LEASE_TTL.as_secs(),
            },
        }));
        let base = Instant::now();
        let nodes = NODES
            .iter()
            .map(|n| (n.to_string(), Self::fresh_harness(&client, n)))
            .collect();
        Self {
            mock,
            client,
            nodes,
            base,
        }
    }

    /// A node harness in the model's post-`init` / post-`crash` state:
    /// not leading, generation 1 (the production `AtomicU64::new(1)`
    /// floor), no observation, no snapshot, fence anchored at 0.
    fn fresh_harness(client: &kube::Client, node: &str) -> NodeHarness {
        NodeHarness {
            election: LeaderElection::new(
                client.clone(),
                "default",
                "rio-mbt".into(),
                node.into(),
                LEASE_TTL,
                STEAL_AFTER,
            ),
            state: LeaderState::pending(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1))),
            stash: None,
            ticks: 0,
            fence_tick: 0,
            was_leading: false,
        }
    }

    fn node(&mut self, n: &str) -> &mut NodeHarness {
        self.nodes
            .get_mut(n)
            .unwrap_or_else(|| panic!("trace references node {n:?}, not in NODES {NODES:?}"))
    }

    /// `tick(n)`: advance the node's clock. Pure bookkeeping — the new
    /// time flows into the next component call.
    fn tick(&mut self, n: &str) {
        self.node(n).ticks += 1;
    }

    /// `apiGet(n)`: the GET + decide half of an election round. The
    /// outcome is stashed (the model's `snap[n]`) for a later PUT action
    /// to consume; `decide()`'s observed-record update happens here as a
    /// side effect, exactly as it does inside the production round.
    async fn api_get(&mut self, n: &str) -> Result {
        let base = self.base;
        let h = self.node(n);
        let now = base + TICK * u32::try_from(h.ticks).expect("tick counter fits u32");
        let outcome = h
            .election
            .fetch_and_decide(now)
            .await
            .context("apiGet: fetch_and_decide against the mock apiserver")?;
        h.stash = Some(outcome);
        Ok(())
    }

    /// `steal(n)` / `renewLease(n)` / `conflict(n)`: the PUT half of an
    /// election round. The model names the three *outcomes* as three
    /// actions; the implementation has one `act()` whose result the
    /// post-step state diff distinguishes — a wrong outcome shows up as
    /// a `lease`/`leading`/`gen` divergence with a readable diff, so the
    /// driver does not assert the outcome here.
    ///
    /// The staleness decision is re-evaluated against the *stashed*
    /// snapshot at *this* step's clock (finding 3 in the module header).
    /// The re-run's `ObservedUpdate` is always `Keep` or `Clear` (the
    /// stashed rv either still matches the observation it itself
    /// started, or the holder is empty), so the observed record is not
    /// perturbed relative to the model's.
    async fn put(&mut self, n: &str) -> Result {
        let base = self.base;
        let our_id = n.to_string();
        let h = self.node(n);
        let now = base + TICK * u32::try_from(h.ticks).expect("tick counter fits u32");
        let outcome = h.stash.take().with_context(|| {
            format!("PUT action for {n} with no stashed apiGet outcome (the model's hasSnap precondition should make this unreachable)")
        })?;
        let outcome = match outcome {
            FetchOutcome::Create => FetchOutcome::Create,
            FetchOutcome::Decided { lease, .. } => {
                // The same projection fetch_and_decide() performs
                // (election.rs): holder + resourceVersion out of the
                // fetched object, into decide().
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
                    &mut h.election.observed,
                    &our_id,
                    STEAL_AFTER,
                    now,
                );
                FetchOutcome::Decided { decision, lease }
            }
        };
        let result = h
            .election
            .act(outcome)
            .await
            .context("PUT: act against the mock apiserver")?;
        // run_lease_loop's Ok(Ok(result)) arm: the completed round-trip
        // re-anchors the self-fence clock, then the
        // (leading?, was_leading) edge detection fires
        // on_acquire/on_lose.
        h.fence_tick = h.ticks;
        match result {
            ElectionResult::Leading { transitions } => {
                if !h.was_leading {
                    h.state.on_acquire(transitions);
                }
                h.was_leading = true;
            }
            ElectionResult::Standby | ElectionResult::Conflict => {
                if h.was_leading {
                    h.state.on_lose();
                }
                h.was_leading = false;
            }
        }
        Ok(())
    }

    /// `selfFence(n)`: the production loop's error-arm fence check. The
    /// driver only invokes it when the trace says to (the model lets the
    /// fence fire any time the deadline has passed; the production loop
    /// checks every renew interval; the driver checks exactly when the
    /// model fired it — a subset of both, sound for a safety check).
    fn self_fence(&mut self, n: &str) {
        let h = self.node(n);
        // maybe_self_fence measures `last_successful_renew.elapsed()`
        // against the real clock (finding 4 in the module header), so
        // the anchor must be a real past instant: now minus the model's
        // tick delta. checked_sub only fails if the host has been up for
        // less time than the delta, which the model's clock ceiling
        // bounds to a handful of TICKs.
        let blind_for = TICK * u32::try_from(h.ticks - h.fence_tick).expect("tick delta fits u32");
        let last_renew = Instant::now()
            .checked_sub(blind_for)
            .expect("host uptime exceeds the model's clock ceiling");
        let mut owe_cost_clear = false;
        let fired = maybe_self_fence(
            &h.state,
            &mut h.was_leading,
            &mut owe_cost_clear,
            last_renew,
        );
        // The model's selfFence precondition is `leading[n] ∧ deadline
        // passed`; the tick mapping guarantees the implementation agrees
        // the deadline passed. A non-firing fence here is a driver or
        // mapping bug, not a protocol divergence — fail loudly rather
        // than letting the post-step diff report a confusing `leading`
        // mismatch.
        assert!(
            fired,
            "selfFence({n}) in the trace but maybe_self_fence did not fire \
             (blind_for={blind_for:?}, threshold={:?})",
            crate::SELF_FENCE_AFTER
        );
    }

    /// `crash(n)`: lose all in-memory state. The mock apiserver keeps
    /// the lease and the node's clock keeps its value (the model resets
    /// neither); everything else — the belief, the generation Arc, the
    /// observed record, the snapshot, the fence anchor — is reborn at
    /// the init values.
    fn crash(&mut self, n: &str) {
        let client = self.client.clone();
        let h = self.node(n);
        let ticks = h.ticks;
        *h = Self::fresh_harness(&client, n);
        h.ticks = ticks;
    }

    /// `recover(n)`: the process restarts. All the state was already
    /// reset by `crash`; recovery is the model flipping `alive` back,
    /// which the driver does not track (no per-node action targets a
    /// crashed node — the model's preconditions guarantee it).
    fn recover(&mut self, _n: &str) {}

    /// Project the mock apiserver's stored Lease into the model's
    /// `LeaseRec`. The mock is seeded at init so the store is never
    /// empty in the base regime; the empty case maps to the model's
    /// born-empty lease anyway (the same value init seeds).
    fn project_lease(&self) -> ModelLease {
        match self.mock.stored() {
            None => ModelLease {
                holder: ModelHolder::NoHolder,
                rv: 0,
                r#gen: 0,
            },
            Some(obj) => ModelLease {
                holder: match obj["spec"]["holderIdentity"].as_str() {
                    None | Some("") => ModelHolder::NoHolder,
                    Some(h) => ModelHolder::Held(h.to_string()),
                },
                rv: obj["metadata"]["resourceVersion"]
                    .as_str()
                    .expect("mock lease has a resourceVersion")
                    .parse()
                    .expect("mock resourceVersion is numeric"),
                r#gen: obj["spec"]["leaseTransitions"]
                    .as_u64()
                    .expect("mock lease has a numeric leaseTransitions"),
            },
        }
    }

    fn project(&self) -> Projection {
        Projection {
            lease: self.project_lease(),
            leading: self
                .nodes
                .iter()
                .map(|(n, h)| (n.clone(), h.state.is_leader()))
                .collect(),
            r#gen: self
                .nodes
                .iter()
                .map(|(n, h)| (n.clone(), h.state.generation()))
                .collect(),
        }
    }
}

// =======================================================================
// The quint-connect driver (the simulation path)
// =======================================================================

/// The [`Driver`] quint-connect drives. Owns a current-thread tokio
/// runtime (quint-connect's `step` is sync; the election API is async)
/// and the [`MbtSystem`] (absent until the trace's `init` step).
struct LeaseDriver {
    rt: tokio::runtime::Runtime,
    sys: Option<MbtSystem>,
}

impl LeaseDriver {
    fn new() -> Self {
        Self {
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime"),
            // Populated by the trace's `init` step (always the first step
            // of every generated trace). Constructing a MockApiServer
            // here would panic: tokio::spawn needs the runtime context
            // that only exists inside `rt.block_on`.
            sys: None,
        }
    }

    fn sys(&mut self) -> &mut MbtSystem {
        self.sys.as_mut().expect("MbtSystem present after init")
    }
}

impl Driver for LeaseDriver {
    type State = Projection;

    fn step(&mut self, step: &Step) -> Result {
        // Split the borrow: the runtime handle and the system are
        // disjoint fields, so `rt.block_on(sys.method())` borrows both
        // without fighting the borrow checker.
        switch!(step {
            init => {
                // A multi-trace run replays init at the start of every
                // trace; the old mock's handler task exits when its last
                // Client clone (held by the old harnesses) is dropped.
                let sys = self.rt.block_on(async { MbtSystem::init() });
                self.sys = Some(sys);
            },
            tickAny(n: String) => self.sys().tick(&n),
            apiGetAny(n: String) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.api_get(&n))?;
            },
            // The model's PUT actions. quint's `--mbt` tracker records
            // the INNERMOST `any` disjunct's name, so a steal/renew step
            // arrives as `claimOk`/`claimFails` (the claim disjunction
            // nested inside both); the outer `stealAny`/`renewLeaseAny`
            // names would only appear if a future quint records the
            // outermost disjunct instead. All five arms are the same
            // implementation event — "node n's PUT lands or 409s" — and
            // the post-step state diff distinguishes the outcomes.
            stealAny(n: String) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.put(&n))?;
            },
            renewLeaseAny(n: String) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.put(&n))?;
            },
            conflictAny(n: String) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.put(&n))?;
            },
            claimOk(n: String) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.put(&n))?;
            },
            claimFails(n: String) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.put(&n))?;
            },
            selfFenceAny(n: String) => self.sys().self_fence(&n),
            crashAny(n: String) => self.sys().crash(&n),
            recoverAny(n: String) => self.sys().recover(&n),
            deleteLease => anyhow::bail!(
                "deleteLease reached the driver: a fault-regime action is \
                 unreachable in leaderElectionBase (MAX_DELETES = 0); \
                 driving the deletion regime is phase 2"
            ),
            pgRestore => anyhow::bail!(
                "pgRestore reached the driver: a fault-regime action is \
                 unreachable in leaderElectionBase (MAX_RESTORES = 0); \
                 driving the pg-faults regime is phase 2"
            )
        })
    }
}

impl State<LeaseDriver> for Projection {
    fn from_driver(driver: &LeaseDriver) -> Result<Self> {
        Ok(driver
            .sys
            .as_ref()
            .context("projection requested before the trace's init step")?
            .project())
    }
}

/// Seeded random simulation against the base regime: quint generates
/// traces by walking `step` from `init`, the driver replays each one,
/// the projection is diffed after every step. The seed is pinned (an
/// input, not a measurement) so CI is deterministic; delete it to
/// explore locally and pin any seed that finds a divergence.
#[quint_connect::quint_run(
    spec = "../docs/spec/models/leaderElection.qnt",
    main = "leaderElectionBase",
    max_samples = 100,
    max_steps = 20,
    seed = "0x52494f01"
)]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_simulation_base() -> impl Driver {
    LeaseDriver::new()
}

// =======================================================================
// The named-run replays (the deterministic path)
// =======================================================================

/// One step of a named run, mirroring the model's per-node action
/// applications. The runs apply actions to literal nodes (no `nondet`),
/// which is exactly why quint-connect cannot replay them (finding 1 in
/// the module header) and why this mirror is needed.
#[derive(Debug, Clone, Copy)]
enum Action {
    ApiGet(&'static str),
    Steal(&'static str),
    RenewLease(&'static str),
    Conflict(&'static str),
    Tick(&'static str),
    SelfFence(&'static str),
    Crash(&'static str),
    Recover(&'static str),
}

use Action::*;

/// `casRaceRun`: both nodes GET the empty lease at rv 0, n1's PUT lands
/// first, n2's PUT against its stale rv-0 snapshot is the 409 path.
const CAS_RACE_RUN: &[Action] = &[ApiGet("n1"), ApiGet("n2"), Steal("n1"), Conflict("n2")];

/// `deposedLeaderStealRun`: n1 acquires and goes silent; n2 watches the
/// rv not change past the steal threshold and steals while n1 still
/// believes (the dual-belief window the generation fence closes).
const DEPOSED_LEADER_STEAL_RUN: &[Action] = &[
    ApiGet("n1"),
    Steal("n1"),
    ApiGet("n2"),
    Tick("n1"),
    Tick("n2"),
    Tick("n1"),
    Tick("n2"),
    Tick("n1"),
    Tick("n2"),
    Tick("n1"),
    Tick("n2"),
    Steal("n2"),
];

/// `selfFenceFalseAlarmRun`: n1 acquires, goes blind past the fence
/// deadline, self-fences, reconnects, and renews its un-stolen lease at
/// the SAME generation (the idempotent re-acquisition).
const SELF_FENCE_FALSE_ALARM_RUN: &[Action] = &[
    ApiGet("n1"),
    Steal("n1"),
    Tick("n1"),
    Tick("n2"),
    Tick("n1"),
    Tick("n2"),
    Tick("n1"),
    Tick("n2"),
    Tick("n1"),
    SelfFence("n1"),
    ApiGet("n1"),
    RenewLease("n1"),
];

/// `crashRecoverRenewRun`: n1 acquires, crashes (the in-memory
/// generation Arc resets to 1), recovers, and renews its still-valid
/// lease at its PRE-crash generation (restored from the transition
/// count).
const CRASH_RECOVER_RENEW_RUN: &[Action] = &[
    ApiGet("n1"),
    Steal("n1"),
    Crash("n1"),
    Recover("n1"),
    ApiGet("n1"),
    RenewLease("n1"),
];

impl MbtSystem {
    /// Apply one mirrored named-run action. The same methods the
    /// quint-connect switch dispatches to — only the dispatcher differs.
    async fn apply(&mut self, action: Action) -> Result {
        match action {
            ApiGet(n) => self.api_get(n).await,
            Steal(n) | RenewLease(n) | Conflict(n) => self.put(n).await,
            Tick(n) => {
                self.tick(n);
                Ok(())
            }
            SelfFence(n) => {
                self.self_fence(n);
                Ok(())
            }
            Crash(n) => {
                self.crash(n);
                Ok(())
            }
            Recover(n) => {
                self.recover(n);
                Ok(())
            }
        }
    }
}

/// Replay one named run: have quint execute it (which also checks its
/// `.expect(...)` clause) and emit the per-step states as an ITF trace,
/// then drive the implementation through the mirrored action sequence
/// and diff the projection against the model's state after every step
/// (including the init state).
fn replay_named_run(run: &str, actions: &[Action]) -> Result {
    // quint test writes one trace file per matched test; {seq} is its
    // 0-based index. Unique-per-process so parallel test binaries don't
    // collide.
    let out = std::env::temp_dir().join(format!("rio-mbt-{}-{}", std::process::id(), run));
    std::fs::create_dir_all(&out).context("create the trace output dir")?;
    let out_pattern = out.join("trace_{seq}.itf.json");
    let output = Command::new("quint")
        .arg("test")
        .arg(SPEC_ABS)
        .args(["--main", "leaderElectionBase"])
        .args(["--match", &format!("^{run}$")])
        .args(["--max-samples", "1"])
        .arg("--out-itf")
        .arg(&out_pattern)
        .args(["--verbosity", "0"])
        .output()
        .context("spawn quint (is it on the PATH?)")?;
    ensure!(
        output.status.success(),
        "quint test --match=^{run}$ failed (the run's .expect() clause may have \
         regressed):\n{}\n{}",
        str::from_utf8(&output.stdout).unwrap_or("<non-UTF-8 quint stdout>"),
        str::from_utf8(&output.stderr).unwrap_or("<non-UTF-8 quint stderr>"),
    );
    let trace_path = out.join("trace_0.itf.json");
    let json = std::fs::read_to_string(&trace_path).with_context(|| {
        format!(
            "read {} (did quint match exactly one test?)",
            trace_path.display()
        )
    })?;
    let trace: itf::Trace<Projection> =
        itf::trace_from_str(&json).context("decode the ITF trace into the projection")?;
    // Best-effort cleanup; a leftover tempdir is not a test failure.
    let _ = std::fs::remove_dir_all(&out);

    ensure!(
        trace.states.len() == actions.len() + 1,
        "{run}: the model's trace has {} states but the mirrored action sequence \
         has {} actions (+1 for init) — the run definition in leaderElection.qnt \
         and the Rust mirror have drifted",
        trace.states.len(),
        actions.len(),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    let mut sys = rt.block_on(async { MbtSystem::init() });
    diff_step(run, 0, "init", &trace.states[0].value, &sys.project())?;
    for (i, action) in actions.iter().enumerate() {
        rt.block_on(sys.apply(*action))
            .with_context(|| format!("{run}: step {} ({action:?})", i + 1))?;
        diff_step(
            run,
            i + 1,
            &format!("{action:?}"),
            &trace.states[i + 1].value,
            &sys.project(),
        )?;
    }
    Ok(())
}

/// One post-step state comparison. The model's state is the oracle; a
/// mismatch is either a driver bug (the mapping, the projection, or the
/// tick translation is wrong) or a genuine model↔implementation
/// disagreement — classify before fixing.
fn diff_step(
    run: &str,
    index: usize,
    action: &str,
    spec: &Projection,
    implementation: &Projection,
) -> Result {
    ensure!(
        spec == implementation,
        "{run}: state divergence after step {index} ({action})\n\
         --- specification ---\n{spec:#?}\n\
         --- implementation ---\n{implementation:#?}",
    );
    Ok(())
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_cas_race() {
    replay_named_run("casRaceRun", CAS_RACE_RUN).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_deposed_leader_steal() {
    replay_named_run("deposedLeaderStealRun", DEPOSED_LEADER_STEAL_RUN).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_self_fence_false_alarm() {
    replay_named_run("selfFenceFalseAlarmRun", SELF_FENCE_FALSE_ALARM_RUN).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_crash_recover_renew() {
    replay_named_run("crashRecoverRenewRun", CRASH_RECOVER_RENEW_RUN).unwrap();
}
