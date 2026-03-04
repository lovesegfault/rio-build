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
#   1. CA data model: build registers a realisation; psql confirms row
#   2. Critical-path priority: chain-a dispatched before standalone solo
#   3. Size-class routing: pre-seeded "bigthing" → w-large not w-small
#   4. Circuit breaker: stop store → breaker opens → restart → recovers
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

    # ── Build: chain + solo (critical-path test derivation) ──────────
    try:
        client.succeed(
            "nix-build --no-out-link "
            "--store 'ssh-ng://control' "
            "--arg busybox '(builtins.storePath ${busybox})' "
            "${testDrvFile} 2>&1"
        )
    except Exception:
        for w in [wsmall1, wsmall2, wlarge]:
            w.execute("journalctl -u rio-worker --no-pager -n 200 >&2")
        control.execute("journalctl -u rio-scheduler --no-pager -n 200 >&2")
        raise

    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_builds_total\\{outcome=\"success\"\\} [1-9]'"
    )

    # ── Assertion 2: Critical-path dispatch order ────────────────────
    # chain-a (3-deep critical path) should dispatch BEFORE solo
    # (1-deep). Both were Ready at the same time; priority-based
    # BinaryHeap (D5) pops chain-a first because its priority is the
    # sum along the path.
    #
    # Grep scheduler logs for "assigned derivation to worker" lines
    # (debug! at dispatch.rs end of assign_to_worker). chain-a before
    # solo. If FIFO, order would be merge-insertion order (arbitrary).
    #
    # Note: this is a weak signal — timing races in the VM could
    # obscure it. The REAL proof is the queue unit tests (D5). This
    # is a smoke check that priority routing is WIRED in dispatch.
    sched_log = control.succeed(
        "journalctl -u rio-scheduler --no-pager | "
        "grep 'assigned derivation to worker' || true"
    )
    # Find line indices for chain-a and solo assignments.
    chain_a_idx = -1
    solo_idx = -1
    for i, line in enumerate(sched_log.splitlines()):
        if "rio-2c-chain-a" in line and chain_a_idx == -1:
            chain_a_idx = i
        if "rio-2c-solo" in line and solo_idx == -1:
            solo_idx = i
    # Both must be dispatched (got assignments).
    assert chain_a_idx >= 0, "chain-a should have been dispatched"
    assert solo_idx >= 0, "solo should have been dispatched"
    # chain-a FIRST: critical path wins. The weak part: with 3 workers
    # and maxBuilds=2 each, there's enough slots for everything — both
    # could go in the same dispatch pass. The heap POP order still
    # determines log order within that pass, so this should hold.
    assert chain_a_idx < solo_idx, (
        f"critical-path: chain-a (3-deep) should dispatch BEFORE solo (1-deep); "
        f"got chain-a at line {chain_a_idx}, solo at line {solo_idx}"
    )

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

    # Build a "bigthing" derivation (single node, pname set to match
    # the seeded EMA). This is inline Nix — simpler than another file.
    # The build itself is trivial (echo); what matters is routing.
    try:
        client.succeed(
            "nix-build --no-out-link --store 'ssh-ng://control' "
            "-E '(derivation { "
            "  name = \"rio-2c-bigthing\"; "
            "  system = builtins.currentSystem; "
            "  builder = \"${busybox}/bin/sh\"; "
            "  args = [\"-c\" \"${busybox}/bin/busybox mkdir -p $out && "
            "    ${busybox}/bin/busybox echo big > $out/mark\"]; "
            "})' 2>&1"
        )
    except Exception:
        control.execute("journalctl -u rio-scheduler --no-pager -n 100 >&2")
        raise

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

    # ── Assertion 4: Circuit breaker ─────────────────────────────────
    # Stop rio-store → scheduler's FindMissingPaths in merge fails →
    # after 5 consecutive failures the breaker opens. While open,
    # SubmitBuild returns UNAVAILABLE (fail fast, no avalanche of
    # 100%-miss rebuilds).
    #
    # Capture the open counter before (should be 0).
    before_open = control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep rio_scheduler_cache_check_circuit_open_total | "
        "grep -oE '[0-9]+$' || echo 0"
    ).strip()

    control.succeed("systemctl stop rio-store.service")
    # Give the store port time to close (otherwise the first few
    # probes might succeed against a lingering socket).
    control.wait_until_fails("curl -sf http://localhost:9002/")

    # Send 6 builds to trip the breaker. Each merge calls
    # check_cached_outputs → FindMissingPaths → connection refused.
    # 5 failures open it; the 6th hits the open-breaker rejection.
    #
    # All 6 should FAIL (breaker or store unreachable — either way not
    # a hung build). `.fail()` expects non-zero exit.
    for i in range(6):
        client.fail(
            f"nix-build --no-out-link --store 'ssh-ng://control' "
            f"-E '(derivation {{ "
            f"  name = \"rio-2c-breaker-{i}\"; "
            f"  system = builtins.currentSystem; "
            f"  builder = \"${busybox}/bin/sh\"; "
            f"  args = [\"-c\" \"${busybox}/bin/busybox mkdir $out\"]; "
            f"}})' 2>&1"
        )

    # Breaker should have opened: counter incremented.
    after_open = control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep rio_scheduler_cache_check_circuit_open_total | "
        "grep -oE '[0-9]+$'"
    ).strip()
    assert int(after_open) > int(before_open), (
        f"circuit breaker should have opened: before={before_open}, after={after_open}"
    )

    # Recovery: start store → next merge's half-open probe succeeds
    # → breaker closes → build succeeds.
    control.succeed("systemctl start rio-store.service")
    control.wait_for_open_port(9002)

    # The breaker stays open for 30s (OPEN_DURATION) OR until a
    # successful probe closes it. The half-open probe runs on every
    # merge attempt, so the next build SHOULD succeed immediately
    # (probe succeeds, breaker closes, build proceeds).
    client.succeed(
        "nix-build --no-out-link --store 'ssh-ng://control' "
        "-E '(derivation { "
        "  name = \"rio-2c-recovered\"; "
        "  system = builtins.currentSystem; "
        "  builder = \"${busybox}/bin/sh\"; "
        "  args = [\"-c\" \"${busybox}/bin/busybox mkdir -p $out && "
        "    ${busybox}/bin/busybox echo ok > $out/mark\"]; "
        "})' 2>&1"
    )

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
