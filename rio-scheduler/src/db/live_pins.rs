//! scheduler_live_pins — auto-pin live-build input closure.
//!
//! Scheduler+store share PG (same migrations/ dir). Scheduler writes
//! directly to scheduler_live_pins; store's gc/mark.rs seeds from it.
//! Best-effort: PG failure during pin/unpin logs + continues (24h grace
//! period is the fallback safety net).
// r[impl sched.gc.live-pins]

use uuid::Uuid;

use super::{SchedulerDb, terminal_status_sql};
use crate::state::DrvHash;

/// The stamp-authority witness (bug_139 / signed Q2 2026-06-07):
/// every `path_tenants` ownership write names the EVIDENCE CLASS that
/// authorizes it, and the funnel derives the lawful (path, tenant)
/// pairs FROM the witness — a witness-less stamp does not compile,
/// and a one-tenant verdict cannot be widened into all-tenant
/// ownership at any call site. Seven producer sites, four classes:
///
/// * walk Success → [`WalkVerified`](Self::WalkVerified) (the wire's
///   per-path verified-tenant sets; stamps INTERSECT);
/// * worker-built / recovery-orphan completion / the late-report
///   `Register` arm (round-9 WO-S1-1 — completed uploads survive
///   cancellation; tenants cold-resolved from the durable interest
///   rows) → [`BuiltLocally`](Self::BuiltLocally) (locally produced
///   bytes — all interested tenants lawful, signed under Q2);
/// * the dispatch locally-present lane →
///   [`AllTenantProbe`](Self::AllTenantProbe) (every stamped tenant's
///   own visibility-gated probe answered present);
/// * merge-time cache hits / CA-cutoff →
///   [`ProbedBy`](Self::ProbedBy) (single-evidence probe — stamps
///   ONLY the probing tenant).
#[derive(Debug)]
pub(crate) enum StampProvenance {
    /// Per-path wire-carried verified-tenant sets, keyed by
    /// sha256(store_path).
    WalkVerified(std::collections::HashMap<Vec<u8>, Vec<Uuid>>),
    /// Locally produced bytes: worker-built or recovery-adopted.
    BuiltLocally,
    /// Every stamped tenant's own visibility-gated FindMissingPaths
    /// answered present.
    AllTenantProbe,
    /// Single-tenant probe evidence (merge-time JWT probe): stamps
    /// only this tenant.
    ProbedBy(Uuid),
}

impl StampProvenance {
    /// Derive the lawful (path_hash, tenant) ownership pairs for one
    /// derivation's paths under this witness — THE one body
    /// (signed Q2); both the single-drv and the batched wrappers
    /// route through it.
    pub(crate) fn lawful_pairs(
        &self,
        output_paths: &[String],
        attributed: &[Uuid],
        hashes: &mut Vec<Vec<u8>>,
        tids: &mut Vec<Uuid>,
    ) {
        use sha2::Digest;
        for p in output_paths {
            let h = sha2::Sha256::digest(p.as_bytes()).to_vec();
            match self {
                StampProvenance::BuiltLocally | StampProvenance::AllTenantProbe => {
                    for t in attributed {
                        hashes.push(h.clone());
                        tids.push(*t);
                    }
                }
                StampProvenance::ProbedBy(probe_tenant) => {
                    if attributed.contains(probe_tenant) {
                        hashes.push(h.clone());
                        tids.push(*probe_tenant);
                    }
                }
                StampProvenance::WalkVerified(verified) => {
                    if let Some(vts) = verified.get(h.as_slice()) {
                        for t in attributed.iter().filter(|t| vts.contains(t)) {
                            hashes.push(h.clone());
                            tids.push(*t);
                        }
                    }
                }
            }
        }
    }
}

