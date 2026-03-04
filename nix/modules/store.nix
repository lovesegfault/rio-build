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
      description = "gRPC listen address (`RIO_LISTEN_ADDR`).";
    };

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      description = ''
        PostgreSQL connection URL (`RIO_DATABASE_URL`).
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

      # Env var naming: figment strips `RIO_` prefix then lowercases to
      # match the Config struct field name (e.g. RIO_LISTEN_ADDR ->
      # `listen_addr`). Each rio binary runs as its own process with its
      # own Config struct, so RIO_LISTEN_ADDR means "this binary's
      # listen_addr" — no cross-component collision.
      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_DATABASE_URL = cfg.databaseUrl;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      };

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-store";
        Restart = "on-failure";
        RestartSec = "5s";
        # `StateDirectory = "rio/store"` creates /var/lib/rio/store with proper
        # ownership. Reserved for the phase3a ChunkBackend wiring.
        StateDirectory = "rio/store";
      };
    };
  };
}
