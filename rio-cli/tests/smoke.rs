//! Smoke test: every rio-cli subcommand connects to a live (mock)
//! AdminService and exits 0.
//!
//! This does NOT assert on output formatting — that's what cli.nix
//! checks against a real scheduler. This proves the binary:
//!   1. parses each subcommand's args
//!   2. connects over gRPC (plaintext — no RIO_TLS__* set)
//!   3. issues the right RPC (server sees a call)
//!   4. drains streams to completion (Logs, Gc)
//!   5. exits 0
//!
//! Server is `MockAdmin` — empty-but-valid responses, no PG, no actor.
//! Runs in ~1s total. The full stack (real scheduler + PG + actor)
//! is in rio-scheduler/src/admin/tests.rs and cli.nix.
//!
//! Binary invocation: `CARGO_BIN_EXE_rio-cli` is set by cargo for
//! integration tests of the crate's own binaries. No `assert_cmd`
//! needed — `std::process::Command` is sufficient for "did it exit 0".

use std::process::Command;

use rio_test_support::grpc::spawn_mock_admin;

/// Invoke rio-cli with `args` pointed at `addr`. Returns (status, stdout, stderr).
///
/// Each subcommand is a separate process, not a library call: rio-cli
/// is binary-only (no lib.rs), and its config loading reads process env.
/// Subprocess isolation also means one test can't poison the next via
/// the `init_client_tls` OnceLock.
///
/// BLOCKING call — tests MUST use `#[tokio::test(flavor = "multi_thread")]`.
/// On the default current-thread runtime, `.output()` blocks the reactor
/// thread that also drives the in-process gRPC server: the subprocess's
/// RPC never sees a response and the CLI's 30s RPC_TIMEOUT fires.
/// Multi-thread puts the server accept loop on a separate worker.
fn run_cli(
    addr: &std::net::SocketAddr,
    args: &[&str],
) -> (std::process::ExitStatus, String, String) {
    // RIO_TLS__* deliberately NOT set: MockAdmin is plaintext.
    // `load_client_tls` on a default TlsConfig (all None) returns
    // Ok(None) → `init_client_tls(None)` → plaintext channel.
    let out = Command::new(env!("CARGO_BIN_EXE_rio-cli"))
        .args(args)
        .env_remove("RIO_TLS__CERT_PATH")
        .env_remove("RIO_TLS__KEY_PATH")
        .env_remove("RIO_TLS__CA_PATH")
        .env("RIO_SCHEDULER_ADDR", addr.to_string())
        .output()
        .expect("spawn rio-cli");
    (
        out.status,
        String::from_utf8(out.stdout).expect("rio-cli stdout is utf8"),
        String::from_utf8(out.stderr).expect("rio-cli stderr is utf8"),
    )
}

