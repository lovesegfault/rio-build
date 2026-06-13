//! The eval-parent side of the `rio.evaljob` worker channel (ADR-024
//! P3b): synchronous frame I/O, the fork-worker orchestration loop, and
//! the IFD relay.
//!
//! The coordinator side (tokio, `rio-build-cli`) and this side share
//! the proto messages and the wire shape — a 4-byte big-endian length
//! prefix, then one encoded message. This side is deliberately
//! synchronous and thread-free: the eval parent forks workers without
//! exec, so the process must hold zero live threads at fork time
//! (`r[bc.evalparent.fork-safety]` — the ingest pipeline's scoped
//! threads exist only inside `ingest_tree` calls, which never span a
//! fork).

pub mod claim;
pub mod framing;
pub mod ifd;
pub mod parent;

pub use claim::ClaimTable;
pub use parent::{EvalParentOpts, run_eval_parent};
