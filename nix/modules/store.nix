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

    listenAddr = lib.mkOption {
      type = lib.types.str;
      default = "[::]:9002";
      description = "gRPC listen address (`RIO_LISTEN_ADDR`). `[::]` binds dual-stack on Linux's default `bindv6only=0`.";
    };

    databaseUrl = lib.mkOption {
      type = lib.types.str;
      description = ''
        PostgreSQL connection URL (`RIO_DATABASE_URL`).
        rio-store applies migrations (sqlx migrate) on startup.
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
        Extra TOML appended to `/etc/rio/store.toml`. figment reads
        this with lower precedence than env vars. Useful for nested
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
    systemd.services.rio-store = rioLib.mkRioService {
      binary = "rio-store";
      description = "rio-store NAR content-addressable store";
      extraAfter = [ "postgresql.service" ];
      # Env var naming: figment strips `RIO_` prefix then lowercases to
      # match the Config struct field name (e.g. RIO_LISTEN_ADDR ->
      # `listen_addr`). Each rio binary runs as its own process with its
      # own Config struct, so RIO_LISTEN_ADDR means "this binary's
      # listen_addr" — no cross-component collision.
      environment = {
        RIO_LISTEN_ADDR = cfg.listenAddr;
        RIO_DATABASE_URL = cfg.databaseUrl;
        RIO_METRICS_ADDR = cfg.metricsAddr;
      }
      // lib.optionalAttrs (cfg.signingKeyFile != null) {
        # toString: the option type is path but figment parses
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
