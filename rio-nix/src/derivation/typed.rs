//! Fail-closed typed accessors over structured-attrs JSON values.
//!
//! Mirrors the oracle's `json-utils.cc` getters (pinned CppNix 2.34.7,
//! `src/libutil/json-utils.cc`) and `derivation-options.cc`'s `flatten`:
//! every accessor either returns the exact requested type or an error —
//! there is no coercion, no `filter_map` dropping, and no silent
//! fallback. Consumers that read *behavioral* structured attributes
//! (`outputChecks`, `unsafeDiscardReferences`, `exportReferencesGraph`,
//! `impureEnvVars`, `passAsFile`, `__noChroot`, …) MUST go through
//! these; the only sanctioned lenient reads are the scheduling-hint
//! tier bounded by [`super::structured::SizingHint`].
//!
//! Error message shapes follow the oracle (`Expected JSON value to be
//! of type '%s' but it is of type '%s': %s`); the trailing dump uses
//! serde_json's compact form, which can differ from nlohmann's in
//! object key order — failure *classification* is the parity surface,
//! not message bytes.
//!
//! This module is not a parser: every function is total over already
//! parsed [`serde_json::Value`]s (recursion is bounded by serde_json's
//! own recursion limit at parse time), so it carries no fuzz target of
//! its own.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// nlohmann `type_name()`: the coarse JSON type word used in oracle
/// error messages. All three numeric storage classes are "number".
pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The refined type word `getUnsigned`'s error path uses for numbers
/// ("floating point number" / "signed integral number"); everything
/// else falls back to [`type_name`].
fn unsigned_error_type_name(value: &Value) -> &'static str {
    match value {
        Value::Number(n) => {
            if n.as_u64().is_some() {
                // Unreachable from `unsigned_lossy` (it accepts these),
                // but keep the refinement total.
                "number"
            } else if n.as_i64().is_some() {
                "signed integral number"
            } else {
                "floating point number"
            }
        }
        other => type_name(other),
    }
}

/// A typed read failed: the value exists but is not the expected type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TypedError {
    /// Oracle `ensureType` (json-utils.cc:35-45).
    #[error("Expected JSON value to be of type '{want}' but it is of type '{got}': {dump}")]
    WrongType {
        want: &'static str,
        got: &'static str,
        dump: String,
    },
    /// Oracle `getUnsigned` (json-utils.cc:62-73) — kept distinct
    /// because its message names the refined numeric storage class.
    /// Raised here only for non-numbers (see [`unsigned_lossy`]).
    #[error(
        "Expected JSON value to be an unsigned integral number but it is of type '{got}': {dump}"
    )]
    NotUnsigned { got: &'static str, dump: String },
    /// Oracle `flatten` (derivation-options.cc:106-114).
    #[error("value is not an array or a string: {dump}")]
    NotStringOrArray { dump: String },
}

fn dump(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".into())
}

/// Oracle `getObject`: the value must be a JSON object.
pub fn object(value: &Value) -> Result<&Map<String, Value>, TypedError> {
    value.as_object().ok_or_else(|| TypedError::WrongType {
        want: "object",
        got: type_name(value),
        dump: dump(value),
    })
}

/// Oracle `getArray`: the value must be a JSON array.
pub fn array(value: &Value) -> Result<&[Value], TypedError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| TypedError::WrongType {
            want: "array",
            got: type_name(value),
            dump: dump(value),
        })
}

/// Oracle `getString`: the value must be a JSON string.
pub fn string(value: &Value) -> Result<&str, TypedError> {
    value.as_str().ok_or_else(|| TypedError::WrongType {
        want: "string",
        got: type_name(value),
        dump: dump(value),
    })
}

/// Oracle `getBoolean`: the value must be a JSON boolean.
// r[impl builder.exec.structured-attrs-typed]
pub fn boolean(value: &Value) -> Result<bool, TypedError> {
    value.as_bool().ok_or_else(|| TypedError::WrongType {
        want: "boolean",
        got: type_name(value),
        dump: dump(value),
    })
}

