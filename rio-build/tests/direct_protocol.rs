//! Direct protocol tests — feed raw bytes through a DuplexStream without SSH.
//! Tests each opcode at the byte level.

use std::sync::Arc;

use rio_build::gateway::session::run_protocol;
use rio_build::store::{MemoryStore, Store};
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::nar::{self, NarNode};
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::{STDERR_ERROR, STDERR_LAST, STDERR_READ};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWriteExt, DuplexStream};

/// Perform the full handshake + wopSetOptions on a client stream.
async fn do_handshake(s: &mut DuplexStream) {
    wire::write_u64(s, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(s, PROTOCOL_VERSION).await.unwrap();
    s.flush().await.unwrap();

    let magic2 = wire::read_u64(s).await.unwrap();
    assert_eq!(magic2, WORKER_MAGIC_2);
    let _server_version = wire::read_u64(s).await.unwrap();

    let features: Vec<String> = vec![];
    wire::write_strings(s, &features).await.unwrap();
    s.flush().await.unwrap();
    let _server_features = wire::read_strings(s).await.unwrap();

    wire::write_u64(s, 0).await.unwrap();
    wire::write_u64(s, 0).await.unwrap();
    s.flush().await.unwrap();

    let _version = wire::read_string(s).await.unwrap();
    let _trusted = wire::read_u64(s).await.unwrap();
    let last = wire::read_u64(s).await.unwrap();
    assert_eq!(last, STDERR_LAST);

    wire::write_u64(s, 19).await.unwrap();
    for _ in 0..12 {
        wire::write_u64(s, 0).await.unwrap();
    }
    wire::write_u64(s, 0).await.unwrap();
    s.flush().await.unwrap();

    let msg = wire::read_u64(s).await.unwrap();
    assert_eq!(msg, STDERR_LAST);
}

fn make_test_store() -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let path =
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1").unwrap();
    store.insert(
        rio_build::store::PathInfoBuilder::new(
            path,
            NixHash::compute(HashAlgo::SHA256, b"fake nar"),
            12345,
        )
        .registration_time(1700000000)
        .ultimate(true)
        .build()
        .unwrap(),
        Some(b"fake nar content".to_vec()),
    );
    store
}

