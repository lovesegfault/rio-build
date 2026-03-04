//! Build executor with FUSE store for rio-build.
//!
//! Receives build assignments from the scheduler, runs builds using
//! nix-daemon within an overlayfs+FUSE environment, and uploads
//! results to the store.
//!
//! # Architecture
//!
//! ```text
//! rio-worker binary
//! +-- gRPC clients
//! |   +-- WorkerService.BuildExecution (bidi stream to scheduler)
//! |   +-- WorkerService.Heartbeat (periodic to scheduler)
//! |   +-- StoreService (fetch inputs, upload outputs)
//! +-- FUSE daemon (fuse/)
//! |   +-- Mount /nix/store via fuser 0.17
//! |   +-- lookup/getattr -> StoreService.QueryPathInfo
//! |   +-- read/readdir -> SSD cache or StoreService.GetPath
//! |   +-- LRU cache on local SSD (cache.rs)
//! +-- Build executor (executor.rs)
//! |   +-- Overlay management (overlay.rs)
//! |   +-- Synthetic DB generation (synth_db.rs)
//! |   +-- Log streaming (log_stream.rs)
//! |   +-- Output upload (upload.rs)
//! +-- Heartbeat loop (runtime.rs, 10s interval)
//! ```

pub mod executor;
pub mod fuse;
pub mod log_stream;
pub mod overlay;
pub mod runtime;
pub mod synth_db;
pub mod upload;

// Re-exports for main.rs — keeps `use rio_worker::{...}` imports stable
// after the lib.rs → runtime.rs extraction.
pub use runtime::{BloomHandle, BuildSpawnContext, build_heartbeat_request, spawn_build_task};
