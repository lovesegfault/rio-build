//! `rio build` — the native-protocol build client (ADR-024, the
//! coordinator half).
//!
//! Process architecture (ADR-024 "The `rio build` client"): this crate
//! is the **coordinator** — pure Rust, tokio/tonic, gRPC to the
//! cluster. It owns the attr work queue, the global digest state, the
//! client CAS handle, and the cluster connection; it **never forks**.
//! Evaluation happens in a separate eval-parent process (C++ libexpr +
//! fork workers) connected over one
//! `socketpair(AF_UNIX, SOCK_STREAM)` speaking length-delimited
//! `rio.evaljob` proto frames ([`framing`] / [`evalchan`]).
//!
//! The coordinator pipeline ([`coordinator`]) runs the five ADR
//! stages with eval and upload overlapping: fold incoming skeleton
//! nodes by digest, negotiate presence (bulk `Has` per object kind,
//! short-circuited by the persistent cluster-ack table in [`acks`]),
//! upload misses largest-first, submit per root on the all-acked gate,
//! and render per-drv status lines from the `BuildEvent` streams.

pub mod acks;
pub mod config;
pub mod coordinator;
pub mod evalchan;
pub mod fetch;
pub mod framing;
pub mod import;
pub mod render;