async fn run_test(
    store: Arc<MemoryStore>,
    client_fn: impl FnOnce(DuplexStream) -> tokio::task::JoinHandle<()>,
) {
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(server_stream);
        let _ = run_protocol(&mut r, &mut w, store.as_ref()).await;
    });
    let client = client_fn(client_stream);
    let (c, s) = tokio::join!(client, server);
    c.unwrap();
    s.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_handshake_direct() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rio_build=debug,rio_nix=debug")
        .with_test_writer()
        .try_init();

    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_is_valid_path_found() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 1).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);
            let valid = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(valid, 1, "expected path to be valid");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_is_valid_path_not_found() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 1).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);
            let valid = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(valid, 0, "expected path to be invalid");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_path_info() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 26).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);

            let valid = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(valid, 1);

            let deriver = wire::read_string(&mut s).await.unwrap();
            assert!(deriver.is_empty());

            let nar_hash = wire::read_string(&mut s).await.unwrap();
            assert_eq!(
                nar_hash.len(),
                64,
                "expected 64-char hex hash, got: {nar_hash}"
            );

            let refs = wire::read_strings(&mut s).await.unwrap();
            assert!(refs.is_empty());

            let reg_time = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(reg_time, 1700000000);

            let nar_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(nar_size, 12345);

            let ultimate = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(ultimate, 1);

            let sigs = wire::read_strings(&mut s).await.unwrap();
            assert!(sigs.is_empty());

            let ca = wire::read_string(&mut s).await.unwrap();
            assert!(ca.is_empty());
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_path_info_not_found() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 26).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);

            let valid = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(valid, 0, "expected path not found");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_valid_paths() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 31).await.unwrap();
            let paths = vec![
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1".to_string(),
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0".to_string(),
            ];
            wire::write_strings(&mut s, &paths).await.unwrap();
            wire::write_u64(&mut s, 0).await.unwrap(); // substitute flag
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);

            let valid = wire::read_strings(&mut s).await.unwrap();
            assert_eq!(valid.len(), 1);
            assert_eq!(
                valid[0],
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1"
            );
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_add_temp_root() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 11).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);
            let result = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(result, 1);
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nar_from_path() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 38).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            // Read STDERR_WRITE chunks until STDERR_LAST
            let mut nar_data = Vec::new();
            loop {
                let msg = wire::read_u64(&mut s).await.unwrap();
                if msg == STDERR_LAST {
                    break;
                }
                assert_eq!(msg, rio_nix::protocol::stderr::STDERR_WRITE);
                let chunk = wire::read_bytes(&mut s).await.unwrap();
                nar_data.extend_from_slice(&chunk);
            }

            assert_eq!(nar_data, b"fake nar content");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_path_from_hash_part() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopQueryPathFromHashPart (29): send a hash part string
            wire::write_u64(&mut s, 29).await.unwrap();
            wire::write_string(&mut s, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .await
                .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);
            let path = wire::read_string(&mut s).await.unwrap();
            assert!(path.is_empty(), "expected empty string (stub), got: {path}");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_add_signatures() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopAddSignatures (37): send a path + string collection of sigs
            wire::write_u64(&mut s, 37).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            let sigs = vec![
                "cache.example.com:fakesig1".to_string(),
                "cache.example.com:fakesig2".to_string(),
            ];
            wire::write_strings(&mut s, &sigs).await.unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);
            let result = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(result, 1);
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_missing() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopQueryMissing (40): send opaque (non-derivation) paths
            wire::write_u64(&mut s, 40).await.unwrap();
            let paths = vec![
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1".to_string(),
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0".to_string(),
            ];
            wire::write_strings(&mut s, &paths).await.unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);

            // willBuild: empty (opaque paths are never "buildable")
            let will_build = wire::read_strings(&mut s).await.unwrap();
            assert!(
                will_build.is_empty(),
                "opaque paths should not be in willBuild, got: {will_build:?}"
            );

            // willSubstitute: empty
            let will_substitute = wire::read_strings(&mut s).await.unwrap();
            assert!(will_substitute.is_empty(), "expected empty willSubstitute");

            // unknown: the missing opaque path
            let unknown = wire::read_strings(&mut s).await.unwrap();
            assert_eq!(unknown.len(), 1, "missing opaque path should be in unknown");
            assert_eq!(
                unknown[0],
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0"
            );

            // downloadSize: 0
            let download_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(download_size, 0);

            // narSize: 0
            let nar_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(nar_size, 0);
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_set_options_with_overrides() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopSetOptions (19): send with non-zero values and 2 override pairs
            wire::write_u64(&mut s, 19).await.unwrap();
            wire::write_u64(&mut s, 1).await.unwrap(); // keepFailed = true
            wire::write_u64(&mut s, 1).await.unwrap(); // keepGoing = true
            wire::write_u64(&mut s, 0).await.unwrap(); // tryFallback = false
            wire::write_u64(&mut s, 2).await.unwrap(); // verbosity = 2
            wire::write_u64(&mut s, 4).await.unwrap(); // maxBuildJobs = 4
            wire::write_u64(&mut s, 300).await.unwrap(); // maxSilentTime = 300
            wire::write_u64(&mut s, 0).await.unwrap(); // obsolete useBuildHook
            wire::write_u64(&mut s, 1).await.unwrap(); // verboseBuild = true
            wire::write_u64(&mut s, 0).await.unwrap(); // obsolete logType
            wire::write_u64(&mut s, 0).await.unwrap(); // obsolete printBuildTrace
            wire::write_u64(&mut s, 8).await.unwrap(); // buildCores = 8
            wire::write_u64(&mut s, 1).await.unwrap(); // useSubstitutes = true
            // override pairs: count = 2
            wire::write_u64(&mut s, 2).await.unwrap();
            wire::write_string(&mut s, "max-jobs").await.unwrap();
            wire::write_string(&mut s, "16").await.unwrap();
            wire::write_string(&mut s, "cores").await.unwrap();
            wire::write_string(&mut s, "8").await.unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_unknown_opcode_closes_connection() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // Send unknown opcode 999
            wire::write_u64(&mut s, 999).await.unwrap();
            s.flush().await.unwrap();

            // Should receive STDERR_ERROR then connection closes
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(msg, rio_nix::protocol::stderr::STDERR_ERROR);

            // Read the error structure
            let _type = wire::read_string(&mut s).await.unwrap();
            let _level = wire::read_u64(&mut s).await.unwrap();
            let _name = wire::read_string(&mut s).await.unwrap();
            let _message = wire::read_string(&mut s).await.unwrap();
            let _have_pos = wire::read_u64(&mut s).await.unwrap();
            let _trace_count = wire::read_u64(&mut s).await.unwrap();

            // Connection should be closed — next read should EOF
            let result = wire::read_u64(&mut s).await;
            assert!(result.is_err(), "expected EOF after unknown opcode");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nar_from_path_missing() {
    let store = Arc::new(MemoryStore::new()); // empty store
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopNarFromPath (38) for a path NOT in the store
            wire::write_u64(&mut s, 38).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-missing-1.0",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            // Should receive STDERR_ERROR (path is not valid)
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(
                msg,
                rio_nix::protocol::stderr::STDERR_ERROR,
                "expected STDERR_ERROR for missing path"
            );

            // Read and discard the error structure
            let _type = wire::read_string(&mut s).await.unwrap();
            let _level = wire::read_u64(&mut s).await.unwrap();
            let _name = wire::read_string(&mut s).await.unwrap();
            let _message = wire::read_string(&mut s).await.unwrap();
            let _have_pos = wire::read_u64(&mut s).await.unwrap();
            let _trace_count = wire::read_u64(&mut s).await.unwrap();
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nar_from_path_invalid_path_format() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopNarFromPath (38) with a garbage (non-store-path) string
            wire::write_u64(&mut s, 38).await.unwrap();
            wire::write_string(&mut s, "this-is-not-a-store-path")
                .await
                .unwrap();
            s.flush().await.unwrap();

            // Should receive STDERR_ERROR for unparseable path
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(
                msg,
                rio_nix::protocol::stderr::STDERR_ERROR,
                "expected STDERR_ERROR for garbage path"
            );

            // Read and discard the error structure
            let _type = wire::read_string(&mut s).await.unwrap();
            let _level = wire::read_u64(&mut s).await.unwrap();
            let _name = wire::read_string(&mut s).await.unwrap();
            let message = wire::read_string(&mut s).await.unwrap();
            assert!(
                message.contains("invalid store path"),
                "error message should mention invalid store path, got: {message}"
            );
            let _have_pos = wire::read_u64(&mut s).await.unwrap();
            let _trace_count = wire::read_u64(&mut s).await.unwrap();
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_path_info_invalid_path_format() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopQueryPathInfo (26) with a garbage (non-store-path) string
            wire::write_u64(&mut s, 26).await.unwrap();
            wire::write_string(&mut s, "this-is-not-a-store-path")
                .await
                .unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);

            // Should return valid=false for unparseable paths
            let valid = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(valid, 0, "expected invalid for garbage path");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_multi_opcode_sequence() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // Op 1: IsValidPath (found)
            wire::write_u64(&mut s, 1).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), STDERR_LAST);
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), 1);

            // Op 2: IsValidPath (not found)
            wire::write_u64(&mut s, 1).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), STDERR_LAST);
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), 0);

            // Op 3: AddTempRoot
            wire::write_u64(&mut s, 11).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), STDERR_LAST);
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), 1);

            // Op 4: QueryPathFromHashPart (stub)
            wire::write_u64(&mut s, 29).await.unwrap();
            wire::write_string(&mut s, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .await
                .unwrap();
            s.flush().await.unwrap();
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), STDERR_LAST);
            let path = wire::read_string(&mut s).await.unwrap();
            assert!(path.is_empty());

            // Op 5: QueryMissing
            wire::write_u64(&mut s, 40).await.unwrap();
            let paths =
                vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1".to_string()];
            wire::write_strings(&mut s, &paths).await.unwrap();
            s.flush().await.unwrap();
            assert_eq!(wire::read_u64(&mut s).await.unwrap(), STDERR_LAST);
            let will_build = wire::read_strings(&mut s).await.unwrap();
            assert!(
                will_build.is_empty(),
                "existing path should not be in willBuild"
            );
            let will_substitute = wire::read_strings(&mut s).await.unwrap();
            assert!(will_substitute.is_empty(), "expected empty willSubstitute");
            let unknown = wire::read_strings(&mut s).await.unwrap();
            assert!(unknown.is_empty(), "existing path should not be in unknown");
            let download_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(download_size, 0);
            let nar_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(nar_size, 0);
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_query_missing_with_derived_path() {
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopQueryMissing (40) with DerivedPath format "path!*"
            wire::write_u64(&mut s, 40).await.unwrap();
            let paths = vec![
                // Existing path with !* suffix (DerivedPath format)
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1!*".to_string(),
                // Missing path with !out suffix
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0!out".to_string(),
            ];
            wire::write_strings(&mut s, &paths).await.unwrap();
            s.flush().await.unwrap();

            let last = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(last, STDERR_LAST);

            // willBuild: only the missing path (with its original DerivedPath form)
            let will_build = wire::read_strings(&mut s).await.unwrap();
            assert_eq!(
                will_build.len(),
                1,
                "only the missing path should be in willBuild"
            );
            assert_eq!(
                will_build[0],
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0!out"
            );

            // willSubstitute: always empty (rio-build has no substituters)
            let will_substitute = wire::read_strings(&mut s).await.unwrap();
            assert!(will_substitute.is_empty(), "expected empty willSubstitute");
            // unknown: empty (the missing path is a Built derivation, goes to willBuild)
            let unknown = wire::read_strings(&mut s).await.unwrap();
            assert!(unknown.is_empty(), "Built paths should not be in unknown");
            let download_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(download_size, 0);
            let nar_size = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(nar_size, 0);
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nar_from_path_large() {
    // Insert a NAR larger than 64KB to test multi-chunk STDERR_WRITE behavior.
    // The handler chunks at 64KB boundaries (CHUNK_SIZE = 64 * 1024).
    let large_nar = vec![0xAB_u8; 100 * 1024]; // 100KB
    let store = Arc::new(MemoryStore::new());
    let path =
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-large-nar-1.0").unwrap();
    store.insert(
        rio_build::store::PathInfoBuilder::new(
            path,
            NixHash::compute(HashAlgo::SHA256, &large_nar),
            large_nar.len() as u64,
        )
        .registration_time(1700000000)
        .ultimate(true)
        .build()
        .unwrap(),
        Some(large_nar.clone()),
    );

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            wire::write_u64(&mut s, 38).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-large-nar-1.0",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            // Read STDERR_WRITE chunks until STDERR_LAST
            let mut nar_data = Vec::new();
            let mut chunk_count = 0_usize;
            loop {
                let msg = wire::read_u64(&mut s).await.unwrap();
                if msg == STDERR_LAST {
                    break;
                }
                assert_eq!(msg, rio_nix::protocol::stderr::STDERR_WRITE);
                let chunk = wire::read_bytes(&mut s).await.unwrap();
                nar_data.extend_from_slice(&chunk);
                chunk_count += 1;
            }

            // 100KB / 64KB = 2 chunks (64KB + 36KB)
            assert!(
                chunk_count >= 2,
                "expected at least 2 chunks for 100KB NAR, got {chunk_count}"
            );
            assert_eq!(nar_data.len(), 100 * 1024, "reassembled NAR size mismatch");
            assert_eq!(nar_data, large_nar, "reassembled NAR data mismatch");
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nar_from_path_missing_closes_connection() {
    // After NarFromPath sends STDERR_ERROR for a missing path, the connection
    // closes to prevent protocol desynchronization (the client expects NAR
    // data but there is none to send).
    let store = make_test_store();
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // wopNarFromPath (38) for a path NOT in the store
            wire::write_u64(&mut s, 38).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing-1.0",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            // Should receive STDERR_ERROR
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(
                msg,
                rio_nix::protocol::stderr::STDERR_ERROR,
                "expected STDERR_ERROR for missing path"
            );

            // Read and discard the error structure
            let _type = wire::read_string(&mut s).await.unwrap();
            let _level = wire::read_u64(&mut s).await.unwrap();
            let _name = wire::read_string(&mut s).await.unwrap();
            let _message = wire::read_string(&mut s).await.unwrap();
            let _have_pos = wire::read_u64(&mut s).await.unwrap();
            let _trace_count = wire::read_u64(&mut s).await.unwrap();

            // Connection should be closed — reading the next opcode should fail
            let result = wire::read_u64(&mut s).await;
            assert!(
                result.is_err(),
                "connection should close after NarFromPath STDERR_ERROR for missing path"
            );
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_known_unimplemented_opcode_closes_connection() {
    let store = Arc::new(MemoryStore::new());
    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // Send opcode 42 (wopRegisterDrvOutput) — known but not yet implemented
            wire::write_u64(&mut s, 42).await.unwrap();
            s.flush().await.unwrap();

            // Should receive STDERR_ERROR (0x63787470)
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(msg, STDERR_ERROR);

            // Read the error structure
            let error_type = wire::read_string(&mut s).await.unwrap();
            assert_eq!(error_type, "Error");
            let level = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(level, 0);
            let name = wire::read_string(&mut s).await.unwrap();
            assert_eq!(name, "rio-build");
            let message = wire::read_string(&mut s).await.unwrap();
            assert!(
                message.contains("wopRegisterDrvOutput"),
                "expected message to contain 'wopRegisterDrvOutput', got: {message}"
            );
            assert!(
                message.contains("not yet implemented"),
                "expected message to contain 'not yet implemented', got: {message}"
            );
            let _have_pos = wire::read_u64(&mut s).await.unwrap();
            let _trace_count = wire::read_u64(&mut s).await.unwrap();

            // Connection should be closed — next read returns EOF or error
            let result = wire::read_u64(&mut s).await;
            assert!(
                result.is_err(),
                "expected EOF after unimplemented opcode closes connection"
            );
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_version_too_old_sends_stderr_error() {
    let (mut client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let store: Arc<MemoryStore> = Arc::new(MemoryStore::new());

    let server = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(server_stream);
        let _ = run_protocol(&mut r, &mut w, store.as_ref()).await;
    });

    let client = tokio::spawn(async move {
        let s = &mut client_stream;

        // Send WORKER_MAGIC_1
        wire::write_u64(s, WORKER_MAGIC_1).await.unwrap();

        // Read WORKER_MAGIC_2 + server version (consume them)
        let magic2 = wire::read_u64(s).await.unwrap();
        assert_eq!(magic2, WORKER_MAGIC_2);
        let _server_version = wire::read_u64(s).await.unwrap();

        // Send client version 1.32 (too old — minimum is 1.37)
        // Encoded as (1 << 8) | 32 = 0x120
        wire::write_u64(s, 0x120).await.unwrap();
        s.flush().await.unwrap();

        // The server should send STDERR_ERROR with message about version
        let msg = wire::read_u64(s).await.unwrap();
        assert_eq!(msg, STDERR_ERROR, "expected STDERR_ERROR for old version");

        // Read the error structure
        let error_type = wire::read_string(s).await.unwrap();
        assert_eq!(error_type, "Error");
        let level = wire::read_u64(s).await.unwrap();
        assert_eq!(level, 0);
        let name = wire::read_string(s).await.unwrap();
        assert_eq!(name, "rio-build");
        let message = wire::read_string(s).await.unwrap();
        assert!(
            message.contains("1.37+"),
            "expected message to mention '1.37+', got: {message}"
        );
        let _have_pos = wire::read_u64(s).await.unwrap();
        let _trace_count = wire::read_u64(s).await.unwrap();
    });

    let (c, s) = tokio::join!(client, server);
    c.unwrap();
    s.unwrap();
}

// ---------------------------------------------------------------------------
// Phase 1b: store-interaction opcode byte-level tests
// ---------------------------------------------------------------------------

/// Helper: create a NAR archive containing a single regular file with the given contents.
fn make_nar(contents: &[u8]) -> Vec<u8> {
    let node = NarNode::Regular {
        executable: false,
        contents: contents.to_vec(),
    };
    let mut buf = Vec::new();
    nar::serialize(&mut buf, &node).unwrap();
    buf
}

/// Helper: compute the SHA-256 hex digest of data.
///
/// The narHash field on the wire uses plain hex encoding.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Helper: create a simple .drv ATerm string for testing.
///
/// Format: `Derive([outputs],[inputDrvs],[inputSrcs],"platform","builder",[args],[env])`
fn make_test_drv() -> String {
    r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("name","test"),("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-test"),("system","x86_64-linux")])"#.to_string()
}

/// wopAddToStoreNar (39): Send a store path with NAR data, verify it is stored.
#[tokio::test(flavor = "multi_thread")]
async fn test_add_to_store_nar_direct() {
    let store = Arc::new(MemoryStore::new());
    let store2 = store.clone();

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            let nar_data = make_nar(b"hello world");
            let nar_hash = sha256_hex(&nar_data);
            let nar_size = nar_data.len() as u64;
            let path_str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-test-nar";

            // Send wopAddToStoreNar (39)
            wire::write_u64(&mut s, 39).await.unwrap();

            // Metadata fields
            wire::write_string(&mut s, path_str).await.unwrap();
            wire::write_string(&mut s, "").await.unwrap();
            wire::write_string(&mut s, &nar_hash).await.unwrap();
            wire::write_strings(&mut s, &[]).await.unwrap();
            wire::write_u64(&mut s, 0).await.unwrap();
            wire::write_u64(&mut s, nar_size).await.unwrap();
            wire::write_bool(&mut s, true).await.unwrap();
            wire::write_strings(&mut s, &[]).await.unwrap();
            wire::write_string(&mut s, "").await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            s.flush().await.unwrap();

            // Server will send STDERR_READ requests — respond with NAR data chunks
            let mut sent = 0usize;
            loop {
                let msg = wire::read_u64(&mut s).await.unwrap();
                if msg == STDERR_LAST {
                    break;
                }
                if msg == STDERR_ERROR {
                    let _error_type = wire::read_string(&mut s).await.unwrap();
                    let _level = wire::read_u64(&mut s).await.unwrap();
                    let _name = wire::read_string(&mut s).await.unwrap();
                    let message = wire::read_string(&mut s).await.unwrap();
                    panic!("unexpected STDERR_ERROR: {message}");
                }
                assert_eq!(msg, STDERR_READ, "expected STDERR_READ");
                let requested = wire::read_u64(&mut s).await.unwrap() as usize;
                let end = (sent + requested).min(nar_data.len());
                let chunk = &nar_data[sent..end];
                wire::write_bytes(&mut s, chunk).await.unwrap();
                s.flush().await.unwrap();
                sent = end;
            }

            // Read success response
            let result = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(result, 1, "expected success (1)");
        })
    })
    .await;

    // Verify the path was stored
    let path = StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-test-nar").unwrap();
    assert!(store2.is_valid_path(&path).await.unwrap());
}

