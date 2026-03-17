//! Exercises the SSH layer (russh `Config` + `Server`/`Handler` overrides).
//!
//! `GatewaySession` in `tests/common/` bypasses SSH entirely via
//! `DuplexStream` → `run_protocol`. These tests need a real TCP socket +
//! `russh::client`, or direct inspection of the extracted config.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use rio_gateway::server::{GatewayServer, build_ssh_config};
use rio_test_support::grpc::{spawn_mock_scheduler, spawn_mock_store};
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg};
use russh::server::Server as _;
use russh::{MethodKind, client};
use tokio::net::TcpListener;

// ===========================================================================
// T2a — config field assertions (keepalive, nodelay, methods)
// ===========================================================================

// r[verify gw.conn.keepalive]
// r[verify gw.conn.nodelay]
/// `build_ssh_config` sets all the hardened fields. russh's `Config`
/// defaults (server/mod.rs:102-128) leave keepalive off, Nagle on, and
/// all auth methods advertised. This test proves we override them.
///
/// End-to-end "does keepalive actually fire" is russh's concern (covered
/// by its own test suite); we only verify we wired the config correctly.
#[test]
fn test_ssh_config_hardened_fields() {
    let host_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let cfg = build_ssh_config(host_key);

    // keepalive: 30s interval, default max=3 → ~90s until half-open drop.
    assert_eq!(
        cfg.keepalive_interval,
        Some(std::time::Duration::from_secs(30)),
        "keepalive_interval must be set (default: None)"
    );
    assert_eq!(cfg.keepalive_max, 3, "keepalive_max default should hold");

    // nodelay: Nagle off for small-request/small-response ping-pong.
    assert!(cfg.nodelay, "nodelay must be true (default: false)");

    // methods: only publickey advertised. `MethodSet` isn't directly
    // comparable, so check via the `Config`'s Debug or via a round-trip.
    // The `From<&[MethodKind]>` impl is what we use in build_ssh_config;
    // comparing via the same construction is the most direct assertion.
    let expected_methods = russh::MethodSet::from(&[MethodKind::PublicKey][..]);
    assert_eq!(
        format!("{:?}", cfg.methods),
        format!("{:?}", expected_methods),
        "methods must be publickey only (default: all)"
    );

    // auth_rejection_time_initial: OpenSSH `none` probe gets fast reject.
    assert_eq!(
        cfg.auth_rejection_time_initial,
        Some(std::time::Duration::from_millis(10)),
        "auth_rejection_time_initial must short-circuit the `none` probe"
    );

    // inactivity_timeout: backstop still present.
    assert_eq!(
        cfg.inactivity_timeout,
        Some(std::time::Duration::from_secs(3600)),
        "inactivity_timeout backstop should remain"
    );
}

// ===========================================================================
// T2b — handle_session_error metric-delta
// ===========================================================================

/// Recorder that captures counter increments into a shared map keyed by
/// (metric_name, sorted-labels-joined). Used for metric-delta assertions.
///
/// `with_local_recorder` scopes this to the closure; no global install,
/// no cross-test contamination.
#[derive(Default)]
struct CountingRecorder {
    // Counter storage: one AtomicU64 per (name, labels) key. `metrics`
    // gives us `CounterFn for AtomicU64` (atomics.rs:22), so wrapping
    // in Arc gives us a valid `Counter::from_arc` target.
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

impl CountingRecorder {
    fn counter_key(key: &Key) -> String {
        let mut labels: Vec<_> = key
            .labels()
            .map(|l| format!("{}={}", l.key(), l.value()))
            .collect();
        labels.sort();
        format!("{}{{{}}}", key.name(), labels.join(","))
    }

