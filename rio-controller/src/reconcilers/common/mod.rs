//! Shared reconciler building blocks.
//!
//! The builder/fetcher split (ADR-019) means two CRDs produce
//! near-identical Job pod specs and follow the same Job-mode
//! reconcile skeleton. `pod.rs` holds the shared pod-spec shape
//! (FUSE volumes, TLS mounts, coverage propagation, RUST_LOG
//! passthrough, probes, capabilities); `job.rs` holds the shared
//! Job-lifecycle plumbing (spawn/reap/status-patch helpers, requeue
//! interval, TTL). The fetcher reconciler stays ~100 lines instead
//! of copy-pasting 1100 + reaching through `builderpool::` for
//! helpers.

pub mod job;
pub mod pod;