/// wopAddMultipleToStore (44): Send multiple store paths via framed stream.
#[tokio::test(flavor = "multi_thread")]
async fn test_add_multiple_to_store_direct() {
    let store = Arc::new(MemoryStore::new());
    let store2 = store.clone();

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // Build the inner entry data
            let nar_data = make_nar(b"multi-store content");
            let nar_hash = sha256_hex(&nar_data);
            let nar_size = nar_data.len() as u64;
            let path_str = "/nix/store/cccccccccccccccccccccccccccccccc-multi-test";

            // Serialize one entry into a buffer
            let mut entry_buf = Vec::new();
            wire::write_string(&mut entry_buf, path_str).await.unwrap();
            wire::write_string(&mut entry_buf, "").await.unwrap();
            wire::write_string(&mut entry_buf, &nar_hash).await.unwrap();
            wire::write_strings(&mut entry_buf, &[]).await.unwrap();
            wire::write_u64(&mut entry_buf, 0).await.unwrap();
            wire::write_u64(&mut entry_buf, nar_size).await.unwrap();
            wire::write_bool(&mut entry_buf, true).await.unwrap();
            wire::write_strings(&mut entry_buf, &[]).await.unwrap();
            wire::write_string(&mut entry_buf, "").await.unwrap();
            wire::write_framed_stream(&mut entry_buf, &nar_data, 64 * 1024)
                .await
                .unwrap();

            // Send wopAddMultipleToStore (44)
            wire::write_u64(&mut s, 44).await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            wire::write_framed_stream(&mut s, &entry_buf, 64 * 1024)
                .await
                .unwrap();
            s.flush().await.unwrap();

            // Read response
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(msg, STDERR_LAST);
            let result = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(result, 1, "expected success (1)");
        })
    })
    .await;

    let path = StorePath::parse("/nix/store/cccccccccccccccccccccccccccccccc-multi-test").unwrap();
    assert!(store2.is_valid_path(&path).await.unwrap());
}

