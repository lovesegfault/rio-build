//! OP-1 (MBT-1): named-run trace replay for `openAttempts.qnt` — the
//! densest un-MBT'd calibration corpus (10 families across four
//! waves) gets its conformance instrument. The `openAttemptsReplay`
//! module's runs (and, for the OQ-12 acceptance red, the
//! `openAttemptsNoncelessMint` calibration's run) execute under
//! `quint test --out-itf`; this driver mirrors each action list
//! through the PRODUCTION materialization-claim lane and diffs a
//! projected state after every step.
//!
//! # The driven lane (scope ladder, stated honestly)
//!
//! The model is ONE abstract open attempt; this driver realizes it on
//! the MATERIALIZATION claim lane end-to-end through the production
//! chokepoints:
//!
//! - `mintDelivered` / `mintResponseLost` → `handle_pull_assignment`
//!   (kind=Materialization, the store's fresh-claim shape: a claim
//!   nonce rides the request exactly as `poll_and_claim` sends it —
//!   bug_251's fix persists it in the mint's transaction,
//!   `assignments.claim_nonce`). "Response lost" is the network fact:
//!   the driver bookkeeps it; the scheduler-side state is identical.
//! - `resumeRedeliver` → a second `handle_pull_assignment` presenting
//!   the same nonce (the store never learned the exec id — that is
//!   what "lost" means); the kernel's `redelivery_credential_ok`
//!   matches `assignments.claim_nonce` and the answer is the
//!   idempotent re-pull (`DeliverExisting` — the SAME exec id, no second mint — quantifier: census(test: mbt_openattempts_run_sweep_window)).
//!   The driver asserts exec-id identity at the resume step.
//! - `consumeCloseOk` → `handle_report_outcome` with a
//!   `MaterializationOutcome::Success` (the settled close: charge-free,
//!   companion releases the claim).
//! - `sweepEstablish` → `tick_sweep_open_pull_attempts` under
//!   `dag_authority()`, with the attempt expired through the
//!   PRODUCTION seam: `materialization.attempt_deadline_secs = 1` +
//!   `establishment_report_slack = 0` and a real >1 s gap — no
//!   `assigned_at` backdating, no paused clock ((cccccc) verdict: the
//!   expiry comparison is wall/PG-domain — `epoch_now()` vs the
//!   PG-stamped `assigned_at` — so pausing tokio's clock would prove
//!   nothing; the real gap is the honest drive).
//!
//! Bound-but-not-yet-replayed actions (their `Mirrors:` bindings land
//! with this harness; runs are additive later): the cancel/outbox
//! plane (`cancelPersistOk/Err`, `flushOk/Err` — TLC-verified laws
//! whose driver needs the build-cancel scaffold), `failover`, the
//! fence plane (`goneAnswered`, `confirmProbeReads`,
//! `resubmitReready`) and the store-client standing arms
//! (`freshClaimSkipsLive`, `claimRefused*` — store-side in-memory
//! ledger state, `standing_effect`'s domain).
//!
//! # Projection (the abstraction function)
//!
//! - `attempt` ← PG: an open `assignments` row (`pending` /
//!   `acknowledged`) is `AOpen`; closed with a `drv_attempts`
//!   attempt-event row is `AClosedCharged`; closed without one is
//!   `AClosedFree`; never-minted is `ANone`.
//! - `node` ← PG `derivations.status`: `cancelled` → `NCancelled`;
//!   row absent → `NAbsent`; every live status → `NWantedLive`.
//! - `viewClaimed` ← the actor's in-memory job view holder
//!   (`materialization_jobs` episode `Held` — the model var IS the
//!   in-memory holder; this test module lives inside the actor module
//!   tree and reads it directly).
//! - `clientHoldsResume` ← PG: the OPEN assignment row's
//!   `claim_nonce IS NOT NULL` (the bug_251 credential; a closed
//!   attempt holds nothing).
//! - `cancelDurable` ← PG: `derivations.status = 'cancelled'`.
//!
//! Omitted from the diff (bookkeeping or out-of-lane, per the
//! materialization-MBT precedent of never diffing pure bookkeeping):
//! `outbox` (actor-private memory; its observable is the depth gauge),
//! `responseLost` (a network fact the driver bookkeeps), `goneFenced`/
//! `goneLicensed` (the build-lane confirm-fence plane — this driver's
//! lane carries no executor token), and all eight oracle latches (the
//! model's own `.expect` clauses check them model-side).
//!
//! All tests are `#[ignore]`d: they shell out to `quint`, which the
//! default `nextest-rio-scheduler` sandbox does not provide. The
//! dedicated check (`mbt-rio-openattempts`, wired in `nix/quint.nix`
//! next to `mbt-rio-fence`) stages the model + the nonceless-mint
//! calibration into the nextest workspace and runs them with
//! `--run-ignored`. Locally:
//!
//! ```text
//! cargo nextest run -p rio-scheduler -E 'test(/mbt_openattempts/)' --run-ignored all
//! ```

