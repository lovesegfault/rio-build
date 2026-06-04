//! The execution outcome: what happened when the request was run.
//!
//! The executor reports facts — exit status, captured output, per-path
//! metadata — and leaves their interpretation (is a non-zero exit
//! retryable? is a missing output an error?) to the caller.

use std::path::PathBuf;
use std::time::SystemTime;

/// Which capture channel a log line arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    /// The single merged pty stream
    /// ([`OutputCapture::MergedPty`](crate::OutputCapture::MergedPty)).
    Merged,
    /// The child's stdout pipe
    /// ([`OutputCapture::SeparatePipes`](crate::OutputCapture::SeparatePipes)).
    Stdout,
    /// The child's stderr pipe
    /// ([`OutputCapture::SeparatePipes`](crate::OutputCapture::SeparatePipes)).
    Stderr,
}

/// An event streamed to the caller while the request executes.
#[derive(Debug, Clone)]
pub enum ExecEvent {
    /// A captured line of process output. The executor does no
    /// interpretation — escape-sequence handling, structured-log
    /// extraction, and batching are caller concerns. The line does not
    /// include its terminator.
    Log {
        /// Which capture channel the line arrived on.
        stream: LogStream,
        /// The raw line bytes (not necessarily UTF-8).
        line: Vec<u8>,
        /// Whether this event ends a logical line: `true` for a line
        /// emitted at its `\n` terminator; `false` for a fragment the
        /// splitter force-emitted (pending-buffer cap reached) or an
        /// EOF flush of a trailing unterminated line. Pure framing
        /// metadata --- the executor attaches no meaning to the
        /// contents, but a caller-side classifier needs the boundary
        /// to treat a split logical line as one unit (the head
        /// classifies, continuations inherit).
        terminated: bool,
    },
    /// Sandbox setup completed and the program is about to exec.
    Started {
        /// The host-side pid of the sandboxed process tree's root (the
        /// process the executor forked, not the pid-namespace-internal
        /// pid 1).
        pid: i32,
    },
}

/// How the sandboxed process tree finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    /// The process exited on its own with this status code.
    Exited(i32),
    /// The process was terminated by this signal (including by the
    /// executor's own limit enforcement, which kills the cgroup).
    Signaled(i32),
    /// The executor killed the process tree because
    /// [`Limits::timeout`](crate::Limits::timeout) elapsed.
    TimedOut,
    /// The executor killed the process tree because no output was
    /// captured for [`Limits::max_silent`](crate::Limits::max_silent).
    Silent,
    /// The executor killed the process tree because it produced more
    /// than [`Limits::max_log_bytes`](crate::Limits::max_log_bytes) of
    /// output.
    LogLimitExceeded,
}

/// What the executor found at one declared output path after exit.
#[derive(Debug, Clone)]
pub struct OutputReport {
    /// The declared (sandbox-absolute) path, as passed in
    /// [`declared_outputs`](crate::ExecutionRequest::declared_outputs).
    pub path: PathBuf,
    /// Where the executor looked for it on the host (the declared path
    /// translated through the writable mount it falls under). Callers
    /// read the produced data from here.
    pub host_path: PathBuf,
    /// Whether `lstat(2)` succeeded on `host_path`.
    pub exists: bool,
    /// Metadata from `lstat(2)`. `None` when `!exists`.
    pub metadata: Option<OutputMetadata>,
}

/// `lstat(2)` metadata for a produced output path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputMetadata {
    /// The full `st_mode` (file type bits and permission bits).
    pub mode: u32,
    /// The owning uid. Callers typically check this against the
    /// sandbox uid they requested.
    pub uid: u32,
    /// `st_size` in bytes.
    pub size: u64,
    /// The file type, decoded from `st_mode` for convenience.
    pub file_type: OutputFileType,
}

/// The file type of a produced output path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFileType {
    /// A regular file.
    Regular,
    /// A directory.
    Directory,
    /// A symbolic link (the metadata describes the link itself, not
    /// its target — outputs are never followed).
    Symlink,
    /// Anything else (FIFO, socket, device node). Callers generally
    /// reject these.
    Other,
}

/// The result of a completed execution.
///
/// Named `ExecutionOutcome` rather than `ExecutionResult` to avoid
/// colliding with callers' own result types one layer up (rio-builder
/// already has a scheduler-facing `executor::ExecutionResult`).
///
/// An `ExecutionOutcome` is returned whenever the sandbox was
/// constructed and the process ran — including when it failed, was
/// killed, or produced nothing. Only request-validation and
/// sandbox-setup failures surface as [`ExecError`](crate::ExecError)
/// instead.
#[derive(Debug, Clone)]
pub struct ExecutionOutcome {
    /// How the process tree finished.
    pub exit: ExitOutcome,
    /// One report per
    /// [`declared_outputs`](crate::ExecutionRequest::declared_outputs)
    /// entry, in the same order.
    pub outputs: Vec<OutputReport>,
    /// When the executor forked the sandbox child.
    pub start: SystemTime,
    /// When the executor reaped the sandbox child. Post-exit work the
    /// caller does on the outputs is not included.
    pub stop: SystemTime,
}
