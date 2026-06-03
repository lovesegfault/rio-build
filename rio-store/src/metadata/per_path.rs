//! The per-path table lifecycle registry.
//!
//! bug_102 was the THIRD per-path table to silently join neither
//! deletion path (pattern R2: a multi-site invariant — "every per-path
//! table has a sweep policy and an invalidate policy" — maintained by
//! hand enumeration at each deletion site). This module retires the
//! class: the registry is the single enumeration, both deletion paths
//! iterate it, and an `information_schema` conformance test fails CI
//! when a migration adds a path-keyed table that no [`PerPathTable`]
//! variant covers — the policy decision becomes part of adding the
//! table, not an archaeology exercise after the next leak.
//!
//! The SQL strings are TODAY'S EXACT statements, moved verbatim from
//! `gc/sweep.rs::delete_swept_path` and `metadata/invalidate.rs` — the
//! registry changes where the statements LIVE, not what they say (the
//! GC hot path is byte-identical).

/// Orphaned `drv_modulo_cache` rows (no narinfo for their `.drv` —
/// the deriver was GC'd) are reclaimed by the GC tail once they are
/// older than this TTL. The const lives HERE, with the registry's
/// `Survive` declaration, so the lifecycle promise and its enforcement
/// share one source: the sweep preserves proof rows; the TTL bounds
/// how long an orphan's usefulness window lasts (a worker re-claiming
/// an output of a long-GC'd deriver after 90 days re-uploads the
/// closure and re-proves — `heal_if_missing` repopulates).
pub(crate) const DRV_MODULO_ORPHAN_TTL_DAYS: i64 = 90;

/// One per-path table. Iteration order of [`PerPathTable::ALL`] is the
/// EXECUTION order of both deletion paths and is load-bearing:
/// `RealisationDeps` first (its FKs are `ON DELETE RESTRICT` — the
/// junction rows must be deleted before the realisations they pin, on
/// BOTH deletion paths since round-16 bug_069) and
/// `Narinfo` last (it is the `ON DELETE CASCADE` root; everything that
/// reads pre-CASCADE state, e.g. the invalidate path's manifest
/// `FOR UPDATE` hoist, must run before it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PerPathTable {
    RealisationDeps,
    Realisations,
    PathTenants,
    DrvModuloCache,
    SchedulerLivePins,
    ManifestData,
    Manifests,
    Narinfo,
}

