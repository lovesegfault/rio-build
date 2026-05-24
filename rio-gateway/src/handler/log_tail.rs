//! Per-derivation live-tail subscriptions to rio-store's
//! `LogService.TailLog`.
//!
//! The gateway used to receive build-log lines as `Event::Log` items on
//! the scheduler's `BuildEvent` stream. The log data plane has moved to
//! rio-store: builders stream lines to the store's `AppendLog`, and the
//! gateway pulls them back out by opening one `TailLog(follow: true)`
//! subscription per *building derivation* of a watched build. The
//! subscription tasks feed a single per-build output channel; the
//! build's event loop relays each chunk to the nix client through the
//! same `relay_log_batch` path the `Event::Log` arm used.
//!
//! ## The subscription lifecycle
//!
//! Each rule below exists because the naive version loses or duplicates
//! lines (see `~/tmp/harden-logs/DESIGN.md` §5.3):
//!
//! - **Open on `Started`** at `since_line = 0`. The store serves
//!   history-then-live, so lines that arrived before the gateway
//!   subscribed are not lost. A `Started` with an empty `exec_id`
//!   (a node-vanished race upstream) gets no subscription.
//! - **Re-open on premature end** with `since_line = last_relayed + 1`
//!   after a backoff. Store deploys, replica crashes, and proxy
//!   failures all close the stream; the *client* owns re-subscription.
//!   The store's chunk granularity means the re-opened stream may
//!   resend lines below the cursor — they are trimmed here so the nix
//!   client never sees a line twice.
//! - **Replace on re-dispatch**: a second `Started` with a *different*
//!   exec_id hard-cancels the old subscription (its execution is dead)
//!   and opens a fresh one at `since_line = 0`.
//! - **Drain on terminal**: the per-derivation `Completed`/`Failed`
//!   event does NOT cancel the subscription — the terminal event (via
//!   the scheduler) and the final log lines (via the store) travel on
//!   different network paths, and cancelling on terminal races away
//!   the build error. The task stops *re-opening* and lets the current
//!   stream drain to its natural end, capped at a post-terminal grace.
//! - **Hard-cancel at build terminus** (`LogTailSet::abort_all`).

use std::collections::HashMap;
use std::time::Duration;

use rio_proto::LogServiceClient;
use rio_proto::store::{TailLogChunk, TailLogRequest};
use rio_proto::types;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tracing::{Instrument, debug, info_span, warn};

/// How long a subscription waits before re-opening a prematurely-ended
/// stream. The store's failure modes here are restart/deploy shaped
/// (the replica serving the stream went away), not congestion shaped,
/// so a fixed backoff is enough; the scheduler-stream reconnect's
/// exponential ladder would just delay the live tail's recovery.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// How long a subscription keeps draining its current stream after the
/// derivation goes terminal. The builder finishes its log upload (with
/// its own 2 s grace) *before* reporting completion to the scheduler,
/// so by the time the terminal event reaches the gateway the store
/// almost always already holds — and has usually already served — the
/// final lines; this grace covers the tail of that race. Matches the
/// house style for bounded teardown waits.
const TERMINAL_GRACE: Duration = Duration::from_secs(2);

/// Per-build output queue depth. A slow nix client fills the channel
/// and backpressures the subscription tasks' `send().await`, which is
/// correct — the store-side per-subscriber queue sheds load if the
/// gateway itself stops reading, and the gateway's own channel applies
/// backpressure to its own reads.
const OUT_QUEUE_DEPTH: usize = 256;

/// Tuning knobs for [`LogTailSet`], overridable in tests so the
/// grace/backoff tests don't take wall-clock seconds.
#[derive(Clone, Copy, Debug)]
pub(super) struct LogTailConfig {
    pub reconnect_backoff: Duration,
    pub terminal_grace: Duration,
}

impl Default for LogTailConfig {
    fn default() -> Self {
        Self {
            reconnect_backoff: RECONNECT_BACKOFF,
            terminal_grace: TERMINAL_GRACE,
        }
    }
}

/// One chunk of log lines from a `TailLog` subscription, tagged with
/// the derivation it belongs to so the build's event loop can attach it
/// to the right `actBuild` activity.
#[derive(Debug)]
pub(super) struct TaggedLogChunk {
    pub derivation_path: String,
    pub first_line_number: u64,
    pub lines: Vec<Vec<u8>>,
}

