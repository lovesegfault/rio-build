//! rio-parity — nixpkgs-parity campaign engine.
//!
//! This crate hosts the eval-set builder (design §5) and, in a later
//! change, the campaign runner. The binary entry point is
//! `src/main.rs`; everything else is a library module so it can be
//! unit-tested against recorded fixtures.

pub mod cmd;

/// Descriptive User-Agent for every engine-originated HTTP request
/// (hydra.nixos.org politeness requirement, design §11).
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
}
