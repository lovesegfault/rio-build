# Shared test-derivation factory.
#
# Two kinds of things live here:
#
# 1. Path literals to `.nix` files that get evaluated IN THE VM via
#    `nix-build --arg busybox '(builtins.storePath ...)' <file>`.
#    These take `{ busybox }:` at the top and use `builtins.currentSystem`.
#    Keep them as separate files — `pkgs.writeText` would obscure the code
#    in VM-side errors (store path instead of filename).
#
# 2. `pkgs.writeText` factories for parameterized derivations where each
#    scenario needs a different marker/sleep. These produce a store path
#    that `nix-build` in the VM can read.
{ pkgs }:
rec {
  # ── Path literals (in-VM nix-build targets) ─────────────────────────

  # 4 parallel leaves + 1 collector. Exercises fanout distribution and
  # FUSE fetch across workers. Phase2a pattern: `nix-build fanout.nix` →
  # rio-root output contains 4 "rio-leaf-N" lines in its stamp file.
  fanout = ./derivations/fanout.nix;

  # A → B → C sequential chain. Each step echoes PHASE2B-LOG-MARKER to
  # stderr → validates the worker LogBatcher → gateway STDERR_NEXT chain.
  # Also does `ls -la ${dep}/` to exercise FUSE readdir.
  chain = ./derivations/chain.nix;

  # A → B → C floating-CA chain (`__contentAddressed = true`). Every
  # step writes marker-independent content to `$out/chain`, so a
  # rebuild of A with a different `marker` arg produces an identical
  # nar_hash → cutoff-compare matches → B+C Skipped on the second
  # submit. Drives vm-ca-cutoff-standalone.
  caChain = ./derivations/ca-chain.nix;

  # Multi-attr set: `all` (chain+solo for critical-path), `bigthing`
  # (pname in env for estimator lookup), `bigblob` (300KiB → chunked).
  sizeclass = ./derivations/sizeclass.nix;

  # Overlay-readdir correctness probe: 5-file dep + consumer that ls's
  # it FIRST (no prior lookup of child names). Asserts count=5.
  # If <5: overlayfs serves readdir from stale dcache (correctness bug).
  multifile = ./derivations/multifile.nix;

  # builtin:fetchurl busybox FOD + raw consumer. Cold-store only.
  # Takes `{ tag, sleepSecs }` at nix-build time via `--arg`.
  coldBootstrap = ./derivations/cold-bootstrap.nix;

  # builtin:fetchurl FOD + raw consumer for the fetcher-split scenario.
  # Same shape as coldBootstrap; url defaults to the scenario's
  # TEST-NET-3 origin (203.0.113.1:80 — passes fetcher-egress, fails
  # builder-egress). One nix-build exercises both dispatch roles.
  fodConsumer = ./derivations/fod-consumer.nix;

  # 50 parallel leaves + 1 collector. Load-test fanout for
  # scheduling.nix:load-50drv. Fanout not linear chain: 50 serial
  # builds at tick=2s ≈ 150-200s; fanout is ~40-60s and exercises
  # bulk-ready dispatch (the actual load concern).
  fiftyFanout = ./derivations/fifty-fanout.nix;

  # Host-side pre-fetch of the busybox for airgapped VM workers. Served
  # via Python http.server on the client VM (see coldBootstrapServer
  # below); cold-bootstrap.nix's url is overridden to http://client:8000/
  # busybox. This gives builtin:fetchurl a real HTTP fetch (same codepath
  # as EKS) without needing internet egress.
  #
  # NOT used via file:// — builtin:fetchurl's file:// handling produces
  # a different NAR hash than http:// for reasons not worth debugging
  # (tested: got sha256-NOALh... vs expected sha256-QrTEn...).
  coldBootstrapBusybox = pkgs.fetchurl {
    url = "http://tarballs.nixos.org/stdenv/x86_64-unknown-linux-gnu/82b583ba2ba2e5706b35dbe23f31362e62be2a9d/busybox";
    hash = "sha256-QrTEnQTBM1Y/qV9odq8irZkQSD9uOMbs2Q5NgCvKCNQ=";
    executable = true;
  };

  # Tiny HTTP server for cold tests. Serves coldBootstrapBusybox at
  # /busybox. Drop into the client VM's NixOS config via imports.
  # Opens firewall port 8000 so the worker (where builtin:fetchurl
  # runs) can reach it.
  coldBootstrapServer = {
    systemd.services.busybox-http = {
      wantedBy = [ "multi-user.target" ];
      script = ''
        mkdir -p /srv
        ln -sf ${coldBootstrapBusybox} /srv/busybox
        cd /srv
        exec ${pkgs.python3}/bin/python3 -m http.server 8000
      '';
    };
    networking.firewall.allowedTCPPorts = [ 8000 ];
  };

  # ── Parameterized factories (pkgs.writeText) ────────────────────────

  # Single trivial leaf. busybox builder, echoes marker to $out.
  # Each scenario can mint its own distinct derivation so builds don't
  # DAG-dedup to the first scenario's result.
  mkTrivial =
    {
      marker,
      sleepSecs ? 0,
    }:
    let
      sleepCmd = pkgs.lib.optionalString (sleepSecs > 0) ''
        ''${busybox}/bin/busybox sleep ${toString sleepSecs}
      '';
    in
    pkgs.writeText "drv-${marker}.nix" ''
      { busybox }:
      derivation {
        name = "rio-test-${marker}";
        builder = "''${busybox}/bin/sh";
        args = [ "-c" '''
          ${sleepCmd}
          echo ${marker} > $out
        ''' ];
        system = builtins.currentSystem;
      }
    '';
}
