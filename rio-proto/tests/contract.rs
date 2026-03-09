//! gRPC wire contract tests.
//!
//! These test the BOUNDARY, not behavior: what happens when a client sends
//! malformed/oversized/out-of-order data that the type system permits (proto3
//! has no non-empty-string constraint, no max-repeated constraint). Every
//! `check_bound` and structural validation in the handlers should have a
//! test here proving it actually fires.
//!
//! Tests are standalone (direct trait calls, no network) where possible —
//! we're testing the handler's guard clauses, not tonic's transport.
//!
//! ## What's NOT here
//!
//! - Behavior tests (scheduler dispatches work, store stores bytes, etc.)
//!   — those are in each crate's integration tests.
//! - Tests for bounds already covered elsewhere. Grep first; don't dup.
//! - The PutPath trailer contract tests — those are in
//!   rio-store/tests/grpc_integration.rs (closer to the implementation).

use rio_proto::interceptor;
use tonic::Request;

// ===========================================================================
// Trace propagation metadata roundtrip (rio-proto::interceptor)
// ===========================================================================

/// A traceparent injected into outgoing metadata survives tonic's
/// Request wrapping unmolested. Guards against tonic versions that
/// strip unknown headers (they shouldn't — traceparent is a standard
/// HTTP/2 header — but defense in depth).
#[test]
fn traceparent_survives_request_wrapping() {
    // Register propagator (normally init_tracing does this).
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    // Manually inject a known-good traceparent (we don't need a real
    // tracer here — we're testing that the bytes survive, not that
    // the context is correctly generated).
    let known_traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let mut req: Request<()> = Request::new(());
    req.metadata_mut()
        .insert("traceparent", known_traceparent.parse().unwrap());

    // Read it back via the extractor path link_parent uses internally.
    let got = req
        .metadata()
        .get("traceparent")
        .expect("header should be present")
        .to_str()
        .expect("valid ASCII");
    assert_eq!(got, known_traceparent);
}

/// link_parent on a request WITHOUT traceparent doesn't panic (already
/// covered by the interceptor module's unit test, but this is the
/// contract-test phrasing: "a client that doesn't trace doesn't break
/// the server").
#[test]
fn untraced_request_does_not_break_server_extraction() {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let req: Request<()> = Request::new(());
    // Must not panic. If it did, every grpcurl call would crash the handler.
    interceptor::link_parent(&req);
}

// ===========================================================================
// Proto3 oneof contract: empty msg field
// ===========================================================================

/// Proto3 oneof fields deserialize absent values as `None`. Handlers that
/// `match msg.msg { Some(variant) => ..., None => {} }` must not panic
/// on None. This tests that the generated proto type supports the case;
/// per-handler behavior is tested in each crate.
#[test]
fn put_path_request_empty_oneof_is_none() {
    use rio_proto::types::PutPathRequest;
    let empty = PutPathRequest { msg: None };
    // Structurally valid. A handler receiving this should skip it
    // (the server code does `match msg.msg { ... None => {} }`).
    assert!(empty.msg.is_none());
}

#[test]
fn worker_message_empty_oneof_is_none() {
    use rio_proto::types::WorkerMessage;
    let empty = WorkerMessage { msg: None };
    assert!(empty.msg.is_none());
}

// ===========================================================================
// check_bound contract
// ===========================================================================

/// `check_bound` produces an InvalidArgument with the field name and
/// both numbers in the message. This is the user-facing contract: when
/// a bound trips, the error says WHICH bound and WHAT the actual value
/// was, so the user can decide whether to split their request.
#[test]
fn check_bound_error_message_contract() {
    let err = rio_common::grpc::check_bound("widgets", 101, 100).unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let msg = err.message();
    assert!(msg.contains("widgets"), "should name the field: {msg}");
    assert!(msg.contains("101"), "should show actual count: {msg}");
    assert!(msg.contains("100"), "should show the limit: {msg}");
}

#[test]
fn check_bound_exactly_at_limit_is_ok() {
    // Limit is INCLUSIVE. 100 items with limit=100 is fine; 101 trips.
    // A `>=` vs `>` bug here would reject valid nixpkgs-sized DAGs.
    assert!(rio_common::grpc::check_bound("x", 100, 100).is_ok());
    assert!(rio_common::grpc::check_bound("x", 101, 100).is_err());
}

// ===========================================================================
// Limits are reasonable (compile-time sanity)
// ===========================================================================

/// Limits are reasonable. `const { assert! }` = COMPILE-TIME check —
/// a PR that lowers one of these below the floor won't even build,
/// let alone reach the VM tests. The #[test] wrapper just gives it
/// a visible home in the test output.
#[test]
fn limits_are_reasonable() {
    // MAX_DAG_NODES: nixpkgs stdenv is ~60k derivations. Lower this
    // below ~70k and real-world builds fail with InvalidArgument.
    const {
        assert!(
            rio_common::limits::MAX_DAG_NODES >= 70_000,
            "MAX_DAG_NODES must handle nixpkgs-scale DAGs (~60k + headroom)"
        )
    };

    // NAR_CHUNK_SIZE: bounds per-upload memory. Streaming upload gets
    // peak memory from 8GiB to ~1MiB; a 64MiB chunk size would undo
    // that.
    const {
        assert!(
            rio_proto::client::NAR_CHUNK_SIZE <= 1024 * 1024,
            "NAR_CHUNK_SIZE bloats per-upload memory; streaming upload design depends on small chunks"
        )
    };

    // DEFAULT_MAX_MESSAGE_SIZE: nixpkgs DAG serializes to ~12MB.
    // Lower this and SubmitBuild fails opaquely ("message too large").
    const {
        assert!(
            rio_proto::DEFAULT_MAX_MESSAGE_SIZE >= 16 * 1024 * 1024,
            "DEFAULT_MAX_MESSAGE_SIZE must fit nixpkgs DAG (~12MB + headroom)"
        )
    };
}