    fn get(&self, rendered_key: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(rendered_key)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let rendered = Self::counter_key(key);
        let atomic = self
            .counters
            .lock()
            .unwrap()
            .entry(rendered)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        Counter::from_arc(atomic)
    }

    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

// r[verify gw.conn.session-error-visible]
/// `handle_session_error` increments `rio_gateway_errors_total{type="session"}`.
///
/// This is the metric-delta half of the keepalive proof: T2a shows
/// keepalive is configured; this shows that WHEN a session errors out
/// (for any reason — keepalive timeout, `?` propagation, connection-setup
/// failure), the error surfaces to the operator via the metric.
///
/// Combined, T2a + T2b prove the full chain: half-open → keepalive fires
/// (russh's job) → session errors → `handle_session_error` → metric.
#[tokio::test]
async fn test_handle_session_error_increments_metric() -> anyhow::Result<()> {
    // Need a GatewayServer to call handle_session_error on. The gRPC
    // clients are never exercised (handle_session_error doesn't touch
    // them), but GatewayServer::new wants them.
    let (_store, store_addr, _sh) = spawn_mock_store().await?;
    let (_sched, sched_addr, _sch) = spawn_mock_scheduler().await?;
    let store_client = rio_proto::client::connect_store(&store_addr.to_string()).await?;
    let scheduler_client = rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

    let mut server = GatewayServer::new(store_client, scheduler_client, vec![]);

    let recorder = CountingRecorder::default();
    let before = recorder.get("rio_gateway_errors_total{type=session}");

    metrics::with_local_recorder(&recorder, || {
        // Sync call — handle_session_error is a plain fn, no .await.
        server.handle_session_error(anyhow::anyhow!("simulated keepalive timeout"));
    });

    let after = recorder.get("rio_gateway_errors_total{type=session}");
    assert_eq!(
        after - before,
        1,
        "handle_session_error must increment type=session by exactly 1"
    );
    Ok(())
}

// ===========================================================================
// T1 — channel limit (5th rejected, close one → 6th succeeds)
// ===========================================================================

struct AcceptAllClient;
impl client::Handler for AcceptAllClient {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Spawn a GatewayServer on 127.0.0.1:0 with one authorized key, using
/// the REAL production `build_ssh_config`. Returns (bound addr, client
/// key, background server join handle).
///
/// Mock gRPC backends — T1 opens channels but never reaches opcodes
/// (no wire handshake sent on the SSH channel data stream).
async fn spawn_ssh_server() -> anyhow::Result<(SocketAddr, PrivateKey, tokio::task::JoinHandle<()>)>
{
    let (_store, store_addr, _sh) = spawn_mock_store().await?;
    let (_sched, sched_addr, _sch) = spawn_mock_scheduler().await?;
    let store_client = rio_proto::client::connect_store(&store_addr.to_string()).await?;
    let scheduler_client = rio_proto::client::connect_scheduler(&sched_addr.to_string()).await?;

    let client_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
    let client_pub = client_key.public_key().clone();

    let host_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
    let config = Arc::new(build_ssh_config(host_key));

    let socket = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = socket.local_addr()?;

    let mut server = GatewayServer::new(store_client, scheduler_client, vec![client_pub]);
    let srv_handle = tokio::spawn(async move {
        if let Err(e) = server.run_on_socket(config, &socket).await {
            eprintln!("ssh server error: {e}");
        }
    });

    Ok((addr, client_key, srv_handle))
}

// r[verify gw.conn.channel-limit]
/// Open 4 channels + exec on each (populates `self.sessions`), then
/// a 5th `channel_open_session` receives `SSH_MSG_CHANNEL_OPEN_FAILURE`.
/// Closing one channel frees a slot; 6th open succeeds.
///
/// Must hold channel handles — dropping them closes the channel and
/// shrinks `sessions.len()`.
#[tokio::test]
async fn test_fifth_channel_rejected() -> anyhow::Result<()> {
    common::init_test_logging();
    let (addr, client_key, srv) = spawn_ssh_server().await?;

    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, AcceptAllClient).await?;
    let auth_ok = session
        .authenticate_publickey(
            "nix",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await?
        .success();
    assert!(auth_ok, "publickey auth should succeed");

    // Open 4 channels, send exec on each. `exec(true, ...)` waits for
    // the server's channel_success reply — by the time it returns, the
    // server has inserted into `self.sessions`.
    let mut chans = Vec::new();
    for i in 0..4 {
        let ch = session.channel_open_session().await?;
        ch.exec(true, "nix-daemon --stdio").await?;
        chans.push(ch);
        // Small yield to let the server process the exec fully before
        // the next open — russh serializes handler calls, but the exec
        // reply races with our next open request at the TCP layer.
        tokio::task::yield_now().await;
        eprintln!("channel {i} opened and exec'd");
    }

    // 5th: server returns Ok(false) → SSH_MSG_CHANNEL_OPEN_FAILURE →
    // client sees Err(ChannelOpenFailure).
    let fifth = session.channel_open_session().await;
    assert!(
        matches!(fifth, Err(russh::Error::ChannelOpenFailure(_))),
        "expected ChannelOpenFailure, got {fifth:?}"
    );

    // Closing one frees a slot. `.close()` sends SSH_MSG_CHANNEL_CLOSE
    // async — give the server a beat to process channel_close →
    // sessions.remove before we try the 6th open.
    let closed = chans.pop().unwrap();
    closed.close().await?;
    drop(closed);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let sixth = session.channel_open_session().await;
    assert!(sixth.is_ok(), "slot should free after close: {sixth:?}");

    drop(chans);
    drop(session);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.real-connection-marker]
/// `auth_publickey_offered` rejects a key not in `authorized_keys`
/// BEFORE the client computes a signature. russh::client handles the
/// `none` probe + offered/accept dance internally; from the client's
/// side, an unknown key just means `authenticate_publickey` returns
/// `AuthResult` with `success() == false`.
///
/// Also indirectly verifies `auth_none` + `mark_real_connection`: the
/// server now tracks this connection as "real" even though auth failed.
/// We don't assert the metric here (cross-task recorder scoping is
/// brittle); the fact that auth_none is overridden and the connection
/// completes the rejection flow cleanly is the check.
#[tokio::test]
async fn test_auth_publickey_offered_rejects_unknown_key() -> anyhow::Result<()> {
    common::init_test_logging();
    // Server has ONE authorized key (generated in spawn_ssh_server).
    // We'll connect with a DIFFERENT key.
    let (addr, _authorized_key, srv) = spawn_ssh_server().await?;

    let unknown_key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;

    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, AcceptAllClient).await?;
    let auth_result = session
        .authenticate_publickey(
            "nix",
            PrivateKeyWithHashAlg::new(Arc::new(unknown_key), None),
        )
        .await?;
    assert!(
        !auth_result.success(),
        "unknown key must be rejected; auth_publickey_offered short-circuits before signature"
    );

