//! Model-based testing: replay traces generated from the
//! materialization-job Quint model
//! (`docs/spec/models/materializationJob.qnt`) against the real
//! scheduler claim plane, diffing the implementation's projected state
//! against the model's after every step.
//!
//! The model proves the job lifecycle protocol is correct; this module
//! proves the code is that protocol. The per-regime `quint verify`
//! checks in `nix/quint.nix` explore the model's state space; the
//! `mbt-rio-materialization` check (same file) replays concrete traces
//! through the production merge intake, the dispatch probe partition,
//! the kinded pull mint, the report consumption, and the moot sweep —
//! so a model<->implementation drift surfaces as a red check with a
//! per-step state diff instead of as a review-time judgment call.
//!
//! live_061 is the motivating instance: `obsoleteOnProducedRun` is the
//! by-other-means trace whose final step demands `JObsolete`. At the
//! pre-fold tree the moot sweep resolved that row `cancelled`, and this
//! replay reds at exactly that step (spec `JObsolete` vs implementation
//! `JCancelled`) — the conformance gap the model carried for the
//! system's whole life with zero production writers, made a per-step
//! machine diff.
//!
//! # Architecture
//!
//! [`MbtSystem`] owns an ephemeral PostgreSQL database
//! (`rio_test_support::TestDb` — the same schema production runs
//! against), a [`MockStore`] standing in for the store fleet, and one
//! BARE [`DagActor`] (leader-by-default, store client + service signer
//! wired) driven by calling the same `pub(super)` handler methods the
//! actor command loop dispatches to. Driver tier note (recorded
//! divergence from the spawned-handle tier most actor tests use): the
//! walk needs two seams only the direct tier exposes — the
//! legacy-interest injection (`interested_builds` without wanted rows;
//! the delta-2b population) and the deferral rewind
//! (`test_set_defer_until`) — and one driver serving both the named
//! runs and the simulation beats two. The command-loop plumbing the
//! direct tier skips is queueing only, covered by every handle-tier
//! actor test.
//!
//! # What is drivable, and what is scoped out
//!
//! The model spans the scheduler job plane and the store-side
//! execution plane. Only the former is the subject:
//!
//! - **Scheduler job plane** (the subject): job creation (the five
//!   origin lanes), the view, the kinded claim mint, the report
//!   consumption routing, the moot sweep, zero-interest cancellation.
//!   Driven by the production handlers; projected from the real PG
//!   rows, the real job view, and the real DAG.
//! - **Store-side** (`executeStep`, `releasePins`, `storeGc`,
//!   `upstreamChange`, `present`/`pins`/`ingested`/`upstreamAvail`):
//!   the executor's ingest walk and the store's pin/GC plane live in
//!   rio-store. The driver mirrors them as bookkeeping (the mock
//!   store's seeded content IS "the path is locally present"), and
//!   none of them are projected. Conformance of the store executor is
//!   out of scope here (its own batteries cover it).
//! - **Build plane** (`buildTerminal`, `builderPull`,
//!   `dispatchFromSource`): interest lifecycle. `buildTerminal` maps
//!   to the production build cancel; the build-kind pull arms are
//!   regime-rare and bail loudly if a re-pinned seed ever reaches
//!   them (the scope ladder note below).
//!
//! Per-action dispositions that are not the obvious "call the real
//! function":
//!
//! - **`createJob` origin lanes.** `OProbe` drives the full dispatch
//!   probe partition (`sweep_ready_cached` with the wanted paths
//!   transiently substitutable — the standalone fenced creation site).
//!   `OMerge`/`OPruned`/`OReprobe` call
//!   [`super::super::DagActor::create_materialization_job`] directly:
//!   production couples those lanes to a merge transaction by design,
//!   so the lane plumbing is not replayable standalone, but the
//!   creation fn IS the production chokepoint all five lanes converge
//!   on (one quantity, one producer). `OReprobe`'s AS-5 status reset
//!   sits below the normalized node projection (see the normalize
//!   note), so the direct create realizes the same projected state.
//!   `OStaleReset` bails (unreached by the pinned seed; the ladder).
//!   The production creation fn also backfills wanted rows for every
//!   live interested build regardless of lane — the model backfills
//!   only at standalone sites — but both sides agree on `liveWanted`
//!   (the union is `OUTPUTS` either way), so the difference is
//!   unobservable in the projected plane.
//! - **`recordWantedRelation`** is a real merge
//!   (`handle_merge_dag`) with nothing substitutable and nothing
//!   present at probe time, so the merge realizes exactly the
//!   relation write (no inline completion, no in-tx job).
//!   Re-records re-merge the same build with its cumulative node set
//!   (the A4 union upsert).
//! - **`legacyBuildArrives`** injects `interested_builds` directly
//!   (the pre-relation-era population has no merge to replay); the
//!   build itself is realized as a real merged build holding a filler
//!   node outside `DRVS` (invisible to the projection).
//! - **`obsoleteOnProduced` / `cancelOnZeroInterest`** drive
//!   `tick_cancel_zero_interest_materialization` — the moot sweep is
//!   the production arm the model action mirrors. The sweep settles
//!   every moot job in one pass while the model settles one per
//!   action: a trace that holds TWO simultaneously-moot jobs would
//!   diff at the first sweep. The pinned seed's traces never reach
//!   that state; a re-pinned seed that does must scope its trace or
//!   accept the documented red.
//! - **`completeReadyFromStore`** seeds the mock with the
//!   driver-mirrored present paths and runs the probe sweep (the
//!   store short-circuit lane), then unseeds — content visibility is
//!   scoped to the action that asserts it, so no other arm's merge or
//!   probe can complete a node the model did not.
//! - **`deferExpires`** rewinds the production deferral through
//!   [`super::super::materialize::JobViewEntry::test_set_defer_until`]
//!   (the deferral is a wall-clock window production re-admits by
//!   time; the model flips a bool — the rewind IS the lapse).
//! - **`durableRelationChange`** is driver bookkeeping consumed at
//!   the consume arms (the mock's substitutable set mirrors
//!   `upstreamAvail` transiently at the report that routes on it).
//!
//! # The projection
//!
//! Field names are the model's variable names (ITF namespace prefix
//! `materializationJobBase::materializationJob::`). Projected:
//!
//! - `jobs` <- the latest `materialization_jobs` row per drv, with the
//!   model's `JClaimed` derived as row-`pending` AND an open
//!   materialization attempt (production has no claimed row state BY
//!   DESIGN — a claim is an open attempt; the job row is untouched
//!   until consumption). `parked` <- `park_until > now()`; the budget
//!   counters <- the post-anchor attempt-ledger window (the 085
//!   creation-reset cut — the same window production's budget fold
//!   reads).
//! - `view` <- the actor's real job view map
//!   (`materialization_jobs`): absent entry = `VNone`, held claim =
//!   `VClaimed(instance)`, otherwise `VPending(parked)` (the
//!   `Claimability` classification; the view-only deferral projects
//!   as unparked exactly as the model's `VPending(false)` +
//!   `deferUntil` split does).
//! - `nodeStatus` <- the DAG node status per drv, mapped
//!   {Created,Queued,Ready}->NQueued, {Assigned,Running}->NRunning,
//!   Completed->NCompleted, {Failed,Poisoned}->NFailed,
//!   DependencyFailed->NDepFailed, Skipped->NCompleted (the CA
//!   cutoff IS completed-by-other-means), Cancelled->NQueued (the
//!   model's `buildTerminal` is node-blind: the cancel cascade's
//!   letter has no model image; the job-plane letters the sweep
//!   writes are the conformance surface). An unmerged drv projects
//!   NQueued (the model's init letter means "no node yet").
//! - `attempts` <- the open assignment row joined on
//!   `attempt_kind = 'materialization'`; the holder instance is the
//!   composite identity's suffix.
//!
//! Omitted: `wanted`/`lastWritten`/`everContrib` (relation plane —
//! exercised through `liveWanted` routing, not projected),
//! `present`/`pins`/`ingested`/`upstreamAvail`/`durableRel`
//! (store-side), `topdownPruned` (carried by the row's origin, which
//! both `OProbe` and `OMerge` map to `cache_opportunity` — not
//! injectively recoverable), `deferUntil` (view-only pacing; its
//! behavioral face is the claim admission the `claimJob` arm
//! exercises), `execsMinted`/`resolutionExec`/`tenant` (identity
//! plane: model ordinals vs production UUIDs), the ledgers (their
//! post-anchor window rides the job counters), and every ghost
//! latch. Diffing driver bookkeeping against the model proves nothing
//! about the implementation.
//!
//! # The normalize step
//!
//! `NReady -> NQueued` on BOTH sides of every comparison. The model
//! keeps the conservative letter at creation (`recordWantedRelation`
//! and `createJob` leave `NQueued`) while production promotes a
//! dep-free merged node to `Ready`; conversely the model's
//! `NReady` (the from-source release) and production's restored
//! `Ready` agree. Every model guard is invariant over the pair —
//! `claimJob`, `createJob`, and `completeReadyFromStore` all accept
//! `NQueued or NReady` — so the pair is one equivalence class at
//! every transition the model takes, and the projection compares the
//! class. The load-bearing letters (NRunning while claimed,
//! NQueued-class after release, NCompleted at the by-other-means
//! edge, the failure letters) stay exact.
//!
//! # Determinism
//!
//! The named-run replays are fully deterministic. The simulation pins
//! its seed in the `#[quint_run]` attribute (an input, not a
//! measurement); unseeded exploration is a local activity — delete
//! the seed, run until a divergence appears, pin the offending seed.
//! Unimplemented walk arms bail with the scope ladder named, so a
//! seed or model change that reaches one is a loud red, never a
//! silent skip.
//!
//! All tests are `#[ignore]`d: they shell out to `quint`, which the
//! default `nextest-rio-scheduler` sandbox does not provide. The
//! dedicated check (`mbt-rio-materialization`, wired in
//! `nix/quint.nix` next to `mbt-rio-logservice`) stages the model
//! into the nextest workspace and runs them with `--run-ignored`.
//! Locally:
//!
//! ```text
//! cargo nextest run -p rio-scheduler -E 'test(/mbt_/)' --run-ignored all
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use uuid::Uuid;

