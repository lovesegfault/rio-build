//! Canonical structured-attrs-aware reads of derivation user attributes.
//!
//! When a derivation sets `__structuredAttrs = true`, Nix's
//! `derivationStrict` serializes the user attrs into the single
//! `env["__json"]` string ONLY — they do NOT appear as separate flat env
//! keys, so a flat `env.get("requiredSystemFeatures")` silently returns
//! `None` for exactly the derivations that declare features. Several
//! components read the same attrs from differently shaped carriers (the
//! gateway from wire-delivered ATerm envs, the recorder from
//! `nix derivation show` JSON, the replay engine from archive-embedded
//! ATerms); this module owns the ONE precedence rule they must share, so
//! two consumers can never again disagree about what one derivation
//! declares.
//!
//! The rule: read the structured payload first; when the payload is
//! absent, or present but lacking the key (or holding it as a non-array),
//! fall back to the flat env key, an ASCII-whitespace-separated name list
//! (Nix tokenizes flat lists on `" \t\n\r"`). Only the payload-ABSENT arm
//! mirrors upstream Nix (`getStringSetAttr` in
//! `src/libstore/derivation-options.cc` at the corpus-pinned producer
//! version, nix 2.34.7 — the legacy `ParsedDerivation::getStringsAttr`
//! name no longer exists there): upstream guards on `if (parsed)`, so a
//! PRESENT payload makes its flat env unreachable — a missing key reads
//! as empty (never the flat value) and a non-array or non-string element
//! throws. The missing-key and non-array fall-throughs, and the
//! per-element skipping of non-strings, are deliberate rio-local
//! tolerance instead: the carriers include hand-assembled envs from
//! untrusted wire ATerms and foreign archives, where `__json` and flat
//! keys CAN co-occur, and reading the flat declaration there beats
//! erroring or silently declaring emptiness. The divergence is
//! adversarial-input-only — `derivationStrict` output never co-occurs a
//! flat decoy with `__json` — and its inflation direction is demotive
//! (more `impureEnvVars` / `requiredSystemFeatures` constrain placement,
//! never widen it). Carriers stay dumb: a [`StructuredAttrsEnv`] adapter
//! only answers "the flat value of key K" and "your structured payload,
//! if any" — all precedence lives in [`string_list_attr`].
//!
//! The `structured-attr-reads` workspace lint (xtask) enforces that no
//! call site reads a [`STRING_LIST_USER_ATTRS`] key straight off an env
//! map with the key written literally or via this module's `*_ATTR`
//! consts; every read routes through this module. The lint is a textual
//! tripwire over those exact shapes — a read through a freshly-bound
//! local alias of a key is outside its alphabet and stays a review
//! concern, not a CI-stopped one.

use std::collections::BTreeMap;

/// The env key `derivationStrict` serializes the `__structuredAttrs`
/// user-attr payload into (a JSON object, as one string value).
pub const STRUCTURED_PAYLOAD_KEY: &str = "__json";

/// `requiredSystemFeatures`: features an executor must offer to host the
/// build (e.g. `kvm`, `big-parallel`). Declaration order is meaningful to
/// no consumer, but the rule preserves it; callers wanting sets sort.
pub const REQUIRED_SYSTEM_FEATURES_ATTR: &str = "requiredSystemFeatures";

/// `impureEnvVars`: caller environment variables a (typically
/// fixed-output) derivation reads at build time.
pub const IMPURE_ENV_VARS_ATTR: &str = "impureEnvVars";

/// THE string-list user attrs subject to the structured-payload-first
/// rule — the class definition as data. The canonical tests iterate it,
/// and the `structured-attr-reads` workspace lint derives its needles
/// from it, so adding an attr here automatically extends both the
/// coverage and the call-site enforcement.
pub const STRING_LIST_USER_ATTRS: [&str; 2] = [REQUIRED_SYSTEM_FEATURES_ATTR, IMPURE_ENV_VARS_ATTR];

/// Narrow view of one derivation's environment, as a carrier presents it.
///
/// Implementations are deliberately dumb data accessors — they know where
/// their carrier keeps the flat env and the structured payload, nothing
/// about precedence. The precedence rule lives in [`string_list_attr`]
/// (and in carrier-side scalar readers built on the same two accessors),
/// so a new carrier cannot re-derive it differently.
pub trait StructuredAttrsEnv {
    /// The flat env value of `key`, if the carrier has one.
    fn flat(&self, key: &str) -> Option<&str>;

