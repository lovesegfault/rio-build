//! Two-phase garbage collection: mark reachable, sweep unreachable.
//!
//! # Phases
//!
//! 1. **Mark** (`mark::compute_unreachable`): recursive CTE over
//!    `narinfo."references"` from root seeds. Returns `store_path_hash`
//!    for paths NOT reachable from any root.
//!
//! 2. **Sweep** ([`sweep::sweep`]): per unreachable path, in batched
//!    transactions: lock the path's manifest row (`FOR UPDATE`, so a
//!    concurrent PutPath for the same path waits), re-check
//!    references, DELETE narinfo (CASCADE) plus realisations and
//!    path_tenants. The sweep never touches `chunks` — chunk GC is
//!    decoupled and owned by the collect cycle (phase 3).
//!
//! 3. **Collect** (`collect::collect_cycle`): the lazy chunk
//!    collector — phase 3 of `run_gc` plus a daily backstop timer.
//!    Derives the live-chunk set from every existing manifest's
//!    `chunk_list` (fail-closed on any unparseable blob), then
//!    soft-deletes + enqueues unreferenced chunks past grace, capped
//!    per cycle with a keyset cursor carrying any backlog to the next
//!    cycle. A dry-run GC keeps this phase observation-only (shadow
//!    mode: would-collect report, no modification).
//!
//! 4. **Drain** ([`drain::spawn_drain_task`]): background task that
//!    reads `pending_s3_deletes`, calls `ChunkBackend::delete_by_key`,
//!    deletes row on success / increments attempts on failure. Max
//!    attempts = 10 (alert-worthy after that).
//!
//! # Root seeds
//!
//! - `manifests WHERE status='uploading'` (in-flight PutPath —
//!   don't delete what's being written)
//! - `narinfo WHERE created_at > now() - grace_hours` (recent
//!   paths — don't GC something that JUST arrived before a build
//!   can reference it)
//! - `extra_roots` param (scheduler's live-build output paths —
//!   passed from `ActorCommand::GcRoots`, may not be in narinfo yet)
//! - `scheduler_live_pins` (scheduler auto-pinned live-build inputs)
//! - per-tenant retention windows (path_tenants × tenants.retention)
//!
//! # Two-phase S3 commit
//!
//! PostgreSQL deletes are transactional, S3 DeleteObject is not. The
//! collect batch enqueues S3 keys in the SAME transaction as its
//! soft-deletes; the drain issues the actual DeleteObject later. If
//! drain fails, the object leaks (storage cost) but PG state is
//! correct. Better than the reverse (S3 deleted, tx rolled back,
//! dangling chunk ref → GetPath fails).

/// The GC-hold control (round-9 WO-S1-4, signed Q3: "GC-hold as a
/// first-class operator control — tonight's freeze was scale-to-0").
///
/// A typed, persisted hold every destructive actor consults: a
/// GLOBAL hold suspends EVERY destructive lane — `run_gc` (no-op
/// before mark, held tick stamped live) plus the whole
/// census-derived lane set via the `gc::lane::DestructiveLane`
/// per-tick consult and the demand-driven reap face
/// (merged_bug_050; `store.gc.hold-lanes`). A TENANT hold makes the
/// held tenant's registered paths reachable regardless of their
/// retention window (seed (f) + the sweep re-check gain the
/// conjunct). Holds are RELEASED, never deleted — the hold history
/// is audit evidence.
///
/// R17 axes, priced: `scope` ∈ {global, tenant} (CHECK-paired with
/// `tenant_id`); mandatory `reason` + `created_by`; `expires_at`
/// NULL = UNBOUNDED — an explicit operator decision recorded in the
/// row, not an accident (a bounded hold self-clears at expiry with no
/// release action; the heal edge is witnessed either way).
///
/// Operator surface (divergence from the "admin verb" prescription,
/// recorded): the wave's R6 proto partition grants this slot no proto
/// file, so the verb is this typed in-crate API + the persisted row
/// (settable from any PG session — `kubectl exec` psql is the
/// documented interim procedure in `store.typ`); the wire verb rides
/// the next proto-granted slot. The ENFORCEMENT — the signed
/// invariant's substance — is total here regardless of which surface
/// sets the row, where "total" is machine-backed (R23′): the
/// destructive-lane census (`gc/lane.rs`,
/// `gensets/destructive-lane-census.txt`) derives the suspended
/// population from the spawn-periodic family × the
/// reaches-delete-sink predicate, with `run_gc` pinned.
pub mod hold {
    use sqlx::PgPool;
    use uuid::Uuid;

    /// The typed hold scope (the closed axis; no wildcard consumers).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GcHoldScope {
        /// Suspend ALL collection — where ALL is machine-backed
        /// (R23′): the census-derived destructive-lane set (the
        /// spawn-periodic family scan × reaches-delete-sink in
        /// `gc/lane.rs::census::destructive_lane_census`, committed
        /// at `rio-store/tests/gensets/destructive-lane-census.txt`)
        /// ∪ {run_gc} pinned — run_gc's mark/sweep/chunk-collect
        /// pass, the chunk-collect backstop, the s3-delete drain,
        /// the log TTL sweep, the gc-orphan-scanner, and the
        /// demand-driven claim-reap face, each consulting fail-closed
        /// per tick (`store.gc.hold-lanes`).
        Global,
        /// Pin one tenant's registered paths as reachable.
        Tenant(Uuid),
    }

    /// One active hold, as mark/run_gc consult it.
    #[derive(Debug, sqlx::FromRow)]
    pub struct ActiveHold {
        pub hold_id: Uuid,
        pub reason: String,
        pub created_by: String,
    }

    /// Set a hold. `expires_at_secs` = None is an UNBOUNDED hold — the
    /// operator's explicit decision, recorded in the row. Returns the
    /// hold id (the release handle).
    // r[impl store.gc.hold+2]
    pub async fn set_hold(
        pool: &PgPool,
        scope: GcHoldScope,
        reason: &str,
        created_by: &str,
        expires_in_secs: Option<i64>,
    ) -> Result<Uuid, sqlx::Error> {
        let (scope_str, tenant_id) = match scope {
            GcHoldScope::Global => ("global", None),
            GcHoldScope::Tenant(t) => ("tenant", Some(t)),
        };
        sqlx::query_scalar(
            r#"
        INSERT INTO gc_holds (scope, tenant_id, reason, created_by, expires_at)
        VALUES ($1, $2, $3, $4,
                CASE WHEN $5::bigint IS NULL THEN NULL
                     ELSE now() + make_interval(secs => $5::bigint) END)
        RETURNING hold_id
        "#,
        )
        .bind(scope_str)
        .bind(tenant_id)
        .bind(reason)
        .bind(created_by)
        .bind(expires_in_secs)
        .fetch_one(pool)
        .await
    }

    /// Release a hold (the heal edge). Idempotent: releasing a released
    /// or unknown hold affects zero rows and returns false.
    // r[impl store.gc.hold+2]
    pub async fn release_hold(pool: &PgPool, hold_id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gc_holds SET released_at = now() \
         WHERE hold_id = $1 AND released_at IS NULL",
        )
        .bind(hold_id)
        .execute(pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The SQL predicate of an ACTIVE hold row (shared by every consult
    /// site so the activity law has one author): not released, and not
    /// past its expiry (NULL expiry = unbounded). A macro (not a const)
    /// so consult sites can `concat!` it into their `&'static str` SQL
    /// (sqlx 0.9's `SqlSafeStr` bound).
    macro_rules! active_hold_predicate {
        () => {
            "released_at IS NULL AND (expires_at IS NULL OR expires_at > now())"
        };
    }
    pub(crate) use active_hold_predicate;

    /// Per-consult destructive capability (merged_bug_050): proof
    /// that the active-hold predicate was consulted and NO global
    /// hold is active, minted ONLY by [`gate`] (private field — other
    /// modules cannot construct one). Non-`Clone`/non-`Copy` and
    /// passed by reference into tick bodies, so a clearance cannot
    /// outlive its tick or be stashed for a later one: every named
    /// delete sink demands `&HoldClearance`, which structurally ties
    /// every destructive act to a same-tick consult.
    ///
    /// TEMPORAL SCOPE (merged_bug_067 — the R28 time axis the
    /// reachability seal left bare): a clearance is authority for at
    /// most `gc::lane::DESTRUCTIVE_BATCH_DRAIN_BOUND` past
    /// its last successful consult. Multi-batch tick bodies demand
    /// `Self::authorize_batch` at each committed-transaction
    /// boundary: the call refuses on an aged clearance (expiry — no
    /// re-consult can resurrect it; the next tick re-gates), refuses
    /// under a hold landed mid-body (the re-consult), and otherwise
    /// refreshes the authority window AND mints the batch's
    /// [`BatchAuthority`] (bug_084/merged_bug_006, R32): the
    /// destructive sinks demand the token BY VALUE, so a batch
    /// outside an authorized boundary does not compile — the
    /// wave-11 unit verdict was advisory data a body could match
    /// and ignore. The axes of the hold law, each at its stated
    /// tier (R28-as-amended-by-R31): reachability — compile-sealed
    /// (this type at the named sinks, carried from merged_bug_050;
    /// the per-batch token at every sink since bug_084);
    /// time — sealed here (the expiry check precedes the re-consult,
    /// so a stale clearance authorizes nothing even when `gc_holds`
    /// is empty); population — DERIVED by the destructive-body
    /// census (`gc/lane.rs`: the lane census over spawn sites + the
    /// body census over until-short destructive loops), never
    /// author-enumerated — the wave-11 hand list wired four of six
    /// bodies and the two unwired ones defeated the operator's
    /// emergency stop.
    // r[impl store.gc.hold-lanes+2]
    // r[impl store.gc.clearance-expiry+2]
    #[derive(Debug)]
    pub struct HoldClearance {
        /// The last successful consult (mint or batch re-consult).
        /// `tokio::time::Instant`: monotonic in production, virtual
        /// under paused test time.
        consulted_at: tokio::time::Instant,
        _proof: (),
    }

    /// Authority for exactly ONE destructive batch (bug_084 +
    /// merged_bug_006, the R32 form): a linear token minted SOLELY by
    /// [`HoldClearance::authorize_batch`]'s `Authorized` arm — private
    /// field, non-`Clone`/non-`Copy`, passed BY VALUE into the
    /// destructive sinks (`sweep_one_batch`, `enqueue_chunk_deletes`,
    /// `drain_one_row`, `reap_one`, the log-sweep batch), so one
    /// authorization cannot be stashed, shared, or re-spent across
    /// batches. The wave-11 unit `Authorized` variant was advisory
    /// data a body could match and ignore (`move |_clearance|` —
    /// merged_bug_006's exact shape); the token makes batch execution
    /// reachable only through the authority.
    // r[impl store.gc.batch-authority]
    #[must_use = "an unconsumed batch authority is an unauthorized batch"]
    #[derive(Debug)]
    pub struct BatchAuthority {
        _proof: (),
    }

    impl BatchAuthority {
        /// Spend the token on the one batch it authorizes — the
        /// explicit consumption every sink performs at its
        /// destructive statement (a move, so a spent authority
        /// cannot fund a second batch).
        pub(crate) fn spend(self) {}
    }

    /// One batch-boundary authorization verdict — see
    /// `HoldClearance::authorize_batch`. Closed alphabet; every
    /// consumer matches it exhaustively (no wildcard arms).
    #[must_use = "an unconsumed verdict is an unauthorized batch"]
    #[derive(Debug)]
    pub enum BatchAuthorize {
        /// No active hold and the clearance is inside its authority
        /// window (now refreshed): the carried token authorizes
        /// exactly the next batch.
        Authorized(BatchAuthority),
        /// An active global hold landed since the last consult: the
        /// body MUST stop before its next destructive batch.
        Held(ActiveHold),
        /// The clearance aged past the drain bound with no successful
        /// consult: dead, refuses unconditionally (even with no hold
        /// in `gc_holds`). The body MUST stop; the next tick re-gates.
        Expired,
    }

    /// Why a destructive body stopped at a batch boundary short of
    /// its own completion — the typed cause behind every
    /// clearance-refused stop (the collect report's
    /// `clearance_stop`, the path sweep's mid-pass stop). ONE home
    /// for the alphabet (moved from `gc/collect.rs` when the path
    /// sweep joined the per-batch law — two near-identical stop
    /// enums invite drift). Closed; no wildcard consumers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ClearanceStop {
        /// The batch-boundary re-consult found an active global hold.
        Held,
        /// The clearance aged past `DESTRUCTIVE_BATCH_DRAIN_BOUND`
        /// with no successful consult — refused with no hold present.
        Expired,
    }

    impl HoldClearance {
        /// THE batch-boundary re-authorization (merged_bug_067):
        /// expiry FIRST (an aged clearance refuses before any
        /// re-consult — the time axis is the clearance's own law,
        /// not the hold table's), then the hold re-consult, then the
        /// window refresh. `&mut self` so a body cannot keep a
        /// shared borrow of the pre-consult proof across the call.
        /// Fail-closed like [`gate`]: a consult error refuses the
        /// batch (`Err`), never authorizes.
        // r[impl store.gc.clearance-expiry+2]
        pub(crate) async fn authorize_batch(
            &mut self,
            pool: &PgPool,
        ) -> Result<BatchAuthorize, sqlx::Error> {
            self.authorize_batch_with_bound(pool, crate::gc::lane::DESTRUCTIVE_BATCH_DRAIN_BOUND)
                .await
        }

        /// The bound-parameterized engine behind
        /// [`Self::authorize_batch`] (which pins `bound` to
        /// `DESTRUCTIVE_BATCH_DRAIN_BOUND` by delegation — the one
        /// production entry). Parameterized so the expiry face is
        /// witnessable in test time without a 30s sleep; production
        /// code calls the delegating wrapper.
        /// THE phase-seam consult (merged_bug_081, R29'): authority
        /// ages from the consumer's last CONSULT OPPORTUNITY, not the
        /// mint. The drain-cadence bound was frozen from the S3 drain
        /// lane's mint-adjacent tick; the collect/run_gc consumers
        /// mint BEFORE a read phase that dwarfs it (full mark+sweep;
        /// ~4 minutes of validation/mark at the 1.5M-path design
        /// point), so past 30s of pre-batch work every cycle Expired
        /// at batch 1 with zero batches — permanent zero
        /// chunk-collect progress exactly at scale, with "next tick
        /// re-gates" re-minting into the same structure. A declared
        /// seam between non-destructive phases is a consult
        /// opportunity: this consult restarts the window on clear,
        /// refuses under a hold, and fails closed on error. It
        /// returns NO [`BatchAuthority`] (R32: tokens only from
        /// `authorize_batch`) and is lawful ONLY at phase seams —
        /// destructive cadences re-authorize per batch, where an
        /// aged clearance still refuses unconditionally.
        // r[impl store.gc.consult-aged-clearance]
        pub(crate) async fn regate(&mut self, pool: &PgPool) -> Result<Regate, sqlx::Error> {
            match active_global_hold(pool).await? {
                Some(h) => Ok(Regate::Held(h)),
                None => {
                    self.consulted_at = tokio::time::Instant::now();
                    Ok(Regate::Refreshed)
                }
            }
        }

        pub(crate) async fn authorize_batch_with_bound(
            &mut self,
            pool: &PgPool,
            bound: std::time::Duration,
        ) -> Result<BatchAuthorize, sqlx::Error> {
            if self.consulted_at.elapsed() > bound {
                return Ok(BatchAuthorize::Expired);
            }
            match active_global_hold(pool).await? {
                Some(h) => Ok(BatchAuthorize::Held(h)),
                None => {
                    self.consulted_at = tokio::time::Instant::now();
                    // THE one mint site for [`BatchAuthority`]: one
                    // successful boundary consult = one batch.
                    Ok(BatchAuthorize::Authorized(BatchAuthority { _proof: () }))
                }
            }
        }
    }

    /// A phase-seam consult verdict — see [`HoldClearance::regate`].
    /// Closed alphabet; no wildcard consumers. Carries NO
    /// [`BatchAuthority`]: a seam consult cannot authorize a batch.
    #[must_use = "an unconsumed seam verdict bypasses the hold consult"]
    #[derive(Debug)]
    pub enum Regate {
        /// No active hold: the authority window restarts at this
        /// consult (the consumer's consult opportunity — R29').
        Refreshed,
        /// An active global hold — the phase MUST NOT proceed to its
        /// destructive batches.
        Held(ActiveHold),
    }

    /// The consult verdict for destructive actors — see [`gate`].
    #[derive(Debug)]
    pub enum HoldGate {
        /// No active global hold: the clearance is the capability the
        /// named delete sinks demand.
        Clear(HoldClearance),
        /// An active global hold — every destructive lane MUST skip.
        Held(ActiveHold),
    }

    // r[impl store.gc.hold-lanes+2]
    /// THE destructive-actor consult: one query, one verdict, the
    /// only mint point for [`HoldClearance`]. Callers MUST fail
    /// CLOSED on `Err` (an unreadable hold table is never read as
    /// "no hold") — the periodic form is
    /// `gc::lane::DestructiveLane`'s tick wrapper; the
    /// demand-driven form is `orphan::reap_one_consulted`. The
    /// minted clearance starts its authority window here
    /// (`consulted_at` = the mint instant).
    pub async fn gate(pool: &PgPool) -> Result<HoldGate, sqlx::Error> {
        Ok(match active_global_hold(pool).await? {
            Some(h) => HoldGate::Held(h),
            None => HoldGate::Clear(HoldClearance {
                consulted_at: tokio::time::Instant::now(),
                _proof: (),
            }),
        })
    }

    /// The first active GLOBAL hold, if any — `run_gc`'s entry consult.
    // r[impl store.gc.hold+2]
    pub async fn active_global_hold(pool: &PgPool) -> Result<Option<ActiveHold>, sqlx::Error> {
        sqlx::query_as(concat!(
            "SELECT hold_id, reason, created_by FROM gc_holds ",
            "WHERE scope = 'global' AND ",
            active_hold_predicate!(),
            " ORDER BY created_at LIMIT 1"
        ))
        .fetch_optional(pool)
        .await
    }
}

