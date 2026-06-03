//! The table-retention registry (bughunt wave D1, merged_bug_163).
//!
//! Every PUBLIC table must declare its row lifecycle: either a named
//! sweeper/reaper deletes its rows, or a written rationale says why
//! rows live forever. "New table, no lifecycle decision" fails CI —
//! `tests/retention.rs` diffs `pg_tables` against this registry, so a
//! migration that creates a table without a registry row (or removes
//! a table without removing its row) is caught at merge time.
//!
//! This is the structural close of the class merged_bug_163 found:
//! migration 078 shipped `materialization_jobs` and
//! `build_wanted_outputs` with no deletion lifecycle at all — both
//! grew forever (resolved jobs and dead builds' wanted rows were
//! never deleted by anything).

/// How a table's rows leave the table.
#[derive(Debug, Clone, Copy)]
pub enum RetentionPolicy {
    /// Rows are deleted by the named sweeper/reaper (a function or
    /// tick arm; the name is documentation, greppable to the code).
    SweptBy(&'static str),
    /// Rows deliberately live forever; the rationale says why that is
    /// sound (bounded cardinality, append-only audit value, singleton).
    KeepForever(&'static str),
}

/// Every public table and its lifecycle. Keep ALPHABETICAL — the test
/// output diff is then stable.
// r[impl sched.db.table-retention]
pub const RETENTION_REGISTRY: &[(&str, RetentionPolicy)] = &[
    (
        "assignments",
        RetentionPolicy::SweptBy("rio-scheduler tick_sweep_dispatched_cells + attempt close paths"),
    ),
    (
        "build_derivations",
        RetentionPolicy::SweptBy("rio-scheduler delete_build (CASCADE family)"),
    ),
    (
        "build_samples",
        RetentionPolicy::SweptBy("rio-scheduler SLA sample TTL sweep"),
    ),
    (
        "build_wanted_outputs",
        RetentionPolicy::SweptBy(
            "rio-scheduler gc_dead_build_wanted_outputs (tick_gc_build_wanted_outputs)",
        ),
    ),
    (
        "builds",
        RetentionPolicy::SweptBy("rio-scheduler delete_build / build retention"),
    ),
    (
        "chunks",
        RetentionPolicy::SweptBy(
            "rio-store collect cycle soft-delete + post-pass tombstone reap (091)",
        ),
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
        RetentionPolicy::SweptBy("rio-scheduler delete_build closure cleanup"),
    ),
    (
        "derivations",
        RetentionPolicy::SweptBy("rio-scheduler derivation retention (terminal-status sweeps)"),
    ),
    (
        "drv_attempts",
        RetentionPolicy::SweptBy(
            "rio-scheduler tick_gc_attempt_ledger (the TG sweep, kernel-proven cut)",
        ),
    ),
    (
        "drv_executions",
        RetentionPolicy::SweptBy("rio-store sweep_expired_logs (log retention)"),
    ),
    (
        "drv_log_chunks",
        RetentionPolicy::SweptBy("rio-store sweep_expired_logs (log retention)"),
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
        RetentionPolicy::SweptBy(
            "operator ResetSlaModel TRUNCATE (rio-scheduler sla/mod.rs); reads are \
             bounded to a 7-day window (sla/hw.rs) — no age sweep exists today, the \
             SLA owner's recorded debt",
        ),
    ),
    (
        "interrupt_samples",
        RetentionPolicy::SweptBy(
            "rio-scheduler sweep_interrupt_samples (sla/cost.rs retention window)",
        ),
    ),
    (
        "jwt_revoked",
        RetentionPolicy::SweptBy(
            "rio-auth revocation TTL sweep (rows expire with the token lifetime)",
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
        RetentionPolicy::SweptBy("rio-store sweep_expired_logs (log retention)"),
    ),
    (
        "manifest_data",
        RetentionPolicy::SweptBy("rio-store path sweep (DELETE CASCADE with manifests)"),
    ),
    (
        "manifests",
        RetentionPolicy::SweptBy("rio-store path sweep + orphan reaper"),
    ),
    (
        "materialization_jobs",
        RetentionPolicy::SweptBy(
            "rio-scheduler gc_resolved_materialization_jobs (tick_gc_materialization_jobs)",
        ),
    ),
    (
        "narinfo",
        RetentionPolicy::SweptBy("rio-store path sweep (delete_swept_path)"),
    ),
    (
        "nodeclaim_cell_state",
        RetentionPolicy::SweptBy(
            "rio-controller nodeclaim reconciler (cell rows follow node lifecycle)",
        ),
    ),
    (
        "path_tenants",
        RetentionPolicy::SweptBy("rio-store path sweep (delete_swept_path)"),
    ),
    (
        "pending_s3_deletes",
        RetentionPolicy::SweptBy(
            "rio-store drain task (delete on S3 success / max-attempts alert)",
        ),
    ),
    (
        "realisation_deps",
        RetentionPolicy::SweptBy("rio-store path sweep (realisations CASCADE)"),
    ),
    (
        "realisations",
        RetentionPolicy::SweptBy("rio-store path sweep (delete_swept_path)"),
    ),
    (
        "scheduler_live_pins",
        RetentionPolicy::SweptBy(
            "rio-scheduler unpin/sweep_stale_live_pins (build_input) + \
             release_materialization_pins_for_resolved_jobs (materialization, 093 key)",
        ),
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
        RetentionPolicy::SweptBy("rio-auth key rotation (superseded keys deleted on rotation)"),
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
