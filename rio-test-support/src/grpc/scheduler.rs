//! Configurable [`SchedulerService`] mock.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rio_proto::types;
use rio_proto::{SchedulerService, SchedulerServiceServer};

use super::spawn::spawn_grpc_server;

/// What `SubmitBuild` does on the next call.
///
/// The three variants are mutually exclusive — previously encoded as a flat
/// struct of mostly-dead `Option`s where invalid combinations were silently
/// resolved by field-probe order. The enum makes the mode explicit.
// r[impl ts.mock.scheduler-outcome+2]
#[derive(Clone)]
pub enum SubmitOutcome {
    /// Immediately return `Err(Status::new(code, ...))`.
    Error(tonic::Code),
    /// Send these events verbatim, auto-filling empty `build_id` / zero
    /// `sequence`, then close the stream.
    Scripted {
        events: Vec<types::BuildEvent>,
        /// Inject `Err(Status)` after sending N events. For gateway
        /// reconnect-on-transport-error tests.
        error_after_n: Option<(usize, tonic::Code)>,
        /// Sleep between each event. For mid-opcode-disconnect tests:
        /// gives the test time to drop the client stream while the
        /// scheduler is still sending Log events, so the gateway's next
        /// stderr.log write gets BrokenPipe → `StreamProcessError::Wire`.
        /// Without this, scripted events flood the mpsc channel
        /// synchronously and the race between client-drop and stream-EOF
        /// is nondeterministic.
        interval: Option<std::time::Duration>,
    },
    /// Send `BuildStarted`, then either `BuildCompleted`, close the stream
    /// without a terminal event, or hang for 3600s (default).
    Simple {
        send_completed: bool,
        close_early: bool,
    },
}

impl Default for SubmitOutcome {
    /// `BuildStarted` then hang — the gateway blocks on build events
    /// until the client disconnects.
    fn default() -> Self {
        Self::Simple {
            send_completed: false,
            close_early: false,
        }
    }
}

impl SubmitOutcome {
    /// `BuildStarted` → `BuildCompleted`.
    pub fn completed() -> Self {
        Self::Simple {
            send_completed: true,
            close_early: false,
        }
    }

    /// `BuildStarted` → close stream (no terminal event). Simulates
    /// scheduler disconnect mid-build.
    pub fn close_early() -> Self {
        Self::Simple {
            send_completed: false,
            close_early: true,
        }
    }

    /// Send `events` verbatim (no error injection, no inter-event delay).
    pub fn scripted(events: Vec<types::BuildEvent>) -> Self {
        Self::Scripted {
            events,
            error_after_n: None,
            interval: None,
        }
    }
}

/// What `WatchBuild` does on the next call. Orthogonal to [`SubmitOutcome`].
#[derive(Clone, Default)]
pub struct WatchOutcome {
    /// If Some, return a stream of these events (same `build_id`/`sequence`
    /// auto-fill as scripted submit), honoring `since_sequence`. For gateway
    /// reconnect tests: SubmitBuild stream errors → gateway calls WatchBuild
    /// → this stream delivers the remainder.
    pub scripted_events: Option<Vec<types::BuildEvent>>,
    /// How many times `watch_build` fails with `Unavailable` before
    /// succeeding (or returning `not_found` if `scripted_events` is `None`).
    /// Decremented on each call. For reconnect-exhausted tests.
    pub fail_count: Arc<AtomicU32>,
}

/// Mock scheduler that records SubmitBuild + CancelBuild calls and has
/// independently-configurable SubmitBuild / WatchBuild behavior.
#[derive(Clone, Default)]
pub struct MockScheduler {
    pub submit: Arc<RwLock<SubmitOutcome>>,
    pub watch: Arc<RwLock<WatchOutcome>>,
    /// Full SubmitBuild requests received (for inspecting DAG contents).
    pub submit_calls: Arc<RwLock<Vec<types::SubmitBuildRequest>>>,
    /// CancelBuild calls received: (build_id, reason).
    pub cancel_calls: Arc<RwLock<Vec<(String, String)>>>,
    /// WatchBuild calls received: (build_id, since_sequence). For asserting
    /// the gateway's reconnect sent the correct resume point (see
    /// `r[gw.reconnect.since-seq]`).
    pub watch_calls: Arc<RwLock<Vec<(String, u64)>>>,
    /// `x-rio-tenant-token` value on each WatchBuild call (`None` = absent).
    /// For `r[gw.jwt.propagate]` — catches bare-struct call sites that
    /// bypass `with_jwt` on the reconnect path.
    pub watch_metadata: Arc<RwLock<Vec<Option<String>>>>,
    /// `x-rio-tenant-token` value on each CancelBuild call (`None` = absent).
    pub cancel_metadata: Arc<RwLock<Vec<Option<String>>>>,
}

/// Extract `x-rio-tenant-token` from request metadata as `Option<String>`.
fn tenant_token<T>(req: &Request<T>) -> Option<String> {
    req.metadata()
        .get(rio_proto::TENANT_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

impl MockScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_submit_outcome(&self, outcome: SubmitOutcome) {
        *self.submit.write().unwrap() = outcome;
    }

    pub fn set_watch_outcome(&self, outcome: WatchOutcome) {
        *self.watch.write().unwrap() = outcome;
    }
}

const TEST_BUILD_ID: &str = "test-build-00000000-1111-2222-3333-444444444444";

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

        let outcome = self.submit.read().unwrap().clone();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let build_id = TEST_BUILD_ID.to_string();

        match outcome {
            SubmitOutcome::Error(code) => {
                return Err(Status::new(code, "mock scheduler error"));
            }
            SubmitOutcome::Scripted {
                events,
                error_after_n,
                interval,
            } => {
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
            }
            SubmitOutcome::Simple {
                send_completed,
                close_early,
            } => {
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

                    if send_completed {
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
                    } else if close_early {
                        // Drop tx immediately: stream ends without a terminal event.
                        drop(tx);
                    } else {
                        // Keep open so gateway blocks on build events until client disconnects.
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                });
            }
        }

        let mut resp = Response::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        resp.metadata_mut().insert(
            rio_proto::BUILD_ID_HEADER,
            TEST_BUILD_ID.parse().expect("test build_id is ASCII"),
        );
        Ok(resp)
    }

    type WatchBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::BuildEvent, Status>>;

    async fn watch_build(
        &self,
        request: Request<types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        self.watch_metadata
            .write()
            .unwrap()
            .push(tenant_token(&request));
        let req = request.into_inner();
        let since = req.since_sequence;
        self.watch_calls
            .write()
            .unwrap()
            .push((req.build_id, since));

        let watch = self.watch.read().unwrap().clone();

        // Injected-failure countdown: decrement and return Unavailable while > 0.
        // For reconnect-exhausted tests (gateway retries watch_build up to
        // MAX_RECONNECT times before giving up).
        if watch
            .fail_count
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
        if let Some(events) = watch.scripted_events {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let build_id = TEST_BUILD_ID.to_string();
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
        self.cancel_metadata
            .write()
            .unwrap()
            .push(tenant_token(&request));
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
