# Two-node k3s fixture running the full Helm chart.
#
# First time gateway.yaml / scheduler.yaml / store.yaml / pdb.yaml /
# rbac.yaml / postgres-secret.yaml are applied in CI. Closes the
# "production uses pod path, VM tests use systemd" gap — and the
# scheduler.replicas=2 podAntiAffinity spreads across server+agent,
# enabling the leader-election scenario.
#
# Topology: k3s-server (8GB) + k3s-agent (6GB) + client (1GB).
# ~15GB total (normal), ~19GB coverage — use withMinCpu 8 in flake.nix.
#
# Airgapped: every image preloaded via services.k3s.images on BOTH
# nodes (pods can schedule on either). NodePort gateway → client
# connects to k3s-server:32222.
{
  pkgs,
  rio-workspace,
  rioModules,
  dockerImages,
  nixhelm,
  system,
  # Coverage: passes coverage.enabled=true to the chart → each rio
  # pod gets LLVM_PROFILE_FILE + hostPath /var/lib/rio/cov. Profraws
  # land on the NODE (not pod) filesystem. collectCoverage tars them.
  # Worker (STS) coverage NOT wired — builders.rs would need an
  # extraEnv field; standalone scenarios already cover worker code.
  coverage ? false,
  ...
}:
let
  # k3s minor pinned to the EKS control-plane version so VM tests
  # exercise the same API surface as the reference deploy. Derives
  # `k3s_1_35` from `kubernetes_version = "1.35"` — eval fails if
  # nixpkgs lacks that attr (k3s lagging EKS), which is the desired
  # signal: don't bump pins.nix past what we can test.
  pins = import ../../pins.nix;
  k3sAttr = "k3s_" + builtins.replaceStrings [ "." ] [ "_" ] pins.kubernetes_version;
  k3sPinned =
    pkgs.${k3sAttr} or (throw ''
      nix/pins.nix sets kubernetes_version = "${pins.kubernetes_version}" but
      nixpkgs has no `${k3sAttr}`. Either nixpkgs needs a bump, or the EKS pin
      is ahead of what k3s ships — VM tests can't validate that combination.
    '');

  common = import ../common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };
  helmRender = import ../../helm-render.nix { inherit pkgs nixhelm system; };
  envoyGatewayRender = import ../../envoy-gateway-render.nix { inherit pkgs nixhelm system; };
  pulled = import ../../docker-pulled.nix { inherit pkgs; };
  mkPkiK8s = import ../lib/pki-k8s.nix { inherit pkgs; };
  jwtKeys = import ../lib/jwt-keys.nix;