use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use uuid::Uuid;

use super::*;
use crate::state::JobOrigin;

/// The model's one derivation, realized as a production drv hash.
const DRV: &str = "mbtoa";
/// The store replica identity presenting the claim.
const REPLICA: &str = "r1";
/// The one wanted output.
const OUT: &str = "o1";

/// The spec path fallback for a local `cargo nextest` run. The
/// `mbt-rio-openattempts` check overrides it via
/// `RIO_MBT_OA_SPEC_PATH`: the test binary runs in a different
/// sandbox than the one that compiled it, so this baked path points
/// at a tree that no longer exists there.
const SPEC_ABS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/spec/models/openAttempts.qnt"
);

fn spec_path() -> std::path::PathBuf {
    std::env::var_os("RIO_MBT_OA_SPEC_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(SPEC_ABS))
}

/// The bug_251 pre-fix calibration lives beside the main model (the
/// staged layout preserves the models/calibration shape so its
/// `import ... from "../openAttempts"` resolves).
fn calib_nonceless_path() -> std::path::PathBuf {
    spec_path()
        .parent()
        .expect("the spec path has a models dir parent")
        .join("calibration/openattempts-nonceless-mint.qnt")
}

fn out_path() -> String {
    test_store_path(&format!("{DRV}-{OUT}"))
}

// =======================================================================
// The projection (the abstraction function)
// =======================================================================

/// The model's `AttemptState` (ITF tag-decoded; the variant names are
/// the model's exact constructors, prefix and all — the tag is the
/// decode key).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelAttempt {
    ANone,
    AOpen,
    AClosedCharged,
    AClosedFree,
}

/// The model's `NodeState`.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelNodeState {
    NWantedLive,
    NCancelled,
    NAbsent,
}

/// The slice of the model's state the implementation observably
/// realizes. See the module header for what each field is projected
/// from and why the rest are omitted. `openAttempts` is imported
/// WITHOUT constants by both run hosts (`openAttemptsReplay` and the
/// `openAttemptsNoncelessMint` calibration), so the ITF vars carry
/// their BARE names — one decoder serves both namespaces.
#[derive(Debug, PartialEq, Deserialize)]
struct Projection {
    attempt: ModelAttempt,
    node: ModelNodeState,
    #[serde(rename = "viewClaimed")]
    view_claimed: bool,
    #[serde(rename = "clientHoldsResume")]
    client_holds_resume: bool,
    #[serde(rename = "cancelDurable")]
    cancel_durable: bool,
}

// =======================================================================
// Driver actions (the named-run mirrors)
// =======================================================================

