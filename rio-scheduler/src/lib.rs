//! DAG-aware build scheduler for rio-build.
//!
//! Receives derivation build requests, analyzes the DAG, and publishes
//! work to workers via a bidirectional streaming RPC.
//!
//! ## Architecture
//!
//! The scheduler uses a single-owner actor model. All mutable state is owned
//! by a single Tokio task (the DAG actor) that processes commands from a
//! bounded mpsc channel. gRPC handlers send commands and await responses.
//!
//! ## Modules
//!
//! - [`actor`]: DAG actor (single-owner event loop, dispatch)
//! - `dag`: In-memory derivation graph
//! - [`state`]: Derivation and build state machines
//! - [`db`]: PostgreSQL persistence (sqlx)
//! - [`grpc`]: SchedulerService + ExecutorService gRPC implementations

pub mod actor;
pub mod admin;
pub(crate) mod assignment;
pub(crate) mod ca;
pub mod config;
pub(crate) mod critical_path;
pub(crate) mod dag;
pub mod db;
pub(crate) mod domain;
pub mod grpc;
pub mod guard;
/// Re-export so existing `crate::lease::{LeaderState, LeaseConfig,
/// run_lease_loop}` paths keep working after the B1 extraction.
pub use rio_lease as lease;
pub mod lease_hooks;
pub mod observability;
// The Phase-0 reference fold over a derivation's failure history — the
// executable specification the retryPolicy model's CountersRefineHistory
// invariant compares the live RetryState counters against. Dead code by
// design until Phase 1 collapses the nine failure entry points onto
// decide(); see the module doc for why it is wired (clippy/rustfmt/tests)
// but unreferenced.
#[allow(dead_code)]
pub(crate) mod retry_policy;
pub mod sla;
pub mod state;

// Re-exports for PoisonConfig + RetryPolicy: main.rs's `Config`
// struct embeds them as `#[serde(default)]` sub-tables. `state` IS
// pub, but the re-export keeps main.rs's imports uniform
// (crate-root path, no deep-module reach-in).
pub use state::{PoisonConfig, RetryPolicy};

/// Re-export of the shared embedded migrator from `rio-migrations`.
/// Test-only (`TestDb::new(&MIGRATOR)`) — production migrates via
/// `rio-store migrate` (helm rio-migrate Job / NixOS oneshot);
/// `main.rs` only verifies with
/// `rio_migrations::migrate::assert_current`. Same migration set as
/// rio-store; both consume the single `rio-migrations` source of truth.
#[cfg(test)]
pub use rio_migrations::MIGRATOR;

/// Histogram bucket boundaries for `rio_scheduler_critical_path_accuracy`.
///
/// Ratio of actual/estimated build duration. `1.0` = perfect prediction,
/// values above `1.0` = underestimate (build took longer than predicted).
/// Bucket edges are chosen to give resolution around `1.0` and capture
/// long tails on both sides.
const CRITICAL_PATH_ACCURACY_BUCKETS: &[f64] = &[0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 5.0];

/// Histogram bucket boundaries for `rio_scheduler_build_graph_edges`.
///
/// Edge COUNT (not seconds) per GetBuildGraph response. Range is 0..~20K
/// (induced subgraph over the 5000-node cap at realistic 4× edge density).
/// Default Prometheus buckets `[0.005..10.0]` are useless here — every
/// sample lands in `+Inf`. These match the suggested buckets in
/// observability.typ's Histogram Buckets table.
const GRAPH_EDGES_BUCKETS: &[f64] = &[100.0, 500.0, 1000.0, 5000.0, 10000.0, 20000.0];

/// Histogram bucket boundaries for `rio_scheduler_merge_phase_seconds`.
/// Spans sub-ms (in-mem phases) → minute (PG/store-RPC phases) so a
/// per-phase regression that pushes MergeDag past the 1s actor-stall
/// warn is visible without `RUST_LOG=debug`.
const MERGE_PHASE_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Histogram bucket boundaries for `rio_scheduler_attempt_requeue_seconds`
/// (the OA1 interval-(ii) instrument). Spans the worker-report cause
/// (same actor turn — tens of ms, dominated by the appending PG
/// transaction), the pod-terminal cause (the controller's classifying
/// report typically lands one or a few reconcile ticks after the
/// disconnect), and the establishment cause (the TTL sweep fires at
/// `TERMINATION_REPORT_TTL` after the disconnect), with headroom for
/// controller-outage tails.
const ATTEMPT_REQUEUE_BUCKETS: &[f64] = &[
    0.05, 0.25, 1.0, 5.0, 15.0, 30.0, 60.0, 90.0, 120.0, 300.0, 600.0,
];

/// Bucket boundaries for `rio_scheduler_tick_phase_seconds`. Same
/// span rationale as `MERGE_PHASE_BUCKETS` (sub-ms in-memory phases →
/// PG/store-RPC phases) but the top extends to 300s: live_053's Tick
/// ran 134.65s with its first ~118s unattributed, and the whole point
/// of the per-phase decomposition is to resolve that tail, not fold
/// it into +Inf.
const TICK_PHASE_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0,
];

/// Bucket boundaries for `rio_scheduler_spawn_intents_response_bytes`
/// (BYTES, not seconds — prost `encoded_len` of one GetSpawnIntents
/// response). Powers-of-~4 from an idle answer (~hundreds of bytes)
/// through the 4 MiB tonic default message cap up to the modeled
/// 150K-ready tail (~150K × 150-400 B/intent ≈ 22-60 MiB). The whole
/// point of the instrument is to OBSERVE that derived tail before the
/// pagination constants are picked, so the top buckets must resolve
/// it rather than fold it into +Inf.
const SPAWN_INTENTS_RESPONSE_BYTES_BUCKETS: &[f64] = &[
    1024.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0, 16777216.0, 67108864.0,
];

/// Bucket boundaries for `rio_scheduler_spawn_intents_per_response`
/// (COUNT of intents serialized into one answer). Edges cover the
/// in-tree window vocabulary a pagination design would draw from
/// (ListMaterializationJobs 256/512, DISPATCH_PROBE_TICK_QUOTA 2048,
/// GetBuildGraph 5000) plus the unpaginated completion-cascade tail.
const SPAWN_INTENTS_PER_RESPONSE_BUCKETS: &[f64] =
    &[1.0, 8.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0];

