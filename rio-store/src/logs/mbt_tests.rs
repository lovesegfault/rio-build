//! Model-based testing: replay traces generated from the LogService
//! Quint model (`docs/spec/models/logService.qnt`) against the real
//! `rio-store/src/logs/` implementation, diffing the implementation's
//! projected state against the model's after every step.
//!
//! The model proves the session/chunk/dedup protocol is correct; this
//! module proves the code is that protocol. The per-regime `quint verify`
//! checks in `nix/quint.nix` explore the model's state space; the
//! `mbt-rio-logservice` check (same file) replays concrete traces from
//! that space through [`gate::check_append_open`],
//! [`sessions::acquire`], [`IngestSession::accept`] /
//! [`IngestSession::cut`], the `drv_log_chunks` manifest,
//! [`tail::read_chunk`]'s dedup walk, and [`sweep::sweep_expired_logs`]
//! — so a model↔implementation drift surfaces as a red check with a
//! per-step state diff instead of as a review-time judgment call.
//!
//! # Architecture
//!
//! [`MbtSystem`] owns an ephemeral PostgreSQL database
//! (`rio_test_support::TestDb` — the same `drv_executions` /
//! `assignments` / `drv_log_chunks` schema production runs against), an
//! in-memory [`MemoryLogChunkStore`] standing in for S3, and one
//! [`ExecHarness`] per model execution holding the real
//! [`IngestSession`] objects. Each model action maps onto the
//! implementation calls the corresponding production code path makes
//! (named in each method's doc comment); the projection maps the real
//! rows + the real session objects back onto the model's variable
//! shape. The `#[quint_run]` simulation and the hand-rolled named-run
//! replays drive the same [`MbtSystem`] and diff the same
//! [`Projection`].
//!
//! # What is drivable, and what is scoped out
//!
//! The model spans three components. Only one of them is the subject:
//!
//! - **Store-side** (`rio-store/src/logs/` — the subject): the open
//!   gate, the ingest session, the chunk cut + manifest, the read path,
//!   the TTL sweep. Driven by calling the real functions; projected
//!   from the real PG rows and the real `IngestSession` fields.
//! - **Scheduler-side** (`dispatch`, `recordFinalLineCount`,
//!   `executionExpires`): PG rows the scheduler writes and the store
//!   reads. The driver writes the rows (mirroring the scheduler's
//!   INSERT/UPDATE); the projection reads them back **through the
//!   store's own accessors** (`sealed_final_line_count`, the gate's
//!   claimed-exec lookup), so the seeding and the store's
//!   interpretation of it are both checked.
//! - **Builder-side** (`produceLine`, `buildFinishes`, `deliverAck`,
//!   `uploaderAbandons`, the `producedEnd` / `ackedBelow` / `sentBelow`
//!   / `uploader` variables): `rio-builder/src/log_upload.rs`'s
//!   uploader, which is not in this crate. The driver mirrors just
//!   enough of it to construct the batches the uploader would send
//!   (`producedEnd`, `sentBelow`); the rest is a documented no-op and
//!   none of it is projected. Conformance of the uploader itself is
//!   out of scope here.
//!
//! Per-action dispositions that are not the obvious "call the real
//! function":
//!
//! - **`sessionAborts`** — the implementation's abort path drops the
//!   whole `IngestShared` from the registry (the deregistration
//!   scopeguard in `service.rs`); there is no API that empties a
//!   session's buffer in place, and adding one for the test would mean
//!   testing the test. The driver keeps the real session object (its
//!   `high_water_line` and ceiling stay projectable) and projects
//!   `buf = Set()` from a bookkeeping flag. An aborted session is
//!   inert in the model (no accept, no refresh, and `cutChunk`
//!   requires a non-empty buf), so the retained real buffer is never
//!   observed again. The abort triggers themselves (3 consecutive cut
//!   failures, the stale-buffer clock) are unit-tested in `ingest.rs`.
//! - **`sweepChunks` / `sweepExecRow`** — the model splits one
//!   `sweep_expired_logs` pass into two actions so the mid-pass crash
//!   window is reachable *in the model*. The implementation's pass is
//!   one atomic call from the caller's perspective; the mid-pass state
//!   is unobservable without injecting a fault between two statements
//!   of one function. The driver runs the real sweep at the
//!   `sweepChunks` step, **skips the state comparison for that one
//!   step**, and compares at the `sweepExecRow` step (where the model
//!   and the one-call implementation agree again). The two-DELETE
//!   ordering is the model checker's obligation, not this replay's.
//! - **`deliverAck` / `uploaderAbandons`** — builder-side effects on
//!   builder-side state; no projected variable observes them. The
//!   implementation half of `deliverAck` — `cut()` returning the
//!   durable-through line only after the manifest INSERT commits — is
//!   asserted at the `cutChunk` arm.
//!
//! # The projection
//!
//! Field names are the model's variable names (the ITF namespace
//! prefix `<regime module>::logService::` is accepted via serde
//! aliases for all four regime modules). Projected:
//!
//! - `chunks` ← `SELECT session_id, first_line, line_count FROM
//!   drv_log_chunks` (the core artifact);
//! - `sessions` ← the real `IngestSession`'s `high_water_line()`,
//!   `final_line_count()`, and `shared().snapshot()` line numbers,
//!   plus the driver's phase/abort bookkeeping for the two pieces the
//!   implementation does not store (the stream phase lives in the gRPC
//!   handler's control flow, and `hwAtLearn` is recorded by the driver
//!   at the moment it calls `set_final_line_count` — the high-water
//!   half of that recording is still read from the real session);
//! - `dbFinalCount` ← `gate::sealed_final_line_count` (the store's own
//!   read of the scheduler-written terminal stamp, including the
//!   non-terminal-status and NULL-count collapses);
//! - `execRowExists`, `dispatched`, `sessionsOpened` ← row-existence /
//!   row-count queries and the driver's session list length.
//!
//! Omitted: `producedEnd`, `buildFinished`, `ackedBelow`, `sentBelow`,
//! `uploader`, `expired`, `sweepChunksDone`, `fabricated`, `wit` —
//! builder-side mirrors, ghosts, and model-only history bookkeeping.
//! Diffing the driver's own bookkeeping against the model proves
//! nothing about the implementation.
//!
//! # The read-path conformance check
//!
//! The model's `servedSpanExact` invariant says the read path's
//! ordered-walk-with-watermark over the manifest yields exactly the
//! union of the chunks' line ranges, each line exactly once.
//! `readTail` is a derived definition, not a state variable, so it is
//! not in the per-step ITF state — instead, after every replayed step,
//! the driver runs the **real** read path
//! ([`tail::read_manifest_range`] + [`tail::read_chunk`] under one
//! [`tail::LineCursor`]) over the real manifest and asserts the
//! yielded line-number sequence equals the manifest union with no
//! duplicates and no gaps. That is the invariant the model checker
//! proves of the model's fold, checked against the implementation's
//! fold, in every state the trace reaches — including the
//! overlapping-chunks states the dedup exists for.
//!
//! # Determinism
//!
//! The named-run replays are fully deterministic. The simulation pins
//! its seed in the `#[quint_run]` attribute (an input, not a
//! measurement); unseeded exploration is a local activity — delete the
//! seed, run until a divergence appears, pin the offending seed.
//!
//! All tests are `#[ignore]`d: they shell out to `quint`, which the
//! default `nextest-rio-store` sandbox does not provide. The dedicated
//! check (`mbt-rio-logservice`, wired in `nix/quint.nix` next to
//! `mbt-rio-lease`) stages the model into the nextest workspace and
//! runs them with `--run-ignored`. Locally:
//!
//! ```text
//! cargo nextest run -p rio-store -E 'test(/mbt_/)' --run-ignored all
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use itf::de::{As, Integer, Same};
use quint_connect::{Driver, Step, switch};
use rio_auth::hmac::AssignmentClaims;
use rio_proto::store::AppendLogHeader;
use rio_proto::types::BuildLogBatch;
use rio_test_support::TestDb;
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::chunks::MemoryLogChunkStore;
use super::ingest::{AcceptOutcome, IngestConfig, IngestSession};
use super::{gate, sessions, sweep, tail};

