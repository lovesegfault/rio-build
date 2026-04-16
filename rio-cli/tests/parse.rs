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
        "cancel-build",
        "drain-executor",
        "pool",
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
        "cancel-build",
        "drain-executor",
    ] {
        let out = run_cli(&[sub, "--help"]);
        assert!(out.status.success(), "{sub} --help: {:?}", out.status);
    }
    // bps has nested subcommands — check both levels.
    for sub in [
        &["pool", "--help"][..],
        &["pool", "get", "--help"],
        &["pool", "describe", "--help"],
    ] {
        let out = run_cli(sub);
        assert!(out.status.success(), "{sub:?}: {:?}", out.status);
    }
}

// ---------------------------------------------------------------------------
// Per-subcommand arg shapes
// ---------------------------------------------------------------------------
//
// One row per (argv, should_parse) pair. `should_parse=true` → clap exit
// code ≠ 2 (clap accepted the args; connect-refused exit 1 is fine).
// `should_parse=false` → clap exit 2 (parse error). Case names group by
// subcommand for readable failure reports.
//
// r[verify store.substitute.upstream]

#[rstest::rstest]
// create-tenant: positional name required; gc/cache flags optional.
#[case::create_tenant_min(&["create-tenant", "foo"], true)]
#[case::create_tenant_all_flags(
    &["create-tenant", "foo", "--gc-retention-hours", "24",
      "--gc-max-store-bytes", "1000000", "--cache-token", "tok"],
    true
)]
#[case::create_tenant_missing_name(&["create-tenant"], false)]
// workers: optional --status filter.
#[case::workers_bare(&["workers"], true)]
#[case::workers_status(&["workers", "--status", "alive"], true)]
// builds: --status/--limit optional; --limit must be numeric.
#[case::builds_bare(&["builds"], true)]
#[case::builds_filtered(&["builds", "--status", "active", "--limit", "5"], true)]
#[case::builds_bad_limit(&["builds", "--limit", "notanumber"], false)]
// derivations: positional build_id XOR --all-active (conflicts_with).
// Neither given is a clap-level accept (Option<String>); the handler's
// runtime check fires AFTER connect — smoke.rs covers that path.
#[case::drvs_by_id(&["derivations", "abc-123"], true)]
#[case::drvs_by_id_status(&["derivations", "abc-123", "--status", "Ready"], true)]
#[case::drvs_by_id_stuck(&["derivations", "abc-123", "--stuck"], true)]
#[case::drvs_all_active(&["derivations", "--all-active"], true)]
#[case::drvs_all_active_stuck(&["derivations", "--all-active", "--stuck"], true)]
#[case::drvs_conflict(&["derivations", "abc-123", "--all-active"], false)]
#[case::drvs_neither(&["derivations"], true)]
// logs: positional drv required; --build-id optional.
#[case::logs_min(&["logs", "/nix/store/foo.drv"], true)]
#[case::logs_with_build_id(&["logs", "/nix/store/foo.drv", "--build-id", "uuid"], true)]
#[case::logs_missing_drv(&["logs"], false)]
// gc: --dry-run flag.
#[case::gc_bare(&["gc"], true)]
#[case::gc_dry_run(&["gc", "--dry-run"], true)]
// poison-clear: positional hash required.
#[case::poison_clear_ok(&["poison-clear", "abc123"], true)]
#[case::poison_clear_missing(&["poison-clear"], false)]
// cancel-build: positional id required; --reason optional.
#[case::cancel_min(&["cancel-build", "abc-123"], true)]
#[case::cancel_with_reason(&["cancel-build", "abc-123", "--reason", "stuck"], true)]
#[case::cancel_missing_id(&["cancel-build"], false)]
// drain-executor: positional id required; --force flag.
#[case::drain_min(&["drain-executor", "builder-0"], true)]
#[case::drain_force(&["drain-executor", "builder-0", "--force"], true)]
#[case::drain_missing_id(&["drain-executor"], false)]
// bps: nested. get has default namespace; describe needs a name; bare errors.
#[case::pool_get_default_ns(&["pool", "get"], true)]
#[case::pool_get_ns(&["pool", "get", "-n", "rio-system"], true)]
#[case::pool_describe(&["pool", "describe", "my-pool"], true)]
#[case::pool_describe_missing_name(&["pool", "describe"], false)]
#[case::pool_bare(&["pool"], false)]
// upstream: nested. list/add/remove all need --tenant; add/remove need --url.
#[case::upstream_list(&["upstream", "list", "--tenant", "t1"], true)]
#[case::upstream_list_no_tenant(&["upstream", "list"], false)]
#[case::upstream_add_min(
    &["upstream", "add", "--tenant", "t1", "--url", "https://cache.nixos.org"],
    true
)]
#[case::upstream_add_full(
    &["upstream", "add", "--tenant", "t1", "--url", "https://cache.nixos.org",
      "--priority", "10",
      "--trusted-key", "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
      "--trusted-key", "other:abc=", "--sig-mode", "add"],
    true
)]
#[case::upstream_add_no_url(&["upstream", "add", "--tenant", "t1"], false)]
#[case::upstream_remove(
    &["upstream", "remove", "--tenant", "t1", "--url", "https://cache.nixos.org"],
    true
)]
#[case::upstream_remove_no_url(&["upstream", "remove", "--tenant", "t1"], false)]
#[case::upstream_bare(&["upstream"], false)]
// --json is #[arg(global = true)] — accepted before OR after the subcommand.
#[case::json_before(&["--json", "status"], true)]
#[case::json_after(&["status", "--json"], true)]
#[case::json_mid(&["workers", "--json", "--status", "alive"], true)]
// --scheduler-addr is global.
#[case::sched_addr_before(&["--scheduler-addr", "1.2.3.4:9001", "status"], true)]
#[case::sched_addr_after(&["status", "--scheduler-addr", "1.2.3.4:9001"], true)]
fn subcommand_arg_shapes(#[case] args: &[&str], #[case] should_parse: bool) {
    if should_parse {
        assert_parsed(args);
    } else {
        assert_rejected(args);
    }
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
