//! Live nix-daemon management for golden conformance tests.
//!
//! Spins up an isolated nix-daemon on a temporary Unix socket,
//! performs interactive protocol exchanges, and captures both
//! client and server byte streams for comparison against rio-build.

use std::path::Path;

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

/// Start a fresh nix-daemon for this test.
///
/// Each call starts a NEW daemon — no static, no sharing across tests.
/// This ensures isolation under both nextest (each test in its own process)
/// AND cargo test (all tests share a process). The previous LazyLock-based
/// sharing caused test interdependence under cargo test: add_signatures
/// mutates the daemon's db, and query_path_info then sees the mutation.
///
/// The caller must bind the returned guard (`let (socket, _guard) = ...;`)
/// to keep the daemon alive for the duration of the test.
pub fn fresh_daemon_socket() -> (String, Option<DaemonGuard>) {
    get_daemon_socket_inner()
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
    use rio_test_support::wire_bytes;

    let mut stream = UnixStream::connect(socket_path).await?;
    let mut all_client = Vec::new();
    let mut all_server = Vec::new();

    // --- Handshake phase 1: magic + version ---
    let phase1 = wire_bytes![u64: WORKER_MAGIC_1, u64: PROTOCOL_VERSION];
    stream.write_all(&phase1).await?;
    stream.flush().await?;
    all_client.extend_from_slice(&phase1);

    // Read server magic2 + server version
    let mut srv_phase1 = vec![0u8; 16];
    stream.read_exact(&mut srv_phase1).await?;
    all_server.extend_from_slice(&srv_phase1);

    // --- Handshake phase 2: features exchange ---
    let phase2 = wire_bytes![strings: wire::NO_STRINGS];
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

    // --- Handshake phase 3: CPU affinity + reserveSpace ---
    let phase3 = wire_bytes![u64: 0, u64: 0];
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
    let set_opts = wire_bytes![
        u64: 19, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
        u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
    ];
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
                // Error is a terminal message — the daemon returns to waiting
                // for the next opcode after sending it. Parse the full error
                // payload, then return (don't loop for another STDERR message).
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
                return Ok(result);
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

/// Experimental features enabled for all `nix` CLI invocations in golden tests.
///
/// Test helpers call `nix eval`/`nix build` which require `nix-command`.
/// Locally this is inherited from user nix.conf; in hermetic CI sandboxes
/// there is no user config. Set via NIX_CONFIG so tests are self-hermetic.
const NIX_CONFIG: &str = "experimental-features = nix-command flakes ca-derivations";

/// Return the golden test store path.
///
/// In hermetic CI (Nix build sandboxes), `RIO_GOLDEN_TEST_PATH` is set by
/// flake.nix to a `pkgs.writeText` path that is in the build's input closure
/// — no `nix eval` needed, no writable /nix/var needed. Locally (outside
/// `nix build .#nextest`), the env var is unset and we fall back to
/// `nix eval` + `builtins.toFile`.
pub fn build_test_path() -> String {
    if let Ok(p) = std::env::var("RIO_GOLDEN_TEST_PATH") {
        assert!(
            std::path::Path::new(&p).exists(),
            "RIO_GOLDEN_TEST_PATH={p} does not exist"
        );
        return p;
    }

    let output = std::process::Command::new("nix")
        .env("NIX_CONFIG", NIX_CONFIG)
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
        "nix eval failed: {}\n\
         Hint: set RIO_GOLDEN_TEST_PATH to a precomputed store path to skip this step.",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        std::path::Path::new(&path).exists(),
        "test path does not exist: {path}"
    );
    path
}

