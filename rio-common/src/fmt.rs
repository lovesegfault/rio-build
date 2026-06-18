//! Human-readable formatting helpers shared across the workspace.
//!
//! Hoisted from `rio-builder/src/banner.rs` (sh-042): the gateway's
//! re-dispatch marker renders the same `(mem, disk)` byte sizing as
//! the builder's `rio: builder` header, so the formatter lives here
//! instead of in a bin crate.

/// IEC-prefixed size formatter shared by the builder banner's
/// `(Nc, mem, disk)` triple, the footer's `rio: peaks` line, and the
/// gateway's `rio: retry … re-dispatching at <size>` marker.
///
/// live_058-d + merged_bug_004: the law is the VIOLATION CLASS, total
/// over the input domain — a present NON-ZERO size never renders as
/// zero at ANY unit — quantifier: census(present_nonzero_never_renders_zero_at_any_unit) — (the live incident's 45-69 MB
/// raw-stamps read as "no memory assigned" during diagnosis; the
/// header doc above forbids claiming a precision the banner doesn't
/// have, and rounding a present value to zero is the inverse violation
/// — at every rung, not just the GiB one the incident happened to
/// hit). The unit ladder descends to the first rung that preserves a
/// non-zero magnitude and bottoms out at bytes; absent stays "? GiB".
/// Pinned by the `present_nonzero_never_renders_zero_at_any_unit`
/// property.
pub fn fmt_size_iec(b: Option<u64>) -> String {
    match b {
        None => "? GiB".to_string(),
        Some(n) if n >= 1 << 30 => format!("{} GiB", n >> 30),
        Some(n) if n >= 1 << 20 => format!("{} MiB", n >> 20),
        Some(n) if n >= 1 << 10 => format!("{} KiB", n >> 10),
        Some(n) => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W11-Y (merged_bug_004): the formatter's law is the VIOLATION
    /// CLASS, total over the input domain — a present non-zero size
    /// NEVER renders as zero at ANY unit — quantifier: census(present_nonzero_never_renders_zero_at_any_unit). The live_058-d fix closed
    /// the truncation-to-zero at the GiB rung and the W10-CL matrix
    /// pinned three cells; the fallthrough still rendered `n >> 20`,
    /// so any present value in [1, 1 MiB) printed "0 MiB" — the
    /// identical shape one granularity down (and "0 KiB" would be
    /// one below that). The population that reaches this banner is
    /// by definition the mis-scaled anomalous one the fix exists to
    /// stay honest for.
    ///
    /// Hoisted from `rio-builder/src/banner.rs` (sh-042) so the
    /// property pins the canonical fn, not a private re-import.
    #[test]
    fn present_nonzero_never_renders_zero_at_any_unit() {
        use proptest::prelude::*;
        // The example red (pre-fix, verbatim in the owning commit):
        // 512 KiB rendered "0 MiB".
        assert_eq!(fmt_size_iec(Some(512 << 10)), "512 KiB");
        // Sub-KiB falls to bytes.
        assert_eq!(fmt_size_iec(Some(37)), "37 B");
        assert_eq!(fmt_size_iec(Some(1)), "1 B");
        // Absent stays "? GiB".
        assert_eq!(fmt_size_iec(None), "? GiB");

        // The property at the formatter's own domain quantifier:
        // nonzero in ⇒ never `0 <unit>` out, domain-wide.
        proptest!(|(n in 1u64..=u64::MAX)| {
            let rendered = fmt_size_iec(Some(n));
            prop_assert!(
                !rendered.starts_with("0 "),
                "present non-zero {} rendered as zero: {}", n, rendered
            );
        });
    }
}
