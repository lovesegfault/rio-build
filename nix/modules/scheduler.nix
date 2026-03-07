{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.scheduler;
in
{
  imports = [ ./common.nix ];

  options.services.rio.scheduler = {
    enable = lib.mkEnableOption "rio-scheduler DAG-aware build scheduler";

    listenAddr = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:9001";
      description = "gRPC listen address for SchedulerService + WorkerService (`RIO_LISTEN_ADDR`).";
    };

    storeAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-store gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        Used for scheduler-side cache checks. E.g., `"localhost:9002"`.
      '';
    };

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      description = ''
        PostgreSQL connection URL (`RIO_DATABASE_URL`).
        rio-scheduler applies migrations (sqlx migrate) on startup.
      '';
    };

    metricsAddr = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:9091";
      description = "Prometheus metrics listen address (`RIO_METRICS_ADDR`).";
    };

    tickIntervalSecs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 10;
      description = "Housekeeping tick interval in seconds (`RIO_TICK_INTERVAL_SECS`).";
    };

    logS3Bucket = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        S3 bucket for build-log gzip flush (`RIO_LOG_S3_BUCKET`).
        `null` = flush disabled; logs are ring-buffer-only (lost on restart,
        but still live-servable while running).
      '';
    };

    logS3Prefix = lib.mkOption {
      type = lib.types.str;
      default = "logs";
      description = "S3 key prefix for build logs (`RIO_LOG_S3_PREFIX`).";
    };

    extraConfig = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = ''
        Extra TOML appended to `/etc/rio/scheduler.toml`. figment reads
        this with lower precedence than env vars and CLI. Use for nested
        config that doesn't map to env vars (e.g. `[[size_classes]]`
        arrays). Example:

            extraConfig = ${"''"}
              [[size_classes]]
              name = "small"
              cutoff_secs = 30.0
              mem_limit_bytes = 1073741824
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
    # TOML config for settings that don't map to flat env vars (nested
    # arrays like size_classes). figment layers: compiled defaults <
    # /etc/rio/scheduler.toml < RIO_* env < CLI. So env vars above
    # still override anything here.
    environment.etc."rio/scheduler.toml" = lib.mkIf (cfg.extraConfig != "") {
      text = cfg.extraConfig;
    };

    systemd.services.rio-scheduler = {
      description = "rio-scheduler DAG-aware build scheduler";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network-online.target"
        "postgresql.service"
        # Store connection is non-fatal (scheduler warns + disables cache check),
        # but starting after store is still the common-case ordering.
        "rio-store.service"
      ];
      wants = [ "network-online.target" ];

      # Env var naming: figment strips `RIO_` then lowercases to match
      # the Config field. `RIO_LISTEN_ADDR` -> `listen_addr`, etc.
      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_STORE_ADDR = cfg.storeAddr;
        RIO_DATABASE_URL = cfg.databaseUrl;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_TICK_INTERVAL_SECS = toString cfg.tickIntervalSecs;
        RIO_LOG_S3_PREFIX = cfg.logS3Prefix;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      }
      // lib.optionalAttrs (cfg.logS3Bucket != null) {
        RIO_LOG_S3_BUCKET = cfg.logS3Bucket;
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

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-scheduler";
        Restart = "on-failure";
        RestartSec = "5s";
      };
    };
  };
}
