//! STDERR streaming loop — the core I/O mechanism for the Nix worker protocol.
//!
//! Every opcode response is wrapped in an STDERR loop: the server sends log messages,
//! progress updates, and errors via STDERR_* message types, and terminates with
//! STDERR_LAST to signal that the operation result follows.
// r[impl gw.stderr.message-types]

use super::wire::{self, WireError};
use tokio::io::{AsyncWrite, AsyncWriteExt};

// STDERR message type constants
pub const STDERR_NEXT: u64 = 0x6f6c6d67;
pub const STDERR_READ: u64 = 0x64617461;
pub const STDERR_WRITE: u64 = 0x64617416;
pub const STDERR_LAST: u64 = 0x616c7473;
pub const STDERR_ERROR: u64 = 0x63787470;
pub const STDERR_START_ACTIVITY: u64 = 0x53545254;
pub const STDERR_STOP_ACTIVITY: u64 = 0x53544f50;
pub const STDERR_RESULT: u64 = 0x52534c54;

/// Structured error for the STDERR_ERROR wire format.
#[derive(Debug, Clone)]
pub struct StderrError {
    /// Error type string, e.g. `"Error"` or `"nix::Interrupted"`.
    pub error_type: String,
    /// Error level (verbosity).
    pub level: u64,
    /// Program name, e.g. `"rio-build"`.
    pub name: String,
    /// Human-readable error message.
    pub message: String,
    /// Optional source position.
    pub position: Option<Position>,
    /// Stack trace entries.
    pub traces: Vec<Trace>,
}

/// A source code position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub file: String,
    pub line: u64,
    pub column: u64,
}

impl Position {
    /// Create a new source position.
    pub fn new(file: impl Into<String>, line: u64, column: u64) -> Self {
        Position {
            file: file.into(),
            line,
            column,
        }
    }
}

/// A stack trace entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub position: Option<Position>,
    pub message: String,
}

impl Trace {
    /// Create a new trace entry.
    pub fn new(position: Option<Position>, message: impl Into<String>) -> Self {
        Trace {
            position,
            message: message.into(),
        }
    }
}

impl StderrError {
    /// Create a new structured error with all fields.
    pub fn new(
        error_type: impl Into<String>,
        level: u64,
        name: impl Into<String>,
        message: impl Into<String>,
        position: Option<Position>,
        traces: Vec<Trace>,
    ) -> Self {
        StderrError {
            error_type: error_type.into(),
            level,
            name: name.into(),
            message: message.into(),
            position,
            traces,
        }
    }

    /// Create a simple error with no position and no traces.
    pub fn simple(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new("Error", 0, name, message, None, Vec::new())
    }
}

/// Activity type enum for STDERR_START_ACTIVITY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum ActivityType {
    Unknown = 0,
    CopyPath = 101,
    FileTransfer = 102,
    Realise = 103,
    CopyPaths = 104,
    Builds = 105,
    Build = 106,
    OptimiseStore = 107,
    VerifyPaths = 108,
    Substitute = 109,
    QueryPathInfo = 110,
    PostBuildHook = 111,
    BuildWaiting = 112,
}

/// Writes STDERR messages to the Nix client.
pub struct StderrWriter<W> {
    writer: W,
    next_activity_id: u64,
    finished: bool,
}

impl<W: AsyncWrite + Unpin> StderrWriter<W> {
    pub fn new(writer: W) -> Self {
        StderrWriter {
            writer,
            next_activity_id: 1,
            finished: false,
        }
    }

    /// Check that the writer has not been finished; return an error if it has.
    fn check_not_finished(&self) -> Result<(), WireError> {
        if self.finished {
            return Err(WireError::Io(std::io::Error::other(
                "StderrWriter already finished",
            )));
        }
        Ok(())
    }

    /// Get a mutable reference to the inner writer (for writing result data after STDERR_LAST).
    ///
    /// # Panics
    ///
    /// Panics if called before `finish()`. The STDERR streaming loop must be
    /// terminated before writing result data to the underlying stream.
    pub fn inner_mut(&mut self) -> &mut W {
        assert!(
            self.finished,
            "StderrWriter::inner_mut() called before finish() — \
             this would corrupt the STDERR stream"
        );
        &mut self.writer
    }