use super::*;
use crate::state::JobOrigin;

/// The model's derivation universe (materializationJobBase's `DRVS`).
const DRVS: &[&str] = &["d1", "d2"];
/// The model's output universe (`OUTPUTS`).
const OUTPUTS: &[&str] = &["o1", "o2"];

/// The spec path fallback for a local `cargo nextest` run. The
/// `mbt-rio-materialization` check overrides it via
/// `RIO_MBT_SPEC_PATH`: the test binary runs in a different sandbox
/// than the one that compiled it, so this baked path points at a tree
/// that no longer exists there.
const SPEC_ABS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/spec/models/materializationJob.qnt"
);

fn spec_path() -> std::path::PathBuf {
    std::env::var_os("RIO_MBT_SPEC_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(SPEC_ABS))
}

/// A [`SchedulerDb`] over the test pool (the ledger-suffix reads).
fn mbt_db(pool: &sqlx::PgPool) -> SchedulerDb {
    SchedulerDb::new(pool.clone())
}

/// The one store path for a model (drv, output) pair.
fn out_path(d: &str, o: &str) -> String {
    test_store_path(&format!("{d}-{o}"))
}

// =======================================================================
// The projection (the abstraction function)
// =======================================================================

/// The slice of the model's state the implementation observably
/// realizes. See the module header for what each field is projected
/// from and why the rest are omitted.
#[derive(Debug, PartialEq, Deserialize)]
struct Projection {
    #[serde(rename = "materializationJobBase::materializationJob::jobs")]
    jobs: BTreeMap<String, ModelJobOpt>,
    #[serde(rename = "materializationJobBase::materializationJob::view")]
    view: BTreeMap<String, ModelView>,
    #[serde(rename = "materializationJobBase::materializationJob::nodeStatus")]
    node_status: BTreeMap<String, ModelNode>,
    #[serde(rename = "materializationJobBase::materializationJob::attempts")]
    attempts: BTreeMap<String, ModelAttemptOpt>,
}

impl Projection {
    /// Collapse the claimable node pair on both sides of every
    /// comparison (the module-header normalize rationale: every model
    /// guard is invariant over `{NQueued, NReady}`, and the two sides
    /// pick different representatives at creation vs release).
    fn normalize(&mut self) {
        for status in self.node_status.values_mut() {
            if *status == ModelNode::NReady {
                *status = ModelNode::NQueued;
            }
        }
    }
}

/// The model's `JobOpt`.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelJobOpt {
    NoJob,
    SomeJob(ModelJob),
}

/// The model's `Job`, restricted to the projected fields (serde skips
/// the identity-plane fields; the module header records why).
#[derive(Debug, PartialEq, Deserialize)]
struct ModelJob {
    state: ModelJobState,
    parked: bool,
    #[serde(rename = "matInfraCount", with = "itf::de::As::<itf::de::Integer>")]
    mat_infra_count: u64,
    #[serde(
        rename = "matUnobtainableCount",
        with = "itf::de::As::<itf::de::Integer>"
    )]
    mat_unobtainable_count: u64,
}

/// The model's `JobState`.
// The variant names are the model's exact constructors (the ITF tag
// is the decode key), prefix and all.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelJobState {
    JPending,
    JClaimed,
    JResolvedSuccess,
    JResolvedFromSource,
    JResolvedUnobtainable,
    JObsolete,
    JCancelled,
}

