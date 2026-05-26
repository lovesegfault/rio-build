{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.rio.worker;
  rioLib = import ./_common.nix { inherit lib config; };
in
{
  imports = [ ./common.nix ];

  options.services.rio.worker = {
    enable = lib.mkEnableOption "rio-builder build executor with castore-FUSE store";

    workerId = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Worker ID (`RIO_EXECUTOR_ID`). Defaults to hostname if unset.
        Two workers with the same ID will steal each other's builds via
        heartbeat merging — ensure uniqueness across the cluster.
      '';
    };

    schedulerAddr = rioLib.mkGrpcAddrOption "scheduler" "Worker opens a bidirectional BuildExecution stream at startup.";
    storeAddr = rioLib.mkGrpcAddrOption "store" "Castore-FUSE fetches blobs/directories from here; executor uploads outputs via PutPathChunked.";

    overlayBaseDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/rio/overlays";
      description = ''
        Per-build overlayfs base directory (`RIO_OVERLAY_BASE_DIR`). The
        per-build castore-FUSE mountpoints (`<dir>/<build_id>.castore`)
        live here too. Must be on a different filesystem than the FUSE
        lower (the kernel rejects an overlay whose upper shares st_dev
        with a FUSE lower) — the rootfs is fine.
      '';
    };

    stagingQuotaBytes = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 0;
      example = 10737418240;
      description = ''
        Kernel project-quota hard limit rio-mountd applies to each
        build's `/var/rio/staging/<build_id>` (`--staging-quota-bytes`).
        Requires `/var/rio/staging` to be on XFS mounted with
        `prjquota`; the default 0 disables enforcement, which is the
        only workable default for a generic host whose /var/rio sits on
        the root filesystem. Production EKS nodes run the rio-mountd
        DaemonSet against the eks-node.nix XFS loopback instead of this
        module.
      '';
    };

    metricsAddr = rioLib.mkMetricsOption 9093;
  };

  config = lib.mkIf cfg.enable {
    # Worker spawns `nix-daemon --stdio` per build; need nix binary + nixbld users.
    nix.enable = lib.mkDefault true;
    nix.settings.sandbox = lib.mkDefault true;

    # FUSE + overlayfs kernel support (castore-FUSE lower + per-build
    # overlay upper). FUSE passthrough needs kernel >= 6.9 — the AMI
    # pins this via nixos-node/kernel.nix; NixOS' default kernel in VM
    # tests is already newer.
    boot.kernelModules = [
      "fuse"
      "overlay"
    ];

    # Host-side group owning /run/rio-mountd/mountd.sock (created 0660
    # root:rio-builder by rio-mountd). 990 matches
    # nix/nixos-node/eks-node.nix and helm `mountd.allowedGid` so the
    # standalone module exercises the same SO_PEERCRED gate shape as
    # production.
    users.groups.rio-builder.gid = 990;

    systemd = {
      services = {
        # rio-mountd: the per-node castore broker. The builder connects to
        # its UDS at every build's Mount{} (fd handoff + staging + the
        # BackingOpen/Promote brokering), so a worker host without it fails
        # every build at castore-mount time.
        rio-mountd = rioLib.mkRioService {
          binary = "rio-mountd";
          description = "rio-mountd castore broker (fd handoff, BackingOpen, verified cache promotion)";
          environment = { };
          serviceConfig = {
            # `//`-merged over mkRioService's bare ExecStart (plain attrset
            # merge, caller wins) so the daemon gets its flags.
            ExecStart = lib.concatStringsSep " " [
              "${config.services.rio.package}/bin/rio-mountd"
              "--socket /run/rio-mountd/mountd.sock"
              "--staging-dir /var/rio/staging"
              "--cache-dir /var/rio/cache"
              "--chunks-dir /var/rio/chunks"
              "--staging-quota-bytes ${toString cfg.stagingQuotaBytes}"
              "--allowed-gid ${toString config.users.groups.rio-builder.gid}"
              "--metrics-addr [::]:9095"
            ];
            # The daemon owns node-local state only; restart fast and
            # unconditionally (the startup orphan scan reaps leftovers).
            Restart = "always";
            RestartSec = "1s";
          };
        };

        rio-builder =
          rioLib.mkRioService {
            binary = "rio-builder";
            description = "rio-builder build executor with castore-FUSE store";
            # Env var naming: the config loader strips `RIO_` then lowercases to
            # match the Config field; `__` nests. `RIO_STORE__ADDR` -> `store.addr`.
            # mountd socket + castore dirs use the builder-config defaults
            # (/run/rio-mountd/mountd.sock, /var/rio/{cache,chunks,staging}),
            # which match the rio-mountd flags above.
            environment = {
              RIO_SCHEDULER__ADDR = cfg.schedulerAddr;
              RIO_STORE__ADDR = cfg.storeAddr;
              RIO_OVERLAY_BASE_DIR = cfg.overlayBaseDir;
              RIO_METRICS_ADDR = cfg.metricsAddr;
            }
            // lib.optionalAttrs (cfg.workerId != null) {
              RIO_EXECUTOR_ID = cfg.workerId;
            };
            # The builder only needs mountd at build time, but ordering on
            # it removes a connect-refused → build-failed race right after
            # boot.
            extraAfter = [ "rio-mountd.service" ];
            serviceConfig = {
              # The worker runs as root (no User=), so CAP_SYS_ADMIN is already
              # available for the castore-FUSE mount, overlayfs, and CLONE_NEWNS
              # in pre_exec. We do NOT narrow CapabilityBoundingSet: the spawned
              # `nix-daemon --stdio` child inherits the bounding set, and its
              # sandbox setup needs CAP_SETUID/SETGID (nixbld users), CAP_CHOWN
              # (output ownership), CAP_SYS_CHROOT (sandbox chroot), CAP_MKNOD
              # (/dev nodes), etc.
              #
              # Group=rio-builder sets the egid SO_PEERCRED reports, which is
              # what rio-mountd's --allowed-gid gate checks (root bypasses the
              # socket-file DAC, not the peer-credential check).
              Group = "rio-builder";
              # Allow opening /dev/fuse (device cgroup allowlist; DevicePolicy=auto
              # so pseudo-devices like /dev/null are always allowed). The builder
              # opens the device itself and hands rio-mountd a dup in Mount{}.
              DeviceAllow = [ "/dev/fuse rw" ];

              # cgroup v2 per-build resource tracking. The worker creates a
              # sub-cgroup per build and moves the spawned nix-daemon into
              # it. memory.peak + polled cpu.stat give tree-wide peak memory
              # and CPU. Per-PID VmHWM would only capture nix-daemon's own
              # RSS (~10MB) — the builder is a fork()ed child whose footprint
              # never appears there.
              #
              # Delegate=yes: grants the service ownership of its cgroup
              # subtree. Without this, cgroup.subtree_control writes fail
              # EACCES and the worker DIES AT STARTUP (cgroup v2 is a hard
              # requirement — no broken-metrics fallback).
              #
              # DelegateSubgroup=builds: systemd v254+. cgroup v2 forbids a
              # cgroup having BOTH processes AND sub-cgroups with enabled
              # controllers (the "no internal processes" rule). This makes
              # systemd run the worker in a `builds/` SUB-cgroup of the
              # service cgroup, leaving the service cgroup EMPTY.
              #
              # The worker's delegated_root() reads /proc/self/cgroup (which
              # points to `.../builds/`) and returns the PARENT
              # (`.../rio-builder.service/`). Per-build cgroups are created
              # there as SIBLINGS of `builds/` — the service cgroup is
              # empty, so enabling +memory +cpu on it succeeds; `builds/`
              # has the worker process but no controller-enabled children
              # (per-build cgroups are not under it). No rule violation.
              #
              # /sys/fs/cgroup/system.slice/rio-builder.service/:
              #   cgroup.subtree_control  ← worker writes "+memory +cpu" (EMPTY cgroup: no EBUSY)
              #   builds/                 ← DelegateSubgroup; worker PID lives here
              #   <drv-hash>/             ← per-build SIBLING (nix-daemon PID → forks builder)
              #     memory.peak           ← tree-wide peak, read at build end
              #     cpu.stat              ← tree-wide cumulative, polled 1Hz
              Delegate = "yes";
              DelegateSubgroup = "builds";
              # The builder is one-shot: exits cleanly after completing a
              # build. systemd respawns it for the next assignment — same
              # role the k8s controller plays with Jobs. Also covers startup
              # races (scheduler not ready → connect refused → exit).
              Restart = "always";
              RestartSec = "1s";
            };
          }
          // {
            # The builder is one-shot and exits cleanly per build; systemd
            # respawns it (Restart=always below). Default StartLimitBurst=5
            # in 10s trips under fanout (50 builds → ~13 rapid restarts per
            # worker). Must be at the unit level — `StartLimitIntervalSec`
            # in [Service] is silently ignored since systemd 230.
            startLimitIntervalSec = 0;

            # nix-daemon --stdio must be on PATH. fuse3 provides fusermount3
            # for the fusectl-abort fallback path.
            path = [
              config.nix.package
              pkgs.fuse3
            ];
          };
      };

      # Ensure /var/rio/* directories exist before either service starts
      # (rio-mountd open()s the three cache/staging trees O_DIRECTORY at
      # startup; the builder creates per-build subdirs of overlayBaseDir
      # itself). Also create the bind-mount TARGETS that executor pre_exec
      # mounts onto: `/nix/var/nix/db` is created by nix-daemon on first
      # local use, but the worker never runs local nix-daemon (only the
      # namespaced --stdio child), so that path may not exist.
      tmpfiles.rules = [
        "d /var/rio 0755 root root -"
        "d /var/rio/cache 0755 root root -"
        "d /var/rio/chunks 0755 root root -"
        "d /var/rio/staging 0755 root root -"
        "d ${cfg.overlayBaseDir} 0755 root root -"
        # Bind-mount targets for executor pre_exec (spawn_daemon_in_namespace).
        # /nix/store and /etc/nix are created by NixOS activation, but
        # /nix/var/nix/db is lazy-created by the local nix-daemon. tmpfiles `d`
        # does NOT create parents, so list the full chain explicitly.
        "d /nix/var 0755 root root -"
        "d /nix/var/nix 0755 root root -"
        "d /nix/var/nix/db 0755 root root -"
      ];
    };
  };
}
