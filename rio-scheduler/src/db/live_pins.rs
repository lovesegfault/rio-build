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
// one of the db fns directly). Source-scanning generator (RC-1 class):
// the member list comes FROM the tree, never from an author-typed
// list. Same-crate scan only — the store-crate half of the census
// lives beside the store's ingest writer (hazard (vvvvv): a per-crate
// nix test sandbox stages only its own crate's source).
// =======================================================================
#[cfg(test)]
mod registration_writer_census {
    use std::collections::BTreeMap;
    use std::path::Path;

    fn scan(dir: &Path, needle: &str, hits: &mut BTreeMap<String, usize>, root: &Path) {
        for entry in std::fs::read_dir(dir).expect("readable src dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                scan(&path, needle, hits, root);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).expect("readable source file");
                let n = text.matches(needle).count();
                if n > 0 {
                    let rel = path
                        .strip_prefix(root)
                        .expect("under root")
                        .to_str()
                        .expect("source paths are utf-8")
                        .to_owned();
                    *hits.entry(rel).or_insert(0) += n;
                }
            }
        }
    }

    /// Needles are assembled at runtime (`concat`-free) so the census
    /// never matches its own source text.
    fn census(parts: &[&str]) -> BTreeMap<String, usize> {
        let needle = parts.join("");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = BTreeMap::new();
        scan(&root, &needle, &mut hits, &root);
        hits
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
            // they exercise the censused fns directly by design
            ("actor/tests/completion.rs".to_string(), 2),
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
}
