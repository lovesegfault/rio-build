//! Configurable [`SchedulerService`] mock.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rio_proto::types;
use rio_proto::{SchedulerService, SchedulerServiceServer};

use super::spawn::spawn_grpc_server;

/// Configurable scheduler behavior for a single SubmitBuild stream.
#[derive(Clone, Default)]
pub struct MockSchedulerOutcome {
    /// If set, SubmitBuild immediately fails with this status code.
    pub submit_error: Option<tonic::Code>,
    /// If set, SubmitBuild sends BuildCompleted after BuildStarted.
    pub send_completed: bool,
    /// If set, the stream is closed immediately after BuildStarted (no
    /// terminal event). Simulates scheduler disconnect mid-build.
    pub close_stream_early: bool,
    /// If Some, send these events verbatim (ignoring the bool flags above)
    /// then close the stream. Empty build_id / zero sequence are
    /// auto-populated so tests only need to set the `event` oneof.
    pub scripted_events: Option<Vec<types::BuildEvent>>,
    /// Inject `Err(Status)` into the SubmitBuild stream after sending N
    /// scripted events. Only applies in scripted mode. For gateway
    /// reconnect-on-transport-error tests.
    pub error_after_n: Option<(usize, tonic::Code)>,
    /// Sleep this long between each scripted event. Only applies in
    /// scripted mode. For mid-opcode-disconnect tests: gives the test
    /// time to drop the client stream while the scheduler is still
    /// sending Log events, so the gateway's next stderr.log write
    /// gets BrokenPipe → `StreamProcessError::Wire`. Without this,
    /// scripted events flood the mpsc channel synchronously and the
    /// race between client-drop and stream-EOF is nondeterministic.
    pub scripted_event_interval: Option<std::time::Duration>,
    /// If Some, WatchBuild returns a stream of these events (same
    /// build_id/sequence auto-fill as SubmitBuild scripted mode).
    /// For gateway reconnect tests: SubmitBuild stream errors →
    /// gateway calls WatchBuild → this stream delivers the remainder.
    pub watch_scripted_events: Option<Vec<types::BuildEvent>>,
    /// How many times watch_build fails with Unavailable before
    /// succeeding (or returning not_found if watch_scripted_events
    /// is None). Decremented on each call. For reconnect-exhausted tests.
    pub watch_fail_count: Arc<AtomicU32>,
}

/// Mock scheduler that records SubmitBuild + CancelBuild calls and has a
/// configurable outcome for SubmitBuild streams.
#[derive(Clone, Default)]
pub struct MockScheduler {
    pub outcome: Arc<RwLock<MockSchedulerOutcome>>,
    /// Full SubmitBuild requests received (for inspecting DAG contents).
    pub submit_calls: Arc<RwLock<Vec<types::SubmitBuildRequest>>>,
    /// CancelBuild calls received: (build_id, reason).
    pub cancel_calls: Arc<RwLock<Vec<(String, String)>>>,
    /// WatchBuild calls received: (build_id, since_sequence). For asserting
    /// the gateway's reconnect sent the correct resume point (see
    /// `r[gw.reconnect.since-seq]`).
    pub watch_calls: Arc<RwLock<Vec<(String, u64)>>>,
}

impl MockScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_outcome(&self, outcome: MockSchedulerOutcome) {
        *self.outcome.write().unwrap() = outcome;
    }
}

#[tonic::async_trait]
impl SchedulerService for MockScheduler {
    type SubmitBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildEvent, Status>>;

