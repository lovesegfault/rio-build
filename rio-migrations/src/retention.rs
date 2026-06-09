//! The table-retention registry (bughunt wave D1, merged_bug_163;
//! typed + symbol-validated in bughunt-2, merged_bug_001/142).
//!
//! Every PUBLIC table must declare its row lifecycle: a named sweeper
//! whose existence is machine-checked, a parent CASCADE resolved in
//! the named migration, or a written rationale for living forever.
//! "New table, no lifecycle decision" fails CI — `tests/retention.rs`
//! diffs `pg_tables` against this registry, so a migration that
//! creates a table without a registry row (or removes a table without
//! removing its row) is caught at merge time.
//!
//! The POLICY CLAIMS are checked too: `xtask lint retention-truth`
//! resolves every [`RetentionPolicy::SweptBy`] symbol in non-test
//! workspace source and requires the defining file to carry the
//! deleting statement, and resolves every
//! [`RetentionPolicy::CascadeFrom`] against the named migration's FK
//! clause. A phantom attribution ("swept by X" where X doesn't exist,
//! doesn't delete this table, or the FK is RESTRICT) fails CI naming
//! the table — it can no longer grep to nothing while rows grow
//! forever.
//!
//! This is the structural close of the class merged_bug_163 found:
//! migration 078 shipped `materialization_jobs` and
//! `build_wanted_outputs` with no deletion lifecycle at all — both
//! grew forever (resolved jobs and dead builds' wanted rows were
//! never deleted by anything). merged_bug_001/142 found the next
//! stratum: rows that EXISTED but lied (`drv_executions` credited to
//! a store sweep that is forbidden from touching it; `jwt_revoked`
//! credited to a TTL sweep that was never written).