/// The DAG-key `drv_hash` (`AssignmentClaims.drv_hash` /
/// `derivations.drv_hash`) of the one modeled derivation.
const DRV: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-mbt-logservice.drv";
/// The same derivation as a full store path
/// (`AppendLogHeader.derivation_path`).
const DRV_PATH: &str = "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-mbt-logservice.drv";
/// `drv_log_hash()` of both of the above — the chunk-key prefix and the
/// `drv_executions.drv_hash` form.
const DRV_HASH_32: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm";
/// The one bound builder. The model's writer identity is the execution
/// index, not the executor (a re-dispatch mints a new exec_id either
/// way), so every execution is assigned to the same executor and the
/// gate's claimed-exec rejection is driven purely by the exec_id
/// mismatch — the same discriminator the model's `e != dispatched`
/// uses.
const EXECUTOR: &str = "mbt-builder-0";
/// The replica identity for `sessions::acquire`. One replica: the model
/// does not track the lease (an admission/routing mechanism, per its
/// scope notes), and a same-replica reacquire never hits the Busy arm.
const REPLICA: &str = "mbt-replica-0";

/// Production-default open caps for the projection (the model does not
/// model the caps; they are high enough that no replayed trace trips
/// them).
fn mbt_caps() -> gate::OpenCaps {
    gate::OpenCaps {
        per_exec_byte_cap: super::ingest::DEFAULT_PER_EXEC_BYTE_CAP,
        max_chunks_per_exec: 100_000,
    }
}

/// The TTL retention used by the `executionExpires` / `sweepChunks`
/// arms. The driver backdates `started_at` past this to mirror the
/// model's `expired` flag flipping.
const RETENTION: Duration = Duration::from_secs(3600);

/// The spec path fallback for a local `cargo nextest` run. The
/// `mbt-rio-logservice` check overrides it via `RIO_MBT_SPEC_PATH`: the
/// test binary runs in a different sandbox than the one that compiled
/// it, so this baked path points at a tree that no longer exists there.
const SPEC_ABS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/spec/models/logService.qnt"
);

fn spec_path() -> std::path::PathBuf {
    std::env::var_os("RIO_MBT_SPEC_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(SPEC_ABS))
}

/// Deterministic content for worker line `n`. The model does not track
/// content (a documented scope boundary); the bytes only need to be
/// non-empty and reproducible so the chunk codec round-trips something
/// real.
fn line_bytes(n: u64) -> Vec<u8> {
    format!("line-{n}").into_bytes()
}

// =======================================================================
// The projection (the abstraction function)
// =======================================================================

/// The subset of the model's state the implementation observably
/// realizes. See the module header for what each field is projected
/// from and why the rest are omitted. The `rename` carries the base
/// regime's ITF namespace prefix (the one the `#[quint_run]`
/// simulation uses); the `alias`es cover the other three regime
/// modules the named runs span.
#[derive(Debug, PartialEq, Deserialize)]
struct Projection {
    #[serde(
        rename = "logServiceBase::logService::dispatched",
        alias = "logServiceRedispatch::logService::dispatched",
        alias = "logServiceResend::logService::dispatched",
        alias = "logServiceSweep::logService::dispatched"
    )]
    dispatched: u64,
    #[serde(
        rename = "logServiceBase::logService::execRowExists",
        alias = "logServiceRedispatch::logService::execRowExists",
        alias = "logServiceResend::logService::execRowExists",
        alias = "logServiceSweep::logService::execRowExists",
        with = "As::<BTreeMap<Integer, Same>>"
    )]
    exec_row_exists: BTreeMap<u64, bool>,
    #[serde(
        rename = "logServiceBase::logService::dbFinalCount",
        alias = "logServiceRedispatch::logService::dbFinalCount",
        alias = "logServiceResend::logService::dbFinalCount",
        alias = "logServiceSweep::logService::dbFinalCount",
        with = "As::<BTreeMap<Integer, Same>>"
    )]
    db_final_count: BTreeMap<u64, ModelCount>,
    #[serde(
        rename = "logServiceBase::logService::sessionsOpened",
        alias = "logServiceRedispatch::logService::sessionsOpened",
        alias = "logServiceResend::logService::sessionsOpened",
        alias = "logServiceSweep::logService::sessionsOpened",
        with = "As::<BTreeMap<Integer, Integer>>"
    )]
    sessions_opened: BTreeMap<u64, u64>,
    #[serde(
        rename = "logServiceBase::logService::sessions",
        alias = "logServiceRedispatch::logService::sessions",
        alias = "logServiceResend::logService::sessions",
        alias = "logServiceSweep::logService::sessions",
        with = "As::<BTreeMap<Integer, BTreeMap<Integer, Same>>>"
    )]
    sessions: BTreeMap<u64, BTreeMap<u64, ModelSessionSlot>>,
    #[serde(
        rename = "logServiceBase::logService::chunks",
        alias = "logServiceRedispatch::logService::chunks",
        alias = "logServiceResend::logService::chunks",
        alias = "logServiceSweep::logService::chunks",
        with = "As::<BTreeMap<Integer, Same>>"
    )]
    chunks: BTreeMap<u64, BTreeSet<ModelChunk>>,
}

impl Projection {
    /// Collapse `dbFinalCount` to `NoCount` for executions whose
    /// lifecycle row no longer exists. Applied to both sides of every
    /// comparison.
    ///
    /// The model defines `dbFinalCount` as the row's
    /// `{status, final_line_count}` columns but never resets the
    /// variable when `sweepExecRow` deletes the row — it does not need
    /// to, because every model guard that reads `dbFinalCount` either
    /// also requires `execRowExists` (`logIsComplete`,
    /// `refreshCeiling`, `recordFinalLineCount`) or requires an
    /// unexpired execution (`openSession`'s ceiling seeding), and a
    /// swept execution is expired by the sweep's own precondition. The
    /// implementation stores both facts in one row, so deleting the
    /// row makes the stamp unreadable (`sealed_final_line_count`
    /// returns `None`). The observable both sides agree on — and the
    /// one every reachable behavior depends on — is the join
    /// `execRowExists(e) ? dbFinalCount(e) : NoCount`; the dangling
    /// post-sweep model value is unobservable by construction, so
    /// comparing it raw would fail the replay on a difference no
    /// action can ever surface.
    fn normalize(&mut self) {
        for (e, count) in &mut self.db_final_count {
            if !self.exec_row_exists.get(e).copied().unwrap_or(false) {
                *count = ModelCount::NoCount;
            }
        }
    }
}

/// The model's `CountOpt`: `drv_executions.{status, final_line_count}`
/// collapsed to "is there a recorded end to enforce".
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelCount {
    NoCount,
    /// The assignment-close stamp (sched.db.exec-stamp-on-close):
    /// status terminal, `final_line_count` NULL — terminal for the
    /// sweep, countless for the completeness gate.
    StampedNoCount,
    Count(u64),
}

/// The model's `SessionSlot`.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelSessionSlot {
    NoSession,
    Session(ModelSession),
}

/// The model's `Session(...)` payload.
#[derive(Debug, PartialEq, Deserialize)]
struct ModelSession {
    phase: ModelPhase,
    #[serde(rename = "highWater")]
    high_water: u64,
    ceiling: ModelCeiling,
    buf: BTreeSet<u64>,
}

/// The model's `Phase`.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelPhase {
    SessOpen,
    SessDetached,
}

/// The model's `CeilingState`.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "tag", content = "value")]
enum ModelCeiling {
    NoCeiling,
    Ceiling(ModelCeilingRec),
}

#[derive(Debug, PartialEq, Deserialize)]
struct ModelCeilingRec {
    value: u64,
    #[serde(rename = "hwAtLearn")]
    hw_at_learn: u64,
}

/// The model's `Chunk`: one `drv_log_chunks` manifest row, keyed by the
/// cutting session's mint-order index.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct ModelChunk {
    sess: u64,
    first: u64,
    count: u64,
}

// =======================================================================
// The system under test
// =======================================================================

