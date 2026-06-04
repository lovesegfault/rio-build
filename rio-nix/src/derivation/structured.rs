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

use super::typed;

/// A typed structured-attrs read failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StructuredAttrError {
    /// `__json` is present but does not parse — the derivation claims
    /// structured attrs and the claim is unreadable. Fail-closed
    /// consumers MUST NOT degrade this to "attr absent" (the pre-fix
    /// readers did, which let an unparseable blob neutralize every
    /// behavioral attribute at once).
    #[error("structured attrs (__json) do not parse: {error}")]
    MalformedJson { error: String },
    /// The attribute exists in the parsed blob with the wrong type.
    /// Mirrors the oracle's `e.addTrace(... "while parsing attribute
    /// \"%s\"")` framing around its `get*` throw.
    #[error("while parsing attribute \"{key}\": {source}")]
    WrongType {
        key: String,
        source: typed::TypedError,
    },
}

/// The scheduling-hint attributes sanctioned for LENIENT reads — the
/// compile-time bound on every non-fail-closed structured-attrs
/// access in the workspace.
///
/// These feed pod sizing and placement only (never sandbox shape,
/// output policy, or env semantics), and the trusted plane treats them
/// as untrusted hints with their own clamps; a wrong-typed value
/// degrading to "absent" costs sizing accuracy, not correctness. This
/// is a documented divergence from the oracle, which type-checks these
/// at build time — rio reads them at SUBMISSION time, where failing
/// the build for a bad `pname` would reject derivations the oracle
/// happily builds. Widening this enum is the only way to add a lenient
/// read, which makes every new exemption a reviewable diff line.
// r[impl builder.exec.structured-attrs-typed]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizingHint {
    /// `pname` — build_samples key.
    Pname,
    /// `name` — `pname` fallback for raw `derivation {}` calls.
    Name,
    /// `version` — sizing-history secondary key.
    Version,
    /// `enableParallelBuilding` — core-count hint.
    EnableParallelBuilding,
    /// `enableParallelChecking` — core-count hint.
    EnableParallelChecking,
    /// `preferLocalBuild` — placement hint.
    PreferLocalBuild,
    /// `requiredSystemFeatures` — placement constraint set.
    RequiredSystemFeatures,
}

impl SizingHint {
    /// The attribute name this hint reads.
    pub fn key(self) -> &'static str {
        match self {
            SizingHint::Pname => "pname",
            SizingHint::Name => "name",
            SizingHint::Version => "version",
            SizingHint::EnableParallelBuilding => "enableParallelBuilding",
            SizingHint::EnableParallelChecking => "enableParallelChecking",
            SizingHint::PreferLocalBuild => "preferLocalBuild",
            SizingHint::RequiredSystemFeatures => "requiredSystemFeatures",
        }
    }
}

/// `__structuredAttrs`-aware env lookup over a derivation environment.
///
/// JSON (`env["__json"]`) is checked first, then the raw env, matching
/// upstream semantics. Two access tiers:
///
/// - the `*_attr` accessors are FAIL-CLOSED (oracle
///   `derivation-options.cc` getters): when the derivation is
///   structured, the blob must parse and the attribute must have the
///   exact type — there is no env fallback and no coercion;
/// - the `lenient_*` accessors keep the historical degrade-to-absent
///   behavior and are bounded to the [`SizingHint`] tier at compile
///   time.
pub struct StructuredEnv<'a> {
    env: &'a BTreeMap<String, String>,
    /// `Ok(None)`: no `__json`. `Ok(Some(v))`: parsed blob.
    /// `Err(msg)`: `__json` present but unparseable.
    json: Result<Option<serde_json::Value>, String>,
}

impl<'a> StructuredEnv<'a> {
    /// Build the lookup view. Parses `env["__json"]` once.
    pub fn new(env: &'a BTreeMap<String, String>) -> Self {
        let json = match env.get("__json") {
            None => Ok(None),
            Some(s) => match serde_json::from_str(s) {
                Ok(v) => Ok(Some(v)),
                Err(e) => Err(e.to_string()),
            },
        };
        Self { env, json }
    }

    /// The `__json` parse failure, when the blob is present but
    /// malformed. `None` means "no blob" or "parsed fine" — pair with
    /// [`Self::is_structured_attrs`] to distinguish.
    pub fn json_malformed(&self) -> Option<&str> {
        self.json.as_ref().err().map(String::as_str)
    }

    /// Fail-closed boolean attribute (oracle `getBoolAttr`,
    /// derivation-options.cc:37-53): structured → the parsed blob's
    /// value must be a JSON boolean (absent → `None`, malformed blob or
    /// wrong type → `Err`); flat → env `"1"` is `true`, anything else
    /// `false` (the oracle's exact env comparison).
    // r[impl builder.exec.structured-attrs-typed]
    pub fn bool_attr(&self, key: &str) -> Result<Option<bool>, StructuredAttrError> {
        match self.json_value(key)? {
            Some(v) => {
                typed::boolean(v)
                    .map(Some)
                    .map_err(|source| StructuredAttrError::WrongType {
                        key: key.to_string(),
                        source,
                    })
            }
            None if self.is_structured_attrs() => Ok(None),
            None => Ok(self.env.get(key).map(|v| v == "1")),
        }
    }