/// Bucket boundaries for `rio_scheduler_pull_outcome_flush_batch_size`
/// (COUNT of reports per flush). Edges land on the sh-027 §3 design
/// vocabulary: 1/2/5 (the retired mailbox-empty trigger's measured
/// N̄≈5.5 degradation), 20 (the design target), 64
/// (`REPORT_OUTCOME_BATCH_MAX`).
const PULL_OUTCOME_FLUSH_BATCH_SIZE_BUCKETS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 32.0, 64.0];

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`. Every
/// `describe_histogram!` in this crate must have an entry here OR be in
/// the `DEFAULT_BUCKETS_OK` exemption list (`tests/metrics_registered.rs`);
/// histograms not listed fall through to the global `[0.005..10.0]` default.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    (
        "rio_scheduler_build_duration_seconds",
        rio_common::observability::BUILD_DURATION_BUCKETS,
    ),
    (
        "rio_scheduler_critical_path_accuracy",
        CRITICAL_PATH_ACCURACY_BUCKETS,
    ),
    ("rio_scheduler_merge_phase_seconds", MERGE_PHASE_BUCKETS),
    ("rio_scheduler_tick_phase_seconds", TICK_PHASE_BUCKETS),
    ("rio_scheduler_build_graph_edges", GRAPH_EDGES_BUCKETS),
    (
        "rio_scheduler_attempt_requeue_seconds",
        ATTEMPT_REQUEUE_BUCKETS,
    ),
    (
        "rio_scheduler_spawn_intents_response_bytes",
        SPAWN_INTENTS_RESPONSE_BYTES_BUCKETS,
    ),
    (
        "rio_scheduler_spawn_intents_per_response",
        SPAWN_INTENTS_PER_RESPONSE_BUCKETS,
    ),
    (
        "rio_scheduler_pull_outcome_flush_batch_size",
        PULL_OUTCOME_FLUSH_BATCH_SIZE_BUCKETS,
    ),
];

/// Registers prometheus metric descriptions. The help strings here are
/// the source for `docs/ref/metrics.typ` — see
/// `xtask/src/regen/docs_data.rs::metrics()` for the data-flow.
// r[impl obs.metric.scheduler+2]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    // Shared rio_pg_iam_* family (rio-common emits; each PG consumer
    // registers — registration and emission are separate call sites,
    // and rio-common has no exporter of its own).
    rio_common::pg_iam::describe_metrics();

    describe_counter!(
        "rio_scheduler_builds_total",
        "Total builds at terminal state (labeled by outcome: success/failure/cancelled)"
    );
    describe_gauge!("rio_scheduler_builds_active", "Currently active builds");
    describe_gauge!(
        "rio_scheduler_status_outbox_depth",
        "Failed terminal-status persist batches awaiting the housekeeping tick's \
         re-drive. Nonzero means PG persists are failing; a sustained nonzero \
         depth means cancelled derivations' attempts stay open (their close \
         rides the persist) until the flush succeeds."
    );
    describe_gauge!(
        "rio_scheduler_derivations_queued",
        "Ready derivations waiting for a worker (DAG-status count; the per-system \
         split is queued_by_system on ClusterStatus/GetSpawnIntents)"
    );
    describe_gauge!(
        "rio_scheduler_derivations_running",
        "Derivations currently building"
    );
    describe_histogram!(
        "rio_scheduler_pull_outcome_flush_batch_size",
        "Count of ReportPullOutcome commands drained per \
         flush_pending_pull_outcomes pass (sh-027 §3: the prod N̄ \
         signal — design target ≥20; the retired mailbox-empty \
         trigger measured N̄≈5.5). The sh-007c S6 O(1)-PG-per-flush \
         amortization scales with this; a sustained le=5 mass means \
         the 250ms deadline window is not coalescing."
    );
    describe_histogram!(
        "rio_scheduler_merge_phase_seconds",
        "Per-phase MergeDag latency (labeled by phase: 0-topdown-roots..6f). \
         Decomposes rio_scheduler_actor_cmd_seconds{cmd=MergeDag}. A single \
         phase >1s is the I-139 signal — N sequential PG awaits in the actor."
    );
    describe_histogram!(
        "rio_scheduler_tick_phase_seconds",
        "Per-phase housekeeping Tick latency (labeled by phase: \
         00-priority-sweep..18-snapshot-publish). Decomposes \
         rio_scheduler_actor_cmd_seconds{cmd=Tick} the way \
         merge_phase_seconds decomposes MergeDag: the Tick is a leader-only \
         single-threaded actor turn, so one slow phase head-of-line blocks \
         every queued RPC and starves admin-served probes — a phase in the \
         tens-of-seconds buckets names the term to bound, instead of a \
         log-silent stall."
    );
    describe_histogram!(
        "rio_scheduler_actor_cmd_seconds",
        "Per-ActorCommand handling latency (labeled by cmd variant); \
         the actor is single-threaded so a slow command head-of-line \
         blocks every queued RPC"
    );
    describe_gauge!(
        "rio_scheduler_backpressure_projected_drain_seconds",
        "Projected mailbox drain time: queue depth × per-turn work-cost \
         EWMA, refreshed at every dequeue. The cost axis of the \
         backpressure law (round-9 B6): the flag engages when this \
         reaches 30s (the submit-side caller deadline class) even at \
         low depth — the live_053 inversion was 140s-class turns at \
         1–12.8% depth with silent depth watermarks. Watch it approach \
         the budget; sustained high values at low mailbox depth mean \
         individual commands are too expensive (pair with \
         rio_scheduler_actor_cmd_seconds to name them)."
    );
    describe_histogram!(
        "rio_scheduler_actor_admin_fast_delivery_seconds",
        "Fast-lane admin DELIVERY latency (enqueue to handler start, \
         labeled by cmd variant: MintExecutorTokens and the SLA reads). \
         The B8 SLO surface: fast-lane queries are served between \
         mailbox commands and at every Tick phase boundary, so delivery \
         is bounded by the largest indivisible actor work slice — \
         samples at/over the 5s bucket mean a single phase or command \
         exceeds the controller's admin deadline and spawn-path mints \
         are at risk again (the live_053 starvation shape)."
    );
    describe_histogram!(
        "rio_scheduler_spawn_intents_response_bytes",
        "Encoded (prost wire) size of one GetSpawnIntents response in bytes. \
         Records the POST-window response (the served page, not the mint): \
         with the round-9 priority-head window landed this is the page-size \
         signal consumer tuning reads (pair with \
         rio_scheduler_spawn_intents_per_response for bytes-per-intent; the \
         unbounded-demand signal is queued_by_system). Sustained samples in \
         the MiB buckets mean pollers run unwindowed (limit=0 legacy reads) \
         and poller traffic alone is taxing the actor."
    );
    describe_histogram!(
        "rio_scheduler_spawn_intents_per_response",
        "Intents serialized into one GetSpawnIntents response (count, not \
         seconds). The companion of \
         rio_scheduler_spawn_intents_response_bytes: divide the two to get \
         observed bytes-per-intent; watch the top buckets to see \
         completion-cascade fan-out reaching the pollers unwindowed."
    );
    describe_histogram!(
        "rio_scheduler_build_duration_seconds",
        "Total build duration"
    );
    describe_counter!(
        "rio_scheduler_cache_hits_total",
        "Derivations served from cache (labeled by source: scheduler/reprobe/existing/dispatch)"
    );
    describe_counter!(
        "rio_scheduler_cache_hit_deferred_total",
        "Cache-hit derivations deferred to Queued because their inputDrvs are not yet Completed"
    );
    describe_counter!(
        "rio_scheduler_cache_check_failures_total",
        "Scheduler cache check (store FindMissingPaths) failures; alert if rate > 0 sustained"
    );
    describe_counter!(
        "rio_scheduler_derivations_gc_deleted_total",
        "Orphan-terminal derivations rows deleted by the periodic Tick sweep (I-169.2)"
    );
    describe_counter!(
        "rio_scheduler_store_degraded_requeues_total",
        "Worker-flagged store-degraded infrastructure reports by gated disposition \
         (merged_bug_032): paced = corroborated + inside the kernel free run \
         (uncharged requeue); uncorroborated = single-node flag charged as plain \
         infra; run_bound = corroborated but past STORE_DEGRADED_FREE_RUN, charged \
         fallthrough into the counted infra budget"
    );
    describe_counter!(
        "rio_scheduler_attempts_gc_deleted_total",
        "drv_attempts rows deleted by the periodic attempt-ledger retention sweep \
         (sched.db.attempts-gc): suffix-complement rows past the retention horizon plus \
         orphaned histories. The decision suffix is provably unchanged by every increment"
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_gc_total",
        "Resolved materialization_jobs rows deleted by the periodic retention sweep \
         (D1/A6, merged_bug_163): past the forensic horizon, unpinned, interest-free. \
         Pending jobs are never deleted"
    );
    describe_counter!(
        "rio_scheduler_wanted_outputs_gc_total",
        "build_wanted_outputs rows deleted by the periodic retention sweep (D1/A6, \
         merged_bug_163): builds long-terminal, plus orphan rows whose build is gone"
    );
    describe_counter!(
        "rio_scheduler_exec_rows_gc_deleted_total",
        "drv_executions lifecycle rows deleted by the execution-row GC \
         (store.log.sweep-ownership second deleter): terminal, no active \
         assignment, referenced by no drv_attempts row, no surviving \
         drv_log_chunks rows, no LIVE log_ingest_sessions row, and older \
         than exec_retention_days — the SQL twin of \
         rio_retry_kernel::exec_row_sweep_eligible. The store's log TTL \
         sweep never deletes these."
    );
    describe_counter!(
        "rio_scheduler_confirm_fences_gc_deleted_total",
        "executor_confirm_fences rows deleted by the housekeeping TTL rider \
         (merged_bug_145): confirm-exit fence rows older than the \
         credential-derived horizon CONFIRM_FENCE_GC_SECS \
         (MAX_HMAC_LIFETIME_SECS + slack — the fence outlives every token \
         the family signer can mint, asserted at its const)"
    );
    describe_counter!(
        "rio_scheduler_resource_floor_bumps_total",
        "resource_floor observations on worker-reported attempt closes (D4, \
         labeled reason=cgroup_oom|disk_full|timeout|executor_variant|worker_abort|\
         witnessed_oom|witnessed_disk|other; headroom=hard|soft). sh-041u: \
         every non-success close observes peaks on EVERY axis the report \
         carries — the close reason only BOOSTS headroom on its own axis (2.0× \
         hard, else 1.2× soft). cgroup_oom and disk_full consume the TYPED \
         failure_classification wire field; the per-axis trust band (peak vs \
         the dispatch-assigned shape — bug_090; worker free text drives \
         nothing; refusals counted on \
         rio_scheduler_uncorroborated_sizing_claim_total) gates the hard arm. \
         witnessed_oom / witnessed_disk are the controller-witnessed \
         per-container kubelet attributions promoted at the establishment \
         sweep, once per attempt ever via the establishment transaction's won \
         flag (live_058-b; node-condition EvictedDiskPressure stays \
         classify-only and has NO label here). worker_abort is the AD5 \
         SIGTERM-abort short-circuit (sh-041 — a compute-bound build \
         interrupted by spot reclaim now jumps cores). headroom=hard ⇒ the \
         M_044 persist (hard_grew); promotion-exempt only when ALSO not at \
         cap (hard_promoted — a grow-to-cap clip persists but does NOT ride \
         exemption). headroom=soft ⇒ in-memory only. \
         The caller alphabet is census-pinned \
         (observe_resource_floor_caller_census, db/live_pins.rs). \
         Reactive upsize: a derivation that OOMs at peak=N retries at mem≈2N."
    );
    describe_counter!(
        "rio_scheduler_infra_requeues_total",
        "Infrastructure-failure requeues by charge disposition \
         (live059-d; labels: charge = counted|exempt). `counted` \
         increments exactly when the fold charged `infra_count` for \
         the event (the consecutive streak toward \
         max_infra_retries-poison); `exempt` exactly when it charged \
         `exempt_infra_count` (CONCURRENT_PUTPATH / floor-promoted — \
         uncharged on the counted budget by design). The live_059 \
         carousel (520 requeues / 128 drvs / 23 min) was INFO-log \
         silent; a sustained per-drv `counted` rate IS the carousel \
         signature — alert on rate (ops wiring post-wave) and expect \
         poison-at-10 events to follow under the consecutive-streak \
         law."
    );
    describe_counter!(
        "rio_scheduler_uncorroborated_sizing_claim_total",
        "Worker-reported peaks refused by the per-axis trust band (bug_090; \
         bug_102 the status-borne timeout axis; labels: class = \
         mem|disk|cgroup_oom|disk_full|timed_out): the peak was outside the \
         band the scheduler-assigned shape admits (mem: peak above \
         TRUST_BAND_MEM × assigned — physically impossible under memory.max; \
         disk: peak above overlay(assigned, H_MAX) + block slack — \
         kubelet-unmintable; cgroup_oom/disk_full: a hard-event CLAIM with \
         peak below half the assigned shape — implausible, degraded to soft; \
         timed_out: attempt-open duration below half the assigned deadline, \
         or no running_since/intent anchor). Refusals are classify-only — the \
         report's retry/charge flow is unaffected; persisted floors never \
         move on a forged-HIGH peak and never hard-promote on a forged-LOW \
         hard claim (the trust gate sits inside observe_peaks; the caller \
         alphabet stays census-pinned, observe_resource_floor_caller_census). \
         Alert if rate > 0: a forged or misbehaving worker report."
    );
    describe_counter!(
        "rio_scheduler_timeout_cores_suppressed_total",
        "sh-045: a Timeout close whose deadline arm DID promote (last_deadline \
         > 0 ∧ wall ≥ assigned/2) AND carried cpu_util ≥ compute_bound_threshold \
         ∧ wall ≥ min_wall — the cores-arm gate WOULD have fired had (Timeout, \
         Cores) been hard. Timeout is NOT cores-hard (cpu_util cannot \
         discriminate serial-saturated from parallel-saturated; the cores arm \
         jumps to prov_max so a wrong promotion costs prov_max× capacity); \
         this counter measures whether parallel-starved Timeout actually \
         occurs in production before any future (Timeout, Cores) → true policy \
         change once cpu.stat throttled_usec is gating. No labels."
    );
    describe_counter!(
        "rio_scheduler_poison_fleet_exhausted_total",
        "Derivations poisoned because failed_builders excluded every registered worker \
         of the matching kind (I-065). Nonzero rate with small fleet = poison threshold \
         unreachable; the build would otherwise defer forever."
    );
    describe_counter!(
        "rio_scheduler_stale_completed_reset_total",
        "Pre-existing Completed nodes reset to Ready at merge because output was \
         GC'd from store. Nonzero rate = GC retention shorter than DAG node lifetime."
    );
    describe_counter!(
        "rio_scheduler_stale_completed_substituted_total",
        "Pre-existing Completed nodes whose GC'd output was repopulated via upstream \
         substitution at merge (instead of reset-to-Ready re-dispatch)."
    );
    describe_counter!(
        "rio_scheduler_stale_realisation_filtered_total",
        "CA realisations dropped from cache-hit set because the realized path was \
         GC'd from store. Same operator signal as stale_completed_reset; this counts \
         the newly-inserted-CA path, that one counts the pre-existing-completed path."
    );
    describe_counter!(
        "rio_scheduler_topdown_prune_total",
        "Submissions pruned to roots-only by the top-down substitution pre-check"
    );
    describe_counter!(
        "rio_scheduler_topdown_substitute_fail_total",
        "Top-down-pruned roots that could not complete via substitution (a wanted output \
         was definitively missing/unsubstitutable at settlement) — build failed fast; \
         resubmit re-probes"
    );
    describe_counter!(
        "rio_scheduler_queue_backpressure",
        "Backpressure activations (queue reached 80% capacity)"
    );
    describe_counter!(
        "rio_scheduler_materialization_aged_out_total",
        "Materialization jobs resolved from-source by the phase-15 unclaimed age-out \
         arm (no holder, not currently parked, created_at past max_attempts × \
         attempt_deadline_secs) — the executor-liveness backstop that re-admits \
         the node to the phase-17 dispatch probe; discriminated by origin"
    );
    describe_gauge!(
        "rio_scheduler_open_attempts",
        "Open pull-mode BUILD attempts (active assignment + execution pair minted \
         by PullAssignment, no terminal classification yet) — one attempt per \
         builder pod. The busy-fleet gauge (successor of the retired stream-era \
         workers_active). Store materialization claims are counted by \
         rio_scheduler_open_materialization_attempts instead (A2.4)."
    );
    describe_gauge!(
        "rio_scheduler_open_materialization_attempts",
        "Open store materialization claims (kind=materialization attempts: active \
         assignment + execution pair, no terminal classification yet). Store-side \
         work — never a builder slot; the build lane is rio_scheduler_open_attempts."
    );
    describe_counter!(
        "rio_scheduler_assignments_total",
        "Total derivation-to-worker assignments"
    );
    describe_counter!(
        "rio_scheduler_input_closure_unattested_total",
        "Dispatches whose input closure was NOT attested, labeled by reason \
         (seeds_unknown/missing_narinfo/db_error/timeout). Under ADR-022 \
         closure-scoped FUSE these are infra-retry loops, not safe degrades \
         — the builder's own drv-parsed BFS may EIO reading through the \
         empty-scoped mount. A sustained nonzero rate means attestation is \
         effectively disabled; compare against rio_scheduler_assignments_total."
    );
    describe_counter!(
        "rio_scheduler_attested_seeds_pg_fallback_total",
        "attested_input_seeds resolutions of a DAG-missed inputDrv via the \
         persisted derivations.expected_output_paths row, labeled by outcome \
         (resolved/degraded_none). resolved > 0 is normal post-restart or \
         after reap; degraded_none means a genuinely-never-merged inputDrv \
         or a floating-CA placeholder."
    );
    describe_counter!(
        "rio_scheduler_cleanup_dropped_total",
        "Terminal-build cleanup commands dropped due to channel backpressure; alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_transition_rejected_total",
        "State-machine transition rejections (labeled by target state); alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_malformed_built_output_total",
        "Worker-supplied BuiltOutput.output_path that failed StorePath::parse \
         (dropped at handle_completion boundary); alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_undeclared_built_output_total",
        "Worker-supplied BuiltOutput.output_name not in derivation's output_names \
         (dropped at handle_completion membership filter); alert if rate > 0"
    );
    describe_counter!(
        "rio_scheduler_unexpected_built_output_total",
        "Worker-supplied BuiltOutput.output_path outside the assignment's \
         dispatch-minted expected set (refused before any path_tenants \
         registration stamp, on the admitted and late-register lanes; the \
         scheduler-side mirror of the store's PutPath path-in-claims check); \
         alert if rate > 0 — a nonzero rate is a forged or misbehaving worker \
         report"
    );
    describe_counter!(
        "rio_scheduler_unevidenced_ca_output_total",
        "Floating-CA BuiltOutput.output_path refused for missing \
         store-recorded production evidence (no upload, no stamp): the \
         reporting build's attributed cohort has no ingest-lane registration \
         for the path, so the tenant-visibility stamp and the realisation \
         row are withheld on the admitted and late-register lanes (the \
         membership law's CA face — the expected-set law cannot bound \
         floating-CA paths, which are computed post-build); alert if \
         rate > 0 — a forged report, or an upload whose best-effort ingest \
         stamp was skipped (store-side warn names the path)"
    );
    describe_counter!(
        "rio_scheduler_pull_rejected_total",
        "Pull-mode unaries rejected (labels: rpc = \
         pull_assignment|report_outcome|list_materialization_jobs|\
         report_materialization_progress, reason = unauthenticated|token_mismatch|\
         kind_unauthorized — a VERIFIED credential whose kind does not \
         authorize the payload class (an executor token on a materialization \
         surface, a non-store service caller, or an executor kind not allowed \
         to take this attempt kind); service_verification_failed — a presented \
         store-service token could not be verified (no service verifier \
         configured, or service-HMAC verification failed): a sustained \
         service_verification_failed rate is the store-fleet HMAC \
         rotation-skew trace; instance_unbound — a verified store credential \
         without the Phase-B instance binding (instance-less token or \
         claim/request instance mismatch); consumption_not_durable — a \
         required durable write did not land and the NACK rides UNAVAILABLE: \
         on rpc=pull_assignment the confirm-fence write-ahead failed and the \
         exit-0 license was withheld (the pod re-pulls or exits nonzero); on \
         rpc=report_outcome the report's consumption close failed to commit \
         and the store's report redelivery re-presents the SAME outcome. \
         Both are PG-brownout traces, neither an identity fault — alert on \
         the pair). \
         A sustained rate on the identity reasons means a pod fleet holds \
         mis-bound/expired executor tokens or a key rotation skew. The \
         historical mint-skip-race carve-out is retired (bug_121: the \
         controller skips mint-omitted intents instead of spawning them \
         token-less — see mint_executor_tokens), so unauthenticated has \
         no documented benign source. The rejected pods exit nonzero and \
         their logs are ephemeral, so this counter is the alertable trace."
    );
    describe_counter!(
        "rio_scheduler_executor_auth_rejected_total",
        "Executor-token credential rejections by detail (labels: rpc; \
         reason = absent — no executor credential on any carrier; \
         malformed — non-ASCII metadata or an undecodable token \
         (format/base64/json); bad_signature — the HMAC tag does not \
         verify (tampered bytes or a wrong/rotated key); expired — \
         signature valid, expiry claim past). Splits the executor-token \
         slice of pull_rejected_total{reason=unauthenticated} so an \
         identity episode reads as WHAT failed — a fleet holding expired \
         tokens vs a key-rotation mismatch vs token-less pods — instead \
         of one merged label; the wire status stays Unauthenticated for \
         every shape and the coarse row keeps counting. Counted at the \
         terminal rejection only: a metadata-carrier failure recovered \
         by a verifying body token does not count."
    );
    describe_counter!(
        "rio_scheduler_pull_establishments_total",
        "Open pull-mode attempts established as unreported executor crashes \
         by the establishment sweep (the C2 charge arm; the store-probe adopt \
         arm does not count, and the probe-unavailable DEFER arm has its own \
         counter — establish_deferred_total). Feeds the OA2 interim hung-node \
         tripwire — the attempt_requeue histogram's establishment cause is \
         shared with the stream-mode correlation-TTL sweep and is unsuitable \
         for that alert."
    );
    describe_counter!(
        "rio_scheduler_establish_deferred_total",
        "Expired open pull-mode attempts the establishment sweep DEFERRED \
         because the store probe was unavailable (no evidence either way — \
         the attempt stays open for a pass with a working probe). The defer \
         is uncharged and unbounded by design, so this counter and the \
         defer-age gauge are the wedge's only observability: the \
         establishment-cluster tripwire counts CHARGES and stays silent \
         through a probe outage."
    );
    describe_gauge!(
        "rio_scheduler_establish_defer_age_seconds",
        "Oldest currently-deferred expired pull attempt's seconds past its \
         establishment window (0 when no attempt is deferred). Recomputed \
         every sweep pass; leader-owned. Rises monotonically while a \
         probe-unavailable wedge persists — the \
         RioSchedulerEstablishDeferPersistent alert's clock is this age, \
         never the charge rate (the opposite polarity)."
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_created_total",
        "Materialization jobs created (substitution-replacement campaign), \
         labeled by origin (pruned|cache_opportunity|stale_reset|reprobe). \
         Dedup-found existing jobs do not count; origin upgrades are counted \
         by ..._origin_upgraded_total."
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_origin_upgraded_total",
        "Pruned-wins origin upgrades on the creation dedup arm (PD-D1): a \
         topdown prune landed on a node with an existing unresolved \
         non-pruned job and upgraded that row's origin to 'pruned' in the \
         same transaction. Not a creation (jobs_created_total is untouched); \
         psql origin distributions diverge from creation-rate dashboards by \
         exactly this counter."
    );
    describe_counter!(
        "rio_scheduler_wanted_width_saturated_total",
        "The conservative-absent wanted-width arm fired (DQ-2): a live \
         interested build's wanted contributions could not be resolved \
         (no in-memory cache entry / no build_wanted_outputs rows — the \
         legacy pre-relation shape), so the effective wanted width \
         degraded to ALL DECLARED outputs (maximal width, never a vacuous \
         narrow set). Sustained increments mean pre-relation builds are \
         still live; the population shrinks to zero as they terminate."
    );
    describe_counter!(
        "rio_scheduler_materialization_no_verifiable_wanted_total",
        "A materialization re-arm found NO verifiable wanted path set \
         (width ZERO — empty set or empty-string placeholder paths, even \
         after the realized-path carrier union), so the consumption \
         closed uncharged and deferred instead of verifying vacuously. \
         The opposite end of the width axis from \
         ..._wanted_width_saturated_total (bug_282: the two event \
         classes share one typed chokepoint and never one counter)."
    );
    describe_gauge!(
        "rio_scheduler_substituting_derivations",
        "Derivations carrying a CLAIMABLE materialization job (unclaimed, not \
         parked, not deferred — claimability()'s three axes) — the substitution \
         backlog (the same quantity ClusterStatus.substituting_derivations \
         reports). Non-claimable jobs are pacing, not demand: they leave this \
         gauge (bug_252 — KEDA drains while the backlog paces). PARKED jobs stay \
         visible via rio_scheduler_materialization_stalled; DEFERRED jobs \
         (defer_until — the bounded <=300s re-probe window) are counted in \
         NEITHER gauge for that window. Leader-published every housekeeping tick \
         from the freshly computed cluster snapshot. The leading rio-store KEDA \
         scaling signal: backlog is known at merge time, minutes before the \
         store feels the ingest load."
    );
    describe_gauge!(
        "rio_scheduler_materialization_stalled",
        "Parked materialization jobs (park budget exhausted — worker-charged \
         infra failures or establishment charges; waiting on upstream recovery or \
         park-backoff expiry). Leader-published every housekeeping tick from ground \
         truth. The PD-20 re-evaluation converts parked jobs with buildable \
         dependency closures from-source ONLY when the conversion-strictness knobs \
         admit it (conversion_requires_worker_charge: establishment-only-parked \
         jobs never convert while ON; conversion_min_park_dwell_secs: minimum dwell \
         since the most recent park). A sustained nonzero value therefore means \
         either a dead/misconfigured upstream stalling work with no from-source \
         fallback, OR convertible work the knobs are deliberately holding — check \
         the strictness knobs before cancelling builds."
    );
    describe_counter!(
        "rio_scheduler_materialization_claims_total",
        "Materialization claims delivered to store replicas (one per minted open \
         attempt). Pairs with jobs_created_total (supply) and jobs_resolved_total \
         (drain) for the lifecycle rates: created-vs-claimed divergence means store \
         executors are not keeping up (or are partitioned from the leader); \
         claimed-vs-resolved divergence means executions are failing or reports are \
         not landing."
    );
    describe_counter!(
        "rio_scheduler_materialization_jobs_resolved_total",
        "Materialization jobs terminally resolved, labeled by outcome \
         (success|from_source|unobtainable|cancelled|obsolete). At-most-once per \
         job (re-resolution no-ops never double-count). success = the wanted set \
         was materialized from upstream; from_source = the job released the node \
         to normal from-source dispatch (durable Vouched/Pending evidence or the \
         PD-20 park re-evaluation); unobtainable = the fail-fast settlement; \
         cancelled = zero live interest remained (node gone/doomed or every \
         interested build terminal); obsolete = the node COMPLETED by other \
         means while the job was open (store probe, sibling production, CA \
         cutoff — live_061: pre-fix this face laundered into cancelled and the \
         label was zero-forever). A high infra-failure or unobtainable rate is \
         the upstream-health signal."
    );
    describe_counter!(
        "rio_scheduler_materialization_converted_total",
        "PD-20 park re-evaluation conversions: parked materialization jobs \
         resolved from_source because their infra budget exhausted while \
         from-source stayed viable — the TIME-driven subset of \
         jobs_resolved_total{outcome=from_source}, labeled by job origin \
         (pruned|cache_opportunity|stale_reset|reprobe). origin=cache_opportunity \
         means upstream-available content converted to a build: expected as \
         bounded noise during cold-start waves, alertable when sustained \
         (RioSchedulerMaterializationConversions — the wedged-ingest churn-loop \
         signature). At-most-once per applied resolution."
    );
    describe_counter!(
        "rio_scheduler_materialization_view_node_skew_total",
        "View/node coherence tripwires, labeled by polarity. \
         polarity=split_release: a pending-unclaimed job whose node is still \
         Assigned/Running with no open assignment (release_claim should have \
         requeued the node in the step that dropped the claim) — always zero in \
         a healthy fleet. polarity=claimed_no_attempt: a claim held in the view \
         with no backing assignment two sweeps running — always zero in a \
         healthy fleet. polarity=node_terminal_job_pending: the moot sweep \
         observed a pending job on a COMPLETED node (the by-other-means face \
         it resolves obsolete) — bounded noise during cache-racing builds; a \
         SUSTAINED rate is the live_061 zombie signature (a terminal edge \
         minting unresolvable pending rows faster than they drain). live_061's \
         lesson: the detector's alphabet quantified only over Assigned/Running \
         nodes, so the terminal-node face never fired — the polarity set is \
         now total over the skew faces the sweep can observe."
    );
    describe_counter!(
        "rio_scheduler_materialization_carrier_dropped_total",
        "Realized-path carriers dropped from the leader-scoped retry stash because the node went terminal/gone before the stale-reset job row applied (merged_bug_257). The carrier had no consumer left; the paths are reproducible from source if interest returns."
    );
    describe_histogram!(
        "rio_scheduler_critical_path_accuracy",
        "Predicted vs actual completion ratio (actual/estimated; 1.0=perfect, >1.0=underestimate)"
    );
    describe_counter!(
        "rio_scheduler_cache_check_circuit_open_total",
        "Circuit-breaker open transitions (store unreachable for 5 consecutive checks); alert if > 0"
    );
    // The following metrics are emitted from actor internals.
    describe_counter!(
        "rio_scheduler_build_timeouts_total",
        "Builds failed by per-build wall-clock timeout (BuildOptions.build_timeout \
         seconds since submission)."
    );
    describe_counter!(
        "rio_scheduler_orphan_builds_cancelled_total",
        "Active builds auto-cancelled by the orphan-watcher sweep: no \
         build_events receiver (gateway SubmitBuild/WatchBuild stream) for \
         >ORPHAN_BUILD_GRACE (5min). Backstop for gateway crash / \
         gateway→scheduler timeout during P0331 disconnect cleanup. Nonzero \
         is expected on gateway restarts; sustained nonzero with healthy \
         gateways means the gateway-side cancel is not firing."
    );
    describe_counter!(
        "rio_scheduler_recovery_total",
        "Scheduler state recoveries from PG after LeaderAcquired; exactly one \
         increment per attempt: outcome=success|failure when the loaded result \
         is applied (or the load failed), outcome=discarded_flap|\
         discarded_unconfirmed when the post-recovery gate throws the result \
         away (discard outcomes take precedence over the load result)"
    );
    describe_counter!(
        "rio_scheduler_recovery_step_down_total",
        "Cooperative lease step-downs requested because a tenure's state \
         recovery FAILED (sched.recovery.step-down): the replica never \
         completes the tenure, clears partial state, and yields the lease so \
         a healthy replica can serve. Under a persistent PG outage this \
         cycles at lease cadence across the replica set (acquire → fail → \
         step down) — deliberate and operator-visible; pair with \
         rio_scheduler_recovery_total{outcome=\"failure\"} (the alerting \
         series). In always-leader (non-K8s) deployments the request is a \
         dead letter: the counter still increments, the tenure stays \
         incomplete, and dispatch remains gated."
    );
    describe_counter!(
        "rio_scheduler_generation_claim_failed_total",
        "Generation-claim INSERTs that failed during recovery (PG error between the \
         seed read and the claim write). The leader proceeds unclaimed: dispatch is \
         not blocked, but that term's generation is absent from the \
         leader_generation_claims ledger, re-opening the deposed-before-persist \
         collision window for that one term. Sustained nonzero means PG is flapping \
         exactly at failover time."
    );
    describe_counter!(
        "rio_scheduler_generation_floor_read_failed_total",
        "PG generation-floor reads that failed during recovery. The term proceeds \
         unclaimed at the recovery-entry generation after the post-claim leadership \
         confirmation — dispatch is not blocked, but that term's generation is absent \
         from the leader_generation_claims ledger and may sit below the durable floor. \
         Sustained nonzero means PG is flapping exactly at failover time."
    );
    describe_counter!(
        "rio_scheduler_evidence_write_fenced_total",
        "Evidence writes (materialization job creation/resolution, wanted-relation \
         rows, derivation status/poison persists, merge transactions) refused by the claims-floor \
         generation fence: the writing replica's serving generation sat below the \
         durable floor (GREATEST over assignments.generation and \
         leader_generation_claims.generation). On a replica that just lost the lease \
         this is the fence working — the successor owns the evidence now; on the \
         current leader it should be ZERO (a nonzero rate there means a capture bug \
         or a PG floor regression and needs investigation)."
    );
    describe_histogram!(
        "rio_scheduler_recovery_duration_seconds",
        "Time to reconstruct actor state from PG on LeaderAcquired \
         (labeled by outcome=success|failure)"
    );
    describe_counter!(
        "rio_scheduler_attempt_record_retries_total",
        "Re-deliveries of failure completions whose attempt-recording transaction \
         (drv_attempts append + decide() + status persist) failed. Each increment is one \
         bounded re-push of the completion event onto the actor mailbox; the derivation \
         stays in its pre-report state until a re-delivery lands. Sustained rate > 0 means \
         PG is rejecting the failure-accounting write path (the backstop sweep is the \
         fallback once the per-derivation re-delivery budget is exhausted)."
    );
    describe_counter!(
        "rio_scheduler_lease_acquired_total",
        "Successful K8s Lease acquisitions (leader elections won)"
    );
    describe_counter!(
        "rio_scheduler_lease_lost_total",
        "K8s Lease losses (stepped down, partition, or preempted)"
    );
    describe_counter!(
        "rio_scheduler_lease_rebound_total",
        "Lease rebounds: holder changes observed late on a still-leading round \
         (a foreign term ran entirely inside our observation gap). Each runs the \
         Compound leadership-edge lose cells and then a full re-recovery."
    );
    describe_gauge!(
        "rio_scheduler_runtime_skew_seconds",
        "Executor-scheduling delay per runtime domain (domain=main|guard), measured by \
         the guard-domain sentinel (src/guard.rs): a no-op probe task's time-to-first-poll \
         on the main runtime, and the guard's own timer overshoot. While a main-domain \
         probe is unanswered the exported value is the RUNNING lower bound, so a live \
         stall is visible as it happens. domain=main at seconds-scale = the sh-002C \
         starvation shape (the 16.35s Tick that self-fenced the leader); domain=guard \
         elevated = the guard itself is starved (cgroup-class pressure — raise the \
         CPU request)."
    );
    describe_counter!(
        "rio_scheduler_runtime_skew_stalls_total",
        "Stall episodes per runtime domain (domain=main|guard): ONE increment per \
         episode in both domains (a shared edge latch — the increment is the rising \
         edge, re-armed at resolution; a main-domain episode that starts and resolves \
         between probe ticks still counts once at its settle). Live main-domain edges \
         log a thread-table capture (tid/comm/state) for attribution; settle-counted \
         episodes do not (nothing live remains). Rate > 0 = the dag-actor is being \
         starved; correlate with rio_scheduler_runtime_skew_seconds and the captured \
         table in logs."
    );
    describe_counter!(
        "rio_scheduler_sla_refit_total",
        "Completed SLA estimator background refreshes (≈60s cadence; \
         VM-test sync barrier — +1 = one refresh completed, regardless \
         of [sla] gate)"
    );
    describe_gauge!(
        "rio_scheduler_sla_refresh_age_seconds",
        "Seconds since the last completed SLA-estimator background \
         refresh. Emitted from the actor's housekeeping cadence (NOT \
         the poller loop) so it climbs when estimator_poller has \
         panicked or refresh() is persistently failing. Climbs from \
         boot until the first leader-side refresh."
    );
    describe_counter!(
        "rio_scheduler_disk_evidence_total",
        "Build samples entering the SLA evidence store (write_build_sample, \
         once per completion), labeled present=true|false by whether \
         peak_disk_bytes arrived. THE live_063 acceptance signal, carried \
         here because the builder-side absence counter is scrape-invisible \
         (one-build pods die before the first scrape): on a prjquota-\
         provisioned fleet present=true should dominate; a fleet stuck at \
         present=false means the disk-evidence chain is declining again \
         (mount, kernel, kubelet half, or the hostUsers posture vs the \
         builder-minted projid — quota.rs's four decline modes)."
    );
    describe_histogram!(
        "rio_scheduler_build_graph_edges",
        "Edge count per GetBuildGraph response. High p99 (>10k) = unusually \
         dense DAG approaching the implicit subgraph bound."
    );
    describe_counter!(
        "rio_scheduler_ca_hash_compares_total",
        "CA early-cutoff output-hash lookups against the content index on \
         successful completion (labeled by outcome=match|miss|skipped_after_miss|\
         malformed|error). High match ratio → CA derivations rebuilding identical \
         content; cutoff-propagate will skip downstream work. skipped_after_miss \
         counts outputs short-circuited after an earlier miss in the same \
         derivation. malformed = executor sent empty output_path; error = PG \
         lookup failed/timed out (alert if rate>0)."
    );
    describe_counter!(
        "rio_scheduler_ca_cutoff_saves_total",
        "Derivations skipped via CA early-cutoff (Queued→Skipped transitions). \
         Each increment is one build that did NOT run because a CA dep's \
         output matched the content index. Direct measure of CA cutoff \
         efficacy."
    );
    describe_counter!(
        "rio_scheduler_ca_cutoff_seconds_saved",
        "Sum of est_duration of skipped derivations, in hw-normalized \
         ref-seconds (r[sched.sla.hw-ref-seconds]; NOT wall-clock per-build — \
         skipped builds were never assigned, so no hw_factor exists to \
         denormalize; divide by fleet min hw_factor for a wall-clock lower \
         bound). est_duration is the Estimator's EMA, not actual — a \
         derivation that's never run has no actual. Paired with saves_total \
         for avg-seconds-per-save."
    );
    describe_counter!(
        "rio_scheduler_ca_cutoff_depth_cap_hits_total",
        "CA cutoff cascade walks that hit MAX_CASCADE_NODES (1000). \
         Node-count cap, not tree-depth — for wide DAGs (fanout>1), 1000 \
         nodes hit well before depth>3. Non-zero = cascades truncated; \
         operator should review if non-zero."
    );
    describe_gauge!(
        "rio_scheduler_actor_mailbox_depth",
        "ActorCommand mpsc queue depth, sampled once per dequeued command. \
         Growth = commands arriving faster than the single-threaded loop \
         retires them. Pair with actor_cmd_seconds to localize a wedge."
    );
    describe_histogram!(
        "rio_scheduler_attempt_requeue_seconds",
        "OA1 interval (ii): scheduler-side terminal/death observation → \
         the event that closes the released attempt and (re)queues the \
         derivation, by cause. cause=worker-report: the worker's own \
         failure report and the requeue, same actor turn (the sample is \
         the in-turn processing latency). cause=pod-terminal: disconnect \
         observation → the controller's classifying termination report \
         consumed. cause=establishment: disconnect observation → the \
         TTL-sweep establishment fill. cause=synthesized: a \
         controller-synthesized cancelled/preempted/reaped verdict closed \
         an open pull attempt charge-free (in-turn latency, AD5). \
         cause=worker-abort: the builder's SIGTERM-abort report closed a \
         still-wanted pull attempt charge-free (in-turn latency, AD5). \
         In stream mode the requeue itself \
         happens at the disconnect, so the pod-terminal/establishment \
         samples measure how long the released attempt waited for its \
         classification — the controller-report-slack input the \
         executor-replacement establishment window is sized against \
         (executor-lifecycle campaign, OA1)."
    );
    describe_counter!(
        "rio_scheduler_broadcast_lagged_total",
        "BuildEvent broadcast events skipped by lagging subscribers \
         (sum of RecvError::Lagged(n) across all bridge tasks; labeled \
         by kind: state = state-event stream, the watcher gets an \
         in-stream ResyncRequired and recovers via snapshot reconcile; \
         log = log-batch stream, skipped batches are lost to live \
         tailing but durable in the store's log lane). Non-zero \
         under sustained event burst (large DAG, many concurrent drvs \
         emitting Log lines) — gateway can't drain fast enough (I-144)."
    );
    crate::sla::metrics::describe_all();
    // merged_bug_001: the absorb-counter HELP lives beside its emit
    // site (admin/mod.rs) — same module-owned pattern as sla above.
    crate::admin::describe_admin_metrics();
    // merged_bug_017: the outbox replay-refused HELP lives beside its
    // emit site (actor/housekeeping.rs) — same module-owned pattern.
    crate::actor::describe_housekeeping_metrics();
    // live_064: the jwt_interceptor this binary layers in emits
    // rio_auth_* — its HELP registers from here so (lllll) holds.
    rio_auth::describe_metrics();

    // Series birth (C3 metric-ownership): every alert-referenced
    // counter and every leader-family gauge exists from the first
    // scrape on every replica. Tail of describe_metrics() because
    // rio_common::server::run installs the real exporter
    // (init_metrics) immediately before calling this fn — the seeds
    // bind to the exporter, not a pre-init noop recorder. NOT in
    // DagActor::new: boot scrape-surface is a process property, and
    // the standby gauge tests' touch-sets must stay actor-clean.
    // r[impl obs.metric.alert-counter-seeded]
    // r[impl obs.metric.scheduler-leader-gate+5]
    crate::observability::seed_alert_counters();
    crate::observability::seed_leader_gauges();
}
