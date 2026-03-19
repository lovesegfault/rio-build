# Wires fixtures × scenarios into the vmTests attrset.
#
# Returns `{ vm-<scenario>-<fixture> = <runNixOSTest>; }`. Coverage mode:
# same structure, `coverage=true` propagates to fixtures → covEnv in
# service envs → collectCoverage fires.
{
  pkgs,
  rio-workspace,
  rioModules,
  dockerImages,
  nixhelm,
  system,
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

  standalone = import ./fixtures/standalone.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  k3sFull = import ./fixtures/k3s-full.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      dockerImages
      nixhelm
      system
      coverage
      ;
  };

  protocol = import ./scenarios/protocol.nix;
  scheduling = import ./scenarios/scheduling.nix;
  security = import ./scenarios/security.nix;
  observability = import ./scenarios/observability.nix;
  lifecycle = import ./scenarios/lifecycle.nix;
  leader-election = import ./scenarios/leader-election.nix;
  cli = import ./scenarios/cli.nix;
  drvs = import ./lib/derivations.nix { inherit pkgs; };

  # Shared fixture for both scheduling splits — identical VM topology.
  schedulingFixture = standalone {
    workers = {
      wsmall1 = {
        maxBuilds = 2;
        sizeClass = "small";
      };
      wsmall2 = {
        maxBuilds = 2;
        sizeClass = "small";
        # Non-passthrough FUSE: exercises open_files tracking,
        # userspace read(), release(). fuse/ops.rs read() at 33%
        # coverage before this — passthrough bypasses the kernel
        # callback entirely.
        extraServiceEnv = {
          RIO_FUSE_PASSTHROUGH = "false";
        };
      };
      wlarge = {
        maxBuilds = 1;
        sizeClass = "large";
        # maxSilentTime enforcement. Only wlarge: small workers run
        # reassignDrv (25s silent sleep) which would trip a silence
        # threshold. bigthing (the only other drv routed to wlarge)
        # echoes immediately — no silence exposure. The max-silent-time
        # subtest routes silenceDrv here via build_history seed.
        #
        # Worker-side config because the Nix ssh-ng client does NOT
        # send wopSetOptions (protocol 1.38) — client --max-silent-time
        # cannot propagate to the gateway.
        extraServiceEnv = {
          RIO_MAX_SILENT_TIME_SECS = "10";
        };
      };
    };
    extraSchedulerConfig = {
      tickIntervalSecs = 2;
      extraConfig = ''
        [[size_classes]]
        name = "small"
        cutoff_secs = 30.0
        mem_limit_bytes = 17179869184

        [[size_classes]]
        name = "large"
        cutoff_secs = 3600.0
        mem_limit_bytes = 68719476736
      '';
    };
    extraStoreConfig = {
      extraConfig = ''
        [chunk_backend]
        kind = "filesystem"
        base_dir = "/var/lib/rio/store/chunks"
      '';
    };
    extraPackages = [ pkgs.postgresql ];
  };

  # Shared lifecycle module for core/ctrlrestart/recovery/reconnect.
  # Plain k3sFull — no autoscaler env tuning. The autoscaler's default
  # poll/windows (~30s/600s) make it effectively inert in these splits.
  lifecycleMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull { };
  };

  # Autoscale split gets the fast-poll env so the scale-up/down cycle
  # completes within the test window. Same chart/images; only the
  # controller pod's env ConfigMap differs.
  lifecycleAutoscaleMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull {
      extraValues = {
        "controller.extraEnv[0].name" = "RIO_AUTOSCALER_POLL_SECS";
        "controller.extraEnv[0].value" = "3";
        "controller.extraEnv[1].name" = "RIO_AUTOSCALER_SCALE_UP_WINDOW_SECS";
        "controller.extraEnv[1].value" = "3";
        "controller.extraEnv[2].name" = "RIO_AUTOSCALER_MIN_INTERVAL_SECS";
        "controller.extraEnv[2].value" = "3";
        "controller.extraEnv[3].name" = "RIO_AUTOSCALER_SCALE_DOWN_WINDOW_SECS";
        "controller.extraEnv[3].value" = "10";
      };
    };
  };

  leMod = leader-election {
    inherit pkgs common;
    fixture = k3sFull { };
  };