    /// Fail-closed string attribute (oracle `getStringAttr`,
    /// derivation-options.cc:20-35).
    // r[impl builder.exec.structured-attrs-typed]
    pub fn string_attr(&self, key: &str) -> Result<Option<String>, StructuredAttrError> {
        match self.json_value(key)? {
            Some(v) => typed::string(v)
                .map(|s| Some(s.to_string()))
                .map_err(|source| StructuredAttrError::WrongType {
                    key: key.to_string(),
                    source,
                }),
            None if self.is_structured_attrs() => Ok(None),
            None => Ok(self.env.get(key).cloned()),
        }
    }

    /// Fail-closed string-list attribute (oracle `getStringSetAttr`,
    /// derivation-options.cc:55-75, modulo set-ification — list order
    /// is preserved here because `passAsFile` consumers are
    /// order-sensitive): structured → every element must be a string
    /// (no dropping); flat → whitespace-split env value.
    // r[impl builder.exec.structured-attrs-typed]
    pub fn string_list_attr(&self, key: &str) -> Result<Option<Vec<String>>, StructuredAttrError> {
        match self.json_value(key)? {
            Some(v) => {
                typed::string_list(v)
                    .map(Some)
                    .map_err(|source| StructuredAttrError::WrongType {
                        key: key.to_string(),
                        source,
                    })
            }
            None if self.is_structured_attrs() => Ok(None),
            None => Ok(self
                .env
                .get(key)
                .map(|s| s.split_whitespace().map(String::from).collect())),
        }
    }

    /// Lenient string read, bounded to the [`SizingHint`] tier:
    /// JSON first, then raw env; wrong types and malformed `__json`
    /// degrade to the fallback chain (a bad hint costs sizing
    /// accuracy, never correctness).
    pub fn lenient_string(&self, hint: SizingHint) -> Option<String> {
        let key = hint.key();
        self.json()
            .and_then(|j| j.get(key)?.as_str().map(String::from))
            .or_else(|| self.env.get(key).cloned())
    }

    /// Lenient boolean read, bounded to the [`SizingHint`] tier. The
    /// env arm accepts `"1"` and `"true"` (Nix encodes eval-time bools
    /// as `"1"`/`""`; `"true"` kept for robustness).
    pub fn lenient_bool(&self, hint: SizingHint) -> Option<bool> {
        let key = hint.key();
        self.json()
            .and_then(|j| j.get(key)?.as_bool())
            .or_else(|| self.env.get(key).map(|v| v == "1" || v == "true"))
    }

