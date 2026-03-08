//! nix-daemon subprocess management.
//!
//! Split into `spawn` (mount-namespace process launch) and
//! `stderr_loop` (wire-protocol STDERR parsing + LogBatcher).
// r[impl worker.daemon.timeout-wrap]
// r[impl worker.daemon.kill-both-paths]
// r[impl worker.daemon.stdio-client]

use std::time::Duration;

mod spawn;
mod stderr_loop;

pub(super) use spawn::spawn_daemon_in_namespace;
pub(super) use stderr_loop::run_daemon_build;

/// Timeout for the daemon setup sequence (handshake + setOptions + send build).
/// This bounds the blast radius of a stuck daemon before the build timeout kicks in.
pub(super) const DAEMON_SETUP_TIMEOUT: Duration = Duration::from_secs(30);