/// The outbox veto's TYPED liveness letter (bug_116, R30-hardened:
/// two modules narrated the SAME row population with OPPOSITE
/// liveness — collect.rs justified the reap's NOT-EXISTS conjunct as
/// "a FINITE wait, never a permanent veto" via a reset edge whose
/// sole production feeder is gated `deleted = FALSE`, structurally
/// unreachable for an already-tombstoned chunk absent PutPath
/// resurrection, while drain.rs honestly listed the stuck causes as
/// operator work: S3 permissions, key-format mismatch, Glacier).
/// Both narrations consume THIS alphabet, so an untrue finiteness
/// claim is no longer writable as prose: the finite-drain narration
/// is constructible only from the `FiniteDrain` variant.
// r[impl store.gc.outbox-veto-letter]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutboxVetoLiveness {
    /// The drain (or the reset feeder) reaches this row without
    /// operator action: in-budget rows drain on cadence; an
    /// exhausted row whose chunk is LIVE (`deleted = FALSE` — a
    /// post-exhaustion resurrection) re-enters the collect feeder
    /// when the chunk next ages out, and the fresh decision resets
    /// the budget (the feeder-witnessed exit edge).
    FiniteDrain,
    /// Exhausted row over a TOMBSTONED chunk: no production event
    /// resets it — the collect feeder only emits decisions for
    /// `deleted = FALSE` chunks. PARKED UNTIL OPERATOR ACTION (fix
    /// S3 permissions / key layout / storage class, then re-enqueue
    /// or release); the `_stuck` gauge is its alarm. The retention
    /// posture is the design — the prior FINITE-wait claim over this
    /// population was the defect.
    ParkedOperator,
}

impl OutboxVetoLiveness {
    /// Classify one outbox row's liveness from the facts the law
    /// ranges over: the retry budget and the chunk's tombstone state.
    pub(crate) fn classify(chunk_deleted: bool, attempts: i32) -> Self {
        if attempts >= drain::MAX_ATTEMPTS && chunk_deleted {
            Self::ParkedOperator
        } else {
            Self::FiniteDrain
        }
    }

    /// The variant's one narration — the strings live HERE and only
    /// here, so the two consuming docs/logs cannot diverge.
    pub(crate) fn narrate(self) -> &'static str {
        match self {
            Self::FiniteDrain => {
                "finite-drain: drains on cadence, or re-enters the collect \
                 feeder on the chunk's next aging-out (no operator needed)"
            }
            Self::ParkedOperator => {
                "parked-operator: no production event resets an exhausted \
                 row over a tombstoned chunk; operator action required \
                 (S3 permissions / key layout / storage class)"
            }
        }
    }
}

pub mod collect;
pub mod drain;
pub mod lane;
pub mod lock;
mod mark;
#[cfg(test)]
mod mark_scan_bench;
pub mod orphan;
pub mod state;
pub mod sweep;
pub mod tenant;

/// PG advisory lock ID for TriggerGC. Arbitrary constant — just
/// needs to not collide with other advisory locks in the schema
/// (currently the only one). "rOGC" ASCII + 1.
///
/// Serializes GC-vs-GC: two concurrent TriggerGC calls would
/// waste work and produce misleading stats. `pg_try_advisory_lock`
/// (non-blocking) — second caller gets "already running".
///
/// I-192: there is no longer a mark-vs-PutPath lock. PutPath's
/// `insert_manifest_uploading` writes `references` into the
/// placeholder narinfo at INSERT time; sweep's per-path re-check
/// (`narinfo."references" @> ARRAY[Q]`, fresh READ-COMMITTED snapshot
/// over ALL narinfo including `'uploading'`) is the sole load-bearing
/// guard. The mark lock was released before sweep anyway, so it never
/// participated in sweep-time safety — it only made PutPath wait for
/// the mark CTE, which doesn't change whether Q ends up in
/// `unreachable` or whether the re-check saves it. See
/// `r[store.gc.sweep-recheck+2]`.
pub const GC_LOCK_ID: i64 = 0x724F_4743_0001;

use std::sync::Arc;

use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::mpsc;
use tonic::Status;
use tracing::{info, warn};

use rio_proto::types::GcProgress;

use crate::backend::ChunkBackend;
#[cfg(test)]
use crate::manifest::{Manifest, ManifestError};

/// Summary stats from a GC run.
#[derive(Debug, Default, Clone)]
pub struct GcStats {
    /// Paths deleted from narinfo (and cascaded tables).
    pub paths_deleted: u64,
    /// Chunks soft-deleted by the collect cycle (run_gc phase 3); a
    /// dry run reports the cycle's would-collect estimate instead.
    pub chunks_deleted: u64,
    /// S3 keys enqueued to pending_s3_deletes by the collect cycle.
    pub s3_keys_enqueued: u64,
    /// Total bytes of chunks soft-deleted by the collect cycle (for
    /// storage savings estimate).
    pub bytes_freed: u64,
    /// Paths skipped because a new narinfo referenced them after
    /// mark (mark-vs-sweep race window — a PutPath completed BETWEEN
    /// mark and sweep with this path in its references). Sweep's
    /// per-path re-check catches these and skips the delete.
    /// Metric for alerting if this is frequent.
    pub paths_resurrected: u64,
}

/// Parameters for `run_gc`. Struct (not positional) so the
/// cron caller can express defaults clearly and the gRPC wrapper
/// can pass everything through without argument-order drift.
///
/// Audit C #27: was positional with `grace_hours` + `extra_roots`
/// missing — would have broken the gRPC API that accepts both.
pub struct GcParams {
    /// Compute stats, ROLLBACK sweep tx. Operator sees "would
    /// delete N paths" without committing.
    pub dry_run: bool,
    /// Paths younger than this are root seeds (don't GC what
    /// just arrived before a build can reference it). Already
    /// clamped at the gRPC boundary; clamped again in mark.rs
    /// (defense in depth).
    pub grace_hours: u32,
    /// Scheduler-populated live-build output paths. May not be
    /// in narinfo yet (worker hasn't uploaded); mark's CTE
    /// handles absent paths gracefully.
    pub extra_roots: Vec<String>,
}

/// Ceiling on `grace_hours` before the `as i32` bind. u32 > i32::MAX
/// wraps negative → `make_interval(hours => negative)` → grace covers
/// nothing → everything sweepable. One year is the practical max;
/// "infinite grace" is a `scheduler_live_pins` entry or `extra_roots`
/// pass, not a huge grace window.
pub(crate) const GRACE_HOURS_CAP: u32 = 24 * 365;

