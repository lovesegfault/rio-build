//! Model-based testing: replay traces generated from the fencedWrites
//! Quint model (`docs/spec/models/fencedWrites.qnt`, main
//! `fencedWritesT1`) against the real fenced-write surface — pinned
//! per-replica `ServingGeneration`s, HELD-OPEN [`FencedTx`]
//! transactions across model steps, and the production chokepoints
//! (`claim_generation`, `begin_fenced`, `mint_assignment_upsert_in_tx`,
//! `close_assignment`, `update_resource_floor`,
//! `replay_status_batch_guarded`) — diffing the implementation's
//! projected state against the model's after every step.
//!
//! This generalizes the ONE hand-executed trace this file's sibling
//! pinned (`fenced_tx.rs::mint_statement_guard_blocks_generation_
//! regression` — the bug_261 READ-COMMITTED race) into model-driven
//! multi-trace replay on the [`mbt_materialization`] lineage: named
//! runs in `fencedWritesT1` are executed by `quint test` (which also
//! re-checks their `.expect` clauses), the emitted ITF trace is
//! decoded into the projection, and the mirrored action sequence is
//! driven through the production fns with the projection diffed after
//! every step (including init).
//!
//! Divergence record (R5, mechanism latitude): no `quint_connect`
//! simulation driver is wired for this plane — the model's own TLC
//! exhaustive check covers the state space, the fence surface's
//! conformance risk is in the CALL MAPPING (which these replays
//! exercise on the real PG), and the model's `step` draws its close
//! subject nondeterministically INSIDE `fencedClose`, which the
//! named-run form pins explicitly via `fencedCloseNamed`. The
//! OQ-12 acceptance rides the named runs + the strawman red below.
//!
//! # The driver mapping (model action -> production call)
//!
//! - `successorClaim(r)` -> [`SchedulerDb::claim_generation`] (the
//!   model's claim stamp; `replicaGen[r]` mirrors the new gen).
//! - `beginTx(r)` -> [`SchedulerDb::begin_fenced`] at `r`'s pinned
//!   generation. The model splits begin into snapshot (`beginTx`) +
//!   refusal (`fenceRefuse`); production decides at begin — the
//!   driver holds the outcome (`Open` keeps the LIVE transaction
//!   open across steps; `Fenced{floor}` is held for the matching
//!   `fenceRefuse` step to consume). A trace step that begins while
//!   admitted maps to a held-open capability — the READ COMMITTED
//!   window the model exists to study is replayed on a REAL
//!   connection, never simulated.
//! - `guardedUpsertCommit(r)` ->
//!   [`SchedulerDb::mint_assignment_upsert_in_tx`] on the HELD
//!   transaction + commit (rows == 1 <=> the model's apply arm;
//!   rows == 0 <=> the predicate-refused arm: rollback + refusal).
//! - `fencedCloseNamed(r, e)` -> [`FencedTx::close_assignment`] with
//!   ordinal `e`'s exec UUID on the HELD transaction + commit (the
//!   TB-4 exec key: closed count 1 <=> the model applied; 0 <=> the
//!   zero-rows arm).
//! - `floorWriteGreatest(r, v)` ->
//!   [`SchedulerDb::update_resource_floor`] (mem dimension carries
//!   the model value). SCOPE NOTE: production's floor writer opens
//!   its OWN fenced transaction, so the driver rolls back the held
//!   one first; a trace interleaving a successor claim between `r`'s
//!   `beginTx` and its `floorWriteGreatest` would diverge (model:
//!   old snapshot admits; production: fresh begin refuses) — the
//!   named runs do not take that shape, and a future run that wants
//!   it must drive the floor UPDATE through a held-transaction seam
//!   instead.
//! - `latchFailedPersist(r)` -> driver bookkeeping (the production
//!   transition is the actor's `latch_status_batch`, memory-only by
//!   definition: PG was down for the failed persist — there is
//!   nothing DB-visible to replay). The bookkept `StatusBatch`
//!   mirror (drv set, terminal status, latched exec UUIDs, enqueue
//!   instant) is exactly what the flush consumes.
//! - `advanceDerivation(r)` -> a fresh fenced transaction carrying
//!   [`SchedulerDb::update_derivation_status_in_tx`] (a NON-terminal
//!   status write — the migration-102 trigger stamps
//!   `status_changed_at`, which is what makes the later guarded
//!   flush refuse) + [`SchedulerDb::mint_assignment_upsert_in_tx`]
//!   (the resubmit re-mint rewriting `exec_id` in place).
//! - `flushReplayGuarded(r)` ->
//!   [`SchedulerDb::replay_status_batch_guarded`] with the bookkept
//!   batch (the m011 fix: stamp-guarded drv UPDATE + the
//!   `exec_id = ANY(latched)` close).
//! - `fenceRefuse(r)` / `answerRetryable(r)` -> consume the held
//!   `Fenced` outcome / assert the production refusal mapping is
//!   retryable (`actor_error_to_status(StaleGeneration).code() ==
//!   UNAVAILABLE` — sched.grpc.fence-retryable) and clear the
//!   bookkept refusal.
//!
//! # The projection
//!
//! Field names are the model's (ITF namespace
//! `fencedWritesT1::fencedWrites::`). Projected, all from PG except
//! the two abstraction-fn inputs named below:
//!
//! - `committedClaims` <- `SELECT generation FROM
//!   leader_generation_claims`.
//! - `activeGen`/`activeExec`/`activeOwner` <- the open
//!   (`pending`/`acknowledged`) assignments row; `activeExec` maps
//!   the row's UUID through the driver's mint-order ledger (model
//!   ordinals vs production UUIDs — the identity-plane mapping the
//!   materialization harness also keeps); `activeOwner` maps the
//!   row's generation through the claims table's `holder_id` (claims
//!   are unique per generation, so the minting replica is recoverable
//!   from PG).
//! - `resourceFloor` <- `derivations.floor_mem_bytes` (the driver
//!   denominates the model's floor in the mem dimension).
//! - `replicaGen` <- `MAX(generation) GROUP BY holder_id`, absent
//!   replicas at 0 (the model's never-led letter).
//!
//! Omitted: `txPhase`/`pendingRefusal` (driver transaction plumbing —
//! their behavioral faces are the begin/refuse/answer arms the replay
//! exercises), `execsMinted` (identity plane; the ordinal ledger is
//! its driver-side image), the bughunt-2 plane vars
//! (`atomicGen`/`latchedExec`/`latchedStale`/lifecycle — actor-memory
//! and lease-plane state with no PG image; `latchedExec` is exercised
//! through the flush mapping), and every latch/ghost (the oracle
//! plane: the model's `.expect` clauses check them model-side).
//!
//! # Determinism + clocks
//!
//! The named-run replays are fully deterministic. The one clock the
//! plane carries is PG's `status_changed_at` vs the latch age
//! ((cccccc) note: both comparands live in the PG domain by
//! construction — `LatchAge::at_replay_boundary` maps the monotonic
//! enqueue instant into PG inside the flush transaction, so pausing
//! tokio's clock would prove nothing here; the driver instead places
//! a real >=20ms gap between the latch and the advance so the
//! stamp-vs-cut comparison is strictly ordered, never
//! equal-timestamp).
//!
//! All tests are `#[ignore]`d: they shell out to `quint`, which the
//! default `nextest-rio-scheduler` sandbox does not provide. The
//! dedicated check (`mbt-rio-fence`, wired in `nix/quint.nix` next to
//! `mbt-rio-materialization`) stages the model into the nextest
//! workspace and runs them with `--run-ignored`. Locally:
//!
//! ```text
//! cargo nextest run -p rio-scheduler -E 'test(/mbt_fence/)' --run-ignored all
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context as _, Result, bail, ensure};
use itf::de::{As, Integer, Same};
use serde::Deserialize;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{AssignmentCloseStatus, FencedBegin, FencedTx, SchedulerDb, ServingGeneration};
use crate::state::{DerivationStatus, DrvHash, ResourceFloor};