/// The model's `ViewState`.
// The variant names are the model's exact constructors (the ITF tag
// is the decode key), prefix and all.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelView {
    VNone,
    VPending(bool),
    VClaimed(String),
}

/// The model's `NodeStatus`.
// The variant names are the model's exact constructors (the ITF tag
// is the decode key), prefix and all.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelNode {
    NQueued,
    NReady,
    NRunning,
    NCompleted,
    NFailed,
    NDepFailed,
}

/// The model's `AttemptOpt`, restricted to the projected fields.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelAttemptOpt {
    NoAttempt,
    OpenAttempt(ModelAttempt),
}

#[derive(Debug, PartialEq, Deserialize)]
struct ModelAttempt {
    kind: ModelAttemptKind,
    instance: String,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelAttemptKind {
    KBuild,
    KMaterialization,
}

// =======================================================================
// Driver actions (shared by the named-run mirrors and the simulation)
// =======================================================================

/// The model origin alphabet, as the walk's nondet pick decodes.
// The variant names are the model's exact constructors (the ITF tag
// is the decode key), prefix and all.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelOrigin {
    OProbe,
    OMerge,
    OPruned,
    OReprobe,
    OStaleReset,
}

/// One model action, in driver-ready form. The named runs build these
/// from constants; the simulation builds them from the trace's nondet
/// picks.
#[derive(Debug, Clone)]
enum Act {
    RecordWanted {
        b: String,
        d: String,
        outs: BTreeSet<String>,
    },
    LegacyArrives {
        b: String,
        d: String,
    },
    CreateJob {
        d: String,
        origin: ModelOrigin,
        tenant: String,
    },
    DedupRefeed {
        d: String,
    },
    DedupPrunedUpgrade {
        d: String,
        tenant: String,
    },
    Claim {
        d: String,
        replica: String,
    },
    ExecuteStep {
        d: String,
        o: String,
    },
    ConsumeSuccess {
        d: String,
    },
    ConsumeUnobtainable {
        d: String,
        missing: BTreeSet<String>,
        refs_missing: bool,
    },
    ReportInfra {
        d: String,
    },
    ReportTransient {
        d: String,
    },
    DeferExpires {
        d: String,
    },
    MootSweep,
    CompleteReadyFromStore {
        d: String,
    },
    BuildTerminal {
        b: String,
    },
    UpstreamChange {
        d: String,
        o: String,
    },
    DurableRelationChange,
}

// =======================================================================
// The system under test
// =======================================================================

struct MbtSystem {
    db: TestDb,
    store: rio_test_support::grpc::MockStore,
    _store_task: tokio::task::JoinHandle<()>,
    actor: DagActor,
    /// model build name -> production build id.
    build_ids: BTreeMap<String, Uuid>,
    /// model build name -> (drv -> cumulative recorded outs). The
    /// re-merge unions exactly as the model's upsert does.
    build_outs: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// drvs with a production node (merged or injected).
    nodes: BTreeSet<String>,
    /// drv -> the open materialization exec id (claim bookkeeping the
    /// report arms consume; the model keys reports by drv).
    open_execs: BTreeMap<String, Uuid>,
    /// Store-side presence mirror: (drv, out) the executor ingested.
    present: BTreeSet<(String, String)>,
    /// Store-side upstream-availability mirror (init: all true).
    upstream_avail: BTreeMap<(String, String), bool>,
}

impl MbtSystem {
    async fn init() -> Result<MbtSystem> {
        let db = TestDb::new(&MIGRATOR).await;
        seed_default_tenant(&db.pool).await;
        let (store, store_client, store_task) =
            rio_test_support::grpc::spawn_mock_store_with_client().await?;
        let actor = Self::fresh_actor(&db, store_client);
        let mut upstream_avail = BTreeMap::new();
        for d in DRVS {
            for o in OUTPUTS {
                upstream_avail.insert(((*d).to_owned(), (*o).to_owned()), true);
            }
        }
        Ok(MbtSystem {
            db,
            store,
            _store_task: store_task,
            actor,
            build_ids: BTreeMap::new(),
            build_outs: BTreeMap::new(),
            nodes: BTreeSet::new(),
            open_execs: BTreeMap::new(),
            present: BTreeSet::new(),
            upstream_avail,
        })
    }