/// Oracle `getStringList`: an array whose EVERY element is a string.
/// A wrong-typed element is an error — never dropped (the pre-fix
/// readers `filter_map`ed, silently shrinking lists like
/// `allowedReferences`).
// r[impl builder.exec.structured-attrs-typed]
pub fn string_list(value: &Value) -> Result<Vec<String>, TypedError> {
    let items = array(value)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(string(item)?.to_string());
    }
    Ok(out)
}

/// nlohmann's implicit `get<uint64_t>` as the oracle's
/// `ptrToOwned<uint64_t>` invokes it (json-utils.hh:79, used for
/// `outputChecks.*.maxSize`/`maxClosureSize`):
///
/// - unsigned-stored numbers: the value;
/// - signed-stored numbers: `static_cast<uint64_t>(int64_t)` — modular
///   wrap (`-1` becomes `u64::MAX`);
/// - float-stored numbers: `static_cast<uint64_t>(double)` —
///   truncation toward zero, with the out-of-range/NaN corners pinned
///   to the x86-64 lowering the pinned oracle binary actually executes
///   (see [`double_to_uint_x86_64`]);
/// - non-numbers: an error (the only fail-closed arm — matching
///   nlohmann's `type_error.302`).
///
/// "Lossy" is in the name so no caller mistakes this for validation:
/// it reproduces the oracle's *acceptance* of weird numerics; the
/// truncated value is then ENFORCED, exactly like the oracle.
// r[impl builder.exec.structured-attrs-typed]
pub fn unsigned_lossy(value: &Value) -> Result<u64, TypedError> {
    match value {
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Ok(u)
            } else if let Some(i) = n.as_i64() {
                Ok(i as u64)
            } else {
                // serde_json numbers are u64/i64/f64; this arm is f64.
                Ok(double_to_uint_x86_64(n.as_f64().unwrap_or(f64::NAN)))
            }
        }
        other => Err(TypedError::NotUnsigned {
            got: unsigned_error_type_name(other),
            dump: dump(other),
        }),
    }
}

/// `static_cast<uint64_t>(double)` as clang/gcc lower it on x86-64
/// (no AVX-512): a branch on 2^63 around `cvttsd2si`. In-range values
/// truncate toward zero exactly; negative in-(i64)-range values take
/// the signed path and wrap modulo 2^64; NaN and values ≥ 2^64
/// collapse to 0 through the XOR-overflow path; below-i64-range
/// negatives hit the indefinite value `1 << 63`.
///
/// (ISO C++ calls every out-of-range case undefined; the differential
/// gate runs the pinned oracle binary on x86-64, so the actual
/// lowering is the parity contract — same reasoning as the i32 helper
/// in the `.attrs.sh` writer.)
fn double_to_uint_x86_64(d: f64) -> u64 {
    let t = d.trunc();
    if t.is_nan() || t >= 18_446_744_073_709_551_616.0 {
        0
    } else if t >= 0.0 {
        t as u64
    } else if t >= -9_223_372_036_854_775_808.0 {
        (t as i64) as u64
    } else {
        1u64 << 63
    }
}

