//! Live nix-daemon management for golden conformance tests.
//!
//! Spins up an isolated nix-daemon on a temporary Unix socket,
//! performs interactive protocol exchanges, and captures both
//! client and server byte streams for comparison against rio-build.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::StorePathEntry;

/// Default nix-daemon socket path.
const DEFAULT_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

/// Perform an interactive protocol exchange with the nix-daemon.
///
/// The nix-daemon protocol is interactive — the handshake involves
/// back-and-forth phases. This function sends client bytes in phases
/// and reads server responses between them.
///
/// Returns (all_client_bytes, all_server_bytes) for the complete session.
pub async fn exchange_with_daemon(
    socket_path: &str,
    opcode_bytes: Option<&[u8]>,
) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1};
    use rio_nix::protocol::wire;

    let mut stream = UnixStream::connect(socket_path).await?;
    let mut all_client = Vec::new();
    let mut all_server = Vec::new();

    // --- Handshake phase 1: magic + version ---
    let mut phase1 = Vec::new();
    wire::write_u64(&mut phase1, WORKER_MAGIC_1).await.unwrap();
    wire::write_u64(&mut phase1, PROTOCOL_VERSION)
        .await
        .unwrap();
    stream.write_all(&phase1).await?;
    stream.flush().await?;
    all_client.extend_from_slice(&phase1);

    // Read server magic2 + server version
    let mut srv_phase1 = vec![0u8; 16];
    stream.read_exact(&mut srv_phase1).await?;
    all_server.extend_from_slice(&srv_phase1);

    // --- Handshake phase 2: features exchange ---
    let mut phase2 = Vec::new();
    wire::write_strings(&mut phase2, &[]).await.unwrap();
    stream.write_all(&phase2).await?;
    stream.flush().await?;
    all_client.extend_from_slice(&phase2);

    // Read server features (count + strings)
    let mut count_buf = vec![0u8; 8];
    stream.read_exact(&mut count_buf).await?;
    let count = u64::from_le_bytes(count_buf.clone().try_into().unwrap()) as usize;
    all_server.extend_from_slice(&count_buf);
    for _ in 0..count {
        let mut len_buf = vec![0u8; 8];
        stream.read_exact(&mut len_buf).await?;
        let len = u64::from_le_bytes(len_buf.clone().try_into().unwrap()) as usize;
        let padded = (len + 7) & !7;
        all_server.extend_from_slice(&len_buf);
        if padded > 0 {
            let mut str_buf = vec![0u8; padded];
            stream.read_exact(&mut str_buf).await?;
            all_server.extend_from_slice(&str_buf);
        }
    }

    // --- Handshake phase 3: post-handshake ---
    let mut phase3 = Vec::new();
    wire::write_u64(&mut phase3, 0).await.unwrap(); // CPU affinity
    wire::write_u64(&mut phase3, 0).await.unwrap(); // reserveSpace
    stream.write_all(&phase3).await?;
    stream.flush().await?;
    all_client.extend_from_slice(&phase3);

    // Read version string (length-prefixed padded) + trusted (u64) + STDERR_LAST (u64)
    let mut len_buf = vec![0u8; 8];
    stream.read_exact(&mut len_buf).await?;
    let len = u64::from_le_bytes(len_buf.clone().try_into().unwrap()) as usize;
    let padded = (len + 7) & !7;
    all_server.extend_from_slice(&len_buf);
    if padded > 0 {
        let mut str_buf = vec![0u8; padded];
        stream.read_exact(&mut str_buf).await?;
        all_server.extend_from_slice(&str_buf);
    }
    let mut post_hs = vec![0u8; 16]; // trusted + STDERR_LAST
    stream.read_exact(&mut post_hs).await?;
    all_server.extend_from_slice(&post_hs);

    // --- SetOptions (opcode 19) ---
    let mut set_opts = Vec::new();
    wire::write_u64(&mut set_opts, 19).await.unwrap();
    for _ in 0..12 {
        wire::write_u64(&mut set_opts, 0).await.unwrap();
    }
    wire::write_u64(&mut set_opts, 0).await.unwrap(); // empty overrides
    stream.write_all(&set_opts).await?;
    stream.flush().await?;
    all_client.extend_from_slice(&set_opts);

    // Read SetOptions response: just STDERR_LAST (nix-daemon sends no result value)
    let mut so_resp = vec![0u8; 8];
    stream.read_exact(&mut so_resp).await?;
    all_server.extend_from_slice(&so_resp);

    // --- Opcode payload (if any) ---
    if let Some(op_bytes) = opcode_bytes {
        stream.write_all(op_bytes).await?;
        stream.flush().await?;
        all_client.extend_from_slice(op_bytes);

        // Read until the daemon stops sending (use a timeout)
        let mut op_response = Vec::new();
        loop {
            let mut buf = vec![0u8; 8192];
            match tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
                .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => op_response.extend_from_slice(&buf[..n]),
                Ok(Err(e)) => return Err(e),
                Err(_) => break, // timeout = daemon is done sending
            }
        }
        all_server.extend_from_slice(&op_response);
    }

    Ok((all_client, all_server))
}

