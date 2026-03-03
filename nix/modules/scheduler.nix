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
  };

  config = lib.mkIf cfg.enable {
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
      };

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-scheduler";
        Restart = "on-failure";
        RestartSec = "5s";
      };
    };
  };
}
