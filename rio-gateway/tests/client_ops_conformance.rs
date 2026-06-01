//! Client⇄gateway conformance tests.
//!
//! Drive rio-nix's CLIENT-side daemon-protocol operations
//! (`rio_nix::protocol::client`) against rio-gateway's REAL protocol session
//! (`session::run_protocol`) over an in-memory duplex, with the gateway's own
//! mock gRPC backends (MockStore / MockScheduler).
//!
//! The existing `wire_opcodes/` tests drive hand-written client bytes against
//! the real server; rio-nix's client unit tests drive the real client against
//! hand-written fake servers. These tests close the loop — real client
//! against real server — so any wire disagreement between the two
//! implementations surfaces here instead of in an operator's session.

mod common;

use std::collections::HashMap;

use common::GatewaySession;
use rio_nix::protocol::build::BuildStatus;
use rio_nix::protocol::client::{
    ClientOpError, NarPayload, StoreEntry, client_add_multiple_to_store, client_add_to_store_nar,
    client_build_paths_with_results, client_handshake, client_query_path_info,
    client_query_valid_paths,
};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use rio_test_support::fixtures::make_nar;
use rio_test_support::grpc::SubmitOutcome;
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

/// Store paths used by the conformance tests (32-char nixbase32 hash + name).
const PATH_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-conformance-a";
const PATH_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-conformance-b";
const PATH_LARGE: &str = "/nix/store/dddddddddddddddddddddddddddddddd-conformance-large";
const PATH_MISSING: &str = "/nix/store/11111111111111111111111111111111-conformance-missing";

/// Input-addressed test derivation for the build-op tests. The output is a
/// full, valid store path so the enriched builtOutputs can be asserted
/// byte-for-byte on the client side. Because it IS parseable, the gateway's
/// post-build store verification (gw.opcode.build-results-honest) asks the
/// store about it — tests that expect this root to come back Built must seed
/// [`CONF_DRV_OUT`] into the mock store, mirroring the worker upload that a
/// real Completed derivation implies.
const CONF_DRV_PATH: &str = "/nix/store/00000000000000000000000000000000-conformance.drv";
const CONF_DRV_OUT: &str = "/nix/store/cccccccccccccccccccccccccccccccc-conformance-out";

/// Minimal valid ATerm body for [`CONF_DRV_PATH`] (one output, no inputs).
fn conf_drv_aterm() -> String {
    format!(
        r#"Derive([("out","{CONF_DRV_OUT}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{CONF_DRV_OUT}")])"#
    )
}

/// Second input-addressed test derivation: recorded as FAILING by the
/// scripted scheduler while the sibling root completes.
const CONF_DRV_FAIL_PATH: &str = "/nix/store/22222222222222222222222222222222-conformance-fail.drv";
const CONF_DRV_FAIL_OUT: &str = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-conformance-fail-out";

/// Minimal valid ATerm body for [`CONF_DRV_FAIL_PATH`] (one output, no inputs).
fn conf_fail_drv_aterm() -> String {
    format!(
        r#"Derive([("out","{CONF_DRV_FAIL_OUT}","","")],[],[],"x86_64-linux","/bin/sh",["-c","false"],[("out","{CONF_DRV_FAIL_OUT}")])"#
    )
}

/// Third input-addressed test derivation: recorded as CACHED (fetched from a
/// substituter) by the scripted scheduler while a sibling root fails. Its
/// `Substituted` status is what discriminates per-root reporting from
/// DAG-result cloning at the STATUS level: a cloned DAG failure would be
/// store-promoted to `Built`, never `Substituted`.
const CONF_DRV_CACHED_PATH: &str =
    "/nix/store/33333333333333333333333333333333-conformance-cached.drv";
const CONF_DRV_CACHED_OUT: &str =
    "/nix/store/ffffffffffffffffffffffffffffffff-conformance-cached-out";

