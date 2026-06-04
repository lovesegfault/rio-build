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
//!   non-flat values are silently skipped). The numeric rendering is a
//!   32-bit surface — see [`simple_value`]; `.attrs.json` keeps the
//!   full-precision values.
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
            //
            // The VALUE is the oracle's recursive `flatten`
            // (derivation-options.cc:106-114) into a string SET:
            // nested arrays are CppNix-legal and accepted; numbers,
            // bools, objects and null are errors — never silently
            // emptied or dropped.
            // r[impl builder.exec.structured-attrs-typed]
            let mut target_set = std::collections::BTreeSet::new();
            rio_nix::derivation::typed::flatten_strings(val, &mut target_set).map_err(|e| {
                GlueError::ExportRefsValueWrongType {
                    key: key.clone(),
                    found: e.to_string(),
                }
            })?;
            let targets: Vec<String> = target_set.into_iter().collect();
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
// r[impl builder.exec.attrs-sh-numeric]
fn simple_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(shell_escape(s)),
        Value::Number(n) => {
            // Oracle handleSimpleType (parsed-derivations.cc:131-135):
            //
            //     auto f = value.get<float>();
            //     if (std::ceil(f) == f)
            //         return std::to_string(value.get<int>());
            //
            // i.e. emit iff the FLOAT (f32) view of the number is
            // integral, and the emitted text is the int (i32)
            // conversion of the STORED value. `.attrs.sh` is a 32-bit
            // surface: big integers wrap modulo 2^32
            // (static_cast<int>(int64_t) — Rust `as i32` is the same
            // modular conversion), and out-of-range doubles pin the
            // oracle's x86-64 `cvttsd2si` result. `.attrs.json` keeps
            // full precision — only the shell rendering is 32-bit.
            //
            // The f32 gate has its own corner: a non-integral double
            // like 16777217.5 ROUNDS to an integral f32 (16777218.0),
            // so the oracle emits it — as the f64 truncation 16777217.
            // (f32::INFINITY passes the gate too — ceil(inf) == inf —
            // and the i32 conversion then yields the indefinite value;
            // NaN fails it, NaN != NaN. JSON cannot carry either, but
            // the arithmetic below is total anyway.)
            let f32_view = if let Some(i) = n.as_i64() {
                i as f32
            } else if let Some(u) = n.as_u64() {
                u as f32
            } else {
                n.as_f64()? as f32
            };
            if f32_view.ceil() != f32_view {
                return None;
            }
            let text = if let Some(i) = n.as_i64() {
                // static_cast<int>(int64_t): modular.
                (i as i32).to_string()
            } else if let Some(u) = n.as_u64() {
                // static_cast<int>(uint64_t): modular.
                (u as i32).to_string()
            } else {
                double_to_int_x86_64(n.as_f64()?).to_string()
            };
            Some(text)
        }
        Value::Null => Some("''".to_string()),
        Value::Bool(true) => Some("1".to_string()),
        Value::Bool(false) => Some(String::new()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// `static_cast<int>(double)` as the oracle's x86-64 build performs it:
/// `cvttsd2si` truncates toward zero, and any input whose truncation is
/// not representable in i32 — including NaN — yields the "integer
/// indefinite" value `i32::MIN` (0x8000_0000).
///
/// (ISO C++ calls the out-of-range case undefined; the differential
/// gate runs the pinned oracle on x86-64, so the instruction's actual
/// behavior is the parity contract. Rust's `as` saturates instead,
/// which is why this helper exists.)
// r[impl builder.exec.attrs-sh-numeric]
fn double_to_int_x86_64(d: f64) -> i32 {
    let t = d.trunc();
    if t.is_nan() || t < i32::MIN as f64 || t > i32::MAX as f64 {
        i32::MIN
    } else {
        t as i32
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

    /// A wrong-typed `exportReferencesGraph` leaf is an error (oracle
    /// `flatten` throws), never silently emptied or skipped.
    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn erg_wrong_typed_value_rejects() {
        for bad in [
            json!({"exportReferencesGraph": {"refs": 42}}),
            json!({"exportReferencesGraph": {"refs": true}}),
            json!({"exportReferencesGraph": {"refs": {"k": "v"}}}),
            json!({"exportReferencesGraph": {"refs": ["ok", 7]}}),
            json!({"exportReferencesGraph": {"refs": [["nested", null]]}}),
        ] {
            let err = prepare_structured_attrs(
                &bad,
                &["out".to_string()],
                &empty_closure(),
                &BTreeMap::new(),
            )
            .expect_err(&format!("must reject: {bad}"));
            assert!(
                err.to_string()
                    .contains("'exportReferencesGraph' value is not an array or a string"),
                "{err}"
            );
            // Permanence is structural: GlueError carries no transient
            // class at all (builder.glue.pure).
        }
    }

    /// Nested arrays are CppNix-legal (`flatten` recurses) — the
    /// pre-fix reader silently emptied them, producing a wrong (empty)
    /// closure file where the oracle exports the real closure.
    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn erg_nested_array_flattens_recursively() {
        use rio_nix::store_path::StorePath;
        use rio_proto::validated::ValidatedPathInfo;

        let p = "/nix/store/dddddddddddddddddddddddddddddddd-dep";
        let infos = vec![ValidatedPathInfo {
            store_path: StorePath::parse(p).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [1u8; 32],
            nar_size: 100,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        }];
        let closure_paths = vec![p.to_string()];
        let index = ClosureIndex::new(&infos, &closure_paths);

        let nested = json!({"exportReferencesGraph": {"refs": [[p]]}});
        let flat = json!({"exportReferencesGraph": {"refs": [p]}});
        let render = |j: &Value| -> Value {
            let files = prepare_structured_attrs(j, &["out".to_string()], &index, &BTreeMap::new())
                .unwrap();
            serde_json::from_slice(&files.attrs_json).unwrap()
        };
        // The EXPANDED graph key must be identical for the nested and
        // flat spellings (the original `exportReferencesGraph` attr is
        // echoed verbatim by both rio and the oracle, so it
        // legitimately differs between the two spellings).
        let nested_out = render(&nested);
        let flat_out = render(&flat);
        assert_eq!(
            nested_out.get("refs"),
            flat_out.get("refs"),
            "nested and flat forms must export the identical closure"
        );
        let arr = nested_out.get("refs").and_then(Value::as_array).unwrap();
        assert_eq!(arr.len(), 1, "the closure member is exported: {nested_out}");
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
        // 32-bit surface inside containers too: a big int in a flat
        // list/attrset wraps exactly like a top-level one.
        let json = json!({"l": [5000000000i64], "m": {"k": 5000000000i64}});
        let sh = write_structured_attrs_shell(&json);
        assert!(sh.contains("declare -a l=(705032704 )\n"));
        assert!(sh.contains("declare -A m=(['k']=705032704 )\n"));
    }

    /// Oracle `handleSimpleType` numeric matrix
    /// (parsed-derivations.cc:131-135): emit iff the f32 view is
    /// integral; the emitted text is the i32 conversion of the stored
    /// value (modular for ints, `cvttsd2si` for doubles).
    // r[verify builder.exec.attrs-sh-numeric]
    #[test]
    fn numeric_rendering_matches_cppnix_int32_semantics() {
        let cases: &[(Value, Option<&str>)] = &[
            // In-range ints: unchanged.
            (json!(42), Some("42")),
            (json!(7), Some("7")),
            (json!(-17), Some("-17")),
            // Integral double: emitted as its int value.
            (json!(3.0), Some("3")),
            // Non-integral double whose f32 view is ALSO non-integral:
            // skipped.
            (json!(2.5), None),
            // Big ints wrap modulo 2^32 (static_cast<int>(int64_t)).
            (json!(5_000_000_000i64), Some("705032704")),
            (json!(-5_000_000_000i64), Some("-705032704")),
            (json!(4_294_967_296i64), Some("0")), // 2^32
            (json!(i64::MAX), Some("-1")),
            (json!(u64::MAX), Some("-1")),
            // Rounding edge: 16777217.5 is non-integral as f64 but its
            // f32 view rounds to 16777218.0 (integral), so the oracle
            // EMITS it — as the f64 truncation.
            (json!(16777217.5), Some("16777217")),
            // Out-of-i32-range double: f32 view integral → emitted as
            // cvttsd2si's indefinite value.
            (json!(1.0e10), Some("-2147483648")),
            (json!(-1.0e10), Some("-2147483648")),
            // Truncation right at the boundary stays representable.
            (json!(2147483647.5), Some("2147483647")),
            (json!(2147483648.0), Some("-2147483648")),
        ];
        for (value, expect) in cases {
            assert_eq!(simple_value(value).as_deref(), *expect, "value: {value}");
        }
    }

    /// Only the `.attrs.sh` rendering is 32-bit; `.attrs.json` keeps
    /// the full-precision number.
    // r[verify builder.exec.attrs-sh-numeric]
    #[test]
    fn attrs_json_keeps_full_precision_for_big_ints() {
        let closure = empty_closure();
        let json = json!({"bigInt": 5000000000i64});
        let files =
            prepare_structured_attrs(&json, &["out".to_string()], &closure, &BTreeMap::new())
                .expect("prepare");
        let json_text = String::from_utf8(files.attrs_json).unwrap();
        assert!(
            json_text.contains("\"bigInt\":5000000000"),
            ".attrs.json must keep the 64-bit value: {json_text}"
        );
        let sh_text = String::from_utf8(files.attrs_sh).unwrap();
        assert!(
            sh_text.contains("declare bigInt=705032704\n"),
            ".attrs.sh wraps to 32-bit: {sh_text}"
        );
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
        // CppNix's pathInfoToJSON shape: colon/nixbase32 narHash (not
        // SRI) and an explicit valid flag on every element.
        assert!(arr[0]["narHash"].as_str().unwrap().starts_with("sha256:"));
        assert_eq!(arr[0]["valid"], json!(true));
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

    /// The structured-attrs path propagates the cycle rejection from the
    /// closure walk: a derivation whose exportReferencesGraph target has
    /// cyclic reference metadata is rejected (never hung, never rendered).
    // r[verify builder.exec.refs-graph-acyclic]
    #[test]
    fn structured_attrs_propagates_cycle_rejection() {
        use rio_nix::store_path::StorePath;
        use rio_proto::validated::ValidatedPathInfo;

        let p_x = "/nix/store/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-x";
        let p_y = "/nix/store/yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy-y";
        let mk = |path: &str, reference: &str| ValidatedPathInfo {
            store_path: StorePath::parse(path).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [1u8; 32],
            nar_size: 1,
            references: vec![StorePath::parse(reference).unwrap()],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        };
        // x → y → x: a two-cycle (not a self-reference).
        let infos = vec![mk(p_x, p_y), mk(p_y, p_x)];
        let closure_paths: Vec<String> = vec![p_x.to_string(), p_y.to_string()];
        let index = ClosureIndex::new(&infos, &closure_paths);

        let json = json!({
            "exportReferencesGraph": {"closure": [p_x]}
        });
        let err = prepare_structured_attrs(&json, &["out".to_string()], &index, &BTreeMap::new())
            .unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsCyclicMetadata { .. }),
            "expected cycle rejection through the structured-attrs path, got {err}"
        );
    }
}