impl TaggedLogChunk {
    /// Rebuild the `BuildLogBatch` shape `relay_log_batch` consumes.
    /// `executor_id` is debugging metadata the relay never reads.
    pub(super) fn into_batch(self) -> types::BuildLogBatch {
        types::BuildLogBatch {
            derivation_path: self.derivation_path,
            lines: self.lines,
            first_line_number: self.first_line_number,
            executor_id: String::new(),
        }
    }
}

/// One live subscription: the execution it is keyed on, the signal that
/// flips it from "re-open forever" to "drain and exit", and the task
/// handle for the hard-cancel paths.
struct TailHandle {
    exec_id: String,
    drain: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// All live-tail subscriptions for one watched build.
///
/// Owned by `submit_and_process_build` alongside the activity state; it
/// survives scheduler-stream reconnects (the subscriptions are
/// independent of the scheduler connection). Holds a clone of the
/// output sender so the receiver in the event loop can never observe
/// `None` while the set is alive.
pub(super) struct LogTailSet {
    client: LogServiceClient<Channel>,
    out_tx: mpsc::Sender<TaggedLogChunk>,
    config: LogTailConfig,
    tasks: HashMap<String, TailHandle>,
}

impl LogTailSet {
    /// Create the set and its output channel. The receiver goes to the
    /// build's event loop; the set keeps a sender clone.
    pub(super) fn new(client: LogServiceClient<Channel>) -> (Self, mpsc::Receiver<TaggedLogChunk>) {
        Self::with_config(client, LogTailConfig::default())
    }

    pub(super) fn with_config(
        client: LogServiceClient<Channel>,
        config: LogTailConfig,
    ) -> (Self, mpsc::Receiver<TaggedLogChunk>) {
        let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE_DEPTH);
        (
            Self {
                client,
                out_tx,
                config,
                tasks: HashMap::new(),
            },
            out_rx,
        )
    }

    /// `DerivationEvent::Started` arrived for `derivation_path`.
    ///
    /// - Empty `exec_id` → no subscription (the field is documented as
    ///   possibly empty on an unreachable node-vanished race; without
    ///   an execution there is no log to tail).
    /// - No existing subscription → open one at `since_line = 0`.
    /// - Existing subscription with the *same* exec_id → duplicate
    ///   `Started` (scheduler-stream replay); keep it.
    /// - Existing subscription with a *different* exec_id → the
    ///   derivation was re-dispatched; the old execution's log is dead.
    ///   Hard-cancel and re-open at `since_line = 0` for the new one.
    pub(super) fn on_started(&mut self, derivation_path: &str, exec_id: &str) {
        if exec_id.is_empty() {
            return;
        }
        if let Some(existing) = self.tasks.get(derivation_path) {
            if existing.exec_id == exec_id {
                return;
            }
            debug!(
                drv = %derivation_path,
                old_exec = %existing.exec_id,
                new_exec = %exec_id,
                "re-dispatch: replacing log-tail subscription"
            );
            if let Some(old) = self.tasks.remove(derivation_path) {
                old.task.abort();
            }
        }
        let (drain_tx, drain_rx) = watch::channel(false);
        let span = info_span!(
            "log_tail",
            drv = %derivation_path,
            exec_id = %exec_id,
        );
        let task = tokio::spawn(
            run_tail(
                self.client.clone(),
                derivation_path.to_string(),
                exec_id.to_string(),
                self.out_tx.clone(),
                drain_rx,
                self.config,
            )
            .instrument(span),
        );
        self.tasks.insert(
            derivation_path.to_string(),
            TailHandle {
                exec_id: exec_id.to_string(),
                drain: drain_tx,
                task,
            },
        );
    }

    /// The derivation went terminal (`Completed`/`Failed`). Stop the
    /// subscription from re-opening and let its current stream drain.
    /// The entry stays in the map so a later `Started` with a new
    /// exec_id is still recognised as a replacement.
    pub(super) fn on_terminal(&mut self, derivation_path: &str) {
        if let Some(handle) = self.tasks.get(derivation_path) {
            // send_replace never fails (we hold the receiver's peer
            // inside the task); if the task already exited the value
            // is simply never read.
            let _ = handle.drain.send_replace(true);
        }
    }

    /// Build terminus: hard-cancel everything still running. Chunks
    /// already delivered to the output channel are unaffected (the
    /// caller drains that channel before dropping it).
    pub(super) fn abort_all(&mut self) {
        for (_, handle) in self.tasks.drain() {
            handle.task.abort();
        }
    }
}