/// What the GC sweep does about a table.
///
/// The `via`/`rationale` payloads are the registry's self-description:
/// read by the policy-documentation test (and reviewers), not by the
/// deletion loops — the loops only execute `Delete` statements.
///
/// There is deliberately NO "rely on the FK to abort" variant
/// (round-16 bug_069 removed `RestrictGuard` from the type): a sweep
/// policy that executes nothing while an `ON DELETE RESTRICT` FK can
/// fire is not a guard, it is a standing wedge — the first aged
/// dep-linked CA chain aborted EVERY subsequent GC run permanently
/// (the same chain re-aborts each retry; nothing ever reclaims it).
/// Tables whose rows must outlive the swept path use `Survive`;
/// everything else declares its statement or its CASCADE parent.
#[allow(dead_code)] // documentation payloads; liveness pinned by every_policy_documents_itself
#[derive(Debug, Clone, Copy)]
pub(crate) enum SweepPolicy {
    /// Explicit `DELETE`, one bind: `store_path_hash`.
    Delete { sql: &'static str },
    /// Rows follow another table's delete via `ON DELETE CASCADE`.
    Cascade { via: &'static str },
    /// Rows survive the sweep BY DESIGN.
    Survive { rationale: &'static str },
}

/// What operator invalidation does about a table
/// (`store.admin.invalidate-total`: the deletion set is a declared
/// SUPERSET of the sweep's).
#[allow(dead_code)] // documentation payloads; liveness pinned by every_policy_documents_itself
#[derive(Debug, Clone, Copy)]
pub(crate) enum InvalidatePolicy {
    /// Unconditional `DELETE`.
    Delete { sql: &'static str, bind: Bind },
    /// `DELETE` skipped when the operator passed `keep_realisations`.
    DeleteUnlessKeptRealisations { sql: &'static str, bind: Bind },
    /// Rows follow another table's delete via `ON DELETE CASCADE`.
    Cascade { via: &'static str },
    /// Rows survive even operator invalidation.
    Survive { rationale: &'static str },
}

/// Which value the statement's `$1` binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bind {
    /// `sha256(store_path)` digest bytes.
    PathHash,
    /// The full store path string.
    PathText,
}

impl PerPathTable {
    /// Execution order — see the type doc.
    pub(crate) const ALL: [PerPathTable; 8] = [
        PerPathTable::RealisationDeps,
        PerPathTable::Realisations,
        PerPathTable::PathTenants,
        PerPathTable::DrvModuloCache,
        PerPathTable::SchedulerLivePins,
        PerPathTable::ManifestData,
        PerPathTable::Manifests,
        PerPathTable::Narinfo,
    ];

    /// `information_schema.tables` name. Read by the conformance test
    /// (the registry's schema linkage), not by the deletion loops.
    #[allow(dead_code)] // liveness pinned by information_schema_conformance
    pub(crate) fn table_name(self) -> &'static str {
        match self {
            PerPathTable::RealisationDeps => "realisation_deps",
            PerPathTable::Realisations => "realisations",
            PerPathTable::PathTenants => "path_tenants",
            PerPathTable::DrvModuloCache => "drv_modulo_cache",
            PerPathTable::SchedulerLivePins => "scheduler_live_pins",
            PerPathTable::ManifestData => "manifest_data",
            PerPathTable::Manifests => "manifests",
            PerPathTable::Narinfo => "narinfo",
        }
    }

    // r[impl store.db.per-path-registry+2]
    pub(crate) fn sweep_policy(self) -> SweepPolicy {
        match self {
            // Step 2a-pre: DELETE realisation_deps in BOTH FK roles
            // BEFORE the realisations delete (round-16 bug_069). Both
            // FKs are ON DELETE RESTRICT; with no statement here, a
            // swept path whose realisation participates in any edge
            // (either role) aborted the whole sweep transaction — and
            // since the same aged chain stays unreachable, EVERY
            // subsequent GC run re-aborted: a permanent wedge, not a
            // surfaced bug. Migration 015's RESTRICT intent
            // ("orphaning edges is a bug to surface") still holds for
            // every NON-sweep deleter — the FK fires for any writer
            // that did not first declare an edge policy here. The
            // reverse-role subselect is fast via
            // realisation_deps_reverse_idx; the forward role rides the
            // PK prefix.
            PerPathTable::RealisationDeps => SweepPolicy::Delete {
                sql: r#"
        DELETE FROM realisation_deps
         WHERE (drv_hash, output_name) IN (
                 SELECT drv_hash, output_name FROM realisations
                  WHERE output_path = (
                    SELECT store_path FROM narinfo WHERE store_path_hash = $1
                  )
               )
            OR (dep_drv_hash, dep_output_name) IN (
                 SELECT drv_hash, output_name FROM realisations
                  WHERE output_path = (
                    SELECT store_path FROM narinfo WHERE store_path_hash = $1
                  )
               )
        "#,
            },
            // Step 2a: DELETE realisations. NOT via CASCADE — realisations
            // has NO FK to narinfo (002_store.sql:134). Without this,
            // dangling realisations rows point to swept paths →
            // wopQueryRealisation returns a path that 404s on fetch.
            // realisations_output_idx makes the subselect fast.
            PerPathTable::Realisations => SweepPolicy::Delete {
                sql: r#"
        DELETE FROM realisations
         WHERE output_path = (
           SELECT store_path FROM narinfo WHERE store_path_hash = $1
         )
        "#,
            },
            // Step 2a': DELETE path_tenants. NOT via CASCADE — path_tenants
            // has NO FK to narinfo (012_path_tenants.sql). Without this,
            // orphaned rows survive the sweep and grant wrong-tenant
            // visibility when a different tenant later re-uploads the same
            // store path (the stale row still JOINs in the
            // r[store.gc.tenant-retention] CTE arm).
            PerPathTable::PathTenants => SweepPolicy::Delete {
                sql: "DELETE FROM path_tenants WHERE store_path_hash = $1",
            },
            PerPathTable::DrvModuloCache => SweepPolicy::Survive {
                rationale: "store.put.ia-deriver-proof+4: proofs survive deriver GC \
                            ('previously verified against resident bytes'); growth is \
                            bounded by the DRV_MODULO_ORPHAN_TTL_DAYS reclaim in the GC \
                            tail, not the per-path sweep",
            },
            PerPathTable::SchedulerLivePins => SweepPolicy::Survive {
                rationale: "scheduler-owned liveness — pins PREVENT sweeps (consulted in \
                            recheck_has_live_referrer); the scheduler deletes its own pins",
            },
            PerPathTable::ManifestData => SweepPolicy::Cascade { via: "manifests" },
            PerPathTable::Manifests => SweepPolicy::Cascade { via: "narinfo" },
            // Step 2b: DELETE narinfo. CASCADE takes manifests,
            // manifest_data.
            PerPathTable::Narinfo => SweepPolicy::Delete {
                sql: "DELETE FROM narinfo WHERE store_path_hash = $1",
            },
        }
    }

    // r[impl store.db.per-path-registry+2]
    pub(crate) fn invalidate_policy(self) -> InvalidatePolicy {
        match self {
            // Junction rows first: their FKs are ON DELETE RESTRICT, so
            // the realisations delete below would otherwise fail whenever
            // the invalidated realisation participates in a dependency
            // edge. An explicit operator invalidation is precisely the
            // case where removing the edges is intended rather than a bug
            // to surface.
            PerPathTable::RealisationDeps => InvalidatePolicy::DeleteUnlessKeptRealisations {
                sql: r#"
            DELETE FROM realisation_deps
             WHERE (drv_hash, output_name) IN (
                     SELECT drv_hash, output_name FROM realisations WHERE output_path = $1
                   )
                OR (dep_drv_hash, dep_output_name) IN (
                     SELECT drv_hash, output_name FROM realisations WHERE output_path = $1
                   )
            "#,
                bind: Bind::PathText,
            },
            PerPathTable::Realisations => InvalidatePolicy::DeleteUnlessKeptRealisations {
                sql: "DELETE FROM realisations WHERE output_path = $1",
                bind: Bind::PathText,
            },
            PerPathTable::PathTenants => InvalidatePolicy::Delete {
                sql: "DELETE FROM path_tenants WHERE store_path_hash = $1",
                bind: Bind::PathHash,
            },
            // r[impl store.admin.invalidate-total]
            // drv_modulo_cache keys on `drv_path_hash = sha256(store_path)`
            // — the SAME digest as the other hash-keyed tables
            // (StorePath::sha256_digest is sha256 over the full path
            // string; populate_on_ingest hashes the identical string).
            // Unconditional: for non-.drv paths the row simply never
            // exists and this is a no-op.
            PerPathTable::DrvModuloCache => InvalidatePolicy::Delete {
                sql: "DELETE FROM drv_modulo_cache WHERE drv_path_hash = $1",
                bind: Bind::PathHash,
            },
            PerPathTable::SchedulerLivePins => InvalidatePolicy::Survive {
                rationale: "scheduler-owned liveness: un-pinning a path the scheduler still \
                            wants is the scheduler's call, not the store operator's; the pin \
                            does not make the path a cache hit",
            },
            PerPathTable::ManifestData => InvalidatePolicy::Cascade { via: "manifests" },
            PerPathTable::Manifests => InvalidatePolicy::Cascade { via: "narinfo" },
            PerPathTable::Narinfo => InvalidatePolicy::Delete {
                sql: "DELETE FROM narinfo WHERE store_path_hash = $1",
                bind: Bind::PathHash,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Ordering invariants the deletion paths rely on (see the type
    /// doc): RESTRICT-guarded junction rows first, the CASCADE root
    /// last.
    #[test]
    fn execution_order_pins() {
        assert_eq!(PerPathTable::ALL[0], PerPathTable::RealisationDeps);
        assert_eq!(PerPathTable::ALL[7], PerPathTable::Narinfo);
        // Realisations deps must precede realisations; manifests
        // (CASCADE parent of manifest_data) must follow it in the
        // declaration even though both are cascades.
        let pos = |t: PerPathTable| PerPathTable::ALL.iter().position(|x| *x == t).unwrap();
        assert!(pos(PerPathTable::RealisationDeps) < pos(PerPathTable::Realisations));
        assert!(pos(PerPathTable::DrvModuloCache) < pos(PerPathTable::Narinfo));
    }

    /// Every policy carries its own documentation: Delete carries the
    /// statement, Cascade names its parent, Survive carries the
    /// rationale. Reading them here keeps the payloads live (they
    /// are the registry's self-description, not dead metadata).
    #[test]
    fn every_policy_documents_itself() {
        for t in PerPathTable::ALL {
            match t.sweep_policy() {
                SweepPolicy::Delete { sql } => assert!(sql.contains("DELETE"), "{t:?}"),
                SweepPolicy::Cascade { via } => assert!(!via.is_empty(), "{t:?}"),
                SweepPolicy::Survive { rationale } => {
                    assert!(!rationale.is_empty(), "{t:?}")
                }
            }
            match t.invalidate_policy() {
                InvalidatePolicy::Delete { sql, .. }
                | InvalidatePolicy::DeleteUnlessKeptRealisations { sql, .. } => {
                    assert!(sql.contains("DELETE"), "{t:?}")
                }
                InvalidatePolicy::Cascade { via } => assert!(!via.is_empty(), "{t:?}"),
                InvalidatePolicy::Survive { rationale } => assert!(!rationale.is_empty(), "{t:?}"),
            }
        }
    }

    // r[verify store.db.per-path-registry+2]
    /// `information_schema` conformance, both directions:
    ///
    /// 1. Every table in the migrated schema with a path-shaped column
    ///    is REGISTERED here or EXEMPT with a named owner — a migration
    ///    adding per-path table N+1 fails THIS test until a
    ///    [`PerPathTable`] variant (and therefore both policies)
    ///    exists.
    /// 2. Every registered table exists in the schema — a renamed or
    ///    dropped table cannot leave a stale registry entry.
    #[tokio::test]
    async fn information_schema_conformance() {
        // Path-shaped columns. drv_hash/output_name composites are NOT
        // path-shaped (they key realisations' identity, not a store
        // path); realisation_deps is registered because its LIFECYCLE
        // is per-path via realisations, not because of its columns.
        const PATH_COLUMNS: &[&str] = &[
            "store_path_hash",
            "store_path",
            "output_path",
            "drv_path",
            "drv_path_hash",
        ];
        // Tables with path-shaped columns whose lifecycle the STORE's
        // deletion paths deliberately do not own. Each entry names the
        // owner; removing a table from the schema removes it here too
        // (direction 2 covers the registry, this list is checked
        // against the live schema below).
        const EXEMPT: &[(&str, &str)] = &[
            // Scheduler-owned (shared migration stream): the scheduler
            // manages these rows' lifecycle; the store's sweep and
            // invalidation never touch them.
            ("derivations", "scheduler"),
            ("assignments", "scheduler"),
            ("build_derivations", "scheduler"),
            ("builds", "scheduler"),
            ("build_event_log", "scheduler"),
            ("build_samples", "scheduler"),
            ("drv_logs", "scheduler"),
            ("derivation_edges", "scheduler"),
        ];

        let db = TestDb::new(&crate::MIGRATOR).await;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT table_name::text, column_name::text \
               FROM information_schema.columns \
              WHERE table_schema = 'public' \
                AND column_name = ANY($1)",
        )
        .bind(
            PATH_COLUMNS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();

        let registered: std::collections::BTreeSet<&str> =
            PerPathTable::ALL.iter().map(|t| t.table_name()).collect();
        let exempt: std::collections::BTreeSet<&str> = EXEMPT.iter().map(|(t, _)| *t).collect();

        // Direction 1: schema ⊆ registry ∪ exempt.
        for (table, column) in &rows {
            assert!(
                registered.contains(table.as_str()) || exempt.contains(table.as_str()),
                "table `{table}` has path-shaped column `{column}` but is neither \
                 registered in PerPathTable (rio-store/src/metadata/per_path.rs) nor \
                 exempt-with-owner. Resolve by EITHER adding a PerPathTable variant \
                 (forcing a sweep policy AND an invalidate policy) OR adding an \
                 EXEMPT entry naming the owning component."
            );
        }

        // Direction 2: registry ∪ exempt ⊆ schema.
        let schema_tables: std::collections::BTreeSet<String> =
            rows.iter().map(|(t, _)| t.clone()).collect();
        for t in PerPathTable::ALL {
            // realisation_deps has no path-shaped column of its own —
            // assert plain existence instead.
            let name = t.table_name();
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                  WHERE table_schema = 'public' AND table_name = $1)",
            )
            .bind(name)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(
                exists,
                "registered table `{name}` does not exist in the migrated schema — \
                 remove the stale PerPathTable variant in \
                 rio-store/src/metadata/per_path.rs or fix the migration"
            );
            if name != "realisation_deps" {
                assert!(
                    schema_tables.contains(name),
                    "registered table `{name}` exists but lost its path-shaped column — \
                     re-derive its policies in rio-store/src/metadata/per_path.rs"
                );
            }
        }
        for (name, _owner) in EXEMPT {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                  WHERE table_schema = 'public' AND table_name = $1)",
            )
            .bind(name)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert!(
                exists,
                "EXEMPT entry `{name}` does not exist in the migrated schema — drop the \
                 stale exemption in rio-store/src/metadata/per_path.rs"
            );
        }
    }
}
