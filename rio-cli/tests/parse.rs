//! clap parse tests — every subcommand's args parse without a server.
//!
//! smoke.rs proves end-to-end (parse → connect → RPC → exit 0) against
//! a MockAdmin. These tests prove just the clap layer: positional vs
//! flag wiring, default values, global flags, subcommand nesting. Runs
//! in <10ms — no subprocess spawn, no gRPC.
//!
//! Uses `--help` round-trips: if clap builds the help string without
//! panicking, the `Parser`/`Subcommand` derives are internally
//! consistent (no duplicate flag names, no conflicting `#[arg]`
//! attrs). Then per-subcommand invocations prove each arg shape.

use std::process::Command;

/// Run rio-cli with `args`. No server — RIO_SCHEDULER_ADDR points at
/// an always-refused port so any subcommand that tries to connect
/// fails fast. These tests assert on parse success (exit 2 on clap
/// parse error) or --help output, not on RPC behavior.
fn run_cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rio-cli"))
        .args(args)
        .env_remove("RIO_TLS__CERT_PATH")
        .env_remove("RIO_TLS__KEY_PATH")
        .env_remove("RIO_TLS__CA_PATH")
        .env("RIO_SCHEDULER_ADDR", "127.0.0.1:1")
        .output()
        .expect("spawn rio-cli")
}

/// Assert this invocation got past clap. Exit 2 is clap's parse-error
/// code; anything else (0 for --help, 1 for connect-refused) means
/// clap accepted the args.
#[track_caller]
fn assert_parsed(args: &[&str]) {
    let out = run_cli(args);
    let stderr = String::from_utf8(out.stderr).expect("rio-cli stderr is utf8");
    assert_ne!(
        out.status.code(),
        Some(2),
        "clap rejected {args:?}:\n{stderr}"
    );
}

/// Assert this invocation failed at the clap layer (exit 2).
#[track_caller]
fn assert_rejected(args: &[&str]) {
    let out = run_cli(args);
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected clap parse error for {args:?}, got {:?}",
        out.status
    );
}

// ---------------------------------------------------------------------------
// --help round-trips: clap derives are internally consistent
// ---------------------------------------------------------------------------

#[test]
fn top_level_help_lists_all_subcommands() {
    let out = run_cli(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).expect("rio-cli stdout is utf8");
    // Every Cmd variant's kebab-case name. If one is missing, the
    // #[derive(Subcommand)] dropped a variant somewhere.
    for sub in [
        "create-tenant",
        "list-tenants",
        "status",
        "workers",
        "builds",
        "logs",
        "gc",
        "poison-clear",
        "drain-executor",
        "cutoffs",
        "wps",
        "upstream",
    ] {
        assert!(help.contains(sub), "--help missing {sub}:\n{help}");
    }
}

#[test]
fn per_subcommand_help_renders() {
    // clap --help exit 0 means the Args/Subcommand derives compiled
    // cleanly AND the help template rendered without a format panic.
    for sub in [
        "create-tenant",
        "list-tenants",
        "status",
        "workers",
        "builds",
        "logs",
        "gc",
        "poison-clear",
        "drain-executor",
        "cutoffs",
    ] {
        let out = run_cli(&[sub, "--help"]);
        assert!(out.status.success(), "{sub} --help: {:?}", out.status);
    }
    // wps has nested subcommands — check both levels.
    for sub in [
        &["wps", "--help"][..],
        &["wps", "get", "--help"],
        &["wps", "describe", "--help"],
    ] {
        let out = run_cli(sub);
        assert!(out.status.success(), "{sub:?}: {:?}", out.status);
    }
}

// ---------------------------------------------------------------------------
// Per-subcommand arg shapes
// ---------------------------------------------------------------------------

#[test]
fn create_tenant_positional_and_flags() {
    assert_parsed(&["create-tenant", "foo"]);
    assert_parsed(&[
        "create-tenant",
        "foo",
        "--gc-retention-hours",
        "24",
        "--gc-max-store-bytes",
        "1000000",
        "--cache-token",
        "tok",
    ]);
    // Missing required positional.
    assert_rejected(&["create-tenant"]);
}

