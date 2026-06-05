//! The materialization settlement laws (bug_182 + merged_bug_055).
//!
//! Two pure decisions every consumption arm used to improvise:
//!
//! 1. **The ack law** ([`consumption_ack`]): what the report intake
//!    answers the store, as a function of the close write's
//!    disposition. Before this law existed, every arm acknowledged
//!    unconditionally — a `Failed` close (PG blip) still answered OK,
//!    which killed the store's 600-second report redelivery, so the
//!    attempt settled ~an hour later through the CHARGED 'unreported'
//!    establishment sweep. `Failed ⇒ NackRetryable` turns that into a
//!    free retry. `Fenced ⇒ Ack` is the signed Q20 posture (deposed
//!    believers ack — the successor's establishment owns the row;
//!    NACKing from a deposed replica would burn the store's report
//!    budget against a replica that can never settle it). Per the
//!    signed Q20 record: if the deposed-but-serving residual ever
//!    warrants it, the NACK alternative is a ONE-LINE change — flip
//!    the `Fenced` arm of [`consumption_ack`] to `NackRetryable`.
//!
//! 2. **The companion law** ([`companion_follow_up`]): what happens to
//!    the in-memory claim when a post-close companion write (job
//!    resolve, park verdict) does not settle. Before this law, a
//!    `Failed` resolve left the entry claimed with its attempt already
//!    closed — the claimed-no-attempt ghost (claim wedged until the
//!    establishment sweep). `Failed ⇒ ReleaseClaimFallback`:
//!    claimable-but-unparked strictly dominates wedged-claimed-forever
//!    (the durable row is still the authority; the next consumer
//!    re-decides). `Fenced ⇒ Inert` — a deposed believer mutates
//!    nothing it no longer owns.
//!
//! Both functions are total over [`WriteDisposition`] (lifted here
//! from the scheduler so the laws and their input alphabet live
//! together) and CBMC-swept below.

/// One fenced durable write's disposition — the input alphabet of both
/// settlement laws (`sched.materialize.view-settlement`).
///
/// `Applied`/`AlreadyResolved` (= settled) authorize view mutations
/// and companions; `Fenced` means a deposed believer (mutate nothing);
/// `Failed` means the write errored (PG unavailable, …).
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDisposition {
    /// The durable write applied (rows > 0): the at-most-once edge.
    Applied,
    /// Already settled durably by an earlier write (idempotent
    /// re-entry; not the at-most-once edge).
    AlreadyResolved,
    /// The claims-floor fence refused the write (deposed believer).
    Fenced,
    /// The write errored (PG unavailable, …).
    Failed,
}

impl WriteDisposition {
    /// Whether the durable state is SETTLED (applied now or earlier)
    /// — the only dispositions that authorize removing a view entry
    /// or running a companion action.
    pub fn settled(self) -> bool {
        matches!(self, Self::Applied | Self::AlreadyResolved)
    }
}

/// What the report intake answers the store for one consumption.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumptionAck {
    /// Acknowledge: the report is consumed (settled) or unservable by
    /// this replica forever (fenced — the successor owns it).
    Ack,
    /// Refuse retryably: the close write did not become durable; the
    /// store's report redelivery (600 s) re-presents the SAME outcome
    /// and the idempotent close retries — strictly better than the
    /// charged 'unreported' establishment settling it an hour later.
    NackRetryable,
}

/// The ack law (bug_182): `Ack ⟺ settled ∨ fenced`.
// r[impl sched.materialize.ack-law]
pub fn consumption_ack(close: WriteDisposition) -> ConsumptionAck {
    match close {
        WriteDisposition::Applied
        | WriteDisposition::AlreadyResolved
        | WriteDisposition::Fenced => ConsumptionAck::Ack,
        WriteDisposition::Failed => ConsumptionAck::NackRetryable,
    }
}

