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
//!   `TryFrom<&str>` which returns `Err(EmptyTenantName)` → map to
//!   `Status::invalid_argument`.

use std::fmt;

/// Normalized tenant name: trimmed, guaranteed non-empty.
///
/// Constructed via `TryFrom<&str>` (error on empty) or
/// [`from_maybe_empty`](Self::from_maybe_empty) (`None` on empty —
/// for sites where empty means single-tenant mode).
///
/// `Deref<Target = str>` and `AsRef<str>` so it drops into any
/// `&str`-taking function without `.as_str()`. `From<NormalizedName>
/// for String` via `.into()` for proto-body re-serialization.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NormalizedName(String);

/// Construction-time rejection: the input was empty or contained only
/// whitespace. The `Display` impl's message is what callers map to
/// `Status::invalid_argument`.
#[derive(Debug, thiserror::Error)]
#[error("tenant name is empty or whitespace-only")]
pub struct EmptyTenantName;

impl NormalizedName {
    /// Trim + empty-to-None. For sites where an empty/whitespace name
    /// means single-tenant mode (no tenant row, pass through) —
    /// `SubmitBuild`, `ListBuilds`, gateway quota. The returned
    /// `Option` IS the mode flag: `None` → skip tenant resolution
    /// entirely, `Some` → resolve against the `tenants` table.
    ///
    /// Sites where empty is a caller error use `TryFrom<&str>` instead.
    pub fn from_maybe_empty(s: &str) -> Option<Self> {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(Self(t.to_string()))
        }
    }

    /// Borrow the inner trimmed string. Same as `Deref`/`AsRef` but
    /// explicit for readability at call sites that want it (e.g.
    /// `sqlx::bind(name.as_str())`).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner String. Prefer `.into()` (`String:
    /// From<NormalizedName>`) unless you're in a generic context
    /// where inference doesn't pick it up.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<&str> for NormalizedName {
    type Error = EmptyTenantName;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::from_maybe_empty(s).ok_or(EmptyTenantName)
    }
}

impl TryFrom<String> for NormalizedName {
    type Error = EmptyTenantName;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        // Could avoid the realloc when `s` is already trimmed, but
        // tenant names are short (< 64 bytes) and this runs once
        // per-RPC — not worth the branch.
        Self::try_from(s.as_str())
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

    #[test]
    fn trims_leading_trailing() {
        // Leading/trailing whitespace of any kind (space, tab,
        // newline) is stripped. Mixed interior whitespace is
        // preserved — normalization is boundary-only.
        let n = NormalizedName::try_from("  team-a\t\n").unwrap();
        assert_eq!(n, "team-a");
        assert_eq!(n.as_str(), "team-a");
        // Interior space untouched. Probably a bad tenant name but
        // that's a policy question for CreateTenant, not this type.
        let n = NormalizedName::try_from(" team a ").unwrap();
        assert_eq!(n, "team a");
    }

    #[test]
    fn rejects_empty_and_whitespace() {
        assert!(NormalizedName::try_from("").is_err());
        assert!(NormalizedName::try_from("   ").is_err());
        assert!(NormalizedName::try_from("\t\n").is_err());
    }

    #[test]
    fn from_maybe_empty_maps_to_none() {
        // The single-tenant-mode helper: None for empty, Some for
        // anything else. Same trimming as try_from, just a different
        // empty-case return.
        assert!(NormalizedName::from_maybe_empty("").is_none());
        assert!(NormalizedName::from_maybe_empty("   ").is_none());
        let n = NormalizedName::from_maybe_empty(" team-a ").unwrap();
        assert_eq!(n, "team-a");
    }

    #[test]
    fn trait_surface() {
        let n = NormalizedName::try_from("surface").unwrap();
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
        let owned: String = n.clone().into();
        assert_eq!(owned, "surface");
        // into_inner
        assert_eq!(n.into_inner(), "surface");
    }
}
