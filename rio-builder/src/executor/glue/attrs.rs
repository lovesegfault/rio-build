//! `__structuredAttrs` materialization: `.attrs.json` and `.attrs.sh`.
//!
//! For a structured-attrs derivation the builder receives its attribute
//! set as two files in the build directory instead of environment
//! variables: `.attrs.json` (the attrs as JSON) and `.attrs.sh` (a bash
//! serialization stdenv sources). This module replicates CppNix's
//! `prepareStructuredAttrs` + `writeStructuredAttrsShell`
//! (`parsed-derivations.cc`):
//!
//! - the JSON is the derivation's `__json` blob,
//! - plus an `outputs` object mapping each output name to its
//!   placeholder (rewritten to the real/scratch path when the file is
//!   written),
//! - with each `exportReferencesGraph` entry replaced by the closure
//!   info of the named paths,
//! - and `.attrs.sh` is derived from that prepared JSON by the bash
//!   serialization rules (strings/integral numbers/bools/null become
//!   `declare`, flat arrays `declare -a`, flat string-keyed objects
//!   `declare -A`; keys that are not valid shell identifiers and
//!   non-flat values are silently skipped).
//!
//! Both files are passed through the build's `inputRewrites` before
//! being written, exactly like CppNix `rewriteStrings()`s the rendered
//! text — placeholders inside arbitrary nested values get rewritten
//! without this module having to understand them.

use std::collections::BTreeMap;

use rio_nix::store_path::hash_placeholder;
use serde_json::Value;

use super::GlueError;
use super::env::rewrite;
use super::refs_graph::ClosureIndex;

/// The two structured-attrs files, already placeholder-rewritten.
#[derive(Debug)]
pub(crate) struct StructuredAttrFiles {
    pub attrs_json: Vec<u8>,
    pub attrs_sh: Vec<u8>,
}

/// Prepare `.attrs.json` + `.attrs.sh` for a structured-attrs derivation.
///
/// `json` is the parsed `__json` blob; `output_names` the derivation's
/// output names in declaration order; `closure` the input-closure index
/// used to expand `exportReferencesGraph`.
pub(crate) fn prepare_structured_attrs(
    json: &Value,
    output_names: &[String],
    closure: &ClosureIndex<'_>,
    rewrites: &BTreeMap<String, String>,
) -> Result<StructuredAttrFiles, GlueError> {
    let mut prepared = json.clone();
    let Value::Object(map) = &mut prepared else {
        return Err(GlueError::StructuredAttrsNotObject);
    };

    // outputs = { <name>: hashPlaceholder(<name>) } — rewritten to the
    // real/scratch paths when the file content is rewritten below.
    let outputs: serde_json::Map<String, Value> = output_names
        .iter()
        .map(|n| (n.clone(), Value::String(hash_placeholder(n))))
        .collect();
    map.insert("outputs".to_string(), Value::Object(outputs));

    // exportReferencesGraph: { <key>: <path or [paths]> } → closure info.
    if let Some(Value::Object(erg)) = map.get("exportReferencesGraph").cloned().as_ref() {
        for (key, val) in erg {
            // Unlike the flat form (where the name becomes a file path
            // under /build and CppNix validates it), the structured form
            // performs NO key validation in CppNix: the key only ever
            // becomes a JSON key in `.attrs.json`, and the `.attrs.sh`
            // writer below skips non-identifier keys exactly like
            // CppNix's `writeStructuredAttrsShell`. Rejecting here would
            // refuse derivations real Nix builds.
            let targets: Vec<String> = match val {
                Value::String(s) => vec![s.clone()],
                Value::Array(a) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                _ => Vec::new(),
            };
            let info = closure.closure_info_json(&targets)?;
            map.insert(key.clone(), info);
        }
    }

    // CppNix serializes `.attrs.json` with nlohmann::json, whose object
    // keys are lexicographically sorted at every nesting level. The
    // workspace's serde_json enables `preserve_order` (insertion order),
    // so sort recursively before serializing — the differential harness
    // asserts byte-identical `.attrs.json` against real Nix, and key
    // order is the only representational difference.
    sort_json_keys(&mut prepared);
    let json_text = serde_json::to_string(&prepared).map_err(GlueError::AttrsJsonSerialize)?;
    let sh_text = write_structured_attrs_shell(&prepared);

    Ok(StructuredAttrFiles {
        attrs_json: rewrite(&json_text, rewrites).into_bytes(),
        attrs_sh: rewrite(&sh_text, rewrites).into_bytes(),
    })
}

/// Recursively rebuild every JSON object with lexicographically sorted
/// keys (nlohmann::json's iteration order, hence CppNix's `.attrs.json`
/// byte layout). Arrays keep their element order; only object key order
/// changes.
fn sort_json_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (_, v) in &mut entries {
                sort_json_keys(v);
            }
            *map = entries.into_iter().collect();
        }
        Value::Array(items) => {
            for v in items {
                sort_json_keys(v);
            }
        }
        _ => {}
    }
}