/// Why one connected `TailLog` stream stopped being driven.
enum StreamEnd {
    /// The stream ended (or errored) while the derivation was not yet
    /// terminal. The caller backs off and re-opens.
    Premature,
    /// The derivation is terminal and the stream ended naturally or the
    /// post-terminal grace expired. The subscription is finished.
    Drained,
    /// The output channel's receiver is gone — the build's event loop
    /// has exited. Nothing left to relay to.
    OutputClosed,
}

// r[impl store.log.tail-reconnect]
/// One subscription's lifetime: open → drive → (backoff → re-open)*.
async fn run_tail(
    mut client: LogServiceClient<Channel>,
    derivation_path: String,
    exec_id: String,
    out_tx: mpsc::Sender<TaggedLogChunk>,
    mut drain: watch::Receiver<bool>,
    config: LogTailConfig,
) {
    // The highest line number forwarded to the output channel. `None`
    // until the first line. Survives re-opens — it is the dedup floor
    // that makes the at-least-once store stream exactly-once on the
    // client's wire.
    let mut last_relayed: Option<u64> = None;
    // Set once the first open-failure of a consecutive run has been
    // logged at `warn!`; reset on a successful open. Without the latch
    // a store that is down for a whole build would emit one warn per
    // second per derivation; without the warn at all, a fleet-wide
    // dead live-tail is indistinguishable from quiet builds at any log
    // level an operator actually runs. The reconnect *counter* below is
    // the alerting signal; the warn is the "which derivation / which
    // status code" breadcrumb next to it.
    let mut warned_open_failure = false;
    loop {
        let since_line = last_relayed.map_or(0, |n| n.saturating_add(1));
        let request = TailLogRequest {
            derivation: derivation_path.clone(),
            exec_id: exec_id.clone(),
            since_line,
            follow: true,
        };
        let stream = match client.tail_log(request).await {
            Ok(resp) => resp.into_inner(),
            Err(status) => {
                // Open failed (store unreachable, NotFound because the
                // execution hasn't recorded anything yet, ...). All of
                // these are retryable from the live tail's perspective
                // — the lines are durable in the store regardless, and
                // a reader that gives up early just degrades to the
                // historical read path. Once the derivation is
                // terminal, stop retrying: the current stream (there
                // is none) has nothing to drain.
                //
                // Deliberately NOT surfaced to the nix client: a
                // "log tail reconnecting" line in build output is
                // noise the user can't act on, and the lines are
                // durable in the store regardless.
                metrics::counter!(
                    "rio_gateway_log_tail_reconnects_total",
                    "reason" => "open_failed"
                )
                .increment(1);
                if warned_open_failure {
                    debug!(code = ?status.code(), "TailLog open failed");
                } else {
                    warned_open_failure = true;
                    warn!(
                        code = ?status.code(),
                        since_line,
                        "TailLog open failed; live tail degraded until the store is reachable \
                         (retrying every {:?})",
                        config.reconnect_backoff
                    );
                }
                if *drain.borrow() {
                    return;
                }
                if backoff_or_terminal(&mut drain, config.reconnect_backoff).await {
                    return;
                }
                continue;
            }
        };
        warned_open_failure = false;
        match drive_stream(
            stream,
            &derivation_path,
            &out_tx,
            &mut drain,
            &mut last_relayed,
            config.terminal_grace,
        )
        .await
        {
            StreamEnd::OutputClosed | StreamEnd::Drained => return,
            StreamEnd::Premature => {
                metrics::counter!(
                    "rio_gateway_log_tail_reconnects_total",
                    "reason" => "stream_ended"
                )
                .increment(1);
                debug!(
                    last_relayed = ?last_relayed,
                    "TailLog stream ended before the derivation was terminal; re-opening"
                );
                if backoff_or_terminal(&mut drain, config.reconnect_backoff).await {
                    return;
                }
            }
        }
    }
}

/// Sleep for `backoff`, waking early if the drain signal flips. Returns
/// `true` if the subscription should exit instead of re-opening (the
/// derivation went terminal while there was no stream to drain).
async fn backoff_or_terminal(drain: &mut watch::Receiver<bool>, backoff: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(backoff) => false,
        // `changed()` errors only when the sender is dropped (the
        // LogTailSet entry was removed by a re-dispatch replacement —
        // but that aborts this task outright, so the Err arm is
        // unreachable belt-and-suspenders).
        res = drain.changed() => res.is_err() || *drain.borrow(),
    }
}

