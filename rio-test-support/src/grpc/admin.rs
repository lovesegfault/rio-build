//! Minimal [`AdminService`] mock for CLI smoke tests.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rio_proto::types;
use rio_proto::{AdminService, AdminServiceServer};

use super::spawn::spawn_grpc_server;

/// One scripted `get_derivation_logs` outcome, consumed FIFO per call
/// from [`MockAdmin::log_script`]. Lets failure-handling clients
/// rehearse the server shapes the real scheduler produces: not-leader
/// delivered in-stream (grpc-web compatibility — leader-gated stream
/// handlers return `Ok(stream)` whose first item is the error status,
/// see the scheduler's `logs::err_stream`), a peer that stops
/// responding, and a stream that breaks mid-transfer.
#[derive(Debug)]
pub enum LogScript {
    /// Never respond: the handler parks forever. Models an ungracefully
    /// dead peer for client-side deadline tests (the client's RST on
    /// timeout drops the parked future).
    Hang,
    /// `Ok(stream)` whose first and only item is `Err(status)` — the
    /// grpc-web error shape the scheduler uses for not-leader and other
    /// pre-stream failures.
    ErrFirst(Status),
    /// One `is_complete = true` chunk carrying `lines`, then EOF.
    Complete { lines: Vec<Vec<u8>> },
    /// One `is_complete = false` chunk carrying `lines`, then
    /// `Err(status)` — a stream that breaks after data already flowed.
    BreakAfter { lines: Vec<Vec<u8>>, status: Status },
}

/// Minimal AdminService mock: returns empty-but-valid responses for all
/// unary RPCs. Configurable knobs: `log_incomplete` and the per-call
/// `log_script`; otherwise this is for CLI smoke tests that just need
/// "connects + non-error exit", not for asserting on scheduler state
/// (that's what the real AdminServiceImpl tests in
/// rio-scheduler/src/admin/tests.rs are for).
///
/// Streaming RPCs (GetDerivationLogs, TriggerGC) return a stream with a
/// single terminal message so the client's drain loop exits cleanly.
// r[impl ts.mock.admin]
#[derive(Clone, Default)]
pub struct MockAdmin {
    /// Every ClearPoison drv_hash received. For asserting the CLI
    /// passed through the positional arg correctly.
    pub clear_poison_calls: Arc<RwLock<Vec<String>>>,
    /// When set, the single `get_derivation_logs` chunk carries
    /// `is_complete = false` — the shape `try_s3` produces for a
    /// `.partial` blob and `try_ring_buffer` for a still-running build.
    /// Drives the CLI's "(log incomplete …)" stderr warning. Unset (the
    /// default) = complete chunk, clean terminal, no warning.
    pub log_incomplete: Arc<AtomicBool>,
    /// Scripted `get_derivation_logs` outcomes, popped front per call.
    /// Empty (the default) = the original single mock chunk, so smoke
    /// tests are unaffected.
    pub log_script: Arc<Mutex<VecDeque<LogScript>>>,
    /// Total `get_derivation_logs` calls received, including scripted
    /// ones. Retry tests assert on this to distinguish "client retried
    /// against the server" from "client failed locally".
    pub log_calls: Arc<AtomicUsize>,
    /// Number of upcoming `list_poisoned` calls to park forever, consumed
    /// one per call (0, the default, answers immediately). Models an
    /// ungracefully dead peer on a UNARY RPC: the parked handler never
    /// responds, so only the client's own deadline can unblock the
    /// caller — there is no race against a fast server reply.
    pub poisoned_hangs: Arc<AtomicUsize>,
    /// Total `list_poisoned` calls received, including parked ones.
    pub poisoned_calls: Arc<AtomicUsize>,
}

impl MockAdmin {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one scripted `get_derivation_logs` outcome (FIFO).
    pub fn push_log_script(&self, script: LogScript) {
        self.log_script.lock().unwrap().push_back(script);
    }

    /// Build the response stream for one [`LogScript`] entry.
    ///
    /// All sends happen before the receiver is returned, so the channel
    /// capacity must cover the largest script (chunk + trailing error =
    /// 2 items) or the handler would deadlock against its own buffer.
    async fn scripted_logs(
        &self,
        script: LogScript,
    ) -> Result<
        Response<tokio_stream::wrappers::ReceiverStream<Result<types::DerivationLogChunk, Status>>>,
        Status,
    > {
        let chunk = |lines: Vec<Vec<u8>>, is_complete: bool| types::DerivationLogChunk {
            derivation_path: "/nix/store/mock.drv".into(),
            exec_id: String::new(),
            lines,
            first_line_number: 0,
            is_complete,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        match script {
            LogScript::Hang => std::future::pending().await,
            LogScript::ErrFirst(status) => {
                let _ = tx.send(Err(status)).await;
            }
            LogScript::Complete { lines } => {
                let _ = tx.send(Ok(chunk(lines, true))).await;
            }
            LogScript::BreakAfter { lines, status } => {
                let _ = tx.send(Ok(chunk(lines, false))).await;
                let _ = tx.send(Err(status)).await;
            }
        }
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }
}

// build.rs-generated: defines the `mock_admin_default_methods!` macro
// (expands to default-stub unary method bodies). Must appear BEFORE
// the AdminService impl (macro_rules is textually scoped —
// definition-before-use within a module).
include!(concat!(env!("OUT_DIR"), "/mock_admin_generated.rs"));

#[tonic::async_trait]
impl AdminService for MockAdmin {
    type GetDerivationLogsStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::DerivationLogChunk, Status>>;
    type TriggerGCStream =
        tokio_stream::wrappers::ReceiverStream<Result<types::GcProgress, Status>>;

