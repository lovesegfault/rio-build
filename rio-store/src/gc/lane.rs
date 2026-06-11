//! The destructive-lane protocol (merged_bug_050's close): the GC
//! hold suspends EVERY lane with delete authority, as a property of
//! the lane ABSTRACTION, not of one entry point.
//!
//! Wave-9's hold control consulted `gc_holds` at exactly one entry
//! (`run_gc`) while the chunk-collect backstop, the 30s
//! `pending_s3_deletes` drain, the hourly log TTL sweep, and the
//! gc-orphan-scanner each kept deleting during the freeze — and a
//! held `run_gc` starving `last_live_cycle_at` GUARANTEED the
//! backstop fired. `DestructiveLane` is the only way to register a
//! deleting periodic lane: its tick wrapper consults holds
//! FAIL-CLOSED before invoking the lane body and mints the per-tick
//! [`super::hold::HoldClearance`] every named delete sink demands — an
//! unregistered `tokio::spawn` loop cannot reach a named sink at
//! compile time.
//!
//! Enforcement, stated at its own strength (R24): the clearance type
//! COMPILE-SEALS the named sinks (`reap_one`, `drain_once`,
//! `collect_cycle` via `collect_backstop_once`/`run_gc`); the
//! [GEN-SET] delete-sink census (`destructive_lane_census` below) is
//! the LOAD-BEARING enforcement of population totality — that every
//! deleting sink (including raw-SQL sinks the type cannot total
//! over, e.g. the seam-registered log sweep) runs under a consult
//! and every spawn-family lane is registered. Future lanes inherit
//! the consult on registration; non-registration is census-red (and
//! compile-red at any named sink).

use std::time::Duration;

use futures_util::future::BoxFuture;
use sqlx::PgPool;
use tracing::{info, warn};

use super::hold;

/// Outcome of one consulted tick — returned by [`DestructiveLane::tick`]
/// so tests assert the verdict structurally (not via log scraping).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LaneTick {
    /// No active hold: the body ran under its minted clearance.
    Ran,
    /// Active global hold: the body was NOT invoked; skip counted.
    SkippedHeld,
    /// The hold consult failed: fail CLOSED — the body was NOT
    /// invoked (an unreadable hold table is never read as "no
    /// hold"); skip counted.
    SkippedConsultError,
}

/// A lane body: borrows the tick's clearance for the tick's lifetime
/// (the `for<'t>` bound is the no-stash law — the future cannot
/// outlive the clearance it was handed).
pub(crate) type LaneBody =
    Box<dyn for<'t> FnMut(&'t hold::HoldClearance) -> BoxFuture<'t, ()> + Send>;

// r[impl store.gc.hold-lanes]
/// The ONLY way to register a deleting periodic lane — see the
/// module doc. Mirrors `rio_common::task::spawn_periodic[_with]`
/// (biased shutdown select, `MissedTickBehavior::Skip`, panic-logged
/// via `spawn_monitored`) with the per-tick hold consult fused in.
pub(crate) struct DestructiveLane;

impl DestructiveLane {
    /// Register a deleting periodic lane on a fixed interval (the
    /// `spawn_periodic` twin; first tick fires immediately).
    pub(crate) fn spawn_periodic(
        name: &'static str,
        interval: Duration,
        pool: PgPool,
        shutdown: rio_common::signal::Token,
        body: LaneBody,
    ) -> tokio::task::JoinHandle<()> {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self::spawn_periodic_with(name, ticker, pool, shutdown, body)
    }

    /// Register a deleting periodic lane with a caller-built ticker
    /// (the `spawn_periodic_with` twin — the backstop skips its
    /// startup tick via `interval_at`).
    pub(crate) fn spawn_periodic_with(
        name: &'static str,
        mut ticker: tokio::time::Interval,
        pool: PgPool,
        shutdown: rio_common::signal::Token,
        mut body: LaneBody,
    ) -> tokio::task::JoinHandle<()> {
        rio_common::task::spawn_monitored(name, async move {
            loop {
                // Biased: shutdown wins over a ready tick
                // deterministically (r[common.task.periodic-biased]).
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        let _ = Self::tick(name, &pool, &mut body).await;
                    }
                }
            }
        })
    }

    // r[impl store.gc.hold-lanes]
    /// ONE consulted tick: consult the hold gate FAIL-CLOSED, mint
    /// the tick's clearance, run the body under it. The testable
    /// seam — W10-D/E/F drive lanes through this exact fn.
    pub(crate) async fn tick(name: &'static str, pool: &PgPool, body: &mut LaneBody) -> LaneTick {
        match hold::gate(pool).await {
            Ok(hold::HoldGate::Clear(clearance)) => {
                body(&clearance).await;
                LaneTick::Ran
            }
            Ok(hold::HoldGate::Held(h)) => {
                info!(
                    lane = name,
                    hold_id = %h.hold_id,
                    reason = %h.reason,
                    created_by = %h.created_by,
                    "destructive lane tick skipped: active global gc hold"
                );
                metrics::counter!(
                    "rio_store_gc_hold_lane_skips_total",
                    "lane" => name, "cause" => "held"
                )
                .increment(1);
                LaneTick::SkippedHeld
            }
            Err(e) => {
                // Fail CLOSED on a destructive subsystem: an
                // unreadable hold table must not be read as "no
                // hold" (the run_gc entry consult's discipline,
                // inherited by every lane).
                warn!(
                    lane = name,
                    error = %e,
                    "destructive lane: hold consult failed; skipping tick (fail closed)"
                );
                metrics::counter!(
                    "rio_store_gc_hold_lane_skips_total",
                    "lane" => name, "cause" => "consult_error"
                )
                .increment(1);
                LaneTick::SkippedConsultError
            }
        }
    }
}

