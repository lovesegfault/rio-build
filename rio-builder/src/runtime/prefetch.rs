//! PrefetchHint handling: warm the FUSE cache and ACK the scheduler.
//!
//! Warm-gate protocol (`r[sched.assign.warm-gate]`): the scheduler
//! gates dispatch on `ExecutorState.warm = true`, flipped on receipt of
//! `PrefetchComplete`. We send the ACK AFTER every path's fetch task
//! has returned — the scheduler's first assignment then arrives with a
//! warm cache.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tracing::instrument;

use rio_proto::types::{ExecutorMessage, PrefetchComplete, PrefetchHint, executor_message};

use crate::fuse;
use crate::fuse::StoreClients;

/// I-212: warm-gate size cap. PrefetchHint paths whose `nar_size`
/// exceeds this are skipped when the JIT allowlist is not yet armed
/// (i.e., during the initial warm-gate batch, before any assignment,
/// when the builder can't tell declared inputs from over-included
/// sibling outputs). The scheduler's `approx_input_closure` sends ALL outputs of
/// each input drv — a 2.9 GB `clang-debug` arrives alongside the
/// `clang-out` the build actually needs. Declared inputs the build DOES
/// need and that exceed the cap are fetched on-demand by JIT lookup
/// (which has the size-aware `jit_fetch_timeout`), so this never blocks
/// a correct build; it only stops the warm-gate from speculatively
/// pulling multi-GB paths it can't prove are needed.
///
/// 256 MiB: large enough to cover the common-set inputs the warm-gate is
/// for (glibc ~40 MB, gcc-unwrapped ~200 MB), small enough to exclude
/// debug outputs (clang-debug 2.9 GB, llvm-debug ~1.5 GB).
// r[impl builder.warmgate.filter]
pub(super) const PREFETCH_WARM_SIZE_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Per-hint dependencies bundled so [`run`](super::run) can call
/// [`handle_prefetch_hint`] without 7 loose fields on `BuilderRuntime`.
pub(super) struct PrefetchDeps {
    pub(super) cache: Arc<crate::fuse::cache::Cache>,
    pub(super) clients: StoreClients,
    pub(super) runtime: tokio::runtime::Handle,
    pub(super) sem: Arc<Semaphore>,
    pub(super) fetch_timeout: Duration,
}