/// Mark → sweep with advisory locks. Extracted from `grpc/admin.rs::
/// trigger_gc` so it's callable outside the stream context (cron
/// reconciler in rio-controller).
///
/// Progress messages go to `progress_tx`. Send failures are ignored
/// (`let _ =`) — GC continues even if the consumer dropped. Callers
/// that don't want progress pass a channel and drop the rx.
///
/// # Advisory lock choreography
///
/// One session-scoped lock, one pool connection:
///
/// **[`GC_LOCK_ID`]** (`pg_try_advisory_lock`): serializes GC-vs-GC.
/// Held for the full run. Non-blocking — second caller gets a `false`
/// back → "already running" terminal progress msg.
///
/// The lease's lock is a `lock::PgSessionLock`: ANY exit that does
/// not run commit/release (error, task cancellation, panic) detaches
/// the lock connection → PG auto-releases on connection close;
/// release goes THROUGH the held connection, and only a clean unlock
/// returns it to the pool (bug_213 — defuse-before-await is
/// inexpressible).
///
/// There is no mark-vs-PutPath lock (I-192) — sweep's per-path
/// reference re-check is the sole concurrency guard. PutPath runs
/// freely throughout mark and sweep.
///
/// # Errors
///
/// Returns `Err(Status)` on pool-acquire/lock-query/mark/sweep failure.
/// Callers forward this into the progress stream as a terminal Err.
///
/// What run_gc phase 3 (the chunk-collect cycle) is allowed to tell
/// the operator (merged_bug_148). Minted exactly once per phase-3
/// arm and consumed by an EXHAUSTIVE match building the final
/// `GcProgress.current_path`: no failure arm can reach the
/// "complete:" string by construction, so a fail-closed suspension or
/// a mid-drain DB failure is never rendered as "0 chunks" (which
/// reads as "nothing to collect" on a destructive subsystem).
#[derive(Debug)]
enum Phase3Render {
    /// The cycle drained and its durable commit landed.
    Committed,
    /// The cycle drained but the gc_collect_state commit is PROVEN
    /// lost — a foreign winner sits at expected+1 with a mismatched
    /// payload (merged_bug_218; evidence discipline merged_bug_022) —
    /// stats are real, the cadence stamp is not.
    CommitLost,
    /// The cycle drained but neither commit leg could prove the
    /// outcome (merged_bug_022): the commit may or may not have
    /// landed. Degraded bookkeeping like CommitLost — exit 0.
    CommitIndeterminate,
    /// Dry run whose durable observation was withheld (corrupt
    /// manifest inside the simulated-swept set, merged_bug_147).
    PreviewOnly,
    /// Fail-closed ParseFailure: ALL chunk collection is suspended.
    Suspended,
    /// The cycle failed against PostgreSQL mid-drain; prior batches'
    /// soft-deletes/enqueues are already committed.
    Failed(String),
}

/// merged_bug_052: the crate-neutral outcome mirror BOTH suites assert
/// through ([`rio_common::classify::GcPhase3Outcome`] — the
/// `AttemptTerminalKind` precedent: the exhaustive `From` lives next
/// to the mirrored enum). `render_phase3` renders THROUGH the shared
/// prefix constants and the CLI matches THROUGH the shared predicate,
/// so a reword is a one-site const edit, never a silent exit-0 on a
/// failed destructive collect.
impl From<&Phase3Render> for rio_common::classify::GcPhase3Outcome {
    fn from(render: &Phase3Render) -> Self {
        use rio_common::classify::GcPhase3Outcome as O;
        match render {
            Phase3Render::Committed => O::Committed,
            Phase3Render::CommitLost => O::CommitLost,
            Phase3Render::CommitIndeterminate => O::CommitIndeterminate,
            Phase3Render::PreviewOnly => O::PreviewOnly,
            Phase3Render::Suspended => O::Suspended,
            Phase3Render::Failed(_) => O::Failed,
        }
    }
}

/// Returns `Ok(None)` when another GC holds [`GC_LOCK_ID`] — the
/// "already running" terminal progress message is sent, but this
/// isn't an error.
pub async fn run_gc(
    pool: &PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    params: GcParams,
    progress_tx: mpsc::Sender<Result<GcProgress, Status>>,
    shutdown: &rio_common::signal::Token,
) -> Result<Option<GcStats>, Status> {
    // --- Concurrency guard: pg_try_advisory_lock ---
    // Two TriggerGC calls -> two concurrent mark+sweep.
    // Correctness is OK (FOR UPDATE + rows_affected checks
    // in sweep) but it wastes work, produces misleading
    // stats (GC2 finds everything already swept), and
    // creates lock contention. One-at-a-time via the GC cycle lease
    // (the session advisory lock + the durable gc_collect_state
    // snapshot); the second caller gets an immediate "already
    // running" response.
    //
    // bug_213: the lease's lock is a PgSessionLock — release goes
    // THROUGH the held connection and a failed/cancelled release
    // detaches (closes) it, so PG frees the lock with the session.
    // The pre-wave defuse-then-await choreography could return the
    // connection to the shared pool with the lock still held (the
    // next run_gc then read "already running" until the pool happened
    // to recycle that connection — bounded by the 60s idle_timeout
    // only on an IDLE pool; a busy pool can keep a pooled connection
    // alive indefinitely).
    // r[impl store.gc.serialize-lock]
    let Some(mut lease) = state::GcCycleLease::try_acquire(pool).await.map_err(|e| {
        warn!(error = %e, "GC: cycle lease acquire failed");
        Status::internal(format!("cycle lease: {e}"))
    })?
    else {
        info!("GC: another GC is already running, returning early");
        let _ = progress_tx
            .send(Ok(GcProgress {
                paths_scanned: 0,
                paths_collected: 0,
                bytes_freed: 0,
                is_complete: true,
                current_path: "already running (concurrent GC in progress)".into(),
            }))
            .await;
        return Ok(None);
    };

    // --- Global-hold consult (round-9 WO-S1-4, signed Q3) ---
    // r[impl store.gc.hold+2]
    // A FIRST-CLASS operator hold replaces the freeze-by-scale-to-0
    // workaround: an active GLOBAL hold makes the whole collection
    // pass (mark, sweep, chunk-collect) a no-op — consulted once at
    // entry, inside the lease so two racing runs serialize their
    // verdicts. A hold set mid-run stops THIS run at its next batch
    // boundary (bug_084 — the per-batch law replaced the round-9
    // "binds the NEXT run" posture: phase-2 path-delete batches and
    // phase-3 collect batches each demand fresh BatchAuthority, so an
    // operator's emergency stop binds within one batch, not one run).
    // Tenant-scoped holds bind inside mark's seed (f) and the sweep
    // re-check instead — and the re-check carries the global conjunct
    // too (defense in depth inside an in-flight batch).
    // run_gc is the PINNED member of the destructive-lane census
    // (RPC-spawned per TriggerGC, not spawn-periodic): its entry
    // consult mints the same HoldClearance the lane wrapper does,
    // and the clearance threads to BOTH destructive phases — the
    // phase-2 path sweep and the phase-3 chunk-collect — whose sinks
    // demand per-batch authority (store.gc.hold-lanes+2), and a
    // drain-bound-aged clearance authorizes nothing further
    // (store.gc.clearance-expiry+2).
    let mut hold_clearance = match hold::gate(pool).await {
        Ok(hold::HoldGate::Held(h)) => {
            info!(
                hold_id = %h.hold_id,
                created_by = %h.created_by,
                reason = %h.reason,
                "GC: active global hold; collection suspended"
            );
            metrics::counter!(
                "rio_store_gc_hold_lane_skips_total",
                "lane" => "run_gc", "cause" => "held"
            )
            .increment(1);
            // r[impl store.gc.hold-lanes+2]
            // The starvation coupling dies HERE (merged_bug_050): a
            // held cycle is a live cycle for staleness purposes —
            // stamping keeps the backstop's due-ness clock quiet so
            // the hold itself can never make the backstop fire (and
            // the stalled alert stays silent through the freeze).
            // Best-effort: a failed stamp degrades to the pre-fix
            // due-ness shape, which the lane wrapper now skips
            // anyway — defense in depth, not a correctness edge.
            if let Err(e) = state::stamp_held_cycle(pool).await {
                warn!(error = %e, "GC: held-cycle stamp failed (lane wrapper still suspends)");
            }
            let _ = progress_tx
                .send(Ok(GcProgress {
                    paths_scanned: 0,
                    paths_collected: 0,
                    bytes_freed: 0,
                    is_complete: true,
                    current_path: format!(
                        "held: global gc hold active (reason: {}; set by {})",
                        h.reason, h.created_by
                    ),
                }))
                .await;
            let _ = lease.release().await;
            return Ok(None);
        }
        Ok(hold::HoldGate::Clear(c)) => c,
        Err(e) => {
            // Fail CLOSED on a destructive subsystem: an unreadable
            // hold table must not be read as "no hold".
            warn!(error = %e, "GC: hold consult failed; refusing to collect");
            metrics::counter!(
                "rio_store_gc_hold_lane_skips_total",
                "lane" => "run_gc", "cause" => "consult_error"
            )
            .increment(1);
            let _ = lease.release().await;
            return Err(Status::internal(format!("gc-hold consult: {e}")));
        }
    };

    // --- Mark phase ---
    // No mark-vs-PutPath lock (I-192). Mark's CTE takes a point-in-time
    // MVCC snapshot; a PutPath placeholder that commits after the
    // snapshot is invisible to mark but visible to sweep's per-path
    // re-check (fresh READ-COMMITTED snapshot over ALL narinfo). The
    // re-check is the load-bearing guard; the lock added nothing on
    // top — it was released before sweep anyway.
    let unreachable =
        match mark::compute_unreachable(pool, params.grace_hours, &params.extra_roots).await {
            Ok(u) => u,
            Err(e) => {
                warn!(error = %e, "GC: mark phase failed");
                let _ = lease.release().await;
                return Err(Status::internal(format!("mark phase: {e}")));
            }
        };

    // Progress after mark: scanned count. We don't have
    // a "total paths" count cheaply (would need COUNT(*)
    // on narinfo), so paths_scanned = unreachable count
    // (what mark found). Captured here so the FINAL message
    // can report the same number — `unreachable` is moved into
    // sweep, and `stats.paths_deleted` regresses below this
    // mid-progress value when paths_resurrected > 0.
    let found_unreachable = unreachable.len() as u64;
    let _ = progress_tx
        .send(Ok(GcProgress {
            paths_scanned: found_unreachable,
            paths_collected: 0,
            bytes_freed: 0,
            is_complete: false,
            current_path: "mark complete, starting sweep".into(),
        }))
        .await;

    info!(
        unreachable = unreachable.len(),
        "GC: mark complete, starting sweep"
    );

    // --- The post-mark consult seam (merged_bug_081, R29') ---
    // Mark is read-only and can dwarf the drain-cadence bound at
    // scale; the seam restarts the authority window where consumption
    // starts (the sweep's first batch) and refuses under a hold
    // landed during mark — nothing destructive has happened yet, so
    // a held seam exits exactly like the entry consult.
    match hold_clearance.regate(pool).await {
        Ok(hold::Regate::Refreshed) => {}
        Ok(hold::Regate::Held(h)) => {
            info!(
                hold_id = %h.hold_id,
                created_by = %h.created_by,
                reason = %h.reason,
                "GC: global hold landed during mark; collection suspended"
            );
            metrics::counter!(
                "rio_store_gc_hold_lane_skips_total",
                "lane" => "run_gc", "cause" => "held"
            )
            .increment(1);
            let _ = progress_tx
                .send(Ok(GcProgress {
                    paths_scanned: found_unreachable,
                    paths_collected: 0,
                    bytes_freed: 0,
                    is_complete: true,
                    current_path: format!(
                        "held: global gc hold landed during mark (reason: {}; set by {})",
                        h.reason, h.created_by
                    ),
                }))
                .await;
            let _ = lease.release().await;
            return Ok(None);
        }
        Err(e) => {
            warn!(error = %e, "GC: post-mark hold consult failed; refusing to sweep");
            let _ = lease.release().await;
            return Err(Status::internal(format!("gc-hold consult: {e}")));
        }
    }

    // --- Sweep phase ---
    // Shutdown token threaded through: sweep checks it between
    // batches (not mid-transaction — a partial batch ROLLBACKs
    // cleanly via tx drop). Returns SweepAbort::Shutdown if fired.
    // The clearance threads through (bug_084): every path-delete
    // batch demands fresh BatchAuthority, so a global hold landing
    // mid-pass stops the sweep at the next batch boundary instead of
    // riding the entry consult through thousands of batches.
    let sweep_outcome = match sweep::sweep(
        pool,
        chunk_backend.as_ref(),
        unreachable,
        params.dry_run,
        shutdown,
        &mut hold_clearance,
    )
    .await
    {
        Ok(s) => s,
        // r[impl store.gc.shutdown-abort]
        Err(sweep::SweepAbort::Shutdown) => {
            info!("GC: sweep aborted by shutdown signal");
            let _ = lease.release().await;
            return Err(Status::aborted("GC aborted: process shutting down"));
        }
        Err(sweep::SweepAbort::Db(e)) => {
            warn!(error = %e, "GC: sweep phase failed");
            let _ = lease.release().await;
            return Err(Status::internal(format!("sweep phase: {e}")));
        }
    };
    let sweep::SweepOutcome {
        mut stats,
        swept_paths,
        clearance_stop: sweep_stop,
    } = sweep_outcome;

    // A clearance-refused sweep stop suspends the REST of the run too
    // (bug_084): phase 3 must not start under a hold the sweep just
    // refused — and an expired clearance authorizes nothing further.
    // Committed batches stand (their stats report); the next run (or
    // the released hold's next pass) re-marks and finishes. The same
    // skip-counter the entry consult uses records the suspension.
    if let Some(stop) = sweep_stop {
        let cause = match stop {
            hold::ClearanceStop::Held => "held",
            hold::ClearanceStop::Expired => "expired",
        };
        info!(
            cause,
            paths_deleted = stats.paths_deleted,
            "GC: path sweep stopped at a batch boundary; \
             skipping phase 3 (collection suspended mid-pass)"
        );
        metrics::counter!(
            "rio_store_gc_hold_lane_skips_total",
            "lane" => "run_gc", "cause" => "mid_pass_stop"
        )
        .increment(1);
        let _ = progress_tx
            .send(Ok(GcProgress {
                paths_scanned: found_unreachable,
                paths_collected: stats.paths_deleted,
                bytes_freed: stats.bytes_freed,
                is_complete: true,
                current_path: format!(
                    "suspended mid-pass at a batch boundary (clearance {cause}); \
                     committed batches stand, the next run finishes"
                ),
            }))
            .await;
        let _ = lease.release().await;
        return Ok(Some(stats));
    }

    // --- Phase 3: chunk-collect cycle (the live collect arm) ---
    // Runs while GC_LOCK_ID is still held: the cycle uses its own
    // pooled connection for the session temp table; the advisory lock
    // stays on lock_conn (same split the sweep's temp table uses).
    // A dry-run GC keeps phase 3 observation-only (Shadow mode) so a
    // dry run never deletes anything; a real run collects (capped per
    // cycle, cursor-resumable). A phase-3 failure never affects the
    // path GC that just committed — log and continue; the daily
    // backstop (and the next GC run) retries. A parse-failure abort is
    // reported inside the cycle (counter + error log), not as an Err.
    // The dry-run estimate is computed against SIMULATED post-sweep
    // state (bug_199): the swept set this run's (rolled-back) sweep
    // settled feeds the shadow mark exclusion, so the report counts
    // the would-be-swept manifests' chunks exactly as a live run
    // would leave them.
    let collect_mode = if params.dry_run {
        collect::CollectMode::Shadow {
            simulated_swept: swept_paths,
        }
    } else {
        collect::CollectMode::Live
    };
    // merged_bug_148: every phase-3 arm mints exactly one render; the
    // final progress send consumes it by exhaustive match.
    let phase3: Phase3Render;
    // bug_284: a live phase 3 is an ATTEMPT — stamp it before the
    // cycle so the backstop's throttle conjunct sees operator-
    // triggered heavy scans too. Warn-only: an operator GC must not
    // abort over cadence bookkeeping (the sweep already committed);
    // dry runs never stamp (a dry run must not defer live cadence).
    if !params.dry_run
        && let Err(e) = lease.stamp_attempt().await
    {
        warn!(error = %e, "GC: collect attempt stamp failed (cadence bookkeeping only)");
    }
    let resume_cursor = lease.state.cursor.clone();
    match collect::collect_cycle(
        pool,
        chunk_backend.as_ref(),
        sweep::CHUNK_GRACE_SECS,
        collect_mode,
        resume_cursor,
        &mut hold_clearance,
    )
    .await
    {
        Ok(report) => {
            // P11: from the cutover release on, the chunk-level GC
            // stats (chunks deleted / bytes freed / S3 keys enqueued)
            // are sourced from the collect cycle, not the path sweep;
            // a dry run reports the would-collect estimate instead.
            if params.dry_run {
                stats.chunks_deleted = report.would_collect;
                stats.bytes_freed = report.would_collect_bytes;
                stats.s3_keys_enqueued = if chunk_backend.is_some() {
                    report.would_collect
                } else {
                    0
                };
            } else {
                stats.chunks_deleted = report.victims_collected;
                stats.bytes_freed = report.victim_bytes;
                stats.s3_keys_enqueued = report.s3_keys_enqueued;
            }

            // The cycle's observation/work lands in the durable
            // gc_collect_state row (migration 090) — cadence, cursor,
            // and the gauge sources every replica publishes from. A
            // parse-failure abort is NOT a cycle: no stamp, the lock
            // is simply released (fail-closed; retention stays
            // visibly stalled until the manifest is repaired).
            // merged_bug_218: ok rides the commit witness (minted at
            // the durability point). merged_bug_022: the result is
            // three-valued and consumed EXHAUSTIVELY — commit_failed
            // means PROVEN lost (foreign winner), commit_indeterminate
            // means unprovable either way; detailed retry evidence
            // (expected/observed epoch, echo) is logged at the
            // classification site in state.rs.
            let record_commit = |committed: state::CycleCommitResult| match committed {
                state::CycleCommitResult::Committed(w) => {
                    w.record_ok_outcome();
                    Phase3Render::Committed
                }
                state::CycleCommitResult::NotCommitted(e) => {
                    metrics::counter!(
                        "rio_store_gc_collect_cycles_total",
                        "outcome" => "commit_failed"
                    )
                    .increment(1);
                    warn!(
                        error = %e,
                        "GC: collect commit PROVEN lost to a foreign winner \
                         (stamp/cursor/backlog not updated; lock freed via \
                         session close)"
                    );
                    Phase3Render::CommitLost
                }
                state::CycleCommitResult::Ambiguous(e) => {
                    metrics::counter!(
                        "rio_store_gc_collect_cycles_total",
                        "outcome" => "commit_indeterminate"
                    )
                    .increment(1);
                    warn!(
                        error = %e,
                        "GC: collect commit INDETERMINATE (may or may not have \
                         landed; see the retry classification logs)"
                    );
                    Phase3Render::CommitIndeterminate
                }
            };
            phase3 = match report.outcome {
                collect::CollectOutcome::Ok => {
                    if params.dry_run {
                        match report.durable {
                            Some(observation) => record_commit(
                                lease
                                    .commit_cycle(state::CycleCommit::Shadow { observation })
                                    .await,
                            ),
                            None => {
                                // merged_bug_147: the real-basis
                                // validation found corruption inside
                                // the simulated-swept set — the dry
                                // run stays preview-only. Nothing
                                // committed, so NO outcome tick (ok
                                // means "the durable commit landed").
                                warn!(
                                    "GC: durable observation withheld (corrupt manifest \
                                     in the simulated-swept set); gc_collect_state untouched"
                                );
                                if let Err(e) = lease.release().await {
                                    warn!(error = %e, "GC: lease release failed (lock freed via session close)");
                                }
                                Phase3Render::PreviewOnly
                            }
                        }
                    } else {
                        record_commit(
                            lease
                                .commit_cycle(state::CycleCommit::Live {
                                    disposition: report
                                        .disposition
                                        .clone()
                                        .expect("live Ok report carries a disposition"),
                                    victims_collected: report.victims_collected,
                                    observation: report
                                        .durable
                                        .expect("live Ok cycle carries an observation"),
                                })
                                .await,
                        )
                    }
                }
                collect::CollectOutcome::ParseFailure => {
                    if let Err(e) = lease.release().await {
                        warn!(error = %e, "GC: lease release failed (lock freed via session close)");
                    }
                    Phase3Render::Suspended
                }
            };
            info!(
                outcome = ?report.outcome,
                dry_run = params.dry_run,
                mark_set_size = report.mark_set_size,
                would_collect = report.would_collect,
                victims_collected = report.victims_collected,
                victim_bytes = report.victim_bytes,
                s3_keys_enqueued = report.s3_keys_enqueued,
                batches_run = report.batches_run,
                cap_reached = report.cap_reached,
                clearance_stop = ?report.clearance_stop,
                pass_complete = report.pass_complete(),
                chunks_reaped = report.chunks_reaped,
                cursor_at_stop = ?report.cursor_at_stop().map(hex::encode),
                cycle_seconds = report.cycle_seconds,
                "GC: collect phase 3 complete"
            );
        }
        Err(e) => {
            // Same error-outcome accounting as the backstop caller: a
            // cycle that fails against PostgreSQL is visible immediately
            // instead of only via the 25h stalled alert.
            metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "error")
                .increment(1);
            warn!(error = %e, "GC: collect phase 3 failed");
            if let Err(e2) = lease.release().await {
                warn!(error = %e2, "GC: lease release after failed cycle (lock freed via session close)");
            }
            phase3 = Phase3Render::Failed(e.to_string());
        }
    }

    // Final progress: complete with stats. paths_scanned echoes the
    // mid-progress `found_unreachable` so it never goes backward;
    // resurrections surface in the `current_path` summary string
    // (proto has no `paths_resurrected` field — adding one is a
    // cross-crate change deferred to keep this fix store-local).
    let current_path = render_phase3(&phase3, params.dry_run, &stats);
    let _ = progress_tx
        .send(Ok(GcProgress {
            paths_scanned: found_unreachable,
            paths_collected: stats.paths_deleted,
            bytes_freed: stats.bytes_freed,
            is_complete: true,
            current_path,
        }))
        .await;

    Ok(Some(stats))
}