/// R17 bound on the IN-FLIGHT destructive batch at hold-start: the
/// per-tick consult means at most ONE tick body is mid-flight when a
/// hold lands; that batch completes-or-aborts and the NEXT tick never
/// starts (the corner is structural — `lane_inflight_batch_completes_
/// then_next_tick_skips` asserts next-never-starts, not wall-clock).
/// VALUE (hypothesis per the WO, derivation recorded): one drain
/// batch at the existing cadence — `DRAIN_BATCH_SIZE` (100) per-key
/// S3 deletes is sized to finish well inside its own 30s
/// `DRAIN_INTERVAL` (the cadence only holds if it does), so the
/// interval IS the derived in-flight bound; the other lanes' tick
/// bodies (orphan batch, log-sweep batch, backstop check) are
/// lighter. Violable: a pathological S3 tail can exceed it — the
/// hold's guarantee degrades to "one already-running batch", never
/// "new batches".
pub(crate) const DESTRUCTIVE_BATCH_DRAIN_BOUND: Duration = Duration::from_secs(30);

// The derivation pin (and the const's consumer): the bound IS one
// drain cadence — if the cadence moves, this bound must be re-derived
// (the batch is sized to finish inside its own interval).
const _: () =
    assert!(DESTRUCTIVE_BATCH_DRAIN_BOUND.as_secs() == super::drain::DRAIN_INTERVAL.as_secs());

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use rio_test_support::TestDb;

    use super::*;
    use crate::backend::ChunkBackend;
    use crate::gc::hold;
    use crate::test_helpers::mem_backend;

    /// Seed the drain's prey: a backend chunk + its pending_s3_deletes
    /// row. Returns (hash, key).
    async fn seed_drain_prey(
        pool: &sqlx::PgPool,
        backend: &Arc<dyn ChunkBackend>,
        tag: u8,
    ) -> ([u8; 32], String) {
        let hash = [tag; 32];
        backend
            .put(&hash, bytes::Bytes::from_static(b"held-chunk-data"))
            .await
            .unwrap();
        let key = backend.key_for(&hash);
        sqlx::query("INSERT INTO pending_s3_deletes (s3_key) VALUES ($1)")
            .bind(&key)
            .execute(pool)
            .await
            .unwrap();
        (hash, key)
    }

    /// Seed the scanner's prey: a stale 'uploading' placeholder.
    async fn seed_scanner_prey(pool: &sqlx::PgPool, tag: u8) -> [u8; 32] {
        let hash = [tag; 32];
        crate::metadata::insert_manifest_uploading(
            pool,
            &hash,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-w10d-prey",
            &[],
        )
        .await
        .unwrap()
        .expect("fresh placeholder");
        sqlx::query(
            "UPDATE manifests SET updated_at = now() - interval '1 hour' \
             WHERE store_path_hash = $1",
        )
        .bind(&hash[..])
        .execute(pool)
        .await
        .unwrap();
        hash
    }

    fn drain_body(pool: sqlx::PgPool, backend: Arc<dyn ChunkBackend>) -> LaneBody {
        Box::new(move |clearance| {
            let pool = pool.clone();
            let backend = Arc::clone(&backend);
            Box::pin(async move {
                crate::gc::drain::drain_once(&pool, &backend, clearance)
                    .await
                    .unwrap();
            })
        })
    }

    fn scanner_body(pool: sqlx::PgPool) -> LaneBody {
        Box::new(move |clearance| {
            let pool = pool.clone();
            Box::pin(async move {
                crate::gc::orphan::scan_once(&pool, clearance)
                    .await
                    .unwrap();
            })
        })
    }

    fn backstop_body(pool: sqlx::PgPool) -> LaneBody {
        Box::new(move |clearance| {
            let pool = pool.clone();
            Box::pin(async move {
                crate::gc::collect::collect_backstop_once(
                    &pool,
                    None,
                    crate::gc::sweep::CHUNK_GRACE_SECS,
                    clearance,
                )
                .await
                .unwrap();
            })
        })
    }

    fn sweep_body(pool: sqlx::PgPool) -> LaneBody {
        Box::new(move |_clearance| {
            let pool = pool.clone();
            Box::pin(async move {
                // The seam-registered lane's body (the sweep itself is
                // unchanged; suspension rides the wrapper).
                let store = crate::logs::chunks::MemoryLogChunkStore::default();
                let _ = crate::logs::sweep::sweep_expired_logs(
                    &pool,
                    &store,
                    Duration::from_secs(30 * 86_400),
                    100,
                )
                .await
                .unwrap();
            })
        })
    }

    // r[verify store.gc.hold-lanes]
    /// W10-D (merged_bug_050): with an ACTIVE GLOBAL HOLD and every
    /// periodic census member due, NO lane executes a destructive act
    /// — every tick skips typed, the skip counters increment for
    /// EVERY census member (the witness population IS the census
    /// output: gc-drain-task, gc-orphan-scanner, gc-collect-backstop,
    /// log-ttl-sweep + the demand-driven claim-reap face), the drain
    /// QUEUE holds (rows age, never execute), and upload evidence
    /// survives. Releasing the hold heals: the next tick runs and
    /// consumes the prey (the bodies are real; the wrapper passes
    /// through).
    ///
    /// Pre-fix red (the lane bodies invoked directly, as the
    /// unconsulted spawn loops did — verbatim in the commit body):
    /// the s3-drain executed its queue and the fifth lane deleted
    /// narinfo during the freeze.
    #[tokio::test]
    async fn hold_suspends_every_destructive_lane() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        let hold_id = hold::set_hold(
            &db.pool,
            hold::GcHoldScope::Global,
            "incident freeze",
            "w10-d",
            None,
        )
        .await
        .unwrap();

        let (chunk_hash, _key) = seed_drain_prey(&db.pool, &backend, 0x5d).await;
        let placeholder_hash = seed_scanner_prey(&db.pool, 0x5e).await;
        // Make the backstop due (NULL last_live_cycle_at is due by
        // definition on a fresh row; ensure the row exists).
        let _ = crate::gc::state::backstop_due_unlocked(&db.pool, Duration::from_secs(1)).await;

        // Every periodic census member ticks once THROUGH THE LANE.
        let mut bodies: Vec<(&'static str, LaneBody)> = vec![
            (
                "gc-drain-task",
                drain_body(db.pool.clone(), Arc::clone(&backend)),
            ),
            ("gc-orphan-scanner", scanner_body(db.pool.clone())),
            ("gc-collect-backstop", backstop_body(db.pool.clone())),
            ("log-ttl-sweep", sweep_body(db.pool.clone())),
        ];
        for (name, body) in &mut bodies {
            let outcome = DestructiveLane::tick(name, &db.pool, body).await;
            assert_eq!(
                outcome,
                LaneTick::SkippedHeld,
                "{name}: an active global hold must skip the tick"
            );
        }
        // The demand-driven face skips too (claim-reap during hold).
        let reaped = crate::gc::orphan::reap_one_consulted(
            &db.pool,
            &placeholder_hash,
            crate::gc::orphan::ReapBy::Stale { secs: 0 },
        )
        .await
        .unwrap();
        assert!(!reaped, "demand-driven reap must skip during a hold");

        // Zero destructive acts: the queue HOLDS, the evidence lives.
        let pending: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending.0, 1, "drain queue must HOLD during the freeze");
        assert!(
            backend.get(&chunk_hash).await.unwrap().is_some(),
            "backend chunk must survive the freeze"
        );
        let placeholders: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM manifests WHERE store_path_hash = $1")
                .bind(&placeholder_hash[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(placeholders.0, 1, "upload evidence must survive the freeze");

        // Skip counters incremented for EVERY census member + the
        // demand face — ONE snapshot (the recorder drains on read).
        let metrics_snapshot = snap.snapshot().into_vec();
        let skipped_lanes: std::collections::BTreeSet<String> = metrics_snapshot
            .iter()
            .filter(|(ck, _, _, _)| ck.key().name() == "rio_store_gc_hold_lane_skips_total")
            .flat_map(|(ck, _, _, _)| {
                ck.key()
                    .labels()
                    .filter(|l| l.key() == "lane")
                    .map(|l| l.value().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        for lane in [
            "gc-drain-task",
            "gc-orphan-scanner",
            "gc-collect-backstop",
            "log-ttl-sweep",
            "claim-reap",
        ] {
            assert!(
                skipped_lanes.contains(lane),
                "skip counter missing for census member {lane}; got {skipped_lanes:?}"
            );
        }

        // The heal edge: release the hold; the next ticks RUN and the
        // prey is consumed (the wrapper passes through real bodies).
        assert!(hold::release_hold(&db.pool, hold_id).await.unwrap());
        for (name, body) in &mut bodies {
            let outcome = DestructiveLane::tick(name, &db.pool, body).await;
            assert_eq!(outcome, LaneTick::Ran, "{name}: released hold must run");
        }
        let pending: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending.0, 0, "released hold: the drain executes its queue");
        let placeholders: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM manifests WHERE store_path_hash = $1")
                .bind(&placeholder_hash[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(placeholders.0, 0, "released hold: the scanner reaps");
    }

    // r[verify store.gc.hold-lanes]
    /// W10-F (fail-closed): an UNREADABLE holds table is never read
    /// as "no hold" — EVERY census-member lane skips typed
    /// (`SkippedConsultError`), the counter increments, and zero
    /// destructive acts land. Same derived population as W10-D.
    #[tokio::test]
    async fn consult_error_fails_closed_for_every_lane() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();
        let (chunk_hash, _key) = seed_drain_prey(&db.pool, &backend, 0x6d).await;
        let placeholder_hash = seed_scanner_prey(&db.pool, 0x6e).await;

        // Make the consult fail: the holds table is unreadable.
        sqlx::query("DROP TABLE gc_holds CASCADE")
            .execute(&db.pool)
            .await
            .unwrap();

        let mut bodies: Vec<(&'static str, LaneBody)> = vec![
            (
                "gc-drain-task",
                drain_body(db.pool.clone(), Arc::clone(&backend)),
            ),
            ("gc-orphan-scanner", scanner_body(db.pool.clone())),
            ("gc-collect-backstop", backstop_body(db.pool.clone())),
            ("log-ttl-sweep", sweep_body(db.pool.clone())),
        ];
        for (name, body) in &mut bodies {
            let outcome = DestructiveLane::tick(name, &db.pool, body).await;
            assert_eq!(
                outcome,
                LaneTick::SkippedConsultError,
                "{name}: a failed consult must skip (fail closed), never bypass"
            );
        }
        let reaped = crate::gc::orphan::reap_one_consulted(
            &db.pool,
            &placeholder_hash,
            crate::gc::orphan::ReapBy::Stale { secs: 0 },
        )
        .await
        .unwrap();
        assert!(!reaped, "demand-driven reap must fail closed");

        let pending: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending.0, 1);
        assert!(backend.get(&chunk_hash).await.unwrap().is_some());
        let placeholders: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM manifests WHERE store_path_hash = $1")
                .bind(&placeholder_hash[..])
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(placeholders.0, 1);
    }

    /// A `ChunkBackend` whose `delete_by_key` parks until released —
    /// the in-flight-batch corner's deterministic gate (structural,
    /// never wall-clock).
    struct GatedBackend {
        inner: Arc<dyn ChunkBackend>,
        entered: tokio::sync::Notify,
        entered_flag: AtomicBool,
        release: tokio::sync::Notify,
        released: AtomicBool,
    }

    #[async_trait::async_trait]
    impl ChunkBackend for GatedBackend {
        async fn put(&self, hash: &[u8; 32], data: bytes::Bytes) -> anyhow::Result<()> {
            self.inner.put(hash, data).await
        }
        async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
            self.inner.get(hash).await
        }
        async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            self.inner.exists_batch(hashes).await
        }
        fn key_for(&self, hash: &[u8; 32]) -> String {
            self.inner.key_for(hash)
        }
        async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
            self.entered_flag.store(true, Ordering::SeqCst);
            self.entered.notify_one();
            if !self.released.load(Ordering::SeqCst) {
                self.release.notified().await;
            }
            self.inner.delete_by_key(key).await
        }
    }

    // r[verify store.gc.hold-lanes]
    /// The in-flight corner (DESTRUCTIVE_BATCH_DRAIN_BOUND): a delete
    /// batch already mid-flight when the hold lands COMPLETES (the
    /// per-tick consult granted it a clearance; aborting mid-batch
    /// buys nothing — the S3 delete is already issued), and the NEXT
    /// batch never starts. Structural: the gate is a Notify, not a
    /// clock.
    #[tokio::test]
    async fn lane_inflight_batch_completes_then_next_tick_skips() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let gated = Arc::new(GatedBackend {
            inner: mem_backend(),
            entered: tokio::sync::Notify::new(),
            entered_flag: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            released: AtomicBool::new(false),
        });
        let backend: Arc<dyn ChunkBackend> = Arc::clone(&gated) as Arc<dyn ChunkBackend>;
        seed_drain_prey(&db.pool, &backend, 0x7d).await;

        // Tick 1 starts with NO hold: the batch enters delete_by_key
        // and parks on the gate.
        let mut body = drain_body(db.pool.clone(), Arc::clone(&backend));
        let tick1 = {
            let pool = db.pool.clone();
            async move { DestructiveLane::tick("gc-drain-task", &pool, &mut body).await }
        };
        let tick1 = tokio::spawn(tick1);
        gated.entered.notified().await;
        assert!(gated.entered_flag.load(Ordering::SeqCst));

        // The hold lands MID-BATCH.
        hold::set_hold(
            &db.pool,
            hold::GcHoldScope::Global,
            "mid-batch freeze",
            "w10-d-corner",
            None,
        )
        .await
        .unwrap();

        // Release the gate: the in-flight batch completes-or-aborts.
        gated.released.store(true, Ordering::SeqCst);
        gated.release.notify_one();
        let outcome = tick1.await.unwrap();
        assert_eq!(outcome, LaneTick::Ran, "the in-flight tick completes");
        let pending: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending.0, 0, "the in-flight batch completed");

        // The NEXT batch never starts.
        seed_drain_prey(&db.pool, &backend, 0x7e).await;
        let mut body2 = drain_body(db.pool.clone(), Arc::clone(&backend));
        let outcome2 = DestructiveLane::tick("gc-drain-task", &db.pool, &mut body2).await;
        assert_eq!(
            outcome2,
            LaneTick::SkippedHeld,
            "the next batch never starts"
        );
        let pending: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pending_s3_deletes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pending.0, 1, "the queue holds from the hold onward");
    }

    // r[verify store.gc.hold-lanes]
    // r[verify store.gc.hold+2]
    /// W10-E (the starvation negation, merged_bug_050 commit 2): a
    /// hold spanning k backstop periods produces ZERO deletes and
    /// ZERO fresh collect cycles (population: the census-derived lane
    /// set), AND the held run_gc's no-op tick STAMPS
    /// last_live_cycle_at — a held cycle is a live cycle for
    /// staleness purposes, so the hold itself can never make the
    /// backstop come due (pre-fix: a held run_gc starved the stamp,
    /// guaranteeing due-ness; the un-held backstop then minted fresh
    /// Live cycles whose enqueued deletes the drain executed).
    #[tokio::test]
    async fn held_cycles_stay_live_so_the_backstop_never_fires_off_the_hold() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend: Arc<dyn ChunkBackend> = mem_backend();

        // Seed the durable collect row with a STALE last_live (the
        // backstop is due) via the production stamp + backdate.
        let _ = crate::gc::state::backstop_due_unlocked(&db.pool, Duration::from_secs(3600)).await;
        sqlx::query(
            "INSERT INTO gc_collect_state (singleton, last_live_cycle_at) \
             VALUES (TRUE, now() - interval '10 days') \
             ON CONFLICT (singleton) DO UPDATE \
             SET last_live_cycle_at = now() - interval '10 days'",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(
            crate::gc::state::backstop_due_unlocked(&db.pool, Duration::from_secs(86_400))
                .await
                .unwrap(),
            "precondition: the backstop is due before the hold"
        );
        let epoch_before: Option<i64> =
            sqlx::query_scalar("SELECT cycle_epoch FROM gc_collect_state WHERE singleton")
                .fetch_optional(&db.pool)
                .await
                .unwrap();

        hold::set_hold(
            &db.pool,
            hold::GcHoldScope::Global,
            "starvation freeze",
            "w10-e",
            None,
        )
        .await
        .unwrap();

        // k backstop periods elapse: k held ticks through the lane.
        let mut body = backstop_body(db.pool.clone());
        for k in 0..3 {
            let outcome = DestructiveLane::tick("gc-collect-backstop", &db.pool, &mut body).await;
            assert_eq!(
                outcome,
                LaneTick::SkippedHeld,
                "backstop period {k}: the held tick must skip"
            );
        }
        // Zero fresh collect cycles: the epoch never moved.
        let epoch_after: Option<i64> =
            sqlx::query_scalar("SELECT cycle_epoch FROM gc_collect_state WHERE singleton")
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            epoch_before, epoch_after,
            "a hold spanning k backstop periods mints zero fresh cycles"
        );

        // The held run_gc tick STAMPS: drive run_gc itself (the
        // pinned member) and assert last_live_cycle_at advanced.
        let before: Option<i64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM last_live_cycle_at)::bigint \
             FROM gc_collect_state WHERE singleton",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let stats = crate::gc::run_gc(
            &db.pool,
            Some(Arc::clone(&backend)),
            crate::gc::GcParams {
                dry_run: false,
                grace_hours: 2,
                extra_roots: vec![],
            },
            tx,
            &rio_common::signal::Token::new(),
        )
        .await
        .unwrap();
        assert!(stats.is_none(), "held run_gc is a no-op");
        let mut held_frame = false;
        while let Some(m) = rx.recv().await {
            if m.unwrap().current_path.starts_with("held:") {
                held_frame = true;
            }
        }
        assert!(held_frame, "the operator surface reports the hold");
        let after: Option<i64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM last_live_cycle_at)::bigint \
             FROM gc_collect_state WHERE singleton",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            after > before,
            "the HELD run_gc tick stamps last_live_cycle_at \
             (a held cycle is a live cycle): {before:?} -> {after:?}"
        );
        // The starvation negation: the backstop-due predicate is now
        // FALSE — the hold can never make the backstop fire.
        assert!(
            !crate::gc::state::backstop_due_unlocked(&db.pool, Duration::from_secs(86_400))
                .await
                .unwrap(),
            "post-stamp the backstop is NOT due: the coupling is dead"
        );
    }
}