#[test]
fn workers_optional_status_filter() {
    assert_parsed(&["workers"]);
    assert_parsed(&["workers", "--status", "alive"]);
}

#[test]
fn builds_status_and_limit() {
    assert_parsed(&["builds"]);
    assert_parsed(&["builds", "--status", "active", "--limit", "5"]);
    // --limit must be numeric.
    assert_rejected(&["builds", "--limit", "notanumber"]);
}

#[test]
fn logs_positional_drv_and_optional_build_id() {
    assert_parsed(&["logs", "/nix/store/foo.drv"]);
    assert_parsed(&["logs", "/nix/store/foo.drv", "--build-id", "uuid"]);
    assert_rejected(&["logs"]);
}

#[test]
fn gc_dry_run_flag() {
    assert_parsed(&["gc"]);
    assert_parsed(&["gc", "--dry-run"]);
}

#[test]
fn poison_clear_requires_hash() {
    assert_parsed(&["poison-clear", "abc123"]);
    assert_rejected(&["poison-clear"]);
}

#[test]
fn drain_worker_positional_and_force() {
    assert_parsed(&["drain-executor", "builder-0"]);
    assert_parsed(&["drain-executor", "builder-0", "--force"]);
    assert_rejected(&["drain-executor"]);
}

#[test]
fn wps_nested_subcommands() {
    // wps get has a default namespace.
    assert_parsed(&["wps", "get"]);
    assert_parsed(&["wps", "get", "-n", "rio-system"]);
    // wps describe needs a name.
    assert_parsed(&["wps", "describe", "my-wps"]);
    assert_rejected(&["wps", "describe"]);
    // bare `wps` with no subcommand is a clap error.
    assert_rejected(&["wps"]);
}

// r[verify store.substitute.upstream]
#[test]
fn upstream_nested_subcommands() {
    // list needs --tenant.
    assert_parsed(&["upstream", "list", "--tenant", "t1"]);
    assert_rejected(&["upstream", "list"]);
    // add needs --tenant + --url; priority/sig-mode have defaults.
    assert_parsed(&[
        "upstream",
        "add",
        "--tenant",
        "t1",
        "--url",
        "https://cache.nixos.org",
    ]);
    assert_parsed(&[
        "upstream",
        "add",
        "--tenant",
        "t1",
        "--url",
        "https://cache.nixos.org",
        "--priority",
        "10",
        "--trusted-key",
        "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
        "--trusted-key",
        "other:abc=",
        "--sig-mode",
        "add",
    ]);
    assert_rejected(&["upstream", "add", "--tenant", "t1"]);
    // remove needs both.
    assert_parsed(&[
        "upstream",
        "remove",
        "--tenant",
        "t1",
        "--url",
        "https://cache.nixos.org",
    ]);
    assert_rejected(&["upstream", "remove", "--tenant", "t1"]);
    // bare `upstream` is a clap error.
    assert_rejected(&["upstream"]);
}

#[test]
fn global_json_flag_accepted_anywhere() {
    // --json is #[arg(global = true)] — accepted before OR after the
    // subcommand. clap handles this, but it's worth locking in: the
    // docs show both forms.
    assert_parsed(&["--json", "status"]);
    assert_parsed(&["status", "--json"]);
    assert_parsed(&["workers", "--json", "--status", "alive"]);
}

#[test]
fn global_scheduler_addr_flag() {
    assert_parsed(&["--scheduler-addr", "1.2.3.4:9001", "status"]);
    assert_parsed(&["status", "--scheduler-addr", "1.2.3.4:9001"]);
}

#[test]
fn no_subcommand_errors() {
    // CliArgs.cmd is Option<Cmd> — clap accepts no subcommand (exit 0
    // from clap's POV), but main() returns an anyhow error (exit 1,
    // not 2). Locks in the "no subcommand given" message.
    let out = run_cli(&[]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).expect("rio-cli stderr is utf8");
    assert!(stderr.contains("no subcommand"), "stderr: {stderr}");
}

#[test]
fn unknown_subcommand_rejected() {
    assert_rejected(&["nope"]);
}
