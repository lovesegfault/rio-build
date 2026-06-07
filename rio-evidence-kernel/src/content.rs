//! Content-binding agreement law for already-complete substitution
//! claims (merged_bug_114, round-3 HIGH).
//!
//! The `AlreadyComplete` claim arm in `rio-store/src/substitute.rs`
//! reaches a STORED row through a narinfo that names the path and
//! verifies against tenant-supplied `trusted_keys` — which is NOT a
//! trust boundary — and it runs BEFORE any body fetch. The stored row
//! may be served as that upstream's Hit (and the upstream's
//! signatures appended over it) only when the upstream's claim AGREES
//! with the stored content on EVERY axis: nar hash, nar size, and the
//! reference set. Disagreement on any axis means the upstream does
//! not have *this* path's bytes — Miss for that upstream, no
//! signature append.
//!
//! This module is the ONE decision body for that agreement
//! (laws-as-types: the arm routes through [`already_complete_agrees`]
//! rather than open-coding the conjunction), and the `#[cfg(kani)]`
//! harness proves AXIS TOTALITY over the bounded domain: agreement
//! holds exactly when all three axes are equal — so a future edit
//! that drops an axis from the conjunction (the round-3
//! axis-blind-predicate class) flips the proof red, not just a unit
//! test.

/// One side's content facts for the agreement decision: the claimed
/// (upstream narinfo) or stored (ingested row) image of a path's
/// bytes. Generic over the hash and reference-set representations so
/// the production call site can pass its `[u8; 32]` hash and
/// `BTreeSet<String>` references while the CBMC harness instantiates
/// small bounded images of the same decision body (the kani.nix
/// bounded-representation discipline — heap collections under CBMC
/// reach `getrandom`).
#[derive(Debug, PartialEq, Eq)]
pub struct ContentFacts<H, R> {
    /// The NAR hash image (production: `[u8; 32]`).
    pub nar_hash: H,
    /// The NAR size in bytes.
    pub nar_size: u64,
    /// The reference-set image (production: `BTreeSet<String>`;
    /// order-insensitive equality is the representation's duty).
    pub references: R,
}

/// The agreement law: an `AlreadyComplete` claim may be served as a
/// Hit (and its signatures appended) iff the claimed facts agree with
/// the stored facts on EVERY axis. Mismatch on any axis ⇒ never Hit.
// r[impl store.substitute.content-binding]
#[must_use]
pub fn already_complete_agrees<H: Eq, R: Eq>(
    stored: &ContentFacts<H, R>,
    claimed: &ContentFacts<H, R>,
) -> bool {
    stored.nar_hash == claimed.nar_hash
        && stored.nar_size == claimed.nar_size
        && stored.references == claimed.references
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Axis totality (merged_bug_114): over the full bounded domain,
    /// `already_complete_agrees` answers `true` EXACTLY when all
    /// three axes are equal — both directions of the law. The
    /// mismatch direction is the spec obligation ("mismatch ⇒ never
    /// Hit"); the agreement direction pins that no FOURTH phantom
    /// condition can refuse an honest race winner. Bounded images:
    /// `u8` hash, `[u8; 2]` reference set — the generic body is the
    /// SAME monomorphized conjunction the production `[u8; 32]` /
    /// `BTreeSet<String>` instantiation compiles.
    #[kani::proof]
    fn check_content_binding_axis_totality() {
        let stored = ContentFacts {
            nar_hash: kani::any::<u8>(),
            nar_size: kani::any::<u64>(),
            references: [kani::any::<u8>(), kani::any::<u8>()],
        };
        let claimed = ContentFacts {
            nar_hash: kani::any::<u8>(),
            nar_size: kani::any::<u64>(),
            references: [kani::any::<u8>(), kani::any::<u8>()],
        };
        let agrees = already_complete_agrees(&stored, &claimed);
        let all_axes_equal = stored.nar_hash == claimed.nar_hash
            && stored.nar_size == claimed.nar_size
            && stored.references == claimed.references;
        assert_eq!(agrees, all_axes_equal);
        // The directional spec phrasing, pinned explicitly: ANY
        // single-axis mismatch refuses agreement.
        if stored.nar_hash != claimed.nar_hash
            || stored.nar_size != claimed.nar_size
            || stored.references != claimed.references
        {
            assert!(!agrees);
        }
    }
}