    /// Send a log message (STDERR_NEXT).
    pub async fn log(&mut self, msg: &str) -> Result<(), WireError> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_NEXT).await?;
        wire::write_string(&mut self.writer, msg).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send STDERR_LAST to end the streaming loop.
    /// After this, the caller should write the operation result directly.
    pub async fn finish(&mut self) -> Result<(), WireError> {
        wire::write_u64(&mut self.writer, STDERR_LAST).await?;
        self.writer.flush().await?;
        self.finished = true;
        Ok(())
    }

    /// Write an optional position to the wire (havePos flag + fields).
    async fn write_position(&mut self, pos: Option<&Position>) -> Result<(), WireError> {
        if let Some(p) = pos {
            wire::write_u64(&mut self.writer, 1).await?; // havePos = true
            wire::write_string(&mut self.writer, &p.file).await?;
            wire::write_u64(&mut self.writer, p.line).await?;
            wire::write_u64(&mut self.writer, p.column).await?;
        } else {
            wire::write_u64(&mut self.writer, 0).await?; // havePos = false
        }
        Ok(())
    }

    // r[impl gw.stderr.error-format]
    // r[impl gw.stderr.error-before-return]
    /// Send STDERR_ERROR with the full structured error format.
    pub async fn error(&mut self, err: &StderrError) -> Result<(), WireError> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_ERROR).await?;

        wire::write_string(&mut self.writer, &err.error_type).await?;
        wire::write_u64(&mut self.writer, err.level).await?;
        wire::write_string(&mut self.writer, &err.name).await?;
        wire::write_string(&mut self.writer, &err.message).await?;
        self.write_position(err.position.as_ref()).await?;

        wire::write_u64(&mut self.writer, err.traces.len() as u64).await?;
        for trace in &err.traces {
            self.write_position(trace.position.as_ref()).await?;
            wire::write_string(&mut self.writer, &trace.message).await?;
        }

        self.writer.flush().await?;
        Ok(())
    }

    /// Send STDERR_WRITE with data. Test-only: no production opcode uses
    /// STDERR_WRITE (wopNarFromPath writes raw bytes after STDERR_LAST).
    #[cfg(test)]
    pub async fn write_data(&mut self, data: &[u8]) -> Result<(), WireError> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_WRITE).await?;
        wire::write_bytes(&mut self.writer, data).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send STDERR_READ to request data from the client. Test-only: production
    /// uses framed streams (protocol >= 1.23), not STDERR_READ.
    #[cfg(test)]
    pub async fn read_request(&mut self, count: u64) -> Result<(), WireError> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_READ).await?;
        wire::write_u64(&mut self.writer, count).await?;
        self.writer.flush().await?;
        Ok(())
    }

    // r[impl gw.stderr.activity]
    /// Send STDERR_START_ACTIVITY. Returns the assigned activity ID.
    pub async fn start_activity(
        &mut self,
        activity_type: ActivityType,
        text: &str,
        level: u64,
        parent_id: u64,
    ) -> Result<u64, WireError> {
        self.check_not_finished()?;
        let id = self.next_activity_id;
        self.next_activity_id += 1;

        wire::write_u64(&mut self.writer, STDERR_START_ACTIVITY).await?;
        wire::write_u64(&mut self.writer, id).await?;
        wire::write_u64(&mut self.writer, level).await?;
        wire::write_u64(&mut self.writer, activity_type as u64).await?;
        wire::write_string(&mut self.writer, text).await?;
        wire::write_u64(&mut self.writer, 0).await?; // fieldsCount = 0
        wire::write_u64(&mut self.writer, parent_id).await?;

        self.writer.flush().await?;
        Ok(id)
    }

    /// Send STDERR_STOP_ACTIVITY.
    pub async fn stop_activity(&mut self, id: u64) -> Result<(), WireError> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_STOP_ACTIVITY).await?;
        wire::write_u64(&mut self.writer, id).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send STDERR_RESULT with structured result data for an activity.
    ///
    /// The result type determines the interpretation of the fields.
    /// Common result types:
    /// - 100 (FileLinked): [u64 fileSize, u64 blocksFreed]
    /// - 101 (BuildLogLine): [string line]
    /// - 102 (UntrustedPath): [string path]
    /// - 103 (CorruptedPath): [string path]
    /// - 104 (SetPhase): [string phase]
    /// - 105 (Progress): [u64 done, u64 expected, u64 running, u64 failed]
    /// - 106 (SetExpected): [u64 type, u64 expected]
    /// - 107 (PostBuildLogLine): [string line]
    ///
    /// Not #[cfg(test)] — rio-worker's daemon.rs tests synthesize
    /// nix-daemon STDERR_RESULT output, and cfg(test) on a lib crate's
    /// method doesn't enable it for dependent crates' tests.
    pub async fn result(
        &mut self,
        activity_id: u64,
        result_type: u64,
        fields: &[ResultField],
    ) -> Result<(), WireError> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_RESULT).await?;
        wire::write_u64(&mut self.writer, activity_id).await?;
        wire::write_u64(&mut self.writer, result_type).await?;
        wire::write_u64(&mut self.writer, fields.len() as u64).await?;
        for field in fields {
            match field {
                ResultField::Int(v) => {
                    wire::write_u64(&mut self.writer, 0).await?; // type 0 = int
                    wire::write_u64(&mut self.writer, *v).await?;
                }
                ResultField::String(s) => {
                    wire::write_u64(&mut self.writer, 1).await?; // type 1 = string
                    wire::write_string(&mut self.writer, s).await?;
                }
            }
        }
        self.writer.flush().await?;
        Ok(())
    }
}

