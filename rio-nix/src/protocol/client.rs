//! Client-side Nix worker protocol implementation.
//!
//! Speaks the Nix worker protocol as a **client** to a local `nix-daemon --stdio`.
//! Used by workers to drive nix-daemon --stdio in the build sandbox.
//!
//! The client protocol mirrors the server protocol in `handshake.rs` and `stderr.rs`:
//! - Client sends `WORKER_MAGIC_1`, reads `WORKER_MAGIC_2` + server version
//! - Client reads the STDERR loop (log messages, activities, errors)
//! - Client sends opcodes and reads results

use super::build::{BuildMode, BuildResult, read_build_result, write_basic_derivation};
use super::handshake::{
    HandshakeError, HandshakeResult, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2,
};
use super::opcodes::WorkerOp;
use super::pathinfo::{ValidPathInfo, read_valid_path_info};
use super::stderr::{
    ResultField, STDERR_ERROR, STDERR_LAST, STDERR_NEXT, STDERR_READ, STDERR_RESULT,
    STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_WRITE, StderrError,
};
use super::wire::{self, Result, WireError};
use crate::derivation::BasicDerivation;
use std::collections::BTreeSet;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// A message received from the daemon during the STDERR loop.
#[derive(Debug)]
pub enum StderrMessage {
    /// Log message (STDERR_NEXT).
    Next(String),
    /// Server requests data from client (STDERR_READ). Contains the byte count.
    Read(u64),
    /// Server sends data to client (STDERR_WRITE).
    Write(Vec<u8>),
    /// End of STDERR loop (STDERR_LAST). Result follows on the stream.
    Last,
    /// Error from the daemon (STDERR_ERROR).
    Error(StderrError),
    /// Structured activity start (STDERR_START_ACTIVITY).
    StartActivity {
        id: u64,
        level: u64,
        activity_type: u64,
        text: String,
        fields: Vec<ResultField>,
        parent_id: u64,
    },
    /// Structured activity stop (STDERR_STOP_ACTIVITY).
    StopActivity { id: u64 },
    /// Structured result for an activity (STDERR_RESULT).
    Result {
        activity_id: u64,
        result_type: u64,
        fields: Vec<ResultField>,
    },
}

/// Read a single STDERR message from the daemon.
pub async fn read_stderr_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<StderrMessage> {
    let msg_type = wire::read_u64(r).await?;

    match msg_type {
        STDERR_NEXT => {
            let msg = wire::read_string(r).await?;
            Ok(StderrMessage::Next(msg))
        }
        STDERR_READ => {
            let count = wire::read_u64(r).await?;
            Ok(StderrMessage::Read(count))
        }
        STDERR_WRITE => {
            let data = wire::read_bytes(r).await?;
            Ok(StderrMessage::Write(data))
        }
        STDERR_LAST => Ok(StderrMessage::Last),
        STDERR_ERROR => {
            let error = read_stderr_error(r).await?;
            Ok(StderrMessage::Error(error))
        }
        STDERR_START_ACTIVITY => {
            let id = wire::read_u64(r).await?;
            let level = wire::read_u64(r).await?;
            let activity_type = wire::read_u64(r).await?;
            let text = wire::read_string(r).await?;
            let fields = read_result_fields(r).await?;
            let parent_id = wire::read_u64(r).await?;
            Ok(StderrMessage::StartActivity {
                id,
                level,
                activity_type,
                text,
                fields,
                parent_id,
            })
        }
        STDERR_STOP_ACTIVITY => {
            let id = wire::read_u64(r).await?;
            Ok(StderrMessage::StopActivity { id })
        }
        STDERR_RESULT => {
            let activity_id = wire::read_u64(r).await?;
            let result_type = wire::read_u64(r).await?;
            let fields = read_result_fields(r).await?;
            Ok(StderrMessage::Result {
                activity_id,
                result_type,
                fields,
            })
        }
        unknown => Err(WireError::Io(std::io::Error::other(format!(
            "unknown STDERR message type: {unknown:#x}"
        )))),
    }
}

/// Read the structured fields of STDERR_ERROR.
async fn read_stderr_error<R: AsyncRead + Unpin>(r: &mut R) -> Result<StderrError> {
    let error_type = wire::read_string(r).await?;
    let level = wire::read_u64(r).await?;
    let name = wire::read_string(r).await?;
    let message = wire::read_string(r).await?;

    // Position
    let have_pos = wire::read_u64(r).await?;
    let position = if have_pos != 0 {
        let file = wire::read_string(r).await?;
        let line = wire::read_u64(r).await?;
        let column = wire::read_u64(r).await?;
        Some(super::stderr::Position::new(file, line, column))
    } else {
        None
    };

    // Traces
    let trace_count = wire::read_u64(r).await?;
    if trace_count > wire::MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(trace_count));
    }
    let mut traces = Vec::with_capacity(trace_count.min(64) as usize);
    for _ in 0..trace_count {
        let trace_have_pos = wire::read_u64(r).await?;
        let trace_pos = if trace_have_pos != 0 {
            let file = wire::read_string(r).await?;
            let line = wire::read_u64(r).await?;
            let column = wire::read_u64(r).await?;
            Some(super::stderr::Position::new(file, line, column))
        } else {
            None
        };
        let trace_msg = wire::read_string(r).await?;
        traces.push(super::stderr::Trace::new(trace_pos, trace_msg));
    }

    Ok(StderrError::new(
        error_type, level, name, message, position, traces,
    ))
}

/// Read typed result fields (used by START_ACTIVITY and RESULT).
async fn read_result_fields<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<ResultField>> {
    let count = wire::read_u64(r).await?;
    if count > wire::MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(count));
    }
    let mut fields = Vec::with_capacity(count.min(64) as usize);
    for _ in 0..count {
        let field_type = wire::read_u64(r).await?;
        let field = match field_type {
            0 => ResultField::Int(wire::read_u64(r).await?),
            1 => ResultField::String(wire::read_string(r).await?),
            _ => {
                return Err(WireError::Io(std::io::Error::other(format!(
                    "unknown result field type: {field_type}"
                ))));
            }
        };
        fields.push(field);
    }
    Ok(fields)
}

