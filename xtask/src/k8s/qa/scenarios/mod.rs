//! Scenario registry. One module per scenario; `ALL` is the manual slice
//! the scheduler iterates.
//!
//! Seeded from `.stress-test/issues/` — each scenario is "the check that
//! would have caught I-NNN on the live cluster". See
//! `~/tmp/rio-qa/i-nnn-scenarios.md` for the full candidate list.

use super::Scenario;

mod common;

mod i006_irsa_denied;
mod i011_builds_tenant_name;
mod i013_cancel_on_disconnect;
mod i023_nodepool_arch;
mod i026_scheduler_health;
mod i032_relay_loop;
mod i037_cordoned_control_plane;
mod i043_shared_input_enoent;
mod i048c_blackhole_self_test;
mod i049_dispatch_latency;
mod i050_metrics_reachable;
mod i056_stale_draining;
mod i078_narinfo_seqscan;
mod i085_controller_ipfamilies;
mod i095_ghost_dispatch;
mod i098_pool_arch_match;
mod i102_cli_builds_latency;
mod i114_liveness_kill;
mod i115_temp_suffix_fastpath;
mod i116_idle_exit;
mod i126_do_not_disrupt;
mod i156_histogram_buckets;
mod i163_mailbox_under_load;
mod i165_fuse_dstate;
mod i167_drv_name_query;
mod i181_kvm_featureless;
mod i201_stranded_chunks;
mod i204_big_parallel_routing;
mod i205_nodepool_schedulable;
mod i212_prefetch_filtered;

pub static ALL: &[&dyn Scenario] = &[
    // ─── Shared (read-only) ───────────────────────────────────────────
    &i023_nodepool_arch::NodepoolArch,
    &i026_scheduler_health::SchedulerHealth,
    &i037_cordoned_control_plane::CordonedControlPlane,
    &i050_metrics_reachable::MetricsReachable,
    &i056_stale_draining::StaleDraining,
    &i078_narinfo_seqscan::NarinfoSeqScan,
    &i085_controller_ipfamilies::ControllerIpFamilies,
    &i098_pool_arch_match::PoolArchMatch,
    &i102_cli_builds_latency::CliBuildsLatency,
    &i156_histogram_buckets::HistogramBuckets,
    &i205_nodepool_schedulable::NodepoolSchedulable,
    // ─── Tenant ───────────────────────────────────────────────────────
    &i006_irsa_denied::IrsaDenied,
    &i011_builds_tenant_name::BuildsTenantName,
    &i013_cancel_on_disconnect::CancelOnDisconnect,
    &i032_relay_loop::RelayLoop,
    &i043_shared_input_enoent::SharedInputEnoent,
    &i049_dispatch_latency::DispatchLatency,
    &i095_ghost_dispatch::GhostDispatch,
    &i114_liveness_kill::LivenessKill,
    &i115_temp_suffix_fastpath::TempSuffixFastpath,
    &i116_idle_exit::IdleExit,
    &i126_do_not_disrupt::DoNotDisrupt,
    &i163_mailbox_under_load::MailboxUnderLoad,
    &i167_drv_name_query::DrvNameQuery,
    &i181_kvm_featureless::KvmFeatureless,
    &i204_big_parallel_routing::BigParallelRouting,
    &i212_prefetch_filtered::PrefetchFiltered,
    // ─── Exclusive ────────────────────────────────────────────────────
    &i048c_blackhole_self_test::BlackholeSelfTest,
    &i165_fuse_dstate::FuseDState,
    &i201_stranded_chunks::StrandedChunks,
];