/// A typed field in a STDERR_RESULT message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultField {
    Int(u64),
    String(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_stderr_log() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.log("hello world").await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_NEXT);
        let msg = wire::read_string(&mut reader).await?;
        assert_eq!(msg, "hello world");
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_last() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.finish().await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_LAST);
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_error_simple() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .error(&StderrError::simple("rio-build", "something broke"))
            .await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_ERROR);

        let error_type = wire::read_string(&mut reader).await?;
        assert_eq!(error_type, "Error");

        let level = wire::read_u64(&mut reader).await?;
        assert_eq!(level, 0);

        let name = wire::read_string(&mut reader).await?;
        assert_eq!(name, "rio-build");

        let message = wire::read_string(&mut reader).await?;
        assert_eq!(message, "something broke");

        let have_pos = wire::read_u64(&mut reader).await?;
        assert_eq!(have_pos, 0); // no position

        let trace_count = wire::read_u64(&mut reader).await?;
        assert_eq!(trace_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_write_data() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.write_data(b"NAR data here").await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_WRITE);
        let data = wire::read_string(&mut reader).await?;
        assert_eq!(data, "NAR data here");
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_write_empty_data() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.write_data(b"").await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_WRITE);
        let data = wire::read_string(&mut reader).await?;
        assert_eq!(data, "");
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_activity_ids() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        let id1 = writer
            .start_activity(ActivityType::Build, "building foo", 0, 0)
            .await?;
        let id2 = writer
            .start_activity(ActivityType::CopyPath, "copying bar", 0, id1)
            .await?;
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_read_request() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.read_request(8192).await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_READ);
        let count = wire::read_u64(&mut reader).await?;
        assert_eq!(count, 8192);
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_stop_activity() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.stop_activity(42).await?;

        let mut reader = Cursor::new(&buf);
        let msg_type = wire::read_u64(&mut reader).await?;
        assert_eq!(msg_type, STDERR_STOP_ACTIVITY);
        let id = wire::read_u64(&mut reader).await?;
        assert_eq!(id, 42);
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_result_with_int_fields() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .result(
                7,
                105, // Progress
                &[
                    ResultField::Int(10),
                    ResultField::Int(100),
                    ResultField::Int(3),
                    ResultField::Int(0),
                ],
            )
            .await?;

        let mut reader = Cursor::new(&buf);
        assert_eq!(wire::read_u64(&mut reader).await?, STDERR_RESULT);
        assert_eq!(wire::read_u64(&mut reader).await?, 7); // activity_id
        assert_eq!(wire::read_u64(&mut reader).await?, 105); // result_type
        assert_eq!(wire::read_u64(&mut reader).await?, 4); // field count

        // Field 0: Int(10)
        assert_eq!(wire::read_u64(&mut reader).await?, 0); // type = int
        assert_eq!(wire::read_u64(&mut reader).await?, 10);
        // Field 1: Int(100)
        assert_eq!(wire::read_u64(&mut reader).await?, 0);
        assert_eq!(wire::read_u64(&mut reader).await?, 100);
        // Field 2: Int(3)
        assert_eq!(wire::read_u64(&mut reader).await?, 0);
        assert_eq!(wire::read_u64(&mut reader).await?, 3);
        // Field 3: Int(0)
        assert_eq!(wire::read_u64(&mut reader).await?, 0);
        assert_eq!(wire::read_u64(&mut reader).await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_result_with_string_field() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .result(
                3,
                101, // BuildLogLine
                &[ResultField::String("building phase: configure".to_string())],
            )
            .await?;

        let mut reader = Cursor::new(&buf);
        assert_eq!(wire::read_u64(&mut reader).await?, STDERR_RESULT);
        assert_eq!(wire::read_u64(&mut reader).await?, 3); // activity_id
        assert_eq!(wire::read_u64(&mut reader).await?, 101); // result_type
        assert_eq!(wire::read_u64(&mut reader).await?, 1); // field count
        assert_eq!(wire::read_u64(&mut reader).await?, 1); // type = string
        assert_eq!(
            wire::read_string(&mut reader).await?,
            "building phase: configure"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_start_activity_wire_format() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        let id = writer
            .start_activity(ActivityType::Build, "building /nix/store/...-hello", 2, 5)
            .await?;
        assert_eq!(id, 1);

        let mut reader = Cursor::new(&buf);
        assert_eq!(wire::read_u64(&mut reader).await?, STDERR_START_ACTIVITY);
        assert_eq!(wire::read_u64(&mut reader).await?, 1); // id
        assert_eq!(wire::read_u64(&mut reader).await?, 2); // level
        assert_eq!(wire::read_u64(&mut reader).await?, 106); // type = Build
        assert_eq!(
            wire::read_string(&mut reader).await?,
            "building /nix/store/...-hello"
        );
        assert_eq!(wire::read_u64(&mut reader).await?, 0); // fieldsCount
        assert_eq!(wire::read_u64(&mut reader).await?, 5); // parentId
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_error_with_position_and_traces() -> anyhow::Result<()> {
        let err = StderrError::new(
            "nix::EvalError",
            1,
            "rio-build",
            "undefined variable 'foo'",
            Some(Position::new("default.nix", 42, 10)),
            vec![
                Trace::new(
                    Some(Position::new("flake.nix", 7, 3)),
                    "while evaluating the attribute 'packages'",
                ),
                Trace::new(None, "while calling the 'import' builtin"),
            ],
        );

        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.error(&err).await?;

        let mut reader = Cursor::new(&buf);
        assert_eq!(wire::read_u64(&mut reader).await?, STDERR_ERROR);

        // Error type
        assert_eq!(wire::read_string(&mut reader).await?, "nix::EvalError");
        // Level
        assert_eq!(wire::read_u64(&mut reader).await?, 1);
        // Name
        assert_eq!(wire::read_string(&mut reader).await?, "rio-build");
        // Message
        assert_eq!(
            wire::read_string(&mut reader).await?,
            "undefined variable 'foo'"
        );

        // Position: havePos = 1
        assert_eq!(wire::read_u64(&mut reader).await?, 1);
        assert_eq!(wire::read_string(&mut reader).await?, "default.nix");
        assert_eq!(wire::read_u64(&mut reader).await?, 42);
        assert_eq!(wire::read_u64(&mut reader).await?, 10);

        // Traces: count = 2
        assert_eq!(wire::read_u64(&mut reader).await?, 2);

        // Trace 0: with position
        assert_eq!(wire::read_u64(&mut reader).await?, 1); // havePos
        assert_eq!(wire::read_string(&mut reader).await?, "flake.nix");
        assert_eq!(wire::read_u64(&mut reader).await?, 7);
        assert_eq!(wire::read_u64(&mut reader).await?, 3);
        assert_eq!(
            wire::read_string(&mut reader).await?,
            "while evaluating the attribute 'packages'"
        );

        // Trace 1: no position
        assert_eq!(wire::read_u64(&mut reader).await?, 0); // havePos
        assert_eq!(
            wire::read_string(&mut reader).await?,
            "while calling the 'import' builtin"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_stderr_writer_rejects_after_finish() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.finish().await?;
        let result = writer.log("x").await;
        assert!(result.is_err());
        Ok(())
    }
}
