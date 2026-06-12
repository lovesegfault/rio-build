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
    ///
    /// Since bug_095 this is a CHECKED NEGATIVE CLAIM, not an
    /// exemption: the retention-truth lint asserts (a) NO cascading
    /// FK on the table survives the migration corpus (a cascade is a
    /// live deletion vector regardless of what the registry says —
    /// gc_holds shipped exactly that contradiction CI-green), and
    /// (b) workspace `DELETE FROM` statements match the declared
    /// [`KeepForeverDeleter`] exactly.
    KeepForever(&'static str, KeepForeverDeleter),
}

/// How a KeepForever table's rows may nonetheless leave the database
/// — the checked-negative-claim taxonomy (bug_095). The lint
/// RESOLVES each variant; none is exempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepForeverDeleter {
    /// NO deletion vector exists: no cascading FK targeting the
    /// table's rows, no production `DELETE FROM` anywhere in the
    /// workspace. Audit/evidence/singleton rows.
    None,
    /// Deleted ONLY inside the named production functions (admin
    /// RPCs / operator verbs): every production `DELETE FROM` hit
    /// must sit inside a listed fn's body; still no cascading FK.
    AdminRpc(&'static [&'static str]),
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
            KeepForeverDeleter::AdminRpc(&["delete_build"]),
        ),
    ),
    (
        "chunk_tenants",
        RetentionPolicy::CascadeFrom {
            parent: "chunks",
            migration: "116_drv_blobs_chunk_tenants.sql",
            note: "tenant-attribution junction; dies with the chunk row when \
                   collect_cycle reaps it (the tenants-side CASCADE is for \
                   tenant deletion only)",
        },
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
            KeepForeverDeleter::None,
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
        "directories",
        RetentionPolicy::SweptBy {
            symbol: "decrement_directory_refs",
            note: "ADR-022 castore: refcount-managed (NOT the dropped chunks.refcount). \
                   sweep_one_batch decrements per swept path's nar_index; \
                   refcount=0 rows tombstoned for the drain. KNOWN GAP: not yet \
                   enrolled in gc::hold::BatchAuthority (reconciliation TODO).",
        },
    ),
    (
        "directory_paths",
        RetentionPolicy::CascadeFrom {
            parent: "manifests",
            migration: "112_directory_paths.sql",
            note: "ADR-022 castore: dies with its store path's manifests row \
                   (delete_swept_path); the directories-side FK CASCADE is \
                   secondary (a directory only goes when no path references it)",
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
        "drv_blob_tenants",
        RetentionPolicy::CascadeFrom {
            parent: "drv_blobs",
            migration: "116_drv_blobs_chunk_tenants.sql",
            note: "tenant-attribution junction; dies with the drv_blobs row \
                   under sweep_unpinned_drv_blobs",
        },
    ),
    (
        "drv_blobs",
        RetentionPolicy::SweptBy {
            symbol: "sweep_unpinned_drv_blobs",
            note: "PG-only sweep: a drv blob with no live build pin and past \
                   the unpinned grace age deletes in one statement (no S3 \
                   object — bodies are inline BYTEA)",
        },
    ),
    (
        "drv_executions",
        RetentionPolicy::SweptBy {
            symbol: "gc_exec_rows",
            note: "the scheduler's execution-row GC — store.log.sweep-ownership+2 \
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
                   (merged_bug_145): rows older than the credential-derived \
                   horizon CONFIRM_FENCE_GC_SECS (MAX_HMAC_LIFETIME_SECS + \
                   slack — the fence outlives every token the family signer \
                   can mint); one row per confirm-exited pod, so volume \
                   tracks pod churn",
        },
    ),
    (
        "file_blobs",
        RetentionPolicy::CascadeFrom {
            parent: "manifests",
            migration: "110_nar_index.sql",
            note: "ADR-022 castore: per-file (digest, store_path_hash, nar_offset) \
                   binding; dies with its store path's manifests row (delete_swept_path)",
        },
    ),
    (
        "gc_collect_state",
        RetentionPolicy::KeepForever(
            "singleton row (CHECK (singleton)); the collector's durable state",
            KeepForeverDeleter::None,
        ),
    ),
    (
        "gc_holds",
        RetentionPolicy::KeepForever(
            "round-9 WO-S1-4: operator-set GC holds; released_at closes a hold \
             without deleting it — the hold history is audit evidence by design \
             (rows are operator-created, bounded by operator action)",
            KeepForeverDeleter::None,
        ),
    ),
    (
        "hw_cost_factors",
        RetentionPolicy::KeepForever(
            "dead on arrival: ADR-023 chose sla_ema_state for the EMA persist; rows are \
             never written (xtask schema_liveness ALLOW_DEAD names it); DROP TABLE \
             deferred to a future migration because 042 is checksum-frozen",
            KeepForeverDeleter::None,
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
            KeepForeverDeleter::None,
        ),
    ),
    (
        "leader_generation_claims",
        RetentionPolicy::KeepForever(
            "one row per leader generation; the claims-floor fence reads MAX(generation) — \
             cardinality bounded by elections, the floor's history is the safety artifact",
            KeepForeverDeleter::None,
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
        "nar_index",
        RetentionPolicy::CascadeFrom {
            parent: "manifests",
            migration: "110_nar_index.sql",
            note: "ADR-022 castore: per-path Directory-DAG index; dies with its \
                   store path's manifests row (delete_swept_path reads dir digests \
                   from it BEFORE the cascade for decrement_directory_refs)",
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
            KeepForeverDeleter::None,
        ),
    ),
    (
        "path_tenant_tombstones",
        RetentionPolicy::KeepForever(
            "round-9 WO-S1-4 (evidence-outlives-bytes, signed Q3): append-only \
             registration audit records minted at sweep — outliving the bytes \
             is their purpose; growth is bounded by sweep volume and an \
             operator truncation is a deliberate audit-disposal act",
            KeepForeverDeleter::None,
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
            symbol: "drain_one_batch",
            note: "drain task: batch DeleteObjects on S3 success / max-attempts alert",
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
            KeepForeverDeleter::None,
        ),
    ),
    (
        "realisation_tombstones",
        RetentionPolicy::KeepForever(
            "round-9 WO-S1-4: append-only identity audit records minted at \
             sweep (the realisation rows' tombstones) — same disposal posture \
             as path_tenant_tombstones",
            KeepForeverDeleter::None,
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
        RetentionPolicy::KeepForever(
            "singleton epoch row; bounded by config pushes",
            KeepForeverDeleter::None,
        ),
    ),
    (
        "sla_ema_state",
        RetentionPolicy::KeepForever(
            "one row per (hw_class, drv family) EMA cell — cardinality bounded by the catalog, \
             the state IS the model; the reference-epoch reseed \
             (check_reference_epoch) resets cluster-scoped cells",
            KeepForeverDeleter::AdminRpc(&["check_reference_epoch"]),
        ),
    ),
    (
        "sla_observed_instance_types",
        RetentionPolicy::KeepForever(
            "one row per observed instance type — bounded by the cloud catalog",
            KeepForeverDeleter::None,
        ),
    ),
    (
        "sla_overrides",
        RetentionPolicy::KeepForever(
            "operator-written rows; removed by the delete_sla_override admin verb only",
            KeepForeverDeleter::AdminRpc(&["delete_sla_override"]),
        ),
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
        RetentionPolicy::CascadeFrom {
            parent: "tenants",
            migration: "026_tenant_upstreams.sql",
            note: "config rows (not audit evidence) die with their tenant; \
                   the RemoveUpstream admin verb (upstreams::delete) removes \
                   individual rows. The prior KeepForever claim was \
                   schema-false — bug_095's checked negative claim caught \
                   the 026 CASCADE on its first run",
        },
    ),
    (
        "tenants",
        RetentionPolicy::KeepForever(
            "operator-managed tenant set; deleted by the delete_tenant admin \
             RPC only (bug_095: gc_holds RESTRICT additionally refuses while \
             ANY hold rows reference the tenant)",
            KeepForeverDeleter::AdminRpc(&["delete_tenant"]),
        ),
    ),
];