/// Maximum STDERR messages to consume before aborting.
///
/// Prevents infinite loops from a buggy or malicious daemon that sends
/// an unbounded stream of log/activity messages without ever sending
/// STDERR_LAST.
const MAX_STDERR_MESSAGES: u64 = 100_000;

/// Drain the STDERR loop until STDERR_LAST, discarding all messages.
///
/// Returns `Ok(())` on STDERR_LAST, `Err` on STDERR_ERROR or I/O error.
/// Aborts after `MAX_STDERR_MESSAGES` to prevent infinite loops.
pub(crate) async fn drain_stderr<R: AsyncRead + Unpin>(r: &mut R) -> Result<()> {
    for _ in 0..MAX_STDERR_MESSAGES {
        match read_stderr_message(r).await? {
            StderrMessage::Last => return Ok(()),
            StderrMessage::Error(e) => {
                return Err(WireError::Io(std::io::Error::other(format!(
                    "daemon error: {}",
                    e.message
                ))));
            }
            StderrMessage::Read(_) => {
                return Err(WireError::Io(std::io::Error::other(
                    "unexpected STDERR_READ during drain",
                )));
            }
            _ => {} // discard log/activity messages
        }
    }
    Err(WireError::Io(std::io::Error::other(format!(
        "exceeded {MAX_STDERR_MESSAGES} STDERR messages without STDERR_LAST"
    ))))
}

/// Error from a client-side daemon operation.
///
/// [`Daemon`](ClientOpError::Daemon) is the daemon's verdict on the
/// operation, delivered as an STDERR_ERROR frame with protocol framing intact
/// (the transport is not corrupted) — callers may report it as a daemon-side
/// rejection or retry the operation. [`Wire`](ClientOpError::Wire) means the
/// channel itself is suspect and must be abandoned.
#[derive(Debug, thiserror::Error)]
pub enum ClientOpError {
    /// The daemon refused the operation (STDERR_ERROR frame).
    #[error("daemon refused operation: {}", .0.message)]
    Daemon(StderrError),
    /// Wire-level failure (I/O, framing, bounds).
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Maximum STDERR messages [`drain_stderr_typed`] consumes before aborting.
///
/// Deliberately ~100× the legacy `MAX_STDERR_MESSAGES`: operations like
/// BuildPathsWithResults stream every build-log line through this drain, so a
/// count cap must never fire on a legitimate long build. It exists only to
/// cut off a pathological/looping daemon that never sends `STDERR_LAST`.
const MAX_BUILD_LOG_STDERR_MESSAGES: usize = 10_000_000;

/// Drain the STDERR loop until `STDERR_LAST`, surfacing `STDERR_ERROR` as
/// [`ClientOpError::Daemon`]. Log lines, activities, and results are
/// discarded (build output is not collected by the callers of this path).
/// `STDERR_READ`/`STDERR_WRITE` are protocol violations for the operations
/// this is used with and map to [`ClientOpError::Wire`].
///
/// Unlike `drain_stderr` (handshake/SetOptions-scale traffic, 100k cap), this
/// drain accepts build-scale log/activity streams and is bounded only by the
/// very high `MAX_BUILD_LOG_STDERR_MESSAGES` ceiling. Callers are expected to
/// bound wall-clock time with a per-op deadline (e.g.
/// `tokio::time::timeout`), since a message-count cap cannot protect against
/// a silently stalled peer.
pub async fn drain_stderr_typed<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::result::Result<(), ClientOpError> {
    drain_stderr_typed_bounded(r, MAX_BUILD_LOG_STDERR_MESSAGES).await
}

/// [`drain_stderr_typed`] with an explicit message-count ceiling. Returns a
/// [`ClientOpError::Wire`] I/O error if more than `max_messages` messages
/// arrive without `STDERR_LAST`.
async fn drain_stderr_typed_bounded<R: AsyncRead + Unpin>(
    r: &mut R,
    max_messages: usize,
) -> std::result::Result<(), ClientOpError> {
    for _ in 0..max_messages {
        match read_stderr_message(r).await? {
            StderrMessage::Last => return Ok(()),
            StderrMessage::Error(e) => return Err(ClientOpError::Daemon(e)),
            StderrMessage::Read(count) => {
                return Err(ClientOpError::Wire(WireError::Io(std::io::Error::other(
                    format!("unexpected STDERR_READ (n={count}) during client operation"),
                ))));
            }
            StderrMessage::Write(data) => {
                return Err(ClientOpError::Wire(WireError::Io(std::io::Error::other(
                    format!(
                        "unexpected STDERR_WRITE (len={}) during client operation",
                        data.len()
                    ),
                ))));
            }
            StderrMessage::Next(_)
            | StderrMessage::StartActivity { .. }
            | StderrMessage::StopActivity { .. }
            | StderrMessage::Result { .. } => continue,
        }
    }
    Err(ClientOpError::Wire(WireError::Io(std::io::Error::other(
        format!("exceeded {max_messages} stderr messages without STDERR_LAST"),
    ))))
}

// ---------------------------------------------------------------------------
// Client handshake
// ---------------------------------------------------------------------------

/// Perform the client-side handshake with a `nix-daemon --stdio` process.
///
/// Mirror of `server_handshake_split` in `handshake.rs`.
pub async fn client_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> std::result::Result<HandshakeResult, HandshakeError> {
    // Phase 1: Magic + version exchange
    wire::write_u64(writer, WORKER_MAGIC_1).await?;
    writer.flush().await.map_err(WireError::Io)?;

    let server_magic = wire::read_u64(reader).await?;
    if server_magic != WORKER_MAGIC_2 {
        return Err(HandshakeError::InvalidMagic(server_magic));
    }

    let server_version = wire::read_u64(reader).await?;
    if server_version < super::handshake::MIN_DAEMON_VERSION {
        // Mirror server_handshake's MIN_CLIENT_VERSION floor: an
        // older-than-expected daemon must fail HERE with a clear error,
        // not desync later at the first version-dependent read.
        return Err(HandshakeError::VersionTooOld {
            client_major: server_version >> 8,
            client_minor: server_version & 0xFF,
        });
    }
    wire::write_u64(writer, PROTOCOL_VERSION).await?;
    writer.flush().await.map_err(WireError::Io)?;

    let negotiated_version = server_version.min(PROTOCOL_VERSION);

    // Phase 2: Feature exchange (protocol >= 1.38)
    if negotiated_version >= super::handshake::encode_version(1, 38) {
        // Send empty client features
        wire::write_strings(writer, wire::NO_STRINGS).await?;
        writer.flush().await.map_err(WireError::Io)?;
        // Read server features
        let _server_features = wire::read_strings(reader).await?;
    }

    // Phase 3: Post-handshake
    // Send CPU affinity = 0, reserveSpace = 0
    wire::write_u64(writer, 0).await?;
    wire::write_u64(writer, 0).await?;
    writer.flush().await.map_err(WireError::Io)?;

    // Read server version string + trusted status
    let _version_string = wire::read_string(reader).await?;
    let _trusted = wire::read_u64(reader).await?;

    // Phase 4: Read initial STDERR_LAST
    drain_stderr(reader).await?;

    Ok(HandshakeResult::new(negotiated_version))
}

// ---------------------------------------------------------------------------
// Client opcodes
// ---------------------------------------------------------------------------

/// Send `wopSetOptions` to the local daemon.
///
/// `max_silent_time`: seconds without build output before the daemon
/// kills the builder. 0 = unbounded (daemon default). Maps to Nix's
/// `--max-silent-time`.
///
/// `build_cores`: value of `NIX_BUILD_CORES` in the builder's env.
/// 0 = "use all cores" (daemon substitutes `nproc`). Maps to Nix's
/// `--cores`. The daemon sets this env var inside the builder's
/// sandbox namespace; setting `NIX_BUILD_CORES` on the daemon
/// PROCESS would be ignored — `wopSetOptions` is the correct channel.
// r[impl nix.client.set-options]
pub async fn client_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    max_silent_time: u64,
    build_cores: u64,
) -> Result<()> {
    wire::write_u64(writer, WorkerOp::SetOptions as u64).await?;

    // keepFailed, keepGoing, tryFallback
    wire::write_bool(writer, false).await?;
    wire::write_bool(writer, false).await?;
    wire::write_bool(writer, false).await?;
    // verbosity
    wire::write_u64(writer, 0).await?;
    // maxBuildJobs
    wire::write_u64(writer, 1).await?;
    // maxSilentTime
    wire::write_u64(writer, max_silent_time).await?;
    // obsolete_useBuildHook (always 1)
    wire::write_u64(writer, 1).await?;
    // verboseBuild — encoded as a Verbosity level, NOT a bool. Nix
    // daemon.cc decodes via `lvlError == readInt()`, so 0=true, 7=false.
    // Ref client encodes `(verboseBuild ? lvlError : lvlVomit)`.
    wire::write_u64(writer, super::stderr::verbosity::VOMIT).await?;
    // obsolete_logType, obsolete_printBuildTrace
    wire::write_u64(writer, 0).await?;
    wire::write_u64(writer, 0).await?;
    // buildCores
    wire::write_u64(writer, build_cores).await?;
    // useSubstitutes
    wire::write_bool(writer, false).await?;
    // overrides (empty)
    wire::write_string_pairs::<_, &str, &str>(writer, &[]).await?;

    writer.flush().await?;

    // nix-daemon sends only STDERR_LAST for SetOptions
    drain_stderr(reader).await?;

    Ok(())
}