/// One ingest session's implementation half plus the driver bookkeeping
/// the model carries but the implementation does not store.
struct SessionHarness {
    session_id: Uuid,
    /// The real per-stream state machine.
    session: IngestSession,
    /// The model's `phase`. The implementation's phase is the gRPC
    /// handler's control flow (is the stream still connected?), which
    /// the driver plays the part of.
    open: bool,
    /// `sessionAborts` fired: the handler dropped the in-memory buffer.
    /// Projected as `buf = Set()`; see the module header for why the
    /// real buffer is not actually emptied.
    aborted: bool,
    /// The session's high-water mark at the moment the driver called
    /// `set_final_line_count` (the model's `hwAtLearn`). Read from the
    /// real session at the call site; `None` until the ceiling is
    /// learned.
    hw_at_learn: Option<u64>,
}

/// One model execution: the real exec_id + sessions plus the mirror of
/// the builder-side uploader state needed to construct honest batches.
struct ExecHarness {
    exec_id: Uuid,
    /// The model's `producedEnd[e]` (builder-side mirror).
    produced_end: u64,
    /// The model's `sentBelow[e]` (builder-side mirror): the uploader's
    /// transmit cursor, reset to its acked watermark on reconnect.
    sent_below: u64,
    /// The model's `ackedBelow[e]` (builder-side mirror). Only read by
    /// `openSession`'s transmit-cursor rewind.
    acked_below: u64,
    /// Mint-ordered: model session index `s` is `sessions[s - 1]`.
    sessions: Vec<SessionHarness>,
}

/// The whole system: the ephemeral PG, the in-memory chunk store, and
/// one harness per model execution.
struct MbtSystem {
    db: TestDb,
    store: MemoryLogChunkStore,
    derivation_id: Uuid,
    execs: Vec<ExecHarness>,
    /// The regime's `MAX_EXECS` / `MAX_SESSIONS`: the projection must
    /// produce a total map over `1..=max` to match the model's
    /// `EXECS.mapBy(...)` shape.
    max_execs: usize,
    max_sessions: usize,
}

