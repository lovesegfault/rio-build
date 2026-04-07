//! Shared reconciler building blocks.
//!
//! The builder/fetcher split (ADR-019) means two CRDs produce
//! near-identical Job pod specs. The diff is a handful of fields
//! (role label, seccomp profile name, readOnlyRootFilesystem,
//! nodeSelector/toleration, RIO_EXECUTOR_KIND env); the rest —
//! FUSE volumes, TLS mounts, coverage propagation, RUST_LOG
//! passthrough, probes, capabilities — is identical. `pod.rs` holds that shared shape so the fetcher
//! reconciler stays ~100 lines instead of copy-pasting 1100.

pub mod pod;
