{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.scheduler;
  rioLib = import ./_common.nix { inherit lib config; };
in
{
  imports = [ ./common.nix ];

  options.services.rio.scheduler = {
    enable = lib.mkEnableOption "rio-scheduler DAG-aware build scheduler";

    listenAddr = rioLib.mkListenAddrOption 9001 "gRPC listen address for SchedulerService + WorkerService";
    storeAddr = rioLib.mkGrpcAddrOption "store" ''Used for scheduler-side cache checks. E.g., `"localhost:9002"`.'';

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      description = ''
        PostgreSQL connection URL (`RIO_DATABASE_URL`). rio-scheduler
        does NOT migrate on startup — it only verifies the schema is
        current; the store module's `rio-migrate` oneshot applies
        migrations (see the assertion below).
      '';
    };

    metricsAddr = rioLib.mkMetricsOption 9091;

    tickIntervalSecs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 10;
      description = "Housekeeping tick interval in seconds (`RIO_TICK_INTERVAL_SECS`).";
    };

    extraConfig = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = ''
        Extra TOML appended to `/etc/rio/scheduler.toml`. The config loader
        reads this with lower precedence than env vars and CLI. Use for nested
        config that doesn't map to env vars (e.g. `[sla]` tables).
        Example:

            extraConfig = ${"''"}
              [sla]
              default_tier = "best-effort"
            ${"''"};
      '';
    };

    lease = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.submodule {
          options = {
            name = lib.mkOption {
              type = lib.types.str;
              description = "Kubernetes Lease name for leader election (`RIO_LEASE_NAME`).";
            };
            namespace = lib.mkOption {
              type = lib.types.str;
              default = "default";
              description = "Namespace for the Lease object (`RIO_LEASE_NAMESPACE`).";
            };
            kubeconfigPath = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = ''
                Path to kubeconfig for out-of-cluster lease API access (`KUBECONFIG`).
                Leave null for in-cluster config (ServiceAccount token mount).
                The scheduler's `kube::Client::try_default()` tries in-cluster
                first, then `KUBECONFIG`. If both fail, the lease loop exits
                gracefully and the scheduler runs as standby (never dispatches).
              '';
            };
          };
        }
      );
      default = null;
      description = ''
        Kubernetes Lease leader election. When set, the scheduler
        uses a K8s Lease object to coordinate multiple replicas —
        only the lease holder dispatches builds. `null` (default)
        disables leader election: single-replica mode, always the
        leader.

        The holder ID is the hostname (systemd `%H`).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        # The ONLY migration runner on NixOS is the store module's
        # rio-migrate oneshot (the `rio-store migrate` subcommand
        # ships in the store binary). A scheduler-only host — or one
        # pointing at a different database than the store — would
        # assert_current-churn forever with no one ever migrating.
        # Eval-time error instead of silent runtime churn. A per-URL
        # parameterized migrate unit is deferred until a real split
        # topology exists (every current fixture co-locates).
        assertion =
          config.services.rio ? store
          && config.services.rio.store.enable
          && cfg.databaseUrl == config.services.rio.store.databaseUrl;
        message = ''
          services.rio.scheduler needs the rio-migrate oneshot from
          services.rio.store on the SAME host and the SAME databaseUrl
          (scheduler only verifies the schema at startup; the store
          module's rio-migrate unit is the only runner). Enable
          services.rio.store with a matching databaseUrl, or add a
          dedicated migrate unit for the scheduler's database.
        '';
      }
    ];
    # TOML config for settings that don't map to flat env vars (nested
    # arrays like [sla.tiers]). The config layering: compiled defaults <
    # /etc/rio/scheduler.toml < RIO_* env < CLI. So env vars above
    # still override anything here.
    environment.etc."rio/scheduler.toml" = lib.mkIf (cfg.extraConfig != "") {
      text = cfg.extraConfig;
    };

    systemd.services.rio-scheduler = rioLib.mkRioService {
      binary = "rio-scheduler";
      description = "rio-scheduler DAG-aware build scheduler";
      extraAfter = [
        "postgresql.service"
        # Migrations run in the store module's rio-migrate oneshot;
        # startup only asserts the schema is current. After= is a
        # no-op when that unit doesn't exist (scheduler without the
        # store module) — Restart=on-failure then churns until
        # someone migrates.
        "rio-migrate.service"
        # Store connection is non-fatal (scheduler warns + disables cache check),
        # but starting after store is still the common-case ordering.
        "rio-store.service"
      ];
      # Env var naming: the config loader strips `RIO_` then lowercases to
      # match the Config field. `RIO_LISTEN_ADDR` -> `listen_addr`, etc.
      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_STORE__ADDR = cfg.storeAddr;
        RIO_DATABASE_URL = cfg.databaseUrl;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_TICK_INTERVAL_SECS = toString cfg.tickIntervalSecs;
      }
      // lib.optionalAttrs (cfg.lease != null) (
        {
          RIO_LEASE_NAME = cfg.lease.name;
          RIO_LEASE_NAMESPACE = cfg.lease.namespace;
          # The scheduler's lease.rs:105 reads HOSTNAME (not a
          # custom RIO_* var) — matches what K8s injects into
          # pods. systemd doesn't set HOSTNAME by default for
          # services (only login shells via pam_env), so set it
          # explicitly from networking.hostName. In a StatefulSet
          # this would be the pod name (ordinal-suffixed = unique
          # per replica).
          #
          # NOT %H: systemd %-specifiers are only expanded in
          # ExecStart/ExecStop/etc, not in Environment= entries.
          # config.networking.hostName is evaluated at nix-build
          # time, which is correct (static per-VM, not dynamic).
          HOSTNAME = config.networking.hostName;
        }
        // lib.optionalAttrs (cfg.lease.kubeconfigPath != null) {
          KUBECONFIG = cfg.lease.kubeconfigPath;
        }
      );
    };
  };
}
