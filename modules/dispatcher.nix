# NixOS module for Rio Dispatcher service

{
  config,
  lib,
  pkgs,
  ...
}:

with lib;

let
  cfg = config.services.rio-dispatcher;
in
{
  options.services.rio-dispatcher = {
    enable = mkEnableOption "Rio Dispatcher - distributed build fleet manager";

    package = mkOption {
      type = types.package;
      description = "The rio-dispatcher package to use";
    };

    grpcAddress = mkOption {
      type = types.str;
      default = "0.0.0.0:50051";
      description = "gRPC server address for builder communication";
      example = "127.0.0.1:50051";
    };

    sshAddress = mkOption {
      type = types.str;
      default = "0.0.0.0:2222";
      description = "SSH server address for Nix client connections";
      example = "0.0.0.0:2222";
    };

    sshHostKeyPath = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Path to SSH host key (auto-generated if not specified)";
    };

    logLevel = mkOption {
      type = types.enum [
        "trace"
        "debug"
        "info"
        "warn"
        "error"
      ];
      default = "info";
      description = "Logging level";
    };

    user = mkOption {
      type = types.str;
      default = "rio-dispatcher";
      description = "User account under which dispatcher runs";
    };

    group = mkOption {
      type = types.str;
      default = "rio-dispatcher";
      description = "Group under which dispatcher runs";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = true;
      description = "Whether to open firewall ports for gRPC and SSH";
    };
  };

  config = mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      description = "Rio Dispatcher service user";
      home = "/var/lib/rio-dispatcher";
      createHome = true;
    };

    users.groups.${cfg.group} = { };

    systemd.services.rio-dispatcher = {
      description = "Rio Dispatcher - Distributed Build Fleet Manager";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        RIO_GRPC_ADDR = cfg.grpcAddress;
        RIO_SSH_ADDR = cfg.sshAddress;
        RIO_LOG_LEVEL = cfg.logLevel;
      }
      // optionalAttrs (cfg.sshHostKeyPath != null) {
        RIO_SSH_HOST_KEY = toString cfg.sshHostKeyPath;
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/rio-dispatcher";
        Restart = "always";
        RestartSec = "10s";

        User = cfg.user;
        Group = cfg.group;

        # State directory
        StateDirectory = "rio-dispatcher";
        WorkingDirectory = "/var/lib/rio-dispatcher";

        # Security hardening
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [
          "/var/lib/rio-dispatcher"
          "/nix/store"
        ];

        # Logging
        StandardOutput = "journal";
        StandardError = "journal";
      };
    };

    # Open firewall ports
    networking.firewall = mkIf cfg.openFirewall {
      allowedTCPPorts =
        let
          grpcPort = lib.toInt (lib.last (lib.splitString ":" cfg.grpcAddress));
          sshPort = lib.toInt (lib.last (lib.splitString ":" cfg.sshAddress));
        in
        [
          grpcPort
          sshPort
        ];
    };
  };
}