    /// A bare leader actor wired like `setup_with_mock_store`'s
    /// (store client + the harness service signer — merged_bug_003:
    /// tenant-scoped probing is the only probing that reports
    /// substitutable paths).
    fn fresh_actor(
        db: &TestDb,
        store_client: rio_proto::store::store_service_client::StoreServiceClient<
            tonic::transport::Channel,
        >,
    ) -> DagActor {
        let plumbing = DagActorPlumbing {
            store_client: Some(store_client),
            service_signer: Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            ))),
            ..Default::default()
        };
        DagActor::new(
            SchedulerDb::new(db.pool.clone()),
            DagActorConfig::default(),
            plumbing,
        )
    }

    /// Reset for a new trace (the simulation path). One ephemeral
    /// database per [`MbtSystem`]; per-trace resets truncate and
    /// rebuild the actor — a per-trace database would multiply the
    /// migration cost by the sample count.
    async fn reset(&mut self) -> Result<()> {
        sqlx::query(
            "TRUNCATE builds, derivations, materialization_jobs, build_wanted_outputs, \
             drv_executions, assignments, drv_attempts, scheduler_live_pins CASCADE",
        )
        .execute(&self.db.pool)
        .await
        .context("truncate the mbt tables")?;
        seed_default_tenant(&self.db.pool).await;
        self.store.state.paths.write().unwrap().clear();
        self.store.state.substitutable.write().unwrap().clear();
        let (store2, store_client, store_task) =
            rio_test_support::grpc::spawn_mock_store_with_client()
                .await
                .context("respawn mock store")?;
        self.store = store2;
        self._store_task = store_task;
        self.actor = Self::fresh_actor(&self.db, store_client);
        self.build_ids.clear();
        self.build_outs.clear();
        self.nodes.clear();
        self.open_execs.clear();
        self.present.clear();
        for v in self.upstream_avail.values_mut() {
            *v = true;
        }
        Ok(())
    }

    fn build_id(&self, b: &str) -> Result<Uuid> {
        self.build_ids
            .get(b)
            .copied()
            .with_context(|| format!("model build {b} has no production build yet"))
    }

    /// A drv's node spec for a merge request.
    fn node_spec(&self, d: &str, wanted: &BTreeSet<String>) -> rio_proto::types::DerivationNode {
        let mut n = make_node(d);
        n.output_names = OUTPUTS.iter().map(|o| (*o).to_string()).collect();
        n.expected_output_paths = OUTPUTS.iter().map(|o| out_path(d, o)).collect();
        n.wanted_output_names = wanted.iter().cloned().collect();
        n
    }

    /// Merge (or re-merge) build `b` with its cumulative node set —
    /// the production realization of `recordWantedRelation` (and of
    /// the build's existence for `legacyBuildArrives` fillers).
    async fn merge_build(&mut self, b: &str, filler: bool) -> Result<()> {
        let build_id = *self
            .build_ids
            .entry(b.to_owned())
            .or_insert_with(Uuid::new_v4);
        let outs = self.build_outs.get(b).cloned().unwrap_or_default();
        let mut nodes: Vec<rio_proto::types::DerivationNode> = outs
            .iter()
            .map(|(d, wanted)| self.node_spec(d, wanted))
            .collect();
        if filler || nodes.is_empty() {
            // A build with no recorded relations still needs a DAG to
            // merge (the legacy population holds in-memory interest
            // only). The filler node lives outside DRVS so the
            // projection never sees it.
            nodes.push(make_node(&format!("legacy-fill-{b}")));
        }
        let req = MergeDagRequest {
            build_id,
            tenant_id: Some(DEFAULT_TEST_TENANT),
            priority_class: PriorityClass::Scheduled,
            nodes,
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: Some("harness-tenant-jwt".into()),
        };
        self.actor
            .handle_merge_dag(req)
            .await
            .map_err(|e| anyhow::anyhow!("merge of model build {b} failed: {e:?}"))?;
        for d in outs.keys() {
            self.nodes.insert(d.clone());
        }
        Ok(())
    }

    /// Ensure drv `d` has a production node even though no build
    /// merged it (the legacy-interest face). Status lands Ready-class,
    /// which normalizes to the model's NQueued.
    async fn ensure_node(&mut self, d: &str) -> Result<()> {
        if self.nodes.contains(d) {
            return Ok(());
        }
        let derivation_id = {
            let row = crate::db::DerivationRow {
                drv_hash: d.into(),
                drv_path: test_drv_path(d),
                pname: Some("test-pkg".into()),
                system: "x86_64-linux".into(),
                status: DerivationStatus::Created,
                required_features: vec![],
                expected_output_paths: OUTPUTS.iter().map(|o| out_path(d, o)).collect(),
                output_names: OUTPUTS.iter().map(|o| (*o).to_string()).collect(),
                is_fixed_output: false,
                is_ca: false,
            };
            let mut tx = self.db.pool.begin().await?;
            let ids = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
            tx.commit().await?;
            ids.get(d).context("just inserted")?.0
        };
        self.actor
            .test_inject_ready_row(crate::db::RecoveryDerivationRow {
                derivation_id,
                ..crate::db::RecoveryDerivationRow::test_default(d, "x86_64-linux")
            });
        self.nodes.insert(d.to_owned());
        Ok(())
    }

    /// The dispatch probe pass with `paths` transiently substitutable
    /// (and the driver-mirrored present paths seeded — the probe's
    /// locally-present arm is how `completeReadyFromStore` lands).
    async fn probe_sweep(&mut self, substitutable: &[String], present: &[String]) -> Result<()> {
        {
            let mut subs = self.store.state.substitutable.write().unwrap();
            subs.clear();
            subs.extend(substitutable.iter().cloned());
        }
        for p in present {
            self.store.seed_with_content(p, b"mbt-present");
        }
        // Mirror the tick's generation advance so each drive re-probes
        // (the per-tick FMP dedup is keyed on this).
        self.actor.probe_generation = self.actor.probe_generation.wrapping_add(1);
        self.actor.sweep_ready_cached().await;
        self.store.state.substitutable.write().unwrap().clear();
        self.store.state.paths.write().unwrap().clear();
        Ok(())
    }

    /// Live-wanted store paths of `d` per the driver's relation
    /// bookkeeping (the union over live builds; the production
    /// backfill saturates legacy interest to all outputs, and so does
    /// the model).
    fn live_wanted_paths(&self, d: &str) -> Vec<String> {
        let mut outs: BTreeSet<String> = BTreeSet::new();
        for (b, per_drv) in &self.build_outs {
            if self.build_ids.contains_key(b)
                && let Some(o) = per_drv.get(d)
            {
                outs.extend(o.iter().cloned());
            }
        }
        outs.iter().map(|o| out_path(d, o)).collect()
    }

    async fn report(
        &mut self,
        d: &str,
        outcome: rio_proto::types::MaterializationOutcome,
    ) -> Result<()> {
        let exec_id = self
            .open_execs
            .remove(d)
            .with_context(|| format!("no open exec bookkept for {d}"))?;
        let mut payload = pull_payload(rio_proto::types::BuildResult::default());
        payload.materialization_outcome = Some(outcome);
        let (tx, rx) = oneshot::channel();
        self.actor
            .handle_report_outcome(exec_id, Some(d.to_owned()), payload, tx)
            .await;
        rx.await
            .context("actor dropped the report reply")?
            .map_err(|e| anyhow::anyhow!("report for {d} rejected: {e:?}"))
    }

    /// Apply one model action through the production surfaces. The
    /// mapping per arm is the module header's disposition table.
    async fn apply(&mut self, act: Act) -> Result<()> {
        match act {
            Act::RecordWanted { b, d, outs } => {
                let first_record = !self.build_ids.contains_key(&b);
                self.build_outs
                    .entry(b.clone())
                    .or_default()
                    .entry(d.clone())
                    .or_default()
                    .extend(outs);
                if first_record {
                    // The build's first record IS its merge.
                    return self.merge_build(&b, false).await;
                }
                // Production merges are one-shot per build (builds_pkey);
                // a later record by the same build rides the same union
                // upsert the merge transaction executes
                // (record_wanted_in_tx — the merged_bug_176 saturating
                // union), plus the in-memory interest the merge would
                // have registered.
                self.ensure_node(&d).await?;
                let bid = self.build_id(&b)?;
                self.actor
                    .dag
                    .node_mut(&DrvHash::from(d.as_str()))
                    .context("node just ensured")?
                    .interested_builds
                    .insert(bid);
                let derivation_id: Uuid =
                    sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                        .bind(&d)
                        .fetch_one(&self.db.pool)
                        .await?;
                let wanted: Vec<String> = self
                    .build_outs
                    .get(&b)
                    .and_then(|m| m.get(&d))
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                let mut tx = self.db.pool.begin().await?;
                crate::db::SchedulerDb::record_wanted_in_tx(
                    &mut tx,
                    &[crate::db::wanted::WantedRow {
                        build_id: bid,
                        derivation_id,
                        wanted_output_names: &wanted,
                    }],
                )
                .await?;
                tx.commit().await?;
                Ok(())
            }
            Act::LegacyArrives { b, d } => {
                if !self.build_ids.contains_key(&b) {
                    self.merge_build(&b, true).await?;
                }
                self.ensure_node(&d).await?;
                let bid = self.build_id(&b)?;
                self.actor
                    .dag
                    .node_mut(&DrvHash::from(d.as_str()))
                    .context("node just ensured")?
                    .interested_builds
                    .insert(bid);
                Ok(())
            }
            Act::CreateJob { d, origin, tenant } => match origin {
                ModelOrigin::OProbe => {
                    let paths = self.live_wanted_paths(&d);
                    ensure!(
                        !paths.is_empty(),
                        "createJob(OProbe) on {d} with no live wanted relation"
                    );
                    self.probe_sweep(&paths, &[]).await
                }
                ModelOrigin::OMerge | ModelOrigin::OPruned | ModelOrigin::OReprobe => {
                    let production_origin = match origin {
                        ModelOrigin::OPruned => JobOrigin::Pruned,
                        ModelOrigin::OReprobe => JobOrigin::Reprobe,
                        _ => JobOrigin::CacheOpportunity,
                    };
                    let creating = self.build_id(&tenant).ok();
                    ensure!(
                        self.actor
                            .create_materialization_job(
                                &DrvHash::from(d.as_str()),
                                production_origin,
                                creating,
                                None,
                            )
                            .await,
                        "createJob({origin:?}) on {d} did not apply"
                    );
                    Ok(())
                }
                ModelOrigin::OStaleReset => bail!(
                    "createJob(OStaleReset) reached the driver: the stale-reset lane is \
                     outside the implemented walk arms (the scope ladder in the module \
                     header) — re-pin the seed or extend the driver"
                ),
            },
            Act::DedupRefeed { d } => {
                ensure!(
                    self.actor
                        .create_materialization_job(
                            &DrvHash::from(d.as_str()),
                            JobOrigin::CacheOpportunity,
                            None,
                            None,
                        )
                        .await,
                    "dedupRefeed({d}) did not apply"
                );
                Ok(())
            }
            Act::DedupPrunedUpgrade { d, tenant } => {
                let creating = self.build_id(&tenant).ok();
                ensure!(
                    self.actor
                        .create_materialization_job(
                            &DrvHash::from(d.as_str()),
                            JobOrigin::Pruned,
                            creating,
                            None,
                        )
                        .await,
                    "dedupPrunedUpgrade({d}) did not apply"
                );
                Ok(())
            }
            Act::Claim { d, replica } => {
                let (tx, rx) = oneshot::channel();
                self.actor
                    .handle_pull_assignment(
                        d.clone(),
                        Some(d.clone()),
                        rio_evidence_kernel::pull::PullKind::Materialization,
                        Some(replica.clone()),
                        None,
                        None,
                        false,
                        None,
                        tx,
                    )
                    .await;
                match rx.await.context("actor dropped the pull reply")? {
                    Ok(PullOutcome::Deliver(a)) => {
                        let exec_id: Uuid = a.exec_id.parse().context("exec id parses")?;
                        self.open_execs.insert(d, exec_id);
                        Ok(())
                    }
                    other => bail!(
                        "claimJob({d}, {replica}): the model admitted the claim but the \
                         implementation answered {other:?} — either a genuine admission \
                         drift or the model's node-blind buildTerminal over-approximation \
                         (the module header's claim-window note)"
                    ),
                }
            }
            Act::ExecuteStep { d, o } => {
                // Store-side ingest: the path becomes locally present.
                // Bookkeeping only — content visibility is scoped to
                // the arms that assert it (the module header).
                self.present.insert((d, o));
                Ok(())
            }
            Act::ConsumeSuccess { d } => {
                let ingested: Vec<String> = self
                    .present
                    .iter()
                    .filter(|(pd, _)| pd == &d)
                    .map(|(pd, po)| out_path(pd, po))
                    .collect();
                let outcome = rio_proto::types::MaterializationOutcome {
                    outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                        rio_proto::types::materialization_outcome::Success {
                            ingested_paths: ingested,
                            verified_paths: vec![],
                            verified_tenants: vec![],
                        },
                    )),
                };
                self.report(&d, outcome).await
            }
            Act::ConsumeUnobtainable {
                d,
                missing,
                refs_missing,
            } => {
                // The consumption routing re-probes upstream for the
                // reprobe arm: mirror the availability env at this
                // report exactly (transiently substitutable = avail
                // and not present).
                let avail: Vec<String> = OUTPUTS
                    .iter()
                    .filter(|o| {
                        self.upstream_avail
                            .get(&(d.clone(), (**o).to_owned()))
                            .copied()
                            .unwrap_or(true)
                            && !self.present.contains(&(d.clone(), (**o).to_owned()))
                    })
                    .map(|o| out_path(&d, o))
                    .collect();
                {
                    let mut subs = self.store.state.substitutable.write().unwrap();
                    subs.clear();
                    subs.extend(avail);
                }
                let outcome = rio_proto::types::MaterializationOutcome {
                    outcome: Some(
                        rio_proto::types::materialization_outcome::Outcome::Unobtainable(
                            rio_proto::types::materialization_outcome::Unobtainable {
                                missing_paths: missing.iter().map(|o| out_path(&d, o)).collect(),
                                verified_paths: vec![],
                                cause: "mbt: verified missing-and-unavailable".into(),
                                missing_reference_paths: if refs_missing {
                                    vec![test_store_path(&format!("{d}-ref-hole"))]
                                } else {
                                    vec![]
                                },
                                trust_refused: false,
                                refusal: rio_proto::types::UnobtainableRefusal::Unspecified.into(),
                            },
                        ),
                    ),
                };
                let r = self.report(&d, outcome).await;
                self.store.state.substitutable.write().unwrap().clear();
                r
            }
            Act::ReportInfra { d } => {
                let outcome = rio_proto::types::MaterializationOutcome {
                    outcome: Some(
                        rio_proto::types::materialization_outcome::Outcome::InfraFailure(
                            rio_proto::types::materialization_outcome::InfraFailure {
                                detail: "mbt: upstream 503".into(),
                            },
                        ),
                    ),
                };
                self.report(&d, outcome).await
            }
            Act::ReportTransient { d } => {
                let outcome = rio_proto::types::MaterializationOutcome {
                    outcome: Some(
                        rio_proto::types::materialization_outcome::Outcome::RetryLater(
                            rio_proto::types::materialization_outcome::RetryLater {
                                detail: "mbt: upstream rate-limited".into(),
                                retry_after_secs: 60,
                                class: "rate_limited".into(),
                            },
                        ),
                    ),
                };
                self.report(&d, outcome).await
            }
            Act::DeferExpires { d } => {
                let entry = self
                    .actor
                    .materialization_jobs
                    .get_mut(&DrvHash::from(d.as_str()))
                    .with_context(|| format!("deferExpires({d}): no view entry"))?;
                entry.test_set_defer_until(None);
                Ok(())
            }
            Act::MootSweep => {
                let authority = self
                    .actor
                    .dag_authority()
                    .context("direct-setup actor is authoritative")?;
                self.actor
                    .tick_cancel_zero_interest_materialization(&authority)
                    .await;
                Ok(())
            }
            Act::CompleteReadyFromStore { d } => {
                let present: Vec<String> = self
                    .present
                    .iter()
                    .filter(|(pd, _)| pd == &d)
                    .map(|(pd, po)| out_path(pd, po))
                    .collect();
                ensure!(
                    !present.is_empty(),
                    "completeReadyFromStore({d}) with nothing present"
                );
                self.probe_sweep(&[], &present).await
            }
            Act::BuildTerminal { b } => {
                // The model's builds exist (live) from init; production
                // builds materialize lazily at their first recorded
                // relation or legacy arrival. A terminal on a build the
                // driver never materialized has no production
                // observable (nothing holds interest through it) — the
                // liveness bookkeeping already reads it as dead.
                let Ok(bid) = self.build_id(&b) else {
                    return Ok(());
                };
                self.actor
                    .handle_cancel_build(bid, None, "mbt: model buildTerminal")
                    .await
                    .map_err(|e| anyhow::anyhow!("buildTerminal({b}) cancel failed: {e:?}"))?;
                self.build_ids.remove(&b);
                Ok(())
            }
            Act::UpstreamChange { d, o } => {
                let slot = self.upstream_avail.entry((d, o)).or_insert(true);
                *slot = !*slot;
                Ok(())
            }
            Act::DurableRelationChange => Ok(()),
        }
    }

    // ---- the projection ------------------------------------------------

    async fn project(&self) -> Result<Projection> {
        let mut jobs = BTreeMap::new();
        let mut attempts = BTreeMap::new();
        let mut view = BTreeMap::new();
        let mut node_status = BTreeMap::new();
        let now = std::time::Instant::now();
        for d in DRVS {
            let drv = (*d).to_owned();
            // The open materialization attempt (production's claimed
            // state IS this row — the abstraction the module header
            // documents).
            let open: Option<(String,)> = sqlx::query_as(
                "SELECT a.builder_id FROM assignments a \
                   JOIN drv_executions e ON e.exec_id = a.exec_id \
                   JOIN derivations dv ON dv.derivation_id = a.derivation_id \
                  WHERE dv.drv_hash = $1 \
                    AND a.status IN ('pending', 'acknowledged') \
                    AND e.attempt_kind = 'materialization'",
            )
            .bind(d)
            .fetch_optional(&self.db.pool)
            .await?;
            let instance =
                |composite: &str| composite.rsplit('@').next().unwrap_or(composite).to_owned();
            attempts.insert(
                drv.clone(),
                match &open {
                    Some((builder_id,)) => ModelAttemptOpt::OpenAttempt(ModelAttempt {
                        kind: ModelAttemptKind::KMaterialization,
                        instance: instance(builder_id),
                    }),
                    None => ModelAttemptOpt::NoAttempt,
                },
            );
            // The latest job row + the post-anchor budget window.
            let row: Option<(Uuid, String, bool)> = sqlx::query_as(
                "SELECT j.derivation_id, j.state, \
                        (j.park_until IS NOT NULL AND j.park_until > now()) AS parked \
                   FROM materialization_jobs j \
                  WHERE j.drv_hash = $1 \
                  ORDER BY j.created_at DESC, j.job_id DESC LIMIT 1",
            )
            .bind(d)
            .fetch_optional(&self.db.pool)
            .await?;
            let job = match row {
                None => ModelJobOpt::NoJob,
                Some((derivation_id, state, parked)) => {
                    let suffix = mbt_db(&self.db.pool)
                        .load_attempt_suffix(&[derivation_id])
                        .await?;
                    let rows = suffix.get(&derivation_id).cloned().unwrap_or_default();
                    let count = |class: crate::state::OutcomeClass| {
                        rows.iter()
                            .filter(|r| {
                                r.event_kind == crate::state::AttemptEventKind::Attempt
                                    && r.outcome_class == class
                            })
                            .count() as u64
                    };
                    let state = match (state.as_str(), open.is_some()) {
                        ("pending", true) => ModelJobState::JClaimed,
                        ("pending", false) => ModelJobState::JPending,
                        ("resolved_success", _) => ModelJobState::JResolvedSuccess,
                        ("resolved_from_source", _) => ModelJobState::JResolvedFromSource,
                        ("resolved_unobtainable", _) => ModelJobState::JResolvedUnobtainable,
                        ("obsolete", _) => ModelJobState::JObsolete,
                        ("cancelled", _) => ModelJobState::JCancelled,
                        (other, _) => bail!("unprojectable job state {other:?}"),
                    };
                    ModelJobOpt::SomeJob(ModelJob {
                        state,
                        parked,
                        mat_infra_count: count(crate::state::OutcomeClass::MaterializationInfra),
                        mat_unobtainable_count: count(
                            crate::state::OutcomeClass::MaterializationUnobtainable,
                        ),
                    })
                }
            };
            jobs.insert(drv.clone(), job);
            // The in-memory view.
            let entry = self.actor.materialization_jobs.get(&DrvHash::from(*d));
            view.insert(
                drv.clone(),
                match entry {
                    None => ModelView::VNone,
                    Some(e) => match e.holder() {
                        Some(holder) => ModelView::VClaimed(instance(holder.as_str())),
                        None => ModelView::VPending(matches!(
                            e.claimability(now),
                            crate::actor::materialize::Claimability::Parked
                        )),
                    },
                },
            );
            // The node status, through the documented map.
            let status = self.actor.dag.node(&DrvHash::from(*d)).map(|s| s.status());
            node_status.insert(
                drv,
                match status {
                    None | Some(DerivationStatus::Created) | Some(DerivationStatus::Queued) => {
                        ModelNode::NQueued
                    }
                    Some(DerivationStatus::Ready) => ModelNode::NReady,
                    Some(DerivationStatus::Assigned) | Some(DerivationStatus::Running) => {
                        ModelNode::NRunning
                    }
                    Some(DerivationStatus::Completed) | Some(DerivationStatus::Skipped) => {
                        ModelNode::NCompleted
                    }
                    Some(DerivationStatus::Failed) | Some(DerivationStatus::Poisoned) => {
                        ModelNode::NFailed
                    }
                    // The model's buildTerminal is node-blind and its
                    // NDepFailed letter has no writer in any regime
                    // (the alphabet carries it for completeness; the
                    // MBT universe has no edges) — production's
                    // interest-death cascade letters (in-flight ->
                    // Cancelled, undispatched -> DependencyFailed)
                    // therefore both project to the model's unchanged
                    // letter. The job-plane letters the sweep writes
                    // are the conformance surface (module header).
                    Some(DerivationStatus::DependencyFailed)
                    | Some(DerivationStatus::Cancelled) => ModelNode::NQueued,
                },
            );
        }
        let mut p = Projection {
            jobs,
            view,
            node_status,
            attempts,
        };
        p.normalize();
        Ok(p)
    }
}

