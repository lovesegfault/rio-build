//! Castore-FUSE lazy `/nix/store` (ADR-022 §2).
//!
//! This module tree replaces the whole-store-path JIT FUSE in
//! [`crate::fuse`] at the P0560 cutover. Until then both coexist:
//! `fuse/` serves production, `castore_fuse/` accretes the new stack
//! bottom-up.
//!
//! - [`mountd`] + [`mountd_proto`] + `bin/rio-mountd.rs` — the
//!   privileged per-node broker (fd handoff, `BACKING_OPEN`, verified
//!   cache promotion) and its UDS wire protocol.
//! - [`mountd_client`] — the builder-side in-process client for that
//!   protocol.
//! - [`sweep`] — the daemon's disk-pressure eviction over the shared
//!   cache/chunk trees and orphaned staging dirs.
//! - [`tree`] — the mount-time Directory-DAG prefetch and the
//!   content-addressed inode table.
//! - [`open`] — the `open()` data path: backing-cache lookup, JIT
//!   fetch, promote.
//! - [`circuit`] — the fetch circuit breaker.
//! - [`fs`] — the `fuser::Filesystem` impl tying it all together.
//!
//! Still missing until P0560: `mount.rs` (the mount sequence that
//! connects to mountd, prefetches the tree, serves the handed-off
//! `/dev/fuse` fd, and stacks the per-build overlay on top).

pub mod circuit;
pub mod fs;
pub mod mountd;
pub mod mountd_client;
pub mod mountd_proto;
pub mod open;
pub mod sweep;
pub mod tree;

/// Cross-module tests that need a gRPC store and/or a mountd peer
/// (the `build_tree` round-trip, the `open()` dispatch, the fill
/// race). Single-module tests live next to their module.
#[cfg(test)]
mod tests;
