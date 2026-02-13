//! Golden fixture recorder.
//!
//! Connects to the real nix-daemon socket, sends constructed client bytes,
//! and saves both client and server byte streams as binary fixtures.
//!
//! Gated behind `RECORD_GOLDEN=1` — skipped by default.

use std::path::Path;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::{
    build_add_temp_root_bytes, build_is_valid_path_bytes, build_query_path_info_bytes,
    build_query_valid_paths_bytes, fixtures_dir,
};

/// Metadata written alongside binary fixtures.
#[derive(Serialize)]
struct FixtureMeta {
    nix_version: String,
    description: String,
    skip_fields: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    store_paths: Vec<StorePathMeta>,
}

#[derive(Serialize)]
struct StorePathMeta {
    path: String,
    deriver: Option<String>,
    nar_hash: String,
    references: Vec<String>,
    registration_time: u64,
    nar_size: u64,
    ultimate: bool,
    sigs: Vec<String>,
    ca: Option<String>,
}

/// Default nix-daemon socket path.
const DEFAULT_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

/// Perform an interactive protocol exchange with the nix-daemon.
///
/// The nix-daemon protocol is interactive — the handshake involves
/// back-and-forth phases. This function sends client bytes in phases
/// and reads server responses between them.
///
/// Returns (all_client_bytes, all_server_bytes) for the complete session.
async fn exchange_with_daemon(
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
            match tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
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

/// Get the installed nix version string.
fn nix_version() -> String {
    let output = std::process::Command::new("nix")
        .arg("--version")
        .output()
        .expect("nix must be in PATH");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Build a test store path and return its full path string.
fn build_test_path() -> String {
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
fn query_path_info_json(store_path: &str) -> StorePathMeta {
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

    StorePathMeta {
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

/// Save a fixture set to disk.
fn save_fixture(name: &str, client_bytes: &[u8], server_bytes: &[u8], meta: &FixtureMeta) {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("create fixtures dir");

    let write = |suffix: &str, data: &[u8]| {
        let path = dir.join(format!("{name}.{suffix}"));
        std::fs::write(&path, data)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        eprintln!("  wrote {} ({} bytes)", path.display(), data.len());
    };

    write("client.bin", client_bytes);
    write("server.bin", server_bytes);

    let meta_json = serde_json::to_string_pretty(meta).expect("serialize meta");
    let meta_path = dir.join(format!("{name}.meta.json"));
    std::fs::write(&meta_path, meta_json)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", meta_path.display()));
    eprintln!("  wrote {}", meta_path.display());
}

/// RAII guard that stops a nix-daemon process and cleans up its temp dir.
struct DaemonGuard {
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
fn start_local_daemon() -> (String, DaemonGuard) {
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

/// Record all golden fixtures from a real nix-daemon.
///
/// This test is skipped unless `RECORD_GOLDEN=1` is set.
/// It requires `nix` and `nix-daemon` in PATH. If no existing daemon
/// socket is found, it starts a temporary one automatically.
#[tokio::test]
async fn record_golden_fixtures() {
    if std::env::var("RECORD_GOLDEN").is_err() {
        eprintln!("skipping: set RECORD_GOLDEN=1 to record golden fixtures");
        return;
    }

    let (socket, _daemon_guard);

    if let Ok(s) = std::env::var("NIX_DAEMON_SOCKET") {
        if Path::new(&s).exists() {
            socket = s;
            _daemon_guard = None;
        } else {
            eprintln!("NIX_DAEMON_SOCKET={s} not found, starting local daemon");
            let (s, g) = start_local_daemon();
            socket = s;
            _daemon_guard = Some(g);
        }
    } else if Path::new(DEFAULT_SOCKET).exists() {
        socket = DEFAULT_SOCKET.to_string();
        _daemon_guard = None;
    } else {
        eprintln!("no nix-daemon socket found, starting local daemon");
        let (s, g) = start_local_daemon();
        socket = s;
        _daemon_guard = Some(g);
    }

    let version = nix_version();
    eprintln!("recording golden fixtures with {version}");
    eprintln!("using socket: {socket}");

    // Build a test path we can query
    eprintln!("building test store path...");
    let test_path = build_test_path();
    eprintln!("test path: {test_path}");

    let _path_info = query_path_info_json(&test_path);

    // Fields that legitimately differ between nix-daemon and rio-build:
    // - version_string: different implementation names
    // - trusted: nix-daemon may report 0 for non-root users
    let skip_fields = vec!["version_string".to_string(), "trusted".to_string()];

    // A path that definitely doesn't exist
    let nonexistent = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nonexistent-1.0";

    // Helper to record a fixture with the interactive exchange
    macro_rules! record {
        ($name:expr, $desc:expr, $skip:expr, $store:expr, $opcode:expr) => {{
            let (client, server) = exchange_with_daemon(&socket, $opcode)
                .await
                .expect(concat!($name, " exchange"));
            save_fixture(
                $name,
                &client,
                &server,
                &FixtureMeta {
                    nix_version: version.clone(),
                    description: $desc.to_string(),
                    skip_fields: $skip,
                    store_paths: $store,
                },
            );
        }};
    }

    // --- Record: handshake only ---
    record!(
        "handshake",
        "Handshake only (magic + version + features + post-handshake)",
        skip_fields.clone(),
        vec![],
        None
    );

    // --- Record: handshake + set_options (no extra opcode) ---
    record!(
        "set_options",
        "Handshake + wopSetOptions with default values",
        skip_fields.clone(),
        vec![],
        None
    );

    // --- Record: is_valid_path (found) ---
    {
        let op = build_is_valid_path_bytes(&test_path).await;
        record!(
            "is_valid_path_found",
            "Handshake + SetOptions + wopIsValidPath for existing path",
            skip_fields.clone(),
            vec![query_path_info_json(&test_path)],
            Some(op.as_slice())
        );
    }

    // --- Record: is_valid_path (not found) ---
    {
        let op = build_is_valid_path_bytes(nonexistent).await;
        record!(
            "is_valid_path_not_found",
            "Handshake + SetOptions + wopIsValidPath for nonexistent path",
            skip_fields.clone(),
            vec![],
            Some(op.as_slice())
        );
    }

    // --- Record: query_path_info ---
    {
        let mut skip_with_reg_time = skip_fields.clone();
        skip_with_reg_time.push("reg_time".to_string());
        let op = build_query_path_info_bytes(&test_path).await;
        record!(
            "query_path_info",
            "Handshake + SetOptions + wopQueryPathInfo for existing path",
            skip_with_reg_time,
            vec![query_path_info_json(&test_path)],
            Some(op.as_slice())
        );
    }

    // --- Record: query_valid_paths ---
    {
        let op = build_query_valid_paths_bytes(&[&test_path, nonexistent], false).await;
        record!(
            "query_valid_paths",
            "Handshake + SetOptions + wopQueryValidPaths with one existing and one missing path",
            skip_fields.clone(),
            vec![query_path_info_json(&test_path)],
            Some(op.as_slice())
        );
    }

    // --- Record: add_temp_root ---
    {
        let op = build_add_temp_root_bytes(&test_path).await;
        record!(
            "add_temp_root",
            "Handshake + SetOptions + wopAddTempRoot",
            skip_fields.clone(),
            vec![],
            Some(op.as_slice())
        );
    }

    eprintln!("\nall golden fixtures recorded successfully");
}
