# Two-node k3s fixture running the full Helm chart.
#
# First time gateway.yaml / scheduler.yaml / store.yaml / pdb.yaml /
# rbac.yaml / postgres-secret.yaml are applied in CI. Closes the
# "production uses pod path, VM tests use systemd" gap — and the
# scheduler.replicas=2 podAntiAffinity spreads across server+agent,
# enabling the leader-election scenario.
#
# Topology: k3s-server (6GB) + k3s-agent (4GB) + client (1GB).
# ~11GB total — use withMinCpu 8 in flake.nix.
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
  # TODO(coverage): controller.extraEnv LLVM_PROFILE_FILE via --set,
  # hostPath cov volume on worker pods. Deferred — scenarios using
  # this fixture (lifecycle, leader-election) have unique coverage
  # surface (kube-rs reconcilers, lease loop) that unit tests can't
  # reach, so coverage-full eventually wants this.
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
  pulled = import ../../docker-pulled.nix { inherit pkgs; };
in
{
  # --set overrides layered on top of vmtest-full.yaml. scenarios/
  # lifecycle.nix passes autoscaler tuning (pollSecs=3 etc).
  extraValues ? { },
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
    extraSet = extraValues;
    namespace = ns;
  };

  # ── Airgap image set ────────────────────────────────────────────────
  # Same list on BOTH nodes — pods land on either via scheduler whims
  # (especially scheduler.replicas=2 antiAffinity). fod-proxy/bootstrap
  # excluded (disabled in vmtest-full.yaml).
  rioImages = [
    dockerImages.gateway
    dockerImages.scheduler
    dockerImages.store
    dockerImages.controller
    dockerImages.worker
    pulled.bitnami-postgresql
  ];

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
          # Empty authorized_keys Secret so the gateway pod's volume
          # mount resolves (no unbound Secret → Pending). sshKeySetup
          # patches this + rollout-restarts. Mirrors the NixOS module's
          # tmpfiles empty-file pattern (common.nix gatewayTmpfiles).
          "03-gateway-ssh-empty".source = pkgs.writeText "gateway-ssh.yaml" ''
            apiVersion: v1
            kind: Secret
            metadata:
              name: rio-gateway-ssh
              namespace: ${ns}
            stringData:
              authorized_keys: ""
          '';
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
        ];
      };

      # 6GB: PG (512Mi) + 5 rio pods (~2GB total) + k3s control plane
      # (~1.5GB) + images (~1GB decompressed) + headroom.
      virtualisation = {
        memorySize = 6144;
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

      # 4GB: scheduler replica (~512Mi) + worker (~1.5Gi with FUSE
      # cache) + images + k3s agent (~500Mi).
      virtualisation = {
        memorySize = 4096;
        cores = 8;
        diskSize = 12288;
      };
    };

  ns = "rio-system";

in
{
  # Exposed for testScript: `k3s-server.succeed("k3s kubectl -n ${fixture.ns} ...")`.
  inherit ns helmRendered;

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
    # ctr images import runs as a separate oneshot unit AFTER k3s
    # starts. Under TCG (nixbuild.net's non-KVM fallback), 6 images ×
    # ~20-30s each ≈ 180s just for import. The agent's kubelet can't
    # reach Ready until pause/CNI images are available, so waiting for
    # agent-Ready FIRST times out before import completes.
    # Gate on the last-to-import image (bitnami PG, largest at ~140MB).
    for n in [k3s_server, k3s_agent]:
        n.wait_until_succeeds(
            "k3s ctr images ls -q | grep -q pause", timeout=240
        )
        n.wait_until_succeeds(
            "k3s ctr images ls -q | grep -q 'bitnami/postgresql'", timeout=240
        )

    # ── Agent joined (images present → kubelet can start pods) ──────
    # Agent Ready condition means kubelet registered + CNI up. With
    # images already imported this is just kubelet startup (~30s).
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
    # then come clean. gateway needs scheduler+store healthy (balanced-
    # channel probe fails → NOT_SERVING → readiness fails). controller
    # just needs apiserver. Order reflects dependency, but `kubectl
    # wait` handles the concurrency.
    for d in ["rio-store", "rio-scheduler", "rio-gateway", "rio-controller"]:
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
    # scales STS to 1 immediately. FUSE device hostPath needs the
    # node's /dev/fuse to exist (boot.kernelModules = ["fuse"] above).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} wait --for=condition=Ready "
        "pod/default-workers-0 --timeout=150s",
        timeout=180,
    )

    # ── Worker registered at scheduler (via metrics port-forward) ───
    # Scheduler pods have no shell (minimal image); port-forward to
    # leader for metrics. Can't wait_until_succeeds across a port-
    # forward spawn easily — just do a fixed-count retry in bash.
    k3s_server.wait_until_succeeds(
        "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
        "  -o jsonpath='{.spec.holderIdentity}') && "
        "k3s kubectl -n ${ns} port-forward $leader 19091:9091 "
        "  >/dev/null 2>&1 & pf=$!; "
        "trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
        "curl -sf http://localhost:19091/metrics | "
        "grep -qx 'rio_scheduler_workers_active 1'",
        timeout=60,
    )
  '';

  # SSH key → k8s Secret for gateway. Replaces common.sshKeySetup
  # (which writes to /var/lib/rio/gateway/authorized_keys on a systemd
  # host). The 03-gateway-ssh-empty manifest above pre-creates the Secret
  # with an empty key list so the pod's volume mount resolves during
  # waitReady; this patches in the real key + rollout-restarts.
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
    # load, no hot-reload). Restart so it picks up the new Secret.
    k3s_server.succeed("k3s kubectl -n ${ns} rollout restart deploy/rio-gateway")
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${ns} rollout status deploy/rio-gateway --timeout=60s",
        timeout=90,
    )
  '';

  # For `${common.collectCoverage pyNodeVars}`. Client excluded (no
  # rio services → empty tarball noise).
  pyNodeVars = "k3s_server, k3s_agent";
}