in
{
  # --set overrides layered on top of vmtest-full.yaml. scenarios/
  # lifecycle.nix passes autoscaler tuning (pollSecs=3 etc).
  extraValues ? { },
  # Extra images for the airgap set. dockerImages.all covers every rio-*
  # binary but NOT squid (dockerImages.fod-proxy is a separate image).
  # Scenarios that enable fodProxy.enabled=true need the squid image
  # preloaded or the pod goes ImagePullBackOff (airgapped — no pull).
  extraImages ? [ ],
  # JWT pubkey mount — scheduler+store get the rio-jwt-pubkey ConfigMap
  # mounted at /etc/rio/jwt; gateway gets the rio-jwt-signing Secret.
  # Uses a fixed test keypair (lib/jwt-keys.nix) passed via --set so
  # the Helm-rendered ConfigMap/Secret carry valid content. The
  # interceptor is DUAL-MODE (header-absent → pass-through), so
  # enabling JWT here doesn't break tests that don't send tokens.
  jwtEnabled ? false,
  # Additional values files layered after vmtest-full.yaml (Helm -f
  # last-wins). privileged-hardening-e2e passes [vmtest-full-nonpriv.
  # yaml] to flip workerPool.privileged:false + devicePlugin.enabled
  # without duplicating the full base values file.
  extraValuesFiles ? [ ],
  # --set (typed) overrides. bool/int values that MUST NOT be coerced
  # to string — e.g. bootstrap.enabled=true (a bool the template checks
  # with {{- if .Values... }}) or scheduler.replicas=2 (an int the
  # Deployment spec expects). --set-string "true" happens to be truthy
  # in Go templating (non-empty string) but --set-string "2" becomes
  # replicas: "2" in the rendered Deployment, which k8s API validation
  # rejects. prod-parity fixture passes bootstrap.enabled here.
  extraValuesTyped ? { },
  # Envoy Gateway (dashboard gRPC-Web). Preloads envoyproxy/gateway +
  # envoyproxy/envoy:distroless images, renders the gateway-helm chart
  # as k3s manifests (CRDs+operator+certgen), sets dashboard.enabled=
  # true in the rio chart so the Gateway/GRPCRoute/EnvoyProxy CRDs
  # render. Adds a rio-dashboard-envoy-tls Secret to the PKI so the
  # operator's client-cert ref resolves. Heavyweight — ~200MB of extra
  # images and ~60s of operator reconcile time; only enable for
  # dashboard-specific scenarios.
  envoyGatewayEnabled ? false,
}:
let
  # ── Shared cluster secrets ──────────────────────────────────────────
  # Cleartext fine for an airgapped VM test; real clusters use a random
  # token. Both server and agent read this file.
  tokenFile = pkgs.writeText "k3s-token" "rio-vm-test-token-not-secret";

  # ── Helm-rendered manifests ─────────────────────────────────────────
  # helm-render.nix splits into 00-crds / 01-rbac / 02-workloads. PG
  # subchart output lands in 02-workloads (it's StatefulSet+Service+
  # Secret, not RBAC). k3s applies in filename order → CRDs before
  # RBAC before workloads.
  helmRendered = helmRender {
    valuesFile = ../../../infra/helm/rio-build/values/vmtest-full.yaml;
    inherit extraValuesFiles;
    # jwt.publicKey / jwt.signingSeed are base64 strings — must go
    # through --set-string (extraSet), not --set (extraSetTyped would
    # try YAML-parsing the trailing `=` padding). Merged with caller's
    # extraValues; caller wins on collision (// is right-biased).
    extraSet = {
      # Postgres tag matches the preloaded FOD (imageTag passthru =
      # finalImageTag). Bumping docker-pulled.nix auto-bumps here.
      # vmtest-full.yaml keeps registry/repository (stable); the tag
      # is the drift axis on nixhelm chart bumps.
      "postgresql.image.tag" = pulled.bitnami-postgresql.imageTag;
    }
    // (pkgs.lib.optionalAttrs jwtEnabled {
      "jwt.publicKey" = jwtKeys.pubkeyB64;
      "jwt.signingSeed" = jwtKeys.seedB64;
    })
    // (pkgs.lib.optionalAttrs envoyGatewayEnabled {
      # containerd airgap cache is tag-indexed, not digest-indexed.
      # values.yaml default has @sha256: suffix (P0379 digest-pin) →
      # exact-string miss → ImagePullBackOff. Override to bare-tag
      # DERIVED from the preload FOD's finalImageName:finalImageTag
      # (destNameTag attr) — bumping docker-pulled.nix auto-bumps here.
      "dashboard.envoyImage" = pulled.envoy-distroless.destNameTag;
    })
    // (pkgs.lib.optionalAttrs coverage {
      # PSA restricted (ADR-019 default for control-plane) blocks the
      # hostPath "cov" volume (_helpers.tpl rio.covVolume) that
      # collects LLVM profraws to the node filesystem. Coverage is a
      # test-only mode; bumping to privileged here keeps production
      # values.yaml at restricted (r[sec.psa.control-plane-restricted]).
      "namespaces.system.psa" = "privileged";
      "namespaces.store.psa" = "privileged";
    })
    // extraValues;
    # coverage is a bool — must use --set (not --set-string) or
    # "false" becomes truthy (non-empty string). jwt.enabled likewise.
    extraSetTyped = {
      "coverage.enabled" = coverage;
    }
    // pkgs.lib.optionalAttrs jwtEnabled {
      "jwt.enabled" = true;
    }
    // pkgs.lib.optionalAttrs envoyGatewayEnabled {
      # Renders dashboard-gateway*.yaml (Gateway/GRPCRoute/EnvoyProxy/
      # SecurityPolicy/ClientTrafficPolicy/BackendTLSPolicy). These CRs
      # sit in 02-workloads.yaml but don't match any 01-rbac kind, so
      # they land in the workloads split correctly.
      "dashboard.enabled" = true;
    }
    // extraValuesTyped;
    namespace = ns;
  };

  # ── mTLS PKI (replaces cert-manager) ─────────────────────────────────
  # openssl-generated root CA + per-component leaf certs with k8s
  # Service-name SANs. Rendered into k8s Secrets so tls.enabled=true
  # works without cert-manager (which isn't in the airgap set —
  # phase4c). Makes the fixture representative of production: mTLS
  # on all gRPC links, plaintext health ports (9101/9102/9194).
  #
  # With envoyGatewayEnabled, also generates rio-dashboard-envoy (the
  # envoy data-plane's client-cert for upstream mTLS to the scheduler).
  # dashboard-gateway-tls.yaml's EnvoyProxy.spec.backendTLS.
  # clientCertificateRef references this Secret.
  pkiK8s = mkPkiK8s {
    inherit ns;
    extraComponents = pkgs.lib.optional envoyGatewayEnabled "dashboard-envoy";
  };

  # ── Airgap image set ────────────────────────────────────────────────
  # Same list on BOTH nodes — pods land on either via scheduler whims
  # (especially scheduler.replicas=2 antiAffinity). fod-proxy/bootstrap
  # excluded (disabled in vmtest-full.yaml).
  #
  # `all` replaces the five per-component images (gateway/scheduler/
  # store/controller/worker): they share the same rio-workspace
  # closure and differed only in Entrypoint. k3s imports serially
  # alphabetically before kubelet — one tarball decompress instead
  # of five. vmtest-full.yaml sets `command:` per pod.
  rioImages = [
    dockerImages.all
    pulled.bitnami-postgresql
  ]
  ++ pkgs.lib.optionals envoyGatewayEnabled [
    # Operator + certgen Job (same image). ~120MB compressed.
    pulled.envoy-gateway
    # Data-plane envoy. Pinned via EnvoyProxy.spec.provider.kubernetes.
    # envoyDeployment.container.image in dashboard-gateway-tls.yaml —
    # matches extraSet bare-tag override above (destNameTag — values.
    # yaml default is digest-pinned but airgap containerd needs tag-only).
    pulled.envoy-distroless
  ]
  # nginx + SPA bundle (rio-dashboard:dev). dashboard.enabled=true
  # (set when envoyGatewayEnabled) renders the nginx Deployment;
  # without this preload the pod goes ImagePullBackOff. The `?`
  # guard: coverage-mode dockerImages elides the dashboard attr
  # (nginx+static has no LLVM instrumentation — mkDockerImages
  # passes rioDashboard=null → docker.nix optionalAttrs drops it).
  # In coverage mode the nginx pod still backoffs, but only the
  # dashboard-curl scenario waits for it — and that scenario is
  # itself gated on the same `?` check (default.nix), so coverage-
  # mode vmTestsCov skips it entirely.
  ++ pkgs.lib.optional (envoyGatewayEnabled && dockerImages ? dashboard) dockerImages.dashboard
  ++ extraImages;

  # ── Containerd tmpfs sizing ──────────────────────────────────────────
  # Decompressed airgap layers: ~1.5GB normal, ~2.5GB cov-mode (the
  # instrumented rio-* images are ~3-4× larger). The tmpfs cap is a
  # hard ceiling — if containerd exceeds it, imports ENOSPC. Worse:
  # kubelet's imagefs.available hard-eviction threshold is 5% — at
  # 95%+ it EVICTS pods, and evicted pod carcasses linger in `kubectl
  # get pods` (status.phase=Failed reason=Evicted) breaking any wait
  # that polls for pods-gone. Observed: rio-fod-proxy.tar.zst (squid
  # + cyrus-sasl + openldap + ~15 deps) decompresses to ~800MB, pushed
  # the 3G tmpfs to 90%+ → kubelet evicted rio-controller then
  # rio-gateway mid-waitReady.
  #
  # extraImages bump: +1G tmpfs + +1G RAM per extra image. Generous
  # (most images won't be 800MB) but eviction recovery is a nightmare
  # to debug — tmpfs is cheap insurance. The VM memory bump must cover
  # what's ACTUALLY written (tmpfs is lazy; unused cap costs nothing).
  #
  # envoyGatewayEnabled adds 2 images (envoy ~200MB + dashboard ~100MB
  # if dockerImages?dashboard — 2 bump units when both present, 1 if
  # dashboard absent). Dashboard fits in envoy's headroom today but
  # counting it explicitly avoids a future uncounted-growth surprise.
  extraImagesBumpGiB =
    builtins.length extraImages
    + (if envoyGatewayEnabled then (if dockerImages ? dashboard then 2 else 1) else 0);
  containerdTmpfsSize =
    if coverage then
      "${toString (4 + extraImagesBumpGiB)}G"
    else
      "${toString (3 + extraImagesBumpGiB)}G";
  # +2048 over the old baseline (6144/4096) covers the tmpfs-resident
  # layers at their normal-mode size (~1.5GB) with headroom. Coverage
  # adds another +2048 for the instrumented-image delta (~1GB extra).
  # extraImages adds +1024/image (matching the tmpfs +1G/image above).
  k3sCovMemBump = (if coverage then 2048 else 0) + (extraImagesBumpGiB * 1024);

  # ── Base config shared by server + agent ────────────────────────────
  # Everything except services.k3s (which differs by role). Extracted
  # so the firewall/kernel/systemd bits don't duplicate. Flannel VXLAN
  # needs 8472/udp both ways; kubelet 10250 for `kubectl exec`/logs
  # (agent → server AND server → agent for 2-node).
  k3sBase = {
    services.k3s.package = k3sPinned;
    swapDevices = [ ];
    boot.kernelModules = [ "fuse" ];
    boot.kernelParams = [ "systemd.unified_cgroup_hierarchy=1" ];

    networking.firewall = {
      allowedTCPPorts = [
        6443 # apiserver (server only, but opening on agent is a no-op)
        10250 # kubelet (kubectl exec/logs)
        32222 # gateway NodePort — kube-proxy listens on every node
      ];
      allowedUDPPorts = [ 8472 ]; # flannel VXLAN
    };

    # containerd needs cgroup delegation for pod cgroups. Without:
    # ContainerCreating forever.
    systemd.services.k3s.serviceConfig.Delegate = "yes";

    # Drop the 1Hz retry spam during the ~180s airgap-import window
    # (apiserver up, kubelet blocked on serial image import → node not
    # yet registered). 182 lines/run pre-filter. "~" prefix = exclude
    # matching from journal ingestion (systemd 253+) — never reaches
    # console→serial→testlog. Other k3s logs unaffected.
    systemd.services.k3s.serviceConfig.LogFilterPatterns = "~Unable to set control-plane role label";

    # ── Containerd image store on tmpfs ────────────────────────────────
    # Eliminates builder-disk variance for airgap imports. Before:
    # containerd writes decompressed layers to ext4→qcow2→builder-disk;
    # cache=writeback helps but fsync still hits host fdatasync. Observed
    # 3.3-5× tail variance on the SAME drv (076de36 commit msg: bitnami
    # 29.5s vs 97s, rio-gateway 37s vs 130s — I/O-bound, ~10-12% CPU).
    # With tmpfs, writes are RAM-to-RAM; variance collapses to 9p-read +
    # decompress (CPU-bound, much tighter distribution).
    #
    # NOT full diskImage=null: PG's PVC (/var/lib/rancher/k3s/storage,
    # local-path-provisioner) stays on qcow2 — PG data growth in tmpfs
    # could OOM the VM on a long-running test. Only the containerd image
    # store (write-once, size-bounded by the airgap set) goes to RAM.
    #
    # neededForBoot=true: stage-1 initrd mkdir -p's the mount point
    # before mounting. The parent path /var/lib/rancher/k3s/agent
    # doesn't exist until k3s creates it — stage-1 ordering avoids
    # the chicken-and-egg.
    #
    # virtualisation.fileSystems (not plain fileSystems): qemu-vm.nix
    # does `fileSystems = mkVMOverride virtualisation.fileSystems`
    # (priority 10) — a plain `fileSystems.*` def is silently dropped.
    virtualisation.fileSystems."/var/lib/rancher/k3s/agent/containerd" = {
      fsType = "tmpfs";
      neededForBoot = true;
      options = [
        "size=${containerdTmpfsSize}"
        "mode=0755"
      ];
    };

    environment.systemPackages = [
      pkgs.curl
      pkgs.kubectl
      pkgs.grpc-health-probe # health-shared probe (lifecycle.nix)
    ];
  };

  # ── Server node ─────────────────────────────────────────────────────
  serverNode =
    { config, ... }:
    {
      imports = [ k3sBase ];
      networking.hostName = "k3s-server";

      services.k3s = {
        enable = true;
        role = "server";
        clusterInit = true;
        inherit tokenFile;
        images = [ config.services.k3s.package.airgap-images ] ++ rioImages;
        manifests = {
          "00-rio-crds".source = "${helmRendered}/00-crds.yaml";
          "01-rio-rbac".source = "${helmRendered}/01-rbac.yaml";
          "02-rio-workloads".source = "${helmRendered}/02-workloads.yaml";
        }
        // pkgs.lib.optionalAttrs envoyGatewayEnabled {
          # Envoy Gateway operator + Gateway API CRDs. Alphabetical
          # sort order (00-envoy-gateway-* < 00-rio-crds, `e` < `r`)
          # means these apply first — the rio chart's dashboard-
          # gateway*.yaml CRs in 02-rio-workloads need
          # gateway.networking.k8s.io + gateway.envoyproxy.io CRDs
          # to exist.
          "00-envoy-gateway-crds".source = "${envoyGatewayRender}/00-envoy-gateway-crds.yaml";
          "01-envoy-gateway-ns".source = "${envoyGatewayRender}/01-envoy-gateway-ns.yaml";
          "01-envoy-gateway-rbac".source = "${envoyGatewayRender}/02-envoy-gateway-rbac.yaml";
          "02-envoy-gateway".source = "${envoyGatewayRender}/03-envoy-gateway.yaml";
        }
        // {
          # Placeholder authorized_keys Secret so the gateway pod's
          # volume mount resolves (no unbound Secret → Pending) AND
          # load_authorized_keys() parses ≥1 key (empty file →
          # anyhow::bail! → process exit → CrashLoopBackOff). The
          # empty-string approach worked but cost ~2 crashed containers
          # per test before sshKeySetup runs: each crash = gRPC retry
          # loop connects to store+scheduler (~1s CPU, ~8K IP traffic),
          # then bails at server.rs:89, then kubelet's 10s backoff. On
          # coverage builds, each crash also flushes ~6M of profraw.
          #
          # This key authorizes nothing — its private half was discarded
          # at generation time. sshKeySetup patches with the client's
          # real key + rollout-restarts before any SSH connect happens.
          "03-gateway-ssh-placeholder".source = pkgs.writeText "gateway-ssh.yaml" ''
            apiVersion: v1
            kind: Secret
            metadata:
              name: rio-gateway-ssh
              namespace: ${ns}
            stringData:
              authorized_keys: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICOWXl9/32g/wAtRqYblAdI7wmPNL6phTBMlkn2o6psr placeholder-unused-vmtest"
          '';
          # mTLS certs. Applied before 02-workloads so pods' TLS
          # Secret volume mounts resolve on first start (no Pending
          # on unbound Secret). See lib/pki-k8s.nix.
          "01-rio-tls-secrets".source = pkiK8s.secretsManifest;
        };
        extraFlags = [
          "--flannel-iface"
          "eth1"
          "--disable"
          "traefik"
          "--disable"
          "metrics-server"
          # local-path-provisioner KEPT (no --disable local-storage) —
          # bitnami PG's PVC binds against it.
          "--tls-san"
          "k3s-server"
          # Dual-stack (P0542): the chart defaults to PreferDualStack +
          # ipFamilies=[IPv6,IPv4] on Services. k3s must have a v6
          # service CIDR or the apiserver assigns v4-only and the
          # production EKS path (ip_family=ipv6) goes unexercised. The
          # NixOS test driver auto-assigns 2001:db8:${vlan}::N/64 to
          # eth1 and exposes it as primaryIPv6Address; node-ip lists v4
          # first to keep it the primary family (kubelet/hostNetwork
          # binds, kube-proxy NodePort) so existing v4-only assertions
          # in scenarios stay valid.
          "--cluster-cidr"
          "10.42.0.0/16,2001:db8:42::/56"
          "--service-cidr"
          "10.43.0.0/16,2001:db8:43::/112"
          "--flannel-ipv6-masq"
          "--node-ip"
          "${config.networking.primaryIPAddress},${config.networking.primaryIPv6Address}"
          # Quiet the "Failed to record snapshots: nodes not found"
          # spam during startup. The etcd snapshot reconciler fires
          # on a tight loop before the kubelet registers the node
          # (IO-starved by the airgap image import). We don't use
          # etcd snapshots in an ephemeral VM test.
          "--etcd-disable-snapshots"
        ];
      };

      # 8GB (was 6GB): PG (512Mi) + 5 rio pods (~2GB) + k3s control
      # plane (~1.5GB) + containerd tmpfs (~1.5GB layers, 3G cap) +
      # headroom. Coverage: +2GB for instrumented-image bloat.
      virtualisation = {
        memorySize = 8192 + k3sCovMemBump;
        cores = 8;
        diskSize = 16384;
      };
    };

  # ── Agent node ──────────────────────────────────────────────────────
  agentNode =
    { config, nodes, ... }:
    {
      imports = [ k3sBase ];
      networking.hostName = "k3s-agent";

      services.k3s = {
        enable = true;
        role = "agent";
        inherit tokenFile;
        serverAddr = "https://${nodes.k3s-server.networking.primaryIPAddress}:6443";
        # Agent loads images into its OWN containerd. Pods scheduled
        # here need local images — this is where the second scheduler
        # replica (antiAffinity) + maybe workers land.
        images = [ config.services.k3s.package.airgap-images ] ++ rioImages;
        extraFlags = [
          "--flannel-iface"
          "eth1"
          "--node-ip"
          "${config.networking.primaryIPAddress},${config.networking.primaryIPv6Address}"
        ];
      };

      # 6GB (was 4GB): scheduler replica (~512Mi) + worker (~1.5Gi
      # with FUSE cache) + containerd tmpfs (~1.5GB layers, 3G cap)
      # + k3s agent (~500Mi). Coverage: +2GB for instrumented images.
      virtualisation = {
        memorySize = 6144 + k3sCovMemBump;
        cores = 8;
        diskSize = 12288;
      };
    };

  ns = "rio-system";
  # ADR-019 four-namespace split. Scenarios that touch the store
  # Deployment or builder/fetcher STS use the per-role bindings; `ns`
  # stays as the control-plane default for scheduler/gateway/controller.
  nsStore = "rio-store";
  nsBuilders = "rio-builders";
  nsFetchers = "rio-fetchers";

