//! Live nix-daemon management for golden conformance tests.
//!
//! Spins up an isolated nix-daemon on a temporary Unix socket,
//! performs interactive protocol exchanges, and captures both
//! client and server byte streams for comparison against rio-build.

use std::path::Path;
use std::sync::{LazyLock, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::StorePathEntry;

/// Default timeout for reading trailing data after STDERR_LAST.
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How to read data after STDERR_LAST in the STDERR message loop.
#[derive(Clone, Copy)]
enum PostLastRead {
    /// Read trailing data with a timeout (for small result fields like bools/u64s).
    Timeout(std::time::Duration),
    /// Parse a complete NAR archive structurally (for NarFromPath).
    Nar,
}

/// Shared daemon instance across all tests.
///
/// Uses `LazyLock` so the daemon is started at most once, even when
/// tests run in parallel. The `Mutex<Option<DaemonGuard>>` keeps the
/// guard alive for the lifetime of the test process.
static SHARED_DAEMON: LazyLock<(String, Mutex<Option<DaemonGuard>>)> = LazyLock::new(|| {
    let (socket, guard) = get_daemon_socket_inner();
    (socket, Mutex::new(guard))
});

/// Get the shared daemon socket path.
///
/// This is the preferred entry point for tests. The daemon is started
/// once and reused across all tests in the process.
pub fn shared_daemon_socket() -> String {
    SHARED_DAEMON.0.clone()
}

/// Perform an interactive protocol exchange with the nix-daemon.
///
/// The nix-daemon protocol is interactive — the handshake involves
/// back-and-forth phases. This function sends client bytes in phases
/// and reads server responses between them.
///
/// Opcode responses are read using a protocol-aware approach: STDERR
/// messages are read structurally until STDERR_LAST, then a short
/// timeout captures any trailing result fields (which are small and
/// sent immediately after STDERR_LAST).
///
/// Returns (all_client_bytes, all_server_bytes) for the complete session.
pub async fn exchange_with_daemon(
    socket_path: &str,
    opcode_bytes: Option<&[u8]>,
) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    exchange_with_daemon_inner(
        socket_path,
        opcode_bytes,
        PostLastRead::Timeout(DEFAULT_READ_TIMEOUT),
    )
    .await
}

/// Like [`exchange_with_daemon`] but parses the post-STDERR_LAST data as a
/// NAR archive instead of using a timeout. This reads exactly the right
/// number of bytes by following the NAR structure, so it completes
/// immediately instead of waiting for a timeout.
pub async fn exchange_with_daemon_nar(
    socket_path: &str,
    opcode_bytes: Option<&[u8]>,
) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    exchange_with_daemon_inner(socket_path, opcode_bytes, PostLastRead::Nar).await
}

/// Shared implementation for all `exchange_with_daemon*` variants.
async fn exchange_with_daemon_inner(
    socket_path: &str,
    opcode_bytes: Option<&[u8]>,
    post_last: PostLastRead,
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
    // All option values are zero (defaults) because rio-build ignores option
    // values — it accepts and discards them. Testing non-default options would
    // only validate the parsing/discarding path, which is already covered by
    // direct protocol tests (test_set_options_with_overrides).
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

        // Read STDERR messages until STDERR_LAST using protocol-aware parsing.
        // This is more reliable than a pure timeout because it follows the
        // actual protocol structure.
        let op_response = read_stderr_loop(&mut stream, post_last).await?;
        all_server.extend_from_slice(&op_response);
    }

    Ok((all_client, all_server))
}