/// Return the CA-addressed golden test store path.
///
/// In hermetic CI, `RIO_GOLDEN_CA_PATH` is set by flake.nix to a
/// fixed-output derivation path (FODs don't need ca-derivations, so this
/// builds anywhere). Locally, fall back to building a CA derivation.
pub fn build_ca_test_path() -> String {
    if let Ok(p) = std::env::var("RIO_GOLDEN_CA_PATH") {
        assert!(
            std::path::Path::new(&p).exists(),
            "RIO_GOLDEN_CA_PATH={p} does not exist"
        );
        return p;
    }

    let output = std::process::Command::new("nix")
        .env("NIX_CONFIG", NIX_CONFIG)
        .args([
            "build",
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
        "nix build (CA) failed: {}\n\
         Hint: set RIO_GOLDEN_CA_PATH to a precomputed store path to skip this step.",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        std::path::Path::new(&path).exists(),
        "CA test path does not exist: {path}"
    );
    path
}

/// Compute path info for a store path from its NAR serialization.
///
/// Uses `nix-store --dump` (legacy command, no state dir needed — works in
/// hermetic sandboxes) + in-process SHA-256. Our golden fixture paths have
/// no deriver, no references, no signatures, so we leave those
/// empty/default. The `ca` field is computed from the file content for FOD-
/// style paths, or passed explicitly for paths where we know it.
///
/// If the env var fixtures are unset, falls back to `nix path-info --json`
/// (needs working Nix state) for non-trivial paths the caller didn't
/// precompute.
pub fn query_path_info_json(store_path: &str) -> StorePathEntry {
    use rio_nix::hash::{HashAlgo, NixHash};

    // In hermetic sandboxes (no /nix/var/nix/db), start_local_daemon()
    // registers fixture paths via --load-db with NO deriver/sigs/ca —
    // compute matching metadata here. Outside sandboxes (real db exists),
    // the daemon symlinks the real db and knows the REAL deriver/sigs,
    // so we must query via `nix path-info` to match. This condition
    // mirrors the linked_db check in start_local_daemon().
    let hermetic = !std::path::Path::new("/nix/var/nix/db").exists();
    let is_fixture = std::env::var("RIO_GOLDEN_TEST_PATH")
        .map(|p| p == store_path)
        .unwrap_or(false)
        || std::env::var("RIO_GOLDEN_CA_PATH")
            .map(|p| p == store_path)
            .unwrap_or(false);

    if hermetic && is_fixture {
        let nar = dump_nar(store_path);
        let nar_hash = NixHash::compute(HashAlgo::SHA256, &nar).to_sri();
        let nar_size = nar.len() as u64;

        // For FOD-style paths (our goldenCaPath), ca = "fixed:sha256:<nixbase32>"
        // of the flat file content. For writeText paths (our goldenTestPath),
        // Nix registers no ca (it's derivation-built, not CA-addressed).
        let ca = if std::env::var("RIO_GOLDEN_CA_PATH").as_deref() == Ok(store_path) {
            let content = std::fs::read(store_path).expect("read ca fixture content");
            let flat_hash = NixHash::compute(HashAlgo::SHA256, &content);
            Some(format!("fixed:{}", flat_hash.to_colon()))
        } else {
            None
        };

        return StorePathEntry {
            path: store_path.to_string(),
            deriver: None,
            nar_hash,
            references: vec![],
            // reg_time is skipped in conformance comparisons anyway.
            registration_time: 0,
            nar_size,
            ultimate: false,
            sigs: vec![],
            ca,
        };
    }

    // Fallback: local dev path — use `nix path-info`.
    let output = std::process::Command::new("nix")
        .env("NIX_CONFIG", NIX_CONFIG)
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

    StorePathEntry {
        path: store_path.to_string(),
        deriver: info
            .get("deriver")
            .and_then(|d| d.as_str())
            .map(String::from),
        nar_hash: info
            .get("narHash")
            .and_then(|h| h.as_str())
            .expect("narHash field")
            .to_string(),
        nar_size: info
            .get("narSize")
            .and_then(|n| n.as_u64())
            .expect("narSize field"),
        references: info
            .get("references")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        sigs: info
            .get("signatures")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        ca: info.get("ca").and_then(|c| c.as_str()).map(String::from),
        registration_time: info
            .get("registrationTime")
            .and_then(|t| t.as_u64())
            .unwrap_or(0),
        ultimate: info
            .get("ultimate")
            .and_then(|u| u.as_bool())
            .unwrap_or(false),
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
/// Outside sandboxes: symlinks `/nix/var/nix/*` into a temp dir so the
/// daemon sees the real store db. Inside hermetic build sandboxes
/// (nixbuild.net), `/nix/var/nix` doesn't exist — the symlink step is a
/// no-op and the daemon starts with an empty db. We then register the
/// golden fixture paths via `nix-store --register-validity` so queries
/// find them.
///
/// Enables `ca-derivations` experimental feature so golden tests can
/// validate content-addressed workflows.
pub fn start_local_daemon() -> (String, DaemonGuard) {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    // Symlink real store state into the temp dir (skip daemon-socket and gc-socket).
    // In hermetic sandboxes this loop is a no-op (/nix/var/nix absent).
    let real_state = std::path::Path::new("/nix/var/nix");
    let mut linked_db = false;
    if let Ok(entries) = std::fs::read_dir(real_state) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str != "daemon-socket" && name_str != "gc-socket" {
                let _ = std::os::unix::fs::symlink(entry.path(), temp_dir.path().join(&name));
                if name_str == "db" {
                    linked_db = true;
                }
            }
        }
    }

    // No real db — hermetic sandbox. Register fixture paths so the daemon
    // knows about them. Without this the daemon returns not-found for all
    // queries and conformance comparisons fail.
    if !linked_db {
        register_fixture_paths(temp_dir.path());
    }

    let daemon_sock_dir = temp_dir.path().join("daemon-socket");
    std::fs::create_dir_all(&daemon_sock_dir).expect("create daemon-socket dir");
    let socket = daemon_sock_dir.join("socket");

    let mut child = std::process::Command::new("nix-daemon")
        .env("NIX_STATE_DIR", temp_dir.path())
        .env("NIX_CONFIG", NIX_CONFIG)
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

/// Register golden fixture paths in a temp state dir's db.
///
/// Uses `nix-store --load-db` which reads the same newline-delimited
/// format as `--dump-db` and writes directly to SQLite without touching
/// the filesystem (no chown/chmod). `--register-validity` would try to
/// canonicalize paths which fails on the read-only /nix/store in
/// hermetic build sandboxes. Hash is raw hex (no algo prefix).
fn register_fixture_paths(state_dir: &std::path::Path) {
    use rio_nix::hash::{HashAlgo, NixHash};
    use std::io::Write;

    let fixtures: Vec<String> = ["RIO_GOLDEN_TEST_PATH", "RIO_GOLDEN_CA_PATH"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .filter(|p| std::path::Path::new(p).exists())
        .collect();
    if fixtures.is_empty() {
        return;
    }

    // nix-store --load-db expects per-path stanzas on stdin
    // (same format as --dump-db output):
    //   <path>\n<narHash raw-hex>\n<narSize>\n<deriver>\n<#refs>\n[refs...]
    // Hash is raw hex WITHOUT algo prefix. See Nix src/libstore/globals.cc.
    let mut reg = String::new();
    for p in &fixtures {
        let nar = dump_nar(p);
        let nar_hash = NixHash::compute(HashAlgo::SHA256, &nar);
        reg.push_str(p);
        reg.push('\n');
        reg.push_str(&nar_hash.to_hex()); // raw hex, no "sha256:" prefix
        reg.push('\n');
        reg.push_str(&nar.len().to_string());
        reg.push('\n');
        reg.push('\n'); // empty deriver
        reg.push_str("0\n"); // 0 references
    }

    let mut child = std::process::Command::new("nix-store")
        .env("NIX_STATE_DIR", state_dir)
        .env("NIX_CONFIG", NIX_CONFIG)
        // --load-db writes directly to SQLite without canonicalizing
        // the store path (no chown/chmod). --register-validity would
        // try to chown, which fails on the read-only /nix/store in
        // hermetic build sandboxes.
        .args(["--load-db"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn nix-store --load-db");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(reg.as_bytes())
        .expect("write registration info");
    let out = child.wait_with_output().expect("wait for nix-store");
    assert!(
        out.status.success(),
        "nix-store --load-db failed:\nstdin:\n{reg}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Start a dedicated nix-daemon for golden tests.
///
/// Always starts a local daemon for test isolation — tests should not
/// depend on the system daemon's configuration (e.g., whether
/// `ca-derivations` is enabled). Respects `NIX_DAEMON_SOCKET` as an
/// explicit override for CI environments that manage their own daemon.
///
/// Called via [`fresh_daemon_socket`].
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
