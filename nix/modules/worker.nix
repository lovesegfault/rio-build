{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.rio.worker;
in
{
  imports = [ ./common.nix ];

  options.services.rio.worker = {
    enable = lib.mkEnableOption "rio-worker build executor with FUSE store";

    workerId = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Worker ID (`RIO_WORKER_ID`). Defaults to hostname if unset.
        Two workers with the same ID will steal each other's builds via
        heartbeat merging — ensure uniqueness across the cluster.
      '';
    };

    schedulerAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-scheduler gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        Worker opens a bidirectional BuildExecution stream at startup.
      '';
    };

    storeAddr = lib.mkOption {
      type = lib.types.str;
      description = ''
        rio-store gRPC address as `host:port` (NOT a URL — main.rs prepends `http://`).
        FUSE fetches from here; executor uploads outputs via PutPath.
      '';
    };

    maxBuilds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1;
      description = "Maximum concurrent builds (`RIO_MAX_BUILDS`).";
    };

    fuseMountPoint = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/fuse-store";
      description = ''
        FUSE mount point for the lazy-fetch store view (`RIO_FUSE_MOUNT_POINT`).

        Do NOT use `/nix/store`: the per-build nix-daemon gets its own mount
        namespace with the overlay bind-mounted at `/nix/store` (executor
        `pre_exec`), so the FUSE mount location is arbitrary and should not
        shadow the host store.
      '';
    };

    fuseCacheDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/cache";
      description = "FUSE local cache directory (`RIO_FUSE_CACHE_DIR`).";
    };

    overlayBaseDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/overlays";
      description = ''
        Per-build overlayfs base directory (`RIO_OVERLAY_BASE_DIR`).
        Must be on a different filesystem than the FUSE mount (kernel rejects
        overlay where FUSE lower + upper share st_dev). In VM tests, the
        rootfs ext4 is fine; do NOT point this at the FUSE mount's subtree.
      '';
    };

    metricsAddr = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:9093";
      description = "Prometheus metrics listen address (`RIO_METRICS_ADDR`).";
    };
  };

  config = lib.mkIf cfg.enable {
    # Worker spawns `nix-daemon --stdio` per build; need nix binary + nixbld users.
    nix.enable = lib.mkDefault true;
    nix.settings.sandbox = lib.mkDefault true;

    # FUSE + overlayfs kernel support.
    boot.kernelModules = [
      "fuse"
      "overlay"
    ];
    # fuse/mod.rs uses SessionACL::All (allow_other); requires /etc/fuse.conf
    # `user_allow_other`. This option sets that flag.
    programs.fuse.userAllowOther = true;

    systemd.services.rio-worker = {
      description = "rio-worker build executor with FUSE store";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      # nix-daemon --stdio must be on PATH. fuse3 provides fusermount3,
      # required by the fuser crate's MountOption::AutoUnmount.
      path = [
        config.nix.package
        pkgs.fuse3
      ];

      # Env var naming: figment strips `RIO_` then lowercases to match
      # the Config field. `RIO_MAX_BUILDS` -> `max_builds`, etc.
      environment = {
        RIO_SCHEDULER_ADDR = cfg.schedulerAddr;
        RIO_STORE_ADDR = cfg.storeAddr;
        RIO_MAX_BUILDS = toString cfg.maxBuilds;
        RIO_FUSE_MOUNT_POINT = cfg.fuseMountPoint;
        RIO_FUSE_CACHE_DIR = cfg.fuseCacheDir;
        RIO_OVERLAY_BASE_DIR = cfg.overlayBaseDir;
        RIO_METRICS_ADDR = cfg.metricsAddr;
        RIO_LOG_FORMAT = config.services.rio.logFormat;
      }
      // lib.optionalAttrs (cfg.workerId != null) {
        RIO_WORKER_ID = cfg.workerId;
      };

      serviceConfig = {
        ExecStart = "${config.services.rio.package}/bin/rio-worker";
        # The worker runs as root (no User=), so CAP_SYS_ADMIN is already
        # available for FUSE mount, overlayfs, and CLONE_NEWNS in pre_exec.
        # We do NOT narrow CapabilityBoundingSet: the spawned `nix-daemon
        # --stdio` child inherits the bounding set, and its sandbox setup
        # needs CAP_SETUID/SETGID (nixbld users), CAP_CHOWN (output ownership),
        # CAP_SYS_CHROOT (sandbox chroot), CAP_MKNOD (/dev nodes), etc.
        # Allow opening /dev/fuse (device cgroup allowlist; DevicePolicy=auto
        # so pseudo-devices like /dev/null are always allowed).
        DeviceAllow = [ "/dev/fuse rw" ];
        # Worker connects to scheduler at startup; scheduler might not be
        # ready yet (remote host, no `After=` ordering across machines).
        # Retry on failure with backoff.
        Restart = "on-failure";
        RestartSec = "5s";
        # Unmount FUSE on shutdown (best-effort; the fuser background session
        # holds it, but the kernel detaches on process exit anyway).
        ExecStopPost = "-${pkgs.util-linux}/bin/umount -l ${cfg.fuseMountPoint}";
      };
    };

    # Ensure /var/rio/* directories exist (worker creates them too, but this
    # runs earlier and sets correct permissions). Also create the bind-mount
    # TARGETS that executor pre_exec mounts onto: `/nix/var/nix/db` is created
    # by nix-daemon on first local use, but the worker never runs local
    # nix-daemon (only the namespaced --stdio child), so that path may not exist.
    systemd.tmpfiles.rules = [
      "d /var/rio 0755 root root -"
      "d ${cfg.fuseCacheDir} 0755 root root -"
      "d ${cfg.overlayBaseDir} 0755 root root -"
      "d ${cfg.fuseMountPoint} 0755 root root -"
      # Bind-mount targets for executor pre_exec (spawn_daemon_in_namespace).
      # /nix/store and /etc/nix are created by NixOS activation, but
      # /nix/var/nix/db is lazy-created by the local nix-daemon. tmpfiles `d`
      # does NOT create parents, so list the full chain explicitly.
      "d /nix/var 0755 root root -"
      "d /nix/var/nix 0755 root root -"
      "d /nix/var/nix/db 0755 root root -"
    ];
  };
}