/// How a table's rows leave the table. Machine-checked — see the
/// module doc and `xtask/src/lint.rs::retention_truth`.
#[derive(Debug, Clone, Copy)]
pub enum RetentionPolicy {
    /// Rows are deleted by the named sweeper/reaper. `symbol` is a
    /// REAL function name: it must define in non-test workspace
    /// source, and a defining file must contain `DELETE FROM {table}`
    /// or a `TRUNCATE` list naming the table (line-continuation
    /// normalized). Not documentation — a lint-resolved claim.
    SweptBy {
        /// The deleting function's name (greppable, lint-resolved).
        symbol: &'static str,
        /// Context: tick arm, guards, companion paths.
        note: &'static str,
    },
    /// Rows die with their parent row: the named migration carries
    /// `REFERENCES {parent} … ON DELETE CASCADE` for this table
    /// (lint-resolved; an `ON DELETE RESTRICT` FK does NOT satisfy
    /// this — that was exactly merged_bug_142's phantom class).
    CascadeFrom {
        /// The parent table whose DELETE takes these rows.
        parent: &'static str,
        /// Migration file (basename) declaring the CASCADE FK.
        migration: &'static str,
        /// Context: who deletes the parent.
        note: &'static str,
    },
    /// Rows deliberately live forever; the rationale says why that is
    /// sound (bounded cardinality, append-only audit value, singleton)
    /// — or records an HONEST retention debt (growth is visible here
    /// instead of behind a false sweeper claim; §5-Q18 disposition).
    KeepForever(&'static str),
}

/// Every public table and its lifecycle. Keep ALPHABETICAL — the test
/// output diff is then stable.
// r[impl sched.db.table-retention+1]
pub const RETENTION_REGISTRY: &[(&str, RetentionPolicy)] = &[
    (
        "assignments",
        RetentionPolicy::CascadeFrom {
            parent: "derivations",
            migration: "034_assignments_terminal_backfill.sql",
            note: "rows die with their derivation (the orphan GC / delete_build \
                   family takes the parent); active-row CLOSES are status \
                   transitions through close_assignments_sql, not deletions — \
                   the prior 'tick_sweep_dispatched_cells' row credited a \
                   closer as a deleter",
        },
    ),
    (
        "build_derivations",
        RetentionPolicy::CascadeFrom {
            parent: "builds",
            migration: "008_round4.sql",
            note: "delete_build's Z1 cascade takes the join rows",
        },
    ),
    (
        "build_samples",
        RetentionPolicy::SweptBy {
            symbol: "delete_samples_older_than",
            note: "SLA sample TTL sweep (db/history.rs); ResetSlaModel truncates \
                   alongside hw_perf_samples",
        },
    ),
    (
        "build_wanted_outputs",
        RetentionPolicy::SweptBy {
            symbol: "gc_dead_build_wanted_outputs",
            note: "tick_gc_build_wanted_outputs arm; delete_build removes a \
                   build's rows in the same transaction",
        },
    ),
    (
        "builds",
        RetentionPolicy::KeepForever(
            "DEBT (Q18 disposition): delete_build exists but is operator/API-driven \
             (dashboard delete) — no autonomous age sweep bounds growth. Recorded \
             honestly so the growth is visible here instead of behind a sweeper \
             claim that names no retention path",
        ),
    ),
    (
        "chunks",
        RetentionPolicy::SweptBy {
            symbol: "collect_cycle",
            note: "soft-delete + post-pass tombstone reap (091 deleted_at); the \
                   reap/collect quals render from the shared row-local predicate \
                   constants",
        },
    ),
    (
        "cluster_key_history",
        RetentionPolicy::KeepForever(
            "key-rotation audit trail; one row per rotation event — cardinality bounded by \
             operational rotations, audit value permanent",
        ),
    ),
    (
        "derivation_edges",
        RetentionPolicy::SweptBy {
            symbol: "gc_orphan_terminal_derivations",
            note: "the orphan GC's edge arm (one CTE with the derivations delete) \
                   — the prior 'delete_build closure cleanup' row credited a fn \
                   that deletes no edge rows",
        },
    ),
    (
        "derivations",
        RetentionPolicy::SweptBy {
            symbol: "gc_orphan_terminal_derivations",
            note: "terminal-status orphan sweep (one CTE with the edge arm)",
        },
    ),
    (
        "drv_attempts",
        RetentionPolicy::SweptBy {
            symbol: "gc_attempt_ledger",
            note: "the TG sweep (tick_gc_attempt_ledger), kernel-proven cut",
        },
    ),
    (
        "drv_executions",
        RetentionPolicy::SweptBy {
            symbol: "gc_exec_rows",
            note: "the scheduler's execution-row GC — store.log.sweep-ownership+1 \
                   FORBIDS the store's log TTL sweep from touching these rows \
                   (the prior row credited exactly that sweep); six-conjunct \
                   eligibility incl. artifact-before-row + the close-stamp \
                   liveness half (sched.db.exec-stamp-on-close)",
        },
    ),
    (
        "drv_log_chunks",
        RetentionPolicy::SweptBy {
            symbol: "sweep_expired_logs",
            note: "the store's log TTL sweep (log retention)",
        },
    ),
    (
        "executor_confirm_fences",
        RetentionPolicy::SweptBy {
            symbol: "gc_confirm_fences",
            note: "the attempt-ledger housekeeping tick's fence rider \
                   (merged_bug_145): rows older than 24h — any straggler \
                   pull has long since timed out; one row per \
                   confirm-exited pod, so volume tracks pod churn",
        },
    ),
    (
        "gc_collect_state",
        RetentionPolicy::KeepForever(
            "singleton row (CHECK (singleton)); the collector's durable state",
        ),
    ),
    (
        "hw_cost_factors",
        RetentionPolicy::KeepForever(
            "dead on arrival: ADR-023 chose sla_ema_state for the EMA persist; rows are \
             never written (xtask schema_liveness ALLOW_DEAD names it); DROP TABLE \
             deferred to a future migration because 042 is checksum-frozen",
        ),
    ),
    (
        "hw_perf_samples",
        RetentionPolicy::SweptBy {
            symbol: "check_reference_epoch",
            note: "TRUNCATE rides the reference-class epoch reset ONLY; reads are \
                   bounded to a 7-day window (sla/hw.rs) — no age sweep exists \
                   today, the SLA owner's recorded debt",
        },
    ),
    (
        "interrupt_samples",
        RetentionPolicy::SweptBy {
            symbol: "sweep_interrupt_samples",
            note: "sla/cost.rs retention window",
        },
    ),
    (
        "jwt_revoked",
        RetentionPolicy::KeepForever(
            "DEBT (Q18 disposition): the prior row credited a 'revocation TTL \
             sweep' that was never written — no deleter exists. Growth is bounded \
             in practice by revocation volume; a bounded-window sweep becomes \
             sound if the owner confirms a cluster-wide max JWT lifetime \
             (exp−iat) — recorded debt until then",
        ),
    ),
    (
        "leader_generation_claims",
        RetentionPolicy::KeepForever(
            "one row per leader generation; the claims-floor fence reads MAX(generation) — \
             cardinality bounded by elections, the floor's history is the safety artifact",
        ),
    ),
    (
        "log_ingest_sessions",
        RetentionPolicy::SweptBy {
            symbol: "sweep_expired_logs",
            note: "stale-session reap rides the log TTL sweep; live sessions die \
                   at close (close_session)",
        },
    ),
    (
        "manifest_data",
        RetentionPolicy::CascadeFrom {
            parent: "manifests",
            migration: "002_store.sql",
            note: "chunk-list rows die with their manifest",
        },
    ),
    (
        "manifests",
        RetentionPolicy::CascadeFrom {
            parent: "narinfo",
            migration: "002_store.sql",
            note: "delete_swept_path's narinfo DELETE cascades here; \
                   delete_manifest_uploading covers aborted uploads",
        },
    ),
    (
        "materialization_jobs",
        RetentionPolicy::SweptBy {
            symbol: "gc_resolved_materialization_jobs",
            note: "tick_gc_materialization_jobs; forensic horizon + live-pin and \
                   interest guards (sched.db.table-retention)",
        },
    ),
    (
        "narinfo",
        RetentionPolicy::SweptBy {
            symbol: "delete_swept_path",
            note: "the sweep phase's per-path metadata delete",
        },
    ),
    (
        "nodeclaim_cell_state",
        RetentionPolicy::KeepForever(
            "bounded cell catalog: one row per (pool, cell); cardinality follows \
             the fleet configuration and the rows are the controller's durable \
             evidence — the prior 'reconciler deletes' claim named no deleting \
             code",
        ),
    ),
    (
        "path_tenants",
        RetentionPolicy::SweptBy {
            symbol: "delete_swept_path",
            note: "explicit per-path delete (no FK to narinfo — 012)",
        },
    ),
    (
        "pending_s3_deletes",
        RetentionPolicy::SweptBy {
            symbol: "drain_one_row",
            note: "drain task: delete on S3 success / max-attempts alert",
        },
    ),
    (
        "realisation_deps",
        RetentionPolicy::KeepForever(
            "DEBT: no deleter exists and the FK to realisations is ON DELETE \
             RESTRICT (015) — the prior 'realisations CASCADE' claim was phantom. \
             Worse than growth: a swept realisation that still has dep rows \
             aborts delete_swept_path's batch — the CA-resolve owner's recorded \
             debt (sweep order or an explicit dep delete is owed)",
        ),
    ),
    (
        "realisations",
        RetentionPolicy::SweptBy {
            symbol: "delete_swept_path",
            note: "explicit delete (no FK to narinfo); realisations_output_idx \
                   serves the subselect",
        },
    ),
    (
        "scheduler_live_pins",
        RetentionPolicy::SweptBy {
            symbol: "sweep_stale_live_pins",
            note: "build_input staleness sweep; unpin_live_inputs (RPC) and \
                   release_materialization_pins_for_resolved_jobs (093 key) \
                   share the file",
        },
    ),
    (
        "sla_config_epoch",
        RetentionPolicy::KeepForever("singleton epoch row; bounded by config pushes"),
    ),
    (
        "sla_ema_state",
        RetentionPolicy::KeepForever(
            "one row per (hw_class, drv family) EMA cell — cardinality bounded by the catalog, \
             the state IS the model",
        ),
    ),
    (
        "sla_observed_instance_types",
        RetentionPolicy::KeepForever(
            "one row per observed instance type — bounded by the cloud catalog",
        ),
    ),
    (
        "sla_overrides",
        RetentionPolicy::KeepForever("operator-written rows; deleted by operators only"),
    ),
    (
        "tenant_keys",
        RetentionPolicy::CascadeFrom {
            parent: "tenants",
            migration: "017_tenant_keys_fk_cascade.sql",
            note: "keys are tenant-owned rows (the 012 path_tenants pattern); \
                   rotation supersedes in place — the prior 'deleted on \
                   rotation' claim named no deleting code",
        },
    ),
    (
        "tenant_upstreams",
        RetentionPolicy::KeepForever("operator-configured upstreams; deleted by admin RPCs only"),
    ),
    (
        "tenants",
        RetentionPolicy::KeepForever("operator-managed tenant set; deleted by admin RPCs only"),
    ),
];
