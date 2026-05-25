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
//! - `plan` (crate-private): `SandboxPlan::compile`, the pure
//!   request → ordered-operations resolution step where every mount
//!   ordering and file content decision lives.
//! - `skeleton` (crate-private): the host-side, pre-fork
//!   materialization of a plan's directory tree, files, and symlinks.
//! - `child` (crate-private): the async-signal-safe post-fork sequence
//!   (namespaces, mounts, `pivot_root`, hardening, privilege drop,
//!   `execve`).
//! - `seccomp` (crate-private): the multi-ABI purity filter the child
//!   installs.
//!
//! The `execute()` entry point that forks the process tree and drives
//! the pieces above is added by the next change; until it lands the
//! sandbox modules have no production caller, which is why they carry a
//! temporary `#[allow(dead_code)]`.

pub mod outcome;
pub mod request;
// The execute() entry point (the next change in this crate) is the
// production consumer of the plan/skeleton/child/seccomp pipeline;
// until it lands, the unit tests are the only callers, so the modules'
// items would otherwise trip dead_code under `--deny warnings`. Remove
// the allows when execute() wires compile -> build -> fork ->
// enter_namespaces -> setup_and_exec together.
#[allow(dead_code)]
pub(crate) mod child;
#[allow(dead_code)]
pub(crate) mod plan;
#[allow(dead_code)]
pub(crate) mod seccomp;
#[allow(dead_code)]
pub(crate) mod skeleton;

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