in
rec {
  # Exposed for testScript: `k3s-server.succeed("k3s kubectl -n ${fixture.ns} ...")`.
  inherit
    ns
    nsStore
    nsBuilders
    nsFetchers
    helmRendered
    ;
  # For grpcurl client cert args (scenarios/lifecycle.nix sched_grpc
  # etc). The controller cert works as a generic mTLS client — rio
  # doesn't check CN, only that the cert chains to the shared CA.
  inherit (pkiK8s) pki;

  nodes = {
    k3s-server = serverNode;
    k3s-agent = agentNode;
    client = common.mkClientNode {
      gatewayHost = "k3s-server";
      gatewayPort = 32222;
      # Chart's gateway pod runs as non-root; the SSH server inside
      # accepts any user (auth is pubkey), but ssh-ng defaults to the
      # local username. Match what the chart expects.
      gatewayUser = "rio";
    };
  };

  # ── testScript snippets ─────────────────────────────────────────────

  # Shell helper: scenarios do a LOT of kubectl. Prepend this to
  # testScript; then use `kubectl("get pods")` in Python. ADR-019:
  # `ns=` kwarg for store/builder resources that moved out of
  # rio-system; defaults to rio-system so existing control-plane
  # calls (scheduler/gateway/controller/lease) are untouched.
  kubectlHelpers = ''
    def kubectl(args, node=k3s_server, ns="${ns}"):
        return node.succeed(f"k3s kubectl -n {ns} {args}")

    def leader_pod():
        """Find scheduler leader via Lease holderIdentity. Same pattern
        as smoke-test.sh sched_leader()."""
        return kubectl(
            "get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.holderIdentity}'"
        ).strip()
  '';

  # Full bring-up: ~3-4min wall. Heavy ordering dependency chain —
  # each step gated on the previous.
  waitReady = ''
    # ── Both k3s units running ──────────────────────────────────────
    k3s_server.wait_for_unit("k3s.service")
    k3s_agent.wait_for_unit("k3s.service")
    k3s_server.wait_for_file("/etc/rancher/k3s/k3s.yaml")

    # ── Airgap import complete on BOTH nodes (BEFORE agent-ready) ───
    # k3s imports airgap images SERIALLY (alphabetically) via a
    # goroutine that runs BEFORE kubelet starts. Under TCG (non-KVM
    # fallback): system bundle (pause/CNI/bitnami) first, then rio-*
    # bundle. bitnami is NOT last — rio-all (~170MB) comes after.
    # Gate on pause (minimal kubelet pod infra) + rio-all (the one
    # rio image, replaces the former 5-image bundle).
    # timeout=240 post containerd-tmpfs fix (24c8537). Pre-tmpfs, agent
    # rio-controller import hit 170s vs 35-40s typical (5× builder-disk
    # tail). Tmpfs collapses that to CPU-bound decompress.
    for n in [k3s_server, k3s_agent]:
        n.wait_until_succeeds(
            "k3s ctr images ls -q | grep -q pause", timeout=240
        )
        n.wait_until_succeeds(
            "k3s ctr images ls -q | grep -q 'bitnami/postgresql'", timeout=240
        )
        n.wait_until_succeeds(
            "k3s ctr images ls -q | grep -q 'rio-all'", timeout=600
        )

    # ── Server node registered (kubelet up, images imported) ────────
    # Agent cannot join until the server's kubelet has registered the
    # server node with the apiserver. Pre-fix, agent-Ready absorbed
    # ~107s of rio-* import time in its 120s budget (successful run:
    # 106.70/120s = 89% burned; 1.8× slower builder blew it by ~72s).
    # This gate directly tests the actual precondition. EXISTS (not
    # Ready) is sufficient: Ready needs CNI which needs flannel which
    # needs the node to exist first.
    # TODO(P0304): timeout=600 predates containerd-tmpfs (same story as
    # the rio-all gate above). ~180s typical under TCG; reduce to 300
    # once tmpfs is verified to have collapsed the builder-disk tail.
    k3s_server.wait_until_succeeds(
        "k3s kubectl get node k3s-server 2>/dev/null",
        timeout=600,
    )

    # ── Flannel CNI ready on BOTH nodes ─────────────────────────────
    # k3s auto-applies manifests as soon as the apiserver is up —
    # INDEPENDENTLY of CNI readiness. Pods scheduled on a node before
    # flannel writes /run/flannel/subnet.env fail CreatePodSandbox
    # with "loadFlannelSubnetEnv failed: no such file or directory".
    # Kubelet backoff (exponential, up to 5m) then delays recovery
    # past the 150s worker-Ready timeout. Observed: pos800 run3
    # dashboard-gateway — ALL pods failed sandbox at t=32-40s, worker
    # never recovered in 150s. Gate on flannel-subnet-written directly
    # on both nodes BEFORE proceeding to pod-level waits.
    for _cni_node in [k3s_server, k3s_agent]:
        _cni_node.wait_until_succeeds(
            "test -f /run/flannel/subnet.env",
            timeout=120,
        )

    # ── Agent joined ────────────────────────────────────────────────
    # With server registered, agent-Ready now measures ONLY the
    # agent's own kubelet start + CNI bring-up (~30-60s under TCG,
    # not the 100+s of hidden import it previously absorbed).
    k3s_server.wait_until_succeeds(
        "k3s kubectl get node k3s-agent "
        "-o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}' "
        "| grep -qx True",
        timeout=120,
    )

    # ── PG Ready (everything else blocks on migrations) ─────────────
    # Bitnami's sts name pattern: <release>-postgresql. Our release
    # name is `rio` (helm template's first arg). PVC binds via local-
    # path-provisioner → initdb runs → readiness probe (pg_isready).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} wait --for=condition=Ready "
        "pod/rio-postgresql-0 --timeout=180s",
        timeout=200,
    )

    # ── rio deployments Available ───────────────────────────────────
    # store + scheduler crash-loop until PG is up (sqlx migrate retry),
    # then come clean. controller just needs apiserver.
    #
    # Gateway NOT waited here: 03-gateway-ssh-placeholder seeds a
    # throwaway key (private half discarded, authorizes nothing) so
    # the pod starts cleanly instead of CrashLooping on empty. But the
    # pod is still useless until sshKeySetup patches in the client's
    # real key + rollout-restarts — that runs AFTER waitReady in every
    # scenario and does its own rollout status wait.
    # ADR-019: store moved to rio-store namespace. Scheduler+controller
    # stay in rio-system.
    for d, dns in [
        ("rio-store", "${nsStore}"),
        ("rio-scheduler", "${ns}"),
        ("rio-controller", "${ns}"),
    ]:
        k3s_server.wait_until_succeeds(
            f"k3s kubectl -n {dns} wait --for=condition=Available "
            f"deploy/{d} --timeout=120s",
            timeout=150,
        )

    # ── Leader elected ──────────────────────────────────────────────
    # Scheduler deployment Available means BOTH replicas Ready, but
    # the Lease may not have a holder yet (acquire is on a 5s tick).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
        "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
        timeout=30,
    )

    # ── Worker STS exists ───────────────────────────────────────────
    # `kubectl wait pod/X` fails IMMEDIATELY (rc=1, "NotFound") if the
    # pod doesn't exist — it does NOT block for --timeout. Controller
    # became Available above, but its first BuilderPool reconcile
    # (watch establish + SSA the STS + STS controller creates pod-0)
    # is still ~1-2s away. impl-39 hit this race: store CrashLoopBackOff
    # × 2 + extra migrations 028/029 pushed controller-Available to
    # 14.69s (vs 0.19s typical), and the immediate NotFound at line ~640
    # below was misread as "controller hung 270s". Gate on the STS
    # existing first — short poll, the actual reconcile is fast once
    # the controller's connect+watch loop is past startup jitter.
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${nsBuilders} get sts rio-builder-x86_64",
        timeout=60,
    )

    # ── Worker pod Ready ────────────────────────────────────────────
    # workerPool.replicas.min=1 in vmtest-full.yaml → controller
    # scales STS to 1 immediately. vmtest-full.yaml uses the
    # privileged:true escape hatch (hostPath /dev/fuse) — the node's
    # /dev/fuse must exist (boot.kernelModules = ["fuse"] above).
    # The default valuesFile (vmtest-full.yaml) uses privileged:true
    # (fast path — skips device-plugin DS bring-up ~30-60s).
    # vmtest-full-nonpriv.yaml exercises the production ADR-012 path via
    # smarter-device-manager (see security.nix:privileged-hardening-e2e).
    #
    # On timeout: dump logs + describe + device-plugin state before
    # re-raising. The nonpriv path (vm-security-nonpriv-k3s) has
    # several novel failure modes this exposes: device-plugin socket
    # path mismatch (k3s kubelet is at /var/lib/rancher/k3s/agent/
    # kubelet, not /var/lib/kubelet → DS registers nowhere → pod
    # Pending on Insufficient smarter-devices/fuse), cgroup remount
    # EPERM under hostUsers:false, FUSE mount fail if device
    # injection didn't happen. Without this dump, the test just
    # times out at 180s with no signal.
    # Single kubectl wait (not wait_until_succeeds retry loop): the
    # inner --timeout=270s already retries internally. A second
    # wait_until_succeeds retry would block another 270s, pushing
    # the diagnostic dump past most CI outer-timeouts. 270s (was
    # 150s) gives headroom for the nonpriv DS bring-up: smarter-
    # device-manager DaemonSet must schedule + go Ready + register
    # the `smarter-devices/fuse` extended resource BEFORE the
    # worker pod can leave Pending (~30-60s extra under TCG).
    rc, _ = k3s_server.execute(
        "k3s kubectl -n ${nsBuilders} wait --for=condition=Ready "
        "pod/rio-builder-x86_64-0 --timeout=270s"
    )
    if rc != 0:
        print("=== worker-Ready TIMEOUT: diagnostic dump ===")
        # Pod describe: shows Pending reason (Unschedulable →
        # insufficient resource) OR CrashLoopBackOff + last-state
        # exit code + events.
        print(k3s_server.execute(
            "k3s kubectl -n ${nsBuilders} describe pod rio-builder-x86_64-0 2>&1"
        )[1])
        # Previous container logs: the crash stderr. --previous
        # because current container may be in backoff (no logs yet).
        # Fall through to current if --previous fails (first crash,
        # no previous container).
        print("--- kubectl logs --previous ---")
        print(k3s_server.execute(
            "k3s kubectl -n ${nsBuilders} logs rio-builder-x86_64-0 --previous 2>&1 "
            "|| k3s kubectl -n ${nsBuilders} logs rio-builder-x86_64-0 2>&1"
        )[1])
        # Device-plugin state: DS rollout + node allocatable. If
        # allocatable.smarter-devices/fuse is absent/0, the DS
        # registered against the wrong kubelet socket (k3s path
        # mismatch) or isn't Ready yet.
        print("--- device-plugin DS + node allocatable ---")
        print(k3s_server.execute(
            "set +e; "
            "k3s kubectl -n ${nsBuilders} get ds rio-device-plugin -o wide 2>&1; "
            "k3s kubectl -n ${nsBuilders} logs ds/rio-device-plugin --tail=50 2>&1; "
            "k3s kubectl get nodes "
            "-o jsonpath='{range .items[*]}{.metadata.name}: "
            "{.status.allocatable.smarter-devices/fuse}{\"\\n\"}{end}' 2>&1; "
            "true"
        )[1])
        # STS + controller state: if the pod was NEVER created (NotFound
        # above), the STS describe shows the admission reject reason
        # (procMount:Unmasked → feature gate, PodSecurity, hostUsers
        # interaction) or controller apply error. If the STS itself
        # is NotFound, the BuilderPool CR may have been rejected at
        # apply time (CRD CEL validation) — dump k3s addon events.
        #
        # set +e: nixos-test-driver wraps execute() in `set -euo
        # pipefail` — without this, the first NotFound aborts the
        # chain and the rest never prints (observed: only the STS
        # error made it out when the BuilderPool CR was CEL-rejected).
        print("--- STS describe + controller logs ---")
        print(k3s_server.execute(
            "set +e; "
            "k3s kubectl -n ${nsBuilders} describe sts rio-builder-x86_64 2>&1; "
            "echo '--- BuilderPool status ---'; "
            "k3s kubectl -n ${nsBuilders} get builderpool x86_64 -o yaml 2>&1; "
            "echo '--- k3s addon events (manifest apply) ---'; "
            "k3s kubectl -n kube-system get events "
            "--field-selector involvedObject.kind=Addon "
            "-o custom-columns=REASON:.reason,MSG:.message 2>&1; "
            # rio_controller=debug means tail=30 is 30 lines of h2/tower
            # noise from a ~10ms window. tail=2000 + grep keeps the
            # actual reconcile flow visible without flooding the test
            # log. store/scheduler logs: a store CrashLoop blocks
            # scheduler from SERVING which blocks controller's connect
            # loop (impl-39: builder-ready timed out, dump showed only
            # h2 noise — store crash invisible).
            "echo '--- controller logs (rio_controller-only) ---'; "
            "k3s kubectl -n ${ns} logs deploy/rio-controller --tail=2000 2>&1 "
            "  | grep -E '\"target\":\"rio_' || true; "
            "echo '--- scheduler logs (ERRORs+state) ---'; "
            "k3s kubectl -n ${ns} logs deploy/rio-scheduler --tail=200 2>&1 "
            "  | grep -E 'ERROR|WARN|migrat|leader|SERVING' || true; "
            "echo '--- store logs (ERRORs+state) ---'; "
            "k3s kubectl -n ${nsStore} logs deploy/rio-store --tail=100 --previous 2>&1; "
            "k3s kubectl -n ${nsStore} logs deploy/rio-store --tail=100 2>&1 "
            "  | grep -E 'ERROR|WARN|migrat|SERVING' || true; "
            "echo '--- pod overview ---'; "
            "k3s kubectl get pods -A 2>&1; "
            "true"
        )[1])
        raise Exception("rio-builder-x86_64-0 not Ready after 270s (see dump above)")

    # ── Worker registered at scheduler ───────────────────────────────
    # Scheduler pods have no shell (minimal image). Scrape via the
    # apiserver's pods/proxy subresource — no local port-forward,
    # no TIME_WAIT churn, no `sleep 2` bind wait.
    #
    # NUMERIC port (9091), not named (`:metrics`): k3s apiserver
    # PANICS (nil-deref in normalizeLocation, upgradeaware.go:173)
    # when named-port resolution fails. Observed v20.
    k3s_server.wait_until_succeeds(
        "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
        "  -o jsonpath='{.spec.holderIdentity}') && "
        'test -n "$leader" && '
        "k3s kubectl get --raw "
        '"/api/v1/namespaces/${ns}/pods/$leader:9091/proxy/metrics" '
        "| grep -qx 'rio_scheduler_workers_active 1'",
        timeout=60,
    )
  '';

  # Scale gateway to 0 → wait for full pod deletion → scale to 1.
  # Necessary after updating the rio-gateway-ssh Secret: gateway reads
  # authorized_keys once at startup (Arc<Vec<PublicKey>>, no hot-reload).
  #
  # rollout restart is NOT sufficient. Kubelet's SecretManager (Watch
  # mode, default since k8s 1.12) caches Secrets via a per-object
  # reflector, refcounted by mounting pods. The OLD gateway pod's
  # volume mount started a reflector; rollout restart creates the new
  # pod ~400ms after the apply; kubelet serves from the SAME cached
  # reflector, which may not have received the watch MODIFIED event yet
  # → new pod mounts the STALE key. Scale-to-zero forces reflector
  # refcount to 0 → cache evict → fresh LIST on the new pod.
  bounceGatewayForSecret = ''
    k3s_server.succeed(
        "k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=0"
    )
    # Terminating pods still show up in `get pods` until kubelet's final
    # grace-0 DELETE. `! ... | grep -q .` succeeds when output is empty.
    k3s_server.wait_until_succeeds(
        "! k3s kubectl -n ${ns} get pods "
        "-l app.kubernetes.io/name=rio-gateway "
        "--no-headers 2>/dev/null | grep -q .",
        timeout=90,
    )
    k3s_server.succeed(
        "k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=1"
    )
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} rollout status deploy/rio-gateway --timeout=60s",
        timeout=90,
    )
  '';

  # SSH key → k8s Secret for gateway. Replaces common.sshKeySetup
  # (which writes to /var/lib/rio/gateway/authorized_keys on a systemd
  # host). The 03-gateway-ssh-placeholder manifest above pre-creates
  # the Secret with a throwaway key so the pod starts cleanly during
  # waitReady; this patches in the real key + scale-bounces (0→1) to
  # force a fresh kubelet Secret LIST (see bounceGatewayForSecret).
  sshKeySetup = ''
    client.succeed(
        "mkdir -p /root/.ssh && "
        "ssh-keygen -t ed25519 -N ''' -C ''' -f /root/.ssh/id_ed25519"
    )
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    # Create-or-replace. --dry-run=client -o yaml | apply is idempotent.
    k3s_server.succeed(
        "k3s kubectl -n ${ns} create secret generic rio-gateway-ssh "
        f"--from-literal=authorized_keys='{pubkey}' "
        "--dry-run=client -o yaml | k3s kubectl apply -f -"
    )
    # Gateway must restart with the FRESH Secret — see
    # bounceGatewayForSecret for why rollout restart is NOT sufficient.
    ${bounceGatewayForSecret}
    # rollout status returns when the Deployment has Ready replicas,
    # but kube-proxy hasn't necessarily synced the endpoint to the
    # NodePort's iptables rules yet. Poll the SSH banner from the
    # client.
    #
    # SSH banner (not nc -z): nc -z only proves kube-proxy has an
    # iptables rule that DNATs to SOMETHING — not that the gateway's
    # SSH accept loop is end-to-end ready. The gateway's readinessProbe
    # is on gRPC port 9190 (main.rs:349), but the SSH listener binds
    # LATER at server.run() (main.rs:425) after host-key + authz-key +
    # JWT-seed file I/O. Under KVM, pod-Ready → kube-proxy-synced can
    # outrun that gap: nc -z succeeds (iptables rule exists), then ssh
    # gets RST (listener not bound yet, or kube-proxy mid-sync flap
    # during the scale 0→1 endpoint churn). Observed: vm-lifecycle-wps
    # — nc succeeded in 0.11s, immediate `nix copy` got Connection
    # refused.
    #
    # RFC 4253 §4.2: SSH servers MUST send `SSH-2.0-<sw>\r\n`
    # immediately on TCP connect. Grepping for ^SSH- proves the full
    # chain: kube-proxy rule → live pod IP → bound listener → russh
    # accept loop responding. `|| true` guards pipefail against nc's
    # idle-timeout exit code; grep's result drives wait_until_succeeds.
    #
    # (Not ssh/ssh-keyscan: russh only accepts the nix-ssh subsystem —
    # `ssh ... true` gets "exec request failed"; ssh-keyscan doesn't
    # complete the handshake with russh.)
    #
    # Flake-fix strategy: structural (banner check replaces port-open
    # check). Retry rejected — would hide the health-SERVING-before-
    # SSH-bound ordering. Widen rejected — no wall-clock gate here.
    client.wait_until_succeeds(
        "(${pkgs.netcat}/bin/nc -w2 k3s-server 32222 </dev/null 2>&1 "
        "|| true) | grep -q ^SSH-",
        timeout=30,
    )
  '';

  # For `${common.collectCoverage pyNodeVars}`. Client excluded (no
  # rio services → empty tarball noise).
  pyNodeVars = "k3s_server, k3s_agent";
}