/// The model's derivation (ONE active assignments row — the Tier-1
/// scope).
const DRV: &str = "mbtfence";

/// The spec path fallback for a local `cargo nextest` run. The
/// `mbt-rio-fence` check overrides it via `RIO_MBT_FENCE_SPEC_PATH`:
/// the test binary runs in a different sandbox than the one that
/// compiled it, so this baked path points at a tree that no longer
/// exists there.
const SPEC_ABS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/spec/models/fencedWrites.qnt"
);

fn spec_path() -> std::path::PathBuf {
    std::env::var_os("RIO_MBT_FENCE_SPEC_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(SPEC_ABS))
}

// =======================================================================
// The projection (the abstraction function)
// =======================================================================

/// The slice of the model's state the implementation observably
/// realizes. See the module header for what each field is projected
/// from and why the rest are omitted.
#[derive(Debug, PartialEq, Deserialize)]
struct Projection {
    #[serde(
        rename = "fencedWritesT1::fencedWrites::committedClaims",
        with = "As::<BTreeSet<Integer>>"
    )]
    committed_claims: BTreeSet<u64>,
    #[serde(
        rename = "fencedWritesT1::fencedWrites::activeGen",
        with = "As::<Integer>"
    )]
    active_gen: u64,
    #[serde(rename = "fencedWritesT1::fencedWrites::activeOwner")]
    active_owner: String,
    #[serde(
        rename = "fencedWritesT1::fencedWrites::activeExec",
        with = "As::<Integer>"
    )]
    active_exec: u64,
    #[serde(
        rename = "fencedWritesT1::fencedWrites::resourceFloor",
        with = "As::<Integer>"
    )]
    resource_floor: u64,
    #[serde(
        rename = "fencedWritesT1::fencedWrites::replicaGen",
        with = "As::<BTreeMap<Same, Integer>>"
    )]
    replica_gen: BTreeMap<String, u64>,
}

