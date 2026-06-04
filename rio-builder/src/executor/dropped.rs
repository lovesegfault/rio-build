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

    /// Iterate the members (detection-only candidate augmentation).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
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

/// The CLOSED table of verdict-producing arms that consult the
/// dropped evidence, with each arm's typed disposition — landing in
/// the same commit that completes its governed population (the
/// round-17 migration-completeness obligation). Test-gated: the table
/// IS the completeness mechanism (its exhaustive `site_marker` match +
/// the site-pin test below); production behavior lives at the four
/// consulting sites the markers name. A new verdict arm (or a removed
/// consultation) fails CI, not review.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictArm {
    /// Glue rejections (exportReferencesGraph family): a rejection
    /// hinging on a dropped member re-classifies as infrastructure
    /// (`MetadataFetch` → re-dispatch re-resolves). Round-16 a8db9717a.
    GlueRejection,
    /// `execve` failures: ENOENT/ENOTDIR with gaps present attribute
    /// to the gap (ENOENT-only dominance — absence cannot produce any
    /// other derivation-caused exec errno).
    ExecErrno,
    /// Exit classification: a failed gap-dispatched build is
    /// infrastructure (rung below OOM/disk-full, above
    /// network-transient, never overriding kill-class); bounded by
    /// the scheduler's infra retry caps.
    ExitClassification,
    /// Registration: an output whose recorded references include a
    /// dropped member must not register (the GC-collects-a-live-dep
    /// corruption channel); detection-only candidate augmentation,
    /// gated BEFORE the policy passes.
    Registration,
}

#[cfg(test)]
impl VerdictArm {
    /// Every arm — the exhaustive enumeration the completeness test
    /// walks.
    pub(crate) const ALL: [VerdictArm; 4] = [
        VerdictArm::GlueRejection,
        VerdictArm::ExecErrno,
        VerdictArm::ExitClassification,
        VerdictArm::Registration,
    ];

    /// The grep-able marker each consulting site carries; the
    /// completeness test asserts each appears in the named sources
    /// exactly once.
    pub(crate) fn site_marker(self) -> &'static str {
        match self {
            VerdictArm::GlueRejection => "VerdictArm::GlueRejection consults dropped",
            VerdictArm::ExecErrno => "VerdictArm::ExecErrno consults dropped",
            VerdictArm::ExitClassification => "VerdictArm::ExitClassification consults dropped",
            VerdictArm::Registration => "VerdictArm::Registration consults dropped",
        }
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

    /// The closed table's SITE PIN: every arm's consulting site
    /// exists in the source, exactly once, carrying the arm's marker
    /// — and the exhaustive match in `site_marker` forces a new arm
    /// to declare its marker before this test can even name it.
    // r[verify builder.result.input-materialization-is-infra+6]
    #[test]
    fn every_verdict_arm_has_exactly_one_consulting_site() {
        let sources: [(&str, &str); 2] = [
            ("executor/mod.rs", include_str!("mod.rs")),
            (
                "executor/native_result/mod.rs",
                include_str!("native_result/mod.rs"),
            ),
        ];
        for arm in VerdictArm::ALL {
            let marker = arm.site_marker();
            let count: usize = sources
                .iter()
                .map(|(_, src)| {
                    // Count marker COMMENT lines, not the string
                    // literals inside site_marker itself (this file is
                    // not in `sources`, so no self-match).
                    src.matches(marker).count()
                })
                .sum();
            assert_eq!(
                count, 1,
                "{arm:?}: expected exactly one consulting site carrying \
                 '{marker}', found {count} — a verdict arm landed or moved \
                 without its dropped-evidence consultation"
            );
        }
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