/// Bash serialization of the prepared attrs (CppNix
/// `writeStructuredAttrsShell`).
fn write_structured_attrs_shell(json: &Value) -> String {
    let Value::Object(map) = json else {
        return String::new();
    };

    let mut out = String::new();
    for (key, value) in map {
        if !is_shell_identifier(key) {
            continue;
        }
        if let Some(simple) = simple_value(value) {
            out.push_str(&format!("declare {key}={simple}\n"));
        } else if let Value::Array(items) = value {
            let mut body = String::new();
            let mut good = true;
            for item in items {
                match simple_value(item) {
                    Some(s) => {
                        body.push_str(&s);
                        body.push(' ');
                    }
                    None => {
                        good = false;
                        break;
                    }
                }
            }
            if good {
                out.push_str(&format!("declare -a {key}=({body})\n"));
            }
        } else if let Value::Object(entries) = value {
            let mut body = String::new();
            let mut good = true;
            for (k2, v2) in entries {
                match simple_value(v2) {
                    Some(s) => {
                        body.push_str(&format!("[{}]={s} ", shell_escape(k2)));
                    }
                    None => {
                        good = false;
                        break;
                    }
                }
            }
            if good {
                out.push_str(&format!("declare -A {key}=({body})\n"));
            }
        }
        // Non-simple, non-array, non-object values cannot occur (those
        // are the only JSON kinds); nested/non-flat values were handled
        // by `good = false` above (key skipped entirely).
    }
    out
}

