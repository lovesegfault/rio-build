//! Direct protocol test — feed raw bytes through a DuplexStream without SSH.
//! This isolates the protocol handler from SSH transport issues.

use std::sync::Arc;

use rio_build::gateway::session::run_protocol;
use rio_build::store::MemoryStore;
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::wire;
use tokio::io::AsyncWriteExt;

#[tokio::test(flavor = "multi_thread")]
async fn test_handshake_direct() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rio_build=debug,rio_nix=debug")
        .with_test_writer()
        .try_init();

    let store = Arc::new(MemoryStore::new());

    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);

    // Server task
    let store_clone = store.clone();
    let server = tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(server_stream);
        match run_protocol(&mut reader, &mut writer, store_clone.as_ref()).await {
            Ok(()) => eprintln!("SERVER: protocol completed OK"),
            Err(e) => eprintln!("SERVER: protocol error: {e}"),
        }
    });

    // Client task: simulate Nix handshake
    let client = tokio::spawn(async move {
        let mut s = client_stream;

        // 1. Send MAGIC_1 + VERSION
        wire::write_u64(&mut s, WORKER_MAGIC_1).await.unwrap();
        wire::write_u64(&mut s, PROTOCOL_VERSION).await.unwrap();
        s.flush().await.unwrap();

        // 2. Read MAGIC_2 + server version
        let magic2 = wire::read_u64(&mut s).await.unwrap();
        assert_eq!(magic2, WORKER_MAGIC_2, "expected WORKER_MAGIC_2");
        let server_version = wire::read_u64(&mut s).await.unwrap();
        assert_eq!(server_version, PROTOCOL_VERSION);

        // 3. Feature exchange (>= 1.38)
        let features: Vec<String> = vec![];
        wire::write_strings(&mut s, &features).await.unwrap();
        s.flush().await.unwrap();
        let server_features = wire::read_strings(&mut s).await.unwrap();
        assert!(server_features.is_empty());

        // 4. Post-handshake: send affinity + reserveSpace
        wire::write_u64(&mut s, 0).await.unwrap(); // affinity
        wire::write_u64(&mut s, 0).await.unwrap(); // reserveSpace
        s.flush().await.unwrap();

        // 5. Read version string + trusted + STDERR_LAST
        let version = wire::read_string(&mut s).await.unwrap();
        assert!(version.contains("rio-build"));
        let trusted = wire::read_u64(&mut s).await.unwrap();
        assert_eq!(trusted, 1);
        let last = wire::read_u64(&mut s).await.unwrap();
        assert_eq!(last, rio_nix::protocol::stderr::STDERR_LAST);

        // 6. Send wopSetOptions (opcode 19)
        wire::write_u64(&mut s, 19).await.unwrap(); // opcode
        // 12 u64 fields (all zeros for defaults)
        for _ in 0..12 {
            wire::write_u64(&mut s, 0).await.unwrap();
        }
        // overrides count = 0
        wire::write_u64(&mut s, 0).await.unwrap();
        s.flush().await.unwrap();

        // 7. Read STDERR_LAST + result
        let msg = wire::read_u64(&mut s).await.unwrap();
        assert_eq!(msg, rio_nix::protocol::stderr::STDERR_LAST);
        let result = wire::read_u64(&mut s).await.unwrap();
        assert_eq!(result, 1); // success

        eprintln!("CLIENT: handshake + options completed successfully!");

        // Close connection
        drop(s);
    });

    let (c, s) = tokio::join!(client, server);
    c.unwrap();
    s.unwrap();
}
