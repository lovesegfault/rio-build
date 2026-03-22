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
    extraSet =
      (pkgs.lib.optionalAttrs jwtEnabled {
        "jwt.publicKey" = jwtKeys.pubkeyB64;
        "jwt.signingSeed" = jwtKeys.seedB64;
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
    };
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
    # matches values.yaml dashboard.envoyImage default (v1.37.1).
    pulled.envoy-distroless
  ]
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
  # envoyGatewayEnabled adds 2 images (~200MB decompressed total) —
  # counted as 1 bump unit (they're smaller than the squid stack).
  extraImagesBumpGiB = builtins.length extraImages + (if envoyGatewayEnabled then 1 else 0);
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
          "--node-ip"
          config.networking.primaryIPAddress
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
          config.networking.primaryIPAddress
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

in
{
  # Exposed for testScript: `k3s-server.succeed("k3s kubectl -n ${fixture.ns} ...")`.
  inherit ns helmRendered;
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
  # testScript; then use `kubectl("get pods")` in Python.
  kubectlHelpers = ''
    def kubectl(args, node=k3s_server):
        return node.succeed(f"k3s kubectl -n ${ns} {args}")

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
    # TODO: timeout=600 predates the containerd-tmpfs fix (24c8537).
    # Pre-tmpfs, agent rio-controller import hit 170s vs 35-40s typical
    # (5× builder-disk tail). Tmpfs collapses that to CPU-bound
    # decompress — once verified green, 300 (or 180) should suffice.
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
    # TODO: timeout=600 predates containerd-tmpfs (same story as the
    # rio-all gate above). ~180s typical under TCG; reduce to 300 once
    # tmpfs is verified to have collapsed the builder-disk tail.
    k3s_server.wait_until_succeeds(
        "k3s kubectl get node k3s-server 2>/dev/null",
        timeout=600,
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
    for d in ["rio-store", "rio-scheduler", "rio-controller"]:
        k3s_server.wait_until_succeeds(
            f"k3s kubectl -n ${ns} wait --for=condition=Available "
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

    # ── Worker pod Ready ────────────────────────────────────────────
    # workerPool.replicas.min=1 in vmtest-full.yaml → controller
    # scales STS to 1 immediately. vmtest-full.yaml uses the
    # privileged:true escape hatch (hostPath /dev/fuse) — the node's
    # /dev/fuse must exist (boot.kernelModules = ["fuse"] above).
    # Production uses the device-plugin path (ADR-012); that's not
    # wired here (smarter-device-manager image not in airgap set).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} wait --for=condition=Ready "
        "pod/default-workers-0 --timeout=300s",
        timeout=360,
    )

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

  # SSH key → k8s Secret for gateway. Replaces common.sshKeySetup
  # (which writes to /var/lib/rio/gateway/authorized_keys on a systemd
  # host). The 03-gateway-ssh-placeholder manifest above pre-creates
  # the Secret with a throwaway key so the pod starts cleanly during
  # waitReady; this patches in the real key + scale-bounces (0→1) to
  # force a fresh kubelet Secret LIST (see comment in the body).
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
    # Gateway reads authorized_keys once at startup (Arc<Vec<PublicKey>>
    # load, no hot-reload) — the pod must restart with the FRESH Secret.
    #
    # rollout restart is NOT sufficient. Kubelet's SecretManager (Watch
    # mode, default since k8s 1.12) caches Secrets via a per-object
    # reflector, refcounted by mounting pods. The OLD gateway pod's
    # volume mount started a reflector at waitReady time (~45s ago),
    # so the placeholder key is cached. rollout restart creates the new
    # pod ~400ms after the apply above; kubelet's VerifyControllerAttachedVolume
    # serves from the SAME cached reflector, which may not have received
    # the watch MODIFIED event yet → new pod mounts the STALE placeholder
    # and starts cleanly (a valid key, no CrashLoop — unlike the old
    # empty-Secret approach where the stale mount self-healed via crash
    # → kubelet backoff → retry with fresh cache). Observed failure:
    # publickey denied, gateway pod Running 1s old with placeholder key,
    # apply-to-mount gap 420ms.
    #
    # Scale to 0 → wait for all gateway pods DELETED from apiserver
    # (kubelet only ack-deletes after full teardown: UnregisterPod →
    # SecretManager refcount-- → hits 0 → reflector.stop() → cache
    # evict) → scale back to 1. The fresh pod's RegisterPod triggers
    # a new reflector with a fresh LIST → mounts the real key.
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
    # rollout status returns when the Deployment has Ready replicas,
    # but kube-proxy hasn't necessarily synced the endpoint to the
    # NodePort's iptables rules yet. Poll TCP accept from the client.
    # nc -z (not ssh/ssh-keyscan): the gateway's russh server only
    # accepts the nix-ssh subsystem — `ssh ... true` gets "exec
    # request failed", and ssh-keyscan doesn't handshake with russh.
    # TCP accept is sufficient: rollout status already proved the
    # gateway's readinessProbe passed (gRPC health SERVING on 9190),
    # so the process is serving; we just need kube-proxy to catch up.
    client.wait_until_succeeds(
        "${pkgs.netcat}/bin/nc -zw2 k3s-server 32222",
        timeout=30,
    )
  '';

  # For `${common.collectCoverage pyNodeVars}`. Client excluded (no
  # rio services → empty tarball noise).
  pyNodeVars = "k3s_server, k3s_agent";
}
