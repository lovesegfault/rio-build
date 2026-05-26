//! Exercises the SSH layer (russh `Config` + `Server`/`Handler` overrides).
//!
//! `GatewaySession` in `tests/common/` bypasses SSH entirely via
//! `DuplexStream` → `run_protocol`. These tests need a real TCP socket +
//! `russh::client`, or direct inspection of the extracted config.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use rio_gateway::server::{GatewayServer, build_ssh_config};
use rio_test_support::grpc::{spawn_mock_scheduler, spawn_mock_store};
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg};
use russh::server::Server as _;
use russh::{MethodKind, client};
use tokio::net::TcpListener;

// ===========================================================================
// T2a — config field assertions (keepalive, nodelay, methods)
// ===========================================================================

// r[verify gw.conn.keepalive+2]
// r[verify gw.conn.nodelay]
/// `build_ssh_config` sets all the hardened fields. russh's `Config`
/// defaults (server/mod.rs:102-128) leave keepalive off, Nagle on, and
/// all auth methods advertised. This test proves we override them.
///
/// End-to-end "does keepalive actually fire" is russh's concern (covered
/// by its own test suite); we only verify we wired the config correctly.
#[test]
fn test_ssh_config_hardened_fields() {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let cfg = build_ssh_config(host_key);

    // keepalive: russh drops at interval × (max+1). 30s × (9+1) = 300s
    // — I-161's 5-minute cold-eval-idle budget for clients without
    // ServerAliveInterval over the SSM-tunnel path.
    assert_eq!(
        cfg.keepalive_interval,
        Some(std::time::Duration::from_secs(30)),
        "keepalive_interval must be set (default: None)"
    );
    assert_eq!(
        cfg.keepalive_max, 9,
        "keepalive_max must give a 5min budget"
    );

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
// T2b — log_session_end metric-delta + stage label
// ===========================================================================

use rio_test_support::metrics::CountingRecorder;

// r[verify gw.conn.session-error-visible]
/// `log_session_end` increments `rio_gateway_errors_total{type="session",stage}`
/// for non-benign errors and skips it for benign disconnects.
///
/// This is the metric-delta half of the keepalive proof: T2a shows
/// keepalive is configured; this shows that WHEN a session errors out
/// (for any reason — keepalive timeout, `?` propagation, connection-setup
/// failure), the error surfaces to the operator via the metric WITH the
/// `ConnStage` reached — `tcp-accepted` distinguishes "client opened TCP
/// but never sent SSH bytes" from a real session going silent.
#[test]
fn test_log_session_end_increments_metric_with_stage() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU8;

    let recorder = CountingRecorder::default();
    let stage = Arc::new(AtomicU8::new(0)); // tcp-accepted
    let peer = "[::1]:1".parse().unwrap();

    metrics::with_local_recorder(&recorder, || {
        rio_gateway::server::log_session_end(
            peer,
            &stage,
            &anyhow::anyhow!("simulated keepalive timeout"),
        );
    });

    let key = "rio_gateway_errors_total{stage=tcp-accepted,type=session}";
    assert_eq!(
        recorder.get(key),
        1,
        "non-benign session error must increment type=session,stage=tcp-accepted; keys={:?}",
        recorder.all_keys()
    );

    // Benign disconnect at channel-open → debug only, no metric.
    let stage = Arc::new(AtomicU8::new(3));
    metrics::with_local_recorder(&recorder, || {
        rio_gateway::server::log_session_end(peer, &stage, &russh::Error::Disconnect.into());
    });
    assert_eq!(
        recorder.get("rio_gateway_errors_total{stage=channel-open,type=session}"),
        0,
        "benign disconnect must NOT increment the metric"
    );
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
/// Mock gRPC backends — these tests open channels but never reach
/// opcodes (no wire handshake sent on the SSH channel data stream).
async fn spawn_ssh_server() -> anyhow::Result<(SocketAddr, PrivateKey, tokio::task::JoinHandle<()>)>
{
    spawn_ssh_server_with(|s| s).await
}

/// [`spawn_ssh_server`] with a builder hook so tests can shrink the
/// limits (`with_max_sessions(4)`, `with_max_channels_per_connection(8)`,
/// `with_empty_connection_grace(200ms)`) instead of opening 4096
/// channels to reach the production defaults.
///
/// Runs the PRODUCTION accept loop ([`GatewayServer::run_on_listener`]),
/// not russh's `run_on_socket`, so the per-connection spawn site — and
/// the pre-auth deadline that lives there — is what these tests
/// exercise. The shutdown token is never fired; tests abort the returned
/// `JoinHandle` instead (per-connection tasks die with the test runtime).
async fn spawn_ssh_server_with(
    configure: impl FnOnce(GatewayServer) -> GatewayServer,
) -> anyhow::Result<(SocketAddr, PrivateKey, tokio::task::JoinHandle<()>)> {
    let (_store, store_addr, _sh) = spawn_mock_store().await?;
    let (_sched, sched_addr, _sch) = spawn_mock_scheduler().await?;
    let store_client = rio_proto::client::connect_single(&store_addr.to_string()).await?;
    let scheduler_client = rio_proto::client::connect_single(&sched_addr.to_string()).await?;

    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let client_pub = client_key.public_key().clone();

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;

    let socket = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = socket.local_addr()?;

    let server = configure(GatewayServer::new(
        store_client,
        scheduler_client,
        vec![client_pub],
    ));
    let srv_handle = tokio::spawn(async move {
        if let Err(e) = server
            .run_on_listener(host_key, socket, rio_common::signal::Token::new())
            .await
        {
            eprintln!("ssh server error: {e}");
        }
    });

    Ok((addr, client_key, srv_handle))
}

/// Connect + authenticate a russh client against a [`spawn_ssh_server`]
/// instance. Extracted because the multi-connection tests (the global
/// session cap is shared across connections) need to do this twice with
/// the same key.
async fn connect_and_auth(
    addr: SocketAddr,
    client_key: PrivateKey,
) -> anyhow::Result<client::Handle<AcceptAllClient>> {
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, AcceptAllClient).await?;
    let auth_ok = session
        .authenticate_publickey(
            "nix",
            PrivateKeyWithHashAlg::new(Arc::new(client_key), None),
        )
        .await?
        .success();
    anyhow::ensure!(auth_ok, "publickey auth should succeed");
    Ok(session)
}