impl MbtSystem {
    /// The model's `init`: an empty store, a derivation that exists but
    /// has never been dispatched.
    async fn init(max_execs: usize, max_sessions: usize) -> Result<Self> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let derivation_id = seed_derivation(&db.pool).await?;
        Ok(Self {
            db,
            store: MemoryLogChunkStore::default(),
            derivation_id,
            execs: Vec::new(),
            max_execs,
            max_sessions,
        })
    }

    /// Re-init for the next simulation trace without paying the
    /// CREATE DATABASE + migrations cost again: truncate every table
    /// the replay touches, reseed the derivation, drop the in-memory
    /// state.
    async fn reset(&mut self) -> Result<()> {
        sqlx::query(
            "TRUNCATE drv_log_chunks, log_ingest_sessions, drv_executions, \
             assignments, derivations CASCADE",
        )
        .execute(&self.db.pool)
        .await?;
        self.derivation_id = seed_derivation(&self.db.pool).await?;
        self.store = MemoryLogChunkStore::default();
        self.execs.clear();
        Ok(())
    }

    fn pool(&self) -> &PgPool {
        &self.db.pool
    }

    fn exec(&mut self, e: u64) -> Result<&mut ExecHarness> {
        let idx = usize::try_from(e).expect("exec index fits usize") - 1;
        self.execs
            .get_mut(idx)
            .with_context(|| format!("trace references execution {e} before its dispatch"))
    }

    /// `dispatch`: the scheduler mints a new execution, records its
    /// assignment row, and INSERTs its lifecycle
    /// row. Mirrors `assign_to_worker`'s assignment write + the
    /// dispatch-time `drv_executions` INSERT. The previous attempt's
    /// assignment is marked failed first (the `assignments_active_uq`
    /// partial unique index allows one active assignment per
    /// derivation, and that is also what a real re-dispatch does).
    async fn dispatch(&mut self) -> Result<()> {
        ensure!(
            self.execs.len() < self.max_execs,
            "dispatch past MAX_EXECS = {}",
            self.max_execs
        );
        let exec_id = Uuid::now_v7();
        sqlx::query("UPDATE assignments SET status = 'failed' WHERE derivation_id = $1")
            .bind(self.derivation_id)
            .execute(self.pool())
            .await?;
        sqlx::query(
            "INSERT INTO assignments \
                 (derivation_id, builder_id, generation, status, assigned_at, exec_id) \
             VALUES ($1, $2, 1, 'acknowledged', now(), $3)",
        )
        .bind(self.derivation_id)
        .bind(EXECUTOR)
        .bind(exec_id)
        .execute(self.pool())
        .await?;
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at) \
             VALUES ($1, $2, $3, now())",
        )
        .bind(exec_id)
        .bind(DRV_HASH_32)
        .bind(EXECUTOR)
        .execute(self.pool())
        .await?;
        self.execs.push(ExecHarness {
            exec_id,
            produced_end: 0,
            sent_below: 0,
            acked_below: 0,
            sessions: Vec::new(),
        });
        Ok(())
    }

    /// `produceLine(e)`: builder-side mirror — one more line into the
    /// uploader's input.
    fn produce_line(&mut self, e: u64) -> Result<()> {
        self.exec(e)?.produced_end += 1;
        Ok(())
    }

    /// `recordFinalLineCount(e)`: the scheduler stamps the lifecycle
    /// row terminal with the builder-reported count. Mirrors
    /// `completion.rs::terminal_log_epilogue`. The model's
    /// precondition guarantees `producedEnd > 0`, so the 0 -> NULL arm
    /// is never taken on a replayed trace.
    async fn record_final_line_count(&mut self, e: u64) -> Result<()> {
        let h = self.exec(e)?;
        ensure!(
            h.produced_end > 0,
            "recordFinalLineCount on a zero-line build (the model's precondition forbids it)"
        );
        let (exec_id, count) = (h.exec_id, h.produced_end as i64);
        sqlx::query(
            "UPDATE drv_executions SET status = 'succeeded', finished_at = now(), \
             final_line_count = $2 WHERE exec_id = $1",
        )
        .bind(exec_id)
        .bind(count)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// `closeExecStamp(e)`: the scheduler's assignment close stamps
    /// the lifecycle row terminal with NO count
    /// (sched.db.exec-stamp-on-close — `close_assignments_sql`'s
    /// stamped CTE, `status IS NULL` guard = first verdict wins). The
    /// store crate cannot call the scheduler's closer; the mirror
    /// executes the stamp's effect with the same qual. `'failed'`
    /// stands for the close family's status — the projection
    /// distinguishes only NULL / terminal / counted.
    async fn close_exec_stamp(&mut self, e: u64) -> Result<()> {
        let exec_id = self.exec(e)?.exec_id;
        sqlx::query(
            "UPDATE drv_executions SET status = 'failed', finished_at = now() \
             WHERE exec_id = $1 AND status IS NULL",
        )
        .bind(exec_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The gate inputs for execution `e`'s builder: an HMAC-verified
    /// assignment-token claim set plus the stream-open header. The
    /// HMAC layer itself is below the model's trust boundary (a
    /// documented scope note), so the claims are constructed directly.
    fn gate_inputs(&mut self, e: u64) -> Result<(AssignmentClaims, AppendLogHeader)> {
        let exec_id = self.exec(e)?.exec_id;
        Ok((
            AssignmentClaims {
                executor_id: EXECUTOR.to_string(),
                drv_hash: DRV.to_string(),
                expected_outputs: vec![],
                is_ca: false,
                expiry_unix: u64::MAX,
                tenant: None,
            },
            AppendLogHeader {
                derivation_path: DRV_PATH.to_string(),
                exec_id: exec_id.to_string(),
            },
        ))
    }

    /// `openSession(e)`: the gate admits the stream, the ingest lease
    /// is acquired, and a fresh `IngestSession` is born — with the
    /// completeness ceiling iff the execution is already terminal with
    /// a known count. Mirrors `service.rs::append_log` steps 3-6.
    async fn open_session(&mut self, e: u64) -> Result<()> {
        let (claims, header) = self.gate_inputs(e)?;
        let ok = gate::check_append_open(self.pool(), &claims, &header, mbt_caps())
            .await
            .map_err(|status| {
                anyhow::anyhow!(
                    "openSession({e}): the model admits this open but check_append_open \
                     rejected it: {status:?}"
                )
            })?;
        ensure!(
            ok.exec_id == self.exec(e)?.exec_id,
            "gate returned a different exec_id than the header claimed"
        );
        let session_id = Uuid::now_v7();
        let acquired = sessions::acquire(self.pool(), ok.exec_id, session_id, REPLICA).await?;
        ensure!(
            matches!(acquired, sessions::Acquire::Acquired),
            "openSession({e}): the model admits this open but sessions::acquire returned \
             {acquired:?}"
        );
        // The cut threshold is irrelevant here (the model's cut is an
        // explicit action, not a size trigger, and `cut_due` is only a
        // returned flag), so the production defaults are kept.
        let session = IngestSession::new(&ok, session_id, IngestConfig::default());
        // The constructor seeds the per-append ceiling from the gate's
        // answer for an already-terminal execution (the late replay).
        // The learn-time high water is 0 by construction.
        let hw_at_learn = ok.final_line_count.map(|_| session.high_water_line());
        let max_sessions = self.max_sessions;
        let h = self.exec(e)?;
        ensure!(
            h.sessions.len() < max_sessions,
            "openSession past MAX_SESSIONS"
        );
        // The uploader rewinds its transmit cursor to the acked
        // watermark on every (re)connect — that reset IS the
        // at-least-once retransmit. The open-time coverage ack
        // (bug_032, the wire-1 letter): the driver's first ack-stream
        // message advertises the durable contiguous watermark
        // (`IngestSession::open_coverage_next_line`) and the
        // uploader's typed trim arm consumes it BEFORE replaying —
        // mirrored here exactly as `log_upload.rs::drive` does.
        let watermark = session.open_coverage_next_line();
        if watermark > 0 {
            h.acked_below = h.acked_below.max(watermark);
        }
        h.sent_below = h.acked_below;
        h.sessions.push(SessionHarness {
            session_id,
            session,
            open: true,
            aborted: false,
            hw_at_learn,
        });
        Ok(())
    }

    /// `openRejectedSuperseded(e)`: the gate rejects a stream open
    /// whose claimed execution is no longer the derivation's latest
    /// assignment. The model says PERMISSION_DENIED; anything else —
    /// including an admission — is a divergence.
    /// `rewriteAssignment(e)`: the scheduler reuses the assignment row
    /// for a new attempt — the claimed exec_id vanishes from the
    /// assignments table. Mirrors the ON CONFLICT upsert the gate
    /// observes as fetch_optional == None.
    async fn rewrite_assignment(&mut self, e: u64) -> Result<()> {
        let exec = self.exec(e)?.exec_id;
        let rewritten = sqlx::query("UPDATE assignments SET exec_id = $2 WHERE exec_id = $1")
            .bind(exec)
            .bind(Uuid::now_v7())
            .execute(self.pool())
            .await?
            .rows_affected();
        ensure!(
            rewritten == 1,
            "rewriteAssignment({e}): expected exactly one assignment row, rewrote {rewritten}"
        );
        Ok(())
    }

    async fn open_rejected_superseded(&mut self, e: u64) -> Result<()> {
        let (claims, header) = self.gate_inputs(e)?;
        match gate::check_append_open(self.pool(), &claims, &header, mbt_caps()).await {
            Err(status) if status.code() == tonic::Code::FailedPrecondition => Ok(()),
            Err(status) => bail!(
                "openRejectedSuperseded({e}): expected FAILED_PRECONDITION, got {:?}: {}",
                status.code(),
                status.message()
            ),
            Ok(ok) => bail!(
                "openRejectedSuperseded({e}): the model rejects this open as superseded but \
                 check_append_open admitted it: {ok:?}"
            ),
        }
    }

    /// `openRejectedComplete(e)`: the gate rejects a stream open
    /// because the execution's log is already complete
    /// (FAILED_PRECONDITION).
    async fn open_rejected_complete(&mut self, e: u64) -> Result<()> {
        let (claims, header) = self.gate_inputs(e)?;
        match gate::check_append_open(self.pool(), &claims, &header, mbt_caps()).await {
            Err(status) if status.code() == tonic::Code::FailedPrecondition => Ok(()),
            Err(status) => bail!(
                "openRejectedComplete({e}): expected FAILED_PRECONDITION, got {:?}: {}",
                status.code(),
                status.message()
            ),
            Ok(ok) => bail!(
                "openRejectedComplete({e}): the model seals this log but check_append_open \
                 admitted the open: {ok:?}"
            ),
        }
    }

    /// The execution's unique open session index (1-based, the model's
    /// `openSessionOf`), or an error if none is open.
    fn open_session_of(&mut self, e: u64) -> Result<usize> {
        let h = self.exec(e)?;
        h.sessions
            .iter()
            .position(|s| s.open)
            .map(|i| i + 1)
            .with_context(|| format!("execution {e} has no open session"))
    }

    /// Feed one batch to a session and translate the verdict. Both
    /// append arms go through here; the model's accept/reject/truncate
    /// outcome is not asserted directly — a wrong verdict shows up as
    /// a `sessions`/`chunks` divergence in the post-step diff with a
    /// readable shape. Stream-fatal errors (the byte cap) and the
    /// overflow rejection are unreachable in the model's bounded line
    /// domain, so either is an immediate failure.
    fn accept_batch(&mut self, e: u64, s: usize, lo: u64, hi: u64) -> Result<()> {
        ensure!(lo < hi, "accept_batch with an empty range [{lo}, {hi})");
        let batch = BuildLogBatch {
            derivation_path: DRV_PATH.to_string(),
            lines: (lo..hi).map(line_bytes).collect(),
            first_line_number: lo,
            executor_id: EXECUTOR.to_string(),
        };
        let h = self.exec(e)?;
        let outcome = h.sessions[s - 1]
            .session
            .accept(batch)
            .map_err(|status| anyhow::anyhow!("accept returned a stream-fatal error: {status}"))?;
        ensure!(
            !matches!(outcome, AcceptOutcome::RejectedOverflow),
            "accept rejected a batch in the model's bounded line domain as an overflow"
        );
        // The covered-replay drop (merged_bug_002's write-path arm):
        // nothing was buffered or charged, and the driver acks the
        // batch from the manifest truth (`service.rs`'s CoveredReplay
        // arm) — the uploader's trim consumes it like any ack. The
        // value arrives clamped to the contiguous durable frontier
        // (merged_bug_005); `None` means no ack was sent, so the
        // mirror's acked-below watermark stays put.
        if let AcceptOutcome::CoveredReplay {
            durable_through: Some(v),
        } = outcome
        {
            h.acked_below = h.acked_below.max(v + 1);
        }
        Ok(())
    }

    /// `appendHonest(e)`: the uploader transmits the whole un-sent
    /// remainder of its retransmit buffer to the open session. Mirrors
    /// `log_upload.rs::drive` -> `service.rs::drive` -> `accept`.
    fn append_honest(&mut self, e: u64) -> Result<()> {
        let s = self.open_session_of(e)?;
        let h = self.exec(e)?;
        let (lo, hi) = (h.sent_below, h.produced_end);
        h.sent_below = hi;
        self.accept_batch(e, s, lo, hi)
    }

    /// `appendFabricated(e, lo, hi)`: the bound builder sends an
    /// arbitrary batch that is not its uploader's honest replay. The
    /// uploader's cursors are untouched.
    fn append_fabricated(&mut self, e: u64, lo: u64, hi: u64) -> Result<()> {
        let s = self.open_session_of(e)?;
        self.accept_batch(e, s, lo, hi)
    }

    /// `refreshCeiling(e, s)`: the handler's heartbeat tick observes
    /// the now-terminal lifecycle row and hands the recorded count to
    /// the session. Mirrors `service.rs::drive`'s heartbeat arm.
    async fn refresh_ceiling(&mut self, e: u64, s: u64) -> Result<()> {
        let exec_id = self.exec(e)?.exec_id;
        let n = gate::sealed_final_line_count(self.pool(), exec_id)
            .await
            .map_err(|status| anyhow::anyhow!("sealed_final_line_count failed: {status}"))?
            .with_context(|| {
                format!(
                    "refreshCeiling({e}, {s}): the model has a recorded count but \
                     sealed_final_line_count returned None"
                )
            })?;
        let h = self.exec(e)?;
        let sess = h
            .sessions
            .get_mut(usize::try_from(s).expect("session index fits usize") - 1)
            .with_context(|| format!("refreshCeiling targets unopened session {s}"))?;
        sess.session
            .set_final_line_count(u64::try_from(n.max(0)).expect("count is non-negative"));
        sess.hw_at_learn = Some(sess.session.high_water_line());
        Ok(())
    }

    /// `cutChunk(e, s)`: cut the longest contiguous prefix of the
    /// session's buffer into one immutable chunk (compress, PUT,
    /// manifest INSERT) and return the durable-through line for the
    /// ack. This *is* `IngestSession::cut` against the in-memory store
    /// and the ephemeral PG.
    async fn cut_chunk(&mut self, e: u64, s: u64) -> Result<()> {
        let exec_idx = usize::try_from(e).expect("exec index fits usize") - 1;
        let sess_idx = usize::try_from(s).expect("session index fits usize") - 1;
        let sess = self
            .execs
            .get_mut(exec_idx)
            .with_context(|| format!("cutChunk on undispatched execution {e}"))?
            .sessions
            .get_mut(sess_idx)
            .with_context(|| format!("cutChunk targets unopened session {s}"))?;
        let acked =
            sess.session.cut(&self.store, &self.db.pool).await.context(
                "cutChunk: the commit failed against the in-memory store + ephemeral PG",
            )?;
        // The model's precondition is a non-empty buffer, so the cut
        // always commits something; `do_cut` sends the ack only after
        // `cut` returns the durable-through line (the ack-after-commit
        // ordering `ackImpliesDurable` is about).
        ensure!(
            acked.is_some(),
            "cutChunk({e}, {s}): the model's buffer is non-empty but cut() drained nothing"
        );
        Ok(())
    }

    /// `deliverAck(e)`: builder-side. The uploader trims its
    /// retransmit buffer up to a committed chunk's end. The acked
    /// watermark is only ever read back by `openSession`'s
    /// transmit-cursor rewind, and the model's nondet chunk choice is
    /// unobservable through any projected variable — the driver
    /// advances its mirror to the committed maximum (every ack
    /// delivery order reaches the same watermark set; the trim is a
    /// max).
    async fn deliver_ack(&mut self, e: u64) -> Result<()> {
        let s = self.open_session_of(e)?;
        let h = self.exec(e)?;
        let (exec_id, session_id) = (h.exec_id, h.sessions[s - 1].session_id);
        let max_end: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(first_line + line_count) FROM drv_log_chunks \
             WHERE exec_id = $1 AND session_id = $2",
        )
        .bind(exec_id)
        .bind(session_id)
        .fetch_one(&self.db.pool)
        .await?;
        let max_end = max_end.with_context(|| {
            format!(
                "deliverAck({e}): the model has a committed chunk to ack but the manifest is \
                 empty"
            )
        })?;
        let h = self.exec(e)?;
        h.acked_below = h
            .acked_below
            .max(u64::try_from(max_end).expect("non-negative line range end"));
        Ok(())
    }

    /// `builderDisconnects(e)`: the stream to the open session ends;
    /// the session goes Detached but keeps its buffer (the final drain
    /// keeps cutting). The clean close RELEASES the ingest-session
    /// registry row (`AppendDriver::run`'s teardown calls
    /// `sessions::release` on every path but a stolen lease) — the
    /// registry-row half is a real PG effect the sweep's structural
    /// liveness exclusion reads (merged_bug_071), so the mirror
    /// executes it; the phase half stays driver bookkeeping.
    ///
    /// THE RELEASE LAW'S POPULATION (merged_bug_010,
    /// `store.log.release-totality`): "released on every path except
    /// a stolen lease" quantifies over every path AFTER A SUCCESSFUL
    /// ACQUIRE — not only the driver's `LoopExit` alphabet this
    /// mirror walks. The OPEN-PHASE family (acquire .. driver spawn:
    /// the ownership witness's error arm, plus cancellation of the
    /// handler future itself) is covered structurally by
    /// `LeaseReleaseGuard` (release-on-drop, disarmed only at the
    /// driver handoff), so the law's enforced population equals its
    /// quantifier by type; the open-phase witnesses are the
    /// `open_phase_failures_release_the_lease_row` falsify-twin pair
    /// and `lease_release_guard_drop_tiers` (service.rs), not model
    /// steps — the model's alphabet starts at the open session this
    /// mirror's `builderConnects` mints.
    async fn builder_disconnects(&mut self, e: u64) -> Result<()> {
        let s = self.open_session_of(e)?;
        let exec_id = self.exec(e)?.exec_id;
        let session_id = self.exec(e)?.sessions[s - 1].session_id;
        sessions::release(self.pool(), exec_id, session_id)
            .await
            .context("builderDisconnects: registry release")?;
        self.exec(e)?.sessions[s - 1].open = false;
        Ok(())
    }

    /// `sessionAborts(e, s)`: the handler tears the stream down and
    /// drops the in-memory buffer. See the module header for why this
    /// is bookkeeping rather than a real buffer drop.
    fn session_aborts(&mut self, e: u64, s: u64) -> Result<()> {
        let idx = usize::try_from(s).expect("session index fits usize") - 1;
        let sess = self
            .exec(e)?
            .sessions
            .get_mut(idx)
            .with_context(|| format!("sessionAborts targets unopened session {s}"))?;
        sess.open = false;
        sess.aborted = true;
        Ok(())
    }

    /// `executionExpires(e)`: the retention clock passes the
    /// execution's `started_at`. The driver backdates the row so the
    /// real sweep's `started_at < now() - retention` SELECT finds it.
    async fn execution_expires(&mut self, e: u64) -> Result<()> {
        let exec_id = self.exec(e)?.exec_id;
        sqlx::query(
            "UPDATE drv_executions SET started_at = now() - make_interval(secs => $2) \
             WHERE exec_id = $1",
        )
        .bind(exec_id)
        .bind(RETENTION.as_secs_f64() * 2.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// `sweepChunks(e)` + `sweepExecRow(e)`: one real
    /// `sweep_expired_logs` pass. Driven at the `sweepChunks` step;
    /// the state comparison for that step is skipped (the
    /// implementation's pass is atomic and the model's mid-pass state
    /// is unobservable); the `sweepExecRow` step is a no-op whose
    /// post-state comparison checks the pass's combined effect.
    async fn sweep_pass(&mut self) -> Result<()> {
        sweep::sweep_expired_logs(&self.db.pool, &self.store, RETENTION, sweep::SWEEP_BATCH)
            .await
            .context("sweep_expired_logs failed")?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // The projection
    // -------------------------------------------------------------------

    /// Map the real state onto the model's variable shape. Every map is
    /// total over `1..=max_execs` x `1..=max_sessions` to match the
    /// model's `EXECS.mapBy(...)` encoding.
    async fn project(&self) -> Result<Projection> {
        let dispatched: i64 =
            sqlx::query_scalar("SELECT count(*) FROM assignments WHERE derivation_id = $1")
                .bind(self.derivation_id)
                .fetch_one(self.pool())
                .await?;

        let mut exec_row_exists = BTreeMap::new();
        let mut db_final_count = BTreeMap::new();
        let mut sessions_opened = BTreeMap::new();
        let mut sessions = BTreeMap::new();
        let mut chunks = BTreeMap::new();
        for e in 1..=self.max_execs as u64 {
            let harness = self.execs.get(usize::try_from(e).unwrap() - 1);
            let exec_id = harness.map(|h| h.exec_id);

            // -- execRowExists: the lifecycle row's existence.
            let exists = match exec_id {
                None => false,
                Some(id) => {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(SELECT 1 FROM drv_executions WHERE exec_id = $1)",
                    )
                    .bind(id)
                    .fetch_one(self.pool())
                    .await?
                }
            };
            exec_row_exists.insert(e, exists);

            // -- dbFinalCount: the ceiling half through the store's own
            // accessor (the terminal-status filter and the NULL
            // collapse are the implementation's, not the test's); the
            // terminal half through the same EXEC_STATUS_TERMINAL
            // vocabulary the sweep eligibility reads — a terminal row
            // with no recorded count is the model's `StampedNoCount`
            // (the assignment-close stamp).
            let (terminal, count) = match exec_id {
                None => (false, None),
                Some(id) => {
                    let count = gate::sealed_final_line_count(self.pool(), id)
                        .await
                        .map_err(|s| anyhow::anyhow!("sealed_final_line_count: {s}"))?;
                    let status: Option<String> =
                        sqlx::query_scalar("SELECT status FROM drv_executions WHERE exec_id = $1")
                            .bind(id)
                            .fetch_optional(self.pool())
                            .await
                            .context("dbFinalCount status probe")?
                            .flatten();
                    let terminal = status
                        .as_deref()
                        .is_some_and(|s| rio_migrations::schema::EXEC_STATUS_TERMINAL.contains(&s));
                    (terminal, count)
                }
            };
            db_final_count.insert(
                e,
                match (terminal, count) {
                    (_, Some(n)) => ModelCount::Count(u64::try_from(n).context("negative count")?),
                    (true, None) => ModelCount::StampedNoCount,
                    (false, None) => ModelCount::NoCount,
                },
            );

            // -- chunks: the manifest rows, re-keyed from session UUIDs
            // to the model's mint-order session indices.
            let mut set = BTreeSet::new();
            if let Some(harness) = harness {
                let rows: Vec<(Uuid, i64, i64)> = sqlx::query_as(
                    "SELECT session_id, first_line, line_count FROM drv_log_chunks \
                     WHERE exec_id = $1",
                )
                .bind(harness.exec_id)
                .fetch_all(self.pool())
                .await?;
                for (session_id, first, count) in rows {
                    let sess = harness
                        .sessions
                        .iter()
                        .position(|s| s.session_id == session_id)
                        .with_context(|| {
                            format!("manifest row for an unknown session {session_id}")
                        })? as u64
                        + 1;
                    set.insert(ModelChunk {
                        sess,
                        first: u64::try_from(first).context("negative first_line")?,
                        count: u64::try_from(count).context("negative line_count")?,
                    });
                }
            }
            chunks.insert(e, set);

            // -- sessions / sessionsOpened: the real IngestSession
            // fields under the driver's phase bookkeeping.
            sessions_opened.insert(e, harness.map_or(0, |h| h.sessions.len() as u64));
            let mut per_exec = BTreeMap::new();
            for s in 1..=self.max_sessions as u64 {
                let slot = harness
                    .and_then(|h| h.sessions.get(usize::try_from(s).unwrap() - 1))
                    .map_or(ModelSessionSlot::NoSession, project_session);
                per_exec.insert(s, slot);
            }
            sessions.insert(e, per_exec);
        }

        let mut projection = Projection {
            dispatched: u64::try_from(dispatched).expect("non-negative count"),
            exec_row_exists,
            db_final_count,
            sessions_opened,
            sessions,
            chunks,
        };
        // A no-op by construction (sealed_final_line_count already
        // returns None for a missing row) but applied to both sides so
        // the comparison is symmetric.
        projection.normalize();
        Ok(projection)
    }

    /// `servedSpanExact` checked against the real read path: for every
    /// execution, walk the manifest exactly as `TailLog` does
    /// (`read_manifest_range`'s `(first_line, session_id)` order, one
    /// shared `LineCursor`) and assert the yielded lines are exactly
    /// the union of the chunks' ranges, each exactly once, in
    /// increasing order. Run after every replayed step.
    async fn check_read_path(&self) -> Result<()> {
        for (idx, harness) in self.execs.iter().enumerate() {
            let refs = tail::read_manifest_range(self.pool(), harness.exec_id, 0)
                .await
                .map_err(|s| anyhow::anyhow!("read_manifest_range: {s}"))?;
            let union: BTreeSet<u64> = refs
                .iter()
                .flat_map(|c| c.first_line..c.first_line + c.line_count)
                .collect();
            let mut cursor = tail::LineCursor::new(0);
            let mut served = Vec::new();
            for chunk in &refs {
                let lines = tail::read_chunk(&self.store, None, chunk, &[], &mut cursor)
                    .await
                    .map_err(|s| anyhow::anyhow!("read_chunk({}): {s}", chunk.s3_key))?;
                served.extend(lines.into_iter().map(|(n, _)| n));
            }
            // No duplicates: the yield count equals the union's size.
            ensure!(
                served.len() == union.len(),
                "read path for execution {} yielded {} lines but the manifest union has {} \
                 (a duplicate or a dropped line):\nserved: {served:?}\nunion: {union:?}",
                idx + 1,
                served.len(),
                union.len(),
            );
            // No loss, no fabrication: the yielded set is the union.
            let served_set: BTreeSet<u64> = served.iter().copied().collect();
            ensure!(
                served_set == union,
                "read path for execution {} served a different line set than the manifest \
                 union:\nserved: {served_set:?}\nunion: {union:?}",
                idx + 1,
            );
            // The client-visible ordering contract.
            ensure!(
                served.windows(2).all(|w| w[0] < w[1]),
                "read path for execution {} yielded lines out of order: {served:?}",
                idx + 1,
            );
        }
        Ok(())
    }
}

/// Project one real session through the driver's phase/abort
/// bookkeeping. The high water and the ceiling value come from the
/// real [`IngestSession`]; the buffer comes from the real
/// `IngestShared` snapshot (in-flight + buffer — the same set the
/// model's `buf` collapses both into).
fn project_session(s: &SessionHarness) -> ModelSessionSlot {
    let buf = if s.aborted {
        BTreeSet::new()
    } else {
        s.session
            .shared()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    };
    ModelSessionSlot::Session(ModelSession {
        phase: if s.open {
            ModelPhase::SessOpen
        } else {
            ModelPhase::SessDetached
        },
        high_water: s.session.high_water_line(),
        ceiling: match s.session.final_line_count() {
            None => ModelCeiling::NoCeiling,
            Some(value) => ModelCeiling::Ceiling(ModelCeilingRec {
                value,
                hw_at_learn: s
                    .hw_at_learn
                    .expect("ceiling set without recording hwAtLearn"),
            }),
        },
        buf,
    })
}

/// Seed the one modeled derivation. Idempotent on `drv_hash`.
async fn seed_derivation(pool: &PgPool) -> Result<Uuid> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ($1, $2, 'x86_64-linux', 'assigned') \
         ON CONFLICT (drv_hash) DO UPDATE SET drv_path = EXCLUDED.drv_path \
         RETURNING derivation_id",
    )
    .bind(DRV)
    .bind(DRV_PATH)
    .fetch_one(pool)
    .await?)
}

