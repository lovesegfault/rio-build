//! In-process tonic server spawn helpers.

use std::net::SocketAddr;

use tokio_stream::StreamExt;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

use rio_proto::{ChunkServiceServer, StoreServiceServer};

use super::store::MockStore;

/// Spawn an in-process tonic server on a random port. Returns `(addr, handle)`.
///
/// Uses `yield_now()` for synchronization — the listener is already bound
/// and accepting before `spawn` returns, so a single yield is sufficient to
/// let the server task enter its accept loop. No `sleep` needed.
///
/// Accepts a prebuilt [`tonic::transport::server::Router`] so callers can
/// compose any number of services:
/// ```ignore
/// let router = Server::builder()
///     .add_service(FooServer::new(foo))
///     .add_service(BarServer::new(bar));
/// let (addr, handle) = spawn_grpc_server(router).await;
/// ```
pub async fn spawn_grpc_server(
    router: tonic::transport::server::Router,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("in-process gRPC server");
    });
    tokio::task::yield_now().await;
    (addr, handle)
}

/// Generic variant of [`spawn_grpc_server`] for routers with layers.
///
/// `Server::builder().layer(...)` changes `Router<Identity>` →
/// `Router<Stack<L, Identity>>`, which the non-generic
/// [`spawn_grpc_server`] can't accept. This variant passes the layer
/// parameter through to tonic's own generic `serve_with_incoming`.
///
/// ```ignore
/// let router = Server::builder()
///     .layer(InterceptorLayer::new(fake_interceptor))
///     .add_service(FooServer::new(foo));
/// let (addr, handle) = spawn_grpc_server_layered(router).await;
/// ```
///
/// The `where` clause mirrors tonic 0.14's `Router<L>::serve_with_incoming`
/// bounds — `L: Layer<Routes>` where `L::Service` is a tower `Service` over
/// `http::Request<tonic::body::Body>`. Callers don't spell the bounds;
/// inference carries them from the `.layer(...)` call site. If tonic's
/// bound changes in a minor bump, this compile-fails loudly rather than
/// silently diverging.
///
/// The non-generic [`spawn_grpc_server`] (≈ `L = Identity`) is kept as a
/// separate fn: Rust's default type parameters on functions are unstable,
/// and a `L = Identity` blanket would force existing callers to turbofish
/// or hit inference ambiguity.
///
/// `ResBody` is the layer's HTTP response body type — each layer can
/// transform the body (e.g., compression, tracing wrappers), so tonic's
/// generic `Router<L>` can't assume `tonic::body::Body`. Callers never
/// spell this: inference flows from `.layer(...)` through `L::Service`'s
/// `Response = http::Response<ResBody>` associated type. It's here only
/// to satisfy tonic's `serve_with_incoming` bound chain.
pub async fn spawn_grpc_server_layered<L, ResBody>(
    router: tonic::transport::server::Router<L>,
) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    L: tower::Layer<tonic::service::Routes> + Send + 'static,
    L::Service: tower::Service<http::Request<tonic::body::Body>, Response = http::Response<ResBody>>
        + Clone
        + Send
        + 'static,
    <L::Service as tower::Service<http::Request<tonic::body::Body>>>::Future: Send,
    <L::Service as tower::Service<http::Request<tonic::body::Body>>>::Error:
        Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    ResBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    ResBody::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        router
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("in-process gRPC server");
    });
    tokio::task::yield_now().await;
    (addr, handle)
}

/// Spawn a MockStore on an ephemeral port. Returns `(store, addr, handle)`.
pub async fn spawn_mock_store()
-> anyhow::Result<(MockStore, SocketAddr, tokio::task::JoinHandle<()>)> {
    let store = MockStore::new();
    let router = Server::builder()
        .add_service(StoreServiceServer::new(store.clone()))
        // dataplane2: ChunkService on the same port (mirrors the real
        // store, which serves both on 9002). Existing callers that
        // never touch chunk RPCs are unaffected.
        .add_service(ChunkServiceServer::new(store.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    Ok((store, addr, handle))
}

/// Spawn a MockStore + connect a StoreServiceClient in one call.
///
/// Extracts the spawn-then-connect combo that appears in worker upload
/// tests, executor input tests, gateway translate tests, and scheduler
/// coverage tests. Returns the JoinHandle so callers can abort/join.
pub async fn spawn_mock_store_with_client() -> anyhow::Result<(
    MockStore,
    rio_proto::StoreServiceClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
)> {
    let (store, addr, handle) = spawn_mock_store().await?;
    let client = rio_proto::client::connect_store(&addr.to_string()).await?;
    Ok((store, client, handle))
}

/// Spawn a MockStore over an in-process duplex transport (no real TCP).
///
/// Use this for `#[tokio::test(start_paused = true)]` tests. The regular
/// [`spawn_mock_store_with_client`] binds a real TCP socket, which makes
/// tokio's auto-advance fire `.timeout()` wrappers while kernel-side
/// accept/handshake are pending — spurious `DeadlineExceeded` on a
/// loaded CI runner (§2.7 `test-start-paused-real-tcp-spawn-blocking`).
///
/// The duplex halves are tokio tasks, so auto-advance sees them as
/// "not idle" while they're doing I/O. Same pattern as
/// `rio-builder/src/executor/daemon/stderr_loop.rs:559`.
///
/// No `JoinHandle` returned: the server task is fire-and-forget (dies
/// with the test). No port to clean up.
pub async fn spawn_mock_store_inproc() -> anyhow::Result<(
    MockStore,
    rio_proto::StoreServiceClient<tonic::transport::Channel>,
)> {
    use hyper_util::rt::TokioIo;
    use tonic::transport::Endpoint;

    let store = MockStore::new();
    let svc = StoreServiceServer::new(store.clone());

    // Channel of server-side duplex halves. Each client "connect" mints
    // a duplex pair, hands one half to the server via this channel.
    // Unbounded: connect is bounded by the test itself (1-3 connections
    // per test), no backpressure needed.
    //
    // Server side receives raw `DuplexStream` (tonic has a blanket
    // `Connected for DuplexStream` impl); only the client side needs
    // `TokioIo` wrapping (tonic's client transport expects hyper's
    // `Read`/`Write`, not tokio's `AsyncRead`/`AsyncWrite`).
    let (conn_tx, conn_rx) = tokio::sync::mpsc::unbounded_channel::<tokio::io::DuplexStream>();
    let incoming =
        tokio_stream::wrappers::UnboundedReceiverStream::new(conn_rx).map(Ok::<_, std::io::Error>);

    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .expect("in-process gRPC server");
    });
    tokio::task::yield_now().await;

    // Connector: on each poll, create a fresh duplex pair, ship the
    // server half, wrap the client half. URI is a dummy — tonic parses
    // it but the connector never uses it.
    //
    // 64 KiB duplex buffer: tonic's default HTTP/2 window is 64 KiB;
    // smaller than a NAR chunk (256 KiB) so the writer blocks briefly,
    // but that's fine under paused time (the blocked write is a tokio
    // task, not real I/O).
    let channel = Endpoint::try_from("http://inproc.mock")?
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let conn_tx = conn_tx.clone();
            async move {
                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                conn_tx
                    .send(server_io)
                    .map_err(|_| std::io::Error::other("inproc server dropped"))?;
                Ok::<_, std::io::Error>(TokioIo::new(client_io))
            }
        }))
        .await?;

    Ok((store, rio_proto::StoreServiceClient::new(channel)))
}