/// Write the `wopBuildDerivation` request payload (opcode + path + drv + mode) and flush.
///
/// Read side is left to the caller: loop on [`read_stderr_message`] until
/// [`StderrMessage::Last`], then call [`read_build_result`](super::build::read_build_result).
/// rio-builder's `run_daemon_build` does this with cancel-safe batching and a
/// silence deadline layered on top of the protocol read.
pub async fn client_send_build_derivation<W: AsyncWrite + Unpin>(
    writer: &mut W,
    drv_path: &str,
    drv: &BasicDerivation,
    build_mode: BuildMode,
) -> Result<()> {
    wire::write_u64(writer, WorkerOp::BuildDerivation as u64).await?;
    wire::write_string(writer, drv_path).await?;
    write_basic_derivation(writer, drv).await?;
    wire::write_u64(writer, build_mode as u64).await?;
    writer.flush().await?;
    Ok(())
}

/// One entry of a `wopBuildPathsWithResults` (46) response.
#[derive(Debug, Clone)]
pub struct KeyedBuildResult {
    /// The derived path exactly as echoed by the daemon (submission order).
    pub derived_path: String,
    /// The daemon's build result for that derived path.
    pub result: BuildResult,
}

/// Send `wopBuildPathsWithResults` (46) with `buildMode = Normal`: build the
/// given derived paths (`"<drvpath>!<out1>,<out2>"` or `"<drvpath>!*"`) and
/// collect the daemon's per-path [`BuildResult`]s.
///
/// Build logs and activities arrive on the STDERR loop and are discarded by
/// the typed drain; after `STDERR_LAST` the daemon replies with a count
/// followed by one (echoed derived path, [`BuildResult`]) entry per requested
/// path, in submission order.
///
/// `negotiated_version` comes from [`client_handshake`]; it gates
/// version-dependent [`BuildResult`] fields (e.g. cpu stats at >= 1.37).
pub async fn client_build_paths_with_results<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    derived_paths: &[S],
    negotiated_version: u64,
) -> std::result::Result<Vec<KeyedBuildResult>, ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsRef<str>,
{
    wire::write_u64(writer, WorkerOp::BuildPathsWithResults as u64).await?;
    wire::write_strings(writer, derived_paths).await?;
    wire::write_u64(writer, BuildMode::Normal as u64).await?;
    writer.flush().await.map_err(WireError::Io)?;

    drain_stderr_typed(reader).await?;

    let count = wire::read_u64(reader).await?;
    if count > wire::MAX_COLLECTION_COUNT {
        return Err(ClientOpError::Wire(WireError::CollectionTooLarge(count)));
    }
    let mut results = Vec::with_capacity(count.min(64) as usize);
    for _ in 0..count {
        let derived_path = wire::read_string(reader).await?;
        let result = read_build_result(reader, negotiated_version).await?;
        results.push(KeyedBuildResult {
            derived_path,
            result,
        });
    }
    Ok(results)
}

