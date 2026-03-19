//! Wire roundtrip tests for proto message defaults.
//!
//! Proto3 scalar fields have implicit defaults (bool → false, int → 0,
//! string → ""). These tests pin that the default survives an
//! encode/decode cycle — i.e., a sender that constructs `::default()`
//! and a receiver that decodes the same bytes agree on the field value.
//! This is the wire-compatibility guarantee for newly added fields:
//! an old sender that doesn't know the field omits it on the wire,
//! and the new receiver reads the proto3 default.

use prost::Message;
use rio_proto::types::HeartbeatRequest;

/// `store_degraded` (field 9) defaults to false through a full
/// encode/decode cycle. Wire-compatibility: an old worker that
/// doesn't know field 9 sends nothing for it; the scheduler reads
/// `false` (healthy). If this ever flips to `true` by default,
/// every legacy worker instantly looks store-degraded.
#[test]
fn heartbeat_request_store_degraded_default_false() {
    let req = HeartbeatRequest::default();
    let bytes = req.encode_to_vec();
    let decoded = HeartbeatRequest::decode(&*bytes).unwrap();
    assert!(!decoded.store_degraded);
}