// r[verify gw.conn.channel-limit+3]
/// The per-connection bound counts SSH-level OPEN channels (not exec'd
/// sessions): with the bound set to 8, eight opens with NO exec all
/// succeed (under the old `sessions.len()` gate they were invisible and
/// unbounded), the 9th receives `SSH_MSG_CHANNEL_OPEN_FAILURE`, and
/// closing one releases the slot for a 10th.
///
/// Must hold channel handles — dropping them closes the channel and
/// decrements `open_channels`.
#[tokio::test]
async fn test_channel_open_absurdity_bound() -> anyhow::Result<()> {
    common::init_test_logging();
    const BOUND: usize = 8;
    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_max_channels_per_connection(BOUND)).await?;
    let session = connect_and_auth(addr, client_key).await?;

    // Open BOUND channels with no exec. Every one must be accepted —
    // the bound is the absurdity threshold, not a working limit.
    let mut chans = Vec::new();
    for i in 0..BOUND {
        let ch = session
            .channel_open_session()
            .await
            .unwrap_or_else(|e| panic!("open #{i} should be accepted (bound={BOUND}): {e:?}"));
        chans.push(ch);
    }

    // BOUND+1th: server returns Ok(false) → SSH_MSG_CHANNEL_OPEN_FAILURE
    // → client sees Err(ChannelOpenFailure).
    let over = session.channel_open_session().await;
    assert!(
        matches!(over, Err(russh::Error::ChannelOpenFailure(_))),
        "open #{BOUND} must be refused at the absurdity bound, got {over:?}"
    );

    // Closing one frees a slot. `.close()` sends SSH_MSG_CHANNEL_CLOSE
    // async — give the server a beat to process channel_close →
    // open_channels -= 1 before we retry.
    //
    // SLEEP JUSTIFICATION: russh provides no client-side await for
    // the server's channel-close confirm. The slot frees when the
    // server's handler.channel_close() runs, which is strictly after
    // the SSH_MSG_CHANNEL_CLOSE round-trip. 50ms is generous for a
    // localhost round-trip + handler dispatch; event-driven sync would
    // require server-side test hooks — not worth the test-only
    // plumbing.
    let closed = chans.pop().unwrap();
    closed.close().await?;
    drop(closed);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let retry = session.channel_open_session().await;
    assert!(retry.is_ok(), "slot should free after close: {retry:?}");

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

    let unknown_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;

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

// r[verify gw.conn.session-cap]
/// The global session cap rejects the over-limit EXEC (clean
/// `channel_failure`), never the channel OPEN (which would corrupt a
/// ControlMaster client), and the cap is shared across connections:
/// with `max_sessions = 4`, six opens on connection A all succeed,
/// execs 1-4 succeed, execs 5-6 get `channel_failure`, an exec on a
/// second connection B is also rejected, and closing one of A's live
/// sessions frees a permit that B can then claim.
#[tokio::test]
async fn test_global_session_cap_rejects_exec_cleanly() -> anyhow::Result<()> {
    common::init_test_logging();
    const CAP: usize = 4;
    let (addr, client_key, srv) = spawn_ssh_server_with(|s| s.with_max_sessions(CAP)).await?;
    let conn_a = connect_and_auth(addr, client_key.clone()).await?;

    // Open 6 channels — all accepted (well under the 512 absurdity
    // bound; the session cap must NOT refuse opens).
    let mut chans = Vec::new();
    for i in 0..6 {
        let ch = conn_a.channel_open_session().await.unwrap_or_else(|e| {
            panic!("open #{i} must be accepted (cap gates execs, not opens): {e:?}")
        });
        chans.push(ch);
    }

    // Exec on each. First CAP → channel_success; the rest →
    // channel_failure. `exec(true, ...)` only SENDS the request; the
    // server's success/failure reply arrives via `wait()`.
    for (i, ch) in chans.iter_mut().enumerate() {
        ch.exec(true, "nix-daemon --stdio").await?;
        let msg = ch.wait().await.expect("server reply");
        if i < CAP {
            assert!(
                matches!(msg, russh::ChannelMsg::Success),
                "exec #{i} should get channel_success: {msg:?}"
            );
        } else {
            assert!(
                matches!(msg, russh::ChannelMsg::Failure),
                "exec #{i} must get channel_failure (global session cap); got {msg:?}"
            );
        }
    }

    // The cap is GLOBAL: a second SSH connection sees the same
    // exhausted semaphore.
    let conn_b = connect_and_auth(addr, client_key).await?;
    let mut b_ch = conn_b.channel_open_session().await?;
    b_ch.exec(true, "nix-daemon --stdio").await?;
    let msg = b_ch.wait().await.expect("server reply");
    assert!(
        matches!(msg, russh::ChannelMsg::Failure),
        "exec on a second connection must also be rejected (the cap is global): {msg:?}"
    );

    // Closing one of A's live sessions releases its permit to B.
    // SLEEP JUSTIFICATION: the permit is released when the server's
    // channel_close → sessions.remove → ChannelSession::Drop runs,
    // strictly after the SSH_MSG_CHANNEL_CLOSE round-trip; russh has
    // no client-side await for that. 50ms >> a localhost round-trip.
    let closed = chans.remove(0);
    closed.close().await?;
    drop(closed);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut b_ch2 = conn_b.channel_open_session().await?;
    b_ch2.exec(true, "nix-daemon --stdio").await?;
    let msg = b_ch2.wait().await.expect("server reply");
    assert!(
        matches!(msg, russh::ChannelMsg::Success),
        "a permit freed on connection A must be claimable from connection B: {msg:?}"
    );

    drop(chans);
    drop(conn_a);
    drop(conn_b);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.channel-limit+3]
