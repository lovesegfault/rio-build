# Container images for each rio component, built via dockerTools.buildLayeredImage.
#
# Usage:
#   nix build .#docker-gateway      # single image tarball at result
#   nix build .#dockerImages        # all 6 at result/{gateway,scheduler,store,controller,worker,fod-proxy}.tar.zst
#   docker load < result/gateway.tar.zst
#   docker run rio-gateway:dev --help
#
# buildLayeredImage stratifies by popularity — the Nix store closure of each
# binary is split into layers by reference count, so shared deps (glibc,
# openssl, rustls) land in reusable layers. Pulling a second rio image only
# fetches the component-specific top layer.
#
# Worker is the outlier: it needs the `nix` binary (spawns `nix-daemon --stdio`)
# + `fuse3` + `util-linux` (mount/umount for overlay teardown) + passwd/group
# stubs (nix-daemon drops privs to nixbld). Gateway/scheduler/store are minimal.
{ pkgs, rio-workspace }:
let
  inherit (pkgs) lib dockerTools;

  # zstd tarballs instead of gzip: ~3x faster to compress, ~4x faster
  # to decompress, smaller output. Both skopeo (docker-archive:
  # transport, magic-byte detect) and k3s airgap import (wharfie
  # SupportedExtensions whitelist) handle .tar.zst natively.
  #
  # nixpkgs' compressor arg is a fixed lookup key (none/gz/zstd) —
  # no level knob. But `zstd` reads ZSTD_CLEVEL from env, and
  # buildLayeredImage is a runCommand whose attrs become builder
  # env. overrideAttrs threads it through.
  #
  # Level 6: ~2MB window. Do NOT crank this + --long: wharfie's
  # zstd decoder on k3s nodes caps at 32MB, exceeding that OOMs
  # the VM test airgap import.
  buildZstd =
    args:
    (dockerTools.buildLayeredImage (args // { compressor = "zstd"; })).overrideAttrs {
      ZSTD_CLEVEL = "6";
    };

  # Common to all images. cacert for TLS (S3, gRPC with mTLS if enabled),
  # tzdata so log timestamps aren't UTC-only.
  baseContents = [
    pkgs.cacert
    pkgs.tzdata
  ];

  mkImage =
    {
      name,
      extraContents ? [ ],
      extraEnv ? [ ],
      extraCommands ? "",
    }:
    buildZstd {
      name = "rio-${name}";
      inherit extraCommands;
      # "dev" not "latest": :latest defaults to imagePullPolicy=Always
      # in K8s (never checks local store), which breaks airgap k3s/kind.
      # Non-latest tag → IfNotPresent default → locally-imported image
      # works. Real release images are tagged by CI with git SHAs
      # anyway; this tag is for local dev + VM tests.
      tag = "dev";

      # Max layer count. Default is 100; Docker's hard limit is 127.
      # More layers = finer-grained caching but more tarball overhead.
      # 60 is a reasonable sweet spot for our closure sizes.
      maxLayers = 60;

      contents = baseContents ++ [ rio-workspace ] ++ extraContents;

      config = {
        Entrypoint = [ "${rio-workspace}/bin/rio-${name}" ];
        Env = [
          # JSON logs by default in containers — orchestrators (k8s,
          # systemd-in-container) expect structured output.
          "RIO_LOG_FORMAT=json"
          # cacert's bundle location. aws-sdk-s3 + rustls read this.
          "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
        ]
        ++ extraEnv;
        # OCI-standard labels for provenance.
        Labels = {
          "org.opencontainers.image.source" = "https://github.com/lovesegfault/rio-build";
          "org.opencontainers.image.description" = "rio-${name} — Nix build orchestration";
          "org.opencontainers.image.licenses" = "MIT OR Apache-2.0";
        };
      };
    };
in
{
  gateway = mkImage { name = "gateway"; };
  # Scheduler also carries rio-cli (admin client) — buildLayeredImage's
  # `contents` symlinks rio-workspace/bin/* into /bin/, so `kubectl exec
  # deploy/rio-scheduler -- rio-cli create-tenant foo` resolves via the
  # default /bin in PATH. The pod's RIO_TLS__* env (from tls-mounts.yaml)
  # gives rio-cli mTLS to localhost:9001 for free.
  scheduler = mkImage { name = "scheduler"; };
  store = mkImage { name = "store"; };

  # Controller is the lightest — it only talks to the K8s API and
  # the scheduler's gRPC. No nix, no fuse, no PG. Just cacert for
  # the in-cluster TLS connection (kube-apiserver serves HTTPS;
  # the service-account CA is mounted separately but kube-rs also
  # reads SSL_CERT_FILE for the initial client config probe).
  controller = mkImage { name = "controller"; };

  # FOD forward proxy. Not a rio binary — just squid with cacert.
  # Built here (not pulled from Docker Hub) so it goes to our ECR
  # with the rest: same git-SHA immutable tag, can't disappear
  # from under us like ubuntu/squid:5.7-22.04_beta did. Config
  # stays in the ConfigMap (deploy/base/fod-proxy.yaml) so
  # operators can edit the allowlist without rebuilding.
  #
  # Can't use mkImage — that's built around rio-workspace binaries.
  fod-proxy = buildZstd {
    name = "rio-fod-proxy";
    tag = "dev";
    maxLayers = 20;
    contents = baseContents ++ [ pkgs.squid ];
    config = {
      # -N: foreground (no daemonize — container PID 1 must block).
      # -d 1: log to stderr at debug level 1 (kubectl logs sees it).
      Entrypoint = [
        "${pkgs.squid}/bin/squid"
        "-N"
        "-d"
        "1"
        "-f"
        "/etc/squid/squid.conf"
      ];
      Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
      ExposedPorts."3128/tcp" = { };
    };
    # Squid writes connection state even with `cache deny all`.
    # The manifest mounts tmpfs over /var/spool/squid; /var/log
    # needs to exist for stderr symlink. /etc/squid: config lands
    # here via ConfigMap subPath mount — dir must pre-exist.
    extraCommands = ''
      mkdir -p var/spool/squid var/log/squid etc/squid
    '';
  };

  # Worker needs the nix toolchain + FUSE runtime + mount utilities.
  worker = mkImage {
    name = "worker";
    extraContents = [
      pkgs.nix # nix-daemon --stdio, spawned per-build
      pkgs.fuse3 # fusermount3, required by the fuser crate's AutoUnmount
      pkgs.util-linux # mount, umount for overlay teardown

      # nix-daemon drops privs to a nixbld{N} user inside its sandbox.
      # It enumerates build users via getgrnam("nixbld")->gr_mem — the
      # EXPLICIT member list in /etc/group. A user's primary group (gid
      # field in passwd) does NOT appear in gr_mem; the member list must
      # be populated or daemon fails: "nixbld group has no members".
      #
      # 8 users: one per concurrent build slot. maxConcurrentBuilds
      # can go up to this without running out. UIDs 30001-30008 match
      # the NixOS convention (nixbld1 at 30001, etc).
      (pkgs.writeTextDir "etc/passwd" (
        ''
          root:x:0:0:root:/root:/bin/sh
        ''
        + lib.concatMapStrings (
          n: "nixbld${toString n}:x:${toString (30000 + n)}:30000:Nix build user ${toString n}:/:/bin/false\n"
        ) (lib.range 1 8)
      ))
      (pkgs.writeTextDir "etc/group" ''
        root:x:0:
        nixbld:x:30000:${lib.concatMapStringsSep "," (n: "nixbld${toString n}") (lib.range 1 8)}
      '')
      # nix-daemon also reads /etc/nix/nix.conf. Give it a minimal one
      # with the settings the executor's per-build nix.conf overrides
      # anyway (via bind-mount), but a baseline prevents "no such file"
      # on the host-daemon startup probe.
      (pkgs.writeTextDir "etc/nix/nix.conf" ''
        build-users-group = nixbld
        sandbox = true
        experimental-features = nix-command
      '')
    ];
    extraEnv = [
      # nix-daemon --stdio must be findable. The worker's executor does
      # `Command::new("nix-daemon")` (no absolute path), relying on PATH.
      "PATH=${
        lib.makeBinPath [
          pkgs.nix
          pkgs.fuse3
          pkgs.util-linux
        ]
      }"
    ];
    # spawn_daemon_in_namespace bind-mounts the per-build synthetic DB
    # at /nix/var/nix/db and the nix.conf at /etc/nix. Bind mount
    # targets must exist. /etc/nix exists (writeTextDir above creates
    # it); /nix/var doesn't — the closure only populates /nix/store.
    # The NixOS VM worker module creates /nix/var/nix/db via tmpfiles;
    # we do it here for the container case.
    #
    # /tmp: nix-daemon's sandbox needs a tmpdir. Containers don't have
    # one by default. sticky-bit (1777) matches the standard /tmp.
    #
    # extraCommands runs in the customisation layer's root dir (unprivileged;
    # nix's sandbox builder user) — paths are relative to image /.
    extraCommands = ''
      mkdir -p nix/var/nix/db
      mkdir -p tmp
      chmod 1777 tmp
    '';
  };
}