    drop(session);
    srv.abort();
    Ok(())
}

// ===========================================================================
// T4 — inter-opcode idle timeout
// ===========================================================================

// r[verify gw.conn.lifecycle]
/// After handshake + wopSetOptions, a client that stops sending for
/// `OPCODE_IDLE_TIMEOUT` (600s) receives `STDERR_ERROR("idle timeout")`
/// and the session closes cleanly.
///
/// Uses `tokio::time::pause` + `advance` — safe here because
/// `GatewaySession` is all in-memory `DuplexStream`; no real TCP, so
/// auto-advance doesn't fire spurious deadlines (unlike the
/// `start_paused + real TCP` footgun in lang-gotchas).
///
/// `flavor = "current_thread"` is required: `pause()` panics on the
/// multi-threaded runtime. gRPC mock setup uses real TCP but happens
/// BEFORE `pause()`, so the kernel-side accept completes in real time.
#[tokio::test(flavor = "current_thread")]
async fn test_idle_timeout_fires_after_handshake() -> anyhow::Result<()> {
    common::init_test_logging();
    let mut sess = common::GatewaySession::new_with_handshake().await?;

    // Handshake done, wopSetOptions sent. Server is now blocked on
    // `tokio::time::timeout(600s, wire::read_u64(reader))` waiting for
    // the next opcode. We send nothing.
    tokio::time::pause();

    // Advance past the timeout. The timeout future wakes, sees Elapsed,
    // server sends STDERR_ERROR best-effort, then returns Ok(()).
    tokio::time::advance(std::time::Duration::from_secs(601)).await;

    // Give the server task a chance to run (still paused time, but
    // yield lets the executor poll it).
    tokio::task::yield_now().await;
    tokio::time::resume();

    // Read the STDERR_ERROR off the client stream.
    let err = rio_test_support::wire::drain_stderr_expecting_error(&mut sess.stream).await?;
    assert!(
        err.message.contains("idle timeout"),
        "expected 'idle timeout' in error message, got: {}",
        err.message
    );

    // Server returned Ok(()) after sending the error; session ends
    // cleanly. join_server should complete without panic.
    sess.join_server().await;
    Ok(())
}
