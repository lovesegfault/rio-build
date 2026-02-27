{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.store;
in
{
  imports = [ ./common.nix ];

  options.services.rio.store = {
    enable = lib.mkEnableOption "rio-store NAR content-addressable store";

    listenAddr = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:9002";
      description = "gRPC listen address (`RIO_STORE_LISTEN_ADDR`).";
    };

    backend = lib.mkOption {
      type = lib.types.enum [
        "filesystem"
        "s3"
      ];
      default = "filesystem";
      description = "Storage backend (`RIO_STORE_BACKEND`).";
    };

    baseDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/rio/store";
      description = "Base directory for filesystem backend (`RIO_STORE_BASE_DIR`).";
    };

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      description = ''
        PostgreSQL connection URL (`DATABASE_URL`).
        rio-store applies migrations (sqlx migrate) on startup.
      '';
    };

    metricsAddr = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:9092";
      description = "Prometheus metrics listen address (`RIO_METRICS_ADDR`).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.rio-store = {
      description = "rio-store NAR content-addressable store";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network-online.target"
        "postgresql.service"
      ];
      wants = [ "network-online.target" ];

      environment = {
        RIO_STORE_LISTEN_ADDR = cfg.listenAddr;
        RIO_STORE_BACKEND = cfg.backend;
        RIO_STORE_BASE_DIR = cfg.baseDir;
        DATABASE_URL = cfg.databaseUrl;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      };

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-store";
        Restart = "on-failure";
        RestartSec = "5s";
        # `StateDirectory = "rio/store"` creates /var/lib/rio/store with proper
        # ownership (matches `baseDir` default; filesystem backend writes NARs here).
        StateDirectory = "rio/store";
      };
    };
  };
}