/// One model action, in driver-ready form. The named runs build these
/// from constants mirroring the run definitions verbatim.
#[derive(Debug, Clone, Copy)]
enum Act {
    /// `mintDelivered`: fresh claim, response reaches the store.
    MintDelivered,
    /// `mintResponseLost`: fresh claim, response bookkept lost.
    MintResponseLost,
    /// The bug_251 PRE-FIX mint (the strawman seam, OQ-12): the same
    /// production entrypoint with NO claim nonce — the shape the fix
    /// retired. Only the acceptance red-holder issues it.
    MintResponseLostNonceless,
    /// `resumeRedeliver`: present the same nonce; expect the
    /// idempotent re-pull of the same exec (the identity is asserted
    /// in the apply arm — the module doc carries the bound law).
    ResumeRedeliver,
    /// `consumeCloseOk`: report a successful materialization outcome.
    ConsumeCloseOk,
    /// `sweepEstablish`: real >deadline+slack gap, then the
    /// establishment sweep under DAG authority.
    SweepEstablish,
}

struct NamedRun {
    spec: fn() -> std::path::PathBuf,
    main: &'static str,
    run: &'static str,
    actions: &'static [Act],
}

const HAPPY_PATH: NamedRun = NamedRun {
    spec: spec_path,
    main: "openAttemptsReplay",
    run: "oaHappyPathRun",
    actions: &[Act::MintDelivered, Act::ConsumeCloseOk],
};

/// The OQ-12 acceptance, GREEN half: the bug_251 redeliver-first
/// window — lost response, credential-honored redelivery, then the
/// legitimate establishment charge.
const SWEEP_WINDOW: NamedRun = NamedRun {
    spec: spec_path,
    main: "openAttemptsReplay",
    run: "oaSweepWindowRun",
    actions: &[
        Act::MintResponseLost,
        Act::ResumeRedeliver,
        Act::SweepEstablish,
    ],
};

// =======================================================================
// The system under test
// =======================================================================

struct OaSystem {
    db: rio_test_support::TestDb,
    actor: DagActor,
    _store_task: tokio::task::JoinHandle<()>,
    /// The store's claim nonce (one fresh claim per trace — the
    /// production `poll_and_claim` mints one per fresh presentation).
    nonce: Uuid,
    /// Minted exec id (driver bookkeeping; the resume step asserts
    /// the redelivery returns the same one).
    exec: Option<Uuid>,
}

