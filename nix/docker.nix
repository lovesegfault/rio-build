# Container images for each rio component, built via dockerTools.buildLayeredImage.
#
# Usage:
#   nix build .#dockerImages.gateway      # single image tarball at result
#   nix build .#dockerImages        # all at result/{gateway,scheduler,store,controller,builder}.tar.zst
#   docker load < result/gateway.tar.zst
#   docker run rio-gateway:dev --help
#
# buildLayeredImage stratifies by popularity — the Nix store closure of each
# binary is split into layers by reference count, so shared deps (glibc,
# openssl, rustls) land in reusable layers. Pulling a second rio image only
# fetches the component-specific top layer.
#
# Worker is the outlier: it needs `fuse3` + `util-linux` (mount/umount for
# overlay teardown), a static /bin/sh for the build sandbox, and the CA
# bundle for fetcher TLS. Gateway/scheduler/store are minimal.
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
  # actual builder transcode; the push.rs cross-ref comment catches
  # Rust↔Nix drift on review.
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
  #
  # gzip the tar: the OCI layout's config json blobs contain literal
  # store paths (Env=["PATH=/nix/store/..."]) which Nix's reference
  # scanner picks up — making the seed's closure include the full
  # uncompressed image contents (glibc, util-linux, etc.) even though
  # those are already INSIDE the compressed layer blobs. Compressing
  # the outer tar masks the strings; `ctr image import` and k3s
  # wharfie both auto-detect gzip.
  #
  # tar must be byte-reproducible: this is an input-addressed runCommand,
  # so the SAME store path can be produced independently on multiple
  # machines (local + remote builders). If two builds yield different
  # bytes under the same path, any downstream derivation that pinned the
  # narHash (e.g. the AMI's closureInfo registration) fails with
  # "hash mismatch importing path" when the build host has the other
  # variant. The skopeo OCI layout itself IS reproducible (Q9.2 — and
  # verified again here: extracted contents diff clean across builders);
  # what leaks is tar metadata: readdir-order entry sequencing, build
  # wall-clock mtimes, and the building user's name on different hosts.
  # gzip's -n already strips its own mtime header.
  #
  # The N transcodes run in PARALLEL into per-image layouts that are
  # merged afterwards. Serially they were the longest single link in
  # the CI warm stage (~20s × N images of zstd recompression). The
  # merge produces byte-identical output to a serial copy into one
  # layout: the blobs are content-addressed (zstd level 6 of the same
  # input is deterministic, re-verified by executor-seed-layer-parity),
  # the no-clobber copy dedups shared layers exactly like skopeo's own
  # already-present check did, and index.json keeps the images-list
  # manifest order. The cost is that shared base layers are transcoded
  # N times instead of once — wasted CPU on otherwise-idle cores, not
  # wall-clock.
  mkSeed =
    { name, images }:
    pkgs.runCommand "rio-${name}-seed.oci.tar.gz"
      {
        nativeBuildInputs = [
          pkgs.skopeo
          pkgs.gnutar
          pkgs.gzip
          pkgs.jq
        ];
      }
      ''
        ${lib.concatStrings (
          lib.imap0 (
            i:
            { ref, archive }:
            ''
              (${ociSkopeoCopy archive "oci:$TMPDIR/oci-${toString i}:${ref}"}) &
            ''
          ) images
        )}
        wait

        d=$TMPDIR/oci
        mkdir -p $d/blobs/sha256
        cp --no-preserve=mode $TMPDIR/oci-0/oci-layout $d/
        for i in $(seq 0 ${toString (builtins.length images - 1)}); do
          for blob in "$TMPDIR/oci-$i"/blobs/sha256/*; do
            [ -e "$d/blobs/sha256/''${blob##*/}" ] \
              || cp --no-preserve=mode "$blob" $d/blobs/sha256/
          done
        done
        # First layout's index as the template; manifests concatenated
        # in images-list order (= the order a serial copy appends them).
        jq -cs '.[0] + {manifests: (map(.manifests) | add)}' \
          ${
            lib.concatStringsSep " " (lib.imap0 (i: _: ''"$TMPDIR/oci-${toString i}/index.json"'') images)
          } > $d/index.json

        tar -C $d -c \
          --sort=name --mtime='@1' --owner=0 --group=0 --numeric-owner \
          . | gzip -1n > $out
      '';

  # Common to all images. cacert for TLS (S3, upstream binary caches),
  # tzdata so log timestamps aren't UTC-only.
  baseContents = [
    pkgs.cacert
    pkgs.tzdata
  ];

  # UID 65532 = distroless nonroot. Control-plane images (scheduler/
  # gateway/controller/store) run unprivileged; K8s securityContext.
  # runAsUser enforces it (templates/_helpers.tpl rio.podSecurityContext
  # — PSA restricted). Image-level User is defense-in-depth for `docker
  # run` without k8s. Builder image does NOT set this — it needs root
  # for FUSE mount + overlay teardown (rio-builders/rio-fetchers
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
  # them. Worker is the only component that needs fuse/mount at
  # runtime, but the aggregate must be a superset of every component.
  #
  # sandboxShell: the minimal static ash (`busybox-sandbox-shell`) exposed
  # at /bin/sh inside every build sandbox (RIO_SANDBOX_SHELL). nixpkgs
  # builds assume /bin/sh exists; the native executor bind-mounts this
  # store path read-only into each sandbox. This is the same shell CppNix
  # uses for its sandbox and the one the differential parity gate runs
  # both arms with — keeping production identical to what the gate
  # validates (and avoiding the full multi-call busybox's extra applets
  # being reachable through /bin/sh argv[0] dispatch).
  sandboxShell = "${pkgs.busybox-sandbox-shell}/bin/busybox";

  builderExtraContents = [
    pkgs.fuse3 # fusermount3, required by the fuser crate's AutoUnmount
    pkgs.util-linuxMinimal # mount, umount for overlay teardown
    pkgs.busybox-sandbox-shell # minimal static ash for the build sandbox (RIO_SANDBOX_SHELL)
    pkgs.pkgsStatic.busybox # in-image /bin/sh + utilities (debugging/exec); NOT the sandbox shell

    # The worker process runs as root and the sandbox writes its own
    # /etc/passwd inside each chroot; the image-level stubs only serve
    # libraries that look up uid 0.
    (pkgs.writeTextDir "etc/passwd" ''
      root:x:0:0:root:/root:/bin/sh
    '')
    (pkgs.writeTextDir "etc/group" ''
      root:x:0:
    '')
  ];

  builderExtraEnv = [
    # mount/umount and fusermount3 must be findable by the executor.
    "PATH=${
      lib.makeBinPath [
        pkgs.fuse3
        pkgs.util-linuxMinimal
      ]
    }"
    # Static shell exposed as /bin/sh inside every build sandbox.
    "RIO_SANDBOX_SHELL=${sandboxShell}"
    # CA bundle mounted read-only into network (fixed-output) sandboxes;
    # same bundle SSL_CERT_FILE already points at for the worker itself.
    "RIO_CA_BUNDLE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
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
  # Proxy target is the Cilium per-Gateway Service. Cilium reconciles
  # the rio-dashboard Gateway into a Deployment + Service named
  # `cilium-gateway-<gateway-name>` in the Gateway's OWN namespace
  # (rio-system) — not a separate operator namespace. gRPC-Web
  # translation happens at rio-scheduler (tonic-web layer, D3); the
  # gateway is plain HTTP routing.
  #
  # Baked-in beats runtime envsubst — a broken upstream is a build-
  # time failure not a runtime surprise. If a deploy renames the
  # Gateway or moves it to a different namespace, this config needs
  # a matching rebuild.
  #
  # r[dash.auth.method-gate+3] readonly allow-list. MUST match the
  # rio-scheduler-readonly HTTPRoute in dashboard-gateway.yaml — the
  # dashboard-method-gate-parity check (nix/misc-checks.nix) diffs the
  # two and fails CI on divergence. nginx is reached via `kubectl
  # port-forward svc/rio-dashboard` (in-cluster, hits nginx directly,
  # NOT the Gateway), so it must enforce the same allow-list or
  # mutating RPCs are fail-OPEN to any port-forwarded browser.
  dashboardReadonlyAdmin = [
    "ClusterStatus"
    "ListExecutors"
    "ListPoisoned"
    "ListBuilds"
    "GetDerivationLogs"
    "ListTenants"
    "GetBuildGraph"
    "GetSpawnIntents"
  ];
  dashboardReadonlyScheduler = [
    "WatchBuild"
    "QueryBuildStatus"
  ];
  # nginx with njs: r[sched.sla.threat.read-path-auth] gates
  # ListPoisoned/ListTenants/GetBuildGraph on a service token. The
  # browser can't hold the HMAC key, so the proxy mints
  # `ServiceClaims{caller:"rio-dashboard", expiry_unix:now+60}` per
  # request and injects it as `x-rio-service-token`. Same envelope as
  # rio-auth's ServiceTokenInterceptor (b64url(json).b64url(hmac)).
  #
  # Config + njs module live in standalone files (.conf / .js) so
  # they get syntax highlighting and lint tools, and so this file
  # stays about layered-image construction. Allow-lists stay here as
  # Nix data — they're shared with dashboardReadonlyMethods (the
  # method-gate-parity check) and substituted into the .conf template.
  dashboardNginx = pkgs.nginx.override {
    modules = [ pkgs.nginxModules.njs ];
  };
  dashboardServiceTokenJs = pkgs.writeText "service-token.js" (
    builtins.readFile ./dashboard-service-token.js
  );
  dashboardNginxConf = pkgs.replaceVars ./dashboard-nginx.conf {
    mimeTypes = "${dashboardNginx}/conf/mime.types";
    serviceTokenJs = dashboardServiceTokenJs;
    spaRoot = rioDashboard;
    readonlyAdminAlt = lib.concatStringsSep "|" dashboardReadonlyAdmin;
    readonlySchedulerAlt = lib.concatStringsSep "|" dashboardReadonlyScheduler;
  };

  mkImage =
    {
      name,
      # Per-crate bin derivations (rio-crates.* entries) shipped in this
      # image. The first one is the entrypoint binary. buildLayeredImage
      # content-addresses layers, so shared deps (glibc, openssl, …)
      # still collapse into shared layers across images.
      bins,
      # config.User. null → no User field (image runs as root). Control-
      # plane images pass nonrootUser (65532:65532); builder leaves it
      # null (needs root for FUSE).
      user ? null,
      extraContents ? [ ],
      extraEnv ? [ ],
      extraCommands ? "",
    }:
    buildZstd {
      name = "rio-${name}";
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
  # Full /service/method paths for the readonly allow-list. Consumed
  # by the dashboard-method-gate-parity check (nix/misc-checks.nix) to
  # diff against the Cilium Gateway HTTPRoute — closes the drift class
  # where nginx and the Gateway implement r[dash.auth.method-gate+3]
  # independently.
  dashboardReadonlyMethods =
    map (m: "/rio.admin.AdminService/${m}") dashboardReadonlyAdmin
    ++ map (m: "/rio.scheduler.SchedulerService/${m}") dashboardReadonlyScheduler;

  # Exported for checks.dashboard-nginx-conf-guard (misc-checks.nix).
  inherit dashboardNginxConf dashboardNginx;

  gateway = mkImage {
    name = "gateway";
    bins = [ rio-crates.rio-gateway ];
    user = nonrootUser;
    extraContents = nonrootEtc;
  };
  # r[impl sec.image.control-plane-minimal]
  # Control-plane images carry ONLY the component binary. rio-cli is NOT
  # bundled here — admin ops run it locally via `cargo xtask k8s cli`
  # (with_cli_tunnel port-forwards 9001/9002 + fetches the service-HMAC key).
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
  # Needs rio-cli (keygen), awscli2, openssl. No Nix closure: the
  # signing keypair comes from `rio-cli keygen`, which emits the same
  # name:base64 format `nix-store --generate-binary-cache-key` did.
  # IRSA grants: defined ONLY in infra/eks/secrets.tf
  # (aws_iam_policy.rio_bootstrap); bootstrap-iam-parity pins every
  # (action, resource) pair against this script's calls.
  #
  # Exposed (not let-local) so the bootstrap-idempotent check
  # (nix/misc-checks.nix) can run it against a mocked aws CLI and
  # assert the signing-key block converges from partial state.
  #
  # r[impl infra.bootstrap.secret-state-probe]
  # (The script body lives in bootstrap-job.sh — outside tracey's
  # extension set; this export is the scannable anchor. secret_state
  # is the sole describe-secret site, pinned by
  # bootstrap-probe-conformance.)
  # r[verify infra.bootstrap.secret-state-probe]
  # (bootstrap-idempotent scenarios J-M: deletion arm per secret
  # class + fail-closed create-only guards; bootstrap-probe-conformance:
  # the count-pinned deny.)
  # r[impl infra.bootstrap.pair-probe-byte-exact]
  # r[verify infra.bootstrap.pair-probe-byte-exact]
  # (bootstrap-idempotent scenario N: a newline-corrupted pub planted
  # directly in provider state must detect, heal to canonical bytes,
  # and settle consistent.)
  bootstrapScript = pkgs.writeShellScript "rio-bootstrap" (builtins.readFile ./bootstrap-job.sh);
  # The real rio-cli package, exported so the bootstrap-idempotent
  # check runs the genuine binary (round-16 MP4: the previous mock
  # could not represent the seed-only secret population whose
  # mishandling was bug_023; byte-faithful harnesses only).
  rioCli = rio-crates.rio-cli;
  bootstrap = buildZstd {
    name = "rio-bootstrap";
    tag = "dev";
    maxLayers = 20;
    contents =
      baseContents
      ++ nonrootEtc
      ++ [
        pkgs.awscli2
        pkgs.openssl
        pkgs.openssh
        rio-crates.rio-cli
        pkgs.bash
        pkgs.coreutils
      ];
    config = {
      Entrypoint = [ "${bootstrapScript}" ];
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
            pkgs.openssh
            rio-crates.rio-cli
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

  # ── AMI layer-cache warm: builder image as an OCI archive ─────────────
  # r[impl infra.node.prebake-layer-warm]
  #
  # PodSpec image refs stay <ECR>/rio-builder:<git-sha> — this archive
  # is NOT pulled. It's `ctr image import`ed into containerd's content
  # store at AMI bake time (kubelet preStart, eks-node.nix) so the first
  # pod's ECR pull finds most layer blobs already local and fetches only
  # the delta (typically the ~10 MB rio-builder top layer, or zero if
  # AMI and deploy are at the same commit).
  #
  # Fetcher pods use the same rio-builder image; the controller injects
  # RIO_EXECUTOR_KIND per-pod (rio-controller pool reconciler), so one
  # warm covers both. Ref names use seed.local/ so they're obviously not
  # pull-addressable; the name is a GC root only (the io.cri-containerd.
  # pinned label keeps kubelet image-GC from deleting the record, the
  # record's existence keeps containerd content-GC from deleting blobs).
  executorSeed = mkSeed {
    name = "executor";
    images = [
      {
        ref = "seed.local/rio-builder:prebaked";
        archive = builder;
      }
    ];
  };

  # The load-bearing check: executorSeed's layer-blob digests MUST equal
  # what push.rs would put in ECR, or the warm is a no-op. Re-runs the
  # exact ociSkopeoCopy transform on builder into a fresh dir and asserts
  # every resulting blob digest is also in the seed. Catches:
  #   - ociSkopeoCopyArgs drifted from what executorSeed used (refactor)
  #   - skopeo version bump changed zstd output (unlikely — Q9.2 verified
  #     bit-reproducible, but a defence)
  # Does NOT catch push.rs flag drift (Rust↔Nix); that's the
  # SKOPEO_OCI_ZSTD_ARGS cross-ref comment's job.
  executorSeedLayerParity =
    pkgs.runCommand "rio-executor-seed-layer-parity"
      {
        nativeBuildInputs = [
          pkgs.skopeo
          pkgs.gnutar
          pkgs.gzip
        ];
      }
      # r[verify infra.node.prebake-layer-warm]
      ''
        set -euo pipefail
        # Reference: what push.rs would produce.
        ${ociSkopeoCopy builder "oci:$TMPDIR/ref:builder"}
        ls $TMPDIR/ref/blobs/sha256 | sort > $TMPDIR/ref-digests

        # Seed: untar, list its blobs.
        mkdir $TMPDIR/seed
        tar -C $TMPDIR/seed -xzf ${executorSeed}
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
        echo OK > $out
      '';

  # ── VM-test seed: all per-component images, one OCI archive ──────────
  # k3s airgap-imports serially before kubelet starts — per-component
  # docker-archives would decompress back-to-back (~125s wall under TCG,
  # k3s-full.nix:280) and re-expand the same shared layers each time.
  # mkSeed packs all manifests into ONE oci-layout tarball with
  # blob-level dedup, so the import is one decompress pass over
  # union(layers). k3s's agent-images preload (services.k3s.images)
  # walks index.json and registers all refs; pods then reference
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
      dashboardNginx
      rioDashboard
    ];
    config = {
      Cmd = [
        "${dashboardNginx}/bin/nginx"
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
