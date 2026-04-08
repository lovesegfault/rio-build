{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.gateway;
in
{
  imports = [ ./common.nix ];

  options.services.rio.gateway = {
    enable = lib.mkEnableOption "rio-gateway SSH frontend speaking the Nix worker protocol";

    listenAddr = lib.mkOption {
      type = lib.types.str;
      default = "[::]:2222";
      description = "SSH listen address (`RIO_LISTEN_ADDR`). `[::]` binds dual-stack on Linux's default `bindv6only=0`.";
    };

    schedulerAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-scheduler gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        E.g., `"localhost:9001"`.
      '';
    };

    storeAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-store gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        E.g., `"localhost:9002"`.
      '';
    };

    hostKeyPath = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/rio/gateway/host_key";
      description = ''
        SSH host key path (`RIO_HOST_KEY`). Auto-generated (ed25519) on first
        start if the file does not exist.
      '';
    };

    authorizedKeysPath = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/rio/gateway/authorized_keys";
      description = ''
        Authorized keys file path (`RIO_AUTHORIZED_KEYS`). Must exist and
        contain at least one valid public key before the gateway starts.
      '';
    };

    metricsAddr = lib.mkOption {
      type = lib.types.str;
      default = "[::]:9090";
      description = "Prometheus metrics listen address (`RIO_METRICS_ADDR`).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.rio-gateway = {
      description = "rio-gateway SSH/Nix-protocol frontend";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network-online.target"
        # Gateway's gRPC connect() at startup is fatal — must wait for
        # store + scheduler to be listening.
        "rio-store.service"
        "rio-scheduler.service"
      ];
      wants = [ "network-online.target" ];

      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_SCHEDULER__ADDR = cfg.schedulerAddr;
        RIO_STORE__ADDR = cfg.storeAddr;
        RIO_HOST_KEY = cfg.hostKeyPath;
        RIO_AUTHORIZED_KEYS = cfg.authorizedKeysPath;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      };

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-gateway";
        Restart = "on-failure";
        RestartSec = "5s";
        # Persists the auto-generated host key across restarts.
        StateDirectory = "rio/gateway";
      };
    };
  };
}