// r[verify gw.conn.session-cap]
/// The headline regression test for the ControlMaster incident: 32
/// concurrent `nix-daemon --stdio` sessions multiplexed onto ONE SSH
/// connection must ALL be admitted under the default limits. Under the
/// previous design (`MAX_CHANNELS_PER_CONNECTION = 4`) only 4 of these
/// execs succeeded and the rest were refused — and any refusal of the
/// channel *open* makes a stock nix client behind `ControlMaster auto`
/// silently fall back to a direct connection whose handshake is
/// corrupted by `LocalCommand`.
#[tokio::test]
async fn test_many_multiplexed_sessions_all_succeed() -> anyhow::Result<()> {
    common::init_test_logging();
    let (addr, client_key, srv) = spawn_ssh_server().await?;
    let session = connect_and_auth(addr, client_key).await?;

    let mut chans = Vec::new();
    for i in 0..32 {
        let ch = session.channel_open_session().await.unwrap_or_else(|e| {
            panic!("open #{i}/32 must be accepted under default limits: {e:?}")
        });
        chans.push(ch);
    }
    for (i, ch) in chans.iter_mut().enumerate() {
        ch.exec(true, "nix-daemon --stdio").await?;
        let msg = ch.wait().await.expect("server reply");
        assert!(
            matches!(msg, russh::ChannelMsg::Success),
            "exec #{i}/32 must succeed under default limits (one ControlMaster \
             carrying a 32-worker nix-fast-build is a legitimate client); got {msg:?}"
        );
    }

    drop(chans);
    drop(session);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// A connection whose channel count transits through zero must survive
/// for the grace period (a ControlMaster between builds), and must
/// still be reaped once the grace period expires without a new channel
/// (an abandoned connection).
#[tokio::test]
async fn test_connection_survives_channel_count_touching_zero() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE)).await?;
    let session = connect_and_auth(addr, client_key).await?;

    // Build 1: open, exec, close — the connection's channel count
    // touches zero.
    let ch1 = session.channel_open_session().await?;
    ch1.exec(true, "nix-daemon --stdio").await?;
    ch1.close().await?;
    drop(ch1);

    // Give the server time to process the close and arm the idle
    // timer, but stay well inside the grace period. Under the previous
    // behavior the server sent SSH_MSG_DISCONNECT here and build 2
    // failed.
    tokio::time::sleep(GRACE / 4).await;

    // Build 2: the mux opens its next session on the same connection.
    let ch2 = session.channel_open_session().await.map_err(|e| {
        anyhow::anyhow!("connection must survive its channel count touching zero: {e:?}")
    })?;
    ch2.exec(true, "nix-daemon --stdio").await?;
    ch2.close().await?;
    drop(ch2);

    // No build 3: after the grace period expires with zero channels,
    // the server disconnects. 3× the grace is comfortably past the
    // timer without making the test slow.
    tokio::time::sleep(GRACE * 3).await;
    let after = session.channel_open_session().await;
    assert!(
        after.is_err(),
        "an idle connection must still be reaped after the grace period, \
         got a successful open: {after:?}"
    );

    drop(session);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// An authenticated connection that NEVER opens a channel (`ssh -N`, a
/// ControlMaster held open with no commands, a client wedged before
/// exec) has had zero open channels continuously since establishment,
/// so it must be disconnected once the grace period expires — and its
/// `max_connections` slot must be released (`rio_gateway_connections_active`
/// returns to its prior value).
///
/// Regression: the grace timer used to be armed only in `channel_close`,
/// so a connection that authenticated and never opened a channel never
/// armed it, answered the 30s keepalives forever (any received data
/// resets russh's `alive_timeouts`), never tripped `inactivity_timeout`
/// (each keepalive reply resets it), and was held indefinitely.
///
/// The recorder is the thread-local default (`set_default_local_recorder`);
/// `#[tokio::test]` is a current-thread runtime, so the spawned server
/// task emits into it.
#[tokio::test]
async fn test_authenticated_connection_without_channels_is_reaped() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    const GAUGE: &str = "rio_gateway_connections_active{}";

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE)).await?;
    let session = connect_and_auth(addr, client_key).await?;

    // Sanity: the authenticated connection is counted. This also proves
    // the recorder is actually wired, so the ==0 assertion at the end
    // can't pass vacuously.
    assert_eq!(
        recorder.gauge_value(GAUGE),
        Some(1.0),
        "authenticated connection must count toward connections_active; gauges: {:?}",
        recorder.gauge_names()
    );

    // Never open a channel. Well past the grace period the server must
    // have sent SSH_MSG_DISCONNECT — a new channel open fails. 3× the
    // grace mirrors test_connection_survives_channel_count_touching_zero.
    tokio::time::sleep(GRACE * 3).await;
    let after = session.channel_open_session().await;
    assert!(
        after.is_err(),
        "a connection that never opened a channel must be disconnected after \
         the grace period, got a successful open: {after:?}"
    );

    // The reap must release the slot: ConnectionHandler::Drop decrements
    // the gauge. Drop runs when the server-side session task finishes
    // tearing down — strictly after the client observes the disconnect —
    // so poll briefly instead of asserting instantly.
    let mut active = recorder.gauge_value(GAUGE);
    for _ in 0..40 {
        if active == Some(0.0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        active = recorder.gauge_value(GAUGE);
    }
    assert_eq!(
        active,
        Some(0.0),
        "connections_active must return to its prior value once the idle \
         connection is reaped; gauges: {:?}",
        recorder.gauge_names()
    );

    drop(session);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// Converse guard for the reap tests: a client that proceeds normally —
/// authenticates, opens a channel, and execs `nix-daemon --stdio` inside
/// the grace window — is NOT disconnected. The admitted exec (a live
/// protocol session) is what disarms the establishment-time grace timer;
/// a bare channel open no longer counts as activity, which is exactly
/// what `test_open_channel_without_exec_is_reaped` pins from the other
/// side.
#[tokio::test]
async fn test_connection_execing_within_grace_survives() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE)).await?;
    let session = connect_and_auth(addr, client_key).await?;

    // Open a channel and exec on it (like a real nix client) inside the
    // grace window, then keep the session open. The exec request is
    // processed by the server within milliseconds of being sent, well
    // inside the grace.
    tokio::time::sleep(GRACE / 4).await;
    let mut ch = session.channel_open_session().await?;
    ch.exec(true, "nix-daemon --stdio").await?;
    let msg = ch.wait().await.expect("server reply");
    assert!(
        matches!(msg, russh::ChannelMsg::Success),
        "exec must be admitted (this is the disarm event): {msg:?}"
    );

    // Well past the point where the establishment-time timer would have
    // fired: the connection must still be alive because the admitted
    // exec disarmed it and the session is still active.
    tokio::time::sleep(GRACE * 3).await;
    let again = session.channel_open_session().await;
    assert!(
        again.is_ok(),
        "a connection with an active protocol session must not be reaped by \
         the empty-connection grace timer: {again:?}"
    );

    drop(ch);
    drop(session);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// A connection that attempts authentication but never succeeds (rejected
