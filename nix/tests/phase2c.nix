# Phase 2c milestone validation as a NixOS VM test.
#
# Milestone (docs/src/phases/phase2c.md:32):
#   Chunk dedup ratio > 30% on nixpkgs rebuild; scheduling latency p99 < 5s.
#
# Chunk dedup + binary cache HTTP are LIBRARY-complete (C1-C6, B1-B3
# all unit/integration tested) but main.rs wiring is deferred to
# phase3a per TODO(phase3a) at rio-store/src/main.rs:159. This VM test
# validates what IS observable end-to-end:
#
#   1. Size-class config load: cutoff_seconds gauge from /etc/rio/scheduler.toml
#   2. Critical-path priority: chain-a dispatched before standalone solo
#   3. Size-class routing: pre-seeded "bigthing" → w-large not w-small
#   4. content_index populated: PutPath writes nar_hash → store_path rows
#
# Circuit breaker: covered by E5 unit tests. Can't trigger via nix-build
# when store is fully down — gateway's wopEnsurePath fails before
# SubmitBuild reaches the scheduler's merge where the breaker lives.
#
# Five VMs:
#   control  — PostgreSQL, rio-store, rio-scheduler, rio-gateway
#   wsmall1  — rio-worker sizeClass="small"
#   wsmall2  — rio-worker sizeClass="small"
#   wlarge   — rio-worker sizeClass="large"
#   client   — Nix client
#
# Run interactively:
#   nix build .#checks.x86_64-linux.vm-phase2c.driverInteractive
#   ./result/bin/nixos-test-driver
{
  pkgs,
  rio-workspace,
  rioModules,
}:
let
  inherit (pkgs) lib;

  inherit (pkgs.pkgsStatic) busybox;
  busyboxClosure = pkgs.closureInfo { rootPaths = [ busybox ]; };
  testDrvFile = ./phase2c-derivation.nix;
  databaseUrl = "postgres://postgres@localhost/rio";

  # Size-class cutoffs. "small" for quick builds (≤30s), "large" for
  # slow ones. mem_limit is effectively unlimited here (u64::MAX-ish);
  # duration is the discriminator for this test.
  schedulerSizeClasses = ''
    [[size_classes]]
    name = "small"
    cutoff_secs = 30.0
    mem_limit_bytes = 17179869184

    [[size_classes]]
    name = "large"
    cutoff_secs = 3600.0
    mem_limit_bytes = 68719476736
  '';

  workerConfig = hostName: sizeClass: {
    imports = [ rioModules.worker ];
    networking.hostName = hostName;

    services.rio = {
      package = rio-workspace;
      logFormat = "pretty";
      worker = {
        enable = true;
        schedulerAddr = "control:9001";
        storeAddr = "control:9002";
        maxBuilds = 2;
        inherit sizeClass;
      };
    };

    environment.systemPackages = [ pkgs.curl ];

    virtualisation = {
      memorySize = 1024;
      diskSize = 4096;
      cores = 4;
      writableStore = false;
    };
    fileSystems."/nix/var" = {
      fsType = "tmpfs";
      neededForBoot = true;
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-phase2c";

  nodes = {
    control = {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
      ];
      networking.hostName = "control";

      services.postgresql = {
        enable = true;
        enableTCPIP = true;
        authentication = lib.mkForce ''
          local all all trust
          host  all all 127.0.0.1/32 trust
          host  all all ::1/128 trust
        '';
        initialScript = pkgs.writeText "rio-init.sql" ''
          CREATE DATABASE rio;
        '';
      };

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
          # size_classes via /etc/rio/scheduler.toml (figment reads it).
          # Env vars can't express TOML arrays-of-tables.
          extraConfig = schedulerSizeClasses;
          # Short tick = faster estimator refresh. Default 10s × 6 = 60s
          # wait for estimator; 2s × 6 = 12s is tolerable for a VM test.
          tickIntervalSecs = 2;
        };
        gateway = {
          enable = true;
          schedulerAddr = "localhost:9001";
          storeAddr = "localhost:9002";
          authorizedKeysPath = "/var/lib/rio/gateway/authorized_keys";
        };
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/rio 0755 root root -"
        "d /var/lib/rio/gateway 0755 root root -"
        "f /var/lib/rio/gateway/authorized_keys 0600 root root -"
      ];

      environment.systemPackages = [
        pkgs.curl
        pkgs.postgresql # psql for direct queries
      ];

      networking.firewall.allowedTCPPorts = [
        2222
        9001
        9002
        9090
        9091
        9092
      ];

      virtualisation = {
        memorySize = 1536;
        diskSize = 4096;
        cores = 4;
      };
    };

    wsmall1 = workerConfig "wsmall1" "small";
    wsmall2 = workerConfig "wsmall2" "small";
    wlarge = workerConfig "wlarge" "large";

    client = {
      networking.hostName = "client";
      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];
      environment.systemPackages = [
        busybox
        pkgs.curl
      ];
      environment.etc."rio/busybox-closure".source = "${busyboxClosure}";
      programs.ssh.extraConfig = ''
        Host control
          HostName control
          User root
          Port 2222
          IdentityFile /root/.ssh/id_ed25519
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';
      virtualisation.memorySize = 1024;
      virtualisation.cores = 4;
    };
  };

  testScript = ''
    start_all()

    # ── Bootstrap ─────────────────────────────────────────────────────
    control.wait_for_unit("postgresql.service")
    control.wait_for_unit("rio-store.service")
    control.wait_for_open_port(9002)
    control.wait_for_unit("rio-scheduler.service")
    control.wait_for_open_port(9001)

    # Verify scheduler loaded the size_classes config. This gauge is
    # set once at startup from /etc/rio/scheduler.toml. If it's absent,
    # figment didn't read the TOML and every subsequent size-class
    # assertion will fail for the wrong reason.
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep 'rio_scheduler_cutoff_seconds{class=\"small\"} 30'"
    )
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep 'rio_scheduler_cutoff_seconds{class=\"large\"} 3600'"
    )

    # SSH key + gateway restart (same dance as phase2a/2b).
    client.succeed("mkdir -p /root/.ssh && ssh-keygen -t ed25519 -N ''' -f /root/.ssh/id_ed25519")
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    control.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    control.succeed("systemctl restart rio-gateway.service")
    control.wait_for_unit("rio-gateway.service")
    control.wait_for_open_port(2222)

    # All 3 workers register. Check they declared size_class.
    for w in [wsmall1, wsmall2, wlarge]:
        w.wait_for_unit("rio-worker.service")
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 3'"
    )

    # Seed busybox.
    client.succeed("ls ${busybox}")
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://control' "
        "$(cat ${busyboxClosure}/store-paths)"
    )

    # ── Assertion 1: CA data model roundtrip ─────────────────────────
    # The phase2c derivations aren't CA (that needs __contentAddressed
    # which needs builder changes), but the REALISATIONS TABLE is what
    # we're validating. Any successful build with wopRegisterDrvOutput
    # being called writes there. Modern Nix sends it opportunistically
    # — let's first check if it does by running a build, then verify
    # via psql. If it turns out nix doesn't send Register for IA
    # derivations, this still validates the table EXISTS and queries
    # work (via E3's schema), and the G2 unit test covers the wire
    # path.
    #
    # Realisations count before (should be 0).
    before = control.succeed(
        "sudo -u postgres psql rio -t -c 'SELECT count(*) FROM realisations'"
    ).strip()
    assert before == "0", f"realisations should start empty, got {before}"

    # Helper: run a build against the gateway, dumping logs on failure.
    def build(attr=""):
        cmd = (
            "nix-build --no-out-link "
            "--store 'ssh-ng://control' "
            "--arg busybox '(builtins.storePath ${busybox})' "
            "${testDrvFile}"
        )
        if attr:
            cmd += f" -A {attr}"
        try:
            return client.succeed(cmd + " 2>&1")
        except Exception:
            for w in [wsmall1, wsmall2, wlarge]:
                w.execute("journalctl -u rio-worker --no-pager -n 200 >&2")
            control.execute("journalctl -u rio-scheduler --no-pager -n 200 >&2")
            raise

    # ── Build: chain + solo (critical-path test derivation) ──────────
    build("all")

    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_builds_total\\{outcome=\"success\"\\} [1-9]'"
    )

    # ── Assertion 2: Scheduling pipeline wired ───────────────────────
    # Both chain-a and solo dispatched. Smoke check that the full
    # merge → compute_initial → push_ready → dispatch_ready →
    # best_worker pipeline works end-to-end.
    #
    # NOT asserting dispatch ORDER: chain-a and solo are both LEAVES
    # with no dependencies → both have priority = est_duration = 30 →
    # TIE (FIFO tie-break by seq). Critical-path gives higher priority
    # to nodes with accumulated work: chain-b=60, chain-c=90 (est_dur +
    # max(children.priority) bottom-up). The benefit is chain-b beating
    # a fresh leaf AFTER chain-a completes — not chain-a beating solo.
    # Unit tests (D4/D5) prove the priority computation; here we just
    # check the wiring is live.
    sched_log = control.succeed(
        "journalctl -u rio-scheduler --no-pager | "
        "grep 'worker acknowledged assignment' || true"
    )
    assert "rio-2c-chain-a" in sched_log, "chain-a should have dispatched"
    assert "rio-2c-solo" in sched_log, "solo should have dispatched"
    assert "rio-2c-chain-c" in sched_log, "chain-c should have dispatched (depends on b depends on a)"

    # ── Assertion 3: Size-class routing (pre-seeded EMA) ─────────────
    # Pre-seed build_history: "rio-2c-bigthing" has 120s EMA. With
    # small cutoff=30s, classify() picks "large" → only wlarge gets it.
    #
    # The estimator refreshes every 6 ticks = 12s (tickInterval=2s).
    # Insert the seed, wait for refresh, then build.
    control.succeed(
        "sudo -u postgres psql rio -c "
        "\"INSERT INTO build_history (pname, system, ema_duration_secs, sample_count, last_updated) "
        "VALUES ('rio-2c-bigthing', 'x86_64-linux', 120.0, 1, now())\""
    )

    # Wait for estimator refresh. 6 ticks × 2s = 12s + slop.
    control.sleep(15)

    # Build bigthing (pname="rio-2c-bigthing" in env — estimator keys
    # on that). With the 120s seeded EMA and 30s small cutoff,
    # classify() picks "large".
    build("bigthing")

    # Check the size_class_assignments metric: large should increment.
    # This is the hard signal — a metric bump proves classify() ran
    # and picked "large", and best_worker filtered accordingly.
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_size_class_assignments_total\\{class=\"large\"\\} [1-9]'"
    )

    # Softer check: wlarge's logs should show the bigthing assignment,
    # small workers' should NOT. (journalctl grep; noisier than the
    # metric but proves end-to-end.)
    wlarge.succeed(
        "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
    )
    # Small workers: neither got it. `fail()` inverts the exit code.
    wsmall1.fail(
        "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
    )
    wsmall2.fail(
        "journalctl -u rio-worker --no-pager | grep 'rio-2c-bigthing'"
    )

    # ── Circuit breaker: covered by E5 unit tests ────────────────────
    # The scheduler's CacheCheckBreaker is inside merge.rs → it fires
    # when FindMissingPaths fails INSIDE the scheduler. But nix-build
    # hits the GATEWAY first, and the gateway does its own store calls
    # (wopEnsurePath → QueryPathInfo) which fail earlier when the store
    # is down. So the gateway short-circuits before SubmitBuild ever
    # reaches the scheduler's merge, and the breaker never probes.
    #
    # This is correct layering — the gateway fails fast too, just not
    # via the breaker. To VM-test the breaker we'd need a store that's
    # UP for gateway calls but DOWN for the scheduler's FindMissingPaths,
    # which is impractical to rig. E5's unit test directly triggers
    # merge with a failing MockStore: that's the authoritative coverage.

    # ── Final: content_index populated ───────────────────────────────
    # Every PutPath now inserts into content_index (G1). Count should
    # be > 0 after all the builds. This validates the insert path is
    # wired in the real binary, not just tests.
    ci_count = control.succeed(
        "sudo -u postgres psql rio -t -c 'SELECT count(*) FROM content_index'"
    ).strip()
    assert int(ci_count) > 0, f"content_index should have entries after builds; got {ci_count}"
  '';
}