/// What happens to the in-memory claim after a companion write.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionFollowUp {
    /// The companion settled durably — its own follow-through
    /// (view removal, requeue) proceeds.
    Settled,
    /// Deposed believer: mutate nothing this replica no longer owns.
    Inert,
    /// The companion write failed: release the claim UNCHARGED so the
    /// job is claimable-but-unparked instead of wedged-claimed-forever
    /// (merged_bug_055's ghost family).
    ReleaseClaimFallback,
}

/// The companion law (merged_bug_055): `Failed ⇒ ReleaseClaimFallback`,
/// never a silently kept claim.
// r[impl sched.materialize.ack-law]
pub fn companion_follow_up(companion: WriteDisposition) -> CompanionFollowUp {
    match companion {
        WriteDisposition::Applied | WriteDisposition::AlreadyResolved => CompanionFollowUp::Settled,
        WriteDisposition::Fenced => CompanionFollowUp::Inert,
        WriteDisposition::Failed => CompanionFollowUp::ReleaseClaimFallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ack law's full table — the unit twin of the CBMC sweep.
    #[test]
    fn ack_iff_settled_or_fenced() {
        for d in [
            WriteDisposition::Applied,
            WriteDisposition::AlreadyResolved,
            WriteDisposition::Fenced,
            WriteDisposition::Failed,
        ] {
            let expect_ack = d.settled() || d == WriteDisposition::Fenced;
            assert_eq!(
                consumption_ack(d) == ConsumptionAck::Ack,
                expect_ack,
                "{d:?}"
            );
        }
    }

    /// The companion law's full table: failed writes ALWAYS fall back
    /// to release; fenced writes are inert; settled writes proceed.
    #[test]
    fn companion_failed_always_releases() {
        assert_eq!(
            companion_follow_up(WriteDisposition::Failed),
            CompanionFollowUp::ReleaseClaimFallback
        );
        assert_eq!(
            companion_follow_up(WriteDisposition::Fenced),
            CompanionFollowUp::Inert
        );
        assert_eq!(
            companion_follow_up(WriteDisposition::Applied),
            CompanionFollowUp::Settled
        );
        assert_eq!(
            companion_follow_up(WriteDisposition::AlreadyResolved),
            CompanionFollowUp::Settled
        );
    }
}

#[cfg(kani)]
mod proofs {
    //! CBMC harnesses for the settlement laws: total over the
    //! 4-variant disposition alphabet, each law pinned to its
    //! defining biconditional.

    use super::*;

    fn any_disposition() -> WriteDisposition {
        match kani::any::<u8>() % 4 {
            0 => WriteDisposition::Applied,
            1 => WriteDisposition::AlreadyResolved,
            2 => WriteDisposition::Fenced,
            _ => WriteDisposition::Failed,
        }
    }

    /// `Ack ⟺ settled ∨ fenced` — equivalently, NACK exactly on
    /// `Failed`. A future disposition variant routed to the wrong arm
    /// breaks this biconditional.
    #[kani::proof]
    fn check_consumption_ack_iff_settled_or_fenced() {
        let d = any_disposition();
        let ack = consumption_ack(d) == ConsumptionAck::Ack;
        assert_eq!(ack, d.settled() || d == WriteDisposition::Fenced);
        assert_eq!(
            consumption_ack(d) == ConsumptionAck::NackRetryable,
            d == WriteDisposition::Failed
        );
    }

    /// `Failed ⇒ ReleaseClaimFallback` and nothing else releases:
    /// the wedged-claim ghost is unrepresentable through this law.
    #[kani::proof]
    fn check_companion_failed_always_releases() {
        let d = any_disposition();
        let follow = companion_follow_up(d);
        assert_eq!(
            follow == CompanionFollowUp::ReleaseClaimFallback,
            d == WriteDisposition::Failed
        );
        assert_eq!(
            follow == CompanionFollowUp::Inert,
            d == WriteDisposition::Fenced
        );
        assert_eq!(follow == CompanionFollowUp::Settled, d.settled());
    }
}