// =======================================================================
// Named-run replays
// =======================================================================

struct NamedRun {
    run: &'static str,
    actions: fn() -> Vec<Act>,
}

fn s(v: &str) -> String {
    v.to_owned()
}

fn outs(os: &[&str]) -> BTreeSet<String> {
    os.iter().map(|o| (*o).to_string()).collect()
}

/// `happyPathRun`: merge -> probe-created job -> claim -> ingest ->
/// success consumption (node completes, job resolves success).
const HAPPY_PATH: NamedRun = NamedRun {
    run: "happyPathRun",
    actions: || {
        vec![
            Act::RecordWanted {
                b: s("b1"),
                d: s("d1"),
                outs: outs(&["o1"]),
            },
            Act::CreateJob {
                d: s("d1"),
                origin: ModelOrigin::OProbe,
                tenant: s("b1"),
            },
            Act::Claim {
                d: s("d1"),
                replica: s("r1"),
            },
            Act::ExecuteStep {
                d: s("d1"),
                o: s("o1"),
            },
            Act::ConsumeSuccess { d: s("d1") },
        ]
    },
};

/// `obsoleteOnProducedRun`: the live_061 by-other-means trace — the
/// executor ingests the wanted output, closes with an infra failure
/// (one charge, node released), the store short-circuit completes the
/// node with no open attempt, and the moot sweep resolves the
/// still-pending job under the alphabet's own letter. At the pre-fold
/// tree the final step reads spec `JObsolete` vs implementation
/// `JCancelled` — the conformance red this check exists to hold.
const OBSOLETE_ON_PRODUCED: NamedRun = NamedRun {
    run: "obsoleteOnProducedRun",
    actions: || {
        vec![
            Act::RecordWanted {
                b: s("b1"),
                d: s("d1"),
                outs: outs(&["o1"]),
            },
            Act::CreateJob {
                d: s("d1"),
                origin: ModelOrigin::OProbe,
                tenant: s("b1"),
            },
            Act::Claim {
                d: s("d1"),
                replica: s("r1"),
            },
            Act::ExecuteStep {
                d: s("d1"),
                o: s("o1"),
            },
            Act::ReportInfra { d: s("d1") },
            Act::CompleteReadyFromStore { d: s("d1") },
            Act::MootSweep,
        ]
    },
};

