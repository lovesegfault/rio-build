//! Scenario registry. One module per scenario; `ALL` is the manual slice
//! the scheduler iterates.
//!
//! Seeded from `.stress-test/issues/` — each scenario is "the check that
//! would have caught I-NNN on the live cluster". See
//! `~/tmp/rio-qa/i-nnn-scenarios.md` for the full candidate list.

use super::Scenario;

mod i032_relay_loop;
mod i048c_blackhole_self_test;
mod i056_stale_draining;
mod i078_narinfo_seqscan;
mod i095_ghost_dispatch;
mod i114_liveness_kill;
mod i163_mailbox_under_load;
mod i165_fuse_dstate;
mod i201_stranded_chunks;
mod i205_nodepool_schedulable;

pub static ALL: &[&dyn Scenario] = &[
    // ─── Shared (read-only) ───────────────────────────────────────────
    &i056_stale_draining::StaleDraining,
    &i078_narinfo_seqscan::NarinfoSeqScan,
    &i205_nodepool_schedulable::NodepoolSchedulable,
    // ─── Tenant ───────────────────────────────────────────────────────
    &i032_relay_loop::RelayLoop,
    &i095_ghost_dispatch::GhostDispatch,
    &i114_liveness_kill::LivenessKill,
    &i163_mailbox_under_load::MailboxUnderLoad,
    // ─── Exclusive ────────────────────────────────────────────────────
    &i048c_blackhole_self_test::BlackholeSelfTest,
    &i165_fuse_dstate::FuseDState,
    &i201_stranded_chunks::StrandedChunks,
];