impl OaSystem {
    async fn init() -> Result<OaSystem> {
        let db = rio_test_support::TestDb::new(&MIGRATOR).await;
        seed_default_tenant(&db.pool).await;
        let (_store, store_client, store_task) =
            rio_test_support::grpc::spawn_mock_store_with_client().await?;
        // The expiry seam (module header): 1 s materialization
        // deadline, zero report slack — the sweep step's real >1 s
        // gap expires the attempt through the production window
        // arithmetic.
        let cfg = DagActorConfig {
            establishment_report_slack: std::time::Duration::ZERO,
            materialization: crate::config::MaterializationConfig {
                attempt_deadline_secs: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let plumbing = DagActorPlumbing {
            store_client: Some(store_client),
            service_signer: Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            ))),
            ..Default::default()
        };
        let mut actor = DagActor::new(SchedulerDb::new(db.pool.clone()), cfg, plumbing);
        // Seed the model's universe: one merged node wanting one
        // output, one pending materialization job (the claimable
        // state `init` assumes — node NWantedLive, attempt ANone).
        let build_id = Uuid::new_v4();
        let mut node = make_node(DRV);
        node.output_names = vec![OUT.to_string()];
        node.expected_output_paths = vec![out_path()];
        node.wanted_output_names = vec![OUT.to_string()];
        let req = MergeDagRequest {
            build_id,
            tenant_id: Some(DEFAULT_TEST_TENANT),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![node],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: Some("harness-tenant-jwt".into()),
        };
        actor
            .handle_merge_dag(req)
            .await
            .map_err(|e| anyhow::anyhow!("seed merge failed: {e:?}"))?;
        ensure!(
            actor
                .create_materialization_job(
                    &DrvHash::from(DRV),
                    JobOrigin::CacheOpportunity,
                    Some(build_id),
                    None,
                )
                .await,
            "seed materialization job did not apply"
        );
        Ok(OaSystem {
            db,
            actor,
            _store_task: store_task,
            nonce: Uuid::new_v4(),
            exec: None,
        })
    }

    /// One pull through the production handler, with the store's
    /// claim shape (`kind=Materialization`, replica identity, the
    /// nonce when the lane carries one).
    async fn pull(&mut self, claim_nonce: Option<Uuid>) -> Result<PullOutcome> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.actor
            .handle_pull_assignment(
                DRV.to_string(),
                Some(DRV.to_string()),
                rio_evidence_kernel::pull::PullKind::Materialization,
                Some(REPLICA.to_string()),
                None,
                claim_nonce,
                false,
                None,
                tx,
            )
            .await;
        rx.await
            .context("actor dropped the pull reply")?
            .map_err(|e| anyhow::anyhow!("pull rejected: {e:?}"))
    }

    /// Apply one mirrored action through the production surfaces.
    async fn apply(&mut self, act: Act) -> Result<()> {
        match act {
            Act::MintDelivered | Act::MintResponseLost => {
                match self.pull(Some(self.nonce)).await? {
                    PullOutcome::Deliver(a) => {
                        self.exec = Some(a.exec_id.parse().context("exec id parses")?);
                        Ok(())
                    }
                    other => bail!("mint: expected Deliver, got {other:?}"),
                }
            }
            Act::MintResponseLostNonceless => {
                // The PRE-FIX shape: no nonce in the mint transaction
                // — nothing for a redelivery to match.
                match self.pull(None).await? {
                    PullOutcome::Deliver(a) => {
                        self.exec = Some(a.exec_id.parse().context("exec id parses")?);
                        Ok(())
                    }
                    other => bail!("nonceless mint: expected Deliver, got {other:?}"),
                }
            }
            Act::ResumeRedeliver => {
                // The store presents its NONCE (it never learned the
                // exec id — that is what a lost response means).
                let minted = self.exec.context("no exec bookkept before the resume")?;
                match self.pull(Some(self.nonce)).await? {
                    PullOutcome::Deliver(a) => {
                        let exec: Uuid = a.exec_id.parse().context("exec id parses")?;
                        ensure!(
                            exec == minted,
                            "redelivery law: expected the SAME exec {minted}, got {exec} \
                             (a second mint is the merged_bug_096 clobber)"
                        );
                        Ok(())
                    }
                    other => bail!(
                        "resumeRedeliver: the model redelivers against the persisted \
                         credential but the implementation answered {other:?} (the \
                         bug_251 pre-fix shape: a lost-response claim that cannot be \
                         re-delivered settles through the CHARGED establishment window)"
                    ),
                }
            }
            Act::ConsumeCloseOk => {
                let exec = self.exec.context("no exec bookkept before the report")?;
                let outcome = rio_proto::types::MaterializationOutcome {
                    outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
                        rio_proto::types::materialization_outcome::Success {
                            ingested_paths: vec![out_path()],
                            verified_paths: vec![],
                            verified_tenants: vec![],
                        },
                    )),
                };
                let mut payload = pull_payload(rio_proto::types::BuildResult::default());
                payload.materialization_outcome = Some(outcome);
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.actor
                    .handle_report_outcome(exec, Some(DRV.to_string()), payload, tx)
                    .await;
                rx.await
                    .context("actor dropped the report reply")?
                    .map_err(|e| anyhow::anyhow!("consume report rejected: {e:?}"))
            }
            Act::SweepEstablish => {
                // Real gap past deadline (1 s) + slack (0): the
                // production expiry arithmetic, no backdating.
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                let authority = self
                    .actor
                    .dag_authority()
                    .context("bare test actor is always leader")?;
                self.actor.tick_sweep_open_pull_attempts(&authority).await;
                Ok(())
            }
        }
    }

    /// Project the production state per the module header.
    async fn project(&self) -> Result<Projection> {
        let pool = &self.db.pool;
        let drv_id: Option<Uuid> =
            sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                .bind(DRV)
                .fetch_optional(pool)
                .await
                .context("derivation id read")?;
        let (attempt, client_holds_resume) = match drv_id {
            None => (ModelAttempt::ANone, false),
            Some(id) => {
                let open_nonce: Option<Option<Uuid>> = sqlx::query_scalar(
                    "SELECT claim_nonce FROM assignments \
                     WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')",
                )
                .bind(id)
                .fetch_optional(pool)
                .await
                .context("open assignment read")?;
                if let Some(nonce) = open_nonce {
                    (ModelAttempt::AOpen, nonce.is_some())
                } else {
                    let ever: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM assignments WHERE derivation_id = $1",
                    )
                    .bind(id)
                    .fetch_one(pool)
                    .await
                    .context("assignment count")?;
                    let charged: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM drv_attempts \
                         WHERE derivation_id = $1 AND event_kind = 'attempt'",
                    )
                    .bind(id)
                    .fetch_one(pool)
                    .await
                    .context("charge rows read")?;
                    let attempt = if charged > 0 {
                        ModelAttempt::AClosedCharged
                    } else if ever > 0 {
                        ModelAttempt::AClosedFree
                    } else {
                        ModelAttempt::ANone
                    };
                    (attempt, false)
                }
            }
        };
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(DRV)
                .fetch_optional(pool)
                .await
                .context("derivation status read")?;
        let node = match status.as_deref() {
            None => ModelNodeState::NAbsent,
            Some("cancelled") => ModelNodeState::NCancelled,
            Some(_) => ModelNodeState::NWantedLive,
        };
        let cancel_durable = matches!(status.as_deref(), Some("cancelled"));
        let view_claimed = self
            .actor
            .materialization_jobs
            .get(&DrvHash::from(DRV))
            .is_some_and(|e| {
                matches!(
                    e.episode(),
                    crate::actor::materialize::ClaimEpisode::Held { .. }
                )
            });
        Ok(Projection {
            attempt,
            node,
            view_claimed,
            client_holds_resume,
            cancel_durable,
        })
    }
}

