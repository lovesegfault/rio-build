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
  drvs = import ./lib/derivations.nix { inherit pkgs; };
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

  vm-scheduling-standalone = scheduling {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        wsmall1 = {
          maxBuilds = 2;
          sizeClass = "small";
        };
        wsmall2 = {
          maxBuilds = 2;
          sizeClass = "small";
        };
        wlarge = {
          maxBuilds = 1;
          sizeClass = "large";
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

  # ── k3s-full scenarios ───────────────────────────────────────────────
  # First boot of the 2-node k3s fixture with full chart. Slow (~4min
  # waitReady), belong in ci-slow.
  vm-lifecycle-k3s = lifecycle {
    inherit pkgs common;
    fixture = k3sFull {
      extraValues = {
        "controller.extraEnv[0].name" = "RIO_AUTOSCALER_POLL_SECS";
        "controller.extraEnv[0].value" = "3";
        "controller.extraEnv[1].name" = "RIO_AUTOSCALER_SCALE_UP_WINDOW_SECS";
        "controller.extraEnv[1].value" = "3";
        "controller.extraEnv[2].name" = "RIO_AUTOSCALER_MIN_INTERVAL_SECS";
        "controller.extraEnv[2].value" = "3";
      };
    };
  };

  vm-leader-election-k3s = leader-election {
    inherit pkgs common;
    fixture = k3sFull { };
  };
}
