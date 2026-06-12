//! `.fields` snapshot tripwire for every `.proto` file in the crate.
//!
//! Same pattern as `rio-migrations/tests/migrations.rs::migration_checksums_frozen`:
//! not a correctness check — a "you touched this, prove you thought
//! about it" gate. Adding/retyping/re-qualifying a proto3 field is a
//! wire-compat decision; this test forces the decision to be explicit.
//!
//! The source-text golden snapshot pins `<type> <name> = <N>;` including
//! the type keyword, so it catches **all** retypes — including the
//! same-field-number same-wire-type case (`string` → `bytes`, both
//! wire-type 2) and the same-field-number cross-wire-type case
//! (`double factor = 3` → `string factor_json = 3`, the r43 bug_011
//! deploy hazard this guards against).
//!
//! ## Coverage
//!
//! Every `rio-proto/proto/*.proto` file gets a frozen-snapshot test and
//! a `#[ignore]`d regenerator. The list is declared once in
//! [`field_snapshot_tests!`] and cross-checked against the on-disk
//! `proto/` directory by [`every_proto_has_a_snapshot_test`], so adding
//! `new.proto` without a tripwire fails a structural test rather than
//! silently shipping unguarded.
//!
//! ## Adding/changing a field
//!
//! The `<name>_fields_frozen` test fails listing the diff. For each NEW
//! or RETYPED scalar field, decide: does the consumer's behaviour
//! differ between "field absent" and "field = proto3 zero-value"? If
//! yes, declare it `optional` and add a back-compat decode test
//! (decode a byte-slice WITHOUT the new tag, assert the consumer
//! behaves as it did pre-addition). For a RETYPED or REMOVED field,
//! `reserved N; reserved "name";` the old number — never reuse a field
//! number on a new type (cross-wire-type fails the whole message
//! decode; same-wire-type silently decodes the wrong value). Then
//! regenerate the snapshot:
//!
//! ```sh
//! cargo test -p rio-proto --test proto_field_presence -- --ignored regenerate_<name>
//! ```
//!
//! and commit `rio-proto/proto/<name>.proto.fields` alongside.
//!
//! See `docs/spec/system/crate-structure.typ` §rio-proto for the rule this guards.

mod common;
use common::extract_fields;

use std::collections::BTreeSet;
use std::path::Path;

/// Compare the live proto's field set against its checked-in `.fields`
/// snapshot. Panics with a readable diff and remediation steps when
/// they differ. Returning normally means the wire contract is unchanged.
fn check_snapshot(name: &str, proto: &str, snapshot: &str) {
    let live = extract_fields(proto);
    let pinned: Vec<String> = snapshot
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
    let live_set: BTreeSet<_> = live.iter().collect();
    let pinned_set: BTreeSet<_> = pinned.iter().collect();
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
        "\n  {name}.proto field set changed — for each new/retyped \
         scalar field, decide whether absence is distinguishable from \
         zero-value on the consumer side (if yes: `optional` + a \
         back-compat decode test in tests/roundtrip.rs that decodes \
         bytes WITHOUT the tag and asserts pre-addition behaviour). \
         For a RETYPED or REMOVED field, `reserved` the old number and \
         name — never reuse a field number on a new type (cross-wire-type \
         fails the whole message decode; same-wire-type silently decodes \
         the wrong value — r43 bug_011). Then regenerate the snapshot:\n    \
         cargo test -p rio-proto --test proto_field_presence -- --ignored regenerate_{name}\n  \
         and commit rio-proto/proto/{name}.proto.fields.\n\n  \
         diff (- pinned, + live):\n{diff}"
    );
}

/// Write the `.fields` snapshot for `name` from its current proto
/// source. Used by the `#[ignore]`d `regenerate_<name>` tests.
///
/// Reads `CARGO_MANIFEST_DIR` at *runtime* (not `env!()`) so the path
/// resolves correctly both under bare `cargo test` and under nextest
/// `--workspace-remap` (where the compile-time path is a per-crate
/// build sandbox that no longer exists).
fn regenerate(name: &str, proto: &str) {
    let fields = extract_fields(proto);
    // One `<decl>\n` per line; service-only protos (no message fields)
    // produce an empty file rather than a lone `\n`, matching what
    // `end-of-file-fixer` would normalise to anyway.
    let out: String = fields.iter().map(|f| format!("{f}\n")).collect();
    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest");
    let path = format!("{manifest}/proto/{name}.proto.fields");
    std::fs::write(&path, out).unwrap();
    eprintln!("wrote {} fields to {path}", fields.len());
}