/// wopQueryDerivationOutputMap (41): Upload a .drv, then query its output map.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_derivation_output_map_direct() {
    let store = Arc::new(MemoryStore::new());

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // First, upload a .drv via wopAddMultipleToStore
            let drv_content = make_test_drv();
            let nar_data = make_nar(drv_content.as_bytes());
            let nar_hash = sha256_hex(&nar_data);
            let nar_size = nar_data.len() as u64;
            let drv_path = "/nix/store/dddddddddddddddddddddddddddddddd-test-drv.drv";

            let mut entry_buf = Vec::new();
            wire::write_string(&mut entry_buf, drv_path).await.unwrap();
            wire::write_string(&mut entry_buf, "").await.unwrap();
            wire::write_string(&mut entry_buf, &nar_hash).await.unwrap();
            wire::write_strings(&mut entry_buf, &[]).await.unwrap();
            wire::write_u64(&mut entry_buf, 0).await.unwrap();
            wire::write_u64(&mut entry_buf, nar_size).await.unwrap();
            wire::write_bool(&mut entry_buf, true).await.unwrap();
            wire::write_strings(&mut entry_buf, &[]).await.unwrap();
            wire::write_string(&mut entry_buf, "").await.unwrap();
            wire::write_framed_stream(&mut entry_buf, &nar_data, 64 * 1024)
                .await
                .unwrap();

            wire::write_u64(&mut s, 44).await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            wire::write_framed_stream(&mut s, &entry_buf, 64 * 1024)
                .await
                .unwrap();
            s.flush().await.unwrap();

            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(msg, STDERR_LAST);
            let result = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(result, 1);

            // Now query the derivation output map (opcode 41)
            wire::write_u64(&mut s, 41).await.unwrap();
            wire::write_string(&mut s, drv_path).await.unwrap();
            s.flush().await.unwrap();

            let msg = wire::read_u64(&mut s).await.unwrap();
            if msg == STDERR_ERROR {
                let _error_type = wire::read_string(&mut s).await.unwrap();
                let _level = wire::read_u64(&mut s).await.unwrap();
                let _name = wire::read_string(&mut s).await.unwrap();
                let message = wire::read_string(&mut s).await.unwrap();
                panic!("unexpected STDERR_ERROR from QueryDerivationOutputMap: {message}");
            }
            assert_eq!(msg, STDERR_LAST);

            // Read output count + entries
            let count = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(count, 1, "expected 1 output");

            let name = wire::read_string(&mut s).await.unwrap();
            assert_eq!(name, "out");
            let path = wire::read_string(&mut s).await.unwrap();
            assert!(path.contains("test"), "output path should contain 'test'");
        })
    })
    .await;
}

