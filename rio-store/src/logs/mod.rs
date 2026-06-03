//! Build-log storage.
//!
//! The store is the system of record for build logs: builders stream
//! `BuildLogBatch`es to `LogService.AppendLog`, the store cuts immutable
//! zstd chunks to S3 (`logs/{drv_hash}/{exec_id}/{session_id}/{seq}.zst`)
//! with a `drv_log_chunks` manifest row per chunk, and readers
//! (`LogService.TailLog`) reassemble the log from the manifest's
//! line-range union. See `docs/spec/system/observability.typ` and the
//! harden-logs design for the full architecture.
//!
//! Submodules (added incrementally):
//! - [`kernel`] — the pure decision kernels (chunk-interval arithmetic,
//!   the read-path overlap dedup, the accept verdict, the completeness
//!   fold), re-exported from the dependency-free `rio-log-kernel` crate
//!   (the kani-verified decision kernels; see its crate docs). No I/O,
//!   no allocation; the other submodules project their inputs into
//!   these and apply the returned verdicts.
//! - [`chunks`] — the chunk key scheme, the line codec, and the
//!   [`chunks::LogChunkStore`] storage abstraction (S3 + in-memory).
//! - [`sessions`] — the live-ingest session registry (one `AppendLog`
//!   stream per execution; routes `TailLog` readers to the ingesting
//!   replica).
//! - [`gate`] — the per-stream-open binding + completeness check (may
//!   this token write to this execution's log?).
//! - [`ingest`] — the per-stream state machine: input bounds, the
//!   in-memory buffer + live-tail fan-out, the chunk cutter, and the
//!   gray-failure abort.
//! - [`tail`] — the manifest-driven read path: chunk selection by line
//!   range, fetch + decompress + overlap dedup, and latest-execution
//!   resolution.
//! - [`service`] — the `LogService` tonic handlers that wire the above
//!   together: token verification, admission, the ingest driver loop,
//!   the live-ingest registry, and the history→live seam.
//! - [`sweep`] — the hourly TTL sweep that deletes expired executions'
//!   manifest rows and chunk objects once `log_retention_days` have
//!   passed since the execution started.

pub mod chunks;
pub mod gate;
pub mod ingest;
pub use rio_log_kernel as kernel;
#[cfg(test)]
mod mbt_tests;
pub mod service;
pub mod sessions;
pub mod sweep;
pub mod tail;

pub use service::LogServiceImpl;
