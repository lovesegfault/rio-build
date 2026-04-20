//! CA data model end-to-end: query a realisation via the Nix wire
//! protocol AND via gRPC.
// r[verify gw.opcode.mandatory-set]
//!
//! Flow under test:
//!   1. Upload a path (the CA build output). Known nar_hash.
//!   2. Seed a realisation directly in MockStore (in production: the
//!      scheduler writes via direct-PG `insert_realisation_batch` at
//!      build-completion — `wopRegisterDrvOutput` is rejected, see
//!      `r[store.realisation.register]`).
//!   3. wopQueryRealisation — gets outPath back. This is the
//!      cache-hit lookup.
//!   4. QueryRealisation via gRPC — proves the store-side record
//!      matches what the wire layer surfaces.
//!
//! All against MockStore (in-memory). The wire protocol path is real
//! (gateway session with DuplexStream); the store is mocked to keep
//! the test PG-free.

mod common;

use rio_nix::protocol::wire;
use rio_test_support::fixtures::{make_nar, make_path_info};
use rio_test_support::wire::{do_handshake, drain_stderr_until_last, send_set_options};
use rio_test_support::wire_send;

/// Read-side roundtrip: store-seeded realisation → wopQueryRealisation
/// → gRPC QueryRealisation. The write-side (`wopRegisterDrvOutput`) is
/// rejected at the gateway (covered by
/// `wire_opcodes::test_register_drv_output_rejected`); production
/// writes go via the scheduler's direct-PG path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_ca_register_query_content_roundtrip() -> anyhow::Result<()> {
    common::init_test_logging();

    let mut sess = common::GatewaySession::new().await?;

    // --- Step 1: Upload the CA output to the store ---
    // This is what a worker does after a successful CA build. The
    // nar_hash is the content identity — same bytes always give the
    // same hash, regardless of the input-addressed store path.
    // output_path: internal (gRPC/PG) repr — full /nix/store/ path.
    // output_basename: wire repr — what real nix clients send in
    //   Realisation JSON (CppNix StorePath::to_string() omits prefix).
    // The gateway translates between them; these assertions prove it.
    let output_path = "/nix/store/caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca-output";
    let output_basename = "caaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca-output";
    let (nar, nar_hash) = make_nar(b"deterministic CA output bytes");
    sess.store
        .seed(make_path_info(output_path, &nar, nar_hash), nar);

    // --- Protocol setup ---
    let s = &mut sess.stream;
    do_handshake(s).await?;
    send_set_options(s).await?;

    // --- Step 2: seed realisation in MockStore (scheduler-side write) ---
    // wopRegisterDrvOutput is rejected at the gateway (no trusted-user
    // in rio); production realisations are scheduler-written via
    // direct-PG. Seed MockStore directly to model that.
    let drv_hash_hex = "ab".repeat(32); // 64-hex-char modular drv hash
    let drv_output_id = format!("sha256:{drv_hash_hex}!out");
    let drv_hash_bytes = hex::decode(&drv_hash_hex)?;
    sess.store.state.realisations.write().unwrap().insert(
        (drv_hash_bytes.clone(), "out".into()),
        rio_proto::types::Realisation {
            drv_hash: drv_hash_bytes.clone(),
            output_name: "out".into(),
            output_path: output_path.into(),
            output_hash: nar_hash.to_vec(),
            signatures: vec!["test-key:fake-sig-base64".into()],
        },
    );

    // --- Step 3: wopQueryRealisation ---
    // The NEXT build sends this with the same drv_hash. If it gets
    // count=1 + outPath, the build is skipped — that's the CA cache
    // hit. Phase 2c stores the data; Phase 5 wires it into the DAG
    // for early cutoff, but the lookup itself works now.
    wire_send!(s;
        u64: 43,                        // wopQueryRealisation
        string: &drv_output_id,
    );
    drain_stderr_until_last(s).await?;

    let count = wire::read_u64(s).await?;
    assert_eq!(count, 1, "realisation we just registered should be found");

    let json_str = wire::read_string(s).await?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)?;

    // Roundtrip: what we registered is what we get back.
    assert_eq!(parsed["id"], drv_output_id, "id echoes what we sent");
    // Store has the full path; gateway stripped /nix/store/ before the
    // json! serialize. This is the Step-2 (query-direction) assertion.
    assert_eq!(
        parsed["outPath"], output_basename,
        "gateway should strip STORE_PREFIX for wire (THIS is the cache-hit payload)"
    );
    assert_eq!(
        parsed["signatures"][0], "test-key:fake-sig-base64",
        "signatures roundtrip"
    );

    // --- Step 4: QueryRealisation via gRPC ---
    // Same lookup as step 3, but via the gRPC client directly — proves
    // the store-side record matches what the wire layer surfaces, and
    // that the gateway's basename↔full-path translation is the only
    // layer doing path-shape munging.
    let realisation = sess
        .store_client
        .query_realisation(rio_proto::types::QueryRealisationRequest {
            drv_hash: drv_hash_bytes.clone(),
            output_name: "out".into(),
        })
        .await?
        .into_inner();

    assert_eq!(
        realisation.output_path, output_path,
        "gRPC QueryRealisation returns full /nix/store/ path (store-internal repr)"
    );
    assert_eq!(realisation.signatures, vec!["test-key:fake-sig-base64"]);

    Ok(())
}