/// Oracle `flatten` (derivation-options.cc:106-114): recursively
/// flatten a value into a string set — strings insert, arrays recurse
/// (NESTED arrays are CppNix-legal and accepted), anything else is an
/// error. The pre-fix reader silently emptied nested arrays and
/// skipped wrong-typed values; both are now impossible.
///
/// The oracle accumulates into a `StringSet` (sorted, deduplicated) —
/// mirrored here with a `BTreeSet` so downstream rendering sees the
/// same membership and order.
// r[impl builder.exec.structured-attrs-typed]
pub fn flatten_strings(value: &Value, out: &mut BTreeSet<String>) -> Result<(), TypedError> {
    match value {
        Value::Array(items) => {
            for item in items {
                flatten_strings(item, out)?;
            }
            Ok(())
        }
        Value::String(s) => {
            out.insert(s.clone());
            Ok(())
        }
        other => Err(TypedError::NotStringOrArray { dump: dump(other) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn typed_getters_accept_exact_types_only() {
        // accept
        assert!(object(&json!({"k": 1})).is_ok());
        assert_eq!(array(&json!([1, 2])).unwrap().len(), 2);
        assert_eq!(string(&json!("s")).unwrap(), "s");
        assert!(boolean(&json!(true)).unwrap());
        assert_eq!(
            string_list(&json!(["a", "b"])).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );

        // reject — no coercion of any kind
        assert!(object(&json!([])).is_err());
        assert!(array(&json!({})).is_err());
        assert!(string(&json!(1)).is_err());
        assert!(boolean(&json!("true")).is_err(), "no string->bool coercion");
        assert!(boolean(&json!(1)).is_err(), "no number->bool coercion");
        assert!(boolean(&json!(null)).is_err());
        // string_list: one bad element fails the WHOLE read (the
        // pre-fix filter_map silently dropped it).
        assert!(string_list(&json!(["a", 7, "b"])).is_err());
        assert!(string_list(&json!("not-a-list")).is_err());

        // message shape mirrors the oracle
        let err = boolean(&json!("true")).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"Expected JSON value to be of type 'boolean' but it is of type 'string': "true""#
        );
    }

    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn unsigned_lossy_matches_nlohmann_conversion() {
        // unsigned: identity
        assert_eq!(unsigned_lossy(&json!(1024u64)).unwrap(), 1024);
        assert_eq!(unsigned_lossy(&json!(u64::MAX)).unwrap(), u64::MAX);
        // signed: modular wrap (static_cast<uint64_t>(int64_t))
        assert_eq!(unsigned_lossy(&json!(-1)).unwrap(), u64::MAX);
        assert_eq!(
            unsigned_lossy(&json!(-5_000_000_000i64)).unwrap(),
            (-5_000_000_000i64) as u64
        );
        // float: truncation toward zero
        assert_eq!(unsigned_lossy(&json!(1024.9)).unwrap(), 1024);
        assert_eq!(unsigned_lossy(&json!(0.999)).unwrap(), 0);
        // float corners: x86-64 lowering
        assert_eq!(unsigned_lossy(&json!(-5.5)).unwrap(), (-5i64) as u64);
        assert_eq!(unsigned_lossy(&json!(2.0e19)).unwrap(), 0, ">= 2^64");
        // non-numbers: error, with the oracle's refined type words
        let err = unsigned_lossy(&json!("1024")).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"Expected JSON value to be an unsigned integral number but it is of type 'string': "1024""#
        );
        assert!(unsigned_lossy(&json!(null)).is_err());
        assert!(unsigned_lossy(&json!([])).is_err());
        assert!(unsigned_lossy(&json!(true)).is_err());
    }

    // r[verify builder.exec.structured-attrs-typed]
    #[test]
    fn flatten_strings_recursive_accept_and_reject() {
        // Nested arrays are CppNix-legal and flatten recursively, into
        // set order (the oracle's StringSet).
        let mut out = BTreeSet::new();
        flatten_strings(&json!(["b", ["a", ["c"]], "b"]), &mut out).unwrap();
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );

        // Wrong-typed leaves error out (oracle message text), even
        // when buried in a nested array — never skipped.
        let mut out = BTreeSet::new();
        let err = flatten_strings(&json!(["ok", [42]]), &mut out).unwrap_err();
        assert_eq!(err.to_string(), "value is not an array or a string: 42");
        for bad in [json!(7), json!(true), json!(null), json!({"k": "v"})] {
            let mut out = BTreeSet::new();
            assert!(flatten_strings(&bad, &mut out).is_err(), "value: {bad}");
        }
    }
}