/// key, wedged ssh-agent) must be disconnected once the grace period
/// expires, releasing its `max_connections` slot
/// (`rio_gateway_connections_active` returns to its prior value).
///
/// Regression: russh 0.61 has no login-grace deadline of its own
/// (`Config` has no such field and `max_auth_attempts` is never
/// enforced), the auth callbacks have no `&mut Session` to arm the
/// empty-connection timer with, and an answering-but-idle client resets
/// both `alive_timeouts` and `inactivity_timeout` on every keepalive
/// reply — so a never-authenticated connection used to hold its permit,
/// fd, and gauge slot forever.
#[tokio::test]
async fn test_unauthenticated_connection_is_reaped() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    const GAUGE: &str = "rio_gateway_connections_active{}";

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (addr, _client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE)).await?;

    // Connect and offer a key the server does not know: rejected at
    // `auth_publickey_offered`, never authenticated. The client then
    // sits on the open transport (a real client would keep answering
    // keepalives, which reset russh's inactivity/keepalive timers).
    let unknown = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, AcceptAllClient).await?;
    let auth = session
        .authenticate_publickey("nix", PrivateKeyWithHashAlg::new(Arc::new(unknown), None))
        .await?;
    anyhow::ensure!(!auth.success(), "unknown key must be rejected");

    // The failed attempt fired `mark_real_connection`: the connection is
    // holding a permit and counts toward the gauge. This also proves the
    // recorder is wired, so the ==0 assertion below can't pass vacuously.
    assert_eq!(
        recorder.gauge_value(GAUGE),
        Some(1.0),
        "a connection that attempted auth must be counted while it is held; gauges: {:?}",
        recorder.gauge_names()
    );

    // Past the deadline the gateway must have disconnected it and
    // released the slot. Poll: ConnectionHandler::Drop runs when the
    // server-side session task finishes tearing down, strictly after the
    // disconnect is sent, so don't assert instantly.
    let mut reaped = false;
    for _ in 0..40 {
        if recorder.gauge_value(GAUGE) == Some(0.0) && session.is_closed() {
            reaped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        reaped,
        "a connection that never authenticates must be disconnected within the \
         pre-auth deadline and release its slot; gauge={:?} closed={}",
        recorder.gauge_value(GAUGE),
        session.is_closed(),
    );

    drop(session);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// A connection that authenticates and opens a channel but never sends
/// an exec request has zero active protocol sessions — no
/// `ChannelSession`, no session-semaphore permit, no protocol task (the
/// handshake/idle timeouts only exist after exec) — so it must be
/// disconnected once the grace period expires. An open-but-never-exec'd
/// channel must NOT count as activity.
#[tokio::test]
async fn test_open_channel_without_exec_is_reaped() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE)).await?;
    let session = connect_and_auth(addr, client_key).await?;

    // Open a channel but never exec on it: no protocol session is ever
    // created, so the connection never leaves the "empty" state.
    let _ch = session.channel_open_session().await?;

    // Well past the grace: the gateway must have disconnected the
    // connection. 3× the grace mirrors the other reap tests.
    tokio::time::sleep(GRACE * 3).await;
    let after = session.channel_open_session().await;
    assert!(
        after.is_err(),
        "a connection whose only channel never exec'd must be reaped after \
         the grace period, got a successful open: {after:?}"
    );

    drop(session);
    srv.abort();
    Ok(())
}

/// Forwarding TCP proxy for one connection that can stop relaying
/// client→server bytes on command (server→client always flows). Lets a
/// test stage a misbehaving SSH client out of a well-behaved russh one:
/// once `block` is set, the russh client's automatic protocol responses
/// (most importantly the `CHANNEL_CLOSE` acknowledgment it sends when the
/// server closes a channel) silently never reach the gateway, while the
/// client itself still sees all server traffic and stays connected.
async fn spawn_blockable_proxy(
    upstream: SocketAddr,
) -> anyhow::Result<(
    SocketAddr,
    Arc<std::sync::atomic::AtomicBool>,
    tokio::task::JoinHandle<()>,
)> {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let block = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let block_pump = Arc::clone(&block);
    let task = tokio::spawn(async move {
        let Ok((mut client_sock, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut server_sock) = tokio::net::TcpStream::connect(upstream).await else {
            return;
        };
        let mut cbuf = [0u8; 16 * 1024];
        let mut sbuf = [0u8; 16 * 1024];
        loop {
            tokio::select! {
                r = client_sock.read(&mut cbuf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Blocked: keep reading (so the client never blocks
                        // on a full send buffer) but discard the bytes.
                        if !block_pump.load(Ordering::SeqCst)
                            && server_sock.write_all(&cbuf[..n]).await.is_err()
                        {
                            break;
                        }
                    }
                },
                r = server_sock.read(&mut sbuf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if client_sock.write_all(&sbuf[..n]).await.is_err() {
                            break;
                        }
                    }
                },
            }
        }
    });
    Ok((addr, block, task))
}

