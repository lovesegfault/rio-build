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
//! build-system environment-variable convention, a build-system user
//! identity, or a build-system log protocol. The request carries opaque
//! mounts, verbatim argv/env, and the sandbox's passwd/group identity
//! ([`SandboxIdentity`] — mandatory, because the executor has no name
//! of its own to default to); the outcome reports raw exit status and
//! per-path metadata. All build-system policy — what the environment
//! contains, what the build user is called, whether a missing output is
//! an error, how log lines are interpreted — lives in the caller. This
//! is what makes the executor reusable across build systems and
//! independently testable.
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
//! [`execute()`] is the entry point that ties the modules together:
//! compile the plan, build the skeleton, fork the process tree, stream
//! captured output, enforce limits, and report the outcome.

pub mod outcome;
pub mod request;

mod child;
mod execute;
mod plan;
mod seccomp;
mod skeleton;

pub use child::{SetupError, SetupPhase};
pub use execute::{BUILD_SUBCGROUP, execute};
pub use outcome::{
    ExecEvent, ExecutionOutcome, ExitOutcome, LogStream, OutputFileType, OutputMetadata,
    OutputReport,
};
pub use plan::HostLayout;
pub use request::{
    ExecutionRequest, InlineFile, Isolation, Limits, Mount, OutputCapture, Personality,
    SandboxIdentity,
};

/// Errors produced by the executor.
///
/// Every variant is an *infrastructure* failure from the caller's point
/// of view — a request that could not be validated or a sandbox that
/// could not be constructed or supervised. A process that runs and
/// exits non-zero (or is killed by a limit) is **not** an error; that
/// is an [`ExitOutcome`] in a successful [`ExecutionOutcome`].
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The request failed [`ExecutionRequest::validate`]. The message
    /// names the rule that was violated and the offending path or field.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Building the host-side chroot skeleton failed (directory
    /// creation, the synthesized `/etc` files, inline files, or the
    /// per-bind source stat).
    #[error("failed to build the sandbox skeleton: {0}")]
    Skeleton(#[source] std::io::Error),
    /// Creating the supervision plumbing failed: pipes, the pty, the
    /// fork itself, the cgroup attach, or the go signal.
    #[error("failed to spawn the sandbox: {0}")]
    Spawn(#[source] std::io::Error),
    /// The forked process reported a sandbox-setup failure before
    /// reaching `execve`. Carries the failing phase, the errno, and
    /// (for indexed phases such as bind mounts) which entry failed.
    #[error(
        "sandbox setup failed while {}: {} (errno {}, entry {})",
        .0.phase.describe(),
        nix::errno::Errno::from_raw(.0.errno).desc(),
        .0.errno,
        .0.detail
    )]
    Setup(SetupError),
}