/// Read the STDERR message loop from the daemon, returning all raw bytes.
///
/// Reads STDERR messages structurally until STDERR_LAST, then reads
/// trailing result data according to `post_last`:
/// - `Timeout`: reads with a timeout (for small result fields)
/// - `Nar`: parses a complete NAR archive structurally (instant)
async fn read_stderr_loop(
    stream: &mut UnixStream,
    post_last: PostLastRead,
) -> std::io::Result<Vec<u8>> {
    use rio_nix::protocol::stderr::{
        STDERR_ERROR, STDERR_LAST, STDERR_NEXT, STDERR_READ, STDERR_RESULT, STDERR_START_ACTIVITY,
        STDERR_STOP_ACTIVITY, STDERR_WRITE,
    };

    let mut result = Vec::new();

    loop {
        // Read the message type (u64 LE)
        let mut msg_buf = [0u8; 8];
        stream.read_exact(&mut msg_buf).await?;
        result.extend_from_slice(&msg_buf);
        let msg = u64::from_le_bytes(msg_buf);

        match msg {
            STDERR_LAST => {
                // STDERR_LAST marks the end of the STDERR loop.
                // What follows depends on the opcode.
                match post_last {
                    PostLastRead::Nar => {
                        // NarFromPath: parse the NAR structure to read
                        // exactly the right number of bytes.
                        read_nar_from_stream(stream, &mut result).await?;
                    }
                    PostLastRead::Timeout(timeout) => {
                        // Most opcodes send small result fields (a bool,
                        // a u64, etc.) immediately after STDERR_LAST.
                        loop {
                            let mut buf = [0u8; 8192];
                            match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
                                Ok(Ok(0)) => break,
                                Ok(Ok(n)) => result.extend_from_slice(&buf[..n]),
                                Ok(Err(e)) => return Err(e),
                                Err(_) => break,
                            }
                        }
                    }
                }
                return Ok(result);
            }
            STDERR_NEXT => {
                // Log message: string
                read_wire_string(stream, &mut result).await?;
            }
            STDERR_WRITE => {
                // Data chunk: string (length-prefixed + padded)
                read_wire_string(stream, &mut result).await?;
            }
            STDERR_ERROR => {
                // Error: type + level + name + message + position + traces
                // (variable-length, but we read until STDERR_LAST or EOF)
                read_wire_string(stream, &mut result).await?; // type
                read_wire_string(stream, &mut result).await?; // message/error msg
                let mut err_code = [0u8; 8];
                stream.read_exact(&mut err_code).await?;
                result.extend_from_slice(&err_code);
                // Position: has_pos (u64), if nonzero: file, line, col
                let mut has_pos = [0u8; 8];
                stream.read_exact(&mut has_pos).await?;
                result.extend_from_slice(&has_pos);
                if u64::from_le_bytes(has_pos) != 0 {
                    read_wire_string(stream, &mut result).await?; // file
                    let mut line_col = [0u8; 16];
                    stream.read_exact(&mut line_col).await?;
                    result.extend_from_slice(&line_col);
                }
                // Traces: count + (position + message) per trace
                let mut count_buf = [0u8; 8];
                stream.read_exact(&mut count_buf).await?;
                result.extend_from_slice(&count_buf);
                let count = u64::from_le_bytes(count_buf) as usize;
                for _ in 0..count {
                    let mut has_pos = [0u8; 8];
                    stream.read_exact(&mut has_pos).await?;
                    result.extend_from_slice(&has_pos);
                    if u64::from_le_bytes(has_pos) != 0 {
                        read_wire_string(stream, &mut result).await?;
                        let mut line_col = [0u8; 16];
                        stream.read_exact(&mut line_col).await?;
                        result.extend_from_slice(&line_col);
                    }
                    read_wire_string(stream, &mut result).await?; // trace message
                }
            }
            STDERR_START_ACTIVITY => {
                // id + level + type + text + fields(count + typed) + parent
                let mut fixed = [0u8; 24]; // id + level + type
                stream.read_exact(&mut fixed).await?;
                result.extend_from_slice(&fixed);
                read_wire_string(stream, &mut result).await?; // text
                // Typed fields
                let mut count_buf = [0u8; 8];
                stream.read_exact(&mut count_buf).await?;
                result.extend_from_slice(&count_buf);
                let count = u64::from_le_bytes(count_buf) as usize;
                for _ in 0..count {
                    let mut type_buf = [0u8; 8];
                    stream.read_exact(&mut type_buf).await?;
                    result.extend_from_slice(&type_buf);
                    if u64::from_le_bytes(type_buf) == 0 {
                        let mut val = [0u8; 8];
                        stream.read_exact(&mut val).await?;
                        result.extend_from_slice(&val);
                    } else {
                        read_wire_string(stream, &mut result).await?;
                    }
                }
                let mut parent = [0u8; 8];
                stream.read_exact(&mut parent).await?;
                result.extend_from_slice(&parent);
            }
            STDERR_STOP_ACTIVITY => {
                let mut id = [0u8; 8];
                stream.read_exact(&mut id).await?;
                result.extend_from_slice(&id);
            }
            STDERR_RESULT => {
                // activity_id + result_type + typed fields
                let mut fixed = [0u8; 16]; // id + type
                stream.read_exact(&mut fixed).await?;
                result.extend_from_slice(&fixed);
                let mut count_buf = [0u8; 8];
                stream.read_exact(&mut count_buf).await?;
                result.extend_from_slice(&count_buf);
                let count = u64::from_le_bytes(count_buf) as usize;
                for _ in 0..count {
                    let mut type_buf = [0u8; 8];
                    stream.read_exact(&mut type_buf).await?;
                    result.extend_from_slice(&type_buf);
                    if u64::from_le_bytes(type_buf) == 0 {
                        let mut val = [0u8; 8];
                        stream.read_exact(&mut val).await?;
                        result.extend_from_slice(&val);
                    } else {
                        read_wire_string(stream, &mut result).await?;
                    }
                }
            }
            STDERR_READ => {
                // Server requesting data: u64 count
                let mut count = [0u8; 8];
                stream.read_exact(&mut count).await?;
                result.extend_from_slice(&count);
            }
            other => {
                // Unknown STDERR type — fall back to timeout-based reading
                // to avoid getting stuck.
                eprintln!(
                    "warning: unknown STDERR message type {other:#x}, falling back to timeout"
                );
                let fallback = match post_last {
                    PostLastRead::Timeout(t) => t,
                    PostLastRead::Nar => DEFAULT_READ_TIMEOUT,
                };
                loop {
                    let mut buf = [0u8; 8192];
                    match tokio::time::timeout(fallback, stream.read(&mut buf)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => result.extend_from_slice(&buf[..n]),
                        Ok(Err(e)) => return Err(e),
                        Err(_) => break,
                    }
                }
                return Ok(result);
            }
        }
    }
}