/// Minimal valid ATerm body for [`CONF_DRV_CACHED_PATH`] (one output, no inputs).
fn conf_cached_drv_aterm() -> String {
    format!(
        r#"Derive([("out","{CONF_DRV_CACHED_OUT}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{CONF_DRV_CACHED_OUT}")])"#
    )
}

/// Interior (non-root) derivation: the scheduler emits a terminal event for
/// it, but the client never requests it, so it must not influence any
/// requested root's result row.
const CONF_DRV_DEP_PATH: &str = "/nix/store/44444444444444444444444444444444-conformance-dep.drv";

/// Take the session's client-side duplex stream, split it, and run the REAL
/// client handshake ([`client_handshake`]) against the gateway. Returns the
/// read/write halves plus the negotiated protocol version.
async fn handshake_session(
    sess: &mut GatewaySession,
) -> anyhow::Result<(ReadHalf<DuplexStream>, WriteHalf<DuplexStream>, u64)> {
    let stream = std::mem::replace(&mut sess.stream, tokio::io::duplex(1).0);
    let (mut rd, mut wr) = tokio::io::split(stream);
    let handshake = client_handshake(&mut rd, &mut wr).await?;
    Ok((rd, wr, handshake.negotiated_version()))
}

/// Build a [`StoreEntry`] (in-memory [`NarPayload::Bytes`]) for a single-file
/// NAR containing `content`. Returns the entry plus the NAR bytes and hash
/// for mock-side assertions.
fn bytes_entry(store_path: &str, content: &[u8]) -> (StoreEntry, Vec<u8>, [u8; 32]) {
    let (nar, hash) = make_nar(content);
    let entry = StoreEntry {
        store_path: store_path.to_string(),
        info: ValidPathInfo {
            nar_hash: hash.to_vec(),
            nar_size: nar.len() as u64,
            ..Default::default()
        },
        nar: NarPayload::Bytes(nar.clone()),
    };
    (entry, nar, hash)
}

/// Drop the client stream halves (EOF on the gateway side) and wait for the
/// session task to exit cleanly.
async fn finish(
    sess: &mut GatewaySession,
    rd: ReadHalf<DuplexStream>,
    wr: WriteHalf<DuplexStream>,
) {
    drop(rd);
    drop(wr);
    sess.join_server().await;
}

// r[verify gw.opcode.query-valid-paths]
/// `client_query_valid_paths` against the real gateway: of two queried paths
/// only the seeded one is valid; the returned set contains exactly that one.
#[tokio::test]
async fn conformance_query_valid_paths() -> anyhow::Result<()> {
    let mut sess = GatewaySession::new().await?;
    sess.store.seed_with_content(PATH_A, b"conformance qvp");
    let (mut rd, mut wr, _version) = handshake_session(&mut sess).await?;

    let valid = client_query_valid_paths(&mut rd, &mut wr, &[PATH_A, PATH_MISSING], false).await?;

    assert_eq!(
        valid.len(),
        1,
        "exactly one queried path is valid: {valid:?}"
    );
    assert!(
        valid.contains(PATH_A),
        "the seeded path should be reported valid, got: {valid:?}"
    );

    finish(&mut sess, rd, wr).await;
    Ok(())
}

// r[verify gw.opcode.query-path-info]
/// `client_query_path_info` against the real gateway: a present path returns
/// `Some(info)` whose nar_size/nar_hash match what the mock store reports; an
/// absent path returns `None`.
#[tokio::test]
async fn conformance_query_path_info() -> anyhow::Result<()> {
    let mut sess = GatewaySession::new().await?;
    let (nar, hash) = sess.store.seed_with_content(PATH_A, b"conformance qpi");
    let (mut rd, mut wr, _version) = handshake_session(&mut sess).await?;

    let info = client_query_path_info(&mut rd, &mut wr, PATH_A)
        .await?
        .expect("seeded path should return Some(info)");
    assert_eq!(
        info.nar_size,
        nar.len() as u64,
        "nar_size should match the mock's NAR"
    );
    assert_eq!(
        info.nar_hash,
        hash.to_vec(),
        "nar_hash should match the mock's NAR hash"
    );

    let missing = client_query_path_info(&mut rd, &mut wr, PATH_MISSING).await?;
    assert!(
        missing.is_none(),
        "absent path should return None, got: {missing:?}"
    );

    finish(&mut sess, rd, wr).await;
    Ok(())
}

