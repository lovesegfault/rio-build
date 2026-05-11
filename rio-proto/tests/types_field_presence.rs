//! `.fields` snapshot tripwire for `types.proto`.
//!
//! Field-set snapshot for `types.proto` messages on the rolling-upgrade
//! boundary (builder↔store, scheduler↔store). Any change to the snapshot
//! forces an explicit decision on wire compatibility — `reserved`
//! tombstone for retypes/removals, additive only for new fields.
//!
//! `admin_types.proto` has its own snapshot in `field_presence.rs`.
//!
//! The source-text golden snapshot pins `<type> <name> = <N>;` including
//! the type keyword, so it catches **all** retypes — including the
//! same-field-number same-wire-type case (`string` → `bytes`, both
//! wire-type 2) and the same-field-number cross-wire-type case
//! (`double factor = 3` → `string factor_json = 3`, the r43 bug_011
//! deploy hazard this guards against).
//!
//! See `docs/src/crate-structure.md` §rio-proto for the rule this guards.

mod common;
use common::extract_fields;

/// `types.proto` field set matches the checked-in snapshot.
///
/// **Adding/changing a field:** the test fails listing the diff. For
/// each NEW or RETYPED scalar field, decide: does the consumer's
/// behaviour differ between "field absent" and "field = proto3
/// zero-value"? If yes, declare it `optional` and add a back-compat
/// decode test (decode a byte-slice WITHOUT the new tag, assert the
/// consumer behaves as it did pre-addition). For a RETYPED or REMOVED
/// field, `reserved N; reserved "name";` the old number — never reuse
/// a field number on a new type (cross-wire-type fails the whole
/// message decode; same-wire-type silently decodes the wrong value).
/// Then regenerate the snapshot:
///
/// ```sh
/// cargo test -p rio-proto --test types_field_presence -- --ignored regenerate_types
/// ```
///
/// and commit `rio-proto/proto/types.proto.fields` alongside.
#[test]
fn types_fields_frozen() {
    const PROTO: &str = include_str!("../proto/types.proto");
    const SNAPSHOT: &str = include_str!("../proto/types.proto.fields");

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
        "\n  types.proto field set changed — for each new/retyped \
         scalar field, decide whether absence is distinguishable from \
         zero-value on the consumer side (if yes: `optional` + a \
         back-compat decode test). For a RETYPED or REMOVED field, \
         `reserved` the old number and name — never reuse a field \
         number on a new type (r43 bug_011). Then regenerate the \
         snapshot:\n    \
         cargo test -p rio-proto --test types_field_presence -- --ignored regenerate_types\n  \
         and commit rio-proto/proto/types.proto.fields.\n\n  \
         diff (- pinned, + live):\n{diff}"
    );
}

/// Regenerate `types.proto.fields` from the current proto.
/// `#[ignore]` so it never runs in CI; invoke explicitly via the
/// command in [`types_fields_frozen`]'s doc.
#[test]
#[ignore = "regenerator, not a test — run with `-- --ignored regenerate_types`"]
fn regenerate_types() {
    let proto = include_str!("../proto/types.proto");
    let fields = extract_fields(proto);
    let out = format!("{}\n", fields.join("\n"));
    let path = format!("{}/proto/types.proto.fields", env!("CARGO_MANIFEST_DIR"));
    std::fs::write(&path, out).unwrap();
    eprintln!("wrote {} fields to {path}", fields.len());
}
