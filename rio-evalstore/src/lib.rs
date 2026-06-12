//! Client-side eval store: a persistent content-addressed cache (CAS) that
//! backs the `rio://` nix store plugin (ADR-024 milestone 0).
//!
//! The crate is the Rust core of an in-process `nix::Store` implementation:
//! a thin C++ shim (`shim/shim.cc`) implements the nix vtable and calls into
//! [`ffi`]. Everything stateful lives here — blob storage, Directory DAGs,
//! derivation capture, store-path computation (via rio-nix), and the
//! cross-implementation path checks that hard-fail on any divergence between
//! rio-nix's hashing and nix's own.
//!
//! Design constraints (ADR-024):
//! - **Synchronous core.** No tokio, no threads at plugin load — nix-eval-jobs
//!   forks eval workers without exec, so anything spawned at dlopen or config
//!   construction would be silently lost or deadlocked in the child.
//! - **Streamed bulk ops.** NAR ingest and regeneration stream through
//!   callbacks; per-file contents are bounded by rio-nix's NAR parser limits.
//! - **Cross-checks are hard errors.** The shim passes nix's own computed
//!   store path / drvPath alongside content; the core recomputes via rio-nix
//!   and refuses to proceed on mismatch, printing both paths.

pub mod cas;
pub mod dirblob;
pub mod dircache;
pub mod ffi;
pub mod stats;
pub mod store;

pub use store::{CaMethod, DumpMethod, EvalStore, EvalStoreError};