/// Build the terminal frame's `current_path` from the typed phase-3
/// render (pure extraction of run_gc's final-send builder so the
/// posture-totality test can drive every variant).
///
/// merged_bug_148: an EXHAUSTIVE match over [`Phase3Render`] — no
/// failure arm can produce the "complete:"/"dry run:" success summary
/// by construction. The failure frames open with the SHARED prefix
/// constants ([`rio_common::classify::GC_CHUNK_COLLECT_SUSPENDED_PREFIX`]
/// / [`rio_common::classify::GC_CHUNK_COLLECT_FAILED_PREFIX`]) — the
/// S6b consumer contract (`rio-cli gc` keys its exit posture on them
/// through `classify::gc_render_is_chunk_collect_failure`;
/// merged_bug_052: the contract is the shared constants, never
/// re-typed literals). `is_complete` stays true on every arm — the
/// stream's end-sentinel semantics are unchanged, exit posture is the
/// consumer's decision.
fn render_phase3(phase3: &Phase3Render, dry_run: bool, stats: &GcStats) -> String {
    use rio_common::classify::{GC_CHUNK_COLLECT_FAILED_PREFIX, GC_CHUNK_COLLECT_SUSPENDED_PREFIX};
    let success_summary = || {
        if dry_run {
            format!(
                "dry run: would delete {} paths, {} chunks, free {} bytes, {} resurrected",
                stats.paths_deleted,
                stats.chunks_deleted,
                stats.bytes_freed,
                stats.paths_resurrected
            )
        } else {
            format!(
                "complete: {} paths deleted, {} chunks, {} S3 keys enqueued, {} bytes freed, {} resurrected",
                stats.paths_deleted,
                stats.chunks_deleted,
                stats.s3_keys_enqueued,
                stats.bytes_freed,
                stats.paths_resurrected
            )
        }
    };
    match phase3 {
        Phase3Render::Committed => success_summary(),
        Phase3Render::CommitLost => format!(
            "{}; collect-state commit LOST (cadence stamp not updated; see server logs)",
            success_summary()
        ),
        Phase3Render::CommitIndeterminate => format!(
            "{}; collect-state commit INDETERMINATE (may or may not have landed; \
             see server logs)",
            success_summary()
        ),
        Phase3Render::PreviewOnly => format!(
            "{}; chunk-collect durable observation WITHHELD \
             (corrupt manifest in the simulated-swept set)",
            success_summary()
        ),
        Phase3Render::Suspended => format!(
            "{GC_CHUNK_COLLECT_SUSPENDED_PREFIX} unparseable chunk_list aborted the cycle \
             fail-closed; {} paths {}; chunk stats unavailable until the manifest is \
             repaired, deleted, or quarantined",
            stats.paths_deleted,
            if dry_run {
                "would be deleted"
            } else {
                "deleted"
            },
        ),
        Phase3Render::Failed(e) => format!(
            "{GC_CHUNK_COLLECT_FAILED_PREFIX} {e}; {} paths {}; partial chunk work may \
             already be committed (see server logs)",
            stats.paths_deleted,
            if dry_run {
                "would be deleted"
            } else {
                "deleted"
            },
        ),
    }
}