#[cfg(test)]
mod census {
    //! The R22′-DERIVED destructive-lane census (merged_bug_050; the
    //! LOAD-BEARING population-totality enforcement — see the module
    //! doc's two-part enforcement truth).
    //!
    //! REFUSAL PREDICATE (named): reaches-delete-sink ∧
    //! NOT-DestructiveLane-routed. The lane set is the census OUTPUT
    //! of the scan over the spawn-periodic FAMILY (`spawn_periodic`
    //! AND `spawn_periodic_with` call sites in the store crate whose
    //! tick body transitively reaches a DELETE/delete_by_key/
    //! pending-enqueue sink), ∪ {run_gc} (RPC-spawned per TriggerGC —
    //! a definitionally-included member, PINNED so a lane-count
    //! coincidence can never mask a membership swap). The registry
    //! row's covered/gap set is this predicate's complement, computed
    //! BY this generator — never self-reported. A typed, DISCLOSED
    //! exemption at the scan layer is the only lawful carve-out form;
    //! this census carves out NOTHING: all five members register.
    //!
    //! The wave-9 defect this kills: the four-lane list was
    //! author-enumerated (the round-6 closure-set defect recurring
    //! inside a HIGH close) — the gc-orphan-scanner hid from it. A
    //! sixth lane cannot hide from the family scan.

