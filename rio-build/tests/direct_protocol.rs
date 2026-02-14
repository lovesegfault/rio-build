//! Direct protocol tests — feed raw bytes through a DuplexStream without SSH.
//! Tests each Phase 1a opcode at the byte level.

use std::sync::Arc;

use rio_build::gateway::session::run_protocol;
use rio_build::store::MemoryStore;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::{STDERR_ERROR, STDERR_LAST};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
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
            assert!(valid[0].contains("hello-2.12.1"));
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
            assert!(
                unknown[0].contains("missing-1.0"),
                "expected missing path in unknown, got: {unknown:?}"
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
            let _will_substitute = wire::read_strings(&mut s).await.unwrap();
            let _unknown = wire::read_strings(&mut s).await.unwrap();
            let _download_size = wire::read_u64(&mut s).await.unwrap();
            let _nar_size = wire::read_u64(&mut s).await.unwrap();
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
            assert!(
                will_build[0].contains("missing-1.0"),
                "expected missing path, got: {:?}",
                will_build
            );

            // willSubstitute, unknown, downloadSize, narSize
            let _will_substitute = wire::read_strings(&mut s).await.unwrap();
            let _unknown = wire::read_strings(&mut s).await.unwrap();
            let _download_size = wire::read_u64(&mut s).await.unwrap();
            let _nar_size = wire::read_u64(&mut s).await.unwrap();
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

            // Send opcode 36 (wopBuildDerivation) — known but not yet implemented
            wire::write_u64(&mut s, 36).await.unwrap();
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
                message.contains("wopBuildDerivation"),
                "expected message to contain 'wopBuildDerivation', got: {message}"
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