// r[verify gw.opcode.add-multiple.batch+2]
/// `client_add_multiple_to_store` against the real gateway: two small entries
/// (`NarPayload::Bytes`) upload successfully, the mock store records one
/// PutPath per entry with the declared path/size/hash, and a follow-up
/// `client_query_valid_paths` sees both paths as valid.
#[tokio::test]
async fn conformance_add_multiple_then_observe() -> anyhow::Result<()> {
    let mut sess = GatewaySession::new().await?;
    let (mut rd, mut wr, _version) = handshake_session(&mut sess).await?;

    let (entry_a, nar_a, hash_a) = bytes_entry(PATH_A, b"conformance multi a");
    let (entry_b, nar_b, hash_b) = bytes_entry(PATH_B, b"conformance multi b");

    client_add_multiple_to_store(&mut rd, &mut wr, false, true, vec![entry_a, entry_b]).await?;

    // Mock-side observation: one PutPath per entry, with the declared store
    // path, NAR size, and NAR hash (the mock verifies the trailer hash
    // against the bytes it received, so a framing bug cannot pass here).
    let calls = sess.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 2, "store should receive 2 PutPath calls");
    let by_path: HashMap<&str, _> = calls.iter().map(|c| (c.store_path.as_str(), c)).collect();
    let call_a = by_path.get(PATH_A).expect("PutPath recorded for entry A");
    assert_eq!(call_a.nar_size, nar_a.len() as u64);
    assert_eq!(call_a.nar_hash, hash_a.to_vec());
    let call_b = by_path.get(PATH_B).expect("PutPath recorded for entry B");
    assert_eq!(call_b.nar_size, nar_b.len() as u64);
    assert_eq!(call_b.nar_hash, hash_b.to_vec());

    // The uploaded paths are now valid through the same session.
    let valid = client_query_valid_paths(&mut rd, &mut wr, &[PATH_A, PATH_B], false).await?;
    assert!(
        valid.contains(PATH_A) && valid.contains(PATH_B),
        "uploaded paths should now be valid, got: {valid:?}"
    );

    finish(&mut sess, rd, wr).await;
    Ok(())
}

// r[verify gw.opcode.add-to-store-nar.framing+2]
/// `client_add_to_store_nar` with a several-hundred-KiB `NarPayload::Reader`
/// against the real gateway: the framed payload spans multiple 256 KiB
/// frames, the upload succeeds, and the mock store observes the full-size
/// ingest.
#[tokio::test]
async fn conformance_add_to_store_nar_large_streams() -> anyhow::Result<()> {
    let mut sess = GatewaySession::new().await?;
    let (mut rd, mut wr, _version) = handshake_session(&mut sess).await?;

    // ~600 KiB of content → a NAR larger than two 256 KiB frame chunks, so
    // the REAL gateway framed reader sees at least three data frames.
    let content = vec![0x5Au8; 600 * 1024];
    let (nar, hash) = make_nar(&content);
    let entry = StoreEntry {
        store_path: PATH_LARGE.to_string(),
        info: ValidPathInfo {
            nar_hash: hash.to_vec(),
            nar_size: nar.len() as u64,
            ..Default::default()
        },
        nar: NarPayload::Reader {
            len: nar.len() as u64,
            reader: Box::new(std::io::Cursor::new(nar.clone())),
        },
    };

    client_add_to_store_nar(&mut rd, &mut wr, entry, false, true).await?;

    let calls = sess.store.calls.put_calls.read().unwrap().clone();
    assert_eq!(calls.len(), 1, "store should receive one PutPath call");
    assert_eq!(calls[0].store_path, PATH_LARGE);
    assert_eq!(calls[0].nar_size, nar.len() as u64);
    assert_eq!(calls[0].nar_hash, hash.to_vec());

    finish(&mut sess, rd, wr).await;
    Ok(())
}

