//! Shared configuration, observability, and utility types.
//!
//! Leaf crate — no `rio-*` dependencies. Provides [`limits`] constants,
//! [`observability`] init, and gRPC/TLS plumbing shared across the
//! workspace. The JWT/HMAC sign-verify stack moved to `rio-auth`;
//! only the [`JwtConfig`](config::JwtConfig) serde struct and the
//! `*_TOKEN_HEADER` string constants in [`grpc`] remain here.
//!
//! `missing_docs` is DENIED crate-wide (WO-S8-8/bug_144): rio-common
//! is an invariant-carrier crate (the wanted-outputs never-shrink
//! contract, the clamped wire types, the transport bounds), and
//! rustdoc attachment is positional — an edit inserting an item
//! between a doc block and its item silently re-targets a
//! load-bearing contract (hit twice: e163c2d77, then the
//! `saturating_wanted_union` splice). Under the deny, a doc-detached
//! pub item is a COMPILE ERROR at the splice commit instead of a
//! silent contract re-target.
#![deny(missing_docs)]

pub mod backoff;
pub mod cell_wire;
pub mod clamped;
pub mod classify;
pub mod config;
pub mod cors;
pub mod dns;
pub mod fmt;
pub mod footprint;
pub mod grpc;
pub mod k8s;
pub mod limits;
pub mod liveness;
pub mod newtype;
pub mod observability;
#[cfg(feature = "pg-iam")]
pub mod pg_error;
#[cfg(feature = "pg-iam")]
pub mod pg_iam;
#[cfg(feature = "aws")]
pub mod s3;
pub mod server;
pub mod signal;
pub mod task;
pub mod tenant;
#[cfg(test)]
pub(crate) mod test_jail;
pub mod transport;
pub mod wanted_outputs;

/// Default bind address for a service port. Used in config defaults.
///
/// `[::]` (v6 unspecified) binds dual-stack on Linux's default
/// `net.ipv6.bindv6only=0` — accepts native v6 AND v4-mapped (`::ffff:a.b.c.d`).
/// One socket, both families. P0542: builders may run on v6-only pod
/// CIDR (I-073/I-079 IPv4 subnet exhaustion); the in-cluster services
/// they dial bind here and answer on whichever family the Service routes.
// r[impl common.helpers]
pub fn default_addr(port: u16) -> std::net::SocketAddr {
    (std::net::Ipv6Addr::UNSPECIFIED, port).into()
}

/// Convert a byte/count budget (`u64`, the config-surface type) into a
/// tokio semaphore permit count, saturating at BOTH boundaries:
/// `usize` on 32-bit targets and [`tokio::sync::Semaphore::MAX_PERMITS`]
/// everywhere (`Semaphore::new` PANICS above it). Use this instead of
/// open-coded casts or bitmask "clamps" — a mask silently *changes*
/// large budgets (`& (usize::MAX >> 3)` zeroes the high bits, so a
/// pathological config could wrap to a tiny budget instead of
/// saturating).
// r[impl common.helpers]
pub fn semaphore_permits(v: u64) -> usize {
    usize::try_from(v)
        .unwrap_or(usize::MAX)
        .min(tokio::sync::Semaphore::MAX_PERMITS)
}

/// `(Some a, Some b) → Some(f a b); else a.or(b)`. The Option-reduce
/// idiom hand-rolled at every "min/max two optional observations,
/// preferring the one that exists" site (sh-045 r2 raised the count
/// from 1 to 4). Takes the reducer as a closure so `f64` (no `Ord`)
/// and `Ord` types share one body — `opt_reduce(a, b, f64::min)` /
/// `opt_reduce(a, b, std::cmp::min)`.
// r[impl common.helpers]
pub fn opt_reduce<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(f(a, b)),
        (a, b) => a.or(b),
    }
}