// =======================================================================
// The named-run replays (the deterministic path)
// =======================================================================

/// One step of a named run, mirroring the model's per-step action
/// applications. The runs apply actions to literal executions and
/// sessions (no `nondet`), which is exactly why quint-connect cannot
/// replay them — `quint test` does not emit the `--mbt`
/// action-tracking variables — and why this mirror exists. A
/// model-side edit to a run that changes any projected variable at any
/// step makes the replay diverge there; the trace-length check catches
/// insertions and deletions.
#[derive(Debug, Clone, Copy)]
enum Action {
    Dispatch,
    ProduceLine(u64),
    BuildFinishes(u64),
    RecordFinalLineCount(u64),
    OpenSession(u64),
    /// The scheduler's ON-CONFLICT in-place rewrite: the (non-latest)
    /// execution's assignment row is re-pointed at a fresh exec_id, so
    /// the old executor's claimed-exec authority is permanently
    /// revoked (the v2 gate's one revocation path).
    RewriteAssignment(u64),
    OpenRejectedSuperseded(u64),
    AppendHonest(u64),
    AppendFabricated(u64, u64, u64),
    RefreshCeiling(u64, u64),
    CutChunk(u64, u64),
    DeliverAck(u64),
    BuilderDisconnects(u64),
    ExecutionExpires(u64),
    /// Model-only environment fact (the scheduler's per-lane attempt-GC
    /// cut releasing the execution): the store-side system under test
    /// holds no `drv_attempts` rows for the harness executions, so the
    /// implementation is ALREADY unreferenced and the step is a no-op
    /// here. The model variable it flips (`ledgerReferenced`) is
    /// scheduler-side state outside the projection.
    LedgerReleases(u64),
    /// The SCHEDULER's execution-row reclaim (`gc_exec_rows` behind
    /// `rio_retry_kernel::exec_row_sweep_eligible`). The store crate
    /// cannot call the scheduler's tick arm, so the mirror executes the
    /// reclaim's EFFECT (the row DELETE); the eligibility guard is the
    /// model's `gcExecRow` precondition here and the kernel's
    /// kani-pinned predicate (`check_exec_sweep_preserves_decisions` +
    /// the unit decision table) in production.
    GcExecRow(u64),
    /// Runs the real sweep pass (chunks + session registry; the
    /// lifecycle row survives — `store.log.sweep-ownership`).
    SweepChunks(u64),
}