/// Read a wire-format string (u64 length + content + padding) from a stream,
/// appending all raw bytes to `out`.
async fn read_wire_string(stream: &mut UnixStream, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut len_buf = [0u8; 8];
    stream.read_exact(&mut len_buf).await?;
    out.extend_from_slice(&len_buf);
    let len = u64::from_le_bytes(len_buf) as usize;
    let padded = (len + 7) & !7;
    if padded > 0 {
        let mut content = vec![0u8; padded];
        stream.read_exact(&mut content).await?;
        out.extend_from_slice(&content);
    }
    Ok(())
}

/// Read a wire-format padded string and return its unpadded content.
/// Appends all raw bytes (length prefix + content + padding) to `buf`.
async fn read_nar_token(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<Vec<u8>> {
    let pos = buf.len();
    read_wire_string(stream, buf).await?;
    let len = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap()) as usize;
    Ok(buf[pos + 8..pos + 8 + len].to_vec())
}

/// Read a complete NAR node (recursive) from the stream.
///
/// NAR nodes have the structure: `( type <type> <type-specific fields> )`
/// For directories, the closing `)` doubles as the loop terminator so the
/// function returns immediately after consuming it.
///
/// Returns a boxed future because recursive async functions require
/// indirection to avoid infinitely-sized futures.
fn read_nar_node<'a>(
    stream: &'a mut UnixStream,
    buf: &'a mut Vec<u8>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + 'a>> {
    Box::pin(async move {
        read_nar_token(stream, buf).await?; // "("
        read_nar_token(stream, buf).await?; // "type"
        let type_val = read_nar_token(stream, buf).await?;

        match type_val.as_slice() {
            b"regular" => {
                let next = read_nar_token(stream, buf).await?;
                if next == b"executable" {
                    read_wire_string(stream, buf).await?; // "" (empty marker)
                    read_nar_token(stream, buf).await?; // "contents"
                }
                // `next` was "contents", or we just read "contents" above.
                // Read file data (potentially large — don't extract content).
                read_wire_string(stream, buf).await?;
            }
            b"symlink" => {
                read_nar_token(stream, buf).await?; // "target"
                read_wire_string(stream, buf).await?; // target path
            }
            b"directory" => {
                loop {
                    let tok = read_nar_token(stream, buf).await?;
                    if tok == b")" {
                        // Directory closing paren — node is complete.
                        return Ok(());
                    }
                    // tok is "entry"
                    read_nar_token(stream, buf).await?; // "("
                    read_nar_token(stream, buf).await?; // "name"
                    read_wire_string(stream, buf).await?; // entry name
                    read_nar_token(stream, buf).await?; // "node"
                    read_nar_node(stream, buf).await?; // recursive child
                    read_nar_token(stream, buf).await?; // ")" closing the entry
                }
            }
            other => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown NAR node type: {}", String::from_utf8_lossy(other)),
                ));
            }
        }

        read_nar_token(stream, buf).await?; // ")" closing the node
        Ok(())
    })
}