/// Drive one connected stream until it ends, the grace expires, or the
/// output closes.
async fn drive_stream(
    mut stream: tonic::Streaming<TailLogChunk>,
    derivation_path: &str,
    out_tx: &mpsc::Sender<TaggedLogChunk>,
    drain: &mut watch::Receiver<bool>,
    last_relayed: &mut Option<u64>,
    terminal_grace: Duration,
) -> StreamEnd {
    // `None` until the derivation goes terminal; then a sleep that caps
    // how long we keep waiting for the stream's natural end.
    let mut grace: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = if *drain.borrow() {
        Some(Box::pin(tokio::time::sleep(terminal_grace)))
    } else {
        None
    };
    loop {
        tokio::select! {
            msg = stream.message() => match msg {
                Ok(Some(chunk)) => {
                    if let Some(tagged) = trim_chunk(chunk, derivation_path, *last_relayed) {
                        *last_relayed = Some(
                            tagged
                                .first_line_number
                                .saturating_add(tagged.lines.len() as u64)
                                .saturating_sub(1),
                        );
                        // Blocking send: a slow nix client backpressures
                        // this subscription (and, transitively, the
                        // store's reads on our behalf). While blocked the
                        // grace timer is not polled — acceptable, because
                        // a blocked send means the event loop is not
                        // consuming, which means the client write is the
                        // bottleneck and "exit promptly after terminal"
                        // has already lost to "deliver the lines at all".
                        if out_tx.send(tagged).await.is_err() {
                            return StreamEnd::OutputClosed;
                        }
                    }
                }
                // The store ends a follow stream when the ingest session
                // closes (the builder finished or reconnected elsewhere)
                // — that is not the same thing as "the derivation is
                // done", so before terminal it is a premature end. An
                // in-stream error (the store's grpc-web-compatible error
                // convention) is treated identically: retryable before
                // terminal, final after.
                Ok(None) => {
                    return if grace.is_some() { StreamEnd::Drained } else { StreamEnd::Premature };
                }
                Err(status) => {
                    debug!(code = ?status.code(), "TailLog stream error");
                    return if grace.is_some() { StreamEnd::Drained } else { StreamEnd::Premature };
                }
            },
            // The derivation went terminal while the stream is open:
            // arm the grace timer and keep draining. Guarded so the
            // branch is only polled until the timer is armed.
            res = drain.changed(), if grace.is_none() => {
                // Err = the sender (the LogTailSet entry) is gone; the
                // set aborts this task on removal so this is
                // unreachable, but exiting is the safe interpretation.
                if res.is_err() {
                    return StreamEnd::Drained;
                }
                if *drain.borrow() {
                    grace = Some(Box::pin(tokio::time::sleep(terminal_grace)));
                }
            }
            // The post-terminal grace expired with the stream still
            // open: stop waiting for its natural end.
            () = async { grace.as_mut().expect("guarded by the if").await }, if grace.is_some() => {
                debug!("post-terminal grace expired; closing the log tail");
                return StreamEnd::Drained;
            }
        }
    }
}

