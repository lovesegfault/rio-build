//! STDERR streaming loop — the core I/O mechanism for the Nix worker protocol.
//!
//! Every opcode response is wrapped in an STDERR loop: the server sends log messages,
//! progress updates, and errors via STDERR_* message types, and terminates with
//! STDERR_LAST to signal that the operation result follows.
// r[impl gw.stderr.message-types]

use super::wire::{self, Result, WireError};
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
///
/// Values match upstream `nix::ActivityType` (`libutil/logging.hh`).
/// `Unknown` = 0, then 100-112. Do NOT renumber — these go on the
/// wire and the client (nom, progress-bar) dispatches on the integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum ActivityType {
    Unknown = 0,
    CopyPath = 100,
    FileTransfer = 101,
    Realise = 102,
    CopyPaths = 103,
    Builds = 104,
    Build = 105,
    OptimiseStore = 106,
    VerifyPaths = 107,
    Substitute = 108,
    QueryPathInfo = 109,
    PostBuildHook = 110,
    BuildWaiting = 111,
    FetchTree = 112,
}

/// Result type enum for STDERR_RESULT.
///
/// Values match upstream `nix::ResultType` (`libutil/logging.hh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum ResultType {
    FileLinked = 100,
    BuildLogLine = 101,
    UntrustedPath = 102,
    CorruptedPath = 103,
    SetPhase = 104,
    Progress = 105,
    SetExpected = 106,
    PostBuildLogLine = 107,
    FetchStatus = 108,
}

/// Nix verbosity levels (`libutil/logging.hh` `Verbosity`).
///
/// Used for the `level` field of `STDERR_START_ACTIVITY` and
/// `STDERR_ERROR`. Upstream's `JSONLogger` does not filter on level,
/// so for nom these are cosmetic; `--log-format bar` does filter.
pub mod verbosity {
    pub const ERROR: u64 = 0;
    pub const WARN: u64 = 1;
    pub const NOTICE: u64 = 2;
    pub const INFO: u64 = 3;
    pub const TALKATIVE: u64 = 4;
    pub const CHATTY: u64 = 5;
    pub const DEBUG: u64 = 6;
    pub const VOMIT: u64 = 7;
}

/// Writes STDERR messages to the Nix client.
///
/// **State machine:** the writer is *live* until exactly one of `finish()`
/// or `error()` succeeds. After `finish()`, `inner_mut()` becomes available
/// for writing the operation result. After `error()`, the writer is poisoned:
/// all further STDERR calls return `Err`, and `inner_mut()` panics. See
/// `r[gw.stderr.error-before-return+2]` — `STDERR_ERROR` and `STDERR_LAST` are
/// mutually exclusive terminal frames.
pub struct StderrWriter<W> {
    writer: W,
    next_activity_id: u64,
    finished: bool,
    errored: bool,
}

impl<W: AsyncWrite + Unpin> StderrWriter<W> {
    pub fn new(writer: W) -> Self {
        StderrWriter {
            writer,
            next_activity_id: 1,
            finished: false,
            errored: false,
        }
    }

    /// Check that the writer is still live (neither finished nor errored).
    fn check_not_finished(&self) -> Result<()> {
        if self.finished || self.errored {
            return Err(WireError::Io(std::io::Error::other(
                "StderrWriter already terminated (finish() or error() was called)",
            )));
        }
        Ok(())
    }

    /// Get a mutable reference to the inner writer (for writing result data after STDERR_LAST).
    ///
    /// # Panics
    ///
    /// Panics if called before `finish()`, or if `error()` was called.
    /// `STDERR_ERROR` is terminal — no result payload follows it on the wire,
    /// so handing out the raw writer after an error would let the handler
    /// emit bytes that a real Nix client will never consume.
    pub fn inner_mut(&mut self) -> &mut W {
        assert!(
            self.finished && !self.errored,
            "StderrWriter::inner_mut() called before finish() — \
             or after error() — this would corrupt the STDERR stream"
        );
        &mut self.writer
    }

