//! rio-parity — nixpkgs-parity campaign engine.
//!
//! This crate hosts the eval-set builder and the campaign engine. The
//! binary entry point is `src/main.rs`; everything else is a library
//! module so it can be unit-tested against recorded fixtures.

pub mod archive;
pub mod cmd;
pub mod evalset;
pub mod hydra;
pub mod nixcache;
pub mod run;
pub mod s3;
pub mod substituter;

/// Canonical public repository URL, embedded in [`user_agent`].
///
/// A constant rather than `env!("CARGO_PKG_REPOSITORY")`: the crate2nix
/// pipeline that builds the release binaries compiles with an empty
/// `CARGO_PKG_REPOSITORY`, and the User-Agent must always carry a
/// reachable project URL so hydra.nixos.org / cache.nixos.org operators
/// can identify this traffic and reach its source.
const REPOSITORY_URL: &str = "https://github.com/lovesegfault/rio-build";

/// Descriptive User-Agent for every engine-originated HTTP request, so
/// hydra.nixos.org and cache.nixos.org operators can tell this traffic
/// apart from anonymous crawlers.
///
/// `contact` is appended when provided so hydra.nixos.org operators can
/// reach whoever is running a campaign.
pub fn user_agent(contact: Option<&str>) -> String {
    let base = format!(
        "rio-parity/{} (+{REPOSITORY_URL})",
        env!("CARGO_PKG_VERSION"),
    );
    match contact {
        Some(c) if !c.is_empty() => format!("{base} (contact: {c})"),
        _ => base,
    }
}

/// Build a reqwest client carrying the politeness `User-Agent` and a
/// request timeout — the shared constructor for every HTTP client in
/// this crate (Hydra, binary cache, tarball download).
///
/// Construction first tries the default TLS configuration (platform
/// root certificates). When that fails because no system CA bundle is
/// available — the hermetic nextest sandbox has none — it falls back to
/// an explicit empty root store, which skips the platform-certificate
/// load entirely. The offline tests only talk plaintext HTTP to
/// loopback servers, so the missing roots are irrelevant there; a
/// production host without a CA bundle still fails clearly, just at the
/// first HTTPS request instead of at client construction.
pub(crate) fn http_client(
    user_agent: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<reqwest::Client> {
    use anyhow::Context as _;

    let builder = || {
        reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(timeout)
    };
    match builder().build() {
        Ok(client) => Ok(client),
        Err(_) => builder()
            .tls_certs_only(std::iter::empty())
            .build()
            .context("build HTTP client"),
    }
}

/// Crate directory for locating committed test fixtures at run time.
///
/// Reads the runtime `CARGO_MANIFEST_DIR` (set by cargo and
/// cargo-nextest for every test process) instead of the compile-time
/// `env!` value: under the crate2nix test pipeline the compile-time
/// path names a per-crate build sandbox that no longer exists when the
/// test binary actually runs, while the runtime value points at the
/// real (or remapped) crate directory containing `tests/fixtures/`.
#[cfg(test)]
pub(crate) fn test_manifest_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
    )
}

/// Clip an HTTP error-response body or subprocess stderr to a short
/// single-line snippet for error messages: whitespace is collapsed and
/// the text is cut at 200 characters. Hydra and cache.nixos.org error
/// pages are HTML and nix stderr can run to many lines, so the snippet
/// keeps the useful part visible without dumping the whole output into
/// every error chain.
pub(crate) fn body_snippet(body: &str) -> String {
    const MAX_CHARS: usize = 200;
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "(empty response body)".to_string();
    }
    let mut chars = collapsed.chars();
    let snippet: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_names_engine_and_repo() {
        let ua = user_agent(None);
        assert!(ua.starts_with("rio-parity/"), "got: {ua}");
        assert!(
            ua.contains("github.com/lovesegfault/rio-build"),
            "got: {ua}"
        );
    }

    #[test]
    fn user_agent_appends_contact() {
        let ua = user_agent(Some("ops@example.com"));
        assert!(ua.ends_with("(contact: ops@example.com)"), "got: {ua}");
        assert_eq!(user_agent(Some("")), user_agent(None));
    }

    #[test]
    fn body_snippet_collapses_and_truncates() {
        assert_eq!(body_snippet("plain error"), "plain error");
        assert_eq!(
            body_snippet("<html>\n  <body>\n    not found\n  </body>\n</html>"),
            "<html> <body> not found </body> </html>"
        );
        assert_eq!(body_snippet("   \n\t  "), "(empty response body)");

        // Truncation counts characters, not bytes, so a multi-byte
        // payload cannot split a code point.
        let long = "ä".repeat(500);
        let snippet = body_snippet(&long);
        assert_eq!(snippet.chars().count(), 201, "200 chars + ellipsis");
        assert!(snippet.ends_with('…'), "got: {snippet}");
    }
}
