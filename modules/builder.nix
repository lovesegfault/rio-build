# NixOS module for Rio Builder service

{
  config,
  lib,
  pkgs,
  ...
}:

with lib;

let
  cfg = config.services.rio-builder;
in
{
  options.services.rio-builder = {
    enable = mkEnableOption "Rio Builder - distributed build worker node";

    package = mkOption {
      type = types.package;
      description = "The rio-builder package to use";
    };

    dispatcherEndpoint = mkOption {
      type = types.str;
      example = "http://dispatcher.example.com:50051";
      description = "Dispatcher gRPC endpoint to connect to";
    };

    grpcAddress = mkOption {
      type = types.str;
      default = "0.0.0.0:50052";
      description = "gRPC server address for receiving build requests from dispatcher";
    };

    platforms = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      description = "Platforms this builder supports (auto-detected if empty)";
    };

    features = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "kvm"
        "benchmark"
      ];
      description = "Features this builder supports (e.g., kvm, benchmark)";
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
      default = "rio-builder";
      description = "User account under which builder runs";
    };

    group = mkOption {
      type = types.str;
      default = "rio-builder";
      description = "Group under which builder runs";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = true;
      description = "Whether to open firewall port for gRPC server";
    };
  };

  config = mkIf cfg.enable {
    # Builder needs Nix to execute builds
    nix.settings = {
      experimental-features = [
        "nix-command"
        "flakes"
      ];
      # Disable sandbox for simplicity in VM tests
      # In production, keep sandbox enabled
      sandbox = mkDefault false;
    };

    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      description = "Rio Builder service user";
      home = "/var/lib/rio-builder";
      createHome = true;
      # Builder needs to interact with Nix
      extraGroups = [ "nixbld" ];
    };

    users.groups.${cfg.group} = { };

    systemd.services.rio-builder = {
      description = "Rio Builder - Distributed Build Worker";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network-online.target"
        "nix-daemon.service"
      ];
      wants = [ "network-online.target" ];
      requires = [ "nix-daemon.service" ];

      environment = {
        RIO_DISPATCHER_ENDPOINT = cfg.dispatcherEndpoint;
        RIO_GRPC_ADDR = cfg.grpcAddress;
        RIO_LOG_LEVEL = cfg.logLevel;
      }
      // optionalAttrs (cfg.platforms != [ ]) {
        RIO_PLATFORMS = concatStringsSep "," cfg.platforms;
      }
      // optionalAttrs (cfg.features != [ ]) {
        RIO_FEATURES = concatStringsSep "," cfg.features;
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/rio-builder";
        Restart = "always";
        RestartSec = "10s";

        User = cfg.user;
        Group = cfg.group;

        # State directory
        StateDirectory = "rio-builder";
        WorkingDirectory = "/var/lib/rio-builder";

        # Security hardening
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [
          "/var/lib/rio-builder"
          "/nix/store"
          "/nix/var" # Needed for nix-store operations (database writes)
          "/tmp"
        ];

        # Logging
        StandardOutput = "journal";
        StandardError = "journal";
      };
    };

    # Open firewall for gRPC server
    networking.firewall = mkIf cfg.openFirewall {
      allowedTCPPorts = [
        (lib.toInt (lib.last (lib.splitString ":" cfg.grpcAddress)))
      ];
    };
  };
}