in
{
  # ── New scenario-based tests ────────────────────────────────────────
  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
    };
    cold = false;
  };

  vm-protocol-cold-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
      # Python http.server on :8000 serving the pre-fetched busybox.
      # cold-bootstrap.nix's url is overridden to http://client:8000/
      # busybox — builtin:fetchurl gets a real HTTP fetch (same codepath
      # as EKS) without needing internet egress.
      extraClientModules = [ drvs.coldBootstrapServer ];
    };
    cold = true;
  };

  # ── scheduling splits (2 tests, standalone fixture) ──────────────────
  # Same 3-worker fixture (wsmall1/wsmall2/wlarge + size-classes) for
  # both — the fragment architecture changes what RUNS, not what's BOOTED.
  # fanout→fuse-direct→fuse-slowpath cache-state chain stays in core;
  # sizeclass+reassign are disruptive (psql seed, SIGKILL) → own test.
  vm-scheduling-core-standalone =
    (scheduling {
      inherit pkgs common;
      fixture = schedulingFixture;
    }).mkTest
      {
        name = "core";
        subtests = [
          "fanout"
          "fuse-direct"
          "overlay-readdir"
          "chunks"
          "cgroup"
          "fuse-slowpath"
        ];
      };

  vm-scheduling-disrupt-standalone =
    (scheduling {
      inherit pkgs common;
      fixture = schedulingFixture;
    }).mkTest
      {
        name = "disrupt";
        subtests = [
          "sizeclass"
          "max-silent-time"
          "reassign"
        ];
      };

  vm-security-standalone = security {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
      withPki = true;
      extraPackages = [
        pkgs.grpcurl
        pkgs.grpc-health-probe
        pkgs.postgresql
      ];
    };
  };

  vm-observability-standalone = observability {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker1 = {
          maxBuilds = 1;
        };
        worker2 = {
          maxBuilds = 1;
        };
        worker3 = {
          maxBuilds = 1;
        };
      };
      withOtel = true;
    };
  };

  # ── lifecycle splits (3 tests, k3s-full fixture) ─────────────────────
  # Monolith was ~14min (13 subtests serially after ~4min bootstrap).
  # Split critical path ~8min (autoscale: 238s subtests + 4min boot).
  # The `initial` subtest was dropped — it only existed to seed out_pin
  # early for gc-sweep; gc-sweep now builds its own paths.
  #
  # P0294: ctrlrestart + reconnect splits removed (Build CRD rip).
  # build-crd-flow + build-crd-errors dropped from core.
  vm-lifecycle-core-k3s = lifecycleMod.mkTest {
    name = "core";
    subtests = [
      "health-shared"
      "cancel-cgroup-kill"
      "gc-dry-run"
      "reconciler-replicas"
      "gc-sweep"
      "refs-end-to-end"
    ];
  };

  vm-lifecycle-recovery-k3s = lifecycleMod.mkTest {
    name = "recovery";
    subtests = [ "recovery" ];
  };

  vm-lifecycle-autoscale-k3s = lifecycleAutoscaleMod.mkTest {
    name = "autoscale";
    subtests = [
      "autoscaler"
      "finalizer"
    ];
    # autoscaler subtests ~238s + finalizer's 300s pod-gone timeout.
    globalTimeout = 1200;
  };

  # ── leader-election splits (2 tests, k3s-full fixture) ───────────────
  # ~0 wall-clock savings (4min bootstrap dominates both) but failures
  # in build-during-failover no longer block the stability checks.
  vm-le-stability-k3s = leMod.mkTest {
    name = "stability";
    subtests = [
      "antiAffinity"
      "lease-acquired"
      "stable-leadership"
      "graceful-release"
      "failover"
    ];
  };

  vm-le-build-k3s = leMod.mkTest {
    name = "build";
    subtests = [ "build-during-failover" ];
  };

  # rio-cli had 0% coverage — never invoked by any test. This runs
  # status + create-tenant + list-tenants against the live scheduler's
  # AdminService. ~5min (mostly k3s bring-up).
  vm-cli-k3s = cli {
    inherit pkgs common;
    fixture = k3sFull { };
  };
}