use Action::*;

impl MbtSystem {
    /// Apply one mirrored named-run action. The same methods the
    /// quint-connect switch dispatches to — only the dispatcher
    /// differs.
    async fn apply(&mut self, action: Action) -> Result<()> {
        match action {
            Dispatch => self.dispatch().await,
            ProduceLine(e) => self.produce_line(e),
            // `buildFinishes` only gates builder-side state the driver
            // does not mirror (the drain deadline, the input-channel
            // close); its observable effect is enabling
            // `recordFinalLineCount`, which the trace sequences anyway.
            // The exec index is carried for symmetry with the model's
            // `buildFinishes(e)` and read only here.
            BuildFinishes(e) => {
                let _ = e;
                Ok(())
            }
            RecordFinalLineCount(e) => self.record_final_line_count(e).await,
            OpenSession(e) => self.open_session(e).await,
            RewriteAssignment(e) => self.rewrite_assignment(e).await,
            OpenRejectedSuperseded(e) => self.open_rejected_superseded(e).await,
            AppendHonest(e) => self.append_honest(e),
            AppendFabricated(e, lo, hi) => self.append_fabricated(e, lo, hi),
            RefreshCeiling(e, s) => self.refresh_ceiling(e, s).await,
            CutChunk(e, s) => self.cut_chunk(e, s).await,
            DeliverAck(e) => self.deliver_ack(e).await,
            BuilderDisconnects(e) => self.builder_disconnects(e).await,
            ExecutionExpires(e) => self.execution_expires(e).await,
            // The exec index is carried for symmetry with the model
            // action's parameter; the implementation step is a no-op
            // (see the variant's doc).
            LedgerReleases(e) => {
                let _ = e;
                Ok(())
            }
            GcExecRow(e) => {
                let exec_id = self.exec(e)?.exec_id;
                sqlx::query("DELETE FROM drv_executions WHERE exec_id = $1")
                    .bind(exec_id)
                    .execute(self.pool())
                    .await
                    .context("gc_exec_rows mirror delete")?;
                Ok(())
            }
            // The sweep pass selects every expired execution itself;
            // the exec indices are carried for symmetry with the
            // model's per-execution sweep actions and read only here.
            SweepChunks(e) => {
                let _ = e;
                self.sweep_pass().await
            }
        }
    }
}

