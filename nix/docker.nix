# Container images for each rio component, built via dockerTools.buildLayeredImage.
#
# Usage:
#   nix build .#docker-gateway      # single image tarball at result
#   nix build .#dockerImages        # all 6 at result/{gateway,scheduler,store,controller,builder,fetcher}.tar.zst
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
  # Per-crate stripped bin derivations, keyed rio-gateway / rio-builder
  # / … (crate2nix.nix memberBins). Each image lists exactly the crates
  # it ships so its build closure doesn't pull in unrelated binaries.
  rio-crates,
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
  # so the airgap import budget (~15min on remote builders, serial
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

  # ── skopeo docker-archive: → oci: transcode flags ─────────────────────
  # The ECR push (xtask/src/k8s/eks/push.rs SKOPEO_OCI_ZSTD_ARGS) and the
  # AMI's executorSeed below MUST use the IDENTICAL skopeo flag set so
  # layer-blob digests match: containerd skips a pull layer iff the
  # content store already has a blob with that EXACT digest. A level
  # mismatch (e.g. 6 vs 9) silently defeats the warm — different
  # compressed bytes → different digest → full re-fetch, no error. The
  # executor-seed-layer-parity check catches drift between this and the
  # actual builder/fetcher transcode; the push.rs cross-ref comment
  # catches Rust↔Nix drift on review.
  #
  # `-f oci` (manifest format): push.rs needs it for ECR (manifest-tool
  # builds an OCI image index from per-arch manifests). The seed's `oci:`
  # destination forces OCI manifests anyway, but keeping the flag
  # identical means a future skopeo behaviour change can't skew them.
  ociSkopeoCopyArgs = [
    "--dest-compress-format"
    "zstd"
    "--dest-compress-level"
    "6"
    "-f"
    "oci"
  ];
  ociSkopeoCopy = src: dest: ''
    skopeo --insecure-policy --tmpdir="$TMPDIR" copy \
      ${lib.escapeShellArgs ociSkopeoCopyArgs} \
      docker-archive:${src} ${dest}
  '';

  # ── Multi-manifest OCI seed builder ───────────────────────────────────
  # Packs N docker-archive images into ONE oci: layout via repeated
  # skopeo copies, then tars it. The destination's content-addressed
  # blobs/sha256/ dedups shared layers across images, so the seed is
  # ~union(layers) not Σ(per-image-size). index.json ends up with N
  # manifests[] entries; `ctr image import --local` (or k3s's agent
  # airgap-images preload, which uses the same containerd importer)
  # registers all N refs from one tarball + one decompress pass.
  #
  # `oci:DIR:REF` (skopeo's oci-layout transport), then tar — NOT
  # `oci-archive:` directly. oci-archive: is single-manifest; the layout
  # transport is what supports the multi-ref dedup.
  mkSeed =
    { name, images }:
    pkgs.runCommand "rio-${name}-seed.oci.tar"
      {
        nativeBuildInputs = [
          pkgs.skopeo
          pkgs.gnutar
        ];
      }
      ''
        d=$TMPDIR/oci
        ${lib.concatMapStrings ({ ref, archive }: ociSkopeoCopy archive "oci:$d:${ref}") images}
        tar -C $d -cf $out .
      '';

  # Common to all images. cacert for TLS (S3, gRPC with mTLS if enabled),
  # tzdata so log timestamps aren't UTC-only.
  baseContents = [
    pkgs.cacert
    pkgs.tzdata
  ];

  # UID 65532 = distroless nonroot. Control-plane images (scheduler/
  # gateway/controller/store) run unprivileged; K8s securityContext.
  # runAsUser enforces it (templates/_helpers.tpl rio.podSecurityContext
  # — PSA restricted). Image-level User is defense-in-depth for `docker
  # run` without k8s. Builder/fetcher images do NOT set this — they need
  # root for FUSE mount + overlay teardown (rio-builders/rio-fetchers
  # namespaces stay at PSA privileged per ADR-019).
  nonrootUser = "65532:65532";
  nonrootEtc = [
    (pkgs.writeTextDir "etc/passwd" ''
      root:x:0:0:root:/root:/bin/false
      nonroot:x:65532:65532:nonroot:/:/bin/false
    '')
    (pkgs.writeTextDir "etc/group" ''
      root:x:0:
      nonroot:x:65532:
    '')
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
  #
  # nixForBuilder: pkgs.nix with movePath() patched to fall back to a
  # recursive copy on EXDEV (I-185). When the builder pod runs with
  # hostUsers:false (ADR-012), rio-builder's overlayfs mount happens
  # inside a non-init user namespace; the kernel then forces
  # redirect_dir=off (and refuses redirect_dir=on). overlayfs without
  # redirect_dir returns EXDEV for ANY rename(2) of a directory whose
  # target parent is a merge-type dir — and the overlay root (= the
  # chroot-store realStoreDir) is always merge-type. nix-daemon's
  # post-build movePath(chroot/{out} -> realStoreDir/{out}) is a raw
  # std::filesystem::rename with no fallback, so every DIRECTORY output
  # fails (file outputs rename fine — the kernel restriction is
  # directory-only). nix's moveFile() temp-then-rename fallback also
  # hits the same EXDEV. The patch makes movePath() copy on EXDEV.
  #
  # appendPatches re-derives the whole nix component scope from the
  # patched source; only nix-store and its reverse-deps actually
  # rebuild differently. Rebuild is cached until pkgs.nix.src bumps.
  #
  # `.nix-cli` (not the umbrella nix-everything): nix-everything pulls
  # in nix-functional-tests as a build-time dep; the local-overlay-
  # store/stale-file-handle test fails in OUR build sandbox regardless
  # of the patch (verified with a no-op README patch — same FAIL). The
  # test exercises nix's own local-overlay:// store backend, which we
  # don't use; nix-cli has bin/nix-daemon and is all the executor needs.
  #
  # Tracey: r[impl builder.overlay.userns-exdev] lives in overlay.rs
  # (the mount that triggers the kernel restriction); docker.nix is
  # outside the tracey impls.include set.
  nixForBuilder = (pkgs.nix.appendPatches [ ./patches/nix-movepath-exdev-fallback.patch ]).nix-cli;

  builderExtraContents = [
    nixForBuilder # nix-daemon --stdio, spawned per-build
    pkgs.fuse3 # fusermount3, required by the fuser crate's AutoUnmount
    pkgs.util-linux # mount, umount for overlay teardown

    # nix-daemon drops privs to a nixbld{N} user inside its sandbox.
    # It enumerates build users via getgrnam("nixbld")->gr_mem — the
    # EXPLICIT member list in /etc/group. A user's primary group (gid
    # field in passwd) does NOT appear in gr_mem; the member list must
    # be populated or daemon fails: "nixbld group has no members".
    #
    # 8 users: nix-daemon's nested sandbox claims one per build.
    # P0537: only one build runs per pod, so one would suffice; eight
    # costs nothing and matches the NixOS convention (nixbld1 at
    # 30001, etc).
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
    # The executor sets NIX_CONF_DIR per build; this image-level conf
    # is what `nix --version` etc. see when probed outside a build.
    (pkgs.writeTextDir "etc/nix/nix.conf" ''
      build-users-group = nixbld
      sandbox = true
      experimental-features = nix-command
    '')
  ];

  builderExtraEnv = [
    # nix-daemon --stdio must be findable. The worker's executor does
    # `Command::new("nix-daemon")` (no absolute path), relying on PATH.
    "PATH=${
      lib.makeBinPath [
        nixForBuilder
        pkgs.fuse3
        pkgs.util-linux
      ]
    }"
  ];

  # /tmp: nix-daemon's sandbox needs a tmpdir. Containers don't have
  # one by default. sticky-bit (1777) matches the standard /tmp.
  #
  # extraCommands runs in the customisation layer's root dir (unprivileged;
  # nix's sandbox builder user) — paths are relative to image /.
  #
  # etc/{passwd,group}: buildLayeredImage's `contents` creates absolute
  # symlinks into /nix/store. containerd ≥2.2.0 (Go 1.24's stricter
  # os.DirFS path validation — containerd/containerd#12683) rejects
  # absolute-symlinked /etc/passwd with "path escapes from parent"
  # during user lookup. Dereference to regular files.
  derefEtc = ''
    for f in etc/passwd etc/group; do
      if [ -L "$f" ]; then
        cp --remove-destination "$(readlink -f "$f")" "$f"
      fi
    done
  '';
  builderExtraCommands = ''
    ${derefEtc}
    mkdir -p tmp
    chmod 1777 tmp
  '';

  # ── Dashboard nginx config ───────────────────────────────────────────
  # Proxy target is the Envoy Gateway operator-managed Service — NOT a
  # localhost sidecar (P0273 landed Gateway API CRDs; the operator
  # spins up envoy data-plane pods from the rio-dashboard Gateway, we
  # don't hand-configure envoy here).
  #
  # The Service lives in envoy-gateway-system (also templated as
  # .Values.dashboard.envoyGatewayNamespace for networkpolicy.yaml —
  # change BOTH together). ControllerNamespaceMode,
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
        # dashboard calls (GRPCRoute matches the same). No trailing
        # / after the second \. — the next token is the ServiceName
        # (AdminService, SchedulerService), not a path segment. The
        # / comes AFTER the service name.
        location ~ ^/rio\.(admin|scheduler)\. {
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
      # Per-crate bin derivations (rio-crates.* entries) shipped in this
      # image. The first one is the entrypoint binary. buildLayeredImage
      # content-addresses layers, so shared deps (glibc, openssl, …)
      # still collapse into shared layers across images.
      bins,
      # Override the image name independently of the entrypoint binary
      # name. Used for the fetcher image: same rio-builder binary,
      # different RIO_EXECUTOR_KIND env, distinct image name so k8s
      # can pull rio-fetcher:dev separately from rio-builder:dev.
      imageName ? "rio-${name}",
      # config.User. null → no User field (image runs as root). Control-
      # plane images pass nonrootUser (65532:65532); builder/fetcher
      # leave it null (need root for FUSE).
      user ? null,
      extraContents ? [ ],
      extraEnv ? [ ],
      extraCommands ? "",
    }:
    buildZstd {
      name = imageName;
      extraCommands = derefEtc + extraCommands;
      # "dev" not "latest": :latest defaults to imagePullPolicy=Always
      # in K8s (never checks local store), which breaks airgap k3s.
      # Non-latest tag → IfNotPresent default → locally-imported image
      # works. Real release images are tagged by CI with git SHAs
      # anyway; this tag is for local dev + VM tests.
      tag = "dev";

      # Max layer count. Default is 100; Docker's hard limit is 127.
      # More layers = finer-grained caching but more tarball overhead.
      # 60 is a reasonable sweet spot for our closure sizes.
      maxLayers = 60;

      contents = baseContents ++ bins ++ extraContents;

      config = {
        Entrypoint = [ "${lib.head bins}/bin/rio-${name}" ];
        Env = baseEnv ++ extraEnv;
        Labels = mkLabels "rio-${name} — Nix build orchestration";
      }
      // lib.optionalAttrs (user != null) { User = user; };
    };