// r[verify gw.opcode.build-paths-with-results+2]
/// `client_build_paths_with_results` against the real gateway with the mock
/// scheduler completing the build: one keyed result, the echoed derived path
/// equals the submission, the status is the success status the mock
/// produces, and builtOutputs come back as full `/nix/store/...` paths.
#[tokio::test]
async fn conformance_build_paths_with_results() -> anyhow::Result<()> {
    let mut sess = GatewaySession::new().await?;
    sess.scheduler
        .set_submit_outcome(SubmitOutcome::completed());
    sess.store
        .seed_with_content(CONF_DRV_PATH, conf_drv_aterm().as_bytes());
    // The output the completed build implies: the gateway verifies it
    // against the store before reporting Built, so the mock store must
    // hold it (a real worker would have uploaded it before Completed).
    sess.store
        .seed_with_content(CONF_DRV_OUT, b"conformance out");
    let (mut rd, mut wr, version) = handshake_session(&mut sess).await?;

    let derived_path = format!("{CONF_DRV_PATH}!out");
    let results =
        client_build_paths_with_results(&mut rd, &mut wr, &[derived_path.as_str()], version)
            .await?;

    assert_eq!(results.len(), 1, "one derived path submitted, one result");
    assert_eq!(
        results[0].derived_path, derived_path,
        "gateway should echo the submitted derived path"
    );
    assert_eq!(
        results[0].result.status,
        BuildStatus::Built,
        "mock scheduler completed the build; error_msg: {:?}",
        results[0].result.error_msg
    );
    assert!(results[0].result.status.is_success());

    // builtOutputs carry full store paths client-side (the wire carries the
    // basename; the client re-attaches the /nix/store/ prefix).
    let outputs = &results[0].result.built_outputs;
    assert_eq!(
        outputs.len(),
        1,
        "the drv's single output should be reported, got: {outputs:?}"
    );
    assert!(
        outputs[0].out_path.starts_with("/nix/store/"),
        "built output must be a full store path, got: {}",
        outputs[0].out_path
    );
    assert_eq!(outputs[0].out_path, CONF_DRV_OUT);
    assert!(
        outputs[0].drv_output_id.starts_with("sha256:")
            && outputs[0].drv_output_id.ends_with("!out"),
        "DrvOutput id should be sha256:<drv-hash>!<output>, got: {}",
        outputs[0].drv_output_id
    );

    // Mock-side observation: the gateway actually submitted the build.
    let submits = sess.scheduler.submit_calls.read().unwrap().clone();
    assert_eq!(submits.len(), 1, "scheduler should receive one SubmitBuild");

    finish(&mut sess, rd, wr).await;
    Ok(())
}