/// Declare the per-proto tripwire tests and the `KNOWN_PROTOS` list in
/// one place so they cannot drift. Each entry produces:
///
/// - `<name>_fields_frozen` — fails on any field-set change until the
///   snapshot is regenerated.
/// - `regenerate_<name>` — `#[ignore]`d regenerator; run explicitly via
///   the command in the module docs.
///
/// `KNOWN_PROTOS` is cross-checked against `proto/*.proto` on disk by
/// [`every_proto_has_a_snapshot_test`] so a new proto file cannot ship
/// without a tripwire entry here.
macro_rules! field_snapshot_tests {
    ($( $frozen:ident, $regen:ident => $name:literal );+ $(;)?) => {
        $(
            #[test]
            fn $frozen() {
                check_snapshot(
                    $name,
                    include_str!(concat!("../proto/", $name, ".proto")),
                    include_str!(concat!("../proto/", $name, ".proto.fields")),
                );
            }

            #[test]
            #[ignore = "regenerator, not a test — run with `-- --ignored regenerate_<name>` (see module docs)"]
            fn $regen() {
                regenerate($name, include_str!(concat!("../proto/", $name, ".proto")));
            }
        )+

        /// Proto base names with a registered `.fields` tripwire. Must
        /// match the `*.proto` files under `rio-proto/proto/` exactly —
        /// enforced by [`every_proto_has_a_snapshot_test`].
        const KNOWN_PROTOS: &[&str] = &[$($name),+];
    };
}

field_snapshot_tests! {
    admin_fields_frozen,       regenerate_admin       => "admin";
    admin_types_fields_frozen, regenerate_admin_types => "admin_types";
    build_types_fields_frozen, regenerate_build_types => "build_types";
    builder_fields_frozen,     regenerate_builder     => "builder";
    castore_fields_frozen,     regenerate_castore     => "castore";
    dag_fields_frozen,         regenerate_dag         => "dag";
    derivation_fields_frozen,  regenerate_derivation  => "derivation";
    scheduler_fields_frozen,   regenerate_scheduler   => "scheduler";
    store_fields_frozen,       regenerate_store       => "store";
    types_fields_frozen,       regenerate_types       => "types";
}

/// Every `*.proto` file under `rio-proto/proto/` has a registered
/// tripwire in [`field_snapshot_tests!`].
///
/// This is the structural completeness gate the per-proto tests can't
/// provide on their own: a brand-new `new.proto` with no `.fields`
/// snapshot won't fail any `<name>_fields_frozen` test (there isn't
/// one), so a field retype in it would ship unguarded — the exact
/// shape of the r43 batch-A gap where only `types.proto` and
/// `admin_types.proto` had tripwires while the other six did not.
///
/// **Adding a new proto file:** add a line to the
/// `field_snapshot_tests!` invocation above, then run
/// `cargo test -p rio-proto --test proto_field_presence -- --ignored regenerate_<name>`
/// to generate `proto/<name>.proto.fields`, and commit both. Until you
/// do, this test fails (missing entry) and then the build fails
/// (missing `include_str!` target) — there is no path to merging an
/// untripwired proto.
#[test]
fn every_proto_has_a_snapshot_test() {
    let manifest =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest");
    let proto_dir = Path::new(&manifest).join("proto");

    let on_disk: BTreeSet<String> = std::fs::read_dir(&proto_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", proto_dir.display()))
        .map(|e| e.expect("dir entry").file_name())
        .filter_map(|name| {
            // `name.proto`, not `name.proto.fields`.
            name.to_str()?.strip_suffix(".proto").map(str::to_owned)
        })
        .collect();

    let registered: BTreeSet<String> = KNOWN_PROTOS.iter().map(|s| (*s).to_owned()).collect();

    let unregistered: Vec<_> = on_disk.difference(&registered).collect();
    let stale: Vec<_> = registered.difference(&on_disk).collect();

    assert!(
        unregistered.is_empty() && stale.is_empty(),
        "\n  proto/*.proto and the field_snapshot_tests! list have drifted.\n\n  \
         proto files with NO `.fields` snapshot tripwire (a field \
         retype/renumber in these would NOT fail CI — the r43 bug_011 \
         hazard):\n    {unregistered:?}\n\n  \
         registered names with no matching proto/<name>.proto on disk \
         (stale entry — remove it and `git rm` the orphan .fields \
         file):\n    {stale:?}\n\n  \
         Add or remove the corresponding `<name>_fields_frozen, \
         regenerate_<name> => \"<name>\";` line in field_snapshot_tests! \
         in rio-proto/tests/proto_field_presence.rs, regenerate the \
         snapshot, and commit both."
    );
}

/// Pin the extraction helper's normalisation behaviour: leading
/// modifiers, generics, trailing comments, comment-only lines,
/// `reserved` statements, and enum values.
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