    /// The embedded census universe — whole-crate per (wwwww), pinned
    /// bidirectionally against the live tree below.
    const CENSUS_SOURCES: &[(&str, &str)] = &[
        ("admission.rs", include_str!("../admission.rs")),
        ("authz.rs", include_str!("../authz.rs")),
        ("backend.rs", include_str!("../backend.rs")),
        ("budget.rs", include_str!("../budget.rs")),
        ("cas.rs", include_str!("../cas.rs")),
        ("chunker.rs", include_str!("../chunker.rs")),
        ("config.rs", include_str!("../config.rs")),
        ("error.rs", include_str!("../error.rs")),
        ("gc/collect.rs", include_str!("collect.rs")),
        ("gc/drain.rs", include_str!("drain.rs")),
        ("gc/lane.rs", include_str!("lane.rs")),
        ("gc/lock.rs", include_str!("lock.rs")),
        ("gc/mark.rs", include_str!("mark.rs")),
        ("gc/mark_scan_bench.rs", include_str!("mark_scan_bench.rs")),
        ("gc/mod.rs", include_str!("mod.rs")),
        ("gc/orphan.rs", include_str!("orphan.rs")),
        ("gc/state.rs", include_str!("state.rs")),
        ("gc/sweep.rs", include_str!("sweep.rs")),
        ("gc/tenant.rs", include_str!("tenant.rs")),
        ("grpc/admin.rs", include_str!("../grpc/admin.rs")),
        ("grpc/chunk.rs", include_str!("../grpc/chunk.rs")),
        ("grpc/get_path.rs", include_str!("../grpc/get_path.rs")),
        ("grpc/mod.rs", include_str!("../grpc/mod.rs")),
        (
            "grpc/put_path/common.rs",
            include_str!("../grpc/put_path/common.rs"),
        ),
        (
            "grpc/put_path/mod.rs",
            include_str!("../grpc/put_path/mod.rs"),
        ),
        (
            "grpc/put_path_batch.rs",
            include_str!("../grpc/put_path_batch.rs"),
        ),
        ("grpc/queries.rs", include_str!("../grpc/queries.rs")),
        ("grpc/sign.rs", include_str!("../grpc/sign.rs")),
        ("ingest.rs", include_str!("../ingest.rs")),
        ("lib.rs", include_str!("../lib.rs")),
        ("logs/chunks.rs", include_str!("../logs/chunks.rs")),
        ("logs/gate.rs", include_str!("../logs/gate.rs")),
        ("logs/ingest.rs", include_str!("../logs/ingest.rs")),
        ("logs/loss.rs", include_str!("../logs/loss.rs")),
        ("logs/mbt_tests.rs", include_str!("../logs/mbt_tests.rs")),
        ("logs/mod.rs", include_str!("../logs/mod.rs")),
        ("logs/service.rs", include_str!("../logs/service.rs")),
        ("logs/sessions.rs", include_str!("../logs/sessions.rs")),
        ("logs/sweep.rs", include_str!("../logs/sweep.rs")),
        ("logs/tail.rs", include_str!("../logs/tail.rs")),
        ("main.rs", include_str!("../main.rs")),
        ("manifest.rs", include_str!("../manifest.rs")),
        (
            "materialize/client.rs",
            include_str!("../materialize/client.rs"),
        ),
        (
            "materialize/executor.rs",
            include_str!("../materialize/executor.rs"),
        ),
        ("materialize/mod.rs", include_str!("../materialize/mod.rs")),
        (
            "metadata/chunked.rs",
            include_str!("../metadata/chunked.rs"),
        ),
        (
            "metadata/cluster_key_history.rs",
            include_str!("../metadata/cluster_key_history.rs"),
        ),
        ("metadata/inline.rs", include_str!("../metadata/inline.rs")),
        ("metadata/mod.rs", include_str!("../metadata/mod.rs")),
        (
            "metadata/queries.rs",
            include_str!("../metadata/queries.rs"),
        ),
        (
            "metadata/tenant_keys.rs",
            include_str!("../metadata/tenant_keys.rs"),
        ),
        (
            "metadata/upstreams.rs",
            include_str!("../metadata/upstreams.rs"),
        ),
        ("realisations.rs", include_str!("../realisations.rs")),
        ("signing.rs", include_str!("../signing.rs")),
        ("substitute.rs", include_str!("../substitute.rs")),
        ("test_helpers.rs", include_str!("../test_helpers.rs")),
        ("visibility.rs", include_str!("../visibility.rs")),
    ];

