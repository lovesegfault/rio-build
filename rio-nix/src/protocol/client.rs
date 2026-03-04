//! Client-side Nix worker protocol implementation.
//!
//! Speaks the Nix worker protocol as a **client** to a local `nix-daemon --stdio`.
//! Used by workers to drive nix-daemon --stdio in the build sandbox.
//!
//! The client protocol mirrors the server protocol in `handshake.rs` and `stderr.rs`:
//! - Client sends `WORKER_MAGIC_1`, reads `WORKER_MAGIC_2` + server version
//! - Client reads the STDERR loop (log messages, activities, errors)
//! - Client sends opcodes and reads results

use super::handshake::{
    HandshakeError, HandshakeResult, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2,
};
use super::opcodes::WorkerOp;
use super::stderr::{
    ResultField, STDERR_ERROR, STDERR_LAST, STDERR_NEXT, STDERR_READ, STDERR_RESULT,
    STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_WRITE, StderrError,
};
use super::wire::{self, WireError};
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
/// Aborts after `MAX_STDERR_MESSAGES` to prevent infinite loops.
pub async fn drain_stderr<R: AsyncRead + Unpin>(r: &mut R) -> Result<(), WireError> {
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
    wire::write_string_pairs::<_, &str, &str>(writer, &[]).await?;

    writer.flush().await?;

    // nix-daemon sends only STDERR_LAST for SetOptions
    drain_stderr(reader).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::handshake::{MIN_CLIENT_VERSION, encode_version, server_handshake_split};
    use crate::protocol::stderr::StderrWriter;

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

    /// Test reading STDERR messages.
    #[tokio::test]
    async fn read_stderr_messages() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        {
            let mut w = StderrWriter::new(&mut buf);
            w.log("building foo").await?;
            let _id = w
                .start_activity(
                    crate::protocol::stderr::ActivityType::Build,
                    "building",
                    0,
                    0,
                )
                .await?;
            w.stop_activity(1).await?;
            w.finish().await?;
        }

        let mut reader = std::io::Cursor::new(buf);
        // Read NEXT
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::Next(ref s) if s == "building foo"));

        // Read START_ACTIVITY
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::StartActivity { id: 1, .. }));

        // Read STOP_ACTIVITY
        let msg = read_stderr_message(&mut reader).await?;
        assert!(matches!(msg, StderrMessage::StopActivity { id: 1 }));

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
    /// branch (124-131) and the trace loop (139-151).
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
    // client_set_options + client_handshake error path
    // -----------------------------------------------------------------------

    /// client_set_options wire layout round-trip. Field order and values
    /// per Nix src/libstore/daemon.cc case SetOptions.
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
            assert_eq!(max_silent_time, 0);
            let obsolete_use_build_hook = wire::read_u64(&mut sr).await?;
            assert_eq!(obsolete_use_build_hook, 1);
            let verbose_build = wire::read_bool(&mut sr).await?;
            assert!(!verbose_build);
            let _obsolete_log_type = wire::read_u64(&mut sr).await?;
            let _obsolete_print_build_trace = wire::read_u64(&mut sr).await?;
            let build_cores = wire::read_u64(&mut sr).await?;
            assert_eq!(build_cores, 0);
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
        client_set_options(&mut cr, &mut cw).await?;
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
}