// r[verify gw.conn.session-cap]
// r[verify gw.conn.exit-status+3]
/// bug_001: a protocol session that ends SERVER-side (handshake timeout
/// here; opcode-idle timeout and protocol errors exit through the same
/// `run_protocol` return) must release the global session permit and the
/// `rio_gateway_channels_active` gauge immediately, and the connection —
/// now holding only a dead session — must be reaped by the
/// empty-connection grace, all WITHOUT any cooperation from the client.
///
/// Staging: client A execs `nix-daemon --stdio` (admitted, takes the only
/// `max_sessions = 1` permit) and then never sends a protocol byte; the
/// shrunken handshake timeout ends the session server-side. A connects
/// through a proxy whose client→server direction is blocked right after
/// the exec is admitted, so the `CHANNEL_CLOSE` acknowledgment the russh
/// client automatically sends in response to the server's channel close
/// never reaches the gateway — the same observable behavior as a client
/// that ignores the server's close and just keeps answering keepalives.
///
/// Regression: the permit, the gauge decrement, and the
/// empty-connection-grace arming all lived on the `sessions` map entry,
/// which only CLIENT action removed — such a client pinned one global
/// session slot per channel forever and the connection was never reaped.
#[tokio::test]
async fn test_server_side_ended_session_releases_capacity() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    const HANDSHAKE: std::time::Duration = std::time::Duration::from_millis(300);
    const GAUGE: &str = "rio_gateway_channels_active{}";

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (addr, client_key, srv) = spawn_ssh_server_with(|s| {
        s.with_max_sessions(1)
            .with_empty_connection_grace(GRACE)
            .with_handshake_timeout(HANDSHAKE)
    })
    .await?;

    // Client A connects through the blockable proxy so the test can cut
    // its outbound traffic the moment the exec has been admitted.
    let (proxy_addr, block_a_outbound, proxy_task) = spawn_blockable_proxy(addr).await?;
    let conn_a = connect_and_auth(proxy_addr, client_key.clone()).await?;
    let mut ch_a = conn_a.channel_open_session().await?;
    ch_a.exec(true, "nix-daemon --stdio").await?;
    let msg = ch_a.wait().await.expect("server reply");
    assert!(
        matches!(msg, russh::ChannelMsg::Success),
        "exec on A must be admitted: {msg:?}"
    );
    assert_eq!(
        recorder.gauge_value(GAUGE),
        Some(1.0),
        "admitted exec must count as an active session; gauges: {:?}",
        recorder.gauge_names()
    );

    // From here on nothing A sends reaches the gateway. A never speaks the
    // nix protocol, so the (shrunken) handshake timeout ends the protocol
    // session server-side ~300ms from the exec; the close the server then
    // sends is acknowledged by the russh client, but that ack is dropped
    // here — exactly the client that never closes its dead channel.
    block_a_outbound.store(true, std::sync::atomic::Ordering::SeqCst);

    // The dead session must release the gauge (and its permit) without any
    // client cooperation. Poll past the handshake timeout with margin.
    let mut active = recorder.gauge_value(GAUGE);
    for _ in 0..80 {
        if active == Some(0.0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        active = recorder.gauge_value(GAUGE);
    }

    // With max_sessions = 1, a second client's exec only succeeds if the
    // dead session's permit was actually released.
    let conn_b = connect_and_auth(addr, client_key).await?;
    let mut ch_b = conn_b.channel_open_session().await?;
    ch_b.exec(true, "nix-daemon --stdio").await?;
    let msg = ch_b.wait().await.expect("server reply");
    assert!(
        matches!(msg, russh::ChannelMsg::Success),
        "a server-side-ended session must release its global session permit \
         (max_sessions = 1, so a still-held permit rejects this exec): {msg:?}"
    );
    assert_eq!(
        active,
        Some(0.0),
        "rio_gateway_channels_active must return to baseline once the protocol \
         session ends server-side, without waiting for the client to close the \
         channel; gauges: {:?}",
        recorder.gauge_names()
    );

    // The connection whose only session died must be reaped by the
    // empty-connection grace: a dead session is not activity, even though
    // its channel is still open from the client's point of view.
    let mut reaped = false;
    for _ in 0..80 {
        if conn_a.is_closed() {
            reaped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        reaped,
        "a connection whose only protocol session ended server-side must be \
         reaped by the empty-connection grace despite the client never closing \
         its channel"
    );

    drop(ch_a);
    drop(conn_a);
    drop(ch_b);
    drop(conn_b);
    proxy_task.abort();
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// A client that opens TCP but never sends its SSH identification string
/// parks the connection inside russh's `run_stream` (`read_ssh_id` is
/// bounded only by the 1 h `inactivity_timeout`), holding its
/// `max_connections` permit and fd the whole time. The pre-auth deadline
/// must cover this pre-handshake phase too.
///
/// `max_connections = 1` makes the released permit observable from the
/// outside: while the silent connection is held a real client is
/// rejected at the connection cap, and once the silent connection is
/// dropped the same client can connect and authenticate. (The
/// `connections_active` gauge is not asserted here on purpose — it is
/// only ever incremented at the first auth callback, which a silent
/// client never reaches.)
#[tokio::test]
async fn test_silent_tcp_connection_is_reaped() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE).with_max_connections(1))
            .await?;

    // Raw TCP connect; never send the SSH identification string.
    let mut silent = tokio::net::TcpStream::connect(addr).await?;

    // Precondition: the silent connection holds the only permit, so a
    // real client is rejected at the connection cap. Proves the permit
    // is actually consumed, so the success assertion below can't pass
    // vacuously.
    let denied = connect_and_auth(addr, client_key.clone()).await;
    assert!(
        denied.is_err(),
        "while the silent connection holds the only permit, a second \
         connection must be rejected at the connection cap"
    );

    // Past the deadline the gateway must have dropped the silent
    // connection and released its slot: a real client can now connect
    // and authenticate. 3× the grace mirrors the other reap tests.
    tokio::time::sleep(GRACE * 3).await;
    let allowed = connect_and_auth(addr, client_key).await;
    assert!(
        allowed.is_ok(),
        "a TCP client that never sends the SSH banner must be dropped within \
         the pre-auth deadline and release its connection slot: {:?}",
        allowed.err()
    );

    // The silent socket itself must have been closed by the server: we
    // read back the server banner it sent at accept, then EOF. Bounded
    // so a regression can't hang the test.
    let mut buf = Vec::new();
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::io::AsyncReadExt::read_to_end(&mut silent, &mut buf),
    )
    .await
    .expect("server must close the silent connection (read_to_end timed out)")?;
    assert!(n > 0, "expected at least the server SSH banner before EOF");

    drop(silent);
    drop(allowed);
    srv.abort();
    Ok(())
}

