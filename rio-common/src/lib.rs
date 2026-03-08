//! Shared configuration, observability, and utility types.
//!
//! Leaf crate — no `rio-*` dependencies. Provides the `string_newtype!` macro
//! and [`DrvHash`](newtype::DrvHash) / [`WorkerId`](newtype::WorkerId) shared
//! across the workspace, plus [`limits`] constants, [`observability`] init,
//! and the self-describing [`BloomFilter`](bloom::BloomFilter).

pub mod bloom;
pub mod config;
pub mod grpc;
pub mod limits;
pub mod newtype;
pub mod observability;
pub mod task;
pub mod tls;
