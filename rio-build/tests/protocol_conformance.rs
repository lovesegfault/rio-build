//! Protocol conformance integration tests.
//!
//! These tests start a real rio-build SSH server and verify protocol
//! correctness using the `nix` CLI.
//!
//! Requirements:
//! - `nix` must be available in PATH (provided by the dev shell)
//! - `ssh-keygen` must be available

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rio_build::gateway::server::{GatewayServer, load_authorized_keys, load_or_generate_host_key};
use rio_build::store::MemoryStore;

/// Set up a test environment: generate SSH keys, create authorized_keys,
/// return the paths and a random available port.
struct TestEnv {
    // Held to keep the temp directory alive for the test duration
    _dir: tempfile::TempDir,
    host_key_path: PathBuf,
    client_key_path: PathBuf,
    authorized_keys_path: PathBuf,
    port: u16,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let host_key_path = dir.path().join("host_key");
        let client_key_path = dir.path().join("client_key");
        let authorized_keys_path = dir.path().join("authorized_keys");

        // Generate client key
        let status = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                client_key_path.to_str().unwrap(),
                "-N",
                "",
                "-q",
            ])
            .status()
            .expect("failed to run ssh-keygen");
        assert!(status.success(), "ssh-keygen failed");

        // Create authorized_keys from the client public key
        let pub_key =
            std::fs::read_to_string(client_key_path.with_extension("pub")).expect("read pub key");
        std::fs::write(&authorized_keys_path, &pub_key).expect("write authorized_keys");

        // Find an available port
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        TestEnv {
            _dir: dir,
            host_key_path,
            client_key_path,
            authorized_keys_path,
            port,
        }
    }

    /// Start the SSH server in the background and return a handle to shut it down.
    async fn start_server(&self, store: Arc<MemoryStore>) -> tokio::task::JoinHandle<()> {
        let host_key = load_or_generate_host_key(&self.host_key_path).expect("load host key");
        let authorized_keys =
            load_authorized_keys(&self.authorized_keys_path).expect("load authorized keys");

        let addr: SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
        let server = GatewayServer::new(store, authorized_keys);

        tokio::spawn(async move {
            if let Err(e) = server.run(host_key, addr).await {
                eprintln!("server error: {e}");
            }
        })
    }

    /// Build NIX_SSHOPTS for connecting to the test server.
    fn nix_ssh_opts(&self) -> String {
        format!(
            "-i {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ControlMaster=no -o ControlPath=none",
            self.client_key_path.display()
        )
    }

    /// Build the ssh-ng store URL.
    fn store_url(&self) -> String {
        format!("ssh-ng://localhost:{}", self.port)
    }

    /// Wait for the server to be ready by attempting TCP connections.
    async fn wait_for_server(&self) {
        let addr: SocketAddr = format!("127.0.0.1:{}", self.port).parse().unwrap();
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "server did not start within 5 seconds on port {}",
            self.port
        );
    }
}

/// Test that `nix store info` (formerly `ping`) completes successfully.
///
/// This validates:
/// - SSH connection establishment
/// - Public key authentication
/// - exec_request handling for `nix-daemon --stdio`
/// - Nix worker protocol handshake (magic bytes + version negotiation)
/// - wopSetOptions parsing
#[tokio::test]
async fn test_nix_store_ping() {
    // Initialize logging for test debugging
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rio_build=debug,rio_nix=debug")
        .with_test_writer()
        .try_init();

    let env = TestEnv::new();
    let store = Arc::new(MemoryStore::new());
    let server_handle = env.start_server(store).await;
    env.wait_for_server().await;

    // nix store ping/info with a timeout
    let output = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::task::spawn_blocking({
            let store_url = env.store_url();
            let ssh_opts = env.nix_ssh_opts();
            move || {
                Command::new("nix")
                    .args(["store", "info", "--store", &store_url])
                    .env("NIX_SSHOPTS", &ssh_opts)
                    .output()
                    .expect("failed to run nix store info")
            }
        })
        .await
        .unwrap()
    })
    .await;

    let output = match output {
        Ok(o) => o,
        Err(_) => {
            server_handle.abort();
            panic!("nix store info timed out after 30 seconds");
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- nix store info stdout ---\n{stdout}");
    eprintln!("--- nix store info stderr ---\n{stderr}");

    assert!(
        output.status.success(),
        "nix store info failed with status {}: stderr: {stderr}",
        output.status
    );

    // The output should contain our version string
    assert!(
        stdout.contains("rio-build") || stderr.contains("rio-build"),
        "expected 'rio-build' in output"
    );

    server_handle.abort();
}
