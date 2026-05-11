//! Shared helper for the `.fields` snapshot tripwire tests.
//!
//! `proto_field_presence.rs` compares each `proto/*.proto` file's live
//! source against its checked-in `.fields` golden. The extraction logic
//! lives here so every snapshot agrees on what counts as a "field
//! declaration".

/// Normalized field-declaration lines from a `.proto` source.
///
/// Matches any line of the shape `... <name> = <N>;` (optionally with a
/// trailing `// ...` comment), which covers scalar, `optional`,
/// `repeated`, `map<K,V>`, message-typed, and enum-value declarations —
/// all wire-contract-relevant. Normalizes inner whitespace to single
/// spaces so reflowing a field's leading indent doesn't trip the wire.
///
/// `reserved N;` and `reserved "name";` lines are intentionally NOT
/// captured (no `=` sign), so adding a tombstone for a removed field
/// only changes the snapshot by *removing* that field's line — exactly
/// the diff a reviewer wants to see.
pub fn extract_fields(proto: &str) -> Vec<String> {
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
