//! PrefetchHint handling: ACK the scheduler's warm-gate.
//!
//! Warm-gate protocol (`r[sched.assign.warm-gate]`): the scheduler
//! gates dispatch on `ExecutorState.warm = true`, flipped on receipt of
//! `PrefetchComplete`.
//!
//! Since the P0560 castore cutover there is no pod-level FUSE cache to
//! warm — input materialization is per-build (the castore mount
//! prefetches the closure's Directory DAG at mount time, and file bytes
//! are served from the node-shared mountd-owned cache). Every hint is
//! therefore handled the way an empty hint always was: ACK immediately
//! so the scheduler's gate opens. The hint message itself, the gate,
//! and this handler are removed together in the post-cutover cleanup.

use tokio::sync::mpsc;
use tracing::instrument;

use rio_proto::types::{ExecutorMessage, PrefetchComplete, PrefetchHint, executor_message};

/// I-212 warm-gate size cap, retained only for the post-cutover spec
/// sweep (the warm-gate fetch body it bounded was removed at the P0560
/// §A cutover; nothing reads it).
// r[impl builder.warmgate.filter]
#[allow(dead_code)]
pub(super) const PREFETCH_WARM_SIZE_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Handle a PrefetchHint from the scheduler: acknowledge it immediately
/// with `PrefetchComplete` so the warm-gate opens.
///
/// Castore-FUSE has nothing to pre-warm at the pod level (the per-build
/// mount does its own DAG prefetch and the node cache is shared and
/// mountd-owned), so the counts are always zero. Spawned, not awaited —
/// the ACK goes through the permanent sink and must not block the
/// BuildExecution event loop even when the relay is backpressured.
// r[impl sched.assign.warm-gate]
// r[impl builder.warmgate.handshake]
#[instrument(skip_all, fields(count = prefetch.store_paths.len()))]
pub fn handle_prefetch_hint(prefetch: PrefetchHint, stream_tx: mpsc::Sender<ExecutorMessage>) {
    let hinted = prefetch.store_paths.len();
    rio_common::task::spawn_monitored("prefetch-complete", async move {
        let ack = ExecutorMessage {
            msg: Some(executor_message::Msg::PrefetchComplete(PrefetchComplete {
                paths_fetched: 0,
                paths_cached: 0,
            })),
        };
        // send().await not try_send(): the ACK MUST land. If the
        // permanent-sink relay is backpressured, block here until a slot
        // frees — dropping the ACK would leave the worker cold in the
        // scheduler's view forever.
        if let Err(e) = stream_tx.send(ack).await {
            // Sink closed → worker is shutting down. Fine — no point
            // ACKing to a scheduler we're disconnecting from.
            tracing::debug!(error = %e,
                            "PrefetchComplete send failed (sink closed; shutting down?)");
        } else {
            tracing::debug!(
                hinted,
                "sent PrefetchComplete (warm-gate ACK; castore-FUSE needs no pod-level warm)"
            );
        }
    });
}
