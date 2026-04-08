//! CA data model end-to-end: register a realisation via the Nix wire
//! protocol, query it back via the wire AND via gRPC.
// r[verify gw.opcode.mandatory-set]
//!
//! This is the Phase 2c CA cache-hit path BEFORE Phase 5 early cutoff:
//! a CA build completes → output uploaded to store → realisation
//! registered → NEXT build with the same modular drv_hash finds the
//! realisation.
//!
//! Flow under test:
//!   1. Upload a path (the CA build output). Known nar_hash.
//!   2. wopRegisterDrvOutput with a Realisation JSON pointing at that
//!      path — gateway → store_client.register_realisation.
//!   3. wopQueryRealisation with the same DrvOutput id — gets outPath
//!      back. This is the cache-hit lookup.
//!   4. QueryRealisation via gRPC directly — proves the store-side
//!      record matches what the wire layer surfaces.
//!
//! All against MockStore (in-memory). The wire protocol path is real
//! (gateway session with DuplexStream); the store is mocked to keep
//! the test PG-free.

mod common;

use rio_nix::protocol::wire;
use rio_test_support::fixtures::{make_nar, make_path_info};
use rio_test_support::wire::{do_handshake, drain_stderr_until_last, send_set_options};
use rio_test_support::wire_send;

/// The full roundtrip: write via wopRegisterDrvOutput, read via
/// wopQueryRealisation, cross-check via the gRPC QueryRealisation. All
/// three hops working together is what makes CA cache-hits real.
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

    // --- Step 2: wopRegisterDrvOutput ---
    // Nix sends this after every CA build. The id is the DrvOutput:
    // `sha256:<modular-drv-hash-hex>!<output-name>`. The modular hash
    // depends ONLY on the derivation's fixed attributes — two builds
    // with identical inputs produce the same hash even with different
    // output paths. That's what makes this useful.
    let drv_hash_hex = "ab".repeat(32); // 64-hex-char modular drv hash
    let drv_output_id = format!("sha256:{drv_hash_hex}!out");
    let realisation_json = serde_json::json!({
        "id": drv_output_id,
        "outPath": output_basename,  // wire format: basename, NOT /nix/store/...
        "signatures": ["test-key:fake-sig-base64"],
        "dependentRealisations": {}
    })
    .to_string();

    wire_send!(s;
        u64: 42,                        // wopRegisterDrvOutput
        string: &realisation_json,
    );
    drain_stderr_until_last(s).await?;
    // No result data after STDERR_LAST for RegisterDrvOutput.

    // Verify MockStore recorded it (shows the gRPC call went through,
    // not just the wire parse). Gateway parsed JSON → gRPC → store.
    let drv_hash_bytes = hex::decode(&drv_hash_hex)?;
    {
        let realisations = sess.store.realisations.read().unwrap();
        let stored = realisations
            .get(&(drv_hash_bytes.clone(), "out".into()))
            .expect("realisation should be stored via gRPC");
        // Sent basename on the wire; gateway prepended /nix/store/ before
        // the gRPC call. This is the Step-1 (register-direction) assertion.
        assert_eq!(
            stored.output_path, output_path,
            "gateway should prepend STORE_PREFIX"
        );
        assert_eq!(stored.signatures, vec!["test-key:fake-sig-base64"]);
    }

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