/// CppNix `handleSimpleType`: the bash rendering of a scalar, or `None`
/// if the value is not a scalar (which makes the surrounding key
/// "non-flat" and skipped).
fn simple_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(shell_escape(s)),
        Value::Number(n) => {
            // Only integral numbers are representable; CppNix routes via
            // float-and-ceil, accepting 2.0 but skipping 2.5.
            if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Some(u.to_string())
            } else {
                let f = n.as_f64()?;
                (f.ceil() == f).then(|| (f as i64).to_string())
            }
        }
        Value::Null => Some("''".to_string()),
        Value::Bool(true) => Some("1".to_string()),
        Value::Bool(false) => Some(String::new()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// CppNix `shellEscape`: wrap in single quotes, escaping embedded single
/// quotes as `'\''`. Always quotes, even for empty/simple strings.
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// `[A-Za-z_][A-Za-z0-9_]*`
fn is_shell_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_closure() -> ClosureIndex<'static> {
        ClosureIndex::new(&[], &[])
    }

    #[test]
    fn outputs_map_uses_placeholders_then_rewrites() {
        let json = json!({"name": "demo", "__structuredAttrs": true});
        let ph = hash_placeholder("out");
        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            ph,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo".to_string(),
        );

        let files =
            prepare_structured_attrs(&json, &["out".to_string()], &empty_closure(), &rewrites)
                .unwrap();

        let parsed: Value = serde_json::from_slice(&files.attrs_json).unwrap();
        assert_eq!(
            parsed["outputs"]["out"],
            json!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo"),
            "placeholder must be rewritten in the written file"
        );
        // .attrs.sh declares the outputs assoc array with the rewritten path.
        let sh = String::from_utf8(files.attrs_sh).unwrap();
        assert!(
            sh.contains(
                "declare -A outputs=(['out']='/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo' )"
            ),
            "got:\n{sh}"
        );
    }

    /// CppNix performs no key-name validation for the structured
    /// `exportReferencesGraph` form (the key never becomes a filesystem
    /// path — unlike the flat form, where the name is a file under
    /// /build); names the flat form rejects, including traversal-shaped
    /// ones, must be accepted here, land in `.attrs.json` as plain JSON
    /// keys, and simply be skipped by the shell serialization.
    #[test]
    fn structured_graph_keys_are_not_name_validated() {
        let json = json!({
            "__structuredAttrs": true,
            "exportReferencesGraph": { "closure info.json": [], "../escape": [] }
        });
        let files = prepare_structured_attrs(
            &json,
            &["out".to_string()],
            &empty_closure(),
            &BTreeMap::new(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_slice(&files.attrs_json).unwrap();
        for key in ["closure info.json", "../escape"] {
            assert!(
                parsed.get(key).is_some(),
                "graph key {key:?} must appear in .attrs.json: {parsed}"
            );
        }
        let sh = String::from_utf8(files.attrs_sh).unwrap();
        assert!(
            !sh.contains("closure info.json") && !sh.contains("escape"),
            ".attrs.sh must skip non-identifier keys:\n{sh}"
        );
    }

    #[test]
    fn attrs_json_keys_are_sorted_at_every_level() {
        // CppNix (nlohmann::json) emits lexicographically sorted keys at
        // every nesting level; the differential harness asserts byte
        // identity, so the serialized order must match regardless of the
        // __json blob's insertion order.
        let json = json!({
            "zeta": {"b": 1, "a": 2},
            "alpha": "first",
            "__structuredAttrs": true,
            "midList": [{"z": 1, "a": 2}]
        });
        let files = prepare_structured_attrs(
            &json,
            &["out".to_string()],
            &empty_closure(),
            &BTreeMap::new(),
        )
        .unwrap();
        let text = String::from_utf8(files.attrs_json).unwrap();

        // Top level: __structuredAttrs < alpha < midList < outputs < zeta.
        let order: Vec<usize> = [
            "\"__structuredAttrs\"",
            "\"alpha\"",
            "\"midList\"",
            "\"outputs\"",
            "\"zeta\"",
        ]
        .iter()
        .map(|k| text.find(*k).expect("key present"))
        .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "top-level keys not sorted: {text}"
        );
        // Nested object inside zeta: a before b.
        let zeta = &text[text.find("\"zeta\"").unwrap()..];
        assert!(
            zeta.find("\"a\"").unwrap() < zeta.find("\"b\"").unwrap(),
            "nested keys not sorted: {zeta}"
        );
        // Objects inside arrays are sorted too.
        let mid = &text[text.find("\"midList\"").unwrap()..text.find("\"outputs\"").unwrap()];
        assert!(
            mid.find("\"a\"").unwrap() < mid.find("\"z\"").unwrap(),
            "array-element object keys not sorted: {mid}"
        );
    }

    #[test]
    fn shell_serialization_rules() {
        let json = json!({
            "aString": "hello world",
            "quoted": "it's",
            "anInt": 7,
            "aFloat": 2.5,
            "wholeFloat": 3.0,
            "yes": true,
            "no": false,
            "nothing": null,
            "flatList": ["a", "b c", 5],
            "nestedList": [["x"]],
            "flatAttrs": {"k": "v", "n": 2},
            "nestedAttrs": {"k": {"deep": true}},
            "bad-key": "skipped",
            "_ok": "kept"
        });
        let sh = write_structured_attrs_shell(&json);

        assert!(sh.contains("declare aString='hello world'\n"));
        assert!(sh.contains(r"declare quoted='it'\''s'"));
        assert!(sh.contains("declare anInt=7\n"));
        assert!(!sh.contains("aFloat"), "non-integral numbers are skipped");
        assert!(sh.contains("declare wholeFloat=3\n"));
        assert!(sh.contains("declare yes=1\n"));
        assert!(sh.contains("declare no=\n"));
        assert!(sh.contains("declare nothing=''\n"));
        assert!(sh.contains("declare -a flatList=('a' 'b c' 5 )\n"));
        assert!(!sh.contains("nestedList"), "non-flat lists are skipped");
        assert!(sh.contains("declare -A flatAttrs=(['k']='v' ['n']=2 )\n"));
        assert!(!sh.contains("nestedAttrs"), "non-flat attrsets are skipped");
        assert!(!sh.contains("bad-key"), "non-identifier keys are skipped");
        assert!(sh.contains("declare _ok='kept'\n"));
    }

    #[test]
    fn export_references_graph_is_expanded() {
        use rio_nix::store_path::StorePath;
        use rio_proto::validated::ValidatedPathInfo;

        let p_dep = "/nix/store/dddddddddddddddddddddddddddddddd-dep";
        let p_top = "/nix/store/ffffffffffffffffffffffffffffffff-top";
        let infos = vec![
            ValidatedPathInfo {
                store_path: StorePath::parse(p_dep).unwrap(),
                store_path_hash: vec![],
                deriver: None,
                nar_hash: [1u8; 32],
                nar_size: 100,
                references: vec![],
                registration_time: 0,
                ultimate: false,
                signatures: vec![],
                content_address: None,
            },
            ValidatedPathInfo {
                store_path: StorePath::parse(p_top).unwrap(),
                store_path_hash: vec![],
                deriver: None,
                nar_hash: [2u8; 32],
                nar_size: 200,
                references: vec![StorePath::parse(p_dep).unwrap()],
                registration_time: 0,
                ultimate: false,
                signatures: vec![],
                content_address: None,
            },
        ];
        let closure_paths: Vec<String> = vec![p_dep.to_string(), p_top.to_string()];
        let index = ClosureIndex::new(&infos, &closure_paths);

        let json = json!({
            "exportReferencesGraph": {"closure": [p_top]}
        });
        let files = prepare_structured_attrs(&json, &["out".to_string()], &index, &BTreeMap::new())
            .unwrap();
        let parsed: Value = serde_json::from_slice(&files.attrs_json).unwrap();

        // The key is replaced by an array of path-info objects covering
        // the closure (dep + top), sorted by path.
        let arr = parsed["closure"].as_array().expect("closure array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["path"], json!(p_dep));
        assert_eq!(arr[1]["path"], json!(p_top));
        assert_eq!(arr[1]["narSize"], json!(200));
        assert_eq!(arr[1]["references"], json!([p_dep]));
        assert!(arr[0]["narHash"].as_str().unwrap().starts_with("sha256-"));
        // closureSize = narSize + sum of references' closure sizes.
        assert_eq!(arr[1]["closureSize"], json!(300));
        // The original exportReferencesGraph key is preserved alongside.
        assert!(parsed.get("exportReferencesGraph").is_some());
    }

    #[test]
    fn non_object_json_is_rejected() {
        let err = prepare_structured_attrs(
            &json!(["not", "an", "object"]),
            &["out".to_string()],
            &empty_closure(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, GlueError::StructuredAttrsNotObject));
    }
}