/// Enqueue S3 keys for soft-deleted chunks to `pending_s3_deletes` in
/// the given transaction. Batched via unnest — one RTT per call
/// instead of per-chunk (a 1000-chunk collect batch would otherwise
/// need 1000 INSERTs at ~1ms RTT = ~1s; batched it's ~1ms).
///
/// `blake3_hash` is written alongside `s3_key` so the drain task can
/// re-check `chunks.deleted` before issuing the S3 DELETE — catches
/// the TOCTOU where PutPath resurrected the chunk after we enqueued
/// it.
///
/// THE CONFLICT ARM IS THE OUTBOX'S EXIT EDGE (bug_111, R30): a
/// duplicate enqueue against a row whose budget REMAINS
/// (`attempts < MAX_ATTEMPTS`) is swallowed — the designed dedup,
/// guarded by the DO UPDATE's WHERE — but against an EXHAUSTED row
/// (`attempts >= MAX_ATTEMPTS`, parked outside the drain's partial
/// index) the fresh collect decision RESETS the budget
/// (`attempts = 0, enqueued_at = now()`): exhaustion is not
/// absorbing, because a fresh decision for the same object is
/// exactly the event that logically restarts the retry budget. The
/// pre-fix `ON CONFLICT DO NOTHING` swallowed the re-decision too,
/// so a ~5min transient S3 outage leaked the object with no retry
/// path and turned the reap qual's NOT-EXISTS conjunct into a
/// permanent veto on the chunk tombstone's hard-delete.
/// THE RESET CARRIES THE FRESH DECISION WHOLE (merged_bug_117, R30
/// hardened): a reset arm implementing "a fresh decision restarts the
/// row" carries EVERY column the fresh decision recomputed. The
/// decision-derived column audit (the R4 line, per column):
/// `s3_key` (carries `EXCLUDED.s3_key`) — fixed-here (the wave-11 arm kept the
/// stale key; after a backend key-layout migration the drain then
/// deleted a ghost at the old key and the real object leaked silently
/// past the reap conjunct); `attempts = 0`, `enqueued_at = now()` —
/// verified (carried since bug_111); `blake3_hash` — n/a (the
/// conflict key itself); `last_error` — verified-by-design
/// (intentionally retained as forensic context until the next attempt
/// overwrites it). The conflict target names migration 024's partial
/// unique index (`blake3_hash WHERE blake3_hash IS NOT NULL`); rows
/// enqueued here always carry a hash.
///
/// Skips hashes that fail `try_from` to `[u8; 32]` (can't-happen — the
/// `chunks` PK is BYTEA but every writer inserts exactly 32 bytes;
/// `warn!` + skip rather than panic so one corrupt row doesn't kill
/// the collect batch). Returns `rows_affected()` — rows INSERTED or
/// RESET (in-budget duplicates are WHERE-false no-ops and do NOT
/// count, so the enqueued-total counter measures what its HELP
/// claims; the pre-fix `keys.len()` return counted the swallowed
/// dups).
///
/// No-op if `backend` is None (inline-only store has no S3 keys).
///
/// Demands the batch's [`hold::BatchAuthority`] BY VALUE (bug_084,
/// R32): the enqueue is the collect batch's outbox sink — the token
/// minted at that batch's boundary consult is consumed here, so an
/// enqueue outside an authorized batch does not compile.
// r[impl store.gc.pending-deletes+2]
// r[impl store.gc.batch-authority]
pub(super) async fn enqueue_chunk_deletes(
    tx: &mut Transaction<'_, Postgres>,
    soft_deleted: &[(Vec<u8>, i64)],
    backend: Option<&Arc<dyn ChunkBackend>>,
    authority: hold::BatchAuthority,
) -> Result<u64, sqlx::Error> {
    // The token is spent: one authority, one batch, this sink.
    authority.spend();
    let Some(backend) = backend else {
        return Ok(0);
    };
    if soft_deleted.is_empty() {
        return Ok(0);
    }
    // r[impl store.chunk.lock-order+2]
    // Sort by hash before building the parallel keys/hashes vecs: the
    // input is a RETURNING set (PG internal order, NOT input-array
    // order). The pending_s3_deletes INSERT below binds UNNEST() —
    // unsorted → circular-wait against a concurrent
    // enqueue_chunk_deletes. One sort here covers all callers (the
    // collect batches). The .to_vec() clone is cheap (~KB) relative
    // to the PG roundtrip.
    let mut soft_deleted: Vec<_> = soft_deleted.to_vec();
    soft_deleted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut keys: Vec<String> = Vec::with_capacity(soft_deleted.len());
    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(soft_deleted.len());
    for (hash, _size) in &soft_deleted {
        let Ok(arr) = <[u8; 32]>::try_from(hash.as_slice()) else {
            warn!(
                len = hash.len(),
                "GC: chunk hash wrong length, skipping S3 enqueue"
            );
            continue;
        };
        keys.push(backend.key_for(&arr));
        hashes.push(hash.clone());
    }
    if keys.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "INSERT INTO pending_s3_deletes (s3_key, blake3_hash) \
         SELECT * FROM unnest($1::text[], $2::bytea[]) \
         ON CONFLICT (blake3_hash) WHERE blake3_hash IS NOT NULL \
         DO UPDATE SET (attempts, enqueued_at, s3_key) = (0, now(), EXCLUDED.s3_key) \
         WHERE pending_s3_deletes.attempts >= $3",
    )
    .bind(&keys)
    .bind(&hashes)
    .bind(crate::gc::drain::MAX_ATTEMPTS)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