/// Should the post-step state comparison be skipped for this action?
/// None today: the store's sweep pass is atomic in both the model
/// (one `sweepChunks` action strips chunks + sessions, the row
/// survives) and the implementation, so its post-state is exact; the
/// scheduler's row reclaim is its own mirrored step (`GcExecRow`).
fn skip_state_diff(action: Action) -> bool {
    let _ = action;
    false
}

/// One named run: which regime module to ask quint for, the regime's
/// constants (the projection must produce total maps of that shape),
/// and the mirrored action sequence.
struct NamedRun {
    run: &'static str,
    main: &'static str,
    max_execs: usize,
    max_sessions: usize,
    actions: &'static [Action],
}

/// `happyPathRun` (base regime): dispatch, produce two lines, stream
/// them, cut, ack, finish, stamp — the log reads complete with every
/// produced line served.
const HAPPY_PATH: NamedRun = NamedRun {
    run: "happyPathRun",
    main: "logServiceBase",
    max_execs: 1,
    max_sessions: 1,
    actions: &[
        Dispatch,
        ProduceLine(1),
        ProduceLine(1),
        OpenSession(1),
        AppendHonest(1),
        CutChunk(1, 1),
        DeliverAck(1),
        BuildFinishes(1),
        RecordFinalLineCount(1),
    ],
};

/// `pastFinalRejectionRun` (base regime): the execution goes terminal
/// at 1 line while the session is open; the refresh observes it; a
/// fabricated batch at the recorded end is rejected whole and a
/// straddling batch is truncated.
const PAST_FINAL_REJECTION: NamedRun = NamedRun {
    run: "pastFinalRejectionRun",
    main: "logServiceBase",
    max_execs: 1,
    max_sessions: 1,
    actions: &[
        Dispatch,
        ProduceLine(1),
        OpenSession(1),
        AppendHonest(1),
        BuildFinishes(1),
        RecordFinalLineCount(1),
        RefreshCeiling(1, 1),
        AppendFabricated(1, 1, 3),
    ],
};

/// `midStreamCeilingResidualRun` (base regime): lines accepted before
/// the session learns the ceiling are kept (the disclosed pre-refresh
/// residual); the learn-time high-water mark records where the
/// obligation starts.
const MID_STREAM_CEILING: NamedRun = NamedRun {
    run: "midStreamCeilingResidualRun",
    main: "logServiceBase",
    max_execs: 1,
    max_sessions: 1,
    actions: &[
        Dispatch,
        ProduceLine(1),
        OpenSession(1),
        AppendFabricated(1, 0, 3),
        BuildFinishes(1),
        RecordFinalLineCount(1),
        RefreshCeiling(1, 1),
    ],
};

/// `supersededWriterRun` (redispatch regime, v2): execution 1's session
/// is open and holding lines when execution 2 is dispatched; execution
/// 1 keeps cutting to its own manifest (supersession alone no longer
/// revokes its authority — merged_bug_101); only the scheduler's
/// in-place row rewrite excludes it, after which its reopen is
/// rejected. Execution 2's log is untouched throughout.
const SUPERSEDED_WRITER: NamedRun = NamedRun {
    run: "supersededWriterRun",
    main: "logServiceRedispatch",
    max_execs: 2,
    max_sessions: 1,
    actions: &[
        Dispatch,
        ProduceLine(1),
        OpenSession(1),
        AppendHonest(1),
        Dispatch,
        CutChunk(1, 1),
        BuilderDisconnects(1),
        RewriteAssignment(1),
        OpenRejectedSuperseded(1),
    ],
};

/// `ambiguousAckOverlapRun` (resend regime): a chunk is committed but
/// its ack never reaches the builder. Pre-letter the reconnect
/// replayed the same lines blind and a second overlapping chunk
/// landed; the open-time coverage ack (bug_032) now answers the
/// ambiguity AT OPEN — the uploader trims to the durable watermark
/// before replaying, the committed-but-unacked span mints nothing,
/// and a fresh produced line streams alone.
const AMBIGUOUS_ACK_OVERLAP: NamedRun = NamedRun {
    run: "ambiguousAckOverlapRun",
    main: "logServiceResend",
    max_execs: 1,
    max_sessions: 2,
    actions: &[
        Dispatch,
        ProduceLine(1),
        ProduceLine(1),
        OpenSession(1),
        AppendHonest(1),
        CutChunk(1, 1),
        BuilderDisconnects(1),
        OpenSession(1),
        ProduceLine(1),
        AppendHonest(1),
        CutChunk(1, 2),
    ],
};

