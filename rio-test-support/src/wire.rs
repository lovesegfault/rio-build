//! Nix wire protocol client-side test helpers.
//!
//! These send the client half of the handshake/setOptions exchange against
//! a `DuplexStream`, `.unwrap()`-ing all wire I/O since they're for tests only.

use rio_nix::protocol::client::{StderrMessage, read_stderr_message};
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::{STDERR_LAST, StderrError};
use rio_nix::protocol::wire;
use tokio::io::{AsyncWriteExt, DuplexStream};

/// Perform the client side of the Nix worker protocol handshake.
///
/// Sends magic1 + version, reads magic2 + server version, exchanges features,
/// sends obsolete-affinity/reserve-space placeholders, reads version-string +
/// trusted flag + STDERR_LAST.
pub async fn do_handshake(s: &mut DuplexStream) {
    wire::write_u64(s, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(s, PROTOCOL_VERSION).await.unwrap();
    s.flush().await.unwrap();

    let magic2 = wire::read_u64(s).await.unwrap();
    assert_eq!(
        magic2, WORKER_MAGIC_2,
        "server must reply with WORKER_MAGIC_2"
    );
    let _server_version = wire::read_u64(s).await.unwrap();

    wire::write_strings(s, wire::NO_STRINGS).await.unwrap();
    s.flush().await.unwrap();
    let _server_features = wire::read_strings(s).await.unwrap();

    wire::write_u64(s, 0).await.unwrap(); // obsolete CPU affinity
    wire::write_u64(s, 0).await.unwrap(); // reserveSpace
    s.flush().await.unwrap();

    let _version = wire::read_string(s).await.unwrap();
    let _trusted = wire::read_u64(s).await.unwrap();

    let last = wire::read_u64(s).await.unwrap();
    assert_eq!(last, STDERR_LAST, "handshake should end with STDERR_LAST");
}

/// Send wopSetOptions (19) with all-zero values and empty overrides.
pub async fn send_set_options(s: &mut DuplexStream) {
    wire::write_u64(s, 19).await.unwrap(); // wopSetOptions
    for _ in 0..12 {
        wire::write_u64(s, 0).await.unwrap();
    }
    wire::write_u64(s, 0).await.unwrap(); // overrides count = 0
    s.flush().await.unwrap();

    let msg = wire::read_u64(s).await.unwrap();
    assert_eq!(msg, STDERR_LAST, "setOptions should end with STDERR_LAST");
}

/// Drain STDERR messages until STDERR_LAST. Returns all non-Last messages.
/// Panics if STDERR_ERROR is received (use `drain_stderr_expecting_error` for
/// error-path tests).
pub async fn drain_stderr_until_last(s: &mut DuplexStream) -> Vec<StderrMessage> {
    let mut msgs = Vec::new();
    loop {
        match read_stderr_message(s).await.unwrap() {
            StderrMessage::Last => return msgs,
            StderrMessage::Error(e) => {
                panic!("unexpected STDERR_ERROR: {}", e.message());
            }
            other => msgs.push(other),
        }
    }
}

/// Drain STDERR messages expecting STDERR_ERROR. Returns the error.
/// Panics if STDERR_LAST is received first.
pub async fn drain_stderr_expecting_error(s: &mut DuplexStream) -> StderrError {
    loop {
        match read_stderr_message(s).await.unwrap() {
            StderrMessage::Error(e) => return e,
            StderrMessage::Last => panic!("expected STDERR_ERROR but got STDERR_LAST"),
            _ => {} // skip other messages
        }
    }
}