// =======================================================================
// Driver actions (the named-run mirrors)
// =======================================================================

/// One model action, in driver-ready form. The named runs build these
/// from constants mirroring the model's run definitions verbatim.
#[derive(Debug, Clone)]
enum Act {
    Claim {
        r: &'static str,
    },
    BeginTx {
        r: &'static str,
    },
    FenceRefuse {
        r: &'static str,
    },
    Upsert {
        r: &'static str,
    },
    CloseNamed {
        r: &'static str,
        e: u64,
    },
    Floor {
        r: &'static str,
        v: u64,
    },
    AnswerRetryable {
        r: &'static str,
    },
    Latch {
        r: &'static str,
    },
    Advance {
        r: &'static str,
    },
    Flush {
        r: &'static str,
    },
    /// The PRE-FIX flush, re-encoded as the strawman seam (OQ-12):
    /// an ABSOLUTE drv UPDATE (no stamp guard) + a DERIVATION-keyed
    /// close (no exec scope, no fence) — the merged_bug_011 /
    /// merged_bug_231 shape that never ships. Only the strawman
    /// acceptance test issues it.
    FlushStrawmanAbsolute,
}

/// The per-replica held begin outcome.
enum TxSlot {
    Idle,
    Open(FencedTx),
    Fenced { floor: i64 },
}

/// The bookkept status-outbox batch (the actor's `StatusBatch`
/// mirror — memory-only in production too).
struct LatchedBatch {
    status: DerivationStatus,
    exec_ids: Vec<Uuid>,
    enqueued_at: Instant,
}

// =======================================================================
// The system under test
// =======================================================================

struct FenceSystem {
    db: SchedulerDb,
    drv_hash: DrvHash,
    drv_id: Uuid,
    /// replica -> pinned serving generation (the model's replicaGen
    /// mirror; the projection reads the PG truth instead).
    gens: BTreeMap<&'static str, i64>,
    /// replica -> held begin outcome (model txPhase image).
    tx: BTreeMap<&'static str, TxSlot>,
    /// mint-order ledger: ordinal (the model's exec id) -> UUID.
    execs: Vec<Uuid>,
    latched: Option<LatchedBatch>,
}