in
rec {
  gateway = mkImage {
    name = "gateway";
    bins = [ rio-crates.rio-gateway ];
    user = nonrootUser;
    extraContents = nonrootEtc;
  };
  # r[impl sec.image.control-plane-minimal]
  # Control-plane images carry ONLY the component binary. rio-cli is NOT
  # bundled here — admin ops run it locally via `cargo xtask k8s cli`
  # (with_cli_tunnel port-forwards 9001/9002 + fetches mTLS client cert).
  # See xtask/src/k8s/mod.rs and the spec marker above; bundling tooling
  # in a control-plane image is an execution primitive in a compromised
  # pod.
  scheduler = mkImage {
    name = "scheduler";
    bins = [ rio-crates.rio-scheduler ];
    user = nonrootUser;
    extraContents = nonrootEtc;
  };
  store = mkImage {
    name = "store";
    bins = [ rio-crates.rio-store ];
    user = nonrootUser;
    extraContents = nonrootEtc;
  };

  # Controller is the lightest — it only talks to the K8s API and
  # the scheduler's gRPC. No nix, no fuse, no PG. Just cacert for
  # the in-cluster TLS connection (kube-apiserver serves HTTPS;
  # the service-account CA is mounted separately but kube-rs also
  # reads SSL_CERT_FILE for the initial client config probe).
  controller = mkImage {
    name = "controller";
    bins = [ rio-crates.rio-controller ];
    user = nonrootUser;
    extraContents = nonrootEtc;
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
      contents =
        baseContents
        ++ nonrootEtc
        ++ [
          pkgs.awscli2
          pkgs.openssl
          pkgs.nix
          pkgs.bash
          pkgs.coreutils
        ];
      config = {
        Entrypoint = [ "${script}" ];
        # PSA restricted (rio.podSecurityContext) sets runAsNonRoot=true.
        # Without an image-level User, kubelet would need runAsUser
        # explicitly; setting it here matches the other control-plane
        # images and makes bare `docker run` unprivileged too.
        User = nonrootUser;
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
        ${derefEtc}
        mkdir -p tmp
        chmod 1777 tmp
      '';
    };

  # Builder needs the nix toolchain + FUSE runtime + mount utilities.
  builder = mkImage {
    name = "builder";
    bins = [ rio-crates.rio-builder ];
    extraContents = builderExtraContents;
    extraEnv = builderExtraEnv;
    extraCommands = builderExtraCommands;
  };

  # Fetcher: same rio-builder binary, RIO_EXECUTOR_KIND=fetcher env
  # baked in per ADR-019 §Terminology. FOD-only, open egress,
  # hash-check bounded. Same contents as builder (needs the same nix
  # toolchain to run FOD fetchers like curl/git).
  fetcher = mkImage {
    name = "builder"; # Entrypoint is still /bin/rio-builder
    imageName = "rio-fetcher";
    bins = [ rio-crates.rio-builder ];
    extraContents = builderExtraContents;
    extraEnv = builderExtraEnv ++ [ "RIO_EXECUTOR_KIND=fetcher" ];
    extraCommands = builderExtraCommands;
  };

  # ── AMI layer-cache warm: builder+fetcher as one OCI archive ──────────
  # r[impl infra.node.prebake-layer-warm]
  #
  # PodSpec image refs stay <ECR>/rio-{builder,fetcher}:<git-sha> — this
  # archive is NOT pulled. It's `ctr image import`ed into containerd's
  # content store at AMI bake time (kubelet preStart, eks-node.nix) so
  # the first pod's ECR pull finds most layer blobs already local and
  # fetches only the delta (typically the ~10 MB rio-builder top layer,
  # or zero if AMI and deploy are at the same commit).
  #
  # builder and fetcher share every layer — only config.Env differs — so
  # mkSeed's blob dedup yields ~124 MB not 2×114 MB. Ref names use
  # seed.local/ so they're obviously not pull-addressable; the names are
  # GC roots only (the io.cri-containerd.pinned label on them keeps
  # kubelet image-GC from deleting the record, the record's existence
  # keeps containerd content-GC from deleting the layer blobs).
  executorSeed = mkSeed {
    name = "executor";
    images = [
      {
        ref = "seed.local/rio-builder:prebaked";
        archive = builder;
      }
      {
        ref = "seed.local/rio-fetcher:prebaked";
        archive = fetcher;
      }
    ];
  };

  # The load-bearing check: executorSeed's layer-blob digests MUST equal
  # what push.rs would put in ECR, or the warm is a no-op. Re-runs the
  # exact ociSkopeoCopy transform on builder/fetcher into a fresh dir and
  # asserts every resulting blob digest is also in the seed. Catches:
  #   - ociSkopeoCopyArgs drifted from what executorSeed used (refactor)
  #   - skopeo version bump changed zstd output (unlikely — Q9.2 verified
  #     bit-reproducible, but a defence)
  # Does NOT catch push.rs flag drift (Rust↔Nix); that's the
  # SKOPEO_OCI_ZSTD_ARGS cross-ref comment's job.
  #
  # Also asserts dedup actually happened: total blob count is ≤ layers+4
  # (N shared layers + 2 configs + 2 manifests), not 2N+4. maxLayers=60
  # bounds N; the check uses a generous ≤70 ceiling so a future
  # maxLayers tweak doesn't spuriously fail it.
  executorSeedLayerParity =
    pkgs.runCommand "rio-executor-seed-layer-parity"
      {
        nativeBuildInputs = [
          pkgs.skopeo
          pkgs.gnutar
          pkgs.jq
        ];
      }
      # r[verify infra.node.prebake-layer-warm]
      ''
        set -euo pipefail
        # Reference: what push.rs would produce (per-image, no dedup).
        ${ociSkopeoCopy builder "oci:$TMPDIR/ref:builder"}
        ${ociSkopeoCopy fetcher "oci:$TMPDIR/ref:fetcher"}
        ls $TMPDIR/ref/blobs/sha256 | sort > $TMPDIR/ref-digests

        # Seed: untar, list its blobs.
        mkdir $TMPDIR/seed
        tar -C $TMPDIR/seed -xf ${executorSeed}
        ls $TMPDIR/seed/blobs/sha256 | sort > $TMPDIR/seed-digests

        # Parity: every reference blob must be in the seed. comm -23
        # prints lines unique to ref (i.e., what the seed is missing).
        missing=$(comm -23 $TMPDIR/ref-digests $TMPDIR/seed-digests)
        if [ -n "$missing" ]; then
          echo "FAIL: seed is missing blobs that ECR push would produce:" >&2
          echo "$missing" >&2
          echo "→ ociSkopeoCopyArgs drifted between executorSeed and the" >&2
          echo "  reference transcode, or skopeo's zstd output changed." >&2
          exit 1
        fi

        # Dedup: builder+fetcher share all layers, so seed blob count
        # should be ~N+4 not ~2N+4. Reference dir (no dedup across the
        # two copies into the SAME layout — actually it does dedup too,
        # since blobs/sha256 is content-addressed). Compare against the
        # manifests' declared layer counts instead.
        n_layers=$(jq -s '[.[].layers[]] | unique | length' \
          $(jq -r '.manifests[].digest | sub("sha256:";"")' $TMPDIR/seed/index.json \
            | sed "s|^|$TMPDIR/seed/blobs/sha256/|"))
        n_blobs=$(wc -l < $TMPDIR/seed-digests)
        echo "seed: $n_blobs blobs, $n_layers unique layers across 2 manifests"
        # N layers + 2 configs + 2 manifests. Allow slack of 2 for index
        # blobs some skopeo versions emit.
        if [ "$n_blobs" -gt "$((n_layers + 6))" ]; then
          echo "FAIL: seed has $n_blobs blobs but only $n_layers unique" >&2
          echo "  layers — dedup did not collapse builder∩fetcher." >&2
          exit 1
        fi

        echo OK > $out
      '';

  # ── VM-test seed: all 6 per-component images, one OCI archive ────────
  # k3s airgap-imports serially before kubelet starts — six per-component
  # docker-archives would decompress back-to-back (~125s wall under TCG,
  # k3s-full.nix:280) and re-expand the same shared layers six times.
  # mkSeed packs all six manifests into ONE oci-layout tarball with
  # blob-level dedup, so the import is one decompress pass over
  # union(layers). k3s's agent-images preload (services.k3s.images)
  # walks index.json and registers all six refs; pods then reference
  # `rio-<component>:dev` directly with no `command:` override.
  #
  # Replaces the former `all` aggregate (one image, all binaries, no
  # Entrypoint, pods set command:). That image pulled in rio-workspace
  # (every binary) which forced building all 657 crate deps even when
  # only granular images were needed, AND was pushed to ECR via the
  # dockerImages linkFarm where it was never used (W1, PLAN-DEPLOY-WINS).
  # vmTestSeed is excluded from the linkFarm (oci-archive, not
  # docker-archive — push.rs's skopeo docker-archive: would reject it).
  #
  # Refs MUST be fully-normalized (docker.io/library/…). containerd's
  # OCI importer registers org.opencontainers.image.ref.name verbatim
  # — unlike docker-archive RepoTags which it normalizes. CRI then
  # looks up the pod's `image: rio-gateway` as `docker.io/library/
  # rio-gateway:dev` (familiar-name normalization). A bare `rio-gateway:
  # dev` ref is an exact-string miss → ErrImagePull in the airgapped VM.
  vmTestSeed =
    let
      dev = n: archive: {
        ref = "docker.io/library/rio-${n}:dev";
        inherit archive;
      };
    in
    mkSeed {
      name = "vmtest";
      images = [
        (dev "gateway" gateway)
        (dev "scheduler" scheduler)
        (dev "store" store)
        (dev "controller" controller)
        (dev "builder" builder)
        (dev "fetcher" fetcher)
      ];
    };
}
# ── Dashboard: nginx + SPA static bundle ───────────────────────────────
# No rio binary — just nginx serving the Svelte dist/ and proxying
# /rio.* gRPC-Web POSTs to the Envoy Gateway Service. Can't use mkImage
# (that's built around a rio-* binary Entrypoint).
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
