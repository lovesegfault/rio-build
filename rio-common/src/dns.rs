//! DNS-1123 label identity: ONE alphabet, ONE sanitizer, ONE composer
//! (merged_bug_243; the merged_bug_158 identity discipline, made a
//! type).
//!
//! The store's materialization executor asserts a per-worker identity
//! `{instance}-w{n}` that the scheduler validates with
//! [`is_dns1123_label`] before letting it claim. Pre-fix the two
//! halves enforced the same alphabet from two private copies, and the
//! COMPOSED wire value was never re-checked: the sanitizer guaranteed
//! ≤63 for the base, then the worker suffix pushed any 61–63-char base
//! (long Helm release pod names; the salted arm lands at exactly 63)
//! to 64–66 chars — every claim rejected `InvalidArgument`, silently
//! warn-and-skipped by the poll loop: a deterministic fleet-wide
//! materialization outage keyed on release-name length.
//!
//! The fix is structural: [`Dns1123Label`] is the only way to hold a
//! wire identity, [`Dns1123Label::sanitize`] budgets the suffix INSIDE
//! the 63-char bound (`reserved`), and [`Dns1123Label::with_worker`]
//! is the ONLY composition operation — it re-establishes the invariant
//! by construction, so a raw `format!("{base}-w{n}")` identity simply
//! has no type to ride to the transport (the transport APIs take
//! `&Dns1123Label`).

use std::fmt;

/// RFC-1123 DNS label bound.
pub const DNS1123_MAX_LEN: usize = 63;

/// Suffix budget the store's executor reserves inside the bound for
/// its `-w{n}` worker composition: `"-w"` + up to 3 digits (worker
/// indices are config-bounded far below 1000; [`Dns1123Label::with_worker`]
/// stays total past it regardless).
pub const WORKER_SUFFIX_RESERVED: usize = 5;