impl FenceSystem {
    async fn init(test_db: &rio_test_support::TestDb) -> Result<FenceSystem> {
        let db = SchedulerDb::new(test_db.pool.clone());
        let drv_id = insert_test_derivation(&db, DRV).await?;
        // The model's init: r1 is the generation-1 leader
        // (committedClaims = {1}) and the resource floor starts at 1.
        ensure!(
            db.claim_generation(1, "r1").await?,
            "init claim for generation 1 must be fresh"
        );
        let floor = ResourceFloor {
            mem_bytes: 1,
            disk_bytes: 0,
            deadline_secs: 0,
        };
        ensure!(
            db.update_resource_floor(&DrvHash::from(DRV), &floor, gen_stamp(1))
                .await?
                .settled(),
            "init floor write must settle"
        );
        let mut gens = BTreeMap::new();
        gens.insert("r1", 1i64);
        gens.insert("r2", 0i64);
        let mut tx = BTreeMap::new();
        tx.insert("r1", TxSlot::Idle);
        tx.insert("r2", TxSlot::Idle);
        Ok(FenceSystem {
            db,
            drv_hash: DrvHash::from(DRV),
            drv_id,
            gens,
            tx,
            execs: Vec::new(),
            latched: None,
        })
    }

    fn gen_of(&self, r: &str) -> Result<i64> {
        let g = *self.gens.get(r).context("known replica")?;
        ensure!(g > 0, "{r} has never led — the model gates on gen > 0");
        Ok(g)
    }

    fn take_slot(&mut self, r: &'static str) -> TxSlot {
        self.tx
            .insert(r, TxSlot::Idle)
            .expect("slot seeded at init")
    }

    /// Ordinal -> UUID (the identity-plane map).
    fn exec_uuid(&self, ordinal: u64) -> Result<Uuid> {
        ensure!(ordinal >= 1, "model exec ids are 1-based");
        self.execs
            .get(usize::try_from(ordinal).expect("small ordinal") - 1)
            .copied()
            .with_context(|| format!("exec ordinal {ordinal} was never minted"))
    }

    /// UUID -> ordinal (the projection direction).
    fn exec_ordinal(&self, id: Uuid) -> Result<u64> {
        self.execs
            .iter()
            .position(|e| *e == id)
            .map(|i| u64::try_from(i).expect("small ordinal") + 1)
            .context("an assignments exec_id outside the mint ledger")
    }