// =======================================================================
// Named-run replay
// =======================================================================

/// Fetch the model's ITF trace for one named run via `quint test`
/// (which also re-checks the run's `.expect` clauses).
fn model_trace(spec: &std::path::Path, main: &str, run: &str) -> Result<Vec<Projection>> {
    let out = std::env::temp_dir().join(format!("rio-mbt-oa-{}-{}", std::process::id(), run));
    std::fs::create_dir_all(&out).context("create the trace output dir")?;
    let out_pattern = out.join("trace_{seq}.itf.json");
    let output = Command::new("quint")
        .arg("test")
        .arg(spec)
        .args(["--main", main])
        .args(["--match", &format!("^{run}$")])
        .args(["--max-samples", "1"])
        .arg("--out-itf")
        .arg(&out_pattern)
        .args(["--verbosity", "0"])
        .output()
        .context("spawn quint (is it on the PATH?)")?;
    ensure!(
        output.status.success(),
        "quint test --match=^{}$ failed (the run's .expect() clause may have regressed):\n{}\n{}",
        run,
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
    let _ = std::fs::remove_dir_all(&out);
    Ok(trace.states.into_iter().map(|s| s.value).collect())
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

/// Replay one named run against the live tree, diffing after every
/// step (including init). `override_at` swaps ONE driver action (the
/// strawman seam, the mbt_fence form); `None` replays the production
/// mapping throughout.
fn replay(run: &NamedRun, override_at: Option<(usize, Act)>) -> Result<()> {
    let states = model_trace(&(run.spec)(), run.main, run.run)?;
    let mut actions: Vec<Act> = run.actions.to_vec();
    ensure!(
        states.len() == actions.len() + 1,
        "{}: the model's trace has {} states but the mirrored action sequence has {} actions \
         (+1 for init) — the run definition and the Rust mirror have drifted",
        run.run,
        states.len(),
        actions.len(),
    );
    if let Some((at, act)) = override_at {
        actions[at] = act;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    rt.block_on(async {
        let mut sys = OaSystem::init().await?;
        diff_step(run.run, 0, "init", &states[0], &sys.project().await?)?;
        for (i, action) in actions.into_iter().enumerate() {
            let label = format!("{action:?}");
            sys.apply(action)
                .await
                .with_context(|| format!("{}: step {} ({label})", run.run, i + 1))?;
            diff_step(
                run.run,
                i + 1,
                &label,
                &states[i + 1],
                &sys.project().await?,
            )?;
        }
        Ok(())
    })
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_openattempts_run_happy_path() {
    replay(&HAPPY_PATH, None).unwrap();
}

/// The OQ-12 acceptance, GREEN half: the bug_251 trace — lost mint
/// response, credential-honored redelivery (same exec, asserted in
/// the driver's resume arm), then the legitimate establishment
/// charge of the genuinely expired attempt.
#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_openattempts_run_sweep_window() {
    replay(&SWEEP_WINDOW, None).unwrap();
}

/// The OQ-12 acceptance, RED half #1 (W13-AW, the mbt_fence strawman
/// form — the permanent red-holder): the live sweep-window trace with
/// the mint OVERRIDDEN to the bug_251 PRE-FIX nonceless shape
/// (`claim_nonce = None` through the same production entrypoint). The
/// per-step diff MUST red at exactly the mint step on the projected
/// credential: the model persists it (`clientHoldsResume` TRUE — the
/// fix's transaction), the strawman leaves `assignments.claim_nonce`
/// NULL. A green here means the harness can no longer observe the
/// regression class it was built for.
///
/// Divergence record (the successor brief's sketch re-derived at the
/// tree): the brief placed this red at the SWEEP step as a charge-row
/// divergence — refuted: the establishment kernel
/// (`establish_expired_attempt`) consumes (kind, node, probe,
/// verifiable) and NO credential input, so an expired un-redelivered
/// attempt charges identically in both worlds; the model's sweep
/// guard `not(responseLost and clientHoldsResume)` is the
/// WINDOW-ORDERING abstraction (store redelivery << establishment
/// window), not a sweep-time conditional. The pre-fix calibration's
/// own header states the real mechanism ("the kernel's
/// colliding-identity refusal answered NotYetReady ... settled
/// through the CHARGED establishment window") — the observable
/// divergences are the persisted credential (this red) and the
/// refused redelivery (the companion red below).
#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_openattempts_nonceless_mint_reds_at_credential() {
    let err = replay(&SWEEP_WINDOW, Some((0, Act::MintResponseLostNonceless)))
        .expect_err("the nonceless mint must diverge from the live model");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("state divergence after step 1"),
        "the divergence must land at the mint step (step 1); got:\n{msg}"
    );
    assert!(
        msg.contains("client_holds_resume: true") && msg.contains("client_holds_resume: false"),
        "the divergence must show the persisted-vs-absent credential; got:\n{msg}"
    );
}

/// The OQ-12 acceptance, RED half #2 (the behavioral consequence,
/// pinned through the production pull surface): after a NONCELESS
/// mint with a lost response, the store's credential presentation is
/// REFUSED — `redelivery_credential_ok` has no `assignments.claim_nonce`
/// to match, the exact pre-fix shape whose establishment then charged
/// a no-fault attempt (`oa251AcceptanceRun` pins the model-side
/// consequence: `chargedNoFault`). The production mint's redelivery
/// on the same presentation is the SWEEP_WINDOW green half.
#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_openattempts_nonceless_redelivery_refused() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    rt.block_on(async {
        let mut sys = OaSystem::init().await.unwrap();
        sys.apply(Act::MintResponseLostNonceless).await.unwrap();
        let err = sys
            .apply(Act::ResumeRedeliver)
            .await
            .expect_err("a nonceless mint's credential presentation must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("the implementation answered"),
            "the refusal must surface as a non-Deliver answer; got:\n{msg}"
        );
    });
}