/// Read a complete NAR archive from the stream by parsing its structure.
///
/// Appends all raw bytes to `buf`. Stops reading exactly when the NAR is
/// complete — no timeout needed.
async fn read_nar_from_stream(stream: &mut UnixStream, buf: &mut Vec<u8>) -> std::io::Result<()> {
    read_nar_token(stream, buf).await?; // "nix-archive-1"
    read_nar_node(stream, buf).await?;
    Ok(())
}

/// Build a test store path and return its full path string.
///
/// Uses `builtins.toFile` which is a Nix built-in that doesn't depend on
/// nixpkgs or NIX_PATH, making this portable across CI environments and
/// flake-based setups.
pub fn build_test_path() -> String {
    let output = std::process::Command::new("nix")
        .args([
            "eval",
            "--raw",
            "--expr",
            r#"builtins.toFile "rio-golden-test" "golden test data\n""#,
        ])
        .output()
        .expect("nix eval must succeed");
    assert!(
        output.status.success(),
        "nix eval failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        std::path::Path::new(&path).exists(),
        "test path does not exist: {path}"
    );
    path
}

/// Build a content-addressed (CA) test store path and return its full path string.
///
/// Uses a fixed-output derivation with `__contentAddressed = true`, which
/// requires the `ca-derivations` experimental feature. The output path is
/// determined by the content hash, not the derivation inputs.
pub fn build_ca_test_path() -> String {
    let output = std::process::Command::new("nix")
        .args([
            "build",
            "--extra-experimental-features",
            "ca-derivations",
            "--impure",
            "--no-link",
            "--print-out-paths",
            "--expr",
            r#"derivation {
                name = "rio-ca-golden";
                builder = "/bin/sh";
                args = ["-c" "echo -n ca-golden-test-data > $out"];
                system = builtins.currentSystem;
                __contentAddressed = true;
                outputHashMode = "flat";
                outputHashAlgo = "sha256";
            }"#,
        ])
        .output()
        .expect("nix build (CA) must succeed");
    assert!(
        output.status.success(),
        "nix build (CA) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        std::path::Path::new(&path).exists(),
        "CA test path does not exist: {path}"
    );
    path
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

/// Dump the NAR content for a store path via `nix-store --dump`.
pub fn dump_nar(store_path: &str) -> Vec<u8> {
    let output = std::process::Command::new("nix-store")
        .args(["--dump", store_path])
        .output()
        .expect("nix-store --dump must succeed");
    assert!(
        output.status.success(),
        "nix-store --dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
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
///
/// Enables `ca-derivations` experimental feature so golden tests can
/// validate content-addressed workflows.
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
        .env(
            "NIX_CONFIG",
            "experimental-features = nix-command flakes ca-derivations",
        )
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

/// Start a dedicated nix-daemon for golden tests.
///
/// Always starts a local daemon for test isolation — tests should not
/// depend on the system daemon's configuration (e.g., whether
/// `ca-derivations` is enabled). Respects `NIX_DAEMON_SOCKET` as an
/// explicit override for CI environments that manage their own daemon.
///
/// Prefer [`shared_daemon_socket`] for tests — it ensures only one daemon
/// is started even when tests run in parallel.
fn get_daemon_socket_inner() -> (String, Option<DaemonGuard>) {
    if let Ok(s) = std::env::var("NIX_DAEMON_SOCKET") {
        if Path::new(&s).exists() {
            return (s, None);
        }
        eprintln!("NIX_DAEMON_SOCKET={s} not found, starting local daemon");
    }
    let (s, g) = start_local_daemon();
    (s, Some(g))
}