    async fn apply(&mut self, act: Act) -> Result<()> {
        match act {
            Act::Claim { r } => {
                let next = self.max_claim().await? + 1;
                ensure!(
                    self.db.claim_generation(next, r).await?,
                    "successorClaim({r}): generation {next} already claimed"
                );
                self.gens.insert(r, next);
            }
            Act::BeginTx { r } => {
                ensure!(
                    matches!(self.tx.get(r), Some(TxSlot::Idle)),
                    "beginTx({r}): the model requires TxIdle"
                );
                let g = self.gen_of(r)?;
                let slot = match self.db.begin_fenced(gen_stamp(g)).await? {
                    FencedBegin::Open(ftx) => TxSlot::Open(ftx),
                    FencedBegin::Fenced { floor } => TxSlot::Fenced { floor },
                };
                self.tx.insert(r, slot);
            }
            Act::FenceRefuse { r } => match self.take_slot(r) {
                TxSlot::Fenced { floor } => {
                    let g = self.gen_of(r)?;
                    ensure!(
                        floor > g,
                        "fenceRefuse({r}): production refused at floor {floor} \
                         but the replica serves {g} — not a fence refusal"
                    );
                }
                TxSlot::Open(_) => bail!(
                    "fenceRefuse({r}): production ADMITTED where the model refuses \
                     — keying divergence"
                ),
                TxSlot::Idle => bail!("fenceRefuse({r}): no held begin outcome"),
            },
            Act::Upsert { r } => {
                let g = self.gen_of(r)?;
                let TxSlot::Open(mut ftx) = self.take_slot(r) else {
                    bail!("guardedUpsertCommit({r}): no held admitted transaction");
                };
                let exec = Uuid::now_v7();
                let rows = SchedulerDb::mint_assignment_upsert_in_tx(
                    ftx.conn(),
                    self.drv_id,
                    &format!("w-{r}"),
                    g,
                    exec,
                    None,
                )
                .await?;
                ensure!(
                    rows == 1,
                    "guardedUpsertCommit({r}): the statement guard refused \
                     (rows == {rows}) where the model's run applies"
                );
                ftx.commit().await?;
                self.execs.push(exec);
            }
            Act::CloseNamed { r, e } => {
                let TxSlot::Open(mut ftx) = self.take_slot(r) else {
                    bail!("fencedCloseNamed({r}, {e}): no held admitted transaction");
                };
                let subject = self.exec_uuid(e)?;
                let _closed = ftx
                    .close_assignment(subject, AssignmentCloseStatus::Completed)
                    .await?;
                ftx.commit().await?;
                // The applied-vs-zero-rows arm is adjudicated by the
                // projection diff (the row either cleared or it did
                // not) — no driver-side branch to get wrong.
            }
            Act::Floor { r, v } => {
                // Production's floor writer opens its OWN fenced
                // transaction — roll the held one back (it wrote
                // nothing; see the module-header scope note).
                let TxSlot::Open(_) = self.take_slot(r) else {
                    bail!("floorWriteGreatest({r}, {v}): no held admitted transaction");
                };
                let g = self.gen_of(r)?;
                let floor = ResourceFloor {
                    mem_bytes: v,
                    disk_bytes: 0,
                    deadline_secs: 0,
                };
                ensure!(
                    self.db
                        .update_resource_floor(&self.drv_hash, &floor, gen_stamp(g))
                        .await?
                        .settled(),
                    "floorWriteGreatest({r}, {v}): production fence refused where \
                     the model's run applies"
                );
            }
            Act::AnswerRetryable { r } => {
                // The production refusal mapping is the law under
                // test: a fence trip surfaces as the retryable
                // UNAVAILABLE family (sched.grpc.fence-retryable),
                // so no client gives up on a request the live leader
                // would accept.
                let status = crate::grpc::actor_guards::actor_error_to_status(
                    crate::actor::ActorError::StaleGeneration {
                        floor: 0,
                        serving: 0,
                    },
                );
                ensure!(
                    status.code() == tonic::Code::Unavailable,
                    "answerRetryable({r}): StaleGeneration mapped to {:?}, \
                     not UNAVAILABLE — the bug_393 terminal-refusal shape",
                    status.code()
                );
            }
            Act::Latch { r } => {
                let g = self.gen_of(r)?;
                let _ = g;
                let (_, exec_id) = self
                    .active_row()
                    .await?
                    .context("latchFailedPersist: the model requires an active exec")?;
                self.latched = Some(LatchedBatch {
                    status: DerivationStatus::Cancelled,
                    exec_ids: vec![exec_id],
                    enqueued_at: Instant::now(),
                });
                // Strict stamp ordering for the flush's age cut (see
                // the module-header clock note).
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Act::Advance { r } => {
                let g = self.gen_of(r)?;
                let FencedBegin::Open(mut ftx) = self.db.begin_fenced(gen_stamp(g)).await? else {
                    bail!("advanceDerivation({r}): fence refused the resubmit re-mint");
                };
                // The resubmit's status write — a NON-terminal value
                // change so the migration-102 trigger stamps
                // status_changed_at without closing assignments.
                SchedulerDb::update_derivation_status_in_tx(
                    ftx.conn(),
                    &self.drv_hash,
                    DerivationStatus::Running,
                    None,
                )
                .await?;
                let exec = Uuid::now_v7();
                let rows = SchedulerDb::mint_assignment_upsert_in_tx(
                    ftx.conn(),
                    self.drv_id,
                    &format!("w-{r}"),
                    g,
                    exec,
                    None,
                )
                .await?;
                ensure!(rows == 1, "advanceDerivation({r}): re-mint refused");
                ftx.commit().await?;
                self.execs.push(exec);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Act::Flush { r } => {
                let g = self.gen_of(r)?;
                let batch = self
                    .latched
                    .take()
                    .context("flushReplayGuarded: nothing latched")?;
                let outcome = self
                    .db
                    .replay_status_batch_guarded(
                        &[DRV],
                        batch.status,
                        &batch.exec_ids,
                        batch.enqueued_at,
                        gen_stamp(g),
                    )
                    .await?;
                ensure!(
                    matches!(
                        outcome,
                        crate::db::derivations::StatusReplay::Applied { .. }
                    ),
                    "flushReplayGuarded({r}): the fence refused the flush"
                );
            }
            Act::FlushStrawmanAbsolute => {
                let batch = self
                    .latched
                    .take()
                    .context("strawman flush: nothing latched")?;
                strawman_absolute_replay(self.db.pool(), batch.status).await?;
            }
        }
        Ok(())
    }

    async fn max_claim(&self) -> Result<i64> {
        let row: (Option<i64>,) =
            sqlx::query_as("SELECT MAX(generation) FROM leader_generation_claims")
                .fetch_one(self.db.pool())
                .await?;
        Ok(row.0.unwrap_or(0))
    }

    /// The open (`pending`/`acknowledged`) assignments row, if any.
    async fn active_row(&self) -> Result<Option<(i64, Uuid)>> {
        let row: Option<(i64, Uuid)> = sqlx::query_as(
            "SELECT generation, exec_id FROM assignments \
             WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')",
        )
        .bind(self.drv_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row)
    }

    async fn project(&self) -> Result<Projection> {
        let claims: Vec<(i64, String)> =
            sqlx::query_as("SELECT generation, holder_id FROM leader_generation_claims")
                .fetch_all(self.db.pool())
                .await?;
        let committed_claims: BTreeSet<u64> = claims
            .iter()
            .map(|(g, _)| u64::try_from(*g).expect("non-negative generation"))
            .collect();
        let mut replica_gen: BTreeMap<String, u64> =
            [("r1".to_owned(), 0u64), ("r2".to_owned(), 0u64)].into();
        for (g, holder) in &claims {
            let g = u64::try_from(*g).expect("non-negative generation");
            let slot = replica_gen.entry(holder.clone()).or_insert(0);
            if g > *slot {
                *slot = g;
            }
        }
        let (active_gen, active_exec, active_owner) = match self.active_row().await? {
            None => (0, 0, String::new()),
            Some((g, exec_id)) => {
                let owner = claims
                    .iter()
                    .find(|(cg, _)| *cg == g)
                    .map(|(_, h)| h.clone())
                    .context("an active row's generation has no claim — floor invariant broken")?;
                (
                    u64::try_from(g).expect("non-negative generation"),
                    self.exec_ordinal(exec_id)?,
                    owner,
                )
            }
        };
        let floor: i64 = sqlx::query_scalar(
            "SELECT COALESCE(floor_mem_bytes, 0) FROM derivations WHERE drv_hash = $1",
        )
        .bind(self.drv_hash.as_str())
        .fetch_one(self.db.pool())
        .await?;
        Ok(Projection {
            committed_claims,
            active_gen,
            active_owner,
            active_exec,
            resource_floor: u64::try_from(floor).expect("non-negative floor"),
            replica_gen,
        })
    }
}

fn gen_stamp(g: i64) -> ServingGeneration {
    ServingGeneration::stamp_from_claim(u64::try_from(g).expect("non-negative generation"))
}

/// The PRE-FIX flush (the strawman seam, never shipped): an ABSOLUTE
/// status UPDATE with no stamp guard plus a DERIVATION-keyed,
/// UNFENCED assignment close — built from the production close
/// renderer so the strawman exercises the exact statement family the
/// fix re-keyed.
async fn strawman_absolute_replay(pool: &sqlx::PgPool, status: DerivationStatus) -> Result<()> {
    sqlx::query("UPDATE derivations SET status = $1, updated_at = now()")
        .bind(status.as_str())
        .execute(pool)
        .await?;
    static CLOSE_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        crate::db::close_assignments_sql(
            "derivation_id IN (SELECT derivation_id FROM derivations)",
            1,
        )
    });
    sqlx::query_scalar::<_, i64>(CLOSE_SQL.as_str())
        .bind(AssignmentCloseStatus::Cancelled.as_str())
        .bind("cancelled")
        .fetch_one(pool)
        .await?;
    Ok(())
}