/// RFC-1123 DNS label check (a k8s pod-name component): lowercase
/// alphanumerics and interior hyphens, 1–63 chars. The scheduler
/// interpolates `executor_instance` into the composite materialization
/// ExecutorId (`{intent}@{instance}`), so the alphabet exclusion of
/// `@` (and everything else) is what keeps that composite unambiguous.
// r[impl store.materialize.worker-identity]
pub fn is_dns1123_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= DNS1123_MAX_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// FNV-1a 64 over the raw identity — the deterministic disambiguation
/// salt for sanitized labels (the same raw always maps to the same
/// identity across restarts; distinct raws that fold to the same
/// sanitized base get distinct salts — merged_bug_158).
fn fnv1a_64(raw: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A validated DNS-1123 label, the only carrier of an executor wire
/// identity. Constructible solely through [`Self::sanitize`] (which
/// budgets a reserved suffix inside the bound) and [`Self::with_worker`]
/// (the one composition operation, which re-establishes the invariant).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dns1123Label(String);

impl Dns1123Label {
    /// Sanitize an arbitrary hostname into a DNS-1123 label of length
    /// ≤ `DNS1123_MAX_LEN - reserved`, leaving `reserved` chars of
    /// suffix budget for [`Self::with_worker`].
    ///
    /// Output-space law (merged_bug_127): every machine-salted label
    /// ends in `-{8 lowercase hex}`, and the pass-through arm REFUSES
    /// raws of that shape (they route through the salted arm instead).
    /// The two output namespaces are therefore structurally disjoint —
    /// a pass-through identity can never equal a salted identity, by
    /// construction, not by probability.
    ///
    /// bug_005 (R28 secondary-input axis): `fallback_stem` is the
    /// seal's SECOND input axis — the empty-raw arm re-enters
    /// sanitize with the composed fallback instead of embedding the
    /// caller's stem verbatim, so a non-conforming stem cannot mint
    /// an invalid label through the one constructor whose point is
    /// making that unrepresentable (W11-BO pins both axes).
    ///
    /// - A raw that is already a valid label within the budget passes
    ///   through unchanged — unless it is salt-shaped (above).
    /// - A raw that must be ALTERED (case-folded, invalid bytes
    ///   replaced, truncated to fit the budget, or salt-shaped) gets
    ///   an 8-hex deterministic FNV-1a salt of the FULL raw appended.
    ///   Within the salted arm, a collision between two distinct raws
    ///   sharing the truncated stem requires a 32-bit FNV-1a
    ///   collision (~2⁻³² per pair) — the previous 16-bit salt made
    ///   that ~2⁻¹⁶ and DETERMINISTIC forever for an unlucky pair of
    ///   release names (no randomness to escape on restart).
    /// - An empty/garbage raw falls back to `{fallback_stem}-{salt}`
    ///   with a per-process RANDOM 8-hex salt and a loud warning: two
    ///   unidentifiable replicas must still not collide (the fallback
    ///   emits the salted SHAPE, so it shares the salted namespace).
    ///
    /// `reserved` is clamped so at least one identity char survives
    /// next to the salt footprint.
    ///
    /// Deploy boundary (recorded trade, extends the merged_bug_243
    /// note): widening the salt and refusing salt-shaped pass-throughs
    /// shifts the affected identities ONCE at rollout; the scheduler's
    /// establishment sweep absorbs the orphaned claims of the old
    /// identities, exactly as for the original truncation change.
    // r[impl store.materialize.worker-identity]
    pub fn sanitize(raw: &str, reserved: usize, fallback_stem: &str) -> Dns1123Label {
        // 9 = the salt suffix's own footprint ("-xxxxxxxx"), kept
        // inside the budget when the altered arm fires; floor keeps
        // one stem char beside it.
        let budget = DNS1123_MAX_LEN.saturating_sub(reserved).max(10);
        let mut out: String = raw
            .chars()
            .map(|c| match c {
                'a'..='z' | '0'..='9' | '-' => c,
                'A'..='Z' => c.to_ascii_lowercase(),
                _ => '-',
            })
            .take(budget)
            .collect();
        out = out.trim_matches('-').to_string();
        if out.is_empty() {
            // No usable identity at all: salt randomly (per process) so
            // two such replicas never collide, and say so loudly.
            use std::hash::{BuildHasher, Hasher};
            let nonce = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            tracing::warn!(
                raw,
                "no usable identity in raw hostname; using a random-salted dev identity"
            );
            // bug_005 (R28 secondary-input axis): every input axis of
            // a by-construction newtype routes through the seal — the
            // composed fallback RE-ENTERS sanitize, so a
            // non-conforming caller-supplied stem (uppercase,
            // underscores, overlong, empty) is sanitized instead of
            // embedded verbatim into the validated type. Terminates
            // structurally: the recursed raw carries the 8-hex nonce
            // segment, which survives sanitization (non-empty out),
            // so the empty-raw arm cannot re-fire; the salt-shaped
            // raw then takes the deterministic-salt arm, keeping the
            // namespace partition law intact.
            return Self::sanitize(
                &format!("{fallback_stem}-{:08x}", nonce & 0xffff_ffff),
                reserved,
                "rio",
            );
        }
        if out == raw && !Self::is_salt_shaped(&out) {
            return Dns1123Label(out);
        }
        // Sanitization altered the raw (including budget truncation),
        // or the raw wears the salted arm's own shape: disambiguate
        // with the deterministic salt so distinct raws cannot fold to
        // one label and the output namespaces stay disjoint. Base
        // re-trimmed to keep the salt inside the budget.
        out.truncate(budget - 9);
        let out = out.trim_matches('-');
        Dns1123Label(format!("{out}-{:08x}", fnv1a_64(raw) & 0xffff_ffff))
    }

    /// The salted arm's output shape: a final `-` segment of exactly
    /// 8 lowercase hex chars. Pass-through refuses this shape so the
    /// two output namespaces cannot overlap.
    fn is_salt_shaped(s: &str) -> bool {
        s.rsplit_once('-').is_some_and(|(_, seg)| {
            seg.len() == 8
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }

    /// The ONE composition operation: the per-worker wire identity
    /// `{base}-w{n}`. Re-establishes the label invariant by
    /// construction — within the reserved budget the composition is a
    /// plain append; a worker index that would overflow the bound
    /// (≥4 digits under [`WORKER_SUFFIX_RESERVED`]) routes back
    /// through [`Self::sanitize`] with zero reserve, which truncates
    /// and deterministically salts. There is no other way to attach a
    /// worker index to an identity.
    // r[impl store.materialize.worker-identity]
    #[must_use]
    pub fn with_worker(&self, n: usize) -> Dns1123Label {
        let composed = format!("{}-w{n}", self.0);
        if is_dns1123_label(&composed) {
            Dns1123Label(composed)
        } else {
            // Total fallback (worker index past the reserved budget):
            // deterministic, still collision-salted by the full
            // composed raw.
            Dns1123Label::sanitize(&composed, 0, "worker")
        }
    }

    /// The validated label text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Dns1123Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Dns1123Label {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // r[verify store.materialize.worker-identity]
    /// m243: the COMPOSED `{instance}-w{n}` wire value is a DNS-1123
    /// label for every raw × worker — the value the scheduler actually
    /// validates. RED (pre-fix, captured against the old two-copy
    /// shape in rio-store): `composed identity must be a DNS-1123
    /// label, got 66 chars: "aaa…aaa-w0"` — a 63-char valid hostname
    /// passed the sanitizer unchanged and the suffix broke the bound.
    #[test]
    fn composed_worker_identity_is_a_dns1123_label() {
        let raw = "a".repeat(63);
        let base = Dns1123Label::sanitize(&raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
        assert!(
            base.as_str().len() <= DNS1123_MAX_LEN - WORKER_SUFFIX_RESERVED,
            "sanitize must budget the reserved suffix inside the bound"
        );
        for worker in [0usize, 7, 999] {
            let composed = base.with_worker(worker);
            assert!(
                is_dns1123_label(composed.as_str()),
                "composed identity must be a DNS-1123 label, got {} chars: {:?}",
                composed.as_str().len(),
                composed.as_str()
            );
        }
        // Past the reserved budget the composition stays total (and
        // deterministic) instead of producing an invalid label.
        let big = base.with_worker(123_456);
        assert!(is_dns1123_label(big.as_str()));
        assert_eq!(
            big,
            base.with_worker(123_456),
            "total fallback is deterministic"
        );
    }

    /// merged_bug_158 invariants survive the budget rework: identities
    /// that USED to fold to one label stay distinct (deterministic FNV
    /// salt over the raw), valid in-budget raws pass through unsalted,
    /// and the same raw maps to the same identity across calls.
    #[test]
    fn sanitize_keeps_injectivity_and_determinism() {
        let s = |raw: &str| Dns1123Label::sanitize(raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
        let a = s("Host_A");
        let b = s("host-a");
        let c = s("host.a");
        assert_eq!(
            b.as_str(),
            "host-a",
            "valid in-budget raw passes through unsalted"
        );
        assert_ne!(a, b, "Host_A no longer folds onto host-a");
        assert_ne!(c, b, "host.a no longer folds onto host-a");
        assert_ne!(a, c, "distinct raws get distinct salts");
        assert_eq!(a, s("Host_A"), "deterministic across restarts");
        // Empty/garbage raw → the salted fallback stem.
        let e = s("");
        assert!(e.as_str().starts_with("rio-store-dev-"), "got {e}");
        assert!(is_dns1123_label(e.as_str()));
    }

    /// The deploy-boundary identity shift (recorded trade): raws of
    /// 59–63 valid chars used to pass through unchanged and now
    /// truncate+salt to fit the worker budget. Their BASE identity
    /// changes once at rollout; the scheduler's establishment sweep
    /// absorbs the orphaned claims of the old identity.
    #[test]
    fn long_valid_raws_now_truncate_and_salt() {
        let raw = "b".repeat(61);
        let label = Dns1123Label::sanitize(&raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
        assert!(label.as_str().len() <= DNS1123_MAX_LEN - WORKER_SUFFIX_RESERVED);
        assert_ne!(
            label.as_str(),
            raw.as_str(),
            "must be altered to fit the budget"
        );
        assert!(label.as_str().ends_with(&format!("-{:08x}", {
            // the salt is over the RAW at full 32-bit width, pinned
            // here so a refactor that salts the truncation instead —
            // or quietly narrows the salt again (merged_bug_127) —
            // breaks loudly
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in raw.bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h & 0xffff_ffff
        })));
    }

    /// merged_bug_127 surface 1: pairwise distinctness over the
    /// shared-prefix population the module exists to serve (long Helm
    /// release pod names whose distinguishing ordinal sits past the
    /// truncation point). The pinned pair was brute-forced to collide
    /// in the OLD 16-bit salt (both `0x0d3e`) while their 32-bit salts
    /// differ. RED (pre-fix): both raws produced the IDENTICAL label
    /// `aaa…a-0d3e` — two replicas deterministically merged into one
    /// scheduler identity on every restart, forever, with no
    /// randomness to escape.
    #[test]
    fn shared_prefix_pair_stays_distinct() {
        let stem = "a".repeat(60);
        let raw_a = format!("{stem}498");
        let raw_b = format!("{stem}784");
        let s = |raw: &str| Dns1123Label::sanitize(raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
        let a = s(&raw_a);
        let b = s(&raw_b);
        assert_ne!(
            a, b,
            "shared-prefix raws with colliding 16-bit salts must stay \
             distinct under the 32-bit salt"
        );
        assert!(is_dns1123_label(a.as_str()));
        assert!(is_dns1123_label(b.as_str()));
        assert_eq!(a, s(&raw_a), "deterministic across restarts");
        // And the worker compositions stay distinct too — the value
        // the scheduler actually keys on.
        assert_ne!(a.with_worker(0), b.with_worker(0));
    }

    /// merged_bug_127 surface 2: the salted and pass-through output
    /// spaces are structurally disjoint. A valid in-budget raw wearing
    /// the salted arm's own shape (`{stem}-{8hex}`) must NOT pass
    /// through verbatim — pre-fix it did, so a literal pod name could
    /// occupy the exact identity a different replica's altered raw
    /// salts onto. RED (pre-fix): `sanitize(x) == x` for
    /// `x = "rio-store-deadbeef"`-class raws.
    #[test]
    fn salted_shape_never_passes_through() {
        let raw = "rio-store-0badcafe"; // valid label, salt-shaped tail
        let s = |raw: &str| Dns1123Label::sanitize(raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
        let label = s(raw);
        assert_ne!(
            label.as_str(),
            raw,
            "salt-shaped raws must be re-salted, not passed through"
        );
        assert!(
            Dns1123Label::is_salt_shaped(label.as_str()),
            "re-salted output stays in the salted namespace: {label}"
        );
        assert_eq!(label, s(raw), "deterministic across restarts");
        // Non-salt-shaped tails keep passing through untouched: 5-char
        // k8s ReplicaSet-style suffixes and short hex don't match.
        for ok in ["rio-store-7f3a", "rio-store-abc12", "rio-store-w0"] {
            assert_eq!(
                s(ok).as_str(),
                ok,
                "non-salt-shaped raw {ok} passes through"
            );
        }
    }

    proptest! {
        // r[verify store.materialize.worker-identity]
        /// Totality over arbitrary raws × worker indices: every
        /// composed identity is a valid DNS-1123 label, and equal
        /// (raw, n) inputs always compose equal identities.
        #[test]
        fn composed_identity_total_over_raws_and_workers(
            raw in ".{0,100}",
            n in 0usize..2048,
        ) {
            let base = Dns1123Label::sanitize(&raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
            prop_assert!(base.as_str().len() <= DNS1123_MAX_LEN - WORKER_SUFFIX_RESERVED);
            let composed = base.with_worker(n);
            prop_assert!(
                is_dns1123_label(composed.as_str()),
                "invalid composed label {:?} from raw {:?}",
                composed.as_str(),
                raw
            );
            // Determinism on the non-empty arm (the empty-raw fallback
            // is deliberately random per process).
            if !base.as_str().starts_with("rio-store-dev-") {
                let again = Dns1123Label::sanitize(&raw, WORKER_SUFFIX_RESERVED, "rio-store-dev")
                    .with_worker(n);
                prop_assert_eq!(composed, again);
            }
        }

        // r[verify store.materialize.worker-identity]
        /// W11-BO (bug_005, R28 secondary-input axis): the seal covers
        /// BOTH input axes — `fallback_stem` is caller-supplied too,
        /// and the empty-raw arm pre-fix embedded it VERBATIM into the
        /// validated newtype (no alphabet/case/budget enforcement;
        /// every existing test held the stem at a short valid
        /// constant, so the hole was untriggered, not unwritable).
        /// Population: the full two-axis input domain — arbitrary raw
        /// × adversarial stems (uppercase, underscores, overlong,
        /// empty). Pre-fix red: `invalid label "BAD_STEM-xxxxxxxx"
        /// from raw "" stem "BAD_STEM"`.
        #[test]
        fn sanitize_total_over_both_input_axes(
            raw in ".{0,100}",
            stem in ".{0,100}",
        ) {
            let label = Dns1123Label::sanitize(&raw, WORKER_SUFFIX_RESERVED, &stem);
            prop_assert!(
                is_dns1123_label(label.as_str()),
                "invalid label {:?} from raw {:?} stem {:?}",
                label.as_str(),
                raw,
                stem
            );
            prop_assert!(
                label.as_str().len() <= DNS1123_MAX_LEN - WORKER_SUFFIX_RESERVED,
                "budget violated: {:?}",
                label.as_str()
            );
        }

        /// merged_bug_127 namespace-disjointness LAW over arbitrary
        /// raws: an output either equals its raw (pass-through, never
        /// salt-shaped) or ends in the 8-hex salt segment (altered or
        /// fallback) — there is no third shape, so the two namespaces
        /// partition the output space and cross-arm collisions are
        /// unrepresentable.
        #[test]
        fn output_namespaces_are_disjoint(raw in ".{0,100}") {
            let label = Dns1123Label::sanitize(&raw, WORKER_SUFFIX_RESERVED, "rio-store-dev");
            if label.as_str() == raw {
                prop_assert!(
                    !Dns1123Label::is_salt_shaped(label.as_str()),
                    "pass-through output wears the salted shape: {:?}",
                    label.as_str()
                );
            } else {
                prop_assert!(
                    Dns1123Label::is_salt_shaped(label.as_str()),
                    "altered output missing the salt segment: {:?} from {:?}",
                    label.as_str(),
                    raw
                );
            }
        }
    }
}
