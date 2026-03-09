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
  coverage ? false,
}:
let
  common = import ./common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };
  inherit (common) busybox;
in
pkgs.testers.runNixOSTest {
  name = "rio-phase1a";

  nodes = {
    # No workers → 9001/9002 firewall entries from mkControlNode are
    # unused cross-VM, but harmless. diskSize=2048 suffices (no builds).
    gateway = common.mkControlNode {
      hostName = "gateway";
      diskSize = 2048;
    };

    client = common.mkClientNode { gatewayHost = "gateway"; };
  };

  testScript = ''
    start_all()

    # ── Bootstrap ─────────────────────────────────────────────────────
    ${common.waitForControlPlane "gateway"}

    # SSH key exchange + gateway restart
    ${common.sshKeySetup "gateway"}

    # ── Milestone: SSH handshake ──────────────────────────────────────
    # `nix store info` exercises the ssh-ng SSH layer + Nix protocol
    # handshake (magic exchange, version negotiation, features, STDERR_LAST).
    client.succeed("nix store info --store 'ssh-ng://gateway'")

    # ── Seed store (for read-only queries to return real data) ────────
    ${common.seedBusybox "gateway"}

    # ── Milestone: `nix path-info` ────────────────────────────────────
    # Exercises wopQueryPathInfo. Response should contain the store path.
    path_info = client.succeed(
        "nix path-info --store 'ssh-ng://gateway' ${busybox}"
    ).strip()
    assert path_info == "${busybox}", f"path-info returned {path_info!r}, expected busybox path"

    # JSON mode: parse and compare narHash + narSize EXACTLY against
    # the client's local store. Round 4 V5: prior check only verified
    # "startswith sha256-" and "> 0" — a WRONG hash from the gateway
    # would pass. Now we compute ground truth locally and compare.
    import json
    # Ground truth from the client's LOCAL store (busybox is in
    # systemPackages so it's registered locally).
    local_json = json.loads(client.succeed(
        "nix path-info --json ${busybox}"
    ))
    local_info = local_json["${busybox}"]
    # Gateway response.
    gateway_json = json.loads(client.succeed(
        "nix path-info --json --store 'ssh-ng://gateway' ${busybox}"
    ))
    gateway_info = gateway_json["${busybox}"]
    # Exact match. If gateway returns a different hash/size, the
    # wopQueryPathInfo handler is corrupting data.
    assert gateway_info["narHash"] == local_info["narHash"], (
        f"narHash MISMATCH: gateway={gateway_info['narHash']!r}, "
        f"local={local_info['narHash']!r}"
    )
    assert gateway_info["narSize"] == local_info["narSize"], (
        f"narSize MISMATCH: gateway={gateway_info['narSize']}, "
        f"local={local_info['narSize']}"
    )
    print(f"path-info EXACT match: narHash={gateway_info['narHash']}, "
          f"narSize={gateway_info['narSize']}")

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

    ${common.collectCoverage "gateway, client"}
  '';
}
