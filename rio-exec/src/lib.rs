//! Build-system-agnostic Linux sandbox executor.
//!
//! Callers describe a process and the world it should see — argv, env,
//! a set of bind mounts, isolation parameters, and resource limits — as
//! an [`ExecutionRequest`]. The executor materializes that world (mount
//! / PID / IPC / UTS / cgroup and optionally network namespaces, an
//! ordered set of bind mounts under a `pivot_root`ed chroot, a seccomp
//! purity filter, a uid/gid drop) and runs the process in it, streaming
//! captured output as [`ExecEvent`]s and returning an
//! [`ExecutionOutcome`].
//!
//! # Boundary discipline
//!
//! Nothing in this crate may reference a store path, a derivation, a
//! build-system environment-variable convention, or a build-system log
//! protocol. The request carries opaque mounts and verbatim argv/env;
//! the outcome reports raw exit status and per-path metadata. All
//! build-system policy — what the environment contains, whether a
//! missing output is an error, how log lines are interpreted — lives in
//! the caller. This is what makes the executor reusable across build
//! systems and independently testable.
//!
//! # Modules
//!
//! - [`request`]: the [`ExecutionRequest`] type tree and its
//!   [`validate`](ExecutionRequest::validate) boundary.
//! - [`outcome`]: the [`ExecutionOutcome`] type tree and the
//!   [`ExecEvent`] stream items.
//!
//! The sandbox construction and the `execute()` entry point are added by
//! follow-up changes; this crate currently defines only the API surface.

pub mod outcome;
pub mod request;
// The sandbox child sequence (the next change in this crate) is the
// production consumer of the seccomp filter; until it lands, the unit
// tests are its only callers, so the module's items would otherwise trip
// dead_code under `--deny warnings`. Remove the allow when the sandbox
// sequence wires `build_filter` + `install` into the pre-exec path.
#[allow(dead_code)]
pub(crate) mod seccomp;

pub use outcome::{
    ExecEvent, ExecutionOutcome, ExitOutcome, LogStream, OutputFileType, OutputMetadata,
    OutputReport,
};
pub use request::{
    ExecutionRequest, InlineFile, Isolation, Limits, Mount, OutputCapture, Personality,
};

/// Errors produced by the executor.
///
/// Every variant is an *infrastructure* failure from the caller's point
/// of view — a request that could not be validated or a sandbox that
/// could not be constructed. A process that runs and exits non-zero is
/// **not** an error; that is an [`ExitOutcome`] in a successful
/// [`ExecutionOutcome`].
///
/// Sandbox-setup failure variants (one per setup phase: namespace
/// creation, mount application, `pivot_root`, privilege drop, …) are
/// added together with the sandbox implementation so each phase failure
/// stays individually attributable.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The request failed [`ExecutionRequest::validate`]. The message
    /// names the rule that was violated and the offending path or field.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}
