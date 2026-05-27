//! rio-parity — nixpkgs-parity campaign engine.
//!
//! This crate hosts the eval-set builder and, in a later change, the
//! campaign runner. The binary entry point is `src/main.rs`; everything
//! else is a library module so it can be unit-tested against recorded
//! fixtures.

pub mod cmd;
pub mod evalset;
pub mod hydra;
pub mod nixcache;
pub mod s3;

/// Descriptive User-Agent for every engine-originated HTTP request, so
/// hydra.nixos.org and cache.nixos.org operators can tell this traffic
/// apart from anonymous crawlers.
///
/// `contact` is appended when provided so hydra.nixos.org operators can
/// reach whoever is running a campaign.
pub fn user_agent(contact: Option<&str>) -> String {
    let base = format!(
        "rio-parity/{} (+{})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY"),
    );
    match contact {
        Some(c) if !c.is_empty() => format!("{base} (contact: {c})"),
        _ => base,
    }
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
