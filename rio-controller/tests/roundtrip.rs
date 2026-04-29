//! Proto back-compat decode tests for messages this crate consumes.
//!
//! Per `docs/src/crate-structure.md` §rio-proto field-addition rule:
//! every `optional` proto3 scalar whose absence is load-bearing on the
//! consumer side gets a case here that decodes a byte-slice WITHOUT the
//! new tag and asserts the consumer's behaviour matches the
//! pre-addition behaviour. The `.fields` snapshot tripwire
//! (`rio-proto/tests/field_presence.rs`) forces a stop here on every
//! `admin_types.proto` field-set change.

use prost::Message;
use rio_proto::types::SpawnIntent;

/// `SpawnIntent.ready` (field 13, `optional bool`): a pre-§13a
/// scheduler doesn't know the field and omits tag 13 on the wire. That
/// MUST decode as `None` (not `Some(false)`), and the controller's
/// §13a Ready-filter (`jobs.rs` `intents.retain`) MUST keep it —
/// pre-§13a schedulers only ever emitted Ready-loop intents, so absent
/// ⇒ Ready is provably the old behaviour. `unwrap_or(false)` would
/// drop everything during a new-controller/old-scheduler skew window
/// (bug_001).
#[test]
fn spawn_intent_ready_absent_is_legacy_ready() {
    // Encode with `ready: None` — prost omits an `optional` field with
    // no value, so the wire bytes contain no tag-13. This is exactly
    // what a pre-§13a scheduler sends.
    let legacy = SpawnIntent {
        intent_id: "legacy".into(),
        cores: 4,
        ready: None,
        ..Default::default()
    };
    let bytes = legacy.encode_to_vec();

    // Tag 13 wire key is `(13 << 3) | 0` = 0x68 (varint wire-type).
    // Assert it's absent — if prost ever starts encoding `None` as an
    // explicit default this test catches it before the consumer breaks.
    assert!(
        !bytes.contains(&0x68),
        "ready=None must not emit tag 13 on the wire; got {bytes:02x?}"
    );

    // Decode: absent tag → `None`, NOT `Some(false)`.
    let decoded = SpawnIntent::decode(&*bytes).unwrap();
    assert_eq!(decoded.ready, None);

    // Consumer behaviour: the §13a Ready-filter keeps it.
    let mut intents = vec![decoded];
    intents.retain(|i| i.ready.unwrap_or(true));
    assert_eq!(intents.len(), 1, "legacy (ready=None) intent was filtered");
    assert_eq!(intents[0].intent_id, "legacy");

    // And the explicit-false case is still dropped (the filter isn't
    // vacuous).
    let mut explicit = vec![SpawnIntent {
        ready: Some(false),
        ..Default::default()
    }];
    explicit.retain(|i| i.ready.unwrap_or(true));
    assert!(explicit.is_empty());
}

/// `disk_headroom_factor` (field 15) absent on the wire — a pre-ADR-023
/// §sizing scheduler — decodes as `None`, NOT `Some(0.0)`. The
/// consumer (`pool::jobs::intent_headroom`) treats `None`/`0.0` as the
/// flat 1.5× fallback; treating absence as a present 0.0 would zero
/// the overlay sizeLimit. Consumer behaviour is unit-tested in
/// `jobs::tests::disk_headroom_factor_widens_ephemeral_request`.
#[test]
fn spawn_intent_disk_headroom_absent_decodes_none() {
    let legacy = SpawnIntent {
        disk_headroom_factor: None,
        ..Default::default()
    };
    let bytes = legacy.encode_to_vec();
    // Tag 15 wire key is `(15 << 3) | 1` = 0x79 (fixed64 wire-type).
    // Empty-default intent encodes zero bytes, so the contains check
    // is exact (no string/varint payload to alias 0x79).
    assert!(
        !bytes.contains(&0x79),
        "disk_headroom_factor=None must not emit tag 15; got {bytes:02x?}"
    );
    let decoded = SpawnIntent::decode(&*bytes).unwrap();
    assert_eq!(decoded.disk_headroom_factor, None);
}
