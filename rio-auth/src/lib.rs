//! Tenant JWT + assignment HMAC token sign/verify, and the gRPC
//! interceptor that wires them.
//!
//! Extracted from `rio-common`: the auth stack is the largest cohesive
//! sub-tree in what was a 17-module god crate, and rebuilding every
//! binary on a JWT-comment edit was the dominant rebuild bottleneck.
//! Depends on `rio-common` for `signal::Token` / `sighup_reload` and
//! the `TENANT_TOKEN_HEADER` constant; nothing in `rio-common` depends
//! back here.

pub mod hmac;
pub mod jwt;
pub mod jwt_interceptor;

/// Clock-read errors. Both [`hmac`] and [`jwt`] gate token validity on
/// "now ≥ epoch"; a pre-epoch system clock means the comparison is
/// meaningless and MUST surface as an error rather than silently
/// accepting (hmac's old `unwrap_or(0)`) or panicking (a stray
/// `.expect`). One shared helper, one behavior.
#[derive(Debug, thiserror::Error)]
#[error("system clock is before UNIX_EPOCH (off by {0:?}) — token expiry undecidable")]
pub struct ClockBeforeEpoch(pub std::time::Duration);

/// Current Unix time in seconds, or [`ClockBeforeEpoch`] if the
/// system clock predates 1970.
///
/// Shared by [`hmac::HmacVerifier::verify`] (expiry check) and the JWT
/// test scaffolding so both modules agree on pre-epoch handling.
/// `jsonwebtoken`'s own `exp` validation reads the clock internally;
/// it errors (not panics) on pre-epoch, so JWT verify already matches
/// this contract.
pub(crate) fn now_unix() -> Result<u64, ClockBeforeEpoch> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| ClockBeforeEpoch(e.duration()))
}