// ---------------------------------------------------------------------------
// Phase 1b: error-path tests
// ---------------------------------------------------------------------------

/// wopQueryDerivationOutputMap: querying a derivation that doesn't exist
/// should return STDERR_ERROR.
#[tokio::test(flavor = "multi_thread")]
async fn test_query_derivation_output_map_not_found() {
    let store = Arc::new(MemoryStore::new());

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            // Query a derivation that was never uploaded
            wire::write_u64(&mut s, 41).await.unwrap();
            wire::write_string(
                &mut s,
                "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-missing.drv",
            )
            .await
            .unwrap();
            s.flush().await.unwrap();

            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(msg, STDERR_ERROR, "expected STDERR_ERROR for missing drv");

            // Read error structure
            let _error_type = wire::read_string(&mut s).await.unwrap();
            let _level = wire::read_u64(&mut s).await.unwrap();
            let _name = wire::read_string(&mut s).await.unwrap();
            let message = wire::read_string(&mut s).await.unwrap();
            assert!(
                message.contains("not found"),
                "error should mention 'not found', got: {message}"
            );
        })
    })
    .await;
}

/// wopAddToStoreNar: sending a NAR with a mismatched hash should
/// return STDERR_ERROR.
#[tokio::test(flavor = "multi_thread")]
async fn test_add_to_store_nar_hash_mismatch() {
    let store = Arc::new(MemoryStore::new());

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            let nar_data = make_nar(b"some content");
            let wrong_hash = "0".repeat(64);
            let nar_size = nar_data.len() as u64;
            let path_str = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-hash-test";

            wire::write_u64(&mut s, 39).await.unwrap();
            wire::write_string(&mut s, path_str).await.unwrap();
            wire::write_string(&mut s, "").await.unwrap();
            wire::write_string(&mut s, &wrong_hash).await.unwrap();
            wire::write_strings(&mut s, &[]).await.unwrap();
            wire::write_u64(&mut s, 0).await.unwrap();
            wire::write_u64(&mut s, nar_size).await.unwrap();
            wire::write_bool(&mut s, true).await.unwrap();
            wire::write_strings(&mut s, &[]).await.unwrap();
            wire::write_string(&mut s, "").await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            s.flush().await.unwrap();

            // Respond to STDERR_READ requests with NAR data
            let mut sent = 0usize;
            loop {
                let msg = wire::read_u64(&mut s).await.unwrap();
                if msg == STDERR_ERROR {
                    // Hash mismatch detected — read the error structure
                    let _error_type = wire::read_string(&mut s).await.unwrap();
                    let _level = wire::read_u64(&mut s).await.unwrap();
                    let _name = wire::read_string(&mut s).await.unwrap();
                    let message = wire::read_string(&mut s).await.unwrap();
                    assert!(
                        message.contains("hash mismatch"),
                        "error should mention 'hash mismatch', got: {message}"
                    );
                    return;
                }
                assert_eq!(msg, STDERR_READ, "expected STDERR_READ or STDERR_ERROR");
                let requested = wire::read_u64(&mut s).await.unwrap() as usize;
                let end = (sent + requested).min(nar_data.len());
                let chunk = &nar_data[sent..end];
                wire::write_bytes(&mut s, chunk).await.unwrap();
                s.flush().await.unwrap();
                sent = end;
            }
        })
    })
    .await;
}