/// Send `wopQueryValidPaths` (31): ask which of `paths` the daemon already
/// has.
///
/// `substitute = false` mirrors `nix copy` behaviour: answering the query
/// must not trigger target-side substitution. With `substitute = true` the
/// daemon may try to substitute missing paths from its configured
/// substituters before answering, so paths it can fetch count as valid.
///
/// The daemon replies (after the STDERR loop) with the subset of `paths` it
/// considers valid, returned as a [`BTreeSet`] for deduplication and cheap
/// membership checks against the queried chunk.
pub async fn client_query_valid_paths<R, W, S>(
    reader: &mut R,
    writer: &mut W,
    paths: &[S],
    substitute: bool,
) -> std::result::Result<BTreeSet<String>, ClientOpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: AsRef<str>,
{
    wire::write_u64(writer, WorkerOp::QueryValidPaths as u64).await?;
    wire::write_strings(writer, paths).await?;
    wire::write_bool(writer, substitute).await?;
    writer.flush().await.map_err(WireError::Io)?;

    drain_stderr_typed(reader).await?;
    let valid = wire::read_strings(reader).await?;
    Ok(valid.into_iter().collect())
}

/// Send `wopQueryPathInfo` (26): fetch one path's [`ValidPathInfo`], `None`
/// if the daemon does not have the path.
///
/// The daemon replies (after the STDERR loop) with a found/not-found bool;
/// the [`ValidPathInfo`] body follows only when found.
pub async fn client_query_path_info<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    path: &str,
) -> std::result::Result<Option<ValidPathInfo>, ClientOpError> {
    wire::write_u64(writer, WorkerOp::QueryPathInfo as u64).await?;
    wire::write_string(writer, path).await?;
    writer.flush().await.map_err(WireError::Io)?;

    drain_stderr_typed(reader).await?;
    if !wire::read_bool(reader).await? {
        return Ok(None);
    }
    Ok(Some(read_valid_path_info(reader).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derivation::DerivationOutput;
    use crate::protocol::build::{
        BuildResult, BuildStatus, BuiltOutput, read_basic_derivation, read_build_result,
        write_build_result,
    };
    use crate::protocol::handshake::{MIN_CLIENT_VERSION, encode_version, server_handshake_split};
    use crate::protocol::pathinfo::write_valid_path_info;
    use crate::protocol::stderr::StderrWriter;

    fn test_drv() -> BasicDerivation {
        BasicDerivation::new(
            vec![DerivationOutput::new("out", "/nix/store/abc-hello", "", "").unwrap()],
            std::collections::BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec!["-c".into(), "true".into()],
            std::collections::BTreeMap::new(),
        )
        .unwrap()
    }

    /// Roundtrip the decomposed wopBuildDerivation client primitives the
    /// way rio-builder's `run_daemon_build` drives them:
    /// `client_send_build_derivation` → `read_stderr_message`* →
    /// `read_build_result`.
    // r[verify builder.daemon.stdio-client]
    #[tokio::test]
    async fn client_build_derivation_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let drv = test_drv();
        let drv_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello.drv";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::BuildDerivation as u64);
            let path = wire::read_string(&mut sr).await?;
            let received = read_basic_derivation(&mut sr).await?;
            let mode = wire::read_u64(&mut sr).await?;
            assert_eq!(mode, BuildMode::Normal as u64);

            wire::write_u64(&mut sw, STDERR_NEXT).await?;
            wire::write_string(&mut sw, "building...").await?;
            wire::write_u64(&mut sw, STDERR_NEXT).await?;
            wire::write_string(&mut sw, "done").await?;
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            write_build_result(
                &mut sw,
                &BuildResult {
                    status: BuildStatus::Built,
                    ..Default::default()
                },
                PROTOCOL_VERSION,
            )
            .await?;
            sw.flush().await?;
            anyhow::Ok((path, received))
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_send_build_derivation(&mut cw, drv_path, &drv, BuildMode::Normal).await?;

        let mut log_lines = Vec::new();
        let result = loop {
            match read_stderr_message(&mut cr).await? {
                StderrMessage::Last => break read_build_result(&mut cr, PROTOCOL_VERSION).await?,
                StderrMessage::Error(e) => panic!("daemon error: {}", e.message),
                StderrMessage::Next(s) => log_lines.push(s),
                _ => {}
            }
        };

        let (server_path, server_drv) = server.await??;
        assert_eq!(server_path, drv_path);
        assert_eq!(server_drv.platform(), drv.platform());
        assert_eq!(result.status, BuildStatus::Built);
        assert_eq!(log_lines, vec!["building...", "done"]);
        Ok(())
    }

    /// Test client handshake against our own server handshake.
    #[tokio::test]
    async fn client_handshake_against_server() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            server_handshake_split(&mut sr, &mut sw, "test-server 0.1.0").await
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let client_result = client_handshake(&mut cr, &mut cw).await?;

        let server_result = server_handle.await??;

        assert_eq!(
            client_result.negotiated_version(),
            server_result.negotiated_version()
        );
        assert!(client_result.negotiated_version() >= MIN_CLIENT_VERSION);
        Ok(())
    }

    /// Test client handshake with 1.37 server (no feature exchange).
    #[tokio::test]
    async fn client_handshake_v137_server() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_version = encode_version(1, 37);

        // Simulate a 1.37 server
        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);

            // Read client MAGIC_1
            let magic = wire::read_u64(&mut sr).await?;
            assert_eq!(magic, WORKER_MAGIC_1);

            // Send MAGIC_2 + version
            wire::write_u64(&mut sw, WORKER_MAGIC_2).await?;
            wire::write_u64(&mut sw, server_version).await?;
            sw.flush().await?;

            // Read client version
            let _client_version = wire::read_u64(&mut sr).await?;

            // NO feature exchange for 1.37

            // Read affinity + reserveSpace
            let _affinity = wire::read_u64(&mut sr).await?;
            let _reserve = wire::read_u64(&mut sr).await?;

            // Send version string + trusted
            wire::write_string(&mut sw, "test-daemon 1.37").await?;
            wire::write_u64(&mut sw, 1).await?;
            sw.flush().await?;

            // Send STDERR_LAST
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let result = client_handshake(&mut cr, &mut cw).await?;

        assert_eq!(result.negotiated_version(), server_version);
        server_handle.await??;
        Ok(())
    }

    /// A daemon advertising < `MIN_DAEMON_VERSION` is rejected at
    /// handshake. Mirror of `handshake::test_version_too_old_rejected`.
    /// Regression: previously `client_handshake` had no floor, so a
    /// 1.34 daemon negotiated successfully and then desynced at
    /// `read_build_result`'s `>=1.37` cpu-field read.
    #[tokio::test]
    async fn client_handshake_rejects_v134_daemon() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_version = encode_version(1, 34);

        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let _magic = wire::read_u64(&mut sr).await?;
            wire::write_u64(&mut sw, WORKER_MAGIC_2).await?;
            wire::write_u64(&mut sw, server_version).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let err = client_handshake(&mut cr, &mut cw)
            .await
            .expect_err("1.34 daemon must be rejected at handshake");
        assert!(
            matches!(
                err,
                HandshakeError::VersionTooOld {
                    client_major: 1,
                    client_minor: 34
                }
            ),
            "got: {err:?}"
        );
        server_handle.await??;
        Ok(())
    }

    /// Test reading STDERR messages.
    #[tokio::test]
    async fn read_stderr_messages() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let aid;
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("building foo").await?;
            aid = w
                .start_activity(
                    crate::protocol::stderr::ActivityType::Build,
                    "building",
                    0,
                    0,
                    &[],
                )
                .await?;
            w.stop_activity(aid).await?;
            w.finish().await?;
        }

        let mut reader = std::io::Cursor::new(buf);
        // Read NEXT
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::Next(ref s) if s == "building foo"));

        // Read START_ACTIVITY
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::StartActivity { id, .. } if id == aid));

        // Read STOP_ACTIVITY
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::StopActivity { id } if id == aid));

        // Read LAST
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::Last));
        Ok(())
    }

    /// Test reading STDERR_ERROR.
    #[tokio::test]
    async fn read_stderr_error_message() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.error(&StderrError::simple("test", "something failed"))
                .await?;
        }

        let mut reader = std::io::Cursor::new(buf);
        let msg = read_stderr_message(&mut reader).await?;
        match msg {
            StderrMessage::Error(e) => {
                assert_eq!(e.error_type, "Error");
                assert_eq!(e.name, "test");
                assert_eq!(e.message, "something failed");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // STDERR variant coverage: READ, WRITE, RESULT, unknown, bounds
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_read_stderr_read_variant() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 1024).await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        assert!(matches!(msg, StderrMessage::Read(1024)));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_write_variant() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_WRITE).await?;
        wire::write_bytes(&mut buf, b"payload").await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        assert!(matches!(msg, StderrMessage::Write(ref d) if d == b"payload"));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_result_variant_with_fields() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 42).await?; // activity_id
        wire::write_u64(&mut buf, 7).await?; // result_type
        wire::write_u64(&mut buf, 2).await?; // field count
        // Field 0: Int(123)
        wire::write_u64(&mut buf, 0).await?; // field_type = Int
        wire::write_u64(&mut buf, 123).await?;
        // Field 1: String("hi")
        wire::write_u64(&mut buf, 1).await?; // field_type = String
        wire::write_string(&mut buf, "hi").await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        match msg {
            StderrMessage::Result {
                activity_id,
                result_type,
                fields,
            } => {
                assert_eq!(activity_id, 42);
                assert_eq!(result_type, 7);
                assert_eq!(fields.len(), 2);
                assert!(matches!(fields[0], ResultField::Int(123)));
                assert!(matches!(&fields[1], ResultField::String(s) if s == "hi"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
        Ok(())
    }

    /// Full STDERR_ERROR with position + traces — exercises the have_pos
    /// branch and the trace-with-position loop in read_stderr_error.
    #[tokio::test]
    async fn test_read_stderr_error_with_position_and_traces() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_ERROR).await?;
        wire::write_string(&mut buf, "Error").await?; // error_type
        wire::write_u64(&mut buf, 1).await?; // level
        wire::write_string(&mut buf, "nix::EvalError").await?; // name
        wire::write_string(&mut buf, "undefined variable").await?; // message
        // Position: have_pos=1, file/line/column
        wire::write_u64(&mut buf, 1).await?;
        wire::write_string(&mut buf, "default.nix").await?;
        wire::write_u64(&mut buf, 10).await?;
        wire::write_u64(&mut buf, 5).await?;
        // Traces: count=2
        wire::write_u64(&mut buf, 2).await?;
        // Trace 0: with position
        wire::write_u64(&mut buf, 1).await?;
        wire::write_string(&mut buf, "lib.nix").await?;
        wire::write_u64(&mut buf, 3).await?;
        wire::write_u64(&mut buf, 1).await?;
        wire::write_string(&mut buf, "while calling").await?;
        // Trace 1: no position
        wire::write_u64(&mut buf, 0).await?;
        wire::write_string(&mut buf, "from CLI").await?;

        let msg = read_stderr_message(&mut std::io::Cursor::new(buf)).await?;
        match msg {
            StderrMessage::Error(e) => {
                assert_eq!(e.message, "undefined variable");
                let pos = e.position.expect("should have position");
                assert_eq!(pos.file, "default.nix");
                assert_eq!(pos.line, 10);
                assert_eq!(pos.column, 5);
                assert_eq!(e.traces.len(), 2);
                assert!(e.traces[0].position.is_some());
                assert_eq!(e.traces[0].message, "while calling");
                assert!(e.traces[1].position.is_none());
                assert_eq!(e.traces[1].message, "from CLI");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_unknown_message_type() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, 0xDEADBEEF).await?;

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("unknown type should error");
        assert!(err.to_string().contains("unknown STDERR message type"));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_result_fields_unknown_field_type() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 1).await?; // activity_id
        wire::write_u64(&mut buf, 0).await?; // result_type
        wire::write_u64(&mut buf, 1).await?; // field count
        wire::write_u64(&mut buf, 99).await?; // field_type = invalid

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("unknown field type should error");
        assert!(err.to_string().contains("unknown result field type"));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_result_fields_too_large() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_RESULT).await?;
        wire::write_u64(&mut buf, 1).await?;
        wire::write_u64(&mut buf, 0).await?;
        wire::write_u64(&mut buf, wire::MAX_COLLECTION_COUNT + 1).await?; // field count > max

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("oversized count should error");
        assert!(matches!(err, WireError::CollectionTooLarge(_)));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_stderr_error_traces_too_large() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_ERROR).await?;
        wire::write_string(&mut buf, "Error").await?;
        wire::write_u64(&mut buf, 0).await?; // level
        wire::write_string(&mut buf, "name").await?;
        wire::write_string(&mut buf, "msg").await?;
        wire::write_u64(&mut buf, 0).await?; // have_pos
        wire::write_u64(&mut buf, wire::MAX_COLLECTION_COUNT + 1).await?; // trace_count > max

        let err = read_stderr_message(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("oversized trace count should error");
        assert!(matches!(err, WireError::CollectionTooLarge(_)));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // drain_stderr error paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_drain_stderr_daemon_error_aborts() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("doing stuff").await?;
            w.error(&StderrError::simple("Build", "oh no")).await?;
        }

        let err = drain_stderr(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("drain should error on STDERR_ERROR");
        assert!(err.to_string().contains("daemon error"));
        assert!(err.to_string().contains("oh no"));
        Ok(())
    }

    #[tokio::test]
    async fn test_drain_stderr_unexpected_read_aborts() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 10).await?;

        let err = drain_stderr(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("STDERR_READ during drain should abort");
        assert!(
            err.to_string()
                .contains("unexpected STDERR_READ during drain")
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // drain_stderr_typed: daemon refusal vs wire failure
    // -----------------------------------------------------------------------

    /// A structured STDERR_ERROR from the daemon surfaces as
    /// `ClientOpError::Daemon` with the daemon's message text intact, after
    /// passing over a preceding log line.
    #[tokio::test]
    async fn drain_stderr_typed_surfaces_daemon_error() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (_sr, sw) = tokio::io::split(server_stream);
            let mut w = StderrWriter::new(sw);
            w.log("about to fail").await?;
            w.error(&StderrError::simple("rio-build", "tenant quota exceeded"))
                .await?;
            anyhow::Ok(())
        });

        let (mut cr, _cw) = tokio::io::split(client_stream);
        let err = drain_stderr_typed(&mut cr)
            .await
            .expect_err("STDERR_ERROR must surface as an error");
        match &err {
            ClientOpError::Daemon(e) => {
                assert_eq!(e.message, "tenant quota exceeded");
            }
            other => panic!("expected ClientOpError::Daemon, got {other:?}"),
        }
        // The daemon's text must survive into the human-readable Display.
        assert!(
            err.to_string().contains("tenant quota exceeded"),
            "Display must carry the daemon message: {err}"
        );
        server.await??;
        Ok(())
    }

    /// Activities, structured results, and log lines are discarded; the drain
    /// completes with `Ok(())` at STDERR_LAST.
    #[tokio::test]
    async fn drain_stderr_typed_passes_activities_and_stops_at_last() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (_sr, sw) = tokio::io::split(server_stream);
            let mut w = StderrWriter::new(sw);
            let aid = w
                .start_activity(
                    crate::protocol::stderr::ActivityType::Build,
                    "building foo",
                    0,
                    0,
                    &[],
                )
                .await?;
            w.result(
                aid,
                crate::protocol::stderr::ResultType::BuildLogLine,
                &[ResultField::String("checking for gcc... yes".into())],
            )
            .await?;
            w.stop_activity(aid).await?;
            w.finish().await?;
            anyhow::Ok(())
        });

        let (mut cr, _cw) = tokio::io::split(client_stream);
        drain_stderr_typed(&mut cr).await?;
        server.await??;
        Ok(())
    }

    /// STDERR_READ is a protocol violation for the operations this drain
    /// serves: it maps to `ClientOpError::Wire` and the message names the
    /// violating frame and its requested byte count.
    #[tokio::test]
    async fn drain_stderr_typed_read_frame_is_wire_violation() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        wire::write_u64(&mut buf, STDERR_READ).await?;
        wire::write_u64(&mut buf, 4096).await?;

        let err = drain_stderr_typed(&mut std::io::Cursor::new(buf))
            .await
            .expect_err("STDERR_READ must abort the typed drain");
        assert!(matches!(err, ClientOpError::Wire(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("STDERR_READ"),
            "message must name the violating frame: {err}"
        );
        assert!(
            err.to_string().contains("4096"),
            "message must carry the requested byte count: {err}"
        );
        Ok(())
    }

    /// The message-count ceiling aborts a drain whose peer keeps streaming
    /// log lines without ever sending STDERR_LAST.
    #[tokio::test]
    async fn drain_stderr_typed_bounded_aborts_past_max_messages() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            for _ in 0..5 {
                w.log("still going").await?;
            }
        }

        let err = drain_stderr_typed_bounded(&mut std::io::Cursor::new(buf), 3)
            .await
            .expect_err("exceeding the message ceiling must abort the drain");
        assert!(matches!(err, ClientOpError::Wire(_)), "got: {err:?}");
        assert!(
            err.to_string().contains("exceeded 3 stderr messages"),
            "message must mention the exceeded bound: {err}"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // client_set_options + client_handshake error path
    // -----------------------------------------------------------------------

    /// client_set_options wire layout round-trip. Field order and values
    /// per Nix src/libstore/daemon.cc case SetOptions.
    // r[verify nix.client.set-options]
    #[tokio::test]
    async fn test_client_set_options_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            // Opcode
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::SetOptions as u64);
            // 13 fields in the exact order the client sends them
            let keep_failed = wire::read_bool(&mut sr).await?;
            let keep_going = wire::read_bool(&mut sr).await?;
            let try_fallback = wire::read_bool(&mut sr).await?;
            assert!(!keep_failed && !keep_going && !try_fallback);
            let verbosity = wire::read_u64(&mut sr).await?;
            assert_eq!(verbosity, 0);
            let max_build_jobs = wire::read_u64(&mut sr).await?;
            assert_eq!(max_build_jobs, 1);
            let max_silent_time = wire::read_u64(&mut sr).await?;
            assert_eq!(max_silent_time, 0, "zero → unbounded (daemon default)");
            let obsolete_use_build_hook = wire::read_u64(&mut sr).await?;
            assert_eq!(obsolete_use_build_hook, 1);
            // Nix decodes via `lvlError == readInt()`; 7 (lvlVomit) = false.
            let verbose_build = wire::read_u64(&mut sr).await?;
            assert_eq!(verbose_build, super::super::stderr::verbosity::VOMIT);
            let _obsolete_log_type = wire::read_u64(&mut sr).await?;
            let _obsolete_print_build_trace = wire::read_u64(&mut sr).await?;
            let build_cores = wire::read_u64(&mut sr).await?;
            assert_eq!(build_cores, 0, "zero → daemon substitutes nproc");
            let use_substitutes = wire::read_bool(&mut sr).await?;
            assert!(!use_substitutes);
            let overrides = wire::read_string_pairs(&mut sr).await?;
            assert!(overrides.is_empty());
            // Send STDERR_LAST to unblock the client's drain_stderr.
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_set_options(&mut cr, &mut cw, 0, 0).await?;
        server.await??;
        Ok(())
    }

    /// Nonzero `max_silent_time` / `build_cores` appear at the correct
    /// wire positions. Field positions per Nix `src/libstore/daemon.cc`
    /// case SetOptions — same layout as the roundtrip test above, but
    /// asserts the params aren't hardcoded-0 (the phase4a P1 plumbing
    /// gap: scheduler sent per-tenant limits, worker ignored them).
    #[tokio::test]
    async fn test_client_set_options_plumbs_values() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::SetOptions as u64);
            // keepFailed, keepGoing, tryFallback, verbosity, maxBuildJobs
            wire::read_bool(&mut sr).await?;
            wire::read_bool(&mut sr).await?;
            wire::read_bool(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            // maxSilentTime — position 6
            let mst = wire::read_u64(&mut sr).await?;
            assert_eq!(mst, 3600, "max_silent_time must reach the wire");
            // useBuildHook, verboseBuild, logType, printBuildTrace
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            wire::read_u64(&mut sr).await?;
            // buildCores — position 11
            let bc = wire::read_u64(&mut sr).await?;
            assert_eq!(bc, 4, "build_cores must reach the wire");
            // useSubstitutes, overrides
            wire::read_bool(&mut sr).await?;
            wire::read_string_pairs(&mut sr).await?;
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        client_set_options(&mut cr, &mut cw, 3600, 4).await?;
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn test_client_handshake_bad_magic() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(64);

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            // Read client MAGIC_1 then send WRONG magic.
            let _m1 = wire::read_u64(&mut sr).await?;
            wire::write_u64(&mut sw, 0xBADC0FFE).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let err = client_handshake(&mut cr, &mut cw)
            .await
            .expect_err("bad magic should fail handshake");
        assert!(matches!(err, HandshakeError::InvalidMagic(0xBADC0FFE)));
        server.await??;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // client_query_valid_paths / client_query_path_info
    // -----------------------------------------------------------------------

    /// client_query_valid_paths wire layout round-trip: the request carries
    /// the opcode, the path collection, and the substitute flag in the order
    /// the gateway's `handle_query_valid_paths` reads them; the response
    /// (after STDERR_LAST) is the daemon's collection of valid paths.
    #[tokio::test]
    async fn client_query_valid_paths_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let path_a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello";
        let path_b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryValidPaths as u64);
            let paths = wire::read_strings(&mut sr).await?;
            assert_eq!(paths, vec![path_a.to_string(), path_b.to_string()]);
            let substitute = wire::read_bool(&mut sr).await?;
            assert!(!substitute, "substitute flag must reach the wire as false");
            // Response: STDERR_LAST, then the valid subset (only path_a).
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_strings(&mut sw, &[path_a]).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let valid = client_query_valid_paths(&mut cr, &mut cw, &[path_a, path_b], false).await?;
        server.await??;
        assert_eq!(valid, BTreeSet::from([path_a.to_string()]));
        Ok(())
    }

    /// Two sequential client_query_path_info calls on one connection: a
    /// found path yields `Some(ValidPathInfo)`, a missing path yields
    /// `None`. Found/not-found is the bool the gateway's
    /// `handle_query_path_info` writes after STDERR_LAST, with the
    /// `ValidPathInfo` body following only when found.
    #[tokio::test]
    async fn client_query_path_info_found_and_missing() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let found_path = "/nix/store/cccccccccccccccccccccccccccccccc-found";
        let missing_path = "/nix/store/dddddddddddddddddddddddddddddddd-missing";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);

            // Call 1: path present — bool(true) + ValidPathInfo body.
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryPathInfo as u64);
            let path = wire::read_string(&mut sr).await?;
            assert_eq!(path, found_path);
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_bool(&mut sw, true).await?;
            write_valid_path_info(
                &mut sw,
                &ValidPathInfo {
                    nar_hash: vec![0xab; 32],
                    nar_size: 123,
                    ..Default::default()
                },
            )
            .await?;
            sw.flush().await?;

            // Call 2: path missing — bool(false), nothing else.
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryPathInfo as u64);
            let path = wire::read_string(&mut sr).await?;
            assert_eq!(path, missing_path);
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_bool(&mut sw, false).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let info = client_query_path_info(&mut cr, &mut cw, found_path).await?;
        assert_eq!(info.expect("path should be found").nar_size, 123);
        let missing = client_query_path_info(&mut cr, &mut cw, missing_path).await?;
        assert!(missing.is_none(), "missing path must map to None");
        server.await??;
        Ok(())
    }

    /// `substitute: true` is plumbed to the wire: the fake server reads the
    /// flag after the path collection and asserts it arrives as true.
    #[tokio::test]
    async fn client_query_valid_paths_substitute_true_plumbed() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let path = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-subst";

        let server = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            let op = wire::read_u64(&mut sr).await?;
            assert_eq!(op, WorkerOp::QueryValidPaths as u64);
            let paths = wire::read_strings(&mut sr).await?;
            assert_eq!(paths, vec![path.to_string()]);
            let substitute = wire::read_bool(&mut sr).await?;
            assert!(substitute, "substitute flag must reach the wire as true");
            wire::write_u64(&mut sw, STDERR_LAST).await?;
            wire::write_strings(&mut sw, &[path]).await?;
            sw.flush().await?;
            anyhow::Ok(())
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let valid = client_query_valid_paths(&mut cr, &mut cw, &[path], true).await?;
        server.await??;
        assert_eq!(valid, BTreeSet::from([path.to_string()]));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // client_build_paths_with_results
    // -----------------------------------------------------------------------

    /// client_build_paths_with_results wire layout round-trip: the request
    /// carries the opcode, the derived-path collection, and buildMode Normal
    /// in the order the gateway's `handle_build_paths_with_results` reads
    /// them; the response (after STDERR_LAST) is a count followed by
    /// per-entry (derived path echo, BuildResult) pairs whose statuses,
    /// error message, and built outputs must be preserved.
    #[tokio::test]
    async fn client_build_paths_with_results_roundtrip() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let dp_all = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv!*";
        let dp_out = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y.drv!out";
        let built_out = BuiltOutput {
            drv_output_id: "sha256:abcdef0123456789!out".to_string(),
            out_path: "/nix/store/cccccccccccccccccccccccccccccccc-x".to_string(),
        };

        let server = tokio::spawn({
            let built_out = built_out.clone();
            async move {
                let (mut sr, mut sw) = tokio::io::split(server_stream);
                let op = wire::read_u64(&mut sr).await?;
                assert_eq!(op, WorkerOp::BuildPathsWithResults as u64);
                let paths = wire::read_strings(&mut sr).await?;
                assert_eq!(paths, vec![dp_all.to_string(), dp_out.to_string()]);
                let mode = wire::read_u64(&mut sr).await?;
                assert_eq!(mode, BuildMode::Normal as u64);

                // Build logs/activities stream on the STDERR loop first.
                wire::write_u64(&mut sw, STDERR_NEXT).await?;
                wire::write_string(&mut sw, "building x...").await?;
                wire::write_u64(&mut sw, STDERR_LAST).await?;
                // Then: count, and per entry the echoed derived path + result.
                wire::write_u64(&mut sw, 2).await?;
                wire::write_string(&mut sw, dp_all).await?;
                write_build_result(
                    &mut sw,
                    &BuildResult {
                        status: BuildStatus::Built,
                        times_built: 1,
                        built_outputs: vec![built_out],
                        ..Default::default()
                    },
                    PROTOCOL_VERSION,
                )
                .await?;
                wire::write_string(&mut sw, dp_out).await?;
                write_build_result(
                    &mut sw,
                    &BuildResult::failure(BuildStatus::PermanentFailure, "boom"),
                    PROTOCOL_VERSION,
                )
                .await?;
                sw.flush().await?;
                anyhow::Ok(())
            }
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let results =
            client_build_paths_with_results(&mut cr, &mut cw, &[dp_all, dp_out], PROTOCOL_VERSION)
                .await?;
        server.await??;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].derived_path, dp_all);
        assert_eq!(results[0].result.status, BuildStatus::Built);
        assert_eq!(
            results[0].result.built_outputs,
            vec![built_out],
            "built output of the successful entry must be preserved"
        );
        assert_eq!(results[1].derived_path, dp_out);
        assert_eq!(results[1].result.status, BuildStatus::PermanentFailure);
        assert_eq!(results[1].result.error_msg, "boom");
        Ok(())
    }
}