// r[verify gw.conn.exit-status+3]
/// bug_004: a peer that sends its SSH banner, never completes the key
/// exchange, and keeps sending small transport packets (`SSH_MSG_IGNORE`)
/// is the worst pre-auth squatter: russh only drains server-queued
/// messages between key exchanges (`!kex.active()` on the
/// `receiver.recv()` arm of its session loop), so the polite
/// `SSH_MSG_DISCONNECT` queued by the pre-auth deadline is never
/// delivered, and every received packet resets both `alive_timeouts` and
/// the inactivity timer, so neither keepalive nor inactivity ever fires.
/// Without a hard bound such a peer holds its `max_connections` permit,
/// fd, and tasks forever (and never appears in `connections_active`,
/// which only counts connections that reached an auth callback).
///
/// The gateway must therefore force-close the transport a short, fixed
/// slack after the polite disconnect at the pre-auth deadline. Observable
/// here with `max_connections = 1`: while the parked peer is alive a real
/// client is rejected at the connection cap (checked immediately AND just
/// past the grace, so the test cannot pass vacuously if russh ever starts
/// erroring on the cleartext IGNOREs and dropping the peer early); within
/// `grace + FORCE_CLOSE_SLACK` (+ scheduling margin) the slot must be
/// released exactly once and a real client must connect and authenticate,
/// and the parked peer's socket must be closed.
#[tokio::test]
async fn test_kex_parked_preauth_connection_is_force_closed() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);

    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE).with_max_connections(1))
            .await?;

    // Raw TCP peer: real SSH banner, then never a KEXINIT — only periodic
    // SSH_MSG_IGNORE packets. Pre-KEX packets are cleartext with no MAC:
    //   uint32 packet_length | byte padding_length | payload | padding
    // payload = [SSH_MSG_IGNORE(2), uint32 data_len = 0]; 6 bytes of
    // padding brings the total to 16 (a multiple of 8, padding ≥ 4).
    // russh tolerates IGNORE at any point (its packet dispatch
    // early-returns on IGNORE/UNIMPLEMENTED/DEBUG) and treats it as
    // liveness, which is exactly what makes this peer unreapable by the
    // transport's own keepalive/inactivity limits.
    const SSH_MSG_IGNORE_PACKET: [u8; 16] = [
        0, 0, 0, 12, // packet_length = 12
        6,  // padding_length
        2,  // SSH_MSG_IGNORE
        0, 0, 0, 0, // ignore payload: empty string
        0, 0, 0, 0, 0, 0, // padding
    ];
    let parked_at = tokio::time::Instant::now();
    let mut parked = tokio::net::TcpStream::connect(addr).await?;
    parked.write_all(b"SSH-2.0-rio-test-kex-parked\r\n").await?;
    let (mut parked_read, mut parked_write) = parked.into_split();
    let trickle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if parked_write
                .write_all(&SSH_MSG_IGNORE_PACKET)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Precondition: the parked peer holds the only permit, so a real
    // client is rejected at the connection cap. Proves the permit is
    // actually consumed, so the success assertion below can't pass
    // vacuously.
    let denied = connect_and_auth(addr, client_key.clone()).await;
    assert!(
        denied.is_err(),
        "while the KEX-parked peer holds the only permit, a second connection \
         must be rejected at the connection cap"
    );

    // Mid-window probe, just past the grace and well before the slack: the
    // parked peer must STILL hold the only slot — the polite disconnect
    // queued at the grace is undeliverable while the key exchange is in
    // flight. If a client gets in here, the parked socket was dropped
    // early (e.g. russh started erroring on the cleartext IGNOREs), and
    // the force-close assertion below would pass without exercising the
    // force-close at all.
    tokio::time::sleep_until(parked_at + GRACE + std::time::Duration::from_secs(1)).await;
    let mid_window = connect_and_auth(addr, client_key.clone()).await;
    if let Ok(session) = mid_window {
        // Only conclusive while still inside the force-close window: a
        // badly stalled runner could land this probe after the slack, at
        // which point the slot is legitimately free.
        let elapsed = parked_at.elapsed();
        assert!(
            elapsed >= GRACE + rio_gateway::server::FORCE_CLOSE_SLACK,
            "the parked peer no longer holds the connection slot at ~grace+1s \
             (elapsed {elapsed:?}): it was released before the force-close window, \
             so the IGNORE-trickling peer is being dropped early and this test \
             would pass vacuously"
        );
        drop(session);
    }

    // Within grace + slack (+ generous scheduling margin) the gateway must
    // have force-closed the parked transport and released its slot: a real
    // client can connect and authenticate. The polite disconnect alone can
    // never achieve this — russh cannot deliver it while the peer keeps
    // the initial key exchange open.
    let force_close_budget =
        GRACE + rio_gateway::server::FORCE_CLOSE_SLACK + std::time::Duration::from_secs(5);
    let deadline = parked_at + force_close_budget;
    let mut admitted = None;
    while tokio::time::Instant::now() < deadline {
        match connect_and_auth(addr, client_key.clone()).await {
            Ok(session) => {
                admitted = Some(session);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
    assert!(
        admitted.is_some(),
        "the KEX-parked peer must be force-closed within the pre-auth deadline \
         plus its slack, releasing the connection slot for a real client"
    );

    // The parked socket itself must have been closed by the gateway (we
    // read the server banner it sent at accept, then EOF / reset). Bounded
    // so a regression can't hang the test.
    let mut buf = Vec::new();
    let read_back = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        parked_read.read_to_end(&mut buf),
    )
    .await
    .expect("gateway must close the KEX-parked connection (read_to_end timed out)");
    // EOF (Ok) and ECONNRESET (Err) are both acceptable proofs of death.
    drop(read_back);

    trickle.abort();
    drop(admitted);
    srv.abort();
    Ok(())
}

/// Forwarding TCP proxy for one connection that can be flipped into a
/// "hold the socket" mode: from that point on it forwards nothing in
/// either direction and never closes the gateway-side socket on its own —
/// not when the russh client half goes away, and not when the gateway
/// half-closes its write side (the TCP FIN russh sends right after
/// processing a queued `SSH_MSG_DISCONNECT`). This stages the post-auth
/// squatter a well-behaved russh client cannot: a peer that receives the
/// gateway's disconnect and simply keeps the TCP connection open. Once
/// holding, the task never finishes on its own — abort it at the end of
/// the test; the force-close itself is observed from the gateway side
/// (slot release / gauge), not from here.
async fn spawn_socket_holding_proxy(
    upstream: SocketAddr,
) -> anyhow::Result<(
    SocketAddr,
    Arc<std::sync::atomic::AtomicBool>,
    tokio::task::JoinHandle<()>,
)> {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hold_pump = Arc::clone(&hold);
    let task = tokio::spawn(async move {
        let Ok((mut client_sock, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut server_sock) = tokio::net::TcpStream::connect(upstream).await else {
            return;
        };
        let mut cbuf = [0u8; 16 * 1024];
        let mut sbuf = [0u8; 16 * 1024];
        // Once holding, neither a client-side close nor a gateway-side
        // half-close may take the held socket down: the held fd must stay
        // open until the gateway force-closes (which the test observes
        // from the gateway's own accounting) and this task is aborted.
        let mut client_open = true;
        let mut server_open = true;
        loop {
            if !client_open && !server_open {
                // Fully quiesced in hold mode: nothing left to relay; just
                // keep the sockets alive (that IS the hold) until aborted.
                std::future::pending::<()>().await;
            }
            // The hold flag is loaded when an event is HANDLED, not before
            // parking in the select: the test flips it while this task is
            // parked, and the very next event (the russh client closing
            // its half once the test drops it) must already see the hold.
            tokio::select! {
                r = client_sock.read(&mut cbuf), if client_open => {
                    let holding = hold_pump.load(Ordering::SeqCst);
                    match r {
                        Ok(0) | Err(_) => {
                            if holding {
                                client_open = false;
                            } else {
                                break;
                            }
                        }
                        Ok(n) => {
                            if !holding && server_sock.write_all(&cbuf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                },
                r = server_sock.read(&mut sbuf), if server_open => {
                    let holding = hold_pump.load(Ordering::SeqCst);
                    match r {
                        // EOF here is only the gateway half-closing its
                        // write side (FIN after the disconnect); a
                        // socket-holding peer keeps its own end open
                        // regardless.
                        Ok(0) | Err(_) => {
                            if holding {
                                server_open = false;
                            } else {
                                break;
                            }
                        }
                        Ok(n) => {
                            if !holding && client_sock.write_all(&sbuf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                },
            }
        }
    });
    Ok((addr, hold, task))
}

// r[verify gw.conn.exit-status+3]
/// The post-auth squatter: an authenticated, exec-less peer that simply
/// never closes its socket after the empty-connection grace disconnect.
/// russh queues the `SSH_MSG_DISCONNECT`, half-closes its write side, and
/// then parks in its post-disconnect drain-read loop — which has no
/// timeout and arms no keepalives — so without a transport-level bound
/// the connection's fd, `max_connections` permit, and
/// `connections_active` slot are pinned until the peer goes away on its
/// own (i.e. never).
///
/// The gateway must force-close such a transport within
/// `grace + FORCE_CLOSE_SLACK`. Observable with `max_connections = 1`:
/// while the holder lives, a real client is rejected at the connection
/// cap (checked immediately AND just past the grace, so a pass cannot
/// come from some earlier teardown); within grace + slack (+ scheduling
/// margin) the gateway must release the slot (a real client connects and
/// authenticates) and undo the holder's `connections_active` increment —
/// neither of which can happen while the held connection's handler is
/// still alive.
#[tokio::test]
async fn test_socket_holding_authenticated_connection_is_force_closed() -> anyhow::Result<()> {
    common::init_test_logging();
    const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
    const GAUGE: &str = "rio_gateway_connections_active{}";

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (addr, client_key, srv) =
        spawn_ssh_server_with(|s| s.with_empty_connection_grace(GRACE).with_max_connections(1))
            .await?;

    // The holder authenticates through the holding proxy and never execs;
    // right after auth the proxy stops relaying and keeps the gateway-side
    // socket open no matter what the russh client half does.
    let (proxy_addr, hold, proxy_task) = spawn_socket_holding_proxy(addr).await?;
    let holder_at = tokio::time::Instant::now();
    let holder = connect_and_auth(proxy_addr, client_key.clone()).await?;
    assert_eq!(
        recorder.gauge_value(GAUGE),
        Some(1.0),
        "the authenticated holder must count toward connections_active; gauges: {:?}",
        recorder.gauge_names()
    );
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    // The russh client object is no longer needed: the proxy now owns the
    // gateway-facing half of the connection, and dropping the client must
    // not release anything on the gateway side — that is the hold.
    drop(holder);

    // Precondition: the holder owns the only permit, so a real client is
    // rejected at the connection cap. Proves the success below can't pass
    // vacuously.
    let denied = connect_and_auth(addr, client_key.clone()).await;
    assert!(
        denied.is_err(),
        "while the holder owns the only permit, a second connection must be \
         rejected at the connection cap"
    );

    // Mid-window probe, just past the grace and well before the slack: the
    // polite disconnect has been queued by now, but a peer that holds its
    // socket open must STILL be occupying the slot — only the force-close
    // may take it away.
    tokio::time::sleep_until(holder_at + GRACE + std::time::Duration::from_secs(1)).await;
    let mid_window = connect_and_auth(addr, client_key.clone()).await;
    if let Ok(session) = mid_window {
        // Only conclusive while still inside the force-close window: a
        // badly stalled runner could land this probe after the slack, at
        // which point the slot is legitimately free.
        let elapsed = holder_at.elapsed();
        assert!(
            elapsed >= GRACE + rio_gateway::server::FORCE_CLOSE_SLACK,
            "the holder's slot was released at ~grace+1s (elapsed {elapsed:?}), before \
             the force-close window — something other than the force-close tore the \
             held connection down, so this test is not exercising the socket-holding \
             path"
        );
        drop(session);
    }

    // Within grace + slack (+ generous scheduling margin) the gateway must
    // have force-closed the held transport and released its slot: a real
    // client can connect and authenticate. The polite disconnect alone can
    // never achieve this — the holder never acts on it.
    let force_close_budget =
        GRACE + rio_gateway::server::FORCE_CLOSE_SLACK + std::time::Duration::from_secs(5);
    let deadline = holder_at + force_close_budget;
    let mut admitted = None;
    while tokio::time::Instant::now() < deadline {
        match connect_and_auth(addr, client_key.clone()).await {
            Ok(session) => {
                admitted = Some(session);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
    assert!(
        admitted.is_some(),
        "a peer that holds its socket open after the grace disconnect must be \
         force-closed within grace + FORCE_CLOSE_SLACK, releasing the connection \
         slot for a real client"
    );

    // And the holder's accounting must be undone: with the follow-up
    // client now the only live connection, connections_active is back to
    // exactly one — the holder's increment was released by its drop. Poll
    // briefly; the handler drop runs as the transport task unwinds.
    let mut active = recorder.gauge_value(GAUGE);
    for _ in 0..40 {
        if active == Some(1.0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        active = recorder.gauge_value(GAUGE);
    }
    assert_eq!(
        active,
        Some(1.0),
        "connections_active must return to the follow-up client alone once the \
         held connection is force-closed; gauges: {:?}",
        recorder.gauge_names()
    );

    drop(admitted);
    proxy_task.abort();
    srv.abort();
    Ok(())
}

// ===========================================================================
// I-109 — authorized_keys hot-reload
// ===========================================================================

/// Like `spawn_ssh_server` but the authorized set is loaded from `path`
/// and a watcher polls it every 50ms. Returns the bound addr + server
/// handle; caller manages keys via the file.
async fn spawn_ssh_server_watching(
    path: std::path::PathBuf,
) -> anyhow::Result<(
    SocketAddr,
    rio_common::signal::Token,
    tokio::task::JoinHandle<()>,
)> {
    let (_store, store_addr, _sh) = spawn_mock_store().await?;
    let (_sched, sched_addr, _sch) = spawn_mock_scheduler().await?;
    let store_client = rio_proto::client::connect_single(&store_addr.to_string()).await?;
    let scheduler_client = rio_proto::client::connect_single(&sched_addr.to_string()).await?;

    let initial = rio_gateway::load_authorized_keys(&path)?;

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let config = Arc::new(build_ssh_config(host_key));

    let socket = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = socket.local_addr()?;

    let mut server = GatewayServer::new(store_client, scheduler_client, initial);
    let shutdown = rio_common::signal::Token::new();
    rio_gateway::spawn_authorized_keys_watcher(
        server.authorized_keys_handle(),
        path,
        std::time::Duration::from_millis(50),
        shutdown.clone(),
    );
    let srv_handle = tokio::spawn(async move {
        if let Err(e) = server.run_on_socket(config, &socket).await {
            eprintln!("ssh server error: {e}");
        }
    });

    Ok((addr, shutdown, srv_handle))
}

/// Attempt publickey auth against `addr` with `key`; return whether it
/// succeeded. Fresh connection per call so each attempt sees the
/// current (possibly reloaded) authorized set.
async fn try_auth(addr: SocketAddr, key: &PrivateKey) -> anyhow::Result<bool> {
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, addr, AcceptAllClient).await?;
    let res = session
        .authenticate_publickey(
            "nix",
            PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None),
        )
        .await?;
    Ok(res.success())
}

/// Atomic-replace `path` with `content` via rename-from-sibling. Mirrors
/// the kubelet `..data` symlink swap (new inode, never a half-written
/// file). Asserts the watcher's mtime poll picks up inode-replacement,
/// not just in-place edits.
fn atomic_write(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// I-109: gateway picks up `authorized_keys` changes without restart.
///
/// 1. Start with key A authorized → A accepted, B rejected.
/// 2. Atomically rewrite the file to authorize B only.
/// 3. After the watcher's poll interval → A rejected, B accepted.
///
/// SLEEP JUSTIFICATION: the watcher polls file mtime on a real
/// `tokio::time::interval`. We can't `pause()` time here — real TCP
/// (russh client + server) is in the loop, and `start_paused` +
/// real-socket is the documented auto-advance footgun. 200ms covers
/// 4× the 50ms test poll interval; bounded and deterministic enough.
#[tokio::test]
async fn test_authorized_keys_hot_reload() -> anyhow::Result<()> {
    common::init_test_logging();

    let key_a = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let key_b = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("authorized_keys");
    std::fs::write(&path, key_a.public_key().to_openssh()?)?;
    // mtime granularity on some filesystems is 1s; the watcher seeds
    // `last_mtime` from this write. Backdate it so the post-swap write
    // below is guaranteed a distinct mtime even on coarse-grained FS.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
    let f = std::fs::File::open(&path)?;
    f.set_modified(old)?;
    drop(f);

    let (addr, shutdown, srv) = spawn_ssh_server_watching(path.clone()).await?;

    assert!(try_auth(addr, &key_a).await?, "key A authorized at startup");
    assert!(!try_auth(addr, &key_b).await?, "key B not yet authorized");

    atomic_write(&path, &key_b.public_key().to_openssh()?)?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        !try_auth(addr, &key_a).await?,
        "key A must be rejected after reload"
    );
    assert!(
        try_auth(addr, &key_b).await?,
        "key B must be accepted after reload"
    );

    shutdown.cancel();
    srv.abort();
    Ok(())
}

/// I-109 failure-mode: a reload that yields zero valid keys (operator
/// botched the Secret) keeps the OLD set live rather than locking
/// everyone out. The watcher logs WARN and retries next tick.
#[tokio::test]
async fn test_authorized_keys_reload_failure_keeps_old_set() -> anyhow::Result<()> {
    common::init_test_logging();

    let key_a = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("authorized_keys");
    std::fs::write(&path, key_a.public_key().to_openssh()?)?;
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(10);
    let f = std::fs::File::open(&path)?;
    f.set_modified(old)?;
    drop(f);

    let (addr, shutdown, srv) = spawn_ssh_server_watching(path.clone()).await?;

    assert!(try_auth(addr, &key_a).await?, "key A authorized at startup");

    // Garbage content → load_authorized_keys bails ("no valid keys").
    atomic_write(&path, "not-a-key\n")?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        try_auth(addr, &key_a).await?,
        "key A still accepted — bad reload must not swap to empty set"
    );

    shutdown.cancel();
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

// r[verify gw.handshake.timeout]
/// bug_087: a client that authenticates and sends `exec_request` but
/// never sends `WORKER_MAGIC_1` must be disconnected after
/// `HANDSHAKE_TIMEOUT` (30s), not parked indefinitely. Pre-fix the
/// handshake read had no `timeout`/`select!{shutdown}` wrapper, so this
/// test would need a 601s `advance` to complete (via the opcode-idle
/// timer that the session never reaches).
///
/// Same `pause`/`advance` mechanics as
/// [`test_idle_timeout_fires_after_handshake`].
#[tokio::test(flavor = "current_thread")]
async fn test_handshake_timeout_fires_before_magic() -> anyhow::Result<()> {
    common::init_test_logging();
    // Bare `new()` — no handshake. Server is now blocked on
    // `timeout(30s, server_handshake_split(..))` waiting for
    // WORKER_MAGIC_1. We send nothing.
    let mut sess = common::GatewaySession::new().await?;

    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();

    // Handshake-timeout arm sends no STDERR (no negotiated wire to
    // send it on); server just returns Ok(()) and closes. The client
    // stream sees EOF.
    let mut buf = [0u8; 1];
    let n = tokio::io::AsyncReadExt::read(&mut sess.stream, &mut buf).await?;
    assert_eq!(n, 0, "server must close stream on handshake timeout (EOF)");

    // Bounded join: pre-fix the server task would still be parked.
    tokio::time::timeout(std::time::Duration::from_secs(5), sess.join_server())
        .await
        .expect("server must exit within 5s after handshake timeout");
    Ok(())
}