/// Deserialize a manifest's `chunk_list` and return its dedup'd chunk
/// hashes, sorted ascending. A manifest CAN repeat chunks (duplicate
/// content blocks in the NAR) — each unique hash appears exactly once.
///
/// Corrupt input (anything `Manifest::deserialize` rejects) is an
/// `Err`, never an empty `Ok`, so a caller that must fail closed can
/// distinguish "this manifest references no chunks" from "this
/// manifest is unreadable". An empty manifest (zero entries) is NOT
/// corrupt: it parses to `Ok` of an empty vec.
///
/// The collector's production parse is the server-side validation
/// pass + set-based expansion inside `collect_cycle`; this Rust-side
/// parse is the test/bench oracle for it — the differential-pinning
/// test compares the SQL expansion against this function over the
/// same rows, and the mark-scan bench uses it when synthesizing and
/// auditing fixtures. Test-only since the legacy decrement paths
/// (its last production callers) were deleted with the counter
/// writers.
///
/// The ascending sort gives a deterministic order independent of the
/// manifest's entry order; the consumers are order-insensitive.
#[cfg(test)]
pub(crate) fn try_parse_unique_chunk_hashes(
    chunk_list: &[u8],
) -> Result<Vec<[u8; 32]>, ManifestError> {
    let manifest = Manifest::deserialize(chunk_list)?;
    let mut hashes: Vec<[u8; 32]> = manifest.entries.into_iter().map(|e| e.hash).collect();
    hashes.sort_unstable();
    hashes.dedup();
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestEntry};
    use crate::test_helpers::mem_backend;
    use rio_test_support::TestDb;

    /// A `ChunkBackend` whose `delete_by_key` fails (non-auth) until
    /// released — the injected S3 outage for the outbox tests.
    struct OutageBackend {
        inner: std::sync::Arc<dyn crate::backend::ChunkBackend>,
        healthy: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::backend::ChunkBackend for OutageBackend {
        async fn put(&self, h: &[u8; 32], d: bytes::Bytes) -> anyhow::Result<()> {
            self.inner.put(h, d).await
        }
        async fn get(&self, h: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
            self.inner.get(h).await
        }
        async fn exists_batch(&self, h: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            self.inner.exists_batch(h).await
        }
        fn key_for(&self, h: &[u8; 32]) -> String {
            self.inner.key_for(h)
        }
        async fn delete_by_key(&self, k: &str) -> anyhow::Result<()> {
            if self.healthy.load(std::sync::atomic::Ordering::SeqCst) {
                self.inner.delete_by_key(k).await
            } else {
                anyhow::bail!("injected S3 outage: transient delete failure")
            }
        }
    }

    /// Enqueue `hashes` through the PRODUCTION outbox statement
    /// (`enqueue_chunk_deletes`) inside its own committed transaction
    /// — the fresh-collect-decision producer, never hand-rolled SQL.
    async fn enqueue_via_production(
        pool: &sqlx::PgPool,
        backend: &Arc<dyn crate::backend::ChunkBackend>,
        hashes: &[[u8; 32]],
    ) -> u64 {
        let mut tx = pool.begin().await.unwrap();
        let soft: Vec<(Vec<u8>, i64)> = hashes.iter().map(|h| (h.to_vec(), 8)).collect();
        let n = enqueue_chunk_deletes(
            &mut tx,
            &soft,
            Some(backend),
            crate::test_helpers::gc_batch_authority(pool).await,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        n
    }

    // r[verify store.gc.outbox-reset+2]
    /// W11-AP (bug_111, R30): from the EXHAUSTED outbox state, a fresh
    /// producer decision reaches execution — the exit edge of the
    /// absorbing state, witnessed at the outbox's own lattice.
    ///
    /// Schedule: an injected S3 outage exhausts a pending_s3_deletes
    /// row through the production drain (MAX_ATTEMPTS real failures);
    /// the backend recovers; a LATER fresh collect decision for the
    /// same object re-enqueues through the production statement.
    /// Pre-fix red (verbatim in the commit body): the decision was
    /// swallowed by ON CONFLICT DO NOTHING — the drain never retried
    /// (zero deletes after recovery) and the object leaked past
    /// backend recovery while the reap conjunct stayed a permanent
    /// veto. Post-fix: attempts reset to 0, the drain retries and
    /// deletes, the pending row leaves, and the reap qual's three
    /// conjuncts (tombstoned, aged, no outbox row) all pass.
    ///
    /// LATCH-FACE cell (the designed dedup, WITNESSED not asserted —
    /// the law's own IFF quantifier over {exhausted, non-exhausted} ×
    /// fresh-decision): a NON-exhausted in-flight row (attempts = 3
    /// via injected failures) receiving a duplicate enqueue through
    /// the same production path is STILL swallowed — attempts AND
    /// enqueued_at unchanged. Red under an unguarded DO UPDATE; green
    /// under the guarded reset.
    #[tokio::test]
    async fn fresh_collect_decision_resets_exhausted_outbox_row() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let outage = std::sync::Arc::new(OutageBackend {
            inner: mem_backend(),
            healthy: std::sync::atomic::AtomicBool::new(false),
        });
        let backend: Arc<dyn crate::backend::ChunkBackend> =
            std::sync::Arc::clone(&outage) as Arc<dyn crate::backend::ChunkBackend>;

        // Chunk X: tombstoned (the collect soft-delete shape), object
        // in the backend, outbox row enqueued via PRODUCTION.
        let hash_x = [0xA1u8; 32];
        backend
            .put(&hash_x, bytes::Bytes::from_static(b"exhausted-prey"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, size, deleted, deleted_at) \
             VALUES ($1, 14, true, now() - interval '2 days')",
        )
        .bind(hash_x.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            enqueue_via_production(&db.pool, &backend, &[hash_x]).await,
            1
        );

        // The outage exhausts the row through the production drain.
        for _ in 0..crate::gc::drain::MAX_ATTEMPTS {
            let (deleted, failed) = crate::gc::drain::drain_once(
                &db.pool,
                &backend,
                &mut crate::test_helpers::gc_clearance(&db.pool).await,
            )
            .await
            .unwrap();
            assert_eq!(
                (deleted, failed),
                (0, 1),
                "each outage tick burns one attempt"
            );
        }
        let attempts: i32 =
            sqlx::query_scalar("SELECT attempts FROM pending_s3_deletes WHERE blake3_hash = $1")
                .bind(hash_x.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            attempts,
            crate::gc::drain::MAX_ATTEMPTS,
            "the row is exhausted"
        );

        // The backend recovers. Absent a fresh decision the row stays
        // parked (the designed operator surface) — the exit edge is
        // the RE-DECISION, not the recovery.
        outage
            .healthy
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // A LATER fresh collect decision for the same object.
        enqueue_via_production(&db.pool, &backend, &[hash_x]).await;

        // The drain retries and the delete executes.
        let (deleted, failed) = crate::gc::drain::drain_once(
            &db.pool,
            &backend,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!(
            (deleted, failed),
            (1, 0),
            "the fresh decision resets the budget: the drain retries and deletes \
             (pre-fix: swallowed, zero drain attempts ever after recovery)"
        );
        assert!(
            backend.get(&hash_x).await.unwrap().is_none(),
            "the leaked object is gone after the reset edge"
        );
        // The reap conjuncts all pass now: tombstoned, aged, no outbox
        // row — the permanent veto is gone.
        let (tombstoned, aged, outbox_rows): (bool, bool, i64) = sqlx::query_as(
            "SELECT deleted, deleted_at < now() - interval '1 day', \
                    (SELECT COUNT(*) FROM pending_s3_deletes p \
                      WHERE p.blake3_hash = chunks.blake3_hash) \
               FROM chunks WHERE blake3_hash = $1",
        )
        .bind(hash_x.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            tombstoned && aged && outbox_rows == 0,
            "the reap conjunct unblocks"
        );
    }

    // r[verify store.gc.outbox-reset+2]
    /// W11-AP latch-face cell — see
    /// [`fresh_collect_decision_resets_exhausted_outbox_row`]'s doc:
    /// the non-exhausted half of the IFF.
    #[tokio::test]
    async fn duplicate_enqueue_still_swallowed_while_attempts_remain() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let outage = std::sync::Arc::new(OutageBackend {
            inner: mem_backend(),
            healthy: std::sync::atomic::AtomicBool::new(false),
        });
        let backend: Arc<dyn crate::backend::ChunkBackend> =
            std::sync::Arc::clone(&outage) as Arc<dyn crate::backend::ChunkBackend>;

        let hash_y = [0xB2u8; 32];
        backend
            .put(&hash_y, bytes::Bytes::from_static(b"in-flight-prey"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, size, deleted, deleted_at) \
             VALUES ($1, 14, true, now() - interval '2 days')",
        )
        .bind(hash_y.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            enqueue_via_production(&db.pool, &backend, &[hash_y]).await,
            1
        );

        // In-flight: 0 < attempts < MAX via injected failures.
        for _ in 0..3 {
            let (_, failed) = crate::gc::drain::drain_once(
                &db.pool,
                &backend,
                &mut crate::test_helpers::gc_clearance(&db.pool).await,
            )
            .await
            .unwrap();
            assert_eq!(failed, 1);
        }
        let (attempts_before, enqueued_before): (i32, String) = sqlx::query_as(
            "SELECT attempts, enqueued_at::text FROM pending_s3_deletes \
              WHERE blake3_hash = $1",
        )
        .bind(hash_y.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(attempts_before, 3);

        // Duplicate enqueue through the production path: the designed
        // dedup MUST hold while attempts remain.
        enqueue_via_production(&db.pool, &backend, &[hash_y]).await;

        let (attempts_after, enqueued_after): (i32, String) = sqlx::query_as(
            "SELECT attempts, enqueued_at::text FROM pending_s3_deletes \
              WHERE blake3_hash = $1",
        )
        .bind(hash_y.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            (attempts_after, enqueued_after.as_str()),
            (attempts_before, enqueued_before.as_str()),
            "a duplicate enqueue against a non-exhausted row is swallowed: \
             attempts AND enqueued_at unchanged (red under an unguarded DO UPDATE)"
        );
        // One row, still — the dedup index held.
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes WHERE blake3_hash = $1")
                .bind(hash_y.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(rows, 1);
    }

    // r[verify store.gc.clearance-expiry+2]
    /// W11-AM face 2 (the expiry face — the time axis's own cell): a
    /// clearance aged past the drain bound refuses its next
    /// batch-authorize WITH NO hold transition — `gc_holds` is empty,
    /// so a re-consult-only implementation would authorize; expiry is
    /// the one mechanism that can refuse here. Driven at the
    /// clearance type's own authorize seam through the production
    /// mint (`hold::gate`); the bound is parameterized so the aging
    /// is real elapsed monotonic time (250ms > 25ms), never a mocked
    /// clock — `authorize_batch` pins the production bound to
    /// `DESTRUCTIVE_BATCH_DRAIN_BOUND` by delegation. Expiry is
    /// terminal at its bound: the second authorize refuses too (this
    /// clearance is dead; the next tick re-gates).
    ///
    /// Strawman red (disclosed — the authorize seam is new, so no
    /// pre-fix tree compiles this test): under the re-consult-only
    /// variant (expiry check removed) the aged authorize returned
    /// Authorized and the assert went red. Verbatim in the commit
    /// body.
    #[tokio::test]
    async fn aged_clearance_refuses_with_no_hold_present() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let mut clearance = match hold::gate(&db.pool).await.unwrap() {
            hold::HoldGate::Clear(c) => c,
            hold::HoldGate::Held(h) => panic!("fresh db carries a hold: {h:?}"),
        };

        // Control: a fresh clearance authorizes (and refreshes its
        // window) at the production bound.
        assert!(matches!(
            clearance.authorize_batch(&db.pool).await.unwrap(),
            hold::BatchAuthorize::Authorized(_)
        ));

        // Age it past a real (test-scaled) bound. No hold lands.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let holds: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM gc_holds")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(holds.0, 0, "precondition: no hold transition anywhere");

        let bound = std::time::Duration::from_millis(25);
        assert!(
            matches!(
                clearance
                    .authorize_batch_with_bound(&db.pool, bound)
                    .await
                    .unwrap(),
                hold::BatchAuthorize::Expired
            ),
            "a drain-bound-aged clearance refuses with gc_holds empty \
             (re-consult cannot be what refused)"
        );
        // Terminal at the bound: nothing further is authorized.
        assert!(matches!(
            clearance
                .authorize_batch_with_bound(&db.pool, bound)
                .await
                .unwrap(),
            hold::BatchAuthorize::Expired
        ));
    }

    /// Build a serialized manifest referencing the given chunk hashes.
    fn make_manifest(hashes: &[[u8; 32]]) -> Vec<u8> {
        Manifest {
            entries: hashes
                .iter()
                .map(|h| ManifestEntry {
                    hash: *h,
                    size: 100,
                })
                .collect(),
        }
        .serialize()
    }

    /// Every corrupt class `Manifest::deserialize` rejects surfaces as
    /// `Err` from the fallible parse — never as an empty `Ok`.
    #[test]
    fn try_parse_rejects_corrupt_chunk_list() {
        use crate::manifest::{MAX_CHUNKS, ManifestError};

        // Empty input (no version byte).
        assert!(matches!(
            try_parse_unique_chunk_hashes(b""),
            Err(ManifestError::Empty)
        ));

        // Unknown version byte.
        assert!(matches!(
            try_parse_unique_chunk_hashes(&[0xFF]),
            Err(ManifestError::UnknownVersion(0xFF))
        ));

        // Body length not a multiple of the entry stride (truncated).
        let mut truncated = make_manifest(&[[0x11u8; 32]]);
        truncated.pop();
        assert!(matches!(
            try_parse_unique_chunk_hashes(&truncated),
            Err(ManifestError::BadLength(_))
        ));

        // Entry count above MAX_CHUNKS.
        let mut oversized = vec![0u8; 1 + (MAX_CHUNKS + 1) * 36];
        oversized[0] = 1;
        assert!(matches!(
            try_parse_unique_chunk_hashes(&oversized),
            Err(ManifestError::TooManyChunks(_))
        ));
    }

    /// Duplicate hashes collapse to one occurrence each and the result
    /// is sorted ascending (the deterministic order the callers and the
    /// future mark batches rely on), regardless of entry order.
    #[test]
    fn try_parse_dedups_and_sorts_hashes() {
        let a = [0x01u8; 32];
        let b = [0x02u8; 32];
        let c = [0x03u8; 32];
        let manifest = make_manifest(&[c, a, c, b, a, c]);

        let hashes = try_parse_unique_chunk_hashes(&manifest).unwrap();
        assert_eq!(hashes, vec![a, b, c], "deduped, ascending");
    }

    /// An empty manifest (zero entries) is well-formed, not corrupt:
    /// it parses to Ok of an empty set.
    #[test]
    fn try_parse_empty_manifest_is_ok_and_empty() {
        let empty = make_manifest(&[]);
        assert!(try_parse_unique_chunk_hashes(&empty).unwrap().is_empty());
    }

    /// enqueue_chunk_deletes: a hash that isn't 32 bytes is skipped
    /// with a warn, not a panic. The well-formed siblings in the same
    /// batch still enqueue. (Can't-happen in practice — chunks PK writers
    /// all insert 32 bytes — but warn+skip beats killing the collect
    /// batch.)
    #[tokio::test]
    // r[verify store.gc.pending-deletes+2]
    async fn enqueue_skips_corrupt_hash() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let mut tx = db.pool.begin().await.unwrap();

        // One well-formed (32 bytes), one corrupt (7 bytes).
        let good = vec![0xAAu8; 32];
        let bad = vec![0xBBu8; 7];
        let zeroed = vec![(good.clone(), 100i64), (bad, 50i64)];

        let enqueued = enqueue_chunk_deletes(
            &mut tx,
            &zeroed,
            Some(&backend),
            crate::test_helpers::gc_batch_authority(&db.pool).await,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Only the well-formed one enqueued.
        assert_eq!(enqueued, 1);
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT blake3_hash FROM pending_s3_deletes")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, good);
    }

    /// bug_304: final `GcProgress.paths_scanned` MUST equal the
    /// mid-progress value (`found_unreachable`), never regress to
    /// `stats.paths_deleted`. Resurrection (which makes the two
    /// diverge) requires a write landing between mark's snapshot and
    /// sweep's re-check — not deterministically reachable through
    /// `run_gc` in a unit test — so this pins the contract on a
    /// non-resurrecting run and asserts the summary string carries
    /// the resurrected count (the second half of the fix).
    /// merged_bug_148: phase-3 failure arms must reach the operator.
    /// Pre-fix a ParseFailure cycle fell through to the unconditional
    /// final send: "complete: ... 0 chunks, 0 S3 keys enqueued, 0
    /// bytes freed" -- indistinguishable from "no collectible chunks"
    /// -- while ALL chunk collection was suspended; the operator best
    /// positioned to repair the manifest walked away.
    #[tokio::test]
    async fn run_gc_parse_failure_reports_suspension_to_operator() {
        use crate::test_helpers::ChunkSeed;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let _c = ChunkSeed::new(0xD0).uploaded().seed(&db.pool).await;
        crate::test_helpers::StoreSeed::path("suspend-src")
            .with_manifest_status("complete")
            .seed(&db.pool)
            .await;
        let h: Vec<u8> = sqlx::query_scalar("SELECT store_path_hash FROM manifests LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2) \
             ON CONFLICT (store_path_hash) DO UPDATE SET chunk_list = EXCLUDED.chunk_list",
        )
        .bind(&h)
        .bind(vec![0xFFu8; 7]) // corrupt: wrong version byte + misaligned
        .execute(&db.pool)
        .await
        .unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        run_gc(
            &db.pool,
            None,
            GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap()
        .unwrap();

        let mut last = None;
        while let Some(m) = rx.recv().await {
            last = Some(m.unwrap());
        }
        let last = last.expect("final frame");
        assert!(last.is_complete, "the stream still ends (sentinel kept)");
        assert!(
            last.current_path
                .starts_with(rio_common::classify::GC_CHUNK_COLLECT_SUSPENDED_PREFIX),
            "the operator surface reports the suspension (shared colon-included \
             prefix — merged_bug_052), got: {}",
            last.current_path
        );
    }

    /// merged_bug_148 Err half: a mid-drain DB failure after committed
    /// batches must not summarize as "0 chunks" -- destructive work
    /// already committed.
    #[tokio::test]
    async fn run_gc_collect_db_failure_reports_failed_to_operator() {
        use crate::test_helpers::ChunkSeed;

        let db = TestDb::new(&crate::MIGRATOR).await;
        for i in 0..3u8 {
            let h = ChunkSeed::new(0xD8 + i).uploaded().seed(&db.pool).await;
            sqlx::query(
                "UPDATE chunks SET created_at = now() - interval '90 days', \
                 last_referenced_at = now() - interval '90 days' WHERE blake3_hash = $1",
            )
            .bind(&h[..])
            .execute(&db.pool)
            .await
            .unwrap();
        }

        collect::COLLECT_FAIL_AFTER_BATCHES.store(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel(64);
        run_gc(
            &db.pool,
            None,
            GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap()
        .unwrap();

        let mut last = None;
        while let Some(m) = rx.recv().await {
            last = Some(m.unwrap());
        }
        let last = last.expect("final frame");
        assert!(last.is_complete);
        assert!(
            last.current_path
                .starts_with(rio_common::classify::GC_CHUNK_COLLECT_FAILED_PREFIX),
            "a mid-drain failure is disclosed (shared colon-included prefix — \
             merged_bug_052), got: {}",
            last.current_path
        );
    }

    /// merged_bug_052 machine witness (banner (a)): the store's render
    /// alphabet and the CLI's exit posture agree, asserted THROUGH the
    /// shared predicate — `gc_render_is_chunk_collect_failure` is
    /// byte-for-byte what the CLI executes. Every [`Phase3Render`]
    /// variant is constructed via a no-wildcard table (a new variant
    /// fails this match at compile time), rendered through the REAL
    /// builder, and its posture must equal its crate-neutral mirror's
    /// `failure_prefix().is_some()`. The retired shape — store asserts
    /// on a colon-free prefix, CLI asserts on its own re-typed
    /// literals — let a store-side reword exit 0 on a failed
    /// destructive collect while both suites stayed green.
    #[test]
    fn phase3_render_posture_total() {
        use rio_common::classify::{GcPhase3Outcome, gc_render_is_chunk_collect_failure};

        // No-wildcard construction table: every variant named once.
        let variants = [
            Phase3Render::Committed,
            Phase3Render::CommitLost,
            Phase3Render::CommitIndeterminate,
            Phase3Render::PreviewOnly,
            Phase3Render::Suspended,
            Phase3Render::Failed("db timeout".into()),
        ];
        // The table is total: a new variant breaks this match.
        for v in &variants {
            match v {
                Phase3Render::Committed
                | Phase3Render::CommitLost
                | Phase3Render::CommitIndeterminate
                | Phase3Render::PreviewOnly
                | Phase3Render::Suspended
                | Phase3Render::Failed(_) => {}
            }
        }

        let stats = GcStats {
            paths_deleted: 3,
            chunks_deleted: 2,
            s3_keys_enqueued: 2,
            bytes_freed: 9,
            paths_resurrected: 0,
        };
        for v in &variants {
            for dry_run in [false, true] {
                let render = render_phase3(v, dry_run, &stats);
                let outcome = GcPhase3Outcome::from(v);
                assert_eq!(
                    gc_render_is_chunk_collect_failure(&render),
                    outcome.failure_prefix().is_some(),
                    "render/exit-posture divergence for {v:?} (dry_run={dry_run}): {render:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn run_gc_final_paths_scanned_monotone() {
        use crate::test_helpers::StoreSeed;

        let db = TestDb::new(&crate::MIGRATOR).await;
        StoreSeed::path("monotone-a")
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;
        StoreSeed::path("monotone-b")
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;

        let (tx, mut rx) = mpsc::channel(8);
        let stats = run_gc(
            &db.pool,
            None,
            GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap()
        .unwrap();

        let mut msgs = Vec::new();
        while let Some(m) = rx.recv().await {
            msgs.push(m.unwrap());
        }
        assert!(msgs.len() >= 2, "mid + final");
        let mid = &msgs[0];
        let fin = msgs.last().unwrap();
        assert!(fin.is_complete);
        assert_eq!(mid.paths_scanned, 2, "mark found both");
        assert_eq!(
            fin.paths_scanned, mid.paths_scanned,
            "final paths_scanned echoes found_unreachable, not paths_deleted"
        );
        assert!(fin.paths_scanned >= fin.paths_collected);
        assert!(
            fin.current_path.contains("0 resurrected"),
            "resurrections surfaced in summary: {}",
            fin.current_path
        );
        assert_eq!(stats.paths_deleted, 2);
    }

    /// I-192 liveness: `run_gc` (full mark+sweep orchestration)
    /// concurrent with a burst of `insert_manifest_uploading` calls.
    /// Every insert MUST succeed — GC never blocks PutPath. This IS
    /// the I-168/I-192 user-facing symptom: before this change, the
    /// inserts would block on `GC_MARK_LOCK_ID` shared and return
    /// `GcMarkBusy` → gRPC `Aborted` after the retry budget.
    ///
    /// `multi_thread`: GC and the insert burst must actually
    /// interleave on separate executor threads.
    ///
    /// Safety (re-check correctness) is proven deterministically by
    /// [`gc_mark_then_insert_then_sweep_preserves_referenced`] below
    /// and `sweep::tests::sweep_recheck_sees_uploading_placeholder`;
    /// a free-running race here can't distinguish "P_i committed
    /// before re-check" from "P_i committed after Q_i's DELETE"
    /// (the latter is a legitimate post-GC dangling ref, not a bug).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_gc_concurrent_with_placeholder_inserts_liveness() {
        use crate::test_helpers::{StoreSeed, path_hash};
        use rio_test_support::fixtures::test_store_path;

        const N: usize = 100;
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed N old, unrooted targets Q_i (48h, past grace=2h) so GC
        // has real mark+sweep work to do while inserts run.
        let mut targets = Vec::with_capacity(N);
        for i in 0..N {
            let q = test_store_path(&format!("i192-live-target-{i:03}"));
            StoreSeed::raw_path(&q)
                .created_hours_ago(48)
                .seed(&db.pool)
                .await;
            targets.push(q);
        }

        // GC task: full run_gc (GC_LOCK_ID + mark + sweep).
        let pool_gc = db.pool.clone();
        let (tx, mut rx) = mpsc::channel(64);
        let gc = tokio::spawn(async move {
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            let stats = run_gc(
                &pool_gc,
                None,
                GcParams {
                    dry_run: false,
                    grace_hours: 2,
                    extra_roots: vec![],
                },
                tx,
                &rio_common::signal::Token::new(),
            )
            .await
            .expect("run_gc");
            drain.await.ok();
            stats
        });

        // Insert burst: N placeholders P_i with refs=[Q_i], concurrent
        // with GC. Each insert MUST succeed — no lock to contend on.
        let mut insert_tasks = Vec::with_capacity(N);
        for (i, q) in targets.iter().cloned().enumerate() {
            let pool = db.pool.clone();
            insert_tasks.push(tokio::spawn(async move {
                let p = test_store_path(&format!("i192-live-uploader-{i:03}"));
                crate::metadata::insert_manifest_uploading(&pool, &path_hash(&p), &p, &[q])
                    .await
                    .expect("insert_manifest_uploading must not fail under concurrent GC")
            }));
        }
        for t in insert_tasks {
            assert!(
                t.await.unwrap().is_some(),
                "fresh path → placeholder inserted"
            );
        }

        let stats = gc.await.unwrap().expect("GC_LOCK_ID free → Some(stats)");
        // Accounting sanity: sweep saw at most N candidates.
        assert!(
            stats.paths_deleted + stats.paths_resurrected <= N as u64,
            "stats out of bounds: {stats:?}"
        );
    }

    /// I-192 safety, deterministic: glue `compute_unreachable` →
    /// concurrent placeholder inserts → `sweep` so the inserts land
    /// PRECISELY in the mark-snapshot/sweep-recheck window the removed
    /// lock used to close. Asserts every target survives via the
    /// re-check alone. This is the end-to-end form of
    /// `mark::tests::placeholder_refs_protect_closure` (mark side) +
    /// `sweep::tests::sweep_recheck_sees_uploading_placeholder` (sweep
    /// side) at N=100 with real concurrency on the insert burst.
    // r[verify store.gc.sweep-recheck+2]
    // r[verify store.put.placeholder-refs]
    #[tokio::test(flavor = "multi_thread")]
    async fn gc_mark_then_insert_then_sweep_preserves_referenced() {
        use crate::test_helpers::{StoreSeed, path_hash};
        use rio_test_support::fixtures::test_store_path;

        const N: usize = 100;
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed N old, unrooted targets Q_i.
        let mut targets = Vec::with_capacity(N);
        for i in 0..N {
            let q = test_store_path(&format!("i192-safe-target-{i:03}"));
            let h = StoreSeed::raw_path(&q)
                .created_hours_ago(48)
                .seed(&db.pool)
                .await;
            targets.push((q, h));
        }

        // T0: mark snapshot. All Q_i unreachable (no P_i exists yet).
        let unreachable = mark::compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert_eq!(unreachable.len(), N, "all targets unreachable pre-insert");

        // T1: 100 concurrent placeholder inserts P_i refs=[Q_i]. All
        // commit AFTER mark's snapshot, BEFORE sweep — the exact window.
        let mut insert_tasks = Vec::with_capacity(N);
        for (i, (q, _)) in targets.iter().cloned().enumerate() {
            let pool = db.pool.clone();
            insert_tasks.push(tokio::spawn(async move {
                let p = test_store_path(&format!("i192-safe-uploader-{i:03}"));
                crate::metadata::insert_manifest_uploading(&pool, &path_hash(&p), &p, &[q])
                    .await
                    .expect("insert must succeed (no GC lock to contend on)")
            }));
        }
        for t in insert_tasks {
            assert!(t.await.unwrap().is_some());
        }

        // T2: sweep with mark's stale unreachable list. Re-check must
        // resurrect EVERY Q_i.
        let stats = sweep::sweep(
            &db.pool,
            None,
            unreachable,
            false,
            &rio_common::signal::Token::new(),
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap()
        .stats;
        assert_eq!(stats.paths_deleted, 0, "no referenced path may be swept");
        assert_eq!(
            stats.paths_resurrected, N as u64,
            "every target resurrected by re-check"
        );

        // No dangling references anywhere.
        let dangling: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM narinfo n
             CROSS JOIN LATERAL unnest(n."references") r
             WHERE NOT EXISTS (SELECT 1 FROM narinfo n2 WHERE n2.store_path = r)
            "#,
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(dangling, 0, "no placeholder may reference a swept path");

        // Every Q_i's narinfo still exists.
        for (_, h) in &targets {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM narinfo WHERE store_path_hash = $1)",
            )
            .bind(h)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(exists, "Q's narinfo must survive sweep");
        }
    }
    /// A string-keyed backend whose key layout can migrate (v1 -> v2)
    /// and whose deletes can fail (the exhaustion injection): the
    /// W12-R fixture. Deletes of missing keys succeed (S3 semantics —
    /// the idempotency the stale-key leak rides).
    #[derive(Default)]
    struct LayoutBackend {
        store: std::sync::Mutex<std::collections::HashMap<String, bytes::Bytes>>,
        v2: std::sync::atomic::AtomicBool,
        healthy: std::sync::atomic::AtomicBool,
    }

    impl LayoutBackend {
        fn key_v1(hash: &[u8; 32]) -> String {
            format!("v1/{}", hex::encode(hash))
        }
        fn key_v2(hash: &[u8; 32]) -> String {
            format!("v2/{}", hex::encode(hash))
        }
        /// The operator key-layout migration: the object moves to the
        /// v2 key and key_for renders v2 from now on.
        fn migrate(&self, hash: &[u8; 32]) {
            let mut store = self.store.lock().unwrap();
            if let Some(data) = store.remove(&Self::key_v1(hash)) {
                store.insert(Self::key_v2(hash), data);
            }
            self.v2.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        fn contains(&self, key: &str) -> bool {
            self.store.lock().unwrap().contains_key(key)
        }
    }

    #[async_trait::async_trait]
    impl crate::backend::ChunkBackend for LayoutBackend {
        async fn put(&self, h: &[u8; 32], d: bytes::Bytes) -> anyhow::Result<()> {
            self.store.lock().unwrap().insert(self.key_for(h), d);
            Ok(())
        }
        async fn get(&self, h: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
            Ok(self.store.lock().unwrap().get(&self.key_for(h)).cloned())
        }
        async fn exists_batch(&self, hs: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            let store = self.store.lock().unwrap();
            Ok(hs
                .iter()
                .map(|h| store.contains_key(&self.key_for(h)))
                .collect())
        }
        fn key_for(&self, h: &[u8; 32]) -> String {
            if self.v2.load(std::sync::atomic::Ordering::SeqCst) {
                Self::key_v2(h)
            } else {
                Self::key_v1(h)
            }
        }
        async fn delete_by_key(&self, k: &str) -> anyhow::Result<()> {
            if !self.healthy.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("injected S3 outage: transient delete failure")
            }
            // S3 semantics: delete-of-missing succeeds silently.
            self.store.lock().unwrap().remove(k);
            Ok(())
        }
    }

    // r[verify store.gc.outbox-reset+2]
    /// W12-R (merged_bug_117, R30-hardened): the reset edge carries
    /// the FRESH DECISION WHOLE — every decision-derived column, not
    /// just the budget fields. Pre-fix the DO UPDATE arm reset
    /// attempts/enqueued_at but kept the STALE s3_key the original
    /// enqueue computed; after a backend key-layout migration the
    /// drain then deletes at the stale key (idempotent success on a
    /// missing object), the row leaves the outbox, the tombstone
    /// reap's NOT-EXISTS conjunct unblocks — and the object at the
    /// NEW key leaks silently, forever. The wave-11 exit edge
    /// converted a parked-but-VISIBLE posture (_stuck gauge) into a
    /// silent permanent leak.
    #[tokio::test]
    async fn outbox_reset_carries_the_recomputed_key() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let layout = std::sync::Arc::new(LayoutBackend::default());
        let backend: Arc<dyn crate::backend::ChunkBackend> =
            std::sync::Arc::clone(&layout) as Arc<dyn crate::backend::ChunkBackend>;

        // Chunk X: tombstoned, object at the v1 key, outbox row
        // enqueued via PRODUCTION while the v1 layout is live.
        let hash_x = [0xC4u8; 32];
        backend
            .put(&hash_x, bytes::Bytes::from_static(b"layout-migration-prey"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, size, deleted, deleted_at) \
             VALUES ($1, 21, true, now() - interval '2 days')",
        )
        .bind(hash_x.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            enqueue_via_production(&db.pool, &backend, &[hash_x]).await,
            1
        );

        // The outage exhausts the row through the production drain.
        for _ in 0..crate::gc::drain::MAX_ATTEMPTS {
            let (deleted, failed) = crate::gc::drain::drain_once(
                &db.pool,
                &backend,
                &mut crate::test_helpers::gc_clearance(&db.pool).await,
            )
            .await
            .unwrap();
            assert_eq!((deleted, failed), (0, 1));
        }

        // The key-layout migration lands between enqueue and reset:
        // the object now lives at v2; key_for renders v2.
        layout.migrate(&hash_x);
        layout
            .healthy
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // The fresh collect decision resets the exhausted row — and
        // MUST carry the recomputed key with it.
        enqueue_via_production(&db.pool, &backend, &[hash_x]).await;
        let row_key: String =
            sqlx::query_scalar("SELECT s3_key FROM pending_s3_deletes WHERE blake3_hash = $1")
                .bind(hash_x.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            row_key,
            LayoutBackend::key_v2(&hash_x),
            "left: the reset arm kept the stale pre-migration key (the \
             drain will delete a ghost and the real object leaks) / \
             right: the exit edge carries every decision-derived column"
        );

        // The drain executes the carried decision: the REAL object
        // dies; the row leaves; the reap conjunct unblocks with no
        // leak behind it.
        let (deleted, failed) = crate::gc::drain::drain_once(
            &db.pool,
            &backend,
            &mut crate::test_helpers::gc_clearance(&db.pool).await,
        )
        .await
        .unwrap();
        assert_eq!((deleted, failed), (1, 0));
        assert!(
            !layout.contains(&LayoutBackend::key_v2(&hash_x)),
            "the object at the recomputed key is deleted — no silent leak"
        );
        let outbox_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_s3_deletes WHERE blake3_hash = $1")
                .bind(hash_x.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            outbox_rows, 0,
            "the outbox row leaves after the real delete"
        );
    }

    // r[verify store.gc.outbox-reset+2]
    /// W12-R2 (merged_bug_117, the accounting face): the enqueue
    /// count is `rows_affected()` — inserted or reset rows only —
    /// so `rio_store_gc_s3_key_enqueued_total` measures what its
    /// HELP claims. Pre-fix the fn returned `keys.len()`, counting
    /// DO-UPDATE-WHERE-false no-ops (in-budget dups) as enqueues.
    #[tokio::test]
    async fn enqueue_count_is_rows_affected_not_keys_attempted() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let outage = std::sync::Arc::new(OutageBackend {
            inner: mem_backend(),
            healthy: std::sync::atomic::AtomicBool::new(false),
        });
        let backend: Arc<dyn crate::backend::ChunkBackend> =
            std::sync::Arc::clone(&outage) as Arc<dyn crate::backend::ChunkBackend>;

        // B: an IN-BUDGET in-flight row (attempts = 3 < MAX).
        let hash_b = [0xC5u8; 32];
        backend
            .put(&hash_b, bytes::Bytes::from_static(b"in-budget-dup"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, size, deleted, deleted_at) \
             VALUES ($1, 13, true, now() - interval '2 days')",
        )
        .bind(hash_b.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            enqueue_via_production(&db.pool, &backend, &[hash_b]).await,
            1,
            "the fresh insert counts"
        );
        for _ in 0..3 {
            let (_, failed) = crate::gc::drain::drain_once(
                &db.pool,
                &backend,
                &mut crate::test_helpers::gc_clearance(&db.pool).await,
            )
            .await
            .unwrap();
            assert_eq!(failed, 1);
        }

        // A: a fresh chunk. The batch [A, B]: A inserts, B is the
        // designed swallow (in-budget dup, DO UPDATE WHERE false).
        let hash_a = [0xC3u8; 32];
        backend
            .put(&hash_a, bytes::Bytes::from_static(b"fresh-insert"))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, size, deleted, deleted_at) \
             VALUES ($1, 12, true, now() - interval '2 days')",
        )
        .bind(hash_a.as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
        let counted = enqueue_via_production(&db.pool, &backend, &[hash_a, hash_b]).await;
        assert_eq!(
            counted, 1,
            "left: keys-attempted counted the swallowed dup as an enqueue \
             (the HELP lies) / right: rows_affected — the insert counts, \
             the no-op does not"
        );
    }
}

// =======================================================================
// Round-9 WO-S1-4 legs (2)+(3) — evidence-outlives-bytes + the GC-hold
// control (signed Q3). Witnesses W9-I / W9-J.
// =======================================================================
#[cfg(test)]
mod registration_evidence_tests {
    use super::*;
    use crate::test_helpers::{StoreSeed, TenantSeed};
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::test_store_path;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    async fn run_full_gc(pool: &sqlx::PgPool) -> Option<GcStats> {
        let (tx, mut rx) = mpsc::channel(64);
        let stats = run_gc(
            pool,
            None,
            GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap();
        while rx.recv().await.is_some() {}
        stats
    }

    /// W9-I — sweep deletes BYTES; the registration/audit records
    /// survive as tombstoned evidence, copied atomically inside the
    /// sweep batch tx. An expired-window registered path sweeps (the
    /// retention policy is lawful) but its (tenant, first_referenced,
    /// deriver) registration record and its realisation identity rows
    /// persist. The anomaly classes are pinned as non-worsened:
    /// scheduler_live_pins rows are untouched by the path sweep, and
    /// no LIVE orphan path_tenants rows are minted (the dying rows
    /// move to the tombstone table, not to limbo).
    // r[verify store.gc.evidence-outlives-bytes]
    // r[verify store.gc.sweep-path-tenants+1]
    #[tokio::test]
    async fn sweep_tombstones_registration_evidence() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // A registered path whose tenant window EXPIRED: 100h old,
        // referenced 72h ago, retention 48h → lawful sweep candidate.
        let path = test_store_path("evidence-outlives");
        let drv_path = test_store_path("evidence-outlives.drv");
        let hash = StoreSeed::raw_path(&path)
            .created_hours_ago(100)
            .seed(&db.pool)
            .await;
        sqlx::query("UPDATE narinfo SET deriver = $2 WHERE store_path_hash = $1")
            .bind(&hash)
            .bind(&drv_path)
            .execute(&db.pool)
            .await
            .unwrap();
        let tenant = TenantSeed::new("evidence-tenant")
            .with_retention_hours(48)
            .seed(&db.pool)
            .await;
        sqlx::query(
            "INSERT INTO path_tenants (store_path_hash, tenant_id, first_referenced_at) \
             VALUES ($1, $2, now() - interval '72 hours')",
        )
        .bind(&hash)
        .bind(tenant)
        .execute(&db.pool)
        .await
        .unwrap();
        // An identity row for the same output.
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash) \
             VALUES ($1, 'out', $2, $3)",
        )
        .bind(vec![7u8; 32])
        .bind(&path)
        .bind(vec![9u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();
        // Anomaly-class baseline (672-pin gap / orphan stamps must not
        // worsen): one unrelated live pin.
        sqlx::query(
            "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) VALUES ($1, 'other-drv')",
        )
        .bind(vec![1u8; 32])
        .bind("x")
        .execute(&db.pool)
        .await
        .ok();
        let pins_before: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduler_live_pins")
            .fetch_one(&db.pool)
            .await
            .unwrap();

        let stats = run_full_gc(&db.pool).await.expect("gc ran");
        assert!(stats.paths_deleted >= 1, "the expired path swept");

        // Bytes gone…
        let narinfo_left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(narinfo_left, 0, "the path's bytes/metadata swept");

        // …records survive.
        let tomb: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
            "SELECT tenant_id, store_path, deriver FROM path_tenant_tombstones \
             WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .fetch_optional(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            tomb,
            Some((tenant, path.clone(), Some(drv_path))),
            "left: the sweep deleted the registration records WITH the \
             bytes (no surviving evidence anywhere) / right: the \
             registration record outlives the bytes as a tombstone"
        );
        let real_tomb: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM realisation_tombstones WHERE output_path = $1",
        )
        .bind(&path)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(real_tomb, 1, "the identity row outlives the bytes");

        // Non-worsened anomaly classes.
        let pins_after: i64 = sqlx::query_scalar("SELECT count(*) FROM scheduler_live_pins")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pins_after, pins_before, "the sweep never touches pins");
        let live_orphans: i64 =
            sqlx::query_scalar("SELECT count(*) FROM path_tenants WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(live_orphans, 0, "no LIVE orphan stamps minted by the sweep");
    }

    /// W9-J global face — hold set ⇒ the whole collection pass is a
    /// no-op (sweep included); hold released ⇒ the NEXT pass proceeds.
    /// Both directions through the production control API (the heal
    /// edge witnessed per T2).
    // r[verify store.gc.hold+2]
    #[tokio::test]
    async fn global_hold_suspends_and_release_heals() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("held-global");
        let hash = StoreSeed::raw_path(&path)
            .created_hours_ago(48)
            .seed(&db.pool)
            .await;

        let hold_id = hold::set_hold(
            &db.pool,
            hold::GcHoldScope::Global,
            "incident-freeze",
            "test-operator",
            None,
        )
        .await
        .unwrap();

        let _ = run_full_gc(&db.pool).await;
        let still_there: i64 =
            sqlx::query_scalar("SELECT count(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            still_there, 1,
            "left: the sweep ran through an ACTIVE GLOBAL HOLD (the \
             operator control does not bind) / right: hold set ⇒ \
             collection is a no-op on the held scope"
        );

        assert!(hold::release_hold(&db.pool, hold_id).await.unwrap());
        let stats = run_full_gc(&db.pool).await.expect("gc ran post-release");
        assert!(
            stats.paths_deleted >= 1,
            "hold released ⇒ the next sweep proceeds (the heal edge)"
        );
    }

    /// W9-J tenant face — a tenant-scoped hold pins the held tenant's
    /// registered paths past their retention window; release heals.
    // r[verify store.gc.hold+2]
    #[tokio::test]
    async fn tenant_hold_pins_registered_paths() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("held-tenant");
        let hash = StoreSeed::raw_path(&path)
            .created_hours_ago(100)
            .seed(&db.pool)
            .await;
        let tenant = TenantSeed::new("held-tenant")
            .with_retention_hours(48)
            .seed(&db.pool)
            .await;
        // Window EXPIRED (72h > 48h) — sweepable without the hold.
        sqlx::query(
            "INSERT INTO path_tenants (store_path_hash, tenant_id, first_referenced_at) \
             VALUES ($1, $2, now() - interval '72 hours')",
        )
        .bind(&hash)
        .bind(tenant)
        .execute(&db.pool)
        .await
        .unwrap();

        let hold_id = hold::set_hold(
            &db.pool,
            hold::GcHoldScope::Tenant(tenant),
            "tenant-investigation",
            "test-operator",
            None,
        )
        .await
        .unwrap();

        let _ = run_full_gc(&db.pool).await;
        let still_there: i64 =
            sqlx::query_scalar("SELECT count(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            still_there, 1,
            "left: the held tenant's registered path swept anyway / \
             right: a tenant hold extends reachability past the window"
        );

        assert!(hold::release_hold(&db.pool, hold_id).await.unwrap());
        let _ = run_full_gc(&db.pool).await;
        let gone: i64 =
            sqlx::query_scalar("SELECT count(*) FROM narinfo WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gone, 0, "release ⇒ the next sweep proceeds (heal edge)");
    }

    // r[verify store.gc.batch-authority]
    /// W12-O (bug_084): a global hold landing MID-PASS — between two
    /// committed path-delete batches of run_gc's phase-2 sweep —
    /// stops the sweep at the NEXT batch boundary and suspends the
    /// rest of the run (phase 3 never starts). The operator's
    /// emergency stop binds within one batch, not one run: pre-fix,
    /// the entry consult was the sweep's ONLY hold consult, so the
    /// remaining batches ("thousands × ~100ms" at scale) kept
    /// deleting paths unrecoverably through the freeze.
    ///
    /// Schedule: six unreachable paths = three test batches
    /// (SWEEP_BATCH_SIZE = 2 under cfg(test)); the interpose lands a
    /// GLOBAL hold through the production `set_hold` statement
    /// immediately after batch 1 commits. Post-fix: exactly the
    /// pre-hold batch's two paths are deleted; the other four
    /// survive the freeze; release + rerun drains the remainder (the
    /// heal edge, resumability witnessed).
    #[tokio::test]
    async fn mid_pass_hold_stops_path_sweep_at_the_batch_boundary() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        for i in 0..6u8 {
            crate::test_helpers::StoreSeed::path(&format!("midpass-{i}"))
                .created_hours_ago(48)
                .seed(&db.pool)
                .await;
        }

        // The hold lands through the production set_hold statement
        // immediately after sweep batch 1 commits (the test interpose
        // — no external caller can time the inter-batch gap).
        sweep::SWEEP_HOLD_AFTER_BATCHES.store(1, std::sync::atomic::Ordering::SeqCst);

        let stats = run_full_gc(&db.pool)
            .await
            .expect("a mid-pass-held run reports its committed progress");
        assert_eq!(
            stats.paths_deleted, 2,
            "left: post-hold batches kept deleting through the freeze \
             (the emergency stop defeated) / right: exactly the \
             pre-hold batch is swept; batch 2 refuses at its boundary"
        );
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM narinfo")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            remaining, 4,
            "the four post-hold paths survive the freeze (the table agrees)"
        );
        // Phase 3 never ran under the refused clearance: no chunk
        // collection happened in this pass.
        assert_eq!(
            stats.chunks_deleted, 0,
            "phase 3 is suspended with the sweep (no collect under the hold)"
        );

        // The heal edge: release the hold; the next run drains the
        // remainder — suspension never converts into lost work.
        let hold_id: uuid::Uuid =
            sqlx::query_scalar("SELECT hold_id FROM gc_holds WHERE created_by = 'sweep-test-hook'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(hold::release_hold(&db.pool, hold_id).await.unwrap());
        let stats = run_full_gc(&db.pool).await.expect("post-release run");
        assert_eq!(stats.paths_deleted, 4, "release ⇒ the remainder sweeps");
    }
}