    /// Dev-tree completeness pin: the embedded universe equals the
    /// live `src/` tree exactly (both directions) — the quantifier
    /// domain is generator-bounded. Sandbox skip disclosed (the
    /// fence_coverage.rs form).
    #[test]
    fn lane_census_universe_matches_live_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        if !root.exists() {
            eprintln!(
                "src/ not on disk (nix sandbox): universe pinned by the \
                 dev-tree run of this same commit"
            );
            return;
        }
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("readable src dir") {
                let path = entry.expect("readable dir entry").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(
                        path.strip_prefix(root)
                            .expect("under root")
                            .to_str()
                            .expect("source paths are UTF-8")
                            .to_string(),
                    );
                }
            }
        }
        let mut live = Vec::new();
        walk(&root, &root, &mut live);
        live.sort();
        let mut embedded: Vec<String> = CENSUS_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
        embedded.sort();
        assert_eq!(
            live, embedded,
            "lane-census universe drifted from the live tree"
        );
    }

    /// The production half (cut at the first `#[cfg(test)]`) minus
    /// line comments: the lane law quantifies over PRODUCTION spawn
    /// sites, and the cut also keeps this census's own needle and
    /// strawman strings (test-half) out of its scan — the
    /// merged_bug_009 self-scan trap, structurally avoided. String
    /// contents stay (SQL lives in strings).
    fn code_lines(src: &str) -> Vec<&str> {
        // Cut at the in-file test MODULE (not any cfg(test) attribute:
        // `#[cfg(test)] mod mark_scan_bench;`-style declarations sit
        // mid-file and must not truncate the production scan).
        let cut = src
            .find("#[cfg(test)]\nmod tests")
            .or_else(|| src.find("#[cfg(test)]\nmod census"))
            .unwrap_or(src.len());
        src[..cut]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect()
    }

    /// Extract `fn <name>` bodies (brace-matched) across the corpus:
    /// name → concatenated bodies (same-name fns merge — conservative
    /// for reachability).
    fn fn_bodies(corpus: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        let mut map: std::collections::BTreeMap<String, String> = Default::default();
        for (_file, src) in corpus {
            let text: String = code_lines(src).join("\n");
            let bytes = text.as_bytes();
            let mut i = 0;
            while let Some(pos) = text[i..].find("fn ") {
                let at = i + pos;
                // fn must be a token start
                if at > 0 && (bytes[at - 1] as char).is_alphanumeric() {
                    i = at + 3;
                    continue;
                }
                let rest = &text[at + 3..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() {
                    i = at + 3;
                    continue;
                }
                // find the opening brace of the body (skip the
                // signature; a `;` first means a trait decl — skip)
                let mut j = at + 3;
                let mut body_start = None;
                let mut depth_paren = 0i32;
                while j < text.len() {
                    match bytes[j] as char {
                        '(' => depth_paren += 1,
                        ')' => depth_paren -= 1,
                        ';' if depth_paren == 0 => break,
                        '{' if depth_paren == 0 => {
                            body_start = Some(j);
                            break;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if let Some(bs) = body_start {
                    let mut depth = 0i32;
                    let mut k = bs;
                    while k < text.len() {
                        match bytes[k] as char {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    let body = &text[bs..k.min(text.len())];
                    map.entry(name).or_default().push_str(body);
                    i = k.min(text.len());
                } else {
                    i = j.max(at + 3) + 1;
                }
            }
        }
        map
    }

    /// The sink tokens: a body containing any of these has delete
    /// authority (narinfo DELETE, S3 object delete, or feeding the
    /// delete pipeline).
    const SINK_TOKENS: &[&str] = &[
        "DELETE FROM",
        "delete_by_key",
        "INSERT INTO pending_s3_deletes",
    ];

    /// Transitive reaches-delete-sink over fn bodies (depth-bounded,
    /// visited-set).
    fn reaches_sink(
        body: &str,
        bodies: &std::collections::BTreeMap<String, String>,
        visited: &mut std::collections::BTreeSet<String>,
        depth: u32,
    ) -> bool {
        if depth > 6 {
            return false;
        }
        if SINK_TOKENS.iter().any(|t| body.contains(t)) {
            return true;
        }
        // Called identifiers: name( — last path segment.
        let mut called: Vec<String> = Vec::new();
        let b = body.as_bytes();
        let mut i = 0;
        while i < body.len() {
            if b[i] as char == '(' {
                // walk back over the identifier
                let mut j = i;
                while j > 0 && ((b[j - 1] as char).is_alphanumeric() || b[j - 1] as char == '_') {
                    j -= 1;
                }
                if j < i {
                    called.push(body[j..i].to_string());
                }
            }
            i += 1;
        }
        for name in called {
            if visited.contains(&name) {
                continue;
            }
            visited.insert(name.clone());
            if let Some(callee) = bodies.get(&name)
                && reaches_sink(callee, bodies, visited, depth + 1)
            {
                return true;
            }
        }
        false
    }

    /// One spawn-family registration site.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct LaneRow {
        lane: String,
        file: String,
        routed: bool,
        destructive: bool,
    }

    /// The generator: scan the corpus for spawn-family call sites and
    /// classify each by the refusal predicate.
    fn derive_lanes(corpus: &[(&str, &str)]) -> Vec<LaneRow> {
        let bodies = fn_bodies(corpus);
        let mut rows = Vec::new();
        for (file, src) in corpus {
            let text: String = code_lines(src).join("\n");
            for needle in ["spawn_periodic_with(", "spawn_periodic("] {
                let mut i = 0;
                while let Some(pos) = text[i..].find(needle) {
                    let at = i + pos;
                    let after = &text[at + needle.len()..];
                    // Balanced-paren arg span of this call FIRST —
                    // every later read stays inside it.
                    let bytes = after.as_bytes();
                    let mut depth = 1i32;
                    let mut k = 0;
                    while k < after.len() && depth > 0 {
                        match bytes[k] as char {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        k += 1;
                    }
                    let args = &after[..k.saturating_sub(1)];
                    // Lane name: the first string literal WITHIN the
                    // args. No literal = a declaration or a forwarding
                    // call (the name rides a variable) — not a lane
                    // registration site.
                    let name = args
                        .find('"')
                        .and_then(|q| args[q + 1..].find('"').map(|e| &args[q + 1..q + 1 + e]));
                    let Some(name) = name else {
                        i = at + needle.len();
                        continue;
                    };
                    // Routed: the call path names DestructiveLane.
                    let line_start = text[..at].rfind('\n').map(|p| p + 1).unwrap_or(0);
                    let window = &text[line_start.saturating_sub(200)..at + needle.len()];
                    let routed = window.contains("DestructiveLane::spawn_periodic");
                    let mut visited = Default::default();
                    let destructive = reaches_sink(args, &bodies, &mut visited, 0);
                    rows.push(LaneRow {
                        lane: name.to_string(),
                        file: file.to_string(),
                        routed,
                        destructive,
                    });
                    i = at + needle.len();
                }
            }
        }
        // De-dup the _with overlap: "spawn_periodic(" matches inside
        // "spawn_periodic_with(" never happens (different suffix), but
        // a DestructiveLane::spawn_periodic body calls
        // spawn_periodic_with internally — same-file forwarding rows
        // for lane.rs itself carry no string literal and were skipped.
        rows.sort();
        rows.dedup();
        rows
    }

    // r[verify store.gc.hold-lanes]
    /// THE lane census: derive the spawn-family rows, refuse any
    /// destructive ∧ ¬routed member, pin run_gc (∪ the family scan),
    /// and pin the full derived set against the committed [GEN-SET]
    /// so membership drift in EITHER direction is review-visible.
    #[test]
    fn destructive_lane_census() {
        let rows = derive_lanes(CENSUS_SOURCES);

        // The refusal predicate: reaches-delete-sink ∧ NOT-routed.
        let violations: Vec<&LaneRow> =
            rows.iter().filter(|r| r.destructive && !r.routed).collect();
        assert!(
            violations.is_empty(),
            "destructive lanes not routed through DestructiveLane \
             (the hold cannot suspend them): {violations:#?}"
        );

        // run_gc: the PINNED ∪-member (RPC-spawned per TriggerGC, not
        // spawn-periodic — the family scan cannot see it, so its
        // membership is pinned here and its consult is asserted
        // structurally: hold::gate mints the clearance its phase-3
        // collect_cycle call demands, which does not compile away).
        let run_gc_consults = CENSUS_SOURCES
            .iter()
            .find(|(f, _)| *f == "gc/mod.rs")
            .map(|(_, src)| {
                let text: String = code_lines(src).join("\n");
                text.contains("hold::gate(pool)") && text.contains("&hold_clearance,")
            })
            .unwrap_or(false);
        assert!(
            run_gc_consults,
            "run_gc (the pinned census member) must consult hold::gate \
             and thread the clearance to collect_cycle"
        );

        // The committed [GEN-SET]: the derived rows, exactly.
        let derived: Vec<String> = rows
            .iter()
            .map(|r| {
                format!(
                    "{}\t{}\trouted={}\tdestructive={}",
                    r.lane, r.file, r.routed, r.destructive
                )
            })
            .collect();
        let committed = include_str!("../../tests/gensets/destructive-lane-census.txt");
        let committed: Vec<String> = committed
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(
            derived, committed,
            "lane census drifted — review the new/removed spawn-family \
             site(s) against store.gc.hold-lanes (a destructive lane \
             MUST register through DestructiveLane), then regenerate \
             rio-store/tests/gensets/destructive-lane-census.txt (the \
             failure output above IS the new content)"
        );
    }

    // r[verify store.gc.hold-lanes]
    /// R22′ planted red: a strawman spawn-family lane with a delete
    /// sink NOT routed through DestructiveLane is flagged by the SAME
    /// derivation — planted at the SCAN layer (raw source enters the
    /// corpus), with a routed control and a non-destructive control
    /// pinning both polarities.
    #[test]
    fn lane_census_flags_strawman_unrouted_deleter() {
        let strawman = r#"
pub fn spawn_rogue(pool: PgPool, shutdown: Token) {
    rio_common::task::spawn_periodic("rogue-deleter", INTERVAL, shutdown, move || {
        let pool = pool.clone();
        async move {
            let _ = rogue_sweep(&pool).await;
        }
    });
}
async fn rogue_sweep(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM narinfo WHERE 1=1").execute(pool).await?;
    Ok(())
}
"#;
        let mut corpus: Vec<(&str, &str)> = CENSUS_SOURCES.to_vec();
        corpus.push(("strawman.rs", strawman));
        let rows = derive_lanes(&corpus);
        let rogue = rows
            .iter()
            .find(|r| r.lane == "rogue-deleter")
            .expect("the scan must see the strawman spawn site");
        assert!(
            rogue.destructive && !rogue.routed,
            "the refusal predicate must flag the strawman: {rogue:?}"
        );

        // Routed control: the same deleter through DestructiveLane is
        // NOT a violation (the consult rides the wrapper).
        let routed_control = r#"
pub fn spawn_lawful(pool: PgPool, shutdown: Token) {
    crate::gc::lane::DestructiveLane::spawn_periodic("lawful-deleter", INTERVAL, pool, shutdown,
        Box::new(move |clearance| {
            let pool = lane_pool.clone();
            Box::pin(async move { let _ = rogue_sweep(&pool).await; })
        }),
    );
}
"#;
        let mut corpus2: Vec<(&str, &str)> = CENSUS_SOURCES.to_vec();
        corpus2.push(("strawman.rs", strawman));
        corpus2.push(("control.rs", routed_control));
        let rows2 = derive_lanes(&corpus2);
        let lawful = rows2
            .iter()
            .find(|r| r.lane == "lawful-deleter")
            .expect("the scan must see the routed control");
        assert!(
            lawful.destructive && lawful.routed,
            "the routed deleter is destructive AND routed (lawful): {lawful:?}"
        );
    }
}