    /// The `__structuredAttrs` user-attr payload, if the carrier has one.
    /// `None` both when the derivation is not structured-attrs and when
    /// the payload is malformed — the payload is machine-written by Nix,
    /// so the malformed arm is never expected to fire, and falling back
    /// to the flat env is the shared tolerance every consumer already
    /// had.
    fn structured_payload(&self) -> Option<&serde_json::Value>;
}

/// Read a string-list user attr (`requiredSystemFeatures`,
/// `impureEnvVars`) under THE structured-payload-first precedence rule.
///
/// - In the structured payload the attr is a JSON array of strings;
///   non-string elements are skipped. A payload that lacks the key, or
///   holds it as a non-array, falls through to the flat env.
/// - In the flat env (non-structured-attrs derivations) the attr is an
///   ASCII-whitespace-separated name list — the separators Nix's
///   `tokenizeString` uses (`" \t\n\r"`).
///
/// Names come back in declaration order; callers needing
/// sorted/deduplicated forms post-process. An attr declared nowhere is
/// the empty list.
pub fn string_list_attr(env: &impl StructuredAttrsEnv, key: &str) -> Vec<String> {
    let from_structured = env.structured_payload().and_then(|payload| {
        Some(
            payload
                .get(key)?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<String>>(),
        )
    });
    if let Some(names) = from_structured {
        return names;
    }
    match env.flat(key) {
        Some(raw) => raw.split_ascii_whitespace().map(String::from).collect(),
        None => Vec::new(),
    }
}

/// [`StructuredAttrsEnv`] view of an ATerm-derived env (the
/// `BTreeMap<String, String>` of [`Derivation::env`](super::Derivation::env)
/// and [`BasicDerivation::env`](super::BasicDerivation::env)).
///
/// Construction parses the `__json` payload once (cheap when absent: one
/// map lookup), so several attr reads over one derivation share the
/// parse.
#[derive(Debug)]
pub struct AtermEnv<'a> {
    env: &'a BTreeMap<String, String>,
    payload: Option<serde_json::Value>,
}

impl<'a> AtermEnv<'a> {
    /// View an ATerm env map, parsing its structured payload (if any).
    pub fn new(env: &'a BTreeMap<String, String>) -> Self {
        let payload = env
            .get(STRUCTURED_PAYLOAD_KEY)
            .and_then(|s| serde_json::from_str(s).ok());
        Self { env, payload }
    }
}