    async fn submit_build(
        &self,
        request: Request<types::SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        let req = request.into_inner();
        self.submit_calls.write().unwrap().push(req);

        let outcome = self.outcome.read().unwrap().clone();
        if let Some(code) = outcome.submit_error {
            return Err(Status::new(code, "mock scheduler error"));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let build_id = "test-build-00000000-1111-2222-3333-444444444444".to_string();
        // Clone before any spawn moves build_id.
        let build_id_for_header = build_id.clone();

        // Scripted mode: send events verbatim, auto-fill build_id/sequence, close.
        if let Some(events) = outcome.scripted_events {
            let error_after_n = outcome.error_after_n;
            let interval = outcome.scripted_event_interval;
            let n_events = events.len();
            tokio::spawn(async move {
                for (seq, mut ev) in events.into_iter().enumerate() {
                    if let Some(d) = interval {
                        tokio::time::sleep(d).await;
                    }
                    // Error injection: after sending N events, send an Err(Status)
                    // into the stream. Gateway's process_build_events maps this
                    // to StreamProcessError::Transport → triggers the reconnect loop.
                    if let Some((n, code)) = error_after_n
                        && seq == n
                    {
                        let _ = tx
                            .send(Err(Status::new(code, "mock: injected stream error")))
                            .await;
                        return;
                    }
                    if ev.build_id.is_empty() {
                        ev.build_id = build_id.clone();
                    }
                    if ev.sequence == 0 {
                        ev.sequence = (seq as u64) + 1;
                    }
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
                // If error_after_n >= events.len(), fire after the last event.
                if let Some((n, code)) = error_after_n
                    && n >= n_events
                {
                    let _ = tx
                        .send(Err(Status::new(code, "mock: injected stream error")))
                        .await;
                }
                // tx drops → stream ends
            });
            let mut resp = Response::new(tokio_stream::wrappers::ReceiverStream::new(rx));
            resp.metadata_mut().insert(
                rio_proto::BUILD_ID_HEADER,
                build_id_for_header.parse().expect("test build_id is ASCII"),
            );
            return Ok(resp);
        }

        tokio::spawn(async move {
            let _ = tx
                .send(Ok(types::BuildEvent {
                    build_id: build_id.clone(),
                    sequence: 1,
                    timestamp: None,
                    event: Some(types::build_event::Event::Started(types::BuildStarted {
                        total_derivations: 1,
                        cached_derivations: 0,
                    })),
                }))
                .await;

            if outcome.send_completed {
                let _ = tx
                    .send(Ok(types::BuildEvent {
                        build_id: build_id.clone(),
                        sequence: 2,
                        timestamp: None,
                        event: Some(types::build_event::Event::Completed(
                            types::BuildCompleted {
                                output_paths: vec!["/nix/store/zzz-output".into()],
                            },
                        )),
                    }))
                    .await;
            } else if outcome.close_stream_early {
                // Drop tx immediately: stream ends without a terminal event.
                drop(tx);
            } else {
                // Keep open so gateway blocks on build events until client disconnects.
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });

        let mut resp = Response::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        resp.metadata_mut().insert(
            rio_proto::BUILD_ID_HEADER,
            build_id_for_header.parse().expect("test build_id is ASCII"),
        );
        Ok(resp)
    }

    type WatchBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildEvent, Status>>;

    async fn watch_build(
        &self,
        request: Request<types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        let req = request.into_inner();
        let since = req.since_sequence;
        self.watch_calls
            .write()
            .unwrap()
            .push((req.build_id, since));

        let outcome = self.outcome.read().unwrap().clone();

        // Injected-failure countdown: decrement and return Unavailable while > 0.
        // For reconnect-exhausted tests (gateway retries watch_build up to
        // MAX_RECONNECT times before giving up).
        if outcome
            .watch_fail_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(Status::unavailable("mock: injected watch_build failure"));
        }

        // Scripted WatchBuild stream — same auto-fill pattern as SubmitBuild.
        // HONORS since_sequence: events with sequence ≤ `since` are skipped,
        // mirroring rio-scheduler's build_event_log replay. Auto-fill runs
        // FIRST (so `sequence: 0` in a scripted event becomes `(idx+1)`
        // before the filter checks it), then the filter compares the
        // FINAL sequence value against `since`.
        if let Some(events) = outcome.watch_scripted_events {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let build_id = "test-build-00000000-1111-2222-3333-444444444444".to_string();
            tokio::spawn(async move {
                for (seq, mut ev) in events.into_iter().enumerate() {
                    if ev.build_id.is_empty() {
                        ev.build_id = build_id.clone();
                    }
                    if ev.sequence == 0 {
                        ev.sequence = (seq as u64) + 1;
                    }
                    // Real scheduler: strictly-greater-than filter.
                    if ev.sequence <= since {
                        continue;
                    }
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            });
            return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )));
        }

        Err(Status::not_found("not implemented in mock"))
    }

    async fn query_build_status(
        &self,
        _request: Request<types::QueryBuildRequest>,
    ) -> Result<Response<types::BuildStatus>, Status> {
        Err(Status::not_found("not implemented in mock"))
    }

    async fn cancel_build(
        &self,
        request: Request<types::CancelBuildRequest>,
    ) -> Result<Response<types::CancelBuildResponse>, Status> {
        let req = request.into_inner();
        self.cancel_calls
            .write()
            .unwrap()
            .push((req.build_id, req.reason));
        Ok(Response::new(types::CancelBuildResponse {
            cancelled: true,
        }))
    }

    async fn resolve_tenant(
        &self,
        request: Request<rio_proto::scheduler::ResolveTenantRequest>,
    ) -> Result<Response<rio_proto::scheduler::ResolveTenantResponse>, Status> {
        // No test exercises this (the gateway tests that need a resolved
        // tenant talk to the real scheduler). Return the unknown-tenant
        // response so the gateway's graceful-degrade path applies.
        Err(Status::invalid_argument(format!(
            "unknown tenant: {}",
            request.into_inner().tenant_name
        )))
    }
}

/// Spawn a MockScheduler on an ephemeral port. Returns `(scheduler, addr, handle)`.
pub async fn spawn_mock_scheduler()
-> anyhow::Result<(MockScheduler, SocketAddr, tokio::task::JoinHandle<()>)> {
    let sched = MockScheduler::new();
    let router = Server::builder().add_service(SchedulerServiceServer::new(sched.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((sched, addr, handle))
}