    // ─── Manual methods: non-default behavior ───────────────────────────
    //
    // These five are NOT generated by build.rs (they're in MANUAL_METHODS
    // there). Streaming RPCs need a non-empty stream body — the CLI drain
    // loop drains to EOF and warns if the terminal frame isn't
    // `is_complete=true`. The custom unaries record/echo for smoke-test
    // assertions or park for client deadline tests. If you add custom
    // behavior to a currently-generated method, move its proto name into
    // MANUAL_METHODS in build.rs and write the body here.

    async fn get_derivation_logs(
        &self,
        _: Request<types::GetDerivationLogsRequest>,
    ) -> Result<Response<Self::GetDerivationLogsStream>, Status> {
        self.log_calls.fetch_add(1, Ordering::SeqCst);
        // Scripted outcome, when one is queued (guard dropped before the
        // await in scripted_logs — Hang parks the handler forever).
        let script = self.log_script.lock().unwrap().pop_front();
        if let Some(script) = script {
            return self.scripted_logs(script).await;
        }
        // One chunk with one line, then EOF. Real server requires
        // derivation_path non-empty; the mock accepts anything (smoke
        // test doesn't validate server-side argument handling).
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx
            .send(Ok(types::DerivationLogChunk {
                derivation_path: "/nix/store/mock.drv".into(),
                exec_id: String::new(),
                lines: vec![b"mock log line".to_vec()],
                first_line_number: 0,
                // SeqCst, not Relaxed: the store happens on the test
                // thread before the subprocess spawn and the load
                // happens on a tonic worker when the RPC arrives, so
                // Relaxed cannot misfire in practice — but this is test
                // infrastructure where the ordering costs nothing and
                // SeqCst removes the "is Relaxed sufficient here?"
                // question for every future reader.
                is_complete: !self.log_incomplete.load(Ordering::SeqCst),
            }))
            .await;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn trigger_gc(
        &self,
        _: Request<types::GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        // Single is_complete=true frame so the CLI's drain loop sees
        // a clean terminal and doesn't emit the "closed without
        // is_complete" warning.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx
            .send(Ok(types::GcProgress {
                is_complete: true,
                ..Default::default()
            }))
            .await;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn list_poisoned(
        &self,
        _: Request<()>,
    ) -> Result<Response<types::ListPoisonedResponse>, Status> {
        self.poisoned_calls.fetch_add(1, Ordering::SeqCst);
        // Consume one hang token if any remain; a parked handler is
        // dropped when the client cancels (deadline → RST_STREAM).
        let hang = self
            .poisoned_hangs
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if hang {
            std::future::pending().await
        }
        Ok(Response::new(types::ListPoisonedResponse::default()))
    }

    async fn clear_poison(
        &self,
        request: Request<types::ClearPoisonRequest>,
    ) -> Result<Response<types::ClearPoisonResponse>, Status> {
        let req = request.into_inner();
        self.clear_poison_calls
            .write()
            .unwrap()
            .push(req.derivation_hash);
        Ok(Response::new(types::ClearPoisonResponse { cleared: false }))
    }

    async fn create_tenant(
        &self,
        request: Request<types::CreateTenantRequest>,
    ) -> Result<Response<types::CreateTenantResponse>, Status> {
        // Echo back a TenantInfo so the CLI's "returned no TenantInfo"
        // check passes.
        let req = request.into_inner();
        Ok(Response::new(types::CreateTenantResponse {
            tenant: Some(types::TenantInfo {
                tenant_id: "00000000-0000-0000-0000-000000000000".into(),
                tenant_name: req.tenant_name,
                ..Default::default()
            }),
        }))
    }

    // ─── Generated methods: Default::default() stubs ────────────────────
    //
    // One async fn per unary RPC NOT in MANUAL_METHODS, each returning
    // Ok(Response::new(<T>::default())). Generated by build.rs from
    // admin.proto so adding a new unary RPC requires zero Rust changes.
    // See build.rs's doc comment for the full story.
    mock_admin_default_methods!();
}

/// Spawn a MockAdmin on an ephemeral port. Returns `(admin, addr, handle)`.
///
/// Plaintext — no TLS. For rio-cli smoke tests: run with no
///
pub async fn spawn_mock_admin()
-> anyhow::Result<(MockAdmin, SocketAddr, tokio::task::JoinHandle<()>)> {
    let admin = MockAdmin::new();
    let router = Server::builder().add_service(AdminServiceServer::new(admin.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((admin, addr, handle))
}
