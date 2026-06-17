//! Live nix-daemon conformance for the client-side worker-protocol
//! helpers (`client_handshake`, `client_query_valid_paths`,
//! `client_add_to_store_nar`).
//!
//! Each test spawns its own hermetic daemon with a throwaway store under
//! `$TMPDIR` (`NIX_STORE_DIR`/`NIX_STATE_DIR` overridden), so imports
//! write into a private store, never the host `/nix/store`. The daemon
//! binary comes from `PATH` (dev shell / `nixForTests` in the nextest
//! sandbox).

use std::path::Path;
use std::time::Duration;

use sha2::Digest as _;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use rio_nix::nar::NarNode;
use rio_nix::protocol::client::{
    client_add_to_store_nar, client_handshake, client_query_valid_paths,
};
use rio_nix::protocol::pathinfo::ValidPathInfo;

/// RAII guard for the per-test daemon: kills the process and removes the
/// throwaway store on drop.
struct TestDaemon {
    child: std::process::Child,
    temp: tempfile::TempDir,
}

impl TestDaemon {
    /// Spawn `nix-daemon` against a fresh store under a tempdir.
    /// `extra_config` is appended to `NIX_CONFIG` (e.g. `require-sigs =
    /// false` for the import-success test).
    fn spawn(extra_config: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temp dir");
        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(state_dir.join("daemon-socket")).expect("create state dir");

        let child = std::process::Command::new("nix-daemon")
            .env("NIX_STORE_DIR", temp.path().join("store"))
            .env("NIX_STATE_DIR", &state_dir)
            .env("NIX_LOG_DIR", temp.path().join("log"))
            .env("NIX_CONF_DIR", temp.path().join("etc"))
            .env(
                "NIX_CONFIG",
                format!("experimental-features = nix-command\n{extra_config}"),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn nix-daemon (is nix in PATH?)");

        let daemon = Self { child, temp };
        // SLEEP JUSTIFICATION: polling for the unix socket an external
        // nix-daemon creates; same pattern (and budget) as the gateway
        // golden harness. 100ms × 50 = 5s; warm systems bind in <200ms.
        for _ in 0..50 {
            if daemon.socket_path().exists() {
                return daemon;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "nix-daemon did not create {} within 5s",
            daemon.socket_path().display()
        );
    }

    fn socket_path(&self) -> std::path::PathBuf {
        self.temp
            .path()
            .join("state")
            .join("daemon-socket")
            .join("socket")
    }

    fn store_dir(&self) -> std::path::PathBuf {
        self.temp.path().join("store")
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Connect + handshake, returning split halves ready for opcodes.
async fn connect(
    socket: &Path,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(socket).await.expect("connect daemon");
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;
    let result = client_handshake(&mut reader, &mut writer)
        .await
        .expect("client handshake against real nix-daemon");
    assert!(
        result.negotiated_version() >= rio_nix::protocol::handshake::MIN_DAEMON_VERSION,
        "negotiated {:#x} below the client floor",
        result.negotiated_version()
    );
    (reader, writer)
}

/// A syntactically valid store path inside the daemon's custom store dir.
/// The hash part only has to be 32 nixbase32 characters — AddToStoreNar
/// registers whatever path the (metadata-supplying) client names.
fn test_store_path(store_dir: &Path, name: &str) -> String {
    format!(
        "{}/0123456789abcdfghijklmnpqrsvwxyz-{name}",
        store_dir.display()
    )
}

fn single_file_nar(payload: &[u8]) -> (Vec<u8>, [u8; 32]) {
    let node = NarNode::Regular {
        executable: false,
        contents: payload.to_vec(),
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, &node).expect("serialize NAR to Vec");
    let hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
    (nar, hash)
}

fn import_info(nar: &[u8], nar_hash: [u8; 32]) -> ValidPathInfo {
    ValidPathInfo {
        deriver: None,
        nar_hash: nar_hash.to_vec(),
        references: vec![],
        registration_time: 0,
        nar_size: nar.len() as u64,
        ultimate: false,
        signatures: vec![],
        content_address: None,
    }
}

/// Import a single-file NAR into a fresh daemon store and read it back:
/// QueryValidPaths flips from empty to the imported path, and the file
/// lands on disk with the exact payload.
#[tokio::test]
async fn test_golden_live_add_to_store_nar_import() {
    // The test's path info carries no signature, so the daemon must not
    // require one (the production client relies on cluster-signed paths;
    // the rejection test below covers the require-sigs arm).
    let daemon = TestDaemon::spawn("require-sigs = false");
    let (mut reader, mut writer) = connect(&daemon.socket_path()).await;

    let payload = b"rio daemon-client golden payload\n";
    let (nar, nar_hash) = single_file_nar(payload);
    let store_path = test_store_path(&daemon.store_dir(), "rio-golden-import");
    let info = import_info(&nar, nar_hash);

    // Prune answer before the import: nothing valid yet.
    let valid = client_query_valid_paths(&mut reader, &mut writer, &[store_path.as_str()], false)
        .await
        .expect("QueryValidPaths against real daemon");
    assert!(valid.is_empty(), "fresh store must report no valid paths");

    let mut nar_src = std::io::Cursor::new(nar);
    client_add_to_store_nar(&mut reader, &mut writer, &store_path, &info, &mut nar_src)
        .await
        .expect("AddToStoreNar against real daemon");

    let valid = client_query_valid_paths(&mut reader, &mut writer, &[store_path.as_str()], false)
        .await
        .expect("QueryValidPaths after import");
    assert_eq!(valid, vec![store_path.clone()]);

    let on_disk = std::fs::read(&store_path).expect("imported path exists on disk");
    assert_eq!(on_disk, payload);
}

/// With the daemon's default `require-sigs = true`, an unsigned import is
/// rejected via STDERR_ERROR and the error text names the missing
/// signature — the exact wording the build client maps to its
/// trusted-public-keys guidance.
#[tokio::test]
async fn test_golden_live_add_to_store_nar_unsigned_rejected() {
    let daemon = TestDaemon::spawn("");
    let (mut reader, mut writer) = connect(&daemon.socket_path()).await;

    let (nar, nar_hash) = single_file_nar(b"unsigned payload\n");
    let store_path = test_store_path(&daemon.store_dir(), "rio-golden-unsigned");
    let info = import_info(&nar, nar_hash);

    let mut nar_src = std::io::Cursor::new(nar);
    let err = client_add_to_store_nar(&mut reader, &mut writer, &store_path, &info, &mut nar_src)
        .await
        .expect_err("unsigned import must be rejected under require-sigs");
    let msg = err.to_string();
    assert!(
        msg.contains("lacks a signature by a trusted key")
            || msg.contains("lacks a valid signature"),
        "rejection must carry the daemon's signature wording, got: {msg}"
    );
}

/// A claimed narHash that doesn't match the streamed bytes is rejected by
/// the daemon (hash verification on its side) and never becomes valid.
#[tokio::test]
async fn test_golden_live_add_to_store_nar_hash_mismatch_rejected() {
    let daemon = TestDaemon::spawn("require-sigs = false");
    let (mut reader, mut writer) = connect(&daemon.socket_path()).await;

    let (nar, _real_hash) = single_file_nar(b"the real bytes\n");
    let store_path = test_store_path(&daemon.store_dir(), "rio-golden-mismatch");
    // Lie about the hash.
    let info = import_info(&nar, [0x11; 32]);

    let mut nar_src = std::io::Cursor::new(nar);
    let err = client_add_to_store_nar(&mut reader, &mut writer, &store_path, &info, &mut nar_src)
        .await
        .expect_err("hash mismatch must be rejected");
    assert!(
        err.to_string().contains("hash mismatch"),
        "expected the daemon's hash-mismatch error, got: {err}"
    );

    // The connection survives a clean daemon-side rejection (the framed
    // stream was fully consumed); the path must not have become valid.
    let valid = client_query_valid_paths(&mut reader, &mut writer, &[store_path.as_str()], false)
        .await
        .expect("QueryValidPaths after rejected import");
    assert!(valid.is_empty());
}
