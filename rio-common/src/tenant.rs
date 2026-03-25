//! Tenant name normalization.
//!
//! Tenant names cross a proto boundary as raw `String`s (from
//! `SubmitBuildRequest.tenant_name`, the gateway's `authorized_keys`
//! comment field, admin RPC bodies). Each boundary site used to do its
//! own `.trim()` + `.is_empty()` — six sites across four crates as of
//! sprint-1, with the obvious risk that one forgets and " team-a "
//! fails a `WHERE tenant_name = 'team-a'` lookup.
//!
//! [`NormalizedName`] centralizes that: construction trims and rejects
//! empty/whitespace-only input. Once you have a `NormalizedName` you
//! know it's a real, trimmed tenant label. The type makes the
//! distinction visible in signatures — a `&NormalizedName` parameter
//! says "already validated", a `&str` says "raw, untrusted".
//!
//! Empty-name semantics vary by callsite:
//!
//! - `SubmitBuild`, `ListBuilds`, gateway quota: empty = single-tenant
//!   mode (no tenant row to resolve against). Use [`NormalizedName::from_maybe_empty`]
//!   which returns `None` — the `Option` IS the mode flag.
//! - `ResolveTenant`, `TenantQuota`, `CreateTenant`: empty = caller
//!   error (the gateway/admin tool shouldn't be calling at all). Use
//!   [`NormalizedName::new`] which returns `Err(NameError)` → map to
//!   `Status::invalid_argument`.
//!
//! Interior whitespace (`"team a"`) is rejected by BOTH constructors
//! — almost certainly a misconfigured `authorized_keys` comment
//! (space where a dash was intended). Rejecting loudly surfaces the
//! misconfiguration immediately vs. creating an unreachable tenant
//! row that nothing else can reference.

use std::fmt;

/// Normalized tenant name: trimmed, non-empty, no interior whitespace.
///
/// Three constructors for three intents:
/// - [`new`](Self::new) → `Result<Self, NameError>` — the caller
///   wants a name and will propagate the error. For `CreateTenant`,
///   `ResolveTenant`, `TenantQuota`.
/// - [`from_maybe_empty`](Self::from_maybe_empty) → `Option<Self>` —
///   the caller accepts absence (single-tenant mode). Empty/
///   whitespace → `None`. Interior whitespace also → `None` (treated
///   as invalid; the caller logs and falls through to single-tenant).
///   For `SubmitBuild`, `ListBuilds`, gateway SSH-key comment.
/// - `new_unchecked` — test-only (`#[cfg(test)]`). Wraps without
///   validation.
///
/// Storage invariant: if a `NormalizedName` exists, its inner string
/// is trimmed, non-empty, and contains no whitespace. No `.trim()`
/// calls downstream; PG `WHERE tenant_name = $1` always matches.
///
/// `Deref<Target = str>` and `AsRef<str>` so it drops into any
/// `&str`-taking function without `.as_str()`. `From<NormalizedName>
/// for String` via `.into()` for proto-body re-serialization.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NormalizedName(String);

/// Construction-time rejection. The `Display` impl's message is what
/// callers map to `Status::invalid_argument`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    /// Input was empty or contained only whitespace after trimming.
    #[error("name is empty after trimming")]
    Empty,
    /// Input contained whitespace between non-whitespace characters
    /// (e.g., `"team a"` — probably a misconfigured `authorized_keys`
    /// comment with a space where a dash was intended). Carries the
    /// trimmed input for the error message so the operator sees
    /// exactly what was rejected.
    #[error("name contains interior whitespace: {0:?}")]
    InteriorWhitespace(String),
}

