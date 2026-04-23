//! `.fields` snapshot tripwire for `admin_types.proto`.
//!
//! Same pattern as `rio-store/tests/migrations.rs::migration_checksums_frozen`:
//! not a correctness check — a "you touched this, prove you thought
//! about it" gate. Adding/retyping/re-qualifying a proto3 field is a
//! wire-compat decision; this test forces the decision to be explicit.
//!
//! See `docs/src/crate-structure.md` §rio-proto for the rule this guards.

/// Normalized field-declaration lines from a `.proto` source.
///
/// Matches any line of the shape `... <name> = <N>;` (optionally with a
/// trailing `// ...` comment), which covers scalar, `optional`,
/// `repeated`, `map<K,V>`, message-typed, and enum-value declarations —
/// all wire-contract-relevant. Normalizes inner whitespace to single
/// spaces so reflowing a field's leading indent doesn't trip the wire.
fn extract_fields(proto: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in proto.lines() {
        // Strip trailing inline comment (after the last `//` that isn't
        // inside a string — proto field decls have no string literals
        // before `;`, so the simple split is safe).
        let line = match raw.find("//") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let line = line.trim();
        // Shape: `... = N;` with N all-digit.
        let Some(body) = line.strip_suffix(';') else {
            continue;
        };
        let Some((head, num)) = body.rsplit_once('=') else {
            continue;
        };
        let num = num.trim();
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
        if head.is_empty() {
            continue;
        }
        out.push(format!("{head} = {num};"));
    }
    out.sort();
    out
}

/// `admin_types.proto` field set matches the checked-in snapshot.
///
/// **Adding/changing a field:** the test fails listing the diff. For
/// each NEW or RETYPED scalar field, decide: does the consumer's
/// behaviour differ between "field absent" and "field = proto3
/// zero-value"? If yes, declare it `optional` and add a back-compat
/// decode test (decode a byte-slice WITHOUT the new tag, assert the
/// consumer behaves as it did pre-addition). Then regenerate the
/// snapshot:
///
/// ```sh
/// cargo test -p rio-proto --test field_presence -- --ignored regenerate
/// ```
///
/// and commit `rio-proto/proto/admin_types.proto.fields` alongside.
#[test]
fn admin_types_fields_frozen() {
    const PROTO: &str = include_str!("../proto/admin_types.proto");
    const SNAPSHOT: &str = include_str!("../proto/admin_types.proto.fields");

    let live = extract_fields(PROTO);
    let pinned: Vec<String> = SNAPSHOT
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();

    if live == pinned {
        return;
    }

    // Compute a readable diff: lines only in live (new/changed) and
    // only in pinned (removed/changed).
    let live_set: std::collections::BTreeSet<_> = live.iter().collect();
    let pinned_set: std::collections::BTreeSet<_> = pinned.iter().collect();
    let added: Vec<_> = live_set.difference(&pinned_set).collect();
    let removed: Vec<_> = pinned_set.difference(&live_set).collect();

    let mut diff = String::new();
    for l in &removed {
        diff.push_str(&format!("    - {l}\n"));
    }
    for l in &added {
        diff.push_str(&format!("    + {l}\n"));
    }

    panic!(
        "\n  admin_types.proto field set changed — for each new/retyped \
         scalar field, decide whether absence is distinguishable from \
         zero-value on the consumer side. If yes: declare it `optional` \
         and add a back-compat decode test in the consumer crate's \
         tests/roundtrip.rs (decode bytes WITHOUT the tag, assert \
         pre-addition behaviour). Then regenerate the snapshot:\n    \
         cargo test -p rio-proto --test field_presence -- --ignored regenerate\n  \
         and commit rio-proto/proto/admin_types.proto.fields.\n\n  \
         diff (- pinned, + live):\n{diff}"
    );
}

/// Regenerate `admin_types.proto.fields` from the current proto.
/// `#[ignore]` so it never runs in CI; invoke explicitly via the
/// command in [`admin_types_fields_frozen`]'s doc.
#[test]
#[ignore = "regenerator, not a test — run with `-- --ignored regenerate`"]
fn regenerate() {
    let proto = include_str!("../proto/admin_types.proto");
    let fields = extract_fields(proto);
    let out = format!("{}\n", fields.join("\n"));
    let path = format!(
        "{}/proto/admin_types.proto.fields",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::write(&path, out).unwrap();
    eprintln!("wrote {} fields to {path}", fields.len());
}

#[test]
fn extract_fields_normalizes() {
    let src = r#"
message M {
  // comment-only line = 9; ignored
  bool plain = 1;
  optional bool   maybe =  2;   // trailing
  repeated string names = 3;
  map<string, uint32> counts = 4;
  google.protobuf.Timestamp ts = 5;
  reserved 6, 7;
}
enum E {
  ZERO = 0;
}
"#;
    let got = extract_fields(src);
    assert_eq!(
        got,
        vec![
            "ZERO = 0;",
            "bool plain = 1;",
            "google.protobuf.Timestamp ts = 5;",
            "map<string, uint32> counts = 4;",
            "optional bool maybe = 2;",
            "repeated string names = 3;",
        ]
    );
}