/// Build a test store path and return its full path string.
pub fn build_test_path() -> String {
    let output = std::process::Command::new("nix")
        .args([
            "build",
            "--no-link",
            "--print-out-paths",
            "--impure",
            "--expr",
            r#"(import <nixpkgs> {}).writeTextFile { name = "rio-golden-test"; text = "golden test data\n"; }"#,
        ])
        .output()
        .expect("nix build must succeed");
    assert!(
        output.status.success(),
        "nix build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Query path info from the real nix store via `nix path-info --json`.
pub fn query_path_info_json(store_path: &str) -> StorePathEntry {
    let output = std::process::Command::new("nix")
        .args(["path-info", "--json", store_path])
        .output()
        .expect("nix path-info must succeed");
    assert!(
        output.status.success(),
        "nix path-info failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON from nix path-info");

    // nix path-info --json returns { "/nix/store/...": { ... } }
    let info = json
        .as_object()
        .and_then(|m| m.get(store_path))
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| panic!("unexpected JSON structure from nix path-info"));

    let deriver = info
        .get("deriver")
        .and_then(|d| d.as_str())
        .map(String::from);
    let nar_hash = info
        .get("narHash")
        .and_then(|h| h.as_str())
        .expect("narHash field")
        .to_string();
    let nar_size = info
        .get("narSize")
        .and_then(|n| n.as_u64())
        .expect("narSize field");
    let references: Vec<String> = info
        .get("references")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let sigs: Vec<String> = info
        .get("signatures")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ca = info.get("ca").and_then(|c| c.as_str()).map(String::from);
    let registration_time = info
        .get("registrationTime")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let ultimate = info
        .get("ultimate")
        .and_then(|u| u.as_bool())
        .unwrap_or(false);

    StorePathEntry {
        path: store_path.to_string(),
        deriver,
        nar_hash,
        references,
        registration_time,
        nar_size,
        ultimate,
        sigs,
        ca,
    }
}

/// RAII guard that stops a nix-daemon process and cleans up its temp dir.
pub struct DaemonGuard {
    child: std::process::Child,
    _temp_dir: tempfile::TempDir,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a local nix-daemon on a temporary Unix socket.
///
/// Uses the real store database by symlinking `/nix/var/nix/*` into a temp
/// dir, then pointing `NIX_STATE_DIR` there. The daemon gets a fresh socket
/// but can see all existing store paths.
pub fn start_local_daemon() -> (String, DaemonGuard) {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    // Symlink real store state into the temp dir (skip daemon-socket and gc-socket)
    let real_state = std::path::Path::new("/nix/var/nix");
    if let Ok(entries) = std::fs::read_dir(real_state) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str != "daemon-socket" && name_str != "gc-socket" {
                let _ = std::os::unix::fs::symlink(entry.path(), temp_dir.path().join(&name));
            }
        }
    }

    let daemon_sock_dir = temp_dir.path().join("daemon-socket");
    std::fs::create_dir_all(&daemon_sock_dir).expect("create daemon-socket dir");
    let socket = daemon_sock_dir.join("socket");

    let mut child = std::process::Command::new("nix-daemon")
        .env("NIX_STATE_DIR", temp_dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start nix-daemon");

    // Wait for the socket to appear
    for _ in 0..50 {
        if socket.exists() {
            return (
                socket.to_string_lossy().to_string(),
                DaemonGuard {
                    child,
                    _temp_dir: temp_dir,
                },
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "nix-daemon did not create socket at {} within 5 seconds",
        socket.display()
    );
}

/// Find or start a nix-daemon and return its socket path with an optional guard.
///
/// Checks `NIX_DAEMON_SOCKET` env var, then the default socket path,
/// and falls back to starting a local daemon.
pub fn get_daemon_socket() -> (String, Option<DaemonGuard>) {
    if let Ok(s) = std::env::var("NIX_DAEMON_SOCKET") {
        if Path::new(&s).exists() {
            return (s, None);
        }
        eprintln!("NIX_DAEMON_SOCKET={s} not found, starting local daemon");
    } else if Path::new(DEFAULT_SOCKET).exists() {
        return (DEFAULT_SOCKET.to_string(), None);
    } else {
        eprintln!("no nix-daemon socket found, starting local daemon");
    }
    let (s, g) = start_local_daemon();
    (s, Some(g))
}