// =======================================================================
// Named-run replay
// =======================================================================

struct NamedRun {
    run: &'static str,
    actions: fn() -> Vec<Act>,
}

/// `fenceHappyPathRun`: mint -> exec-keyed close -> floor ratchet ->
/// successor claim -> fence refusal -> retryable answer.
const HAPPY_PATH: NamedRun = NamedRun {
    run: "fenceHappyPathRun",
    actions: || {
        vec![
            Act::BeginTx { r: "r1" },
            Act::Upsert { r: "r1" },
            Act::BeginTx { r: "r1" },
            Act::CloseNamed { r: "r1", e: 1 },
            Act::BeginTx { r: "r1" },
            Act::Floor { r: "r1", v: 2 },
            Act::Claim { r: "r2" },
            Act::BeginTx { r: "r1" },
            Act::FenceRefuse { r: "r1" },
            Act::AnswerRetryable { r: "r1" },
        ]
    },
};

/// `fenceOutboxReplayRun`: the merged_bug_011 acceptance shape — the
/// latch carries E1, the resubmit re-mints E2, and the guarded flush
/// applies nothing (no stale apply; the close lands only on latched
/// execs, and E1 is no longer a row).
const OUTBOX_REPLAY: NamedRun = NamedRun {
    run: "fenceOutboxReplayRun",
    actions: || {
        vec![
            Act::BeginTx { r: "r1" },
            Act::Upsert { r: "r1" },
            Act::Latch { r: "r1" },
            Act::Advance { r: "r1" },
            Act::Flush { r: "r1" },
        ]
    },
};

