//! End-to-end build integration tests via ssh-ng.
//!
//! Tests that `nix build --store ssh-ng://localhost` can execute builds
//! by uploading derivations and receiving build results through rio-build.
//!
//! Requirements:
//! - `nix`, `nix-daemon`, `ssh-keygen` in PATH (provided by dev shell)
//! - Multi-user Nix daemon running (for nix-daemon --stdio)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rio_build::gateway::server::{GatewayServer, load_authorized_keys, load_or_generate_host_key};
use rio_build::store::MemoryStore;

struct TestEnv {
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

        let pub_key =
            std::fs::read_to_string(client_key_path.with_extension("pub")).expect("read pub key");
        std::fs::write(&authorized_keys_path, &pub_key).expect("write authorized_keys");

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

    fn nix_ssh_opts(&self) -> String {
        format!(
            "-i {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ControlMaster=no -o ControlPath=none",
            self.client_key_path.display()
        )
    }

    fn store_url(&self) -> String {
        format!("ssh-ng://localhost:{}", self.port)
    }

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

/// Test that `nix build` of a trivial derivation works end-to-end via ssh-ng.
///
/// This validates the full Phase 1b pipeline:
/// 1. SSH connection + handshake
/// 2. wopSetOptions
/// 3. wopQueryValidPaths (check which outputs exist)
/// 4. wopAddToStoreNar / wopAddMultipleToStore (upload .drv + sources)
/// 5. wopQueryDerivationOutputMap (resolve output paths)
/// 6. wopBuildDerivation (build via local nix-daemon --stdio)
/// 7. wopNarFromPath (retrieve build output)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_build_trivial_derivation() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rio_build=debug,rio_nix=debug")
        .with_test_writer()
        .try_init();

    // Check nix-daemon is available
    let daemon_check = Command::new("nix-daemon").arg("--version").output();
    if !matches!(daemon_check, Ok(ref o) if o.status.success()) {
        eprintln!("skipping test_build_trivial_derivation: nix-daemon not available");
        return;
    }

    let env = TestEnv::new();
    let store = Arc::new(MemoryStore::new());
    let server_handle = env.start_server(store).await;
    env.wait_for_server().await;

    // Build a trivial derivation via our ssh-ng store.
    // Using `builtins.derivation` with `writeText`-style builder that
    // just echoes content to $out. Since client=server (localhost),
    // all input paths are already in /nix/store.
    let nix_expr = r#"
      derivation {
        name = "rio-build-test";
        system = builtins.currentSystem;
        builder = "/bin/sh";
        args = [ "-c" "echo 'hello from rio-build' > $out" ];
      }
    "#;

    let output = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::task::spawn_blocking({
            let store_url = env.store_url();
            let ssh_opts = env.nix_ssh_opts();
            move || {
                Command::new("nix")
                    .args([
                        "build",
                        "--impure",
                        "--no-link",
                        "--store",
                        &store_url,
                        "--expr",
                        nix_expr,
                    ])
                    .env("NIX_SSHOPTS", &ssh_opts)
                    .output()
                    .expect("failed to run nix build")
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
            panic!("nix build timed out after 60 seconds");
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr_text = String::from_utf8_lossy(&output.stderr);

    eprintln!("--- nix build stdout ---\n{stdout}");
    eprintln!("--- nix build stderr ---\n{stderr_text}");

    // Assert the build succeeded — the full end-to-end protocol path
    // (handshake → AddToStoreNar/AddMultipleToStore → QueryDerivationOutputMap
    // → BuildDerivation → NarFromPath) must complete successfully.
    assert!(
        output.status.success(),
        "nix build failed with status {}.\nstdout: {}\nstderr: {}",
        output.status,
        stdout,
        stderr_text,
    );

    server_handle.abort();
}
