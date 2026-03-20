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
{
  pkgs,
  rio-workspace,
  # Svelte SPA dist/ output (nix/dashboard.nix). Nullable: the coverage-
  # mode mkDockerImages call site doesn't thread it through (dashboard
  # is nginx+static — no rio binary, no LLVM instrumentation). The
  # `dashboard` attr below is conditionally emitted so an un-passed
  # rioDashboard doesn't break eval.
  rioDashboard ? null,
  coverage ? false,
}:
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
  # Normal builds: level 6 (~2MB window, fast). Coverage builds:
  # level 19 (8MB window) — instrumented binaries are ~3-4x larger,
  # so the airgap import budget (~15min on nixbuild.net, serial
  # alphabetical import before kubelet starts) runs out before the
  # testScript begins. -19 is a one-time build cost; decompression
  # speed is nearly level-independent. Do NOT use --ultra / --long:
  # wharfie's zstd decoder on k3s nodes caps at a 32MB window,
  # exceeding that OOMs the VM test airgap import. Level 19's 8MB
  # window is safe; --ultra -22 (128MB) is not.
  zstdLevel = if coverage then "19" else "6";
  buildZstd =
    args:
    (dockerTools.buildLayeredImage (args // { compressor = "zstd"; })).overrideAttrs {
      ZSTD_CLEVEL = zstdLevel;
    };

  # Common to all images. cacert for TLS (S3, gRPC with mTLS if enabled),
  # tzdata so log timestamps aren't UTC-only.
  baseContents = [
    pkgs.cacert
    pkgs.tzdata
  ];

  baseEnv = [
    # JSON logs by default in containers — orchestrators (k8s,
    # systemd-in-container) expect structured output.
    "RIO_LOG_FORMAT=json"
    # cacert's bundle location. aws-sdk-s3 + rustls read this.
    "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
  ];

  # OCI-standard labels for provenance.
  mkLabels = desc: {
    "org.opencontainers.image.source" = "https://github.com/lovesegfault/rio-build";
    "org.opencontainers.image.description" = desc;
    "org.opencontainers.image.licenses" = "MIT OR Apache-2.0";
  };

  # ── Worker runtime extras ────────────────────────────────────────────
  # Factored out so the `all` aggregate image (VM-test-only) can reuse
  # them. Worker is the only component that needs nix/fuse/mount at
  # runtime, but the aggregate must be a superset of every component.
  workerExtraContents = [
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

  workerExtraEnv = [
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
  workerExtraCommands = ''
    mkdir -p nix/var/nix/db
    mkdir -p tmp
    chmod 1777 tmp
  '';

  # ── Dashboard nginx config ───────────────────────────────────────────
  # Proxy target is the Envoy Gateway operator-managed Service — NOT a
  # localhost sidecar (P0273 landed Gateway API CRDs; the operator
  # spins up envoy data-plane pods from the rio-dashboard Gateway, we
  # don't hand-configure envoy here).
  #
  # The Service lives in envoy-gateway-system (ControllerNamespaceMode,
  # the operator's default — see nix/tests/scenarios/dashboard-
  # gateway.nix:31-37 comment). The Gateway/GRPCRoute/EnvoyProxy CRs
  # live in rio-system but the Deployment+Service they produce land in
  # the operator's namespace. Stable name `rio-dashboard-envoy` is
  # pinned by EnvoyProxy.spec.provider.kubernetes.envoyService.name
  # (values.yaml dashboard.envoyServiceName — otherwise the operator
  # generates envoy-{ns}-{gw}-{hash} which is awkward to hardcode).
  #
  # If a deploy changes envoyServiceName or runs the operator in
  # non-default-namespace mode, this config needs a matching rebuild.
  # Baked-in beats runtime envsubst — a broken upstream is a build-
  # time failure not a runtime surprise.
  dashboardNginxConf = pkgs.writeText "nginx.conf" ''
    # Non-root container (runAsUser, readOnlyRootFilesystem). Master
    # process runs foreground; no `user` directive (we're already
    # unprivileged — nginx warns on `user` when not root and ignores
    # it). pid + temp paths go to /tmp (emptyDir mount in the
    # Deployment).
    daemon off;
    pid /tmp/nginx.pid;
    error_log /dev/stderr info;
    events { worker_connections 1024; }
    http {
      include ${pkgs.nginx}/conf/mime.types;
      access_log /dev/stdout;
      # All writable paths in /tmp. readOnlyRootFilesystem blocks the
      # compiled-in defaults (/var/cache/nginx/* on most distros,
      # $prefix/client_body_temp/* on nixpkgs). One emptyDir covers
      # all five — nginx only writes to these on large bodies /
      # upstream responses; gRPC-Web POSTs are small proto frames.
      client_body_temp_path /tmp/client_body;
      proxy_temp_path       /tmp/proxy;
      fastcgi_temp_path     /tmp/fastcgi;
      uwsgi_temp_path       /tmp/uwsgi;
      scgi_temp_path        /tmp/scgi;

      # Single upstream: Envoy Gateway's stable Service DNS. Port
      # 8080 matches the Gateway listener (dashboard-gateway.yaml).
      upstream envoy_gateway {
        server rio-dashboard-envoy.envoy-gateway-system.svc.cluster.local:8080;
      }
      server {
        # 8080 not 80: runAsNonRoot means no CAP_NET_BIND_SERVICE →
        # can't bind <1024. The k8s Service maps :80 → targetPort:8080.
        listen 8080;

        # SPA: all unknown routes serve index.html, the client-side
        # router (svelte-routing / whatever P0274 picked) handles the
        # path. try_files short-circuits to the real file for static
        # assets (/assets/*.js, *.css).
        location / {
          root ${rioDashboard};
          try_files $uri /index.html;
        }

        # gRPC-Web is plain HTTP/1.1 POST with a length-prefixed
        # proto body. nginx proxies to the Envoy Gateway Service;
        # envoy's grpc_web filter (auto-injected when a GRPCRoute
        # attaches — listener.go:424-425) handles the gRPC-Web →
        # HTTP/2 gRPC translation and presents the mTLS client cert
        # (BackendTLSPolicy) to rio-scheduler.
        #
        # Pattern matches /rio.admin.AdminService/* and
        # /rio.scheduler.SchedulerService/* — the two services the
        # dashboard calls (GRPCRoute matches the same).
        location ~ ^/rio\.(admin|scheduler)\./ {
          proxy_pass http://envoy_gateway;
          proxy_http_version 1.1;

          # LOAD-BEARING: without proxy_buffering off, nginx buffers
          # the entire server-streaming response before flushing to
          # the client. GetBuildLogs / WatchBuild are live-tailing
          # streams — a build that runs for minutes would produce
          # ZERO output in the browser until completion. Envoy's
          # grpc_web filter emits length-prefixed DATA frames as they
          # arrive; nginx must pass them through unbuffered. Same
          # constraint the sidecar design had.
          proxy_buffering off;
          # nginx default proxy_read_timeout is 60s. The
          # ClientTrafficPolicy (dashboard-gateway-policy.yaml) sets
          # envoy's streamIdleTimeout to 1h for the LLVM-cold-ccache
          # case — a build that prints nothing for a while. Match
          # here or nginx cuts the stream first.
          proxy_read_timeout 3600s;

          # Pass through the headers envoy's CORS SecurityPolicy
          # allow-lists. Content-Type carries the application/
          # grpc-web+proto marker; X-Grpc-Web is the spec'd client
          # flag.
          proxy_set_header Content-Type $content_type;
          proxy_set_header X-Grpc-Web $http_x_grpc_web;
        }
      }
    }
  '';

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
        Env = baseEnv ++ extraEnv;
        Labels = mkLabels "rio-${name} — Nix build orchestration";
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
  # stays in the ConfigMap (infra/helm/rio-build/templates/fod-proxy.yaml)
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

  # Secrets bootstrap. Argo PreSync hook — runs before the main sync,
  # generates rio/hmac + rio/signing-key in AWS Secrets Manager IF THEY
  # DON'T EXIST (describe-secret guard). Regenerating would invalidate
  # in-flight assignment tokens / all narinfo signatures. ESO then syncs
  # them back into k8s Secrets. Public signing key goes to rio/signing-
  # key-pub so operators can read it without touching the private half.
  #
  # Needs nix (nix-store --generate-binary-cache-key), awscli2, openssl.
  # IRSA via the rio-bootstrap ServiceAccount gives it
  # secretsmanager:CreateSecret/PutSecretValue/DescribeSecret on rio/*.
  bootstrap =
    let
      script = pkgs.writeShellScript "rio-bootstrap" ''
        set -euo pipefail
        : "''${AWS_REGION:?}" "''${CHUNK_BUCKET:?}"

        if aws secretsmanager describe-secret --secret-id rio/hmac >/dev/null 2>&1; then
          echo "[bootstrap] rio/hmac already exists, skipping"
        else
          echo "[bootstrap] generating rio/hmac"
          # 32 raw bytes. SecretBinary (not SecretString) — the HMAC key
          # isn't text. ESO's decodingStrategy: None preserves raw bytes.
          openssl rand 32 > /tmp/hmac
          aws secretsmanager create-secret --name rio/hmac \
            --secret-binary fileb:///tmp/hmac
        fi

        if aws secretsmanager describe-secret --secret-id rio/signing-key >/dev/null 2>&1; then
          echo "[bootstrap] rio/signing-key already exists, skipping"
        else
          echo "[bootstrap] generating rio/signing-key"
          tmp=$(mktemp -d)
          # Key name includes the bucket so narinfo `Sig:` lines identify
          # which cluster signed them. Format: name:base64-seed.
          nix-store --generate-binary-cache-key "rio-$CHUNK_BUCKET" \
            "$tmp/key.sec" "$tmp/key.pub"
          aws secretsmanager create-secret --name rio/signing-key \
            --secret-string "file://$tmp/key.sec"
          # Public half separately so operators can `get-secret-value`
          # it for their nix.conf trusted-public-keys without access to
          # the private half.
          aws secretsmanager create-secret --name rio/signing-key-pub \
            --secret-string "file://$tmp/key.pub"
          echo "[bootstrap] public key (add to nix.conf trusted-public-keys):"
          cat "$tmp/key.pub"
        fi
      '';
    in
    buildZstd {
      name = "rio-bootstrap";
      tag = "dev";
      maxLayers = 20;
      contents = baseContents ++ [
        pkgs.awscli2
        pkgs.openssl
        pkgs.nix
        pkgs.bash
        pkgs.coreutils
      ];
      config = {
        Entrypoint = [ "${script}" ];
        Env = [
          "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
          "PATH=${
            lib.makeBinPath [
              pkgs.awscli2
              pkgs.openssl
              pkgs.nix
              pkgs.coreutils
            ]
          }"
        ];
      };
      # mktemp needs /tmp.
      extraCommands = ''
        mkdir -p tmp
        chmod 1777 tmp
      '';
    };

  # Worker needs the nix toolchain + FUSE runtime + mount utilities.
  worker = mkImage {
    name = "worker";
    extraContents = workerExtraContents;
    extraEnv = workerExtraEnv;
    extraCommands = workerExtraCommands;
  };

  # ── VM-test aggregate: all five rio binaries, one image ──────────────
  # The five per-component images share the same rio-workspace closure
  # and differ ONLY in Entrypoint. k3s airgap-imports serially and
  # alphabetically before kubelet starts — five near-identical ~170MB
  # tarballs decompress back-to-back, burning ~125s of wall time under
  # TCG (k3s-full.nix:280). This image carries every component (worker's
  # contents are the superset) with NO Entrypoint; pods set `command:`
  # per container instead. One decompress cycle instead of five.
  #
  # NOT for prod — ECR pushes distinct per-component images (skopeo
  # docker-archive: transport) so rolling one component doesn't touch
  # the others. VM tests don't have that constraint.
  all = buildZstd {
    name = "rio-all";
    tag = "dev";
    maxLayers = 60;
    contents = baseContents ++ [ rio-workspace ] ++ workerExtraContents;
    config = {
      # No Entrypoint. buildLayeredImage's `contents` symlinks
      # rio-workspace/bin/* into /bin/, so pods use
      # `command: ["/bin/rio-gateway"]` etc.
      Env = baseEnv ++ workerExtraEnv;
      Labels = mkLabels "rio-all — all rio components (VM-test aggregate)";
    };
    extraCommands = workerExtraCommands;
  };
}
# ── Dashboard: nginx + SPA static bundle ───────────────────────────────
# No rio binary — just nginx serving the Svelte dist/ and proxying
# /rio.* gRPC-Web POSTs to the Envoy Gateway Service. Can't use mkImage
# (that's built around a rio-workspace Entrypoint).
#
# optionalAttrs: the coverage-mode mkDockerImages call site doesn't
# pass rioDashboard (nginx+static has no LLVM instrumentation). The
# flake's `dockerImages` linkFarm (mapAttrsToList) iterates all attrs,
# so an unconditional dashboard attr with rioDashboard=null would fail
# eval at `contents = [ ... null ]`. Emitting the attr only when the
# SPA derivation was provided keeps both call sites clean.
// lib.optionalAttrs (rioDashboard != null) {
  dashboard = buildZstd {
    name = "rio-dashboard";
    tag = "dev";
    maxLayers = 20;
    # rioDashboard in contents: buildLayeredImage symlinks it into the
    # image root so nginx's `root ${rioDashboard}` (a /nix/store path)
    # resolves. dashboardNginxConf is referenced by absolute store
    # path in Cmd — the layer closure includes it transitively.
    contents = [
      pkgs.nginx
      rioDashboard
    ];
    config = {
      Cmd = [
        "${pkgs.nginx}/bin/nginx"
        "-c"
        "${dashboardNginxConf}"
      ];
      Labels = mkLabels "rio-dashboard — Svelte SPA + gRPC-Web proxy to Envoy Gateway";
      ExposedPorts."8080/tcp" = { };
    };
    # /tmp: pid + temp_path directives (readOnlyRootFilesystem in the
    # Deployment — see dashboardNginxConf). /var/log/nginx: nginx
    # opens /var/log/nginx/error.log at parse-start BEFORE reading
    # our error_log directive; the dir must exist or it ENOENTs.
    extraCommands = ''
      mkdir -p tmp var/log/nginx
      chmod 1777 tmp
    '';
  };
}
