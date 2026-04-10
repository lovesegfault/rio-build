{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.gateway;
  rioLib = import ./_common.nix { inherit lib config; };
in
{
  imports = [ ./common.nix ];

  options.services.rio.gateway = {
    enable = lib.mkEnableOption "rio-gateway SSH frontend speaking the Nix worker protocol";

    listenAddr = rioLib.mkListenAddrOption 2222 "SSH listen address";
    schedulerAddr = rioLib.mkGrpcAddrOption "scheduler" ''E.g., `"localhost:9001"`.'';
    storeAddr = rioLib.mkGrpcAddrOption "store" ''E.g., `"localhost:9002"`.'';

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

    metricsAddr = rioLib.mkMetricsOption 9090;
  };

  config = lib.mkIf cfg.enable {
    systemd.services.rio-gateway = rioLib.mkRioService {
      binary = "rio-gateway";
      description = "rio-gateway SSH/Nix-protocol frontend";
      # Gateway's gRPC connect() at startup is fatal — must wait for
      # store + scheduler to be listening.
      extraAfter = [
        "rio-store.service"
        "rio-scheduler.service"
      ];
      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_SCHEDULER__ADDR = cfg.schedulerAddr;
        RIO_STORE__ADDR = cfg.storeAddr;
        RIO_HOST_KEY = cfg.hostKeyPath;
        RIO_AUTHORIZED_KEYS = cfg.authorizedKeysPath;
        RIO_METRICS_ADDR = cfg.metricsAddr;
      };
      serviceConfig = {
        # Persists the auto-generated host key across restarts.
        StateDirectory = "rio/gateway";
      };
    };
  };
}
