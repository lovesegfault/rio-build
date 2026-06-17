//! PrefetchHint handling: ACK the scheduler's warm-gate.
//!
//! Warm-gate protocol (`r[sched.assign.warm-gate]`): the scheduler
//! gates dispatch on `ExecutorState.warm = true`, flipped on receipt of
//! `PrefetchComplete`. Pre-ADR-022 the builder used the hint to fetch
//! whole NARs into its local JIT cache before ACKing; with the
//! castore-FUSE lower there is nothing for the builder to pre-warm —
//! metadata comes from the mount-time Directory-DAG prefetch and file
//! content is fetched lazily into the mountd-owned node cache at
//! `open()`. The ACK itself is still required: without it the scheduler
//! never marks the executor warm and never dispatches.

use tokio::sync::mpsc;
use tracing::instrument;

use rio_proto::types::{ExecutorMessage, PrefetchComplete, PrefetchHint, executor_message};

/// Handle a PrefetchHint from the scheduler: acknowledge it immediately
/// with `PrefetchComplete` so the warm-gate opens.
///
/// Called from the runtime's stream loop. Does NOT block the caller —
/// the ACK send is spawned (the permanent sink can be backpressured by
/// log batches from a chatty build, and the stream loop must keep
/// processing Cancel messages meanwhile).
///
/// `paths_fetched`/`paths_cached` are reported as 0: the builder no
/// longer materializes hint paths. The scheduler gates on receipt of
/// the message, not on the counts.
#[instrument(skip_all, fields(count = prefetch.store_paths.len()))]
pub fn handle_prefetch_hint(prefetch: PrefetchHint, stream_tx: mpsc::Sender<ExecutorMessage>) {
    let hinted = prefetch.store_paths.len();
    // r[impl sched.assign.warm-gate]
    // r[impl builder.warmgate.handshake+2]
    rio_common::task::spawn_monitored("prefetch-complete", async move {
        // send().await not try_send(): the ACK MUST land. If the
        // permanent-sink relay is backpressured we block here until a
        // slot frees; dropping the ACK would leave the worker cold in
        // the scheduler's view forever.
        let ack = ExecutorMessage {
            msg: Some(executor_message::Msg::PrefetchComplete(PrefetchComplete {
                paths_fetched: 0,
                paths_cached: 0,
            })),
        };
        if let Err(e) = stream_tx.send(ack).await {
            // Sink closed → worker is shutting down. Fine — no point
            // ACKing to a scheduler we're disconnecting from.
            tracing::debug!(error = %e,
                            "PrefetchComplete send failed (sink closed; shutting down?)");
        } else {
            tracing::debug!(
                hinted,
                "sent PrefetchComplete (warm-gate ACK; castore lower needs no pre-warm)"
            );
        }
    });
}
