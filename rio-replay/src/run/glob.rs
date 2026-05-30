//! Minimal glob matching for job-name filters (`*` = any run of
//! characters, `?` = exactly one character).
//!
//! Deliberately tiny instead of pulling `globset`: campaign filters only
//! need `*`/`?` over job names like
//! `python3Packages.requests.x86_64-linux`.

/// Returns true when `name` matches `pattern` (`*` matches any substring
/// including the empty one, `?` matches exactly one character, everything
/// else is literal).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], n) || (!n.is_empty() && inner(p, &n[1..])),
            (Some(b'?'), Some(_)) => inner(&p[1..], &n[1..]),
            (Some(c), Some(d)) if c == d => inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything.x86_64-linux"));
        assert!(glob_match(
            "python3Packages.*",
            "python3Packages.requests.x86_64-linux"
        ));
        assert!(glob_match("*.x86_64-linux", "hello.x86_64-linux"));
        assert!(glob_match("hello.?86_64-linux", "hello.x86_64-linux"));
        assert!(!glob_match(
            "python3Packages.*",
            "haskellPackages.lens.x86_64-linux"
        ));
        assert!(!glob_match("hello", "hello.x86_64-linux"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }
}
