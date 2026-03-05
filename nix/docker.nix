# Container images for each rio component, built via dockerTools.buildLayeredImage.
#
# Usage:
#   nix build .#docker-gateway      # single image tarball at result
#   nix build .#dockerImages        # all 4 at result/{gateway,scheduler,store,worker}.tar.gz
#   docker load < result/gateway.tar.gz
#   docker run rio-gateway:latest --help
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
    }:
    dockerTools.buildLayeredImage {
      name = "rio-${name}";
      tag = "latest";

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
  scheduler = mkImage { name = "scheduler"; };
  store = mkImage { name = "store"; };

  # Controller is the lightest — it only talks to the K8s API and
  # the scheduler's gRPC. No nix, no fuse, no PG. Just cacert for
  # the in-cluster TLS connection (kube-apiserver serves HTTPS;
  # the service-account CA is mounted separately but kube-rs also
  # reads SSL_CERT_FILE for the initial client config probe).
  controller = mkImage { name = "controller"; };

  # Worker needs the nix toolchain + FUSE runtime + mount utilities.
  worker = mkImage {
    name = "worker";
    extraContents = [
      pkgs.nix # nix-daemon --stdio, spawned per-build
      pkgs.fuse3 # fusermount3, required by the fuser crate's AutoUnmount
      pkgs.util-linux # mount, umount for overlay teardown

      # nix-daemon drops privs to the nixbld user inside its sandbox.
      # Without /etc/passwd, it can't look up the uid and fails with
      # "getpwnam: user nixbld does not exist". writeTextDir gives us
      # a minimal passwd/group file in the image layer.
      (pkgs.writeTextDir "etc/passwd" ''
        root:x:0:0:root:/root:/bin/sh
        nixbld:x:30000:30000:Nix build user:/:/bin/false
      '')
      (pkgs.writeTextDir "etc/group" ''
        root:x:0:
        nixbld:x:30000:
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
  };
}
