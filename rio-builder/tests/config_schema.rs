//! Snapshot guard for `schema_for!(Config)` — see CLAUDE.md "Config
//! schemas are committed snapshots".
rio_test_support::config_schema_frozen!(rio_builder::config::Config);

/// Tripwire for the netrc gate-out: the capability is deliberately NOT
/// operator-reachable (`fetcher.netrc.delivery-unwired`); the parser
/// and its provenance scoping are gate-level code kept correct under
/// test with no production delivery path. A netrc key landing in
/// `Config` re-blesses the frozen snapshot above and then trips THIS
/// test, whose failure message is the wiring checklist — so the knob
/// cannot land silently or without the full contract.
// r[verify fetcher.netrc.delivery-unwired]
#[test]
fn netrc_stays_unwired() {
    let fixture = include_str!("fixtures/config-schema.json");
    assert!(
        !fixture.to_ascii_lowercase().contains("netrc"),
        "a netrc key reached rio-builder's Config schema. netrc is gated out \
         (fetcher.netrc.delivery-unwired); wiring it is ONE change that must carry: \
         (1) file-path secret delivery per the ca_bundle pattern (mounted secret, \
         never inline config); (2) the SandboxOptions plumbing in executor/mod.rs \
         replacing the hardcoded `netrc: None`; (3) an impl annotation on the \
         producing knob, rewriting fetcher.netrc.delivery-unwired as the delivery \
         contract (it is deliberately uncovered today); (4) the origin-scope, \
         case-fold, and strict-parse tests exercised against the operator-delivered \
         file format. Land all of it in this change, update this tripwire to assert \
         the new shape — or drop the key."
    );
}