/// Replay one named run: have quint execute it (which also checks its
/// `.expect(...)` clauses) and emit the per-step states as an ITF
/// trace, then drive the implementation through the mirrored action
/// sequence and diff the projection against the model's state after
/// every step (including the init state).
fn replay_named_run(run: &NamedRun) -> Result<()> {
    let out = std::env::temp_dir().join(format!("rio-mbt-{}-{}", std::process::id(), run.run));
    std::fs::create_dir_all(&out).context("create the trace output dir")?;
    let out_pattern = out.join("trace_{seq}.itf.json");
    let output = Command::new("quint")
        .arg("test")
        .arg(spec_path())
        .args(["--main", "materializationJobBase"])
        .args(["--match", &format!("^{}$", run.run)])
        .args(["--max-samples", "1"])
        .arg("--out-itf")
        .arg(&out_pattern)
        .args(["--verbosity", "0"])
        .output()
        .context("spawn quint (is it on the PATH?)")?;
    ensure!(
        output.status.success(),
        "quint test --match=^{}$ failed (the run's .expect() clause may have regressed):\n{}\n{}",
        run.run,
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
    let mut trace: itf::Trace<Projection> =
        itf::trace_from_str(&json).context("decode the ITF trace into the projection")?;
    for state in &mut trace.states {
        state.value.normalize();
    }
    // Best-effort cleanup; a leftover tempdir is not a test failure.
    let _ = std::fs::remove_dir_all(&out);

    let actions = (run.actions)();
    ensure!(
        trace.states.len() == actions.len() + 1,
        "{}: the model's trace has {} states but the mirrored action sequence has {} actions \
         (+1 for init) — the run definition in materializationJob.qnt and the Rust mirror \
         have drifted",
        run.run,
        trace.states.len(),
        actions.len(),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    rt.block_on(async {
        let mut sys = MbtSystem::init().await?;
        diff_step(
            run.run,
            0,
            "init",
            &trace.states[0].value,
            &sys.project().await?,
        )?;
        for (i, action) in actions.into_iter().enumerate() {
            let label = format!("{action:?}");
            sys.apply(action)
                .await
                .with_context(|| format!("{}: step {} ({label})", run.run, i + 1))?;
            diff_step(
                run.run,
                i + 1,
                &label,
                &trace.states[i + 1].value,
                &sys.project().await?,
            )?;
        }
        Ok(())
    })
}

/// One post-step state comparison. The model's state is the oracle; a
/// mismatch is either a driver bug (the action mapping, the
/// projection, or the seeding is wrong) or a genuine
/// model<->implementation disagreement — classify before fixing
/// either.
fn diff_step(
    run: &str,
    index: usize,
    action: &str,
    spec: &Projection,
    implementation: &Projection,
) -> Result<()> {
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
fn mbt_run_happy_path() {
    replay_named_run(&HAPPY_PATH).unwrap();
}

// r[verify sched.materialize.obsolescence]
#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_obsolete_on_produced() {
    replay_named_run(&OBSOLETE_ON_PRODUCED).unwrap();
}

// =======================================================================
// The quint-connect driver (the simulation path)
// =======================================================================

/// The [`quint_connect::Driver`] quint-connect drives over the base
/// regime. Owns a current-thread tokio runtime (quint-connect's `step`
/// is sync; the actor API is async) and the [`MbtSystem`] (absent
/// until the first trace's init step).
struct MaterializationDriver {
    rt: tokio::runtime::Runtime,
    sys: Option<MbtSystem>,
}

impl MaterializationDriver {
    fn new() -> Self {
        Self {
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime"),
            sys: None,
        }
    }

    fn reset(&mut self) -> Result<()> {
        match self.sys.as_mut() {
            None => {
                let sys = self.rt.block_on(MbtSystem::init())?;
                self.sys = Some(sys);
            }
            Some(sys) => self.rt.block_on(sys.reset())?,
        }
        Ok(())
    }

    fn apply(&mut self, act: Act) -> quint_connect::Result {
        let sys = self.sys.as_mut().expect("init ran");
        self.rt.block_on(sys.apply(act))?;
        Ok(())
    }
}

impl quint_connect::Driver for MaterializationDriver {
    type State = Projection;

    fn step(&mut self, step: &quint_connect::Step) -> quint_connect::Result {
        self.dispatch_action(step)
    }
}

impl MaterializationDriver {
    /// The action dispatcher. Split out of `Driver::step` so the
    /// `switch!` expansion (an attributed block expression) sits in
    /// tail position. Nondet pick bindings carry the MODEL's pick
    /// names verbatim (the `switch!` lookup key is the identifier),
    /// so the snake-case lint is off for this fn.
    #[allow(non_snake_case)]
    fn dispatch_action(&mut self, step: &quint_connect::Step) -> quint_connect::Result {
        use quint_connect::switch;
        switch!(step {
            init => self.reset()?,
            step => self.reset()?,
            recordWantedRelation(b: String, d: String, outs: BTreeSet<String>) => {
                self.apply(Act::RecordWanted { b, d, outs })?;
            },
            legacyBuildArrives(b: String, d: String) => {
                self.apply(Act::LegacyArrives { b, d })?;
            },
            createJob(d: String, origin: ModelOrigin, b: String) => {
                self.apply(Act::CreateJob { d, origin, tenant: b })?;
            },
            dedupRefeed(d: String) => {
                self.apply(Act::DedupRefeed { d })?;
            },
            dedupPrunedUpgrade(d: String, b: String) => {
                self.apply(Act::DedupPrunedUpgrade { d, tenant: b })?;
            },
            claimJob(d: String, replica: String) => {
                self.apply(Act::Claim { d, replica })?;
            },
            executeStep(d: String, o: String) => {
                self.apply(Act::ExecuteStep { d, o })?;
            },
            consumeSuccess(d: String) => {
                self.apply(Act::ConsumeSuccess { d })?;
            },
            consumeUnobtainable(d: String, missing: BTreeSet<String>, refsMissing: bool) => {
                self.apply(Act::ConsumeUnobtainable { d, missing, refs_missing: refsMissing })?;
            },
            reportInfra(d: String) => {
                self.apply(Act::ReportInfra { d })?;
            },
            reportTransient(d: String) => {
                self.apply(Act::ReportTransient { d })?;
            },
            deferExpires(d: String) => {
                self.apply(Act::DeferExpires { d })?;
            },
            cancelOnZeroInterest(d: String) => {
                let _ = d;
                self.apply(Act::MootSweep)?;
            },
            obsoleteOnProduced(d: String) => {
                let _ = d;
                self.apply(Act::MootSweep)?;
            },
            completeReadyFromStore(d: String) => {
                self.apply(Act::CompleteReadyFromStore { d })?;
            },
            buildTerminal(b: String) => {
                self.apply(Act::BuildTerminal { b })?;
            },
            upstreamChange(d: String, o: String) => {
                self.apply(Act::UpstreamChange { d, o })?;
            },
            durableRelationChange(d: String) => {
                let _ = d;
                self.apply(Act::DurableRelationChange)?;
            },
            // Regime-disabled or outside the implemented walk arms:
            // each bail names the gate so a seed/model change that
            // reaches one is a loud red (the scope ladder).
            parkBackoffExpires(d: String) => {
                anyhow::bail!("parkBackoffExpires({d}): park-cycle arms are outside the \
                               implemented walk arms (scope ladder) — re-pin the seed or \
                               extend the driver");
            },
            parkReevaluate(d: String) => {
                anyhow::bail!("parkReevaluate({d}): park-cycle arms are outside the \
                               implemented walk arms (scope ladder)");
            },
            builderPull(d: String) => {
                anyhow::bail!("builderPull({d}): build-kind pulls are outside the \
                               implemented walk arms (scope ladder)");
            },
            dispatchFromSource(d: String) => {
                anyhow::bail!("dispatchFromSource({d}): the from-source build cycle is \
                               outside the implemented walk arms (scope ladder)");
            },
            establishCrashedAttempt(d: String) => {
                anyhow::bail!("establishCrashedAttempt({d}): ENABLE_CRASH is false in \
                               materializationJobBase — unreachable");
            },
            releasePins(d: String) => {
                anyhow::bail!("releasePins({d}): store-side pin release is below this \
                               driver's plane (scope ladder)");
            },
            storeGc(d: String) => {
                anyhow::bail!("storeGc({d}): ENABLE_GC is false in materializationJobBase \
                               — unreachable");
            },
            workerAborted(d: String) => {
                anyhow::bail!("workerAborted({d}): ENABLE_WORKER_ABORTED is false in \
                               materializationJobBase — unreachable");
            },
            failover => {
                anyhow::bail!("failover: MAX_GEN = 0 in materializationJobBase — unreachable");
            },
            redialChannel(replica: String) => {
                anyhow::bail!("redialChannel({replica}): no channel ever stales in \
                               materializationJobBase — unreachable");
            },
            staleTenureWriteDiscarded(d: String) => {
                anyhow::bail!("staleTenureWriteDiscarded({d}): ENABLE_STALE_TENURE is false \
                               in materializationJobBase — unreachable");
            },
            resolveApplied(d: String) => {
                anyhow::bail!("resolveApplied({d}): ENABLE_RESOLVE_FAULTS is false in \
                               materializationJobBase — unreachable");
            },
            resolveFenced(d: String) => {
                anyhow::bail!("resolveFenced({d}): ENABLE_RESOLVE_FAULTS is false in \
                               materializationJobBase — unreachable");
            },
            resolveErr(d: String) => {
                anyhow::bail!("resolveErr({d}): ENABLE_RESOLVE_FAULTS is false in \
                               materializationJobBase — unreachable");
            },
            walkObserveAny => {
                anyhow::bail!("walkObserveAny: ENABLE_WALK_FOLD is false in \
                               materializationJobBase — unreachable");
            },
            walkFold => {
                anyhow::bail!("walkFold: ENABLE_WALK_FOLD is false in \
                               materializationJobBase — unreachable");
            },
            stampObserveAny => {
                anyhow::bail!("stampObserveAny: the stamp plane is disabled in \
                               materializationJobBase — unreachable");
            },
            stampFold => {
                anyhow::bail!("stampFold: the stamp plane is disabled in \
                               materializationJobBase — unreachable");
            },
        })
    }
}

impl quint_connect::State<MaterializationDriver> for Projection {
    fn from_driver(driver: &MaterializationDriver) -> quint_connect::Result<Self> {
        let sys = driver
            .sys
            .as_ref()
            .context("projection requested before the trace's init step")?;
        driver.rt.block_on(sys.project())
    }

    fn from_spec(value: itf::Value) -> quint_connect::Result<Self> {
        let mut projection: Projection = itf::de::decode_value(value)
            .context("deserialize the model's state into the projection")?;
        projection.normalize();
        Ok(projection)
    }
}

/// Seeded random simulation against the base regime: quint generates
/// traces by walking `step` from `init` with `--mbt` action tracking,
/// the driver replays each one, and the projection is diffed after
/// every step. The seed is pinned (an input, not a measurement) so CI
/// is deterministic; delete it to explore locally and pin any seed
/// that finds a divergence.
#[quint_connect::quint_run(
    spec = "../docs/spec/models/materializationJob.qnt",
    main = "materializationJobBase",
    max_samples = 40,
    max_steps = 14,
    seed = "0x52494f4d"
)]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_simulation_base() -> impl quint_connect::Driver {
    MaterializationDriver::new()
}
