//! Resolve-time residency-gap evidence, typed (round-17 merged_bug_054
//! / RC17-12).
//!
//! When the input-closure resolve finds a member absent from the store
//! (read lag or a GC race), the member is dropped from the closure and
//! the build is dispatched WITH THE GAP — tolerate-and-arbitrate, the
//! premise of the round-16 dropped-set design (a8db9717a). The drop is
//! EVIDENCE, not silence: every verdict-producing arm that could
//! otherwise launder the gap into a permanent verdict must consult it.
//!
//! The CLOSED table of those arms (`VerdictArm`) and its
//! completeness mechanism land in the same commits that wire the
//! remaining arms (the migration-completeness obligation: mechanism
//! ships with the population it governs). The pre-registered
//! escalation for this family — refuse-at-resolve, which deletes all
//! four arm populations at the cost of gap-tolerant successes — is
//! deliberately NOT taken (owner decision Q5, round-17).

use std::collections::BTreeSet;

/// The typed dropped-set evidence. Constructed once at resolve time
/// ([`from_resolve`](Self::from_resolve)); consumed read-only by the
/// verdict arms.
#[derive(Debug, Clone, Default)]
pub(crate) struct DroppedInputs(BTreeSet<String>);

impl DroppedInputs {
    /// Wrap the resolve loop's dropped set. The only constructor: the
    /// evidence is minted where the drop happens, nowhere else.
    pub(crate) fn from_resolve(dropped: BTreeSet<String>) -> Self {
        Self(dropped)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Exact-member containment.
    pub(crate) fn contains(&self, path: &str) -> bool {
        self.0.contains(path)
    }

    /// The member this path equals or sits under (`<member>/...`),
    /// if any — the same prefix logic the round-16 glue arbitration
    /// established.
    pub(crate) fn covering_member(&self, path: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|p| path == p.as_str() || path.starts_with(&format!("{p}/")))
            .map(String::as_str)
    }

    /// Human summary for verdict messages: count plus the members.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} resolve-time residency gap(s): {}",
            self.0.len(),
            self.0
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DroppedInputs {
        DroppedInputs::from_resolve(
            ["/nix/store/aaaa-dep", "/nix/store/bbbb-lib"]
                .into_iter()
                .map(String::from)
                .collect(),
        )
    }

    #[test]
    fn covering_member_matches_exact_and_prefix_never_sibling() {
        let d = sample();
        assert_eq!(
            d.covering_member("/nix/store/aaaa-dep"),
            Some("/nix/store/aaaa-dep")
        );
        assert_eq!(
            d.covering_member("/nix/store/aaaa-dep/lib/x.so"),
            Some("/nix/store/aaaa-dep")
        );
        // A sibling sharing the prefix bytes is NOT covered.
        assert_eq!(d.covering_member("/nix/store/aaaa-dep2"), None);
        assert_eq!(d.covering_member("/nix/store/cccc-other"), None);
    }
}
