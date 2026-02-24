//! Client-side Nix worker protocol implementation.
//!
//! Speaks the Nix worker protocol as a **client** to a local `nix-daemon --stdio`.
//! Used by the gateway in Phase 1b for local build execution, and by workers
//! in Phase 2 for the same purpose.
//!
//! The client protocol mirrors the server protocol in `handshake.rs` and `stderr.rs`:
//! - Client sends `WORKER_MAGIC_1`, reads `WORKER_MAGIC_2` + server version
//! - Client reads the STDERR loop (log messages, activities, errors)
//! - Client sends opcodes and reads results

use super::build::{
    BuildMode, BuildResult, BuildStatus, read_build_result, write_basic_derivation,
};
use super::handshake::{
    HandshakeError, HandshakeResult, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2,
};
use super::opcodes::WorkerOp;
use super::stderr::{
    ResultField, STDERR_ERROR, STDERR_LAST, STDERR_NEXT, STDERR_READ, STDERR_RESULT,
    STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_WRITE, StderrError,
};
use super::wire::{self, WireError};
use crate::derivation::BasicDerivation;
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
pub async fn read_stderr_message<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<StderrMessage, WireError> {
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
async fn read_stderr_error<R: AsyncRead + Unpin>(r: &mut R) -> Result<StderrError, WireError> {
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
async fn read_result_fields<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Vec<ResultField>, WireError> {
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
/// Aborts after [`MAX_STDERR_MESSAGES`] to prevent infinite loops.
pub async fn drain_stderr<R: AsyncRead + Unpin>(r: &mut R) -> Result<(), WireError> {
    for _ in 0..MAX_STDERR_MESSAGES {
        match read_stderr_message(r).await? {
            StderrMessage::Last => return Ok(()),
            StderrMessage::Error(e) => {
                return Err(WireError::Io(std::io::Error::other(format!(
                    "daemon error: {}",
                    e.message()
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

// ---------------------------------------------------------------------------
// Client handshake
// ---------------------------------------------------------------------------

/// Perform the client-side handshake with a `nix-daemon --stdio` process.
///
/// Mirror of `server_handshake_split` in `handshake.rs`.
pub async fn client_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> Result<HandshakeResult, HandshakeError> {
    // Phase 1: Magic + version exchange
    wire::write_u64(writer, WORKER_MAGIC_1).await?;
    writer.flush().await.map_err(WireError::Io)?;

    let server_magic = wire::read_u64(reader).await?;
    if server_magic != WORKER_MAGIC_2 {
        return Err(HandshakeError::InvalidMagic(server_magic));
    }

    let server_version = wire::read_u64(reader).await?;
    wire::write_u64(writer, PROTOCOL_VERSION).await?;
    writer.flush().await.map_err(WireError::Io)?;

    let negotiated_version = server_version.min(PROTOCOL_VERSION);

    // Phase 2: Feature exchange (protocol >= 1.38)
    if negotiated_version >= super::handshake::encode_version(1, 38) {
        // Send empty client features
        wire::write_strings(writer, &[]).await?;
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
/// Sends minimal default options. The local daemon needs this before
/// it will accept build requests.
pub async fn client_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
) -> Result<(), WireError> {
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
    wire::write_u64(writer, 0).await?;
    // obsolete_useBuildHook (always 1)
    wire::write_u64(writer, 1).await?;
    // verboseBuild
    wire::write_bool(writer, false).await?;
    // obsolete_logType, obsolete_printBuildTrace
    wire::write_u64(writer, 0).await?;
    wire::write_u64(writer, 0).await?;
    // buildCores
    wire::write_u64(writer, 0).await?;
    // useSubstitutes
    wire::write_bool(writer, false).await?;
    // overrides (empty)
    wire::write_string_pairs(writer, &[]).await?;

    writer.flush().await?;

    // nix-daemon sends only STDERR_LAST for SetOptions
    drain_stderr(reader).await?;

    Ok(())
}

/// Send `wopBuildDerivation` to the local daemon and collect the result.
///
/// The caller should process STDERR messages (forwarding logs to the
/// remote client) by calling `read_stderr_message` in a loop. This
/// function is a convenience that drains STDERR and returns only the
/// `BuildResult`.
///
/// For log forwarding, use the lower-level `send_build_derivation` +
/// `read_stderr_message` loop instead.
pub async fn client_build_derivation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    drv_path: &str,
    basic_drv: &BasicDerivation,
    build_mode: BuildMode,
) -> Result<BuildResult, WireError> {
    // Send opcode
    wire::write_u64(writer, WorkerOp::BuildDerivation as u64).await?;
    wire::write_string(writer, drv_path).await?;

    // Send BasicDerivation
    write_basic_derivation(writer, basic_drv).await?;

    // Send buildMode
    wire::write_u64(writer, build_mode as u64).await?;
    writer.flush().await?;

    // Read STDERR loop until STDERR_LAST
    loop {
        match read_stderr_message(reader).await? {
            StderrMessage::Last => break,
            StderrMessage::Error(e) => {
                return Ok(BuildResult::failure(
                    BuildStatus::MiscFailure,
                    e.message().to_string(),
                ));
            }
            StderrMessage::Next(msg) => {
                tracing::debug!(target: "nix-daemon", "{}", msg);
            }
            StderrMessage::Read(_) => {
                return Ok(BuildResult::failure(
                    BuildStatus::MiscFailure,
                    "daemon sent STDERR_READ, not supported for build forwarding".to_string(),
                ));
            }
            _ => {} // discard activity messages
        }
    }

    // Read BuildResult
    read_build_result(reader).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::handshake::{MIN_CLIENT_VERSION, encode_version, server_handshake_split};
    use crate::protocol::stderr::StderrWriter;

    /// Test client handshake against our own server handshake.
    #[tokio::test]
    async fn client_handshake_against_server() {
        let (client_stream, server_stream) = tokio::io::duplex(8192);

        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);
            server_handshake_split(&mut sr, &mut sw, "test-server 0.1.0").await
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let client_result = client_handshake(&mut cr, &mut cw).await.unwrap();

        let server_result = server_handle.await.unwrap().unwrap();

        assert_eq!(
            client_result.negotiated_version(),
            server_result.negotiated_version()
        );
        assert!(client_result.negotiated_version() >= MIN_CLIENT_VERSION);
    }

    /// Test client handshake with 1.37 server (no feature exchange).
    #[tokio::test]
    async fn client_handshake_v137_server() {
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server_version = encode_version(1, 37);

        // Simulate a 1.37 server
        let server_handle = tokio::spawn(async move {
            let (mut sr, mut sw) = tokio::io::split(server_stream);

            // Read client MAGIC_1
            let magic = wire::read_u64(&mut sr).await.unwrap();
            assert_eq!(magic, WORKER_MAGIC_1);

            // Send MAGIC_2 + version
            wire::write_u64(&mut sw, WORKER_MAGIC_2).await.unwrap();
            wire::write_u64(&mut sw, server_version).await.unwrap();
            sw.flush().await.unwrap();

            // Read client version
            let _client_version = wire::read_u64(&mut sr).await.unwrap();

            // NO feature exchange for 1.37

            // Read affinity + reserveSpace
            let _affinity = wire::read_u64(&mut sr).await.unwrap();
            let _reserve = wire::read_u64(&mut sr).await.unwrap();

            // Send version string + trusted
            wire::write_string(&mut sw, "test-daemon 1.37")
                .await
                .unwrap();
            wire::write_u64(&mut sw, 1).await.unwrap();
            sw.flush().await.unwrap();

            // Send STDERR_LAST
            wire::write_u64(&mut sw, STDERR_LAST).await.unwrap();
            sw.flush().await.unwrap();
        });

        let (mut cr, mut cw) = tokio::io::split(client_stream);
        let result = client_handshake(&mut cr, &mut cw).await.unwrap();

        assert_eq!(result.negotiated_version(), server_version);
        server_handle.await.unwrap();
    }

    /// Test reading STDERR messages.
    #[tokio::test]
    async fn read_stderr_messages() {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("building foo").await.unwrap();
            let _id = w
                .start_activity(
                    crate::protocol::stderr::ActivityType::Build,
                    "building",
                    0,
                    0,
                )
                .await
                .unwrap();
            w.stop_activity(1).await.unwrap();
            w.finish().await.unwrap();
        }

        let mut reader = std::io::Cursor::new(buf);
        // Read NEXT
        let msg = read_stderr_message(&mut reader).await.unwrap();
        assert!(matches!(msg, StderrMessage::Next(ref s) if s == "building foo"));

        // Read START_ACTIVITY
        let msg = read_stderr_message(&mut reader).await.unwrap();
        assert!(matches!(msg, StderrMessage::StartActivity { id: 1, .. }));

        // Read STOP_ACTIVITY
        let msg = read_stderr_message(&mut reader).await.unwrap();
        assert!(matches!(msg, StderrMessage::StopActivity { id: 1 }));

        // Read LAST
        let msg = read_stderr_message(&mut reader).await.unwrap();
        assert!(matches!(msg, StderrMessage::Last));
    }

    /// Test reading STDERR_ERROR.
    #[tokio::test]
    async fn read_stderr_error_message() {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.error(&StderrError::simple("test", "something failed"))
                .await
                .unwrap();
        }

        let mut reader = std::io::Cursor::new(buf);
        let msg = read_stderr_message(&mut reader).await.unwrap();
        match msg {
            StderrMessage::Error(e) => {
                assert_eq!(e.error_type(), "Error");
                assert_eq!(e.name(), "test");
                assert_eq!(e.message(), "something failed");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
