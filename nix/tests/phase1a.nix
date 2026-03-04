# Phase 1a milestone validation: read-only Nix worker protocol over ssh-ng.
#
# Milestones (docs/src/phases/phase1a.md:54-58):
#   - SSH handshake completes successfully
#   - `nix path-info --store ssh-ng://...` returns correct info
#   - `nix store ls --store ssh-ng://...` works
#
# Topology (2 VMs):
#   gateway — rio-store, rio-scheduler, rio-gateway (scheduler unused but
#             required by gateway's startup connect())
#   client  — Nix CLI speaking ssh-ng to gateway
#
# This test does NOT exercise builds. It validates the read-only opcode
# path that Phase 1a shipped: handshake, wopQueryPathInfo, wopIsValidPath,
# wopQueryValidPaths, wopNarFromPath (via `nix store cat`).
#
# Run interactively:
#   nix build .#checks.x86_64-linux.vm-phase1a.driverInteractive
#   ./result/bin/nixos-test-driver
#   >>> start_all(); gateway.shell_interact()
{
  pkgs,
  rio-workspace,
  rioModules,
}:
let
  common = import ./common.nix { inherit pkgs rio-workspace rioModules; };
  inherit (common) busybox busyboxClosure databaseUrl;
in
pkgs.testers.runNixOSTest {
  name = "rio-phase1a";

  nodes = {
    gateway = {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
        common.postgresqlConfig
      ];
      networking.hostName = "gateway";

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty";
        store = {
          enable = true;
          inherit databaseUrl;
        };
        scheduler = {
          enable = true;
          storeAddr = "localhost:9002";
          inherit databaseUrl;
        };
        gateway = {
          enable = true;
          schedulerAddr = "localhost:9001";
          storeAddr = "localhost:9002";
          authorizedKeysPath = "/var/lib/rio/gateway/authorized_keys";
        };
      };

      systemd.tmpfiles.rules = common.gatewayTmpfiles;

      networking.firewall.allowedTCPPorts = [ 2222 ];

      virtualisation = {
        memorySize = 1024;
        diskSize = 2048;
        cores = 4;
      };
    };

    client = common.mkClientNode { gatewayHost = "gateway"; };
  };

  testScript = ''
    start_all()

    # ── Bootstrap ─────────────────────────────────────────────────────
    gateway.wait_for_unit("postgresql.service")
    gateway.wait_for_unit("rio-store.service")
    gateway.wait_for_open_port(9002)
    gateway.wait_for_unit("rio-scheduler.service")
    gateway.wait_for_open_port(9001)

    # SSH key exchange + gateway restart
    ${common.sshKeySetup "gateway"}

    # ── Milestone: SSH handshake ──────────────────────────────────────
    # `nix store info` exercises the ssh-ng SSH layer + Nix protocol
    # handshake (magic exchange, version negotiation, features, STDERR_LAST).
    client.succeed("nix store info --store 'ssh-ng://gateway'")

    # ── Seed store (for read-only queries to return real data) ────────
    client.succeed("ls ${busybox}")
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://gateway' "
        "$(cat ${busyboxClosure}/store-paths)"
    )

    # ── Milestone: `nix path-info` ────────────────────────────────────
    # Exercises wopQueryPathInfo. Response should contain the store path.
    path_info = client.succeed(
        "nix path-info --store 'ssh-ng://gateway' ${busybox}"
    ).strip()
    assert path_info == "${busybox}", f"path-info returned {path_info!r}, expected busybox path"

    # JSON mode: parse and check narHash + narSize are populated.
    import json
    path_info_json = json.loads(client.succeed(
        "nix path-info --json --store 'ssh-ng://gateway' ${busybox}"
    ))
    # Output shape: {"/nix/store/...-busybox": {"narHash": "...", "narSize": N, ...}}
    info = path_info_json["${busybox}"]
    assert info["narHash"].startswith("sha256-"), f"bad narHash: {info['narHash']}"
    assert info["narSize"] > 0, f"bad narSize: {info['narSize']}"
    print(f"path-info: narHash={info['narHash']}, narSize={info['narSize']}")

    # ── Milestone: `nix store ls` ─────────────────────────────────────
    # Exercises wopNarFromPath (fetches NAR, parses directory structure).
    # Busybox has a bin/ directory with the busybox binary + applet symlinks.
    ls_output = client.succeed(
        "nix store ls --store 'ssh-ng://gateway' ${busybox}/bin"
    )
    assert "busybox" in ls_output, f"store ls missing busybox binary: {ls_output}"
    print(f"store ls found {len(ls_output.splitlines())} entries in ${busybox}/bin")

    # ── Bonus: wopIsValidPath via `nix store verify` ──────────────────
    # Verify without --check-contents just does a validity check.
    client.succeed(
        "nix store verify --no-trust --store 'ssh-ng://gateway' ${busybox}"
    )

    # ── Negative: query a nonexistent path ────────────────────────────
    # Should cleanly report the path doesn't exist (not hang or crash).
    # Use a syntactically-valid-but-nonexistent store path (32-char hash).
    client.fail(
        "nix path-info --store 'ssh-ng://gateway' "
        "/nix/store/0000000000000000000000000000000a-nonexistent"
    )
  '';
}
