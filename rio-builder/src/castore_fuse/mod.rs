//! Castore-FUSE lazy `/nix/store` (ADR-022 §2).
//!
//! This module tree replaced the pre-ADR-022 whole-store-path JIT FUSE
//! at the P0560 §A cutover: the executor mounts a per-build castore
//! FUSE (via [`mount`]) as the overlay's only lower.
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
//! - [`stream`] — the P0575 streaming fill for large-file misses
//!   (chunk-by-chunk background fetch served through `read()` while
//!   it runs).
//! - [`circuit`] — the fetch circuit breaker.
//! - [`fs`] — the `fuser::Filesystem` impl tying it all together.
//! - [`mount`] — the per-build mount sequence (P0560 §A): DAG prefetch,
//!   mountd fd handoff, the builder-side `mount(2)`, and the serve
//!   session the per-build overlay stacks on.

pub mod circuit;
pub mod fs;
pub mod mount;
pub mod mountd;
pub mod mountd_client;
pub mod mountd_proto;
pub mod open;
pub mod stream;
pub mod sweep;
pub mod tree;

/// Cross-module tests that need a gRPC store and/or a mountd peer
/// (the `build_tree` round-trip, the `open()` dispatch, the fill
/// race). Single-module tests live next to their module.
#[cfg(test)]
mod tests;