    /// Send a log message (STDERR_NEXT).
    pub async fn log(&mut self, msg: &str) -> Result<()> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_NEXT).await?;
        wire::write_string(&mut self.writer, msg).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send STDERR_LAST to end the streaming loop.
    /// After this, the caller should write the operation result directly.
    /// Returns `Err` if `error()` or `finish()` was already called —
    /// `STDERR_ERROR` and `STDERR_LAST` are mutually exclusive terminal frames.
    pub async fn finish(&mut self) -> Result<()> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_LAST).await?;
        self.writer.flush().await?;
        self.finished = true;
        Ok(())
    }

    /// Write an optional position to the wire (havePos flag + fields).
    async fn write_position(&mut self, pos: Option<&Position>) -> Result<()> {
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
    // r[impl gw.stderr.error-before-return+2]
    /// Send STDERR_ERROR with the full structured error format.
    ///
    /// This is a **terminal** operation. After calling `error()`, the writer
    /// is poisoned: `finish()`, `log()`, `start_activity()`, etc. all return
    /// `Err`, and `inner_mut()` panics. The handler MUST `return Err(...)`
    /// immediately after this call — see `r[gw.stderr.error-before-return+2]`.
    pub async fn error(&mut self, err: &StderrError) -> Result<()> {
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
        // Flag-set-after-flush: if the write failed (broken pipe), the
        // writer is NOT poisoned. No handler retries wire writes today,
        // but the semantics are cleaner.
        self.errored = true;
        Ok(())
    }

    /// Send STDERR_WRITE with data. Test-only: no production opcode uses
    /// STDERR_WRITE (wopNarFromPath writes raw bytes after STDERR_LAST).
    #[cfg(test)]
    pub async fn write_data(&mut self, data: &[u8]) -> Result<()> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_WRITE).await?;
        wire::write_bytes(&mut self.writer, data).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Send STDERR_READ to request data from the client. Test-only: production
    /// uses framed streams (protocol >= 1.23), not STDERR_READ.
    #[cfg(test)]
    pub async fn read_request(&mut self, count: u64) -> Result<()> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_READ).await?;
        wire::write_u64(&mut self.writer, count).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Write a count-prefixed list of typed fields (shared by
    /// `STDERR_START_ACTIVITY` and `STDERR_RESULT`). Wire format per
    /// upstream `TunnelLogger`: `u64 count`, then for each field a
    /// `u64 tag` (0 = int, 1 = string) followed by the value.
    async fn write_fields(&mut self, fields: &[ResultField]) -> Result<()> {
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
        Ok(())
    }

    // r[impl gw.stderr.activity+2]
    /// Send STDERR_START_ACTIVITY. Returns the assigned activity ID.
    ///
    /// `fields` carries the activity-type-specific structured data the
    /// client UI reads (upstream `Logger::Fields`). For
    /// `ActivityType::Build` this is `[drvPath, machineName, curRound,
    /// nrRounds]` — nom and `--log-format bar` read `fields[0]` for the
    /// derivation name and `fields[1]` for the `on <machine>` suffix.
    pub async fn start_activity(
        &mut self,
        activity_type: ActivityType,
        text: &str,
        level: u64,
        parent_id: u64,
        fields: &[ResultField],
    ) -> Result<u64> {
        self.check_not_finished()?;
        let id = self.next_activity_id;
        self.next_activity_id += 1;

        wire::write_u64(&mut self.writer, STDERR_START_ACTIVITY).await?;
        wire::write_u64(&mut self.writer, id).await?;
        wire::write_u64(&mut self.writer, level).await?;
        wire::write_u64(&mut self.writer, activity_type as u64).await?;
        wire::write_string(&mut self.writer, text).await?;
        self.write_fields(fields).await?;
        wire::write_u64(&mut self.writer, parent_id).await?;

        self.writer.flush().await?;
        Ok(id)
    }

    /// Send STDERR_STOP_ACTIVITY.
    pub async fn stop_activity(&mut self, id: u64) -> Result<()> {
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
    /// Not #[cfg(test)] — rio-builder's daemon.rs tests synthesize
    /// nix-daemon STDERR_RESULT output, and cfg(test) on a lib crate's
    /// method doesn't enable it for dependent crates' tests.
    pub async fn result(
        &mut self,
        activity_id: u64,
        result_type: ResultType,
        fields: &[ResultField],
    ) -> Result<()> {
        self.check_not_finished()?;
        wire::write_u64(&mut self.writer, STDERR_RESULT).await?;
        wire::write_u64(&mut self.writer, activity_id).await?;
        wire::write_u64(&mut self.writer, result_type as u64).await?;
        self.write_fields(fields).await?;
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
            .start_activity(ActivityType::Build, "building foo", 0, 0, &[])
            .await?;
        let id2 = writer
            .start_activity(ActivityType::CopyPath, "copying bar", 0, id1, &[])
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
                ResultType::Progress,
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
                ResultType::BuildLogLine,
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
            .start_activity(
                ActivityType::Build,
                "building /nix/store/...-hello",
                2,
                5,
                &[],
            )
            .await?;
        assert_eq!(id, 1);

        let mut reader = Cursor::new(&buf);
        assert_eq!(wire::read_u64(&mut reader).await?, STDERR_START_ACTIVITY);
        assert_eq!(wire::read_u64(&mut reader).await?, 1); // id
        assert_eq!(wire::read_u64(&mut reader).await?, 2); // level
        assert_eq!(wire::read_u64(&mut reader).await?, 105); // type = Build
        assert_eq!(
            wire::read_string(&mut reader).await?,
            "building /nix/store/...-hello"
        );
        assert_eq!(wire::read_u64(&mut reader).await?, 0); // fieldsCount
        assert_eq!(wire::read_u64(&mut reader).await?, 5); // parentId
        Ok(())
    }

    // r[verify gw.stderr.activity+2]
    /// Wire-level: `ActivityType` discriminants match upstream
    /// `nix::ActivityType` (`libutil/logging.hh`). Regression guard for
    /// PLAN-NOM bug #1: every variant was off-by-one (Build=106 →
    /// nom's `actBuild` handler never fired; nom saw `actOptimiseStore`).
    #[test]
    fn test_activity_type_wire_values() {
        assert_eq!(ActivityType::Unknown as u64, 0);
        assert_eq!(ActivityType::CopyPath as u64, 100);
        assert_eq!(ActivityType::FileTransfer as u64, 101);
        assert_eq!(ActivityType::Realise as u64, 102);
        assert_eq!(ActivityType::CopyPaths as u64, 103);
        assert_eq!(ActivityType::Builds as u64, 104);
        assert_eq!(ActivityType::Build as u64, 105);
        assert_eq!(ActivityType::OptimiseStore as u64, 106);
        assert_eq!(ActivityType::VerifyPaths as u64, 107);
        assert_eq!(ActivityType::Substitute as u64, 108);
        assert_eq!(ActivityType::QueryPathInfo as u64, 109);
        assert_eq!(ActivityType::PostBuildHook as u64, 110);
        assert_eq!(ActivityType::BuildWaiting as u64, 111);
        assert_eq!(ActivityType::FetchTree as u64, 112);
    }

    /// Wire-level: `ResultType` discriminants match upstream
    /// `nix::ResultType` (`libutil/logging.hh`).
    #[test]
    fn test_result_type_wire_values() {
        assert_eq!(ResultType::FileLinked as u64, 100);
        assert_eq!(ResultType::BuildLogLine as u64, 101);
        assert_eq!(ResultType::UntrustedPath as u64, 102);
        assert_eq!(ResultType::CorruptedPath as u64, 103);
        assert_eq!(ResultType::SetPhase as u64, 104);
        assert_eq!(ResultType::Progress as u64, 105);
        assert_eq!(ResultType::SetExpected as u64, 106);
        assert_eq!(ResultType::PostBuildLogLine as u64, 107);
        assert_eq!(ResultType::FetchStatus as u64, 108);
    }

    // r[verify gw.stderr.activity+2]
    /// Byte-exact: `start_activity(Build, ..., [drv,"",1,1])` encoding
    /// matches upstream `TunnelLogger::startActivity` (`daemon.cc`).
    /// Reference capture from `nix build --log-format internal-json`
    /// (PLAN-NOM § Upstream comparison): a real daemon emits
    /// `{"action":"start","fields":["<drv>","",1,1],"id":N,"level":3,
    /// "parent":0,"text":"...","type":105}` for `actBuild`.
    #[tokio::test]
    async fn test_start_activity_actbuild_golden_bytes() -> anyhow::Result<()> {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.3.drv";
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .start_activity(
                ActivityType::Build,
                "building '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.3.drv'",
                verbosity::INFO,
                0,
                &[
                    ResultField::String(drv.into()),
                    ResultField::String(String::new()),
                    ResultField::Int(1),
                    ResultField::Int(1),
                ],
            )
            .await?;

        // Hand-assemble the wire bytes per upstream TunnelLogger format:
        //   STDERR_START_ACTIVITY, id, level, type, text, fields, parent
        // All u64 little-endian; strings length-prefixed + zero-padded to 8.
        let mut want = Vec::new();
        wire::write_u64(&mut want, STDERR_START_ACTIVITY).await?;
        wire::write_u64(&mut want, 1).await?; // id (first activity)
        wire::write_u64(&mut want, 3).await?; // lvlInfo
        wire::write_u64(&mut want, 105).await?; // actBuild
        wire::write_string(
            &mut want,
            "building '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.3.drv'",
        )
        .await?;
        wire::write_u64(&mut want, 4).await?; // fieldsCount
        wire::write_u64(&mut want, 1).await?; // tag=string
        wire::write_string(&mut want, drv).await?;
        wire::write_u64(&mut want, 1).await?; // tag=string
        wire::write_string(&mut want, "").await?;
        wire::write_u64(&mut want, 0).await?; // tag=int
        wire::write_u64(&mut want, 1).await?;
        wire::write_u64(&mut want, 0).await?; // tag=int
        wire::write_u64(&mut want, 1).await?;
        wire::write_u64(&mut want, 0).await?; // parent

        assert_eq!(buf, want, "encoder bytes diverge from TunnelLogger format");

        // And: the reader round-trips it.
        let mut reader = Cursor::new(&buf);
        let msg = crate::protocol::client::read_stderr_message(&mut reader).await?;
        match msg {
            crate::protocol::client::StderrMessage::StartActivity {
                id,
                level,
                activity_type,
                text,
                fields,
                parent_id,
            } => {
                assert_eq!(id, 1);
                assert_eq!(level, 3);
                assert_eq!(activity_type, 105);
                assert!(text.contains("hello-2.12.3.drv"));
                assert_eq!(
                    fields,
                    vec![
                        ResultField::String(drv.into()),
                        ResultField::String(String::new()),
                        ResultField::Int(1),
                        ResultField::Int(1),
                    ]
                );
                assert_eq!(parent_id, 0);
            }
            other => panic!("expected StartActivity, got {other:?}"),
        }
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

    // r[verify gw.stderr.error-before-return+2]
    /// error() is terminal: a subsequent finish() must be rejected
    /// (STDERR_ERROR and STDERR_LAST are mutually exclusive).
    #[tokio::test]
    async fn test_error_then_finish_rejected() {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .error(&StderrError::simple("rio-build", "boom"))
            .await
            .unwrap();

        let result = writer.finish().await;
        assert!(result.is_err(), "finish() after error() must return Err");

        // Byte-level: STDERR_ERROR frame, NOT followed by STDERR_LAST.
        let mut reader = Cursor::new(&buf);
        assert_eq!(wire::read_u64(&mut reader).await.unwrap(), STDERR_ERROR);
        // Consume the error body (type, level, name, message, havePos, traceCount).
        let _ = wire::read_string(&mut reader).await.unwrap(); // type
        let _ = wire::read_u64(&mut reader).await.unwrap(); // level
        let _ = wire::read_string(&mut reader).await.unwrap(); // name
        let _ = wire::read_string(&mut reader).await.unwrap(); // message
        assert_eq!(wire::read_u64(&mut reader).await.unwrap(), 0); // havePos
        assert_eq!(wire::read_u64(&mut reader).await.unwrap(), 0); // traceCount
        // The cursor is now at EOF. No STDERR_LAST bytes follow.
        assert_eq!(
            reader.position() as usize,
            buf.len(),
            "no bytes after STDERR_ERROR frame"
        );
    }

    /// error() is terminal: subsequent log() is rejected.
    #[tokio::test]
    async fn test_error_then_log_rejected() {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .error(&StderrError::simple("rio-build", "boom"))
            .await
            .unwrap();
        assert!(writer.log("this should not be sent").await.is_err());
    }

    /// error() is terminal: inner_mut() panics (no result payload after STDERR_ERROR).
    #[tokio::test]
    #[should_panic(expected = "after error()")]
    async fn test_error_then_inner_mut_panics() {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer
            .error(&StderrError::simple("rio-build", "boom"))
            .await
            .unwrap();
        let _ = writer.inner_mut(); // ← panic here
    }

    /// finish() is idempotent-rejecting: double finish() is an error.
    #[tokio::test]
    async fn test_double_finish_rejected() {
        let mut buf = Vec::new();
        let mut writer = StderrWriter::new(&mut buf);
        writer.finish().await.unwrap();
        assert!(writer.finish().await.is_err());
        // Exactly one STDERR_LAST in the buffer (8 bytes).
        assert_eq!(buf.len(), 8);
    }
}