// r[verify gw.opcode.build-paths-with-results+2]
/// Multi-root submission with one failing root: the gateway must report
/// each root's own terminal status — the completed root comes back Built
/// (with its builtOutputs), the failed root comes back with its own
/// failure status and error message, the cached root comes back
/// Substituted — instead of cloning the DAG-level failure onto every root.
///
/// The fixture and assertions are designed so that DAG-result cloning
/// CANNOT pass, on two independent axes:
///
/// - **Message**: the failing root's `error_msg` is asserted EQUAL to the
///   drv event's own message. The DAG-level `BuildFailed` message wraps it
///   (`derivation '…' failed: <msg>`), so a clone-fallback satisfies a
///   `contains` check but never equality.
/// - **Status**: the cached root's own terminal is `Substituted`. A cloned
///   DAG failure on that root would be store-promoted to `Built` (its
///   output IS present), so anything but `Substituted` here means the
///   root's recorded terminal was not consulted.
///
/// The event stream also carries a terminal for an interior derivation the
/// client never requested ([`CONF_DRV_DEP_PATH`]) with a DIFFERENT error
/// text: the DAG the scheduler reports on is a superset of the requested
/// roots, and only the per-root projection of it may surface in results.
#[tokio::test]
async fn conformance_build_paths_with_results_multi_root_per_root_status() -> anyhow::Result<()> {
    use rio_proto::types;
    let mut sess = GatewaySession::new().await?;
    sess.store
        .seed_with_content(CONF_DRV_PATH, conf_drv_aterm().as_bytes());
    sess.store
        .seed_with_content(CONF_DRV_FAIL_PATH, conf_fail_drv_aterm().as_bytes());
    sess.store
        .seed_with_content(CONF_DRV_CACHED_PATH, conf_cached_drv_aterm().as_bytes());
    // The COMPLETED and CACHED roots' outputs are in the store (a worker
    // upload and a substituter fetch both imply presence). The failing
    // root's output is deliberately absent: the store verification must
    // leave that root's own failure standing (no promotion without
    // positive evidence).
    sess.store
        .seed_with_content(CONF_DRV_OUT, b"conformance out");
    sess.store
        .seed_with_content(CONF_DRV_CACHED_OUT, b"conformance cached out");
    let fail_msg = "builder failed with exit code 2";
    let dep_msg = "interior dependency failed: exit code 1";
    sess.scheduler
        .set_submit_outcome(SubmitOutcome::scripted(vec![
            types::BuildEvent {
                event: Some(types::build_event::Event::Started(types::BuildStarted {
                    total_derivations: 4,
                    cached_derivations: 0,
                })),
                ..Default::default()
            },
            types::BuildEvent {
                event: Some(types::build_event::Event::Derivation(
                    types::DerivationEvent::completed(
                        CONF_DRV_PATH.to_string(),
                        vec![CONF_DRV_OUT.to_string()],
                    ),
                )),
                ..Default::default()
            },
            types::BuildEvent {
                event: Some(types::build_event::Event::Derivation(
                    types::DerivationEvent::cached(
                        CONF_DRV_CACHED_PATH.to_string(),
                        vec![CONF_DRV_CACHED_OUT.to_string()],
                    ),
                )),
                ..Default::default()
            },
            // Interior (unrequested) derivation fails with its OWN message:
            // it must not leak into any requested root's result row.
            types::BuildEvent {
                event: Some(types::build_event::Event::Derivation(
                    types::DerivationEvent::failed(
                        CONF_DRV_DEP_PATH.to_string(),
                        dep_msg.to_string(),
                        types::BuildResultStatus::PermanentFailure,
                    ),
                )),
                ..Default::default()
            },
            types::BuildEvent {
                event: Some(types::build_event::Event::Derivation(
                    types::DerivationEvent::failed(
                        CONF_DRV_FAIL_PATH.to_string(),
                        fail_msg.to_string(),
                        types::BuildResultStatus::PermanentFailure,
                    ),
                )),
                ..Default::default()
            },
            types::BuildEvent {
                event: Some(types::build_event::Event::Failed(types::BuildFailed {
                    error_message: format!("derivation '{CONF_DRV_FAIL_PATH}' failed: {fail_msg}"),
                    failed_derivation: CONF_DRV_FAIL_PATH.to_string(),
                    status: types::BuildResultStatus::PermanentFailure as i32,
                })),
                ..Default::default()
            },
        ]));
    let (mut rd, mut wr, version) = handshake_session(&mut sess).await?;

    let ok_path = format!("{CONF_DRV_PATH}!out");
    let fail_path = format!("{CONF_DRV_FAIL_PATH}!out");
    let cached_path = format!("{CONF_DRV_CACHED_PATH}!out");
    let results = client_build_paths_with_results(
        &mut rd,
        &mut wr,
        &[ok_path.as_str(), fail_path.as_str(), cached_path.as_str()],
        version,
    )
    .await?;

    assert_eq!(results.len(), 3, "one result per requested root, in order");
    assert_eq!(results[0].derived_path, ok_path);
    assert_eq!(results[1].derived_path, fail_path);
    assert_eq!(results[2].derived_path, cached_path);
    // Per-root fidelity: the completed sibling is NOT dragged down by the
    // failing root.
    assert_eq!(
        results[0].result.status,
        BuildStatus::Built,
        "completed root must be Built, got {:?} ({})",
        results[0].result.status,
        results[0].result.error_msg
    );
    assert_eq!(results[0].result.built_outputs.len(), 1);
    assert_eq!(results[0].result.built_outputs[0].out_path, CONF_DRV_OUT);
    assert!(
        results[0].result.error_msg.is_empty(),
        "completed root must not carry an error message, got: {}",
        results[0].result.error_msg
    );
    // The failing root carries its own status and its own message,
    // VERBATIM. Equality is the discriminating assertion: the DAG-level
    // wrapper ("derivation '…' failed: <msg>") contains the message but is
    // not equal to it, so a regression to DAG-result cloning fails here.
    assert_eq!(results[1].result.status, BuildStatus::PermanentFailure);
    assert_eq!(
        results[1].result.error_msg, fail_msg,
        "failing root must carry its OWN drv event message verbatim, not \
         the DAG-level wrapper or another derivation's message"
    );
    assert!(
        results[1].result.built_outputs.is_empty(),
        "failing root must not report built outputs, got: {:?}",
        results[1].result.built_outputs
    );
    // The cached root reports its own terminal at the STATUS level: a
    // cloned DAG failure would be store-promoted to Built (the output is
    // present), never Substituted.
    assert_eq!(
        results[2].result.status,
        BuildStatus::Substituted,
        "cached root must report its own Substituted terminal, got {:?} ({})",
        results[2].result.status,
        results[2].result.error_msg
    );
    assert_eq!(results[2].result.built_outputs.len(), 1);
    assert_eq!(
        results[2].result.built_outputs[0].out_path,
        CONF_DRV_CACHED_OUT
    );
    assert!(
        results[2].result.error_msg.is_empty(),
        "cached root must not carry an error message, got: {}",
        results[2].result.error_msg
    );

    finish(&mut sess, rd, wr).await;
    Ok(())
}