/// `sweepCompleteLogRun` (sweep regime): the TTL sweep deletes a
/// complete log; the manifest and the lifecycle row go and the log
/// stops reading complete.
const SWEEP_COMPLETE_LOG: NamedRun = NamedRun {
    run: "sweepCompleteLogRun",
    main: "logServiceSweep",
    max_execs: 1,
    max_sessions: 1,
    actions: &[
        Dispatch,
        ProduceLine(1),
        OpenSession(1),
        AppendHonest(1),
        CutChunk(1, 1),
        DeliverAck(1),
        BuildFinishes(1),
        RecordFinalLineCount(1),
        ExecutionExpires(1),
        // The builder disconnects FIRST (merged_bug_071): a live
        // ingest session structurally shields its execution from the
        // sweep — the clean close releases the registry row, and only
        // then is the expired execution a victim. The scheduler later
        // reclaims the lifecycle row once the attempt ledger releases
        // AND the artifact conjuncts clear
        // (store.log.sweep-ownership+2's sixth conjunct).
        BuilderDisconnects(1),
        SweepChunks(1),
        LedgerReleases(1),
        GcExecRow(1),
    ],
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
        .args(["--main", run.main])
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

    ensure!(
        trace.states.len() == run.actions.len() + 1,
        "{}: the model's trace has {} states but the mirrored action sequence has {} actions \
         (+1 for init) — the run definition in logService.qnt and the Rust mirror have drifted",
        run.run,
        trace.states.len(),
        run.actions.len(),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    rt.block_on(async {
        let mut sys = MbtSystem::init(run.max_execs, run.max_sessions).await?;
        diff_step(
            run.run,
            0,
            "init",
            &trace.states[0].value,
            &sys.project().await?,
        )?;
        for (i, action) in run.actions.iter().enumerate() {
            sys.apply(*action)
                .await
                .with_context(|| format!("{}: step {} ({action:?})", run.run, i + 1))?;
            if !skip_state_diff(*action) {
                diff_step(
                    run.run,
                    i + 1,
                    &format!("{action:?}"),
                    &trace.states[i + 1].value,
                    &sys.project().await?,
                )?;
            }
            sys.check_read_path().await.with_context(|| {
                format!("{}: read path after step {} ({action:?})", run.run, i + 1)
            })?;
        }
        Ok(())
    })
}

/// One post-step state comparison. The model's state is the oracle; a
/// mismatch is either a driver bug (the action mapping, the
/// projection, or the PG seeding is wrong) or a genuine
/// model↔implementation disagreement — classify before fixing either.
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

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_past_final_rejection() {
    replay_named_run(&PAST_FINAL_REJECTION).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_mid_stream_ceiling_residual() {
    replay_named_run(&MID_STREAM_CEILING).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_superseded_writer() {
    replay_named_run(&SUPERSEDED_WRITER).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_ambiguous_ack_overlap() {
    replay_named_run(&AMBIGUOUS_ACK_OVERLAP).unwrap();
}

#[test]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_run_sweep_complete_log() {
    replay_named_run(&SWEEP_COMPLETE_LOG).unwrap();
}

// =======================================================================
// The quint-connect driver (the simulation path)
// =======================================================================

/// The [`Driver`] quint-connect drives over the base regime. Owns a
/// current-thread tokio runtime (quint-connect's `step` is sync; the
/// store API is async) and the [`MbtSystem`] (absent until the first
/// trace's `init` step).
struct LogServiceDriver {
    rt: tokio::runtime::Runtime,
    sys: Option<MbtSystem>,
}

impl LogServiceDriver {
    fn new() -> Self {
        Self {
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime"),
            // Populated by the first trace's init step. Bootstrapping
            // the ephemeral postgres here would waste it if the trace
            // generation fails before the first replay.
            sys: None,
        }
    }

    /// Reset for a new trace. The first init creates the ephemeral
    /// database (one CREATE DATABASE + the migrations for the whole
    /// simulation); subsequent inits truncate and reseed — a
    /// per-trace database would multiply the migration cost by the
    /// sample count.
    fn reset(&mut self) -> Result<()> {
        match self.sys.as_mut() {
            None => {
                let sys = self.rt.block_on(MbtSystem::init(1, 1))?;
                self.sys = Some(sys);
            }
            Some(sys) => self.rt.block_on(sys.reset())?,
        }
        Ok(())
    }

    fn sys(&mut self) -> &mut MbtSystem {
        self.sys.as_mut().expect("MbtSystem present after init")
    }
}

impl Driver for LogServiceDriver {
    type State = Projection;

    fn step(&mut self, step: &Step) -> quint_connect::Result {
        self.dispatch_action(step)?;
        // The read-path conformance check runs after every step (the
        // state diff itself runs in the framework's check_state).
        if let Some(sys) = self.sys.as_ref() {
            self.rt.block_on(sys.check_read_path())?;
        }
        Ok(())
    }
}

impl LogServiceDriver {
    /// The action dispatcher. Split out of [`Driver::step`] so the
    /// `switch!` expansion (an attributed block expression) sits in
    /// tail position — applying `?` to it directly would put the
    /// attribute in expression position, which stable Rust rejects.
    fn dispatch_action(&mut self, step: &Step) -> quint_connect::Result {
        switch!(step {
            // The first state of every trace is the init state. quint's
            // `--mbt` tracker labels it `init` in the first trace of a
            // multi-trace run and `step` in the subsequent ones — both
            // mean "reset and reseed".
            init => self.reset()?,
            step => self.reset()?,
            dispatch => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.dispatch())?;
            },
            produceLineAny(e: u64) => self.sys().produce_line(e)?,
            // Builder-side: arms the drain deadline, closes the
            // uploader's input. Nothing the store observes.
            buildFinishesAny(e: u64) => { let _ = e; },
            recordFinalLineCountAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.record_final_line_count(e))?;
            },
            openSessionAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.open_session(e))?;
            },
            openRejectedSupersededAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.open_rejected_superseded(e))?;
            },
            openRejectedCompleteAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.open_rejected_complete(e))?;
            },
            appendHonestAny(e: u64) => self.sys().append_honest(e)?,
            appendFabricatedAny(e: u64, lo: u64, hi: u64) => {
                self.sys().append_fabricated(e, lo, hi)?;
            },
            refreshCeilingAny(e: u64, s: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.refresh_ceiling(e, s))?;
            },
            cutChunkAny(e: u64, s: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.cut_chunk(e, s))?;
            },
            deliverAckAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.deliver_ack(e))?;
            },
            builderDisconnectsAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.builder_disconnects(e))?;
            },
            closeExecStampAny(e: u64) => {
                let sys = self.sys.as_mut().expect("init ran");
                self.rt.block_on(sys.close_exec_stamp(e))?;
            },
            sessionAbortsAny(e: u64, s: u64) => self.sys().session_aborts(e, s)?,
            // Builder-side: the uploader drops its retransmit buffer.
            // Disclosed loss the store never sees.
            uploaderAbandonsAny(e: u64) => { let _ = e; },
            executionExpiresAny(e: u64) => bail!(
                "executionExpires({e}) reached the base-regime driver: ENABLE_SWEEP is false \
                 in logServiceBase, so this action should be unreachable"
            ),
            sweepChunksAny(e: u64) => bail!(
                "sweepChunks({e}) reached the base-regime driver: ENABLE_SWEEP is false in \
                 logServiceBase, so this action should be unreachable"
            ),
            sweepExecRowAny(e: u64) => bail!(
                "sweepExecRow({e}) reached the base-regime driver: ENABLE_SWEEP is false in \
                 logServiceBase, so this action should be unreachable"
            )
        })
    }
}

impl quint_connect::State<LogServiceDriver> for Projection {
    fn from_driver(driver: &LogServiceDriver) -> quint_connect::Result<Self> {
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
    spec = "../docs/spec/models/logService.qnt",
    main = "logServiceBase",
    max_samples = 100,
    max_steps = 20,
    seed = "0x52494f4c"
)]
#[ignore = "shells out to quint; run by the dedicated MBT check with --run-ignored"]
fn mbt_simulation_base() -> impl Driver {
    LogServiceDriver::new()
}
