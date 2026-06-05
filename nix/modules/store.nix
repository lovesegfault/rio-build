{
  config,
  lib,
  ...
}:
let
  cfg = config.services.rio.store;
  rioLib = import ./_common.nix { inherit lib config; };
in
{
  imports = [ ./common.nix ];

  options.services.rio.store = {
    enable = lib.mkEnableOption "rio-store NAR content-addressable store";

    listenAddr = rioLib.mkListenAddrOption 9002 "gRPC listen address";

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      description = ''
        PostgreSQL connection URL (`RIO_DATABASE_URL`). Also used by
        the `rio-migrate` oneshot (below) — rio-store itself does not
        migrate on startup, it only verifies the schema is current.
      '';
    };

    metricsAddr = rioLib.mkMetricsOption 9092;

    signingKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Path to the ed25519 narinfo signing key (Nix secret-key format:
        `name:base64-seed`). Null = signing disabled. Generate with
        `nix-store --generate-binary-cache-key`. File should be mode
        0600. This is a PATH (read at runtime), not inlined content —
        keeps the secret out of the Nix store.
      '';
    };

    extraConfig = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = ''
        Extra TOML appended to `/etc/rio/store.toml`. The config loader
        reads this with lower precedence than env vars. Useful for nested
        config — though the `[chunk_backend]` tagged enum also works
        via env vars (`RIO_CHUNK_BACKEND__KIND=s3` +
        `RIO_CHUNK_BACKEND__BUCKET=...`; the k8s overlays use that).
        TOML is just more readable for multi-field sections. Example:

            extraConfig = ${"''"}
              [chunk_backend]
              kind = "filesystem"
              base_dir = "/var/lib/rio/store/chunks"
            ${"''"};

        S3 example (credentials from aws-sdk default chain — env vars
        or instance profile, NOT in this TOML):

            extraConfig = ${"''"}
              [chunk_backend]
              kind = "s3"
              bucket = "my-nar-chunks"
              prefix = "prod/"
            ${"''"};
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # /etc/rio/store.toml < RIO_* env < CLI. Env vars above override.
    environment.etc."rio/store.toml" = lib.mkIf (cfg.extraConfig != "") {
      text = cfg.extraConfig;
    };

    # Database migrations run here, NOT in the services: the systemd
    # mirror of the k8s rio-migrate Job. rio-store and
    # rio-scheduler only `assert_current` at startup and fail (with
    # Restart=on-failure churning them) until this completes. Lives in
    # the store module because the `rio-store migrate` subcommand ships
    # in the store binary; scheduler.nix orders After= it (no-op when
    # the store module is disabled — every current fixture co-locates
    # both on the control node).
    systemd.services.rio-migrate = {
      description = "rio database migrations";
      wantedBy = [ "multi-user.target" ];
      # NixOS postgresql.service waits for pg_isready in postStart, so
      # ordering After it means the one-shot connect attempt succeeds.
      after = [
        "network-online.target"
        "postgresql.service"
      ];
      wants = [ "network-online.target" ];
      environment = {
        RIO_DATABASE_URL = cfg.databaseUrl;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      };
      serviceConfig = {
        Type = "oneshot";
        # Stay "active" after exit — `systemctl status rio-migrate`
        # reads as done, not dead, for the rest of the boot.
        RemainAfterExit = true;
        ExecStart = "${config.services.rio.package}/bin/rio-store migrate";
        # Valid with Type=oneshot: systemd refuses only Restart=always/
        # on-success there (service_verify). RemainAfterExit latches
        # SUCCESS only — a failed ExecStart still lands the unit in
        # "failed", so on-failure re-runs it (covers a PG that restarts
        # mid-migration).
        Restart = "on-failure";
        RestartSec = "5s";
      };
    };

    systemd.services.rio-store = rioLib.mkRioService {
      binary = "rio-store";
      description = "rio-store NAR content-addressable store";
      extraAfter = [
        "postgresql.service"
        "rio-migrate.service"
      ];
      # Env var naming: the config loader strips the `RIO_` prefix then
      # lowercases to match the Config struct field name (e.g.
      # RIO_LISTEN_ADDR -> `listen_addr`). Each rio binary runs as its own process with its
      # own Config struct, so RIO_LISTEN_ADDR means "this binary's
      # listen_addr" — no cross-component collision.
      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_DATABASE_URL = cfg.databaseUrl;
        RIO_METRICS_ADDR = cfg.metricsAddr;
      }
      // lib.optionalAttrs (cfg.signingKeyFile != null) {
        # toString: the option type is path but the config loader parses
        # RIO_SIGNING_KEY_PATH as a string (which Rust turns into
        # PathBuf). If we passed the path unquoted, Nix would copy
        # it to the store — NOT what we want for a secret. toString
        # keeps it as the literal runtime path.
        RIO_SIGNING_KEY_PATH = toString cfg.signingKeyFile;
      };
      serviceConfig = {
        # StateDirectory creates /var/lib/rio/store with proper
        # ownership. Filesystem chunk backend base_dir should point
        # under here (or a separate mount). The chunks/ subdir is
        # created by FilesystemChunkBackend::new at startup.
        StateDirectory = "rio/store";
      };
    };
  };
}