/// `fenceForeignSettleRun`: the failover settle (TB-4 face (b)) — a
/// live successor closes the exec its predecessor minted.
const FOREIGN_SETTLE: NamedRun = NamedRun {
    run: "fenceForeignSettleRun",
    actions: || {
        vec![
            Act::BeginTx { r: "r1" },
            Act::Upsert { r: "r1" },
            Act::Claim { r: "r2" },
            Act::BeginTx { r: "r2" },
            Act::CloseNamed { r: "r2", e: 1 },
        ]
    },
};

/// Fetch the model's ITF trace for one named run via `quint test`
/// (which also re-checks the run's `.expect` clauses).
fn model_trace(run: &str) -> Result<Vec<Projection>> {
    let out = std::env::temp_dir().join(format!("rio-mbt-fence-{}-{}", std::process::id(), run));
    std::fs::create_dir_all(&out).context("create the trace output dir")?;
    let out_pattern = out.join("trace_{seq}.itf.json");
    let output = Command::new("quint")
        .arg("test")
        .arg(spec_path())
        .args(["--main", "fencedWritesT1"])
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
/// strawman seam); `None` replays the production mapping throughout.
fn replay(run: &NamedRun, override_at: Option<(usize, Act)>) -> Result<()> {
    let states = model_trace(run.run)?;
    let mut actions = (run.actions)();
    ensure!(
        states.len() == actions.len() + 1,
        "{}: the model's trace has {} states but the mirrored action sequence has {} actions \
         (+1 for init) — the run definition in fencedWrites.qnt and the Rust mirror have drifted",
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
        let test_db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let mut sys = FenceSystem::init(&test_db).await?;
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
fn mbt_fence_run_happy_path() {
    replay(&HAPPY_PATH, None).unwrap();
}

// r[verify sched.lease.generation-fence+3]
#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_fence_run_outbox_replay() {
    replay(&OUTBOX_REPLAY, None).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_fence_run_foreign_settle() {
    replay(&FOREIGN_SETTLE, None).unwrap();
}

/// The OQ-12 acceptance red (W13-AS, strawman half): the PRE-FIX
/// absolute replay — no stamp guard, derivation-keyed unfenced close
/// — at the flush step of the merged_bug_011 trace. The per-step diff
/// MUST red at exactly that step: the model holds the successor exec
/// (E2) open; the strawman kills it. A green here means the harness
/// can no longer detect the regression class it was built for.
#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_fence_strawman_absolute_replay_reds_at_flush() {
    let err = replay(&OUTBOX_REPLAY, Some((4, Act::FlushStrawmanAbsolute)))
        .expect_err("the strawman absolute replay must diverge from the model");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("state divergence after step 5"),
        "the divergence must land at the flush step (step 5); got:\n{msg}"
    );
    assert!(
        msg.contains("active_gen: 0"),
        "the divergence must show the strawman closing the successor exec; got:\n{msg}"
    );
}