/// Handle a PrefetchHint from the scheduler: spawn one fire-and-forget
/// task per path to warm the FUSE cache, then send `PrefetchComplete`
/// once all paths have finished (succeeded, cached, or errored).
///
/// Called from main.rs's event loop. Does NOT block the caller: each
/// path is spawned as an independent tokio task that acquires a permit
/// from `sem` before entering the blocking pool. A joiner task awaits
/// all handles and sends the warm-gate ACK.
///
/// Warm-gate protocol (`r[sched.assign.warm-gate]`): the scheduler
/// gates dispatch on `ExecutorState.warm = true`, flipped on receipt of
/// `PrefetchComplete`. We send the ACK AFTER every path's fetch task
/// has returned — the scheduler's first assignment then arrives with a
/// warm cache. An empty hint (paths=[]) sends the ACK immediately.
///
/// No JoinHandle leak: if the worker SIGTERMs mid-prefetch, the tasks
/// abort with the runtime — the partial fetch is in a .tmp-XXXX sibling
/// dir (see fetch_extract_insert) which cache init cleans up on next
/// start. The joiner task also aborts; no ACK is sent — that's fine,
/// we're shutting down.
#[instrument(skip_all, fields(count = prefetch.store_paths.len()))]
pub fn handle_prefetch_hint(
    prefetch: PrefetchHint,
    cache: Arc<fuse::cache::Cache>,
    clients: crate::fuse::StoreClients,
    rt: tokio::runtime::Handle,
    sem: Arc<Semaphore>,
    fetch_timeout: std::time::Duration,
    stream_tx: mpsc::Sender<ExecutorMessage>,
) {
    // Collect JoinHandles for the ACK-joiner task. A typical hint
    // is ≤100 paths (scheduler caps at MAX_PREFETCH_PATHS=100) →
    // ≤100 handles. Cheap (JoinHandle is a small struct).
    let mut handles: Vec<tokio::task::JoinHandle<&'static str>> =
        Vec::with_capacity(prefetch.store_paths.len());

    // Spawn one task per path. Don't await — the
    // whole point is to NOT block the stream loop
    // on prefetch. The semaphore bounds concurrent
    // in-flight; excess queue in tokio's task
    // scheduler (cheap — no blocking-pool thread
    // is held until the permit is acquired).
    for store_path in prefetch.store_paths {
        // Scheduler sends full paths; we need
        // basename. Malformed (no /nix/store/
        // prefix) → skip with debug log. Don't
        // fail the loop — one bad path in a
        // batch shouldn't poison the rest.
        let Some(basename) = rio_nix::store_path::basename(&store_path) else {
            tracing::debug!(
                path = %store_path,
                "prefetch: malformed path (no /nix/store/ prefix), skipping"
            );
            metrics::counter!("rio_builder_prefetch_total", "result" => "malformed").increment(1);
            continue;
        };
        let basename = basename.to_string();

        // Clone handles into the task. All cheap:
        // Arc clone, tonic Channel is Arc-internal,
        // tokio Handle is a lightweight token.
        let cache = Arc::clone(&cache);
        let clients = clients.clone();
        let rt = rt.clone();
        let sem = Arc::clone(&sem);

        let handle = tokio::spawn(async move {
            // Permit BEFORE spawn_blocking: if the
            // semaphore is saturated, this task
            // waits here (cheap async wait) not
            // in the blocking pool. Tasks queue
            // in tokio's scheduler; blocking
            // threads only taken when a permit
            // is available.
            //
            // On Err(Closed): semaphore closed →
            // worker shutting down. Drop the
            // prefetch silently — it was a hint.
            let Ok(_permit) = sem.acquire_owned().await else {
                return "shutdown";
            };

            // I-212 filter, BEFORE spawn_blocking: jit_classify is a
            // cheap RwLock read; QueryPathInfo (warm-gate arm) is a
            // single async RPC. Both belong in the async half so the
            // blocking pool only sees work that's actually going to
            // fetch.
            use crate::fuse::cache::JitClass;
            let store_path = format!("/nix/store/{basename}");
            match cache.jit_classify(&basename) {
                JitClass::NotInput => {
                    // Armed and NOT a declared input → the build can
                    // never read this path (FUSE lookup would ENOENT).
                    metrics::counter!("rio_builder_prefetch_filtered_total",
                                      "reason" => "not_input")
                    .increment(1);
                    metrics::counter!("rio_builder_prefetch_total",
                                      "result" => "not_input")
                    .increment(1);
                    return "not_input";
                }
                JitClass::NotArmed => {
                    // Warm-gate batch (before any assignment). The
                    // scheduler over-includes; we can't tell declared
                    // from sibling. Size-cap via QPI: skip paths above
                    // PREFETCH_WARM_SIZE_CAP_BYTES. On QPI error, fall
                    // through to fetch — I-211's progress-based timeout
                    // makes a large fetch correct, just slow.
                    let mut sc = clients.store.clone();
                    if let Ok(Some(info)) = rio_proto::client::query_path_info_opt(
                        &mut sc,
                        &store_path,
                        fetch_timeout,
                        &[],
                    )
                    .await
                        && info.nar_size > PREFETCH_WARM_SIZE_CAP_BYTES
                    {
                        metrics::counter!("rio_builder_prefetch_filtered_total",
                                          "reason" => "size_cap")
                        .increment(1);
                        metrics::counter!("rio_builder_prefetch_total",
                                          "result" => "size_cap")
                        .increment(1);
                        return "size_cap";
                    }
                }
                JitClass::KnownInput { .. } => {
                    // Declared input — fetch unconditionally. A huge
                    // declared input is still needed; JIT lookup would
                    // fetch it anyway, just later.
                }
            }

            // spawn_blocking: Cache methods use
            // block_on internally (nested-runtime
            // panic from async). The permit moves
            // into the blocking closure and drops
            // when it returns — next queued task
            // wakes.
            let result = tokio::task::spawn_blocking(move || {
                use crate::fuse::fetch::{PrefetchSkip, prefetch_path_blocking};
                let _permit = _permit; // hold through blocking work
                match prefetch_path_blocking(&cache, &clients, &rt, fetch_timeout, &basename) {
                    Ok(None) => "fetched",
                    Ok(Some(PrefetchSkip::AlreadyCached)) => "already_cached",
                    Ok(Some(PrefetchSkip::AlreadyInFlight)) => "already_in_flight",
                    Err(_) => "error",
                }
            })
            .await;

            // JoinError (panic in blocking) →
            // record as "panic". Don't re-panic
            // — we're fire-and-forget.
            let label = result.unwrap_or("panic");
            metrics::counter!("rio_builder_prefetch_total", "result" => label).increment(1);
            label
        });
        handles.push(handle);
    }

    // r[impl sched.assign.warm-gate]
    // r[impl builder.warmgate.handshake]
    // Joiner: wait for ALL path-fetch tasks to return, then send the
    // PrefetchComplete ACK. spawn_monitored so a panic in the joiner
    // logs with task=prefetch-complete instead of vanishing. Does NOT
    // block the caller (main.rs event loop).
    //
    // Serialization: all per-path tasks were spawned above. They run
    // concurrently (bounded by `sem`). The joiner awaits each handle
    // in order — order doesn't matter for semantics (we only care
    // about "all done"), it's just the simplest join-all. A slow path
    // delays the ACK for the whole batch, which is CORRECT: the
    // scheduler should wait until the cache is actually warm.
    rio_common::task::spawn_monitored("prefetch-complete", async move {
        let mut fetched: u32 = 0;
        let mut cached: u32 = 0;
        for handle in handles {
            // JoinError (task aborted or panicked) → count as neither
            // fetched nor cached. The label was already recorded in
            // the metric above; for the ACK we just skip it.
            if let Ok(label) = handle.await {
                match label {
                    "fetched" => fetched += 1,
                    "already_cached" | "already_in_flight" => cached += 1,
                    // "error", "malformed", "shutdown", "panic" → noise.
                    // Scheduler gates on receipt, not on counts.
                    _ => {}
                }
            }
        }

        // send().await not try_send(): the ACK MUST land. If the
        // permanent-sink relay is backpressured (256 cap filled by
        // log batches during a chatty build), we block here until a
        // slot frees. That delays the NEXT prefetch hint's ACK by
        // one stream-roundtrip — acceptable. Dropping the ACK would
        // leave the worker cold in the scheduler's view forever.
        let ack = ExecutorMessage {
            msg: Some(executor_message::Msg::PrefetchComplete(PrefetchComplete {
                paths_fetched: fetched,
                paths_cached: cached,
            })),
        };
        if let Err(e) = stream_tx.send(ack).await {
            // Sink closed → worker is shutting down. Fine — no point
            // ACKing to a scheduler we're disconnecting from.
            tracing::debug!(error = %e,
                            "PrefetchComplete send failed (sink closed; shutting down?)");
        } else {
            tracing::debug!(fetched, cached, "sent PrefetchComplete (warm-gate ACK)");
        }
    });
}