/// A daemon-side refusal surfaces as the typed `ClientOpError::Daemon`, not
/// a wire error. Uses the per-tenant store-quota gate: the gateway rejects
/// the build submission with STDERR_ERROR before contacting the scheduler
/// (and before any payload bytes follow the request), so `Daemon` is
/// deterministic — there is no teardown race with payload writing.
#[tokio::test]
async fn conformance_daemon_refusal_is_typed() -> anyhow::Result<()> {
    let mut sess = GatewaySession::new_with_tenant("team-conformance").await?;
    // 200 MiB used vs 100 MiB limit → the quota gate refuses the submission.
    sess.store.state.tenant_quotas.write().unwrap().insert(
        "team-conformance".to_string(),
        (200 * 1024 * 1024, Some(100 * 1024 * 1024)),
    );
    sess.store
        .seed_with_content(CONF_DRV_PATH, conf_drv_aterm().as_bytes());
    let (mut rd, mut wr, version) = handshake_session(&mut sess).await?;

    let derived_path = format!("{CONF_DRV_PATH}!out");
    let err = client_build_paths_with_results(&mut rd, &mut wr, &[derived_path.as_str()], version)
        .await
        .expect_err("over-quota build submission must be refused");

    match err {
        ClientOpError::Daemon(e) => assert!(
            e.message.contains("over store quota"),
            "daemon refusal should carry the quota message, got: {}",
            e.message
        ),
        other => panic!("expected ClientOpError::Daemon, got: {other:?}"),
    }

    // The refusal happened at the gateway: the scheduler never saw it.
    assert!(
        sess.scheduler.submit_calls.read().unwrap().is_empty(),
        "quota refusal must be pre-SubmitBuild"
    );

    finish(&mut sess, rd, wr).await;
    Ok(())
}