/// Drop the prefix of `chunk` that has already been relayed and tag it
/// with the derivation path. Returns `None` when nothing new remains.
///
/// The store's chunk granularity means a re-opened stream (and even the
/// first response of a fresh stream, which replays whole stored chunks)
/// can carry lines below the requested `since_line`; this trim is what
/// turns the store's at-least-once delivery into exactly-once on the
/// client's wire.
fn trim_chunk(
    chunk: TailLogChunk,
    derivation_path: &str,
    last_relayed: Option<u64>,
) -> Option<TaggedLogChunk> {
    if chunk.lines.is_empty() {
        return None;
    }
    let floor = last_relayed.map_or(0, |n| n.saturating_add(1));
    let first = chunk.first_line_number;
    let last = first.saturating_add(chunk.lines.len() as u64 - 1);
    if last < floor {
        return None;
    }
    let skip = usize::try_from(floor.saturating_sub(first)).unwrap_or(usize::MAX);
    if skip >= chunk.lines.len() {
        return None;
    }
    let mut lines = chunk.lines;
    if skip > 0 {
        lines.drain(..skip);
    }
    Some(TaggedLogChunk {
        derivation_path: derivation_path.to_string(),
        first_line_number: first.saturating_add(skip as u64),
        lines,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rio_proto::store::log_service_server::{LogService, LogServiceServer};
    use rio_proto::store::{AppendLogAck, AppendLogRequest, TailLogChunk, TailLogRequest};
    use rio_test_support::grpc::spawn_grpc_server;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::transport::Server;
    use tonic::{Request, Response, Status, Streaming};

    use super::{LogTailConfig, LogTailSet, TaggedLogChunk};

    // ------------------------------------------------------------------
    // The mock LogService
    // ------------------------------------------------------------------

    /// What one accepted `tail_log` call does after serving its scripted
    /// chunks.
    enum SessionEnd {
        /// End the stream cleanly (the ingest session closed).
        Close,
        /// Keep the stream open until the test drops the guard sender or
        /// the client disconnects. Models a quiet live build.
        Hold,
    }

    /// One scripted `tail_log` response.
    struct SessionScript {
        chunks: Vec<TailLogChunk>,
        end: SessionEnd,
    }

    #[derive(Clone, Default)]
    struct MockTail {
        inner: Arc<MockTailInner>,
    }

    #[derive(Default)]
    struct MockTailInner {
        /// Every `TailLogRequest` received, in arrival order.
        requests: Mutex<Vec<TailLogRequest>>,
        /// Scripts consumed in order, one per `tail_log` call. An
        /// exhausted script list serves an empty close (so a stray
        /// re-subscription doesn't hang a test).
        scripts: Mutex<VecDeque<SessionScript>>,
        /// Senders keeping `Hold` sessions open. Dropping the mock (end
        /// of test) closes them.
        holds: Mutex<Vec<mpsc::Sender<Result<TailLogChunk, Status>>>>,
        /// Set while a `tail_log` call is being held open *before*
        /// serving its chunks (see `gate_next_session`). The test uses
        /// this to deterministically interleave "the subscription is
        /// open" with "the terminal signal fires".
        gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    }

    impl MockTail {
        fn push_script(&self, chunks: Vec<TailLogChunk>, end: SessionEnd) {
            self.inner
                .scripts
                .lock()
                .unwrap()
                .push_back(SessionScript { chunks, end });
        }

        /// The next `tail_log` call records its request, then parks
        /// until the returned `Notify` is notified, then serves its
        /// script. Lets a test assert "the request arrived" and flip
        /// external state before any chunk is served.
        fn gate_next_session(&self) -> Arc<tokio::sync::Notify> {
            let notify = Arc::new(tokio::sync::Notify::new());
            *self.inner.gate.lock().unwrap() = Some(notify.clone());
            notify
        }

        fn requests(&self) -> Vec<TailLogRequest> {
            self.inner.requests.lock().unwrap().clone()
        }

        fn request_count(&self) -> usize {
            self.inner.requests.lock().unwrap().len()
        }
    }

    #[tonic::async_trait]
    impl LogService for MockTail {
        type AppendLogStream = ReceiverStream<Result<AppendLogAck, Status>>;
        type TailLogStream = ReceiverStream<Result<TailLogChunk, Status>>;

        async fn append_log(
            &self,
            _request: Request<Streaming<AppendLogRequest>>,
        ) -> Result<Response<Self::AppendLogStream>, Status> {
            Err(Status::unimplemented("mock tail does not accept appends"))
        }

        async fn tail_log(
            &self,
            request: Request<TailLogRequest>,
        ) -> Result<Response<Self::TailLogStream>, Status> {
            let req = request.into_inner();
            self.inner.requests.lock().unwrap().push(req);
            let gate = self.inner.gate.lock().unwrap().take();
            let script = self
                .inner
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(SessionScript {
                    chunks: vec![],
                    end: SessionEnd::Close,
                });
            let (tx, rx) = mpsc::channel(64);
            let inner = self.inner.clone();
            tokio::spawn(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                for chunk in script.chunks {
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                match script.end {
                    SessionEnd::Close => {}
                    SessionEnd::Hold => {
                        // Park the sender so the stream stays open until
                        // the mock is dropped or the client disconnects.
                        inner.holds.lock().unwrap().push(tx);
                    }
                }
            });
            Ok(Response::new(ReceiverStream::new(rx)))
        }
    }

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    const DRV: &str = "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-tailed.drv";
    const EXEC_A: &str = "01900000-0000-7000-8000-00000000000a";
    const EXEC_B: &str = "01900000-0000-7000-8000-00000000000b";

    /// Test-scale timings: large enough that a loaded runner still
    /// observes the grace *window* (lines served inside it are
    /// relayed), small enough that the grace-expiry test stays fast.
    fn test_config() -> LogTailConfig {
        LogTailConfig {
            reconnect_backoff: Duration::from_millis(50),
            terminal_grace: Duration::from_millis(400),
        }
    }

    struct Harness {
        mock: MockTail,
        set: LogTailSet,
        out_rx: mpsc::Receiver<TaggedLogChunk>,
        _server: tokio::task::JoinHandle<()>,
    }

    async fn harness() -> Harness {
        let mock = MockTail::default();
        let router = Server::builder().add_service(LogServiceServer::new(mock.clone()));
        let (addr, server) = spawn_grpc_server(router).await;
        let client = rio_proto::LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect to the mock LogService");
        let (set, out_rx) = LogTailSet::with_config(client, test_config());
        Harness {
            mock,
            set,
            out_rx,
            _server: server,
        }
    }

    fn chunk(first_line: u64, n: usize) -> TailLogChunk {
        TailLogChunk {
            exec_id: EXEC_A.to_string(),
            lines: (0..n)
                .map(|i| format!("line-{:05}", first_line + i as u64).into_bytes())
                .collect(),
            first_line_number: first_line,
            is_complete: false,
        }
    }

    /// Receive tagged chunks from `out_rx` until `n` total lines have
    /// arrived (or ~2 s elapse). Returns the flattened
    /// `(line_number, text)` pairs in arrival order.
    async fn recv_lines(rx: &mut mpsc::Receiver<TaggedLogChunk>, n: usize) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        while out.len() < n {
            let tagged = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out after {} of {n} lines", out.len()))
                .expect("output channel closed early");
            for (i, line) in tagged.lines.iter().enumerate() {
                out.push((
                    tagged.first_line_number + i as u64,
                    // Test fixture lines are always UTF-8 (the `chunk`
                    // helper formats them); a hard failure here is a
                    // test bug, not a display concern.
                    String::from_utf8(line.clone()).expect("test lines are UTF-8"),
                ));
            }
        }
        out
    }

    /// Poll `cond` every 10 ms until it returns true or ~2 s elapse.
    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    /// Rule 1 + 5: a `Started` opens a `TailLog(since_line: 0,
    /// follow: true)` subscription and the served chunks arrive on the
    /// output channel tagged with the derivation path, in order.
    #[tokio::test]
    async fn subscribes_on_started_and_relays_lines() {
        let mut h = harness().await;
        h.mock
            .push_script(vec![chunk(0, 3), chunk(3, 2)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 5).await;
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4],
            "lines arrive in order with contiguous numbering"
        );
        assert_eq!(lines[0].1, "line-00000");
        assert_eq!(lines[4].1, "line-00004");

        let reqs = h.mock.requests();
        assert_eq!(reqs.len(), 1, "exactly one subscription opened");
        assert_eq!(reqs[0].derivation, DRV);
        assert_eq!(reqs[0].exec_id, EXEC_A);
        assert_eq!(reqs[0].since_line, 0);
        assert!(reqs[0].follow, "the live tail is a follow subscription");

        h.set.abort_all();
    }

    // r[verify store.log.tail-reconnect]
    /// Rule 2: a stream that ends while the derivation is not terminal
    /// is re-opened with `since_line = last_relayed + 1`, and lines the
    /// store resends below that cursor (chunk granularity) are not
    /// relayed twice.
    #[tokio::test]
    async fn resubscribes_on_premature_stream_end_with_cursor() {
        let mut h = harness().await;
        // Session 1 serves lines 0..50 and closes (a store deploy).
        h.mock.push_script(vec![chunk(0, 50)], SessionEnd::Close);
        // Session 2 resends the whole containing chunk (lines 0..60) —
        // the store's chunk granularity means a since_line=50 read can
        // legally return lines starting at 0.
        h.mock.push_script(vec![chunk(0, 60)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);

        let lines = recv_lines(&mut h.out_rx, 60).await;
        let numbers: Vec<u64> = lines.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            numbers,
            (0..60).collect::<Vec<u64>>(),
            "every line exactly once, in order, across the reconnect"
        );

        let reqs = h.mock.requests();
        assert_eq!(reqs.len(), 2, "the subscription re-opened once");
        assert_eq!(reqs[0].since_line, 0);
        assert_eq!(
            reqs[1].since_line, 50,
            "the re-open resumes at last_relayed + 1"
        );
        assert_eq!(reqs[1].exec_id, EXEC_A, "same execution across re-opens");

        h.set.abort_all();
    }

    /// Rule 3: a second `Started` with a different exec_id replaces the
    /// subscription (new request at since_line=0 for the new exec); a
    /// second `Started` with the same exec_id is a duplicate and does
    /// nothing.
    #[tokio::test]
    async fn replaces_subscription_on_redispatch() {
        let mut h = harness().await;
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Hold);
        h.mock.push_script(vec![chunk(0, 1)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the first subscription to open", || {
            h.mock.request_count() == 1
        })
        .await;
        // Drain the first session's lines so the channel ordering in the
        // assertion below is unambiguous.
        let _ = recv_lines(&mut h.out_rx, 2).await;

        // Duplicate Started, same exec: no new subscription.
        h.set.on_started(DRV, EXEC_A);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            h.mock.request_count(),
            1,
            "a duplicate Started with the same exec_id must not re-subscribe"
        );

        // Re-dispatch: new exec_id.
        h.set.on_started(DRV, EXEC_B);
        wait_for("the replacement subscription to open", || {
            h.mock.request_count() == 2
        })
        .await;

        let reqs = h.mock.requests();
        assert_eq!(
            reqs[1].exec_id, EXEC_B,
            "the new subscription is for the new execution"
        );
        assert_eq!(
            reqs[1].since_line, 0,
            "a re-dispatched execution's log starts over at line 0"
        );

        // An empty exec_id never subscribes (rule 1's guard).
        h.set.on_started("/nix/store/other-thing.drv", "");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            h.mock.request_count(),
            2,
            "an empty exec_id must not open a subscription"
        );

        h.set.abort_all();
    }

    /// Rule 4, first half: the terminal signal arriving while the store
    /// still has lines buffered does not race them away — they are
    /// relayed before the subscription closes.
    #[tokio::test]
    async fn drains_to_natural_end_on_terminal() {
        let mut h = harness().await;
        // The mock parks the session before serving anything, so the
        // interleaving "subscription open → terminal signal → lines
        // served" is deterministic, not a sleep race.
        let gate = h.mock.gate_next_session();
        h.mock.push_script(vec![chunk(0, 50)], SessionEnd::Close);

        h.set.on_started(DRV, EXEC_A);
        wait_for("the subscription to open", || h.mock.request_count() == 1).await;

        // The derivation goes terminal while the mock still holds all
        // 50 lines.
        h.set.on_terminal(DRV);
        gate.notify_one();

        let lines = recv_lines(&mut h.out_rx, 50).await;
        assert_eq!(
            lines.len(),
            50,
            "every buffered line is relayed after the terminal signal"
        );
        assert_eq!(lines.last().unwrap().0, 49);

        h.set.abort_all();
    }

    /// Rule 4, second half: a stream that never ends after the terminal
    /// signal is cut off at the post-terminal grace cap (the task
    /// exits; it does not wait forever).
    #[tokio::test]
    async fn terminal_grace_cap_closes_a_stuck_stream() {
        let mut h = harness().await;
        // The session serves 2 lines and then holds the stream open
        // forever (a wedged store replica / a follow stream whose
        // ingest session never closes).
        h.mock.push_script(vec![chunk(0, 2)], SessionEnd::Hold);

        h.set.on_started(DRV, EXEC_A);
        let _ = recv_lines(&mut h.out_rx, 2).await;

        h.set.on_terminal(DRV);

        // The task must exit within the grace (400 ms test-scale) plus
        // slack — NOT hang forever on the held-open stream.
        let handle = h
            .set
            .tasks
            .get(DRV)
            .expect("the subscription is still tracked")
            .task
            .abort_handle();
        wait_for("the subscription task to exit at the grace cap", || {
            handle.is_finished()
        })
        .await;

        h.set.abort_all();
    }

    /// Rule 1's guard as its own test: a `Started` with an empty
    /// exec_id opens no subscription at all.
    #[tokio::test]
    async fn empty_exec_id_does_not_subscribe() {
        let h = harness().await;
        let mut set = h.set;
        set.on_started(DRV, "");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.mock.request_count(),
            0,
            "no TailLog request for an execution-less Started"
        );
        assert!(set.tasks.is_empty(), "no task spawned");
    }
}