/// wopAddMultipleToStore: sending an entry with a mismatched NAR hash
/// should return STDERR_ERROR.
#[tokio::test(flavor = "multi_thread")]
async fn test_add_multiple_to_store_hash_mismatch() {
    let store = Arc::new(MemoryStore::new());

    run_test(store, |s| {
        tokio::spawn(async move {
            let mut s = s;
            do_handshake(&mut s).await;

            let nar_data = make_nar(b"content");
            let wrong_hash = "f".repeat(64);
            let nar_size = nar_data.len() as u64;
            let path_str = "/nix/store/ffffffffffffffffffffffffffffffff-bad-hash";

            let mut entry_buf = Vec::new();
            wire::write_string(&mut entry_buf, path_str).await.unwrap();
            wire::write_string(&mut entry_buf, "").await.unwrap();
            wire::write_string(&mut entry_buf, &wrong_hash)
                .await
                .unwrap();
            wire::write_strings(&mut entry_buf, &[]).await.unwrap();
            wire::write_u64(&mut entry_buf, 0).await.unwrap();
            wire::write_u64(&mut entry_buf, nar_size).await.unwrap();
            wire::write_bool(&mut entry_buf, true).await.unwrap();
            wire::write_strings(&mut entry_buf, &[]).await.unwrap();
            wire::write_string(&mut entry_buf, "").await.unwrap();
            wire::write_framed_stream(&mut entry_buf, &nar_data, 64 * 1024)
                .await
                .unwrap();

            wire::write_u64(&mut s, 44).await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            wire::write_bool(&mut s, false).await.unwrap();
            wire::write_framed_stream(&mut s, &entry_buf, 64 * 1024)
                .await
                .unwrap();
            s.flush().await.unwrap();

            // Expect STDERR_ERROR for hash mismatch
            let msg = wire::read_u64(&mut s).await.unwrap();
            assert_eq!(msg, STDERR_ERROR, "expected STDERR_ERROR for hash mismatch");

            let _error_type = wire::read_string(&mut s).await.unwrap();
            let _level = wire::read_u64(&mut s).await.unwrap();
            let _name = wire::read_string(&mut s).await.unwrap();
            let message = wire::read_string(&mut s).await.unwrap();
            assert!(
                message.contains("hash mismatch") || message.contains("failed"),
                "error should mention hash issue, got: {message}"
            );
        })
    })
    .await;
}