impl SchedulerDb {
    /// Pin a batch of store paths as live-build inputs for a drv.
    /// SHA-256 each path for store_path_hash (matches narinfo keying).
    /// ON CONFLICT DO NOTHING: re-pin is idempotent.
    pub(crate) async fn pin_live_inputs(
        &self,
        drv_hash: &DrvHash,
        store_paths: &[String],
    ) -> Result<(), sqlx::Error> {
        if store_paths.is_empty() {
            return Ok(());
        }
        use sha2::Digest;
        let hashes: Vec<Vec<u8>> = store_paths
            .iter()
            .map(|p| sha2::Sha256::digest(p.as_bytes()).to_vec())
            .collect();

        // Batch INSERT via UNNEST. Arrays are parallel (same length
        // by construction: same source vec). ON CONFLICT DO NOTHING
        // for idempotence — re-dispatching a drv (after reassign)
        // shouldn't error.
        //
        // `query!` (not runtime `query`): compile-checks the column
        // list against `rio_migrations::schema::LivePin` — store reads
        // these columns in gc/mark.rs + gc/sweep.rs.
        let drv_hashes = vec![drv_hash.as_str(); hashes.len()];
        sqlx::query!(
            r#"
            INSERT INTO scheduler_live_pins (store_path_hash, drv_hash)
            SELECT * FROM UNNEST($1::bytea[], $2::text[])
            ON CONFLICT DO NOTHING
            "#,
            &hashes,
            &drv_hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert the (output_path × tenant_id) cartesian product into
    /// path_tenants. SHA-256 each path (matches narinfo.store_path_hash
    /// keying — same as `pin_live_inputs`). ON CONFLICT DO NOTHING on
    /// the composite PK (store_path_hash, tenant_id): repeated builds
    /// of the same path by the same tenant are idempotent.
    ///
    /// Best-effort: caller warns on Err but does NOT fail completion.
    /// GC may under-retain a path if this upsert fails, but the build
    /// still succeeds (24h global grace is the fallback).
    ///
    /// Returns `rows_affected()` so callers/tests can assert on the
    /// delta (0 on re-call = idempotence proof).
    pub(crate) async fn upsert_path_tenants(
        &self,
        output_paths: &[String],
        tenant_ids: &[Uuid],
        provenance: &StampProvenance,
    ) -> Result<u64, sqlx::Error> {
        if output_paths.is_empty() || tenant_ids.is_empty() {
            return Ok(0);
        }
        // The pair set is DERIVED from the provenance (signed Q2): the
        // witness decides which (path, tenant) ownership rows are
        // lawful — a caller cannot widen a one-tenant verdict into the
        // attributed cartesian.
        let mut hashes: Vec<Vec<u8>> = Vec::new();
        let mut tids: Vec<Uuid> = Vec::new();
        provenance.lawful_pairs(output_paths, tenant_ids, &mut hashes, &mut tids);
        self.upsert_path_tenants_raw(&hashes, &tids, provenance)
            .await
    }

    /// Pre-flattened variant of [`upsert_path_tenants`]: caller has
    /// already built the parallel `(store_path_hash, tenant_id)` arrays
    /// (no cartesian product applied here). Used by the batched
    /// merge-time path where each drv may have a different tenant set,
    /// so the caller flattens across drvs and issues ONE round-trip
    /// instead of N. Same UNNEST + `ON CONFLICT DO NOTHING` semantics.
    ///
    /// [`upsert_path_tenants`]: Self::upsert_path_tenants
    pub(crate) async fn upsert_path_tenants_raw(
        &self,
        hashes: &[Vec<u8>],
        tids: &[Uuid],
        provenance: &StampProvenance,
    ) -> Result<u64, sqlx::Error> {
        debug_assert_eq!(hashes.len(), tids.len());
        // Belt-and-braces at the final funnel (signed Q2): the pairs
        // must be consistent with the witness even when a caller
        // pre-flattened them.
        if let StampProvenance::ProbedBy(probe_tenant) = provenance {
            debug_assert!(
                tids.iter().all(|t| t == probe_tenant),
                "ProbedBy stamps carry exactly the probing tenant"
            );
        }
        if let StampProvenance::WalkVerified(verified) = provenance {
            debug_assert!(
                hashes
                    .iter()
                    .zip(tids.iter())
                    .all(|(h, t)| verified.get(h.as_slice()).is_some_and(|v| v.contains(t))),
                "WalkVerified stamps are within the wire-carried sets"
            );
        }
        if hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            INSERT INTO path_tenants (store_path_hash, tenant_id)
            SELECT * FROM UNNEST($1::bytea[], $2::uuid[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(hashes)
        .bind(tids)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Round-9 WO-S1-3 — the IDENTITY half of the registration writer
    /// family (the signed Q1 invariant's second half: *registered
    /// evidence carries identity so resubmission re-associates*): fill
    /// the (output path ↔ deriver) linkage on the shared narinfo rows
    /// for paths whose uploader did not declare it. MONOTONE: only
    /// absent (`NULL`/`''`) deriver cells fill — a wire-declared
    /// deriver from the ingest lane is never overwritten (the uploader
    /// is closer to the truth). Parallel arrays pair each path hash
    /// with ITS drv_path so the batch funnel fills cross-drv in one
    /// round trip. Best-effort like every registration write.
    pub(crate) async fn fill_deriver_linkage(
        &self,
        path_hashes: &[Vec<u8>],
        drv_paths: &[String],
    ) -> Result<u64, sqlx::Error> {
        debug_assert_eq!(path_hashes.len(), drv_paths.len());
        if path_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            r#"
            UPDATE narinfo n
               SET deriver = u.d
              FROM UNNEST($1::bytea[], $2::text[]) AS u(h, d)
             WHERE n.store_path_hash = u.h
               AND (n.deriver IS NULL OR n.deriver = '')
            "#,
        )
        .bind(path_hashes)
        .bind(drv_paths)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Unpin all live inputs for a drv. Called on terminal status.
    /// Idempotent: unpinning a never-pinned drv = 0 rows deleted.
    ///
    /// Build-input pins ONLY (`pin_kind = 'build_input'`): a
    /// materialization pin for the same drv survives — its release is
    /// the all-interest-terminal rule
    /// ([`Self::release_materialization_pins_for_resolved_jobs`]),
    /// never the pinning build's terminal status (PP-2, design §5.3).
    /// Dormant flag-off: every as-built pin row carries the 078
    /// `'build_input'` default, so the predicate selects exactly the
    /// rows it always did.
    // r[impl sched.materialize.pinning]
    pub(crate) async fn unpin_live_inputs(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM scheduler_live_pins \
             WHERE drv_hash = $1 AND pin_kind = 'build_input'",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Batch variant of [`unpin_live_inputs`]: delete pins for many
    /// derivations in one round-trip. Paired with
    /// [`update_derivation_status_batch`] for the cancel-build path
    /// where N sequential unpins stalled the actor.
    ///
    /// Build-input pins only — same kind exclusion (and same dormancy
    /// argument) as [`unpin_live_inputs`].
    ///
    /// [`unpin_live_inputs`]: Self::unpin_live_inputs
    /// [`update_derivation_status_batch`]: Self::update_derivation_status_batch
    // r[impl sched.materialize.pinning]
    pub(crate) async fn unpin_live_inputs_batch(
        &self,
        drv_hashes: &[&str],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query!(
            "DELETE FROM scheduler_live_pins \
             WHERE drv_hash = ANY($1::text[]) AND pin_kind = 'build_input'",
            drv_hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Sweep stale pins: delete rows for derivations that are no
    /// longer in non-terminal state. Called after recovery (handles
    /// crash-between-pin-and-unpin — scheduler crashed after pin at
    /// dispatch but before unpin at completion).
    ///
    /// The subquery matches `load_nonterminal_derivations`' filter
    /// (both splice `terminal_status_sql!`): a drv NOT in that
    /// set is terminal (or deleted entirely).
    ///
    /// Build-input pins only: the sweep's premise ("terminal drv ⇒
    /// inputs no longer in use") is false for materialization pins,
    /// whose release is the all-interest-terminal rule (PP-2).
    // r[impl sched.materialize.pinning]
    pub(crate) async fn sweep_stale_live_pins(&self) -> Result<u64, sqlx::Error> {
        // Compile-time splice of the terminal-status tuple — see
        // terminal_status_sql! for why it isn't a bind param.
        let result = sqlx::query(terminal_status_sql!(
            r"
            DELETE FROM scheduler_live_pins
             WHERE pin_kind = 'build_input'
               AND drv_hash NOT IN (
               SELECT drv_hash FROM derivations
                WHERE status NOT IN ",
            r"
             )
            "
        ))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Pin store paths a materialization execution ingested or verified
    /// present (`pin_kind = 'materialization'`, `job_id` stamped) — the
    /// design §5.1 pin-at-ingest write, issued BEFORE the Success
    /// report is sent.
    ///
    /// Pin kinds are DISJOINT ROW SETS under the 093 key
    /// (store_path_hash, drv_hash, pin_kind): a build_input pin for the
    /// same (path, drv) is a different row with its own (build-terminal)
    /// release lifecycle, never re-kinded (bug_253 — the pre-093
    /// re-kind deleted a still-live build's only protecting row in the
    /// from_source sequence). Re-pinning the same materialization path
    /// is an idempotent `job_id` refresh.
    ///
    /// Executes [`rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL`] —
    /// the ONE shared text the store-side executor
    /// (`rio-store/src/materialize/executor.rs`) also runs against the
    /// shared PG (bug_192; PD-13: rio-store cannot link rio-scheduler,
    /// both link rio-migrations).
    // r[impl sched.materialize.pinning]
    /// Test-seeding twin (merged_bug_284 sweep): the PRODUCTION
    /// pin-at-ingest executes the same
    /// [`rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL`] from
    /// rio-store; this scheduler-side twin seeds the release-rule
    /// batteries against the EXACT production upsert shape.
    #[cfg(test)]
    pub(crate) async fn pin_materialized_paths(
        &self,
        job_id: Uuid,
        drv_hash: &DrvHash,
        store_paths: &[String],
    ) -> Result<(), sqlx::Error> {
        if store_paths.is_empty() {
            return Ok(());
        }
        use sha2::Digest;
        let hashes: Vec<Vec<u8>> = store_paths
            .iter()
            .map(|p| sha2::Sha256::digest(p.as_bytes()).to_vec())
            .collect();
        let drv_hashes: Vec<String> = vec![drv_hash.as_str().to_string(); hashes.len()];
        sqlx::query(rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL)
            .bind(&hashes)
            .bind(&drv_hashes)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The design §5.3 materialization release rule (release site iii —
    /// the recovery/housekeeping sweep arm): delete materialization
    /// pins whose job is RESOLVED (state ≠ 'pending') and for which NO
    /// live interest remains (no live build carries a wanted-relation
    /// row for the job's derivation — the `materialization_interest`
    /// view). Pins of unresolved jobs, and pins whose job still has a
    /// live interested build, always survive. Returns released-row
    /// count.
    ///
    /// Production callers (the Phase-A "no caller" note is history):
    /// the housekeeping release sweep (actor/materialize.rs), the
    /// build-terminal release (actor/build.rs via
    /// release_materialization_pins_best_effort), and the recovery
    /// sweep (actor/recovery.rs). The battery pins the rule.
    // r[impl sched.materialize.pinning]
    pub(crate) async fn release_materialization_pins_for_resolved_jobs(
        &self,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM scheduler_live_pins p \
              WHERE p.pin_kind = 'materialization' \
                AND EXISTS (SELECT 1 FROM materialization_jobs j \
                             WHERE j.job_id = p.job_id \
                               AND j.state <> 'pending') \
                AND NOT EXISTS (SELECT 1 FROM materialization_interest i \
                                 WHERE i.job_id = p.job_id)",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

// =======================================================================
// W9-B (round-9 WO-S1-1) — the registration-writer census, scheduler
// crate half. Proposition: ZERO uncensused registration writers of the
// tenant-ownership table — the ONE production SQL body is the
// witness-funneled `upsert_path_tenants_raw` above, and every caller
// of the writer family is a censused chokepoint (or a test exercising
// one of the db fns directly). Source-scanning generator (RC-1 class)
// over the EMBEDDED whole-crate universe (the substitute.rs
// CENSUS_SOURCES / fence_coverage.rs hybrid — the nix gate runs test
// binaries without the source tree on disk, hazard (vvvvv): a
// runtime-only walk is premise-unreachable exactly where it gates);
// completeness of the embed vs the live tree is pinned BOTH directions
// by `census_universe_matches_live_tree` on every dev run of the same
// commit (sandbox skip disclosed, never silent). Same-crate scan only
// — the store-crate half lives beside the store's ingest writer.
// =======================================================================
#[cfg(test)]
mod registration_writer_census {
    use std::collections::BTreeMap;

    /// EVERY `.rs` under `rio-scheduler/src`, embedded at compile time.
    /// Machine-generated (the generator command is recorded in the
    /// owning commit body); the completeness pin below forces this
    /// list to track the live tree exactly.
    const CENSUS_SOURCES: &[(&str, &str)] = &[
        ("actor/breaker.rs", include_str!("../actor/breaker.rs")),
        ("actor/build.rs", include_str!("../actor/build.rs")),
        ("actor/command.rs", include_str!("../actor/command.rs")),
        (
            "actor/completion.rs",
            include_str!("../actor/completion.rs"),
        ),
        ("actor/config.rs", include_str!("../actor/config.rs")),
        ("actor/debug.rs", include_str!("../actor/debug.rs")),
        ("actor/dispatch.rs", include_str!("../actor/dispatch.rs")),
        ("actor/event.rs", include_str!("../actor/event.rs")),
        ("actor/executor.rs", include_str!("../actor/executor.rs")),
        ("actor/floor.rs", include_str!("../actor/floor.rs")),
        ("actor/handle.rs", include_str!("../actor/handle.rs")),
        (
            "actor/housekeeping.rs",
            include_str!("../actor/housekeeping.rs"),
        ),
        (
            "actor/materialize.rs",
            include_str!("../actor/materialize.rs"),
        ),
        ("actor/merge.rs", include_str!("../actor/merge.rs")),
        ("actor/mod.rs", include_str!("../actor/mod.rs")),
        ("actor/pull.rs", include_str!("../actor/pull.rs")),
        ("actor/recovery.rs", include_str!("../actor/recovery.rs")),
        (
            "actor/report_ctx.rs",
            include_str!("../actor/report_ctx.rs"),
        ),
        ("actor/snapshot.rs", include_str!("../actor/snapshot.rs")),
        (
            "actor/tests/build.rs",
            include_str!("../actor/tests/build.rs"),
        ),
        (
            "actor/tests/completion.rs",
            include_str!("../actor/tests/completion.rs"),
        ),
        (
            "actor/tests/dispatch.rs",
            include_str!("../actor/tests/dispatch.rs"),
        ),
        (
            "actor/tests/establishment.rs",
            include_str!("../actor/tests/establishment.rs"),
        ),
        (
            "actor/tests/executor.rs",
            include_str!("../actor/tests/executor.rs"),
        ),
        (
            "actor/tests/fencing.rs",
            include_str!("../actor/tests/fencing.rs"),
        ),
        (
            "actor/tests/helpers.rs",
            include_str!("../actor/tests/helpers.rs"),
        ),
        (
            "actor/tests/integration.rs",
            include_str!("../actor/tests/integration.rs"),
        ),
        (
            "actor/tests/keep_going.rs",
            include_str!("../actor/tests/keep_going.rs"),
        ),
        (
            "actor/tests/lifecycle_sweep.rs",
            include_str!("../actor/tests/lifecycle_sweep.rs"),
        ),
        (
            "actor/tests/materialize.rs",
            include_str!("../actor/tests/materialize.rs"),
        ),
        (
            "actor/tests/merge.rs",
            include_str!("../actor/tests/merge.rs"),
        ),
        (
            "actor/tests/misc.rs",
            include_str!("../actor/tests/misc.rs"),
        ),
        ("actor/tests/mod.rs", include_str!("../actor/tests/mod.rs")),
        (
            "actor/tests/pull.rs",
            include_str!("../actor/tests/pull.rs"),
        ),
        (
            "actor/tests/recovery.rs",
            include_str!("../actor/tests/recovery.rs"),
        ),
        (
            "actor/tests/sla_contract.rs",
            include_str!("../actor/tests/sla_contract.rs"),
        ),
        (
            "actor/tests/wiring.rs",
            include_str!("../actor/tests/wiring.rs"),
        ),
        ("admin/builds.rs", include_str!("../admin/builds.rs")),
        ("admin/executors.rs", include_str!("../admin/executors.rs")),
        ("admin/gc.rs", include_str!("../admin/gc.rs")),
        ("admin/graph.rs", include_str!("../admin/graph.rs")),
        ("admin/mod.rs", include_str!("../admin/mod.rs")),
        ("admin/sla.rs", include_str!("../admin/sla.rs")),
        (
            "admin/spawn_intents.rs",
            include_str!("../admin/spawn_intents.rs"),
        ),
        ("admin/tenants.rs", include_str!("../admin/tenants.rs")),
        (
            "admin/tests/builds_tests.rs",
            include_str!("../admin/tests/builds_tests.rs"),
        ),
        (
            "admin/tests/gc_tests.rs",
            include_str!("../admin/tests/gc_tests.rs"),
        ),
        (
            "admin/tests/graph_tests.rs",
            include_str!("../admin/tests/graph_tests.rs"),
        ),
        ("admin/tests/mod.rs", include_str!("../admin/tests/mod.rs")),
        (
            "admin/tests/spawn_intents_tests.rs",
            include_str!("../admin/tests/spawn_intents_tests.rs"),
        ),
        (
            "admin/tests/tenants_tests.rs",
            include_str!("../admin/tests/tenants_tests.rs"),
        ),
        (
            "admin/tests/workers_tests.rs",
            include_str!("../admin/tests/workers_tests.rs"),
        ),
        ("assignment.rs", include_str!("../assignment.rs")),
        ("ca/mod.rs", include_str!("../ca/mod.rs")),
        ("ca/resolve.rs", include_str!("../ca/resolve.rs")),
        ("config.rs", include_str!("../config.rs")),
        ("critical_path.rs", include_str!("../critical_path.rs")),
        ("dag/mod.rs", include_str!("../dag/mod.rs")),
        ("dag/tests.rs", include_str!("../dag/tests.rs")),
        ("db/assignments.rs", include_str!("../db/assignments.rs")),
        ("db/attempts.rs", include_str!("../db/attempts.rs")),
        ("db/batch.rs", include_str!("../db/batch.rs")),
        ("db/builds.rs", include_str!("../db/builds.rs")),
        (
            "db/confirm_fences.rs",
            include_str!("../db/confirm_fences.rs"),
        ),
        ("db/derivations.rs", include_str!("../db/derivations.rs")),
        ("db/executions.rs", include_str!("../db/executions.rs")),
        ("db/history.rs", include_str!("../db/history.rs")),
        ("db/live_pins.rs", include_str!("../db/live_pins.rs")),
        (
            "db/materialization.rs",
            include_str!("../db/materialization.rs"),
        ),
        ("db/mod.rs", include_str!("../db/mod.rs")),
        (
            "db/open_attempts.rs",
            include_str!("../db/open_attempts.rs"),
        ),
        ("db/recovery.rs", include_str!("../db/recovery.rs")),
        ("db/tenants.rs", include_str!("../db/tenants.rs")),
        (
            "db/tests/assignments.rs",
            include_str!("../db/tests/assignments.rs"),
        ),
        (
            "db/tests/attempts.rs",
            include_str!("../db/tests/attempts.rs"),
        ),
        ("db/tests/batch.rs", include_str!("../db/tests/batch.rs")),
        ("db/tests/builds.rs", include_str!("../db/tests/builds.rs")),
        (
            "db/tests/confirm_fences.rs",
            include_str!("../db/tests/confirm_fences.rs"),
        ),
        (
            "db/tests/derivations.rs",
            include_str!("../db/tests/derivations.rs"),
        ),
        (
            "db/tests/fence_coverage.rs",
            include_str!("../db/tests/fence_coverage.rs"),
        ),
        (
            "db/tests/fenced_tx.rs",
            include_str!("../db/tests/fenced_tx.rs"),
        ),
        (
            "db/tests/history.rs",
            include_str!("../db/tests/history.rs"),
        ),
        (
            "db/tests/live_pins.rs",
            include_str!("../db/tests/live_pins.rs"),
        ),
        (
            "db/tests/materialization.rs",
            include_str!("../db/tests/materialization.rs"),
        ),
        ("db/tests/mod.rs", include_str!("../db/tests/mod.rs")),
        (
            "db/tests/open_attempts.rs",
            include_str!("../db/tests/open_attempts.rs"),
        ),
        (
            "db/tests/recovery.rs",
            include_str!("../db/tests/recovery.rs"),
        ),
        (
            "db/tests/tenants.rs",
            include_str!("../db/tests/tenants.rs"),
        ),
        (
            "db/tests/transactions.rs",
            include_str!("../db/tests/transactions.rs"),
        ),
        ("db/tests/wanted.rs", include_str!("../db/tests/wanted.rs")),
        ("db/wanted.rs", include_str!("../db/wanted.rs")),
        ("domain.rs", include_str!("../domain.rs")),
        (
            "grpc/actor_guards.rs",
            include_str!("../grpc/actor_guards.rs"),
        ),
        (
            "grpc/executor_service.rs",
            include_str!("../grpc/executor_service.rs"),
        ),
        ("grpc/mod.rs", include_str!("../grpc/mod.rs")),
        (
            "grpc/scheduler_service.rs",
            include_str!("../grpc/scheduler_service.rs"),
        ),
        (
            "grpc/tests/bridge_tests.rs",
            include_str!("../grpc/tests/bridge_tests.rs"),
        ),
        (
            "grpc/tests/guards_tests.rs",
            include_str!("../grpc/tests/guards_tests.rs"),
        ),
        ("grpc/tests/mod.rs", include_str!("../grpc/tests/mod.rs")),
        (
            "grpc/tests/pull_tests.rs",
            include_str!("../grpc/tests/pull_tests.rs"),
        ),
        (
            "grpc/tests/submit_tests.rs",
            include_str!("../grpc/tests/submit_tests.rs"),
        ),
        ("lease_hooks.rs", include_str!("../lease_hooks.rs")),
        ("lib.rs", include_str!("../lib.rs")),
        ("main.rs", include_str!("../main.rs")),
        ("observability.rs", include_str!("../observability.rs")),
        ("retry_policy.rs", include_str!("../retry_policy.rs")),
        ("sla/alpha.rs", include_str!("../sla/alpha.rs")),
        ("sla/bootstrap.rs", include_str!("../sla/bootstrap.rs")),
        ("sla/catalog.rs", include_str!("../sla/catalog.rs")),
        ("sla/config.rs", include_str!("../sla/config.rs")),
        ("sla/cost.rs", include_str!("../sla/cost.rs")),
        ("sla/dip.rs", include_str!("../sla/dip.rs")),
        ("sla/explain.rs", include_str!("../sla/explain.rs")),
        ("sla/explore.rs", include_str!("../sla/explore.rs")),
        ("sla/fit.rs", include_str!("../sla/fit.rs")),
        ("sla/hw.rs", include_str!("../sla/hw.rs")),
        ("sla/ingest.rs", include_str!("../sla/ingest.rs")),
        ("sla/metrics.rs", include_str!("../sla/metrics.rs")),
        ("sla/mod.rs", include_str!("../sla/mod.rs")),
        ("sla/override.rs", include_str!("../sla/override.rs")),
        ("sla/prior.rs", include_str!("../sla/prior.rs")),
        ("sla/quantile.rs", include_str!("../sla/quantile.rs")),
        ("sla/solve.rs", include_str!("../sla/solve.rs")),
        ("sla/types.rs", include_str!("../sla/types.rs")),
        ("state/build.rs", include_str!("../state/build.rs")),
        ("state/db_str.rs", include_str!("../state/db_str.rs")),
        (
            "state/derivation.rs",
            include_str!("../state/derivation.rs"),
        ),
        ("state/executor.rs", include_str!("../state/executor.rs")),
        ("state/mod.rs", include_str!("../state/mod.rs")),
        ("state/newtypes.rs", include_str!("../state/newtypes.rs")),
        (
            "state/recovered_instant.rs",
            include_str!("../state/recovered_instant.rs"),
        ),
        ("tests.rs", include_str!("../tests.rs")),
    ];

    /// Needles are assembled at runtime so the census never matches
    /// its own source text.
    fn census(parts: &[&str]) -> BTreeMap<String, usize> {
        census_over(
            &CENSUS_SOURCES
                .iter()
                .map(|(f, t)| (f.to_string(), (*t).to_string()))
                .collect::<Vec<_>>(),
            parts,
        )
    }

    /// The raw-source scanner the censuses run on — split out so the
    /// W10-N plant can push a STRAWMAN SOURCE FILE through the SAME
    /// outermost derivation layer (R22′: plants enter at the raw
    /// scan, never as post-extraction fixtures).
    fn census_over(universe: &[(String, String)], parts: &[&str]) -> BTreeMap<String, usize> {
        let needle = parts.join("");
        let mut hits = BTreeMap::new();
        for (rel, text) in universe {
            let n = text.matches(&needle).count();
            if n > 0 {
                *hits.entry(rel.clone()).or_insert(0) += n;
            }
        }
        hits
    }

    /// The one comparison every pinned census rides (factored so the
    /// W10-N plant proves THE CHECK rejects — not a weaker sibling):
    /// `Err` names the drifted rows verbatim.
    fn assert_census(
        actual: &BTreeMap<String, usize>,
        expected: &BTreeMap<String, usize>,
        what: &str,
    ) -> Result<(), String> {
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "{what}: census drifted.\n  actual:   {actual:?}\n  expected: {expected:?}\n\
                 every row is generated from the embedded-source scan — file the \
                 new consumer with a disposition (re-derives | membership-checked \
                 | priced-residual(named)) or remove the read"
            ))
        }
    }

    /// Dev-tree completeness pin: the embedded universe equals the
    /// live `src/` tree EXACTLY (both directions) — a new source file
    /// fails here until embedded, so the census quantifier domain is
    /// generator-bounded, never author-bounded. In the nix sandbox
    /// (no source dir) the embedded scan is the same commit's content;
    /// the skip is disclosed, not silent (the substitute.rs form).
    #[test]
    fn census_universe_matches_live_tree() {
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
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(
                        path.strip_prefix(root)
                            .expect("under root")
                            .to_str()
                            .expect("source paths are utf-8")
                            .to_owned(),
                    );
                }
            }
        }
        let mut live: Vec<String> = Vec::new();
        walk(&root, &root, &mut live);
        live.sort();
        let mut embedded: Vec<String> = CENSUS_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
        embedded.sort();
        assert_eq!(
            embedded, live,
            "census universe drifted from the live tree: add/remove the \
             named files in CENSUS_SOURCES so the registration census sees \
             the whole crate in the nix sandbox too"
        );
    }

    /// The SQL-body census: exactly ONE ownership-INSERT statement in
    /// this crate — the provenance-funneled writer in this file. A
    /// second SQL body anywhere is an uncensused stamp path (the W9-B
    /// reject). Test-seed INSERTs in OTHER crates are that crate's
    /// census's rows.
    #[test]
    fn one_production_insert_statement() {
        let hits = census(&["INSERT INTO ", "path_tenants"]);
        let expected: BTreeMap<String, usize> = [("db/live_pins.rs".to_string(), 1)].into();
        assert_eq!(
            hits, expected,
            "the ownership-INSERT census moved — every registration write \
             must route through the witness-funneled upsert_path_tenants_raw \
             (signed Q2); re-derive the pin only for a censused writer"
        );
    }

    /// The db-writer call-site census: the actor-side stamp
    /// chokepoints — completion.rs's `stamp_path_tenants` (single-drv
    /// funnel: the warm epilogue resolves AND the late-report Register
    /// arm) and `upsert_path_tenants_for_batch` (the I-139 batch
    /// funnel) — plus the in-family wrapper in this file and the
    /// db-layer tests that pin the writer fns' own semantics.
    #[test]
    fn writer_family_callers_pinned() {
        let mut hits = census(&[".upsert_path_tenants", "("]);
        for (k, v) in census(&[".upsert_path_tenants_raw", "("]) {
            *hits.entry(k).or_insert(0) += v;
        }
        let expected: BTreeMap<String, usize> = [
            // the wrapper delegating to _raw (one call, in-family)
            ("db/live_pins.rs".to_string(), 1),
            // stamp_path_tenants (single funnel) + the batch funnel
            ("actor/completion.rs".to_string(), 2),
            // db-fn semantics tests (idempotence/witness-law pins) —
            // they exercise the censused fns directly by design — plus
            // the bug_138 W10-M forged-report red's victim seeding
            // (tenant B's pre-existing rows are the I-217 baseline the
            // flip assertion reads; seeded through the censused writer
            // on purpose, never a raw INSERT)
            ("actor/tests/completion.rs".to_string(), 3),
        ]
        .into();
        assert_eq!(
            hits, expected,
            "a new caller of the path_tenants writer family appeared — \
             route it through the censused stamp chokepoints \
             (stamp_path_tenants / upsert_path_tenants_for_batch) or \
             census it here with its witness rationale"
        );
    }

    // r[verify sched.trust.report-membership]
    /// bug_138 commit 2 (W10-N) — the TAINT-TO-CONSUMER census, the
    /// per-consumer half of the membership law (RC-2(iii): a priced
    /// residual must name every downstream sink of the tainted field
    /// or the pricing is a weaker-sibling witness). Two generated
    /// member lists ([GEN-SET], R15/R22′ — derived from the
    /// embedded-source scan over the whole crate, never
    /// author-enumerated):
    ///
    /// (a) every consumer of the worker report's `built_outputs`
    ///     payload (the tainted object), and
    /// (b) every WRITE to the post-boundary carrier
    ///     (the dot-`output_paths` assignment shape) — the field the admitted
    ///     epilogue, events, registration, and CA planes all read.
    ///
    /// Per-row dispositions (the census IS the pricing record):
    ///
    /// payload (`built_outputs`-dot-read) consumers —
    /// - actor/completion.rs (11): the trust boundary itself — the
    ///   shape filter, the declared-name retain, the bug_138
    ///   MEMBERSHIP retain (admitted lane), the late-validator
    ///   inputs, the Register-apply membership check (durable row),
    ///   the post-retain epilogue write, and the CA bookkeeping.
    ///   Disposition: membership-checked; the CA-exempt face is the
    ///   NAMED priced residual — floating-CA paths are content-proven
    ///   by the store's verify_ca_store_path on upload + the gated
    ///   realisation insert; the evicted-CA modular-hash residual is
    ///   priced at the Register applier.
    /// - actor/pull.rs (1): feeds `validated_late_outputs` — the late
    ///   lane whose path law runs at apply. Membership-checked.
    /// - domain.rs (1): the proto→domain conversion — a
    ///   taint-PRESERVING carrier, not a sink; every consumer of the
    ///   converted value is one of this census's rows.
    /// - assignment.rs (1) / ca/resolve.rs (1): doc-comment
    ///   narrative only (no field read; the scan counts prose
    ///   honestly rather than special-casing comments).
    ///
    /// carrier (dot-`output_paths` assignment) writes —
    /// - actor/completion.rs (1): THE one worker-sourced write,
    ///   downstream of the membership retain. Membership-checked.
    /// - actor/dispatch.rs (1): re-derives (expected_output_paths
    ///   clone — scheduler-authoritative cache-hit completion).
    /// - actor/merge.rs (1): re-derives (store-probe cache hit over
    ///   the expected set).
    /// - actor/recovery.rs (1): re-derives (durable verified rows).
    /// - actor/materialize.rs (1): re-derives (store-verified
    ///   materialization carrier).
    /// - actor/debug.rs (1): re-derives (operator/test debug command
    ///   — not a worker surface).
    ///
    /// Downstream consumer planes the rows above feed, named for the
    /// pricing record (the repaired bug_138 lie — the visibility
    /// consumer is now FIRST-CLASS): path_tenants → GC tenant
    /// retention AND the store's own_built_projection →
    /// `visibility_verdict(owned=true) = Visible` (the I-217 flip
    /// channel W10-M pins); realisations → gateway QueryRealisation;
    /// completed events → client-facing output paths; FindMissingPaths
    /// probes → dispatch short-circuits.
    #[test]
    fn worker_report_taint_sinks_pinned() {
        let payload = census(&[".built_", "outputs"]);
        let expected_payload: BTreeMap<String, usize> = [
            ("actor/completion.rs".to_string(), 11),
            ("actor/pull.rs".to_string(), 1),
            ("assignment.rs".to_string(), 1),
            ("ca/resolve.rs".to_string(), 1),
            ("domain.rs".to_string(), 1),
        ]
        .into();
        assert_census(
            &payload,
            &expected_payload,
            "worker-report payload (built_outputs) consumers",
        )
        .unwrap();

        let writes = census(&[".output_paths", " = "]);
        let expected_writes: BTreeMap<String, usize> = [
            ("actor/completion.rs".to_string(), 1),
            ("actor/debug.rs".to_string(), 1),
            ("actor/dispatch.rs".to_string(), 1),
            ("actor/materialize.rs".to_string(), 1),
            ("actor/merge.rs".to_string(), 1),
            ("actor/recovery.rs".to_string(), 1),
        ]
        .into();
        assert_census(
            &writes,
            &expected_writes,
            "output_paths carrier writes (exactly one worker-sourced, membership-checked)",
        )
        .unwrap();
    }

    /// W10-N's planted red (R22′): a STRAWMAN unlisted sink — raw
    /// source text carrying a new payload consumer and a new carrier
    /// write — enters at the scanner layer (the outermost derivation
    /// layer, not a post-extraction fixture) and the census
    /// comparison MUST reject it, naming the strawman file. The
    /// strawman text is runtime-assembled so this file's static text
    /// never matches the needles itself.
    #[test]
    fn taint_census_plants_red_on_unlisted_sink() {
        let strawman = format!(
            "fn exfiltrate(r: &BuildResult) {{ for o in &r{}built_outputs {{ send(o); }} }}\n\
             fn clobber(state: &mut DerivationState) {{ state{}output_paths = stolen; }}\n",
            '.', '.'
        );
        let mut universe: Vec<(String, String)> = CENSUS_SOURCES
            .iter()
            .map(|(f, t)| (f.to_string(), (*t).to_string()))
            .collect();
        universe.push(("actor/strawman_sink.rs".to_string(), strawman));

        let payload = census_over(&universe, &[".built_", "outputs"]);
        let expected_payload: BTreeMap<String, usize> = [
            ("actor/completion.rs".to_string(), 11),
            ("actor/pull.rs".to_string(), 1),
            ("assignment.rs".to_string(), 1),
            ("ca/resolve.rs".to_string(), 1),
            ("domain.rs".to_string(), 1),
        ]
        .into();
        let err = assert_census(&payload, &expected_payload, "plant: payload consumers")
            .expect_err("an unlisted payload consumer MUST go census-red");
        assert!(
            err.contains("strawman_sink.rs"),
            "the red must NAME the unlisted sink; got: {err}"
        );

        let writes = census_over(&universe, &[".output_paths", " = "]);
        let expected_writes: BTreeMap<String, usize> = [
            ("actor/completion.rs".to_string(), 1),
            ("actor/debug.rs".to_string(), 1),
            ("actor/dispatch.rs".to_string(), 1),
            ("actor/materialize.rs".to_string(), 1),
            ("actor/merge.rs".to_string(), 1),
            ("actor/recovery.rs".to_string(), 1),
        ]
        .into();
        let err = assert_census(&writes, &expected_writes, "plant: carrier writes")
            .expect_err("an unlisted carrier write MUST go census-red");
        assert!(
            err.contains("strawman_sink.rs"),
            "the red must NAME the unlisted write; got: {err}"
        );
    }

    /// W9-G (round-9 WO-S1-3): realisation rows are written by
    /// registration writers ONLY — the scheduler's realisation-INSERT
    /// population is the `ca/resolve.rs` authority module (production
    /// insert fns + their in-file battery seeds), pinned by exact
    /// count. A realisation INSERT in any other file is an uncensused
    /// identity writer.
    #[test]
    fn realisation_writers_pinned() {
        let hits = census(&["INSERT INTO ", "realisations"]);
        let expected: BTreeMap<String, usize> = [("ca/resolve.rs".to_string(), 10)].into();
        assert_eq!(
            hits, expected,
            "the realisation-INSERT census moved — identity rows are \
             written by the ca/resolve.rs authority only; census new \
             writers here with their witness rationale"
        );
    }

    // r[verify sched.attempt.witnessed-terminal]
    /// live_058-b ([GEN-SET], the R23′ bind): the `bump_resource_floor`
    /// caller alphabet is MACHINE-PINNED — the fn doc's and the lib.rs
    /// HELP's "the callers are the alphabet" sentences cite THIS census
    /// instead of restating a list that goes stale (the restated-
    /// sentence arm is struck: an alphabet/ONLY claim takes the machine
    /// bind). Current population: the two worker-reported lanes
    /// (completion.rs — `cgroup_oom`, `timeout`) and the establishment
    /// sweep's witnessed-OomKilled disposition row (housekeeping.rs —
    /// `witnessed_oom`, live_058-b). A new caller reds here until it
    /// files its row, its label, and the lib.rs HELP; rides the same
    /// embedded-source universe as every census in this module (the
    /// dev-tree completeness pin is `census_universe_matches_live_tree`;
    /// the raw-layer plant is `worker_report_taint_sinks_pinned`'s
    /// strawman, which proves `assert_census` rejects and NAMES
    /// unlisted rows).
    #[test]
    fn bump_resource_floor_caller_census() {
        let hits = census(&[".bump_resource", "_floor("]);
        let expected: BTreeMap<String, usize> = [
            ("actor/completion.rs".to_string(), 2),
            ("actor/housekeeping.rs".to_string(), 1),
        ]
        .into();
        assert_census(&hits, &expected, "bump_resource_floor callers")
            .expect("the floor-promotion caller alphabet is census-pinned");
    }
}