impl NormalizedName {
    /// Validating constructor: trims leading/trailing whitespace,
    /// rejects empty and interior-whitespace inputs.
    ///
    /// Interior-whitespace rejection is deliberate: `"team a"` is
    /// almost certainly a typo (space instead of dash) in
    /// `authorized_keys`. Rejecting at construction surfaces the
    /// misconfiguration immediately with a specific error, vs.
    /// creating a tenant row that no other callsite will ever match
    /// (all lookups use the dashed form).
    pub fn new(s: &str) -> Result<Self, NameError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(NameError::Empty);
        }
        if trimmed.chars().any(char::is_whitespace) {
            return Err(NameError::InteriorWhitespace(trimmed.to_string()));
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Trim + invalid-to-None. For sites where an empty/whitespace
    /// name means single-tenant mode (no tenant row, pass through) —
    /// `SubmitBuild`, `ListBuilds`, gateway quota. The returned
    /// `Option` IS the mode flag: `None` → skip tenant resolution
    /// entirely, `Some` → resolve against the `tenants` table.
    ///
    /// Interior whitespace (`"team a"`) also returns `None` here —
    /// collapsing "empty" and "malformed" is intentional: both mean
    /// "no valid tenant to resolve against". Sites that need to
    /// distinguish (and error loudly on malformed) use [`new`] instead.
    ///
    /// [`new`]: Self::new
    pub fn from_maybe_empty(s: &str) -> Option<Self> {
        Self::new(s).ok()
    }

    /// Test-only: wrap without validation. Bypasses the trim +
    /// whitespace checks — use only when a test needs to inject a
    /// known-valid name without round-tripping through `new`.
    ///
    /// Cross-crate tests: use `new("...").unwrap()` instead (test
    /// inputs are known-valid so the unwrap never fires).
    #[cfg(test)]
    pub(crate) fn new_unchecked(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the inner trimmed string. Same as `Deref`/`AsRef` but
    /// explicit for readability at call sites that want it (e.g.
    /// `sqlx::bind(name.as_str())`).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for NormalizedName {
    type Error = NameError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl TryFrom<String> for NormalizedName {
    type Error = NameError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        // Could avoid the realloc when `s` is already trimmed, but
        // tenant names are short (< 64 bytes) and this runs once
        // per-RPC — not worth the branch.
        Self::new(&s)
    }
}

impl fmt::Display for NormalizedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for NormalizedName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for NormalizedName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for NormalizedName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<NormalizedName> for String {
    fn from(n: NormalizedName) -> String {
        n.0
    }
}

impl PartialEq<str> for NormalizedName {
    fn eq(&self, o: &str) -> bool {
        self.0 == o
    }
}

impl PartialEq<&str> for NormalizedName {
    fn eq(&self, o: &&str) -> bool {
        self.0 == *o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn trims_leading_trailing() {
        // Leading/trailing whitespace of any kind (space, tab,
        // newline) is stripped.
        let n = NormalizedName::new("  team-a\t\n").unwrap();
        assert_eq!(n, "team-a");
        assert_eq!(n.as_str(), "team-a");
    }

    #[test]
    fn rejects_empty_and_whitespace() {
        assert_eq!(NormalizedName::new(""), Err(NameError::Empty));
        assert_eq!(NormalizedName::new("   "), Err(NameError::Empty));
        assert_eq!(NormalizedName::new("\t\n"), Err(NameError::Empty));
    }

    #[test]
    fn rejects_interior_whitespace() {
        // Interior space is almost certainly a misconfigured
        // authorized_keys comment ("team a" where "team-a" was
        // intended). Reject loudly so the operator notices.
        assert_eq!(
            NormalizedName::new(" team a "),
            Err(NameError::InteriorWhitespace("team a".into()))
        );
        // Same for tab, newline — any char::is_whitespace.
        assert_eq!(
            NormalizedName::new("team\tb"),
            Err(NameError::InteriorWhitespace("team\tb".into()))
        );
        // Multiple interior spaces.
        assert_eq!(
            NormalizedName::new("a  b  c"),
            Err(NameError::InteriorWhitespace("a  b  c".into()))
        );
        // Error payload is the TRIMMED input (leading/trailing already
        // gone) — the operator sees exactly what the middleware saw.
        assert_eq!(
            NormalizedName::new("  has space  "),
            Err(NameError::InteriorWhitespace("has space".into()))
        );
    }

    #[test]
    fn from_maybe_empty_maps_to_none() {
        // The single-tenant-mode helper: None for invalid, Some for
        // valid. Collapses Empty and InteriorWhitespace — both mean
        // "no valid tenant to resolve against".
        assert!(NormalizedName::from_maybe_empty("").is_none());
        assert!(NormalizedName::from_maybe_empty("   ").is_none());
        assert!(
            NormalizedName::from_maybe_empty("team a").is_none(),
            "interior whitespace also → None (collapsed with empty)"
        );
        let n = NormalizedName::from_maybe_empty(" team-a ").unwrap();
        assert_eq!(n, "team-a");
    }

    #[test]
    fn trait_surface() {
        let n = NormalizedName::new("surface").unwrap();
        // Display
        assert_eq!(n.to_string(), "surface");
        // Deref<Target = str>
        let s: &str = &n;
        assert_eq!(s, "surface");
        // AsRef<str>
        assert_eq!(<NormalizedName as AsRef<str>>::as_ref(&n), "surface");
        // Borrow<str> — lets HashMap<NormalizedName, _>::get(&str) work.
        let b: &str = std::borrow::Borrow::borrow(&n);
        assert_eq!(b, "surface");
        // Into<String>
        let owned: String = n.into();
        assert_eq!(owned, "surface");
    }

    #[test]
    fn new_unchecked_bypasses_validation() {
        // Test-only escape hatch: wraps without checking. Lets tests
        // inject known-valid names without the full round-trip.
        let n = NormalizedName::new_unchecked("team-a");
        assert_eq!(n.as_str(), "team-a");
        // Bypasses validation entirely — even invalid input wraps.
        // (Don't do this outside tests.)
        let bad = NormalizedName::new_unchecked("has space");
        assert_eq!(bad.as_str(), "has space");
    }

    #[test]
    fn name_error_display() {
        // These messages surface in `Status::invalid_argument` bodies
        // — operator-visible. Assert the exact text.
        assert_eq!(NameError::Empty.to_string(), "name is empty after trimming");
        assert_eq!(
            NameError::InteriorWhitespace("team a".into()).to_string(),
            r#"name contains interior whitespace: "team a""#
        );
    }

    proptest! {
        /// The storage invariant: any `NormalizedName` that exists has
        /// an inner string that is trimmed, non-empty, and whitespace-
        /// free. Proptest generates arbitrary strings (including
        /// unicode, control chars) — we only assert on the Ok branch.
        #[test]
        fn invariant_trimmed_nonempty_no_whitespace(s in ".*") {
            if let Ok(n) = NormalizedName::new(&s) {
                // Trimmed: no leading/trailing whitespace.
                prop_assert_eq!(n.as_str(), n.as_str().trim());
                // Non-empty.
                prop_assert!(!n.as_str().is_empty());
                // No interior whitespace.
                prop_assert!(!n.as_str().chars().any(char::is_whitespace));
                // Matches the trimmed input (construction only trims,
                // never rewrites).
                prop_assert_eq!(n.as_str(), s.trim());
            }
        }

        /// `from_maybe_empty` and `new().ok()` are identical — the
        /// Option constructor is exactly the Result constructor with
        /// the error discarded. Proves no divergence crept in.
        #[test]
        fn from_maybe_empty_matches_new_ok(s in ".*") {
            prop_assert_eq!(
                NormalizedName::from_maybe_empty(&s),
                NormalizedName::new(&s).ok()
            );
        }
    }
}
