//! Bidirectional mapping between the nix-daemon wire `BuildStatus`
//! ([`rio_nix::protocol::build::BuildStatus`]) and the proto
//! [`BuildResultStatus`].
//!
//! 12 of 15 nix variants overlap 1:1 with proto. The remainder collapse
//! intentionally (see arm comments). Both directions are exhaustive — no
//! `_` arm — so adding a variant to either enum is a compile error here
//! until the mapping decision is made.
//!
//! - `From<NixStatus>` is the **builder→scheduler** path: nix-daemon
//!   reports a wire status, builder converts to proto for `BuildResult`.
//! - `From<BuildResultStatus>` is the **scheduler→gateway→client** path:
//!   scheduler reports a proto status in `BuildFailed`, gateway converts
//!   back to the nix wire status the client expects.

use rio_nix::protocol::build::BuildStatus as NixStatus;

use crate::types::BuildResultStatus;

// r[impl builder.status.nix-to-proto]
impl From<NixStatus> for BuildResultStatus {
    fn from(nix: NixStatus) -> Self {
        match nix {
            NixStatus::Built => BuildResultStatus::Built,
            NixStatus::Substituted => BuildResultStatus::Substituted,
            NixStatus::AlreadyValid | NixStatus::ResolvesToAlreadyValid => {
                BuildResultStatus::AlreadyValid
            }
            NixStatus::PermanentFailure => BuildResultStatus::PermanentFailure,
            NixStatus::TransientFailure => BuildResultStatus::TransientFailure,
            NixStatus::CachedFailure => BuildResultStatus::CachedFailure,
            NixStatus::DependencyFailed => BuildResultStatus::DependencyFailed,
            NixStatus::LogLimitExceeded => BuildResultStatus::LogLimitExceeded,
            NixStatus::OutputRejected => BuildResultStatus::OutputRejected,
            NixStatus::InputRejected => BuildResultStatus::InputRejected,
            NixStatus::TimedOut => BuildResultStatus::TimedOut,
            NixStatus::NotDeterministic => BuildResultStatus::NotDeterministic,
            // MiscFailure is nix-daemon's own catch-all (used when it can't
            // classify). PermanentFailure is the honest proto equivalent —
            // "it failed, we don't know why, don't retry."
            NixStatus::MiscFailure => BuildResultStatus::PermanentFailure,
            // Workers run with `substitute = false` (WORKER_NIX_CONF) — we
            // never ask the daemon to substitute. If we see this, something
            // is misconfigured; PermanentFailure + the error_msg is the
            // right signal.
            NixStatus::NoSubstituters => BuildResultStatus::PermanentFailure,
        }
    }
}

impl From<BuildResultStatus> for NixStatus {
    fn from(proto: BuildResultStatus) -> Self {
        match proto {
            BuildResultStatus::Built => NixStatus::Built,
            BuildResultStatus::Substituted => NixStatus::Substituted,
            BuildResultStatus::AlreadyValid => NixStatus::AlreadyValid,
            BuildResultStatus::PermanentFailure => NixStatus::PermanentFailure,
            BuildResultStatus::TransientFailure => NixStatus::TransientFailure,
            BuildResultStatus::CachedFailure => NixStatus::CachedFailure,
            BuildResultStatus::DependencyFailed => NixStatus::DependencyFailed,
            BuildResultStatus::LogLimitExceeded => NixStatus::LogLimitExceeded,
            BuildResultStatus::OutputRejected => NixStatus::OutputRejected,
            BuildResultStatus::InputRejected => NixStatus::InputRejected,
            BuildResultStatus::TimedOut => NixStatus::TimedOut,
            BuildResultStatus::NotDeterministic => NixStatus::NotDeterministic,
            // Proto-only variants. Nix has no `Cancelled` or
            // `InfrastructureFailure` — both are rio's classification on
            // top of the daemon's. TransientFailure tells the nix client
            // "this might work if retried", which is true for both: a
            // cancel can be re-submitted, and infra failures (worker
            // crash, FUSE EIO) are by definition not the build's fault.
            BuildResultStatus::Cancelled | BuildResultStatus::InfrastructureFailure => {
                NixStatus::TransientFailure
            }
            // Unspecified = scheduler didn't populate the status field
            // (old scheduler, or a code path that doesn't classify yet).
            // Preserve the historical gateway behaviour of MiscFailure.
            BuildResultStatus::Unspecified => NixStatus::MiscFailure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the nix→proto mapping so a regression (e.g., TimedOut →
    /// TransientFailure, which would reintroduce the reassignment storm)
    /// fails CI.
    // r[verify builder.status.nix-to-proto]
    #[test]
    fn nix_to_proto_exhaustive_and_stable() {
        let one_to_one = [
            (
                NixStatus::PermanentFailure,
                BuildResultStatus::PermanentFailure,
            ),
            (
                NixStatus::TransientFailure,
                BuildResultStatus::TransientFailure,
            ),
            (NixStatus::CachedFailure, BuildResultStatus::CachedFailure),
            (
                NixStatus::DependencyFailed,
                BuildResultStatus::DependencyFailed,
            ),
            (
                NixStatus::LogLimitExceeded,
                BuildResultStatus::LogLimitExceeded,
            ),
            (NixStatus::OutputRejected, BuildResultStatus::OutputRejected),
            (NixStatus::InputRejected, BuildResultStatus::InputRejected),
            (NixStatus::TimedOut, BuildResultStatus::TimedOut),
            (
                NixStatus::NotDeterministic,
                BuildResultStatus::NotDeterministic,
            ),
        ];
        for (nix, want) in one_to_one {
            assert_eq!(
                BuildResultStatus::from(nix),
                want,
                "1:1 mapping broke for {nix:?}"
            );
        }
        assert_eq!(
            BuildResultStatus::from(NixStatus::MiscFailure),
            BuildResultStatus::PermanentFailure
        );
        assert_eq!(
            BuildResultStatus::from(NixStatus::NoSubstituters),
            BuildResultStatus::PermanentFailure
        );
    }

    /// Round-trip: every proto status maps to SOME nix status, and every
    /// nix status that has a 1:1 proto peer round-trips back to itself.
    /// Collapsed variants (MiscFailure, NoSubstituters,
    /// ResolvesToAlreadyValid) intentionally don't round-trip.
    #[test]
    fn proto_to_nix_round_trip() {
        for nix in [
            NixStatus::Built,
            NixStatus::Substituted,
            NixStatus::AlreadyValid,
            NixStatus::PermanentFailure,
            NixStatus::TransientFailure,
            NixStatus::CachedFailure,
            NixStatus::DependencyFailed,
            NixStatus::LogLimitExceeded,
            NixStatus::OutputRejected,
            NixStatus::InputRejected,
            NixStatus::TimedOut,
            NixStatus::NotDeterministic,
        ] {
            assert_eq!(NixStatus::from(BuildResultStatus::from(nix)), nix);
        }
        assert_eq!(
            NixStatus::from(BuildResultStatus::Cancelled),
            NixStatus::TransientFailure
        );
        assert_eq!(
            NixStatus::from(BuildResultStatus::InfrastructureFailure),
            NixStatus::TransientFailure
        );
        assert_eq!(
            NixStatus::from(BuildResultStatus::Unspecified),
            NixStatus::MiscFailure
        );
    }
}