/// Assert exit 0. Includes stdout+stderr in the panic message — if the
/// CLI errored, the gRPC code/message is in stderr and that's the
/// actual diagnostic.
#[track_caller]
fn assert_ok(sub: &str, (status, stdout, stderr): (std::process::ExitStatus, String, String)) {
    assert!(
        status.success(),
        "{sub}: exit {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unary_subcommands_exit_ok() -> anyhow::Result<()> {
    let (_admin, addr, _handle) = spawn_mock_admin().await?;

    // These are all unary RPCs via the rpc() helper. One per RPC —
    // a panic names which one failed.
    assert_ok("status", run_cli(&addr, &["status"]));
    assert_ok("list-tenants", run_cli(&addr, &["list-tenants"]));
    assert_ok("create-tenant", run_cli(&addr, &["create-tenant", "smoke"]));
    assert_ok("workers", run_cli(&addr, &["workers"]));
    assert_ok("builds", run_cli(&addr, &["builds"]));
    assert_ok(
        "builds --status",
        run_cli(&addr, &["builds", "--status", "active", "--limit", "5"]),
    );
    assert_ok("cutoffs", run_cli(&addr, &["cutoffs"]));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn poison_clear_passes_hash_through() -> anyhow::Result<()> {
    let (admin, addr, _handle) = spawn_mock_admin().await?;

    let hash = "deadbeef00000000000000000000000000000000000000000000000000000000";
    assert_ok("poison-clear", run_cli(&addr, &["poison-clear", hash]));

    // MockAdmin records the drv_hash it received. Proves the positional
    // arg is wired correctly (not swapped with a flag, not truncated).
    let calls = admin.clear_poison_calls.read().unwrap();
    assert_eq!(calls.as_slice(), &[hash.to_string()]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn json_flag_produces_valid_json() -> anyhow::Result<()> {
    let (_admin, addr, _handle) = spawn_mock_admin().await?;

    // workers --json: cli.nix asserts `jq -e '.workers | length'` parses.
    // Prove the shape here too — deserializes as an object with a
    // `workers` array key, not a bare array or a scalar.
    let (status, stdout, stderr) = run_cli(&addr, &["workers", "--json"]);
    assert!(status.success(), "workers --json: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("workers --json not valid JSON: {e}\n{stdout}"));
    assert!(v.get("workers").is_some_and(|w| w.is_array()));

    // builds --json: same shape check.
    let (status, stdout, stderr) = run_cli(&addr, &["builds", "--json"]);
    assert!(status.success(), "builds --json: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout)?;
    assert!(v.get("builds").is_some_and(|b| b.is_array()));
    assert!(v.get("total_count").is_some());

    // status --json: the flattened summary+workers+builds shape.
    let (status, stdout, stderr) = run_cli(&addr, &["status", "--json"]);
    assert!(status.success(), "status --json: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout)?;
    assert!(v.get("total_workers").is_some()); // flattened StatusJson field
    assert!(v.get("workers").is_some_and(|w| w.is_array()));

    // cutoffs --json: named key (not bare array), same as workers/builds.
    let (status, stdout, stderr) = run_cli(&addr, &["cutoffs", "--json"]);
    assert!(status.success(), "cutoffs --json: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout)?;
    assert!(v.get("classes").is_some_and(|c| c.is_array()));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_drains_stream_and_prints_bytes() -> anyhow::Result<()> {
    let (_admin, addr, _handle) = spawn_mock_admin().await?;

    // MockAdmin's get_build_logs sends one chunk with b"mock log line".
    // The CLI writes each line raw + newline.
    let (status, stdout, stderr) = run_cli(&addr, &["logs", "/nix/store/abc-foo.drv"]);
    assert!(status.success(), "logs: {stderr}");
    assert_eq!(stdout, "mock log line\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_drains_stream_to_completion() -> anyhow::Result<()> {
    let (_admin, addr, _handle) = spawn_mock_admin().await?;

    // MockAdmin's trigger_gc sends one is_complete=true frame. The CLI
    // should print the completion summary and NOT the "closed without
    // is_complete" warning (which goes to stderr).
    let (status, stdout, stderr) = run_cli(&addr, &["gc", "--dry-run"]);
    assert!(status.success(), "gc: {stderr}");
    assert!(stdout.contains("GC dry-run complete"), "stdout: {stdout}");
    assert!(
        !stderr.contains("closed without is_complete"),
        "unexpected warning for clean stream close: {stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_failure_errors_cleanly() -> anyhow::Result<()> {
    // Port 1 is never listened on — connect_channel should fail fast
    // (rio-proto has a 10s CONNECT_TIMEOUT, and TCP to :1 is refused
    // immediately). Exit nonzero, error to stderr, nothing on stdout.
    let bad: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
    let (status, stdout, stderr) = run_cli(&bad, &["status"]);
    assert!(!status.success(), "expected failure on refused connect");
    assert!(
        stdout.is_empty(),
        "no partial output on connect fail: {stdout}"
    );
    assert!(stderr.contains("connect to scheduler"), "stderr: {stderr}");
    Ok(())
}
