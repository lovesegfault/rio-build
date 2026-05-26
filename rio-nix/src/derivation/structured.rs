//! `__structuredAttrs`-aware attribute lookup.
//!
//! When a derivation sets `__structuredAttrs = true`, Nix's
//! `derivationStrict` serializes the user attributes into `env["__json"]`
//! ONLY — they do NOT appear as separate env keys. Any consumer that
//! reads behavioral attributes out of a derivation's environment
//! (`passAsFile`, `exportReferencesGraph`, `impureEnvVars`,
//! `preferLocalBuild`, `requiredSystemFeatures`, `outputChecks`,
//! `unsafeDiscardReferences`, …) therefore has to look in the JSON blob
//! first and fall back to the flat env, mirroring Nix's
//! `ParsedDerivation::get{String,Bool,StringList}Attr`.
//!
//! This type is the single shared implementation of that lookup. It is
//! used by rio-gateway's scheduling-hint extraction (`translate.rs`) and
//! by rio-builder's native-executor glue (request-side env
//! materialization and result-side output checks). Keeping one parser
//! prevents the two sides from drifting on JSON-vs-env precedence.
//!
//! Policy (clamping tenant-controlled values, rejecting oversized lists)
//! deliberately does NOT live here — callers own their own threat
//! models. See rio-gateway's `ClampedAttrs` extension for the gateway's
//! ADR-023 clamps.

use std::collections::BTreeMap;

/// `__structuredAttrs`-aware env lookup over a derivation environment.
///
/// JSON (`env["__json"]`) is checked first, then the raw env, matching
/// upstream semantics. Malformed `__json` degrades to env-only lookup
/// (no error): a derivation with a hostile or broken `__json` should
/// behave like one without structured attrs rather than failing parse
/// at every consumer.
pub struct StructuredEnv<'a> {
    env: &'a BTreeMap<String, String>,
    json: Option<serde_json::Value>,
}

impl<'a> StructuredEnv<'a> {
    /// Build the lookup view. Parses `env["__json"]` once.
    pub fn new(env: &'a BTreeMap<String, String>) -> Self {
        let json = env.get("__json").and_then(|s| serde_json::from_str(s).ok());
        Self { env, json }
    }

    /// Whether the derivation opted into structured attributes
    /// (`__structuredAttrs = true` at eval time).
    ///
    /// Detection matches Nix's `ParsedDerivation::hasStructuredAttrs()`:
    /// an instantiated structured-attrs derivation is one whose env
    /// carries the `__json` blob. The eval-time `__structuredAttrs`
    /// attribute is consumed by `derivationStrict` and does NOT appear
    /// as an env var in the `.drv` — keying on it mis-detects every
    /// real structured-attrs derivation as a flat-env one (caught by
    /// the vm-differential harness: the builder then gets no
    /// `.attrs.sh` and an injected `out=`, and structured builds fail).
    ///
    /// Presence of the key is deliberately checked on the raw env (not
    /// `self.json`): a present-but-malformed `__json` must still route
    /// the derivation down the structured path so materialization can
    /// fail loudly instead of silently building with a flat env.
    // r[impl builder.exec.structured-attrs]
    pub fn is_structured_attrs(&self) -> bool {
        self.env.contains_key("__json")
    }

    /// The parsed `__json` blob, when present and well-formed.
    ///
    /// Request-side materialization (`.attrs.json` / `.attrs.sh`) needs
    /// the whole object, not individual attrs — that is the only reason
    /// this accessor exists; prefer the typed lookups below.
    pub fn json(&self) -> Option<&serde_json::Value> {
        self.json.as_ref()
    }

    /// String attribute: JSON first, then raw env.
    pub fn string(&self, key: &str) -> Option<String> {
        self.json
            .as_ref()
            .and_then(|j| j.get(key)?.as_str().map(String::from))
            .or_else(|| self.env.get(key).cloned())
    }

    /// Boolean attribute: JSON first (real JSON bool), then raw env
    /// (Nix encodes eval-time bools as `"1"` / `""`; accept `"true"`
    /// for robustness).
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.json
            .as_ref()
            .and_then(|j| j.get(key)?.as_bool())
            .or_else(|| self.env.get(key).map(|v| v == "1" || v == "true"))
    }

    /// String-list attribute: JSON array of strings first, then the
    /// whitespace-split raw env value (Nix's flat encoding for list
    /// attrs like `passAsFile` / `impureEnvVars` /
    /// `requiredSystemFeatures`).
    pub fn strings(&self, key: &str) -> Option<Vec<String>> {
        self.json
            .as_ref()
            .and_then(|j| {
                Some(
                    j.get(key)?
                        .as_array()?
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                )
            })
            .or_else(|| {
                self.env
                    .get(key)
                    .map(|s| s.split_whitespace().map(String::from).collect())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn flat_env_lookup() {
        let env = env_of(&[
            ("preferLocalBuild", "1"),
            ("requiredSystemFeatures", "kvm big-parallel"),
            ("pname", "hello"),
        ]);
        let s = StructuredEnv::new(&env);
        assert!(!s.is_structured_attrs());
        assert_eq!(s.bool("preferLocalBuild"), Some(true));
        assert_eq!(
            s.strings("requiredSystemFeatures"),
            Some(vec!["kvm".into(), "big-parallel".into()])
        );
        assert_eq!(s.string("pname"), Some("hello".into()));
        assert_eq!(s.string("missing"), None);
        assert_eq!(s.bool("missing"), None);
        assert_eq!(s.strings("missing"), None);
    }

    #[test]
    fn json_takes_precedence_over_env() {
        let env = env_of(&[
            ("__structuredAttrs", "1"),
            (
                "__json",
                r#"{"pname":"from-json","preferLocalBuild":true,"requiredSystemFeatures":["kvm"]}"#,
            ),
            ("pname", "from-env"),
        ]);
        let s = StructuredEnv::new(&env);
        assert!(s.is_structured_attrs());
        assert_eq!(s.string("pname"), Some("from-json".into()));
        assert_eq!(s.bool("preferLocalBuild"), Some(true));
        assert_eq!(
            s.strings("requiredSystemFeatures"),
            Some(vec!["kvm".into()])
        );
        assert!(s.json().is_some());
    }

    #[test]
    fn malformed_json_degrades_to_env() {
        let env = env_of(&[("__json", "{not json"), ("pname", "fallback")]);
        let s = StructuredEnv::new(&env);
        assert!(s.json().is_none());
        assert_eq!(s.string("pname"), Some("fallback".into()));
        // The key is present, so the derivation still routes down the
        // structured path — materialization then fails loudly on the
        // malformed blob instead of silently building with a flat env.
        assert!(s.is_structured_attrs());
    }

    #[test]
    fn json_presence_alone_means_structured_attrs() {
        // What `nix-instantiate` actually emits for a structured-attrs
        // derivation: the env carries `__json` and NOTHING else marks
        // the opt-in (`__structuredAttrs` is consumed at eval time and
        // never appears as an env var). Caught by the vm-differential
        // harness when detection keyed on the nonexistent marker.
        let env = env_of(&[("__json", r#"{"name":"demo","outputs":["out"]}"#)]);
        assert!(StructuredEnv::new(&env).is_structured_attrs());
    }

    #[test]
    fn marker_without_json_is_not_structured_attrs() {
        // A hand-written env var named `__structuredAttrs` (no `__json`)
        // is just an ordinary attribute of a flat-env derivation; only
        // the presence of the serialized blob switches modes.
        let env = env_of(&[("__structuredAttrs", "1"), ("pname", "hello")]);
        assert!(!StructuredEnv::new(&env).is_structured_attrs());
    }
}