    /// Lenient string-list read, bounded to the [`SizingHint`] tier
    /// (non-string JSON elements dropped; env arm whitespace-split).
    pub fn lenient_strings(&self, hint: SizingHint) -> Option<Vec<String>> {
        let key = hint.key();
        self.json()
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

    /// The parsed blob's value for `key`, with malformed `__json`
    /// surfaced as the error it is.
    fn json_value(&self, key: &str) -> Result<Option<&serde_json::Value>, StructuredAttrError> {
        match &self.json {
            Ok(json) => Ok(json.as_ref().and_then(|j| j.get(key))),
            Err(error) => Err(StructuredAttrError::MalformedJson {
                error: error.clone(),
            }),
        }
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
        self.json.as_ref().ok().and_then(Option::as_ref)
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
        assert_eq!(s.lenient_bool(SizingHint::PreferLocalBuild), Some(true));
        assert_eq!(
            s.lenient_strings(SizingHint::RequiredSystemFeatures),
            Some(vec!["kvm".into(), "big-parallel".into()])
        );
        assert_eq!(s.lenient_string(SizingHint::Pname), Some("hello".into()));
        // Absent hints are None on every accessor shape.
        assert_eq!(s.lenient_string(SizingHint::Version), None);
        assert_eq!(s.lenient_bool(SizingHint::EnableParallelBuilding), None);
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
        assert_eq!(
            s.lenient_string(SizingHint::Pname),
            Some("from-json".into())
        );
        assert_eq!(s.lenient_bool(SizingHint::PreferLocalBuild), Some(true));
        assert_eq!(
            s.lenient_strings(SizingHint::RequiredSystemFeatures),
            Some(vec!["kvm".into()])
        );
        assert!(s.json().is_some());
    }

    #[test]
    fn malformed_json_degrades_to_env() {
        let env = env_of(&[("__json", "{not json"), ("pname", "fallback")]);
        let s = StructuredEnv::new(&env);
        assert!(s.json().is_none());
        assert_eq!(s.lenient_string(SizingHint::Pname), Some("fallback".into()));
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

    /// The fail-closed tier: structured derivations get oracle getter
    /// semantics — exact types, no env fallback, malformed blob is an
    /// error, wrong type is an error.
    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn typed_attrs_fail_closed_on_the_json_path() {
        let env = env_of(&[
            (
                "__json",
                r#"{"goodBool":true,"badBool":"true","goodList":["a","b"],"badList":["a",7],"goodStr":"s","badStr":1}"#,
            ),
            // Hostile twin in the flat env: must NOT be consulted for a
            // structured derivation.
            ("absentKey", "1"),
        ]);
        let s = StructuredEnv::new(&env);

        assert_eq!(s.bool_attr("goodBool").unwrap(), Some(true));
        assert_eq!(
            s.string_list_attr("goodList").unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(s.string_attr("goodStr").unwrap(), Some("s".to_string()));

        // Wrong type: error naming the attribute (oracle addTrace
        // framing), never a coercion, never an env fallback.
        let err = s.bool_attr("badBool").unwrap_err();
        assert!(
            err.to_string()
                .starts_with(r#"while parsing attribute "badBool":"#),
            "{err}"
        );
        assert!(s.string_list_attr("badList").is_err(), "no element drops");
        assert!(s.string_attr("badStr").is_err());

        // Absent in the blob of a structured drv: None — the flat env
        // twin is ignored (the oracle's parsed branch never reads env).
        assert_eq!(s.bool_attr("absentKey").unwrap(), None);
    }

    /// Malformed `__json` is an error for every fail-closed accessor —
    /// not "attr absent" (the lenient tier's degrade).
    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn typed_attrs_error_on_malformed_json() {
        let env = env_of(&[("__json", "{not json"), ("k", "1")]);
        let s = StructuredEnv::new(&env);
        assert!(s.json_malformed().is_some());
        assert!(matches!(
            s.bool_attr("k"),
            Err(StructuredAttrError::MalformedJson { .. })
        ));
        assert!(matches!(
            s.string_attr("k"),
            Err(StructuredAttrError::MalformedJson { .. })
        ));
        assert!(matches!(
            s.string_list_attr("k"),
            Err(StructuredAttrError::MalformedJson { .. })
        ));
        // The lenient tier still degrades (hint reads must not fail a
        // submission over a blob the build itself will reject later).
        assert_eq!(s.lenient_string(SizingHint::Pname), None);
    }

    /// Flat-env fallbacks of the fail-closed tier are infallible and
    /// match the oracle's env arm exactly (`*i == "1"` for bools —
    /// stricter than the lenient tier's `"true"` acceptance).
    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn typed_attrs_flat_env_fallbacks() {
        let env = env_of(&[
            ("flag1", "1"),
            ("flagTrue", "true"),
            ("list", "a b  c"),
            ("s", "v"),
        ]);
        let s = StructuredEnv::new(&env);
        assert_eq!(s.bool_attr("flag1").unwrap(), Some(true));
        assert_eq!(
            s.bool_attr("flagTrue").unwrap(),
            Some(false),
            "oracle env arm is *i == \"1\" only"
        );
        assert_eq!(
            s.string_list_attr("list").unwrap(),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(s.string_attr("s").unwrap(), Some("v".to_string()));
        assert_eq!(s.bool_attr("missing").unwrap(), None);
    }

    /// The lenient accessors are reachable only through the
    /// [`SizingHint`] enum — the compile-time bound on non-fail-closed
    /// reads. This test pins the tier's membership: widening it is a
    /// reviewable enum diff, not a stray string.
    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn lenient_tier_is_enum_bounded() {
        let hints = [
            SizingHint::Pname,
            SizingHint::Name,
            SizingHint::Version,
            SizingHint::EnableParallelBuilding,
            SizingHint::EnableParallelChecking,
            SizingHint::PreferLocalBuild,
            SizingHint::RequiredSystemFeatures,
        ];
        let keys: Vec<&str> = hints.iter().map(|h| h.key()).collect();
        assert_eq!(
            keys,
            vec![
                "pname",
                "name",
                "version",
                "enableParallelBuilding",
                "enableParallelChecking",
                "preferLocalBuild",
                "requiredSystemFeatures",
            ]
        );

        let env = env_of(&[("pname", "hello"), ("preferLocalBuild", "true")]);
        let s = StructuredEnv::new(&env);
        assert_eq!(s.lenient_string(SizingHint::Pname), Some("hello".into()));
        // Lenient bool keeps the historical "true" acceptance.
        assert_eq!(s.lenient_bool(SizingHint::PreferLocalBuild), Some(true));
        assert_eq!(s.lenient_strings(SizingHint::RequiredSystemFeatures), None);
    }
}