impl StructuredAttrsEnv for AtermEnv<'_> {
    fn flat(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    fn structured_payload(&self) -> Option<&serde_json::Value> {
        self.payload.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Producer-VERBATIM `__json` payload: the exact string nix 2.34.7's
    /// `derivationStrict` wrote into the env of a `__structuredAttrs`
    /// NixOS-VM-test-shaped derivation (captured via
    /// `nix derivation show`; the same corpus the recorder's
    /// show-JSON fixtures pin). Both [`STRING_LIST_USER_ATTRS`] keys
    /// appear so the canonical suite can iterate the class definition.
    const VERBATIM_PAYLOAD: &str = "{\"builder\":\"/bin/sh\",\"fetched\":\"/nix/store/rsrhap460vd96m7fwffigi23cprl38r2-structured-fetch-1.0\",\"impureEnvVars\":[\"https_proxy\",\"http_proxy\",\"no_proxy\"],\"name\":\"structured-vm-test\",\"requiredSystemFeatures\":[\"kvm\",\"nixos-test\"],\"system\":\"x86_64-linux\"}";

    fn aterm_env(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Expected values per attr in [`VERBATIM_PAYLOAD`], keyed in
    /// [`STRING_LIST_USER_ATTRS`] order so the test below cannot cover
    /// fewer attrs than the class declares.
    const VERBATIM_EXPECTED: [&[&str]; STRING_LIST_USER_ATTRS.len()] = [
        &["kvm", "nixos-test"],
        &["https_proxy", "http_proxy", "no_proxy"],
    ];

    #[test]
    fn structured_payload_wins_over_flat_decoys_for_every_listed_attr() {
        // The whole class, iterated from its definition: for every listed
        // attr, the structured payload beats a flat decoy key (which can
        // never co-occur with `__json` in real derivationStrict output —
        // the decoy proves precedence, not coexistence).
        let env = aterm_env(&[
            ("__structuredAttrs", "1"),
            (STRUCTURED_PAYLOAD_KEY, VERBATIM_PAYLOAD),
            (REQUIRED_SYSTEM_FEATURES_ATTR, "decoy-flat-feature"),
            (IMPURE_ENV_VARS_ATTR, "DECOY_VAR"),
        ]);
        let view = AtermEnv::new(&env);
        for (key, expected) in STRING_LIST_USER_ATTRS.iter().zip(VERBATIM_EXPECTED) {
            assert_eq!(
                string_list_attr(&view, key),
                expected,
                "{key} must come from the structured payload"
            );
        }
    }

    #[test]
    fn flat_env_reads_split_on_ascii_whitespace_for_every_listed_attr() {
        // Non-structured derivations: the flat value is an
        // ASCII-whitespace-separated list (Nix tokenizes on " \t\n\r"),
        // kept in declaration order.
        for key in STRING_LIST_USER_ATTRS {
            let env = aterm_env(&[(key, "kvm\tbig-parallel  nixos-test\n")]);
            assert_eq!(
                string_list_attr(&AtermEnv::new(&env), key),
                vec!["kvm", "big-parallel", "nixos-test"],
                "{key}"
            );
        }
    }

    #[test]
    fn payload_without_the_key_falls_through_to_flat() {
        // A structured payload that simply lacks the attr (or holds it as
        // a non-array) is not a declaration of emptiness — the read falls
        // through to the flat env. This is rio-local tolerance, NOT an
        // upstream mirror: nix 2.34.7's getStringSetAttr
        // (derivation-options.cc) never reads the flat env once a payload
        // exists — missing key is empty, non-array throws (see the module
        // doc for why rio's hand-assembled-env carriers tolerate instead).
        for key in STRING_LIST_USER_ATTRS {
            let env = aterm_env(&[
                (STRUCTURED_PAYLOAD_KEY, "{\"name\":\"other\"}"),
                (key, "fallback-feature"),
            ]);
            assert_eq!(
                string_list_attr(&AtermEnv::new(&env), key),
                vec!["fallback-feature"],
                "{key}"
            );
            let env = aterm_env(&[(
                STRUCTURED_PAYLOAD_KEY,
                &format!("{{\"{key}\":\"not-an-array\"}}"),
            )]);
            assert_eq!(
                string_list_attr(&AtermEnv::new(&env), key),
                Vec::<String>::new(),
                "{key}: non-array payload value with no flat fallback"
            );
        }
    }

    #[test]
    fn malformed_payload_falls_back_to_flat_and_absence_is_empty() {
        for key in STRING_LIST_USER_ATTRS {
            let env = aterm_env(&[(STRUCTURED_PAYLOAD_KEY, "{not json"), (key, "kvm")]);
            assert_eq!(string_list_attr(&AtermEnv::new(&env), key), vec!["kvm"]);

            let empty = aterm_env(&[]);
            assert_eq!(
                string_list_attr(&AtermEnv::new(&empty), key),
                Vec::<String>::new()
            );
        }
    }

    #[test]
    fn round_trips_attr_lists_through_both_encodings() {
        // Agreement property between the rule's two arms: the same
        // declaration, encoded the structured way (JSON array in the
        // payload) and the flat way (whitespace-joined env value), reads
        // back identically — so no consumer can observe different
        // features for the same derivation depending on its
        // __structuredAttrs flag.
        let lists: [&[&str]; 4] = [
            &[],
            &["kvm"],
            &["kvm", "nixos-test", "big-parallel"],
            &["feature.with.dots", "feature-with-dashes", "under_scores"],
        ];
        for key in STRING_LIST_USER_ATTRS {
            for list in lists {
                let payload = serde_json::json!({ key: list }).to_string();
                let structured = aterm_env(&[(STRUCTURED_PAYLOAD_KEY, payload.as_str())]);
                let flat = aterm_env(&[(key, list.join(" ").as_str())]);
                assert_eq!(
                    string_list_attr(&AtermEnv::new(&structured), key),
                    list,
                    "{key}: structured encoding"
                );
                assert_eq!(
                    string_list_attr(&AtermEnv::new(&flat), key),
                    list,
                    "{key}: flat encoding"
                );
            }
        }
    }
}
