# kwok-only fixture: bare kube-apiserver + etcd + KCM + kube-scheduler
# (nixpkgs `services.kubernetes`, easyCerts, NO kubelet/proxy/flannel/
# containerd) + KWOK fake-kubelet + the full rio control-plane
# (postgres + store + scheduler + gateway + controller) as host systemd
# units. Single ~2 GB server VM + a 512 MB client for ssh-ng builds.
#
# issue #57 1d: object-state controller tests (Pool / ComponentScaler /
# nodeclaim_pool) only need an apiserver to round-trip CRs and a
# rio-controller to reconcile them — they don't need real builder pods,
# Cilium, containerd, or airgap image import. k3s-full pays ~4 min boot
# + ~18 GB RAM for those; this fixture pays ~30 s + ~2.5 GB.
#
# Exports the k3sFull interface (ns/nsStore/nsBuilders/nsFetchers/
# helmRendered/nodes/kubectlHelpers/waitReady/sshKeySetup{,For}/
# pyNodeVars) so object-state scenarios run with minimal edits. The
# server node is NAMED `k3s-server` and ships a `k3s` shim (`exec
# kubectl "$@"`-ish) so the many `k3s_server.succeed("k3s kubectl …")`
# literals in scenarios resolve unchanged.
#
# What this fixture CANNOT prove (use k3s-full): real builder/fetcher
# pod execution, FUSE/cgroup paths, Cilium netpol, kube-proxy/NodePort,
# PSA admission on rio-* pods, kubelet Secret-mount semantics.
{
  pkgs,
  rio-workspace,
  rioModules,
  nixhelm,
  system,
  coverage ? false,
  ...
}:
let
  inherit (pkgs) lib;
  common = import ../common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };
  kwok = import ./kwok.nix { inherit pkgs; };
  helmRender = import ../../helm-render.nix { inherit pkgs nixhelm system; };

  ns = "rio-system";
  nsStore = "rio-store";
  nsBuilders = "rio-builders";
  nsFetchers = "rio-fetchers";
  kubeconfig = "/etc/kubernetes/cluster-admin.kubeconfig";

  # `k3s kubectl …` → `kubectl …` shim. KUBECONFIG baked in so bare
  # `kubectl` (the kubectlHelpers form) and `k3s kubectl` (the literal
  # form scattered through scenarios) both work.
  k3sShim = pkgs.writeShellScriptBin "k3s" ''
    exec env KUBECONFIG=${kubeconfig} "$@"
  '';
  wrappedKubectl =
    pkgs.runCommand "kubectl-admin"
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
      }
      ''
        mkdir -p $out/bin
        makeWrapper ${pkgs.kubectl}/bin/kubectl $out/bin/kubectl \
          --set KUBECONFIG ${kubeconfig}
      '';

  # One synthetic Node so kube-scheduler has a bind target. KWOK's
  # node-initialize Stage flips Ready=True; pod-ready/pod-complete then
  # progress anything kube-scheduler binds here (the helm-rendered
  # rio-* Deployments + builder Jobs all become KWOK-faked Ready pods —
  # the REAL rio-* processes run as systemd units on the same host).
  fakeNode = pkgs.writeText "kwok-node-0.yaml" ''
    apiVersion: v1
    kind: Node
    metadata:
      name: kwok-node-0
      annotations:
        kwok.x-k8s.io/node: fake
      labels:
        kubernetes.io/hostname: kwok-node-0
        kubernetes.io/os: linux
        kubernetes.io/arch: amd64
        type: kwok
    status:
      allocatable: {cpu: "32", memory: 32Gi, pods: "110"}
      capacity: {cpu: "32", memory: 32Gi, pods: "110"}
  '';
in
{
  # Per-test knobs (subset of k3sFull's). extraValuesTyped/extraValues/
  # extraValuesFiles flow into helm-render exactly as in k3s-full so
  # vm-componentscaler-kwok-only / vm-sla-sizing-kwok pass the same
  # values blocks they did against k3sFull.
  extraValues ? { },
  extraValuesTyped ? { },
  extraValuesFiles ? [ ],
  extraSchedulerConfig ? { },
  # Apply karpenter NodeClaim/NodePool CRDs + EC2NodeClass stub +
  # kwok nodeclaim Stages. With these CRDs present, rio-controller's
  # `nodeclaim_crd_present()` returns true → nodeclaim_pool reconciler
  # spawns and `validate_config` requires `nodeclaim_pool.database_url`
  # (always supplied below) + the `[nodeclaim_pool]` TOML table from
  # the helm-rendered ConfigMap. Default off — only the
  # forecast-provisioning scenario needs it.
  withKarpenter ? false,
}:
let
  helmRendered = helmRender {
    valuesFile = ../../../infra/helm/rio-build/values/vmtest-full.yaml;
    inherit extraValuesFiles;
    extraSet = extraValues;
    extraSetTyped = {
      "coverage.enabled" = coverage;
    }
    // extraValuesTyped;
    namespace = ns;
  };

  # Extract the rio-controller-config ConfigMap's controller.toml body
  # from the rendered workloads YAML. The systemd rio-controller below
  # mounts it at /etc/rio/controller.toml (rio_common::config::load's
  # search path) so `[nodeclaim_pool]` — a nested table that the
  # `RIO_*` env layer can't express — loads exactly as it would from
  # the chart's ConfigMap mount. yq-go: `.data` keys are quoted; the
  # `// ""` guards a missing ConfigMap (controller.toml is empty →
  # config falls through to compiled defaults + env).
  controllerConfigToml =
    pkgs.runCommand "rio-controller-config.toml"
      {
        nativeBuildInputs = [ pkgs.yq-go ];
      }
      ''
        yq 'select(.kind == "ConfigMap" and .metadata.name == "rio-controller-config")
            | .data."controller.toml" // ""' \
          ${helmRendered}/02-workloads.yaml > $out
      '';

  # Everything kubectl-applied at boot, ordered. CRDs (rio + kwok Stage
  # + optionally karpenter) FIRST so kwok-controller's Stage discovery
  # resolves NodeClaim on first informer sync — the k3s-full
  # `rollout restart deploy/kwok-controller` workaround in
  # forecast-provisioning.nix is unnecessary when ordering is right.
  bootManifests = pkgs.runCommand "kwok-only-boot-manifests" { } ''
    mkdir -p $out
    ln -s ${helmRendered}/00-crds.yaml      $out/00-rio-crds.yaml
    ln -s ${kwok.stageCRD}                  $out/01-kwok-stage-crd.yaml
    ${lib.optionalString withKarpenter ''
      ln -s ${kwok.karpenterCRDs}             $out/02-karpenter-crds.yaml
      ln -s ${kwok.ec2NodeClassStub}          $out/03-ec2nodeclass.yaml
    ''}
    ln -s ${helmRendered}/01-rbac.yaml      $out/04-rio-rbac.yaml
    ln -s ${helmRendered}/02-workloads.yaml $out/05-rio-workloads.yaml
    ln -s ${kwok.kwokDefaultStages}         $out/06-kwok-default-stages.yaml
    ${lib.optionalString withKarpenter ''
      ln -s ${kwok.nodeclaimStages}           $out/07-nodeclaim-stages.yaml
    ''}
    ln -s ${fakeNode}                       $out/08-fake-node.yaml
  '';

  serverNode =
    { ... }:
    {
      imports = [
        (common.mkControlNode {
          hostName = "k3s-server";
          memorySize = 2048;
          diskSize = 6144;
          inherit extraSchedulerConfig;
          # 9091/9092/9094: scheduler/store/controller metrics for
          # cross-VM curl probes. 6443: kube-apiserver (kubectl from
          # client not currently used, but cheap to open).
          extraFirewallPorts = [
            9091
            9092
            9094
          ];
          extraPackages = [
            wrappedKubectl
            k3sShim
            pkgs.grpcurl
          ];
        })
      ];

      # ── bare k8s control-plane (no kubelet) ──────────────────────────
      # roles=["master"] auto-wires apiserver + KCM + scheduler + etcd +
      # easyCerts cfssl PKI + cluster-admin.kubeconfig at
      # /etc/kubernetes/. Workload-side (kubelet/proxy/flannel/coredns/
      # containerd) is mkDefault → forced off below. masterAddress must
      # match the node hostname so the apiserver cert SAN covers the
      # name kubectl resolves.
      services.kubernetes = {
        roles = [ "master" ];
        masterAddress = "k3s-server";
        easyCerts = true;
        apiserver.allowPrivileged = true;
        kubelet.enable = lib.mkForce false;
        proxy.enable = lib.mkForce false;
        flannel.enable = lib.mkForce false;
        addonManager.enable = lib.mkForce false;
        addons.dns.enable = lib.mkForce false;
      };
      # roles=["master"] pulls in containerd via kubelet's default;
      # mkForce off saves ~200 MB RSS + boot time.
      virtualisation.containerd.enable = lib.mkForce false;

      # ── rio-controller (host binary, KUBECONFIG mode) ────────────────
      # No NixOS module (chart-only component); inline unit. kube::Client
      # ::try_default reads $KUBECONFIG when no in-cluster SA token is
      # mounted. Scheduler/store are co-located systemd → localhost.
      # /etc/rio/controller.toml carries the helm-rendered
      # `[nodeclaim_pool]` table; env keys override the simple scalars.
      environment.etc."rio/controller.toml".source = controllerConfigToml;
      systemd.services = {
        # easyCerts' cfssl CA + certmgr emit the cluster-admin kubeconfig
        # asynchronously. The apply-oneshot below polls /healthz so it
        # tolerates the gap, but ordering on certmgr cuts the typical
        # wait from ~15s to ~2s.
        kwok-apply-manifests = {
          wantedBy = [ "multi-user.target" ];
          after = [
            "kube-apiserver.service"
            "certmgr.service"
          ];
          path = [ wrappedKubectl ];
          serviceConfig = {
            Type = "oneshot";
            RemainAfterExit = true;
          };
          # Retry loop absorbs apiserver-not-ready + CRD-establish lag
          # (a CR applied before its CRD's `Established` condition gets
          # NotFound; the next retry succeeds). --server-side: CRDs are
          # large; client-side apply hits the 256 KB annotation limit on
          # the karpenter NodeClaim CRD.
          # Post-loop guards: a `for ... do cmd && break; done` falls
          # through with exit 0 when every attempt fails (errexit ignores
          # the LHS of &&). Without the guards the oneshot would report
          # SUCCESS even if apiserver never came up or every manifest was
          # rejected, and the real failure surfaces as an opaque
          # `get pool x86-64` timeout 60 s later.
          script = ''
            for _ in $(seq 1 60); do
              kubectl get --raw=/healthz >/dev/null 2>&1 && break
              sleep 1
            done
            kubectl get --raw=/healthz >/dev/null 2>&1 || {
              echo "kube-apiserver /healthz not ready after 60s" >&2
              exit 1
            }
            for f in ${bootManifests}/*.yaml; do
              ok=
              for _ in $(seq 1 30); do
                kubectl apply --server-side -f "$f" && { ok=1; break; }
                sleep 1
              done
              [ -n "$ok" ] || {
                echo "kubectl apply failed after 30 attempts: $f" >&2
                exit 1
              }
            done
          '';
        };

        # ── kube-build-scheduler (real second scheduler) ───────────────
        # forecast-provisioning sets buildScheduler.enabled=true →
        # rio-controller stamps builder Job pods with schedulerName=
        # kube-build-scheduler. Under k3s-full a real kube-scheduler
        # container ran (kwok.airgapImages preloaded it); under
        # kwok-only the chart-rendered Deployment is KWOK-faked-Ready
        # with no process, the default-scheduler ignores pods whose
        # schedulerName ≠ default-scheduler, and KWOK only progresses
        # pods that are already BOUND — so the builder pod sits Pending
        # forever and wait_worker_pod times out. A second host
        # kube-scheduler instance claiming that name is the fix; it
        # binds to whichever KWOK-managed Node fits, then KWOK fakes
        # the kubelet side. The chart's Deployment still renders (and
        # KWOK fakes it Ready), but it is the host instance that does
        # the actual binding. --leader-elect=false: no Lease contention
        # with the default scheduler (different schedulerName, but the
        # Lease is per-process not per-name).
        kube-build-scheduler = lib.mkIf withKarpenter {
          wantedBy = [ "multi-user.target" ];
          after = [ "kwok-apply-manifests.service" ];
          requires = [ "kwok-apply-manifests.service" ];
          serviceConfig = {
            Restart = "on-failure";
            RestartSec = 2;
            ExecStart = ''
              ${pkgs.kubernetes}/bin/kube-scheduler \
                --kubeconfig=${kubeconfig} \
                --authentication-kubeconfig=${kubeconfig} \
                --authorization-kubeconfig=${kubeconfig} \
                --leader-elect=false \
                --config=${pkgs.writeText "kube-build-scheduler.yaml" ''
                  apiVersion: kubescheduler.config.k8s.io/v1
                  kind: KubeSchedulerConfiguration
                  leaderElection:
                    leaderElect: false
                  clientConnection:
                    kubeconfig: ${kubeconfig}
                  profiles:
                    - schedulerName: kube-build-scheduler
                ''}
            '';
          };
        };

        # ── kwok-controller (host binary, --kubeconfig) ────────────────
        # --manage-all-nodes: NO real kubelet exists, so every Node is
        # KWOK's. Stage CRD + (optionally) NodeClaim CRD are applied
        # BEFORE this unit starts → discovery resolves on first informer
        # sync (the k3s-full restart hack is unnecessary). Restart=
        # on-failure absorbs the rare race where certmgr is still writing
        # the kubeconfig when kwok first reads it.
        kwok-controller = {
          wantedBy = [ "multi-user.target" ];
          after = [ "kwok-apply-manifests.service" ];
          requires = [ "kwok-apply-manifests.service" ];
          serviceConfig = {
            Restart = "on-failure";
            RestartSec = 2;
            ExecStart = ''
              ${pkgs.kwok}/bin/kwok \
                --kubeconfig=${kubeconfig} \
                --manage-all-nodes=true \
                --enable-crds=Stage \
                --node-lease-duration-seconds=40
            '';
          };
        };

        rio-controller = {
          wantedBy = [ "multi-user.target" ];
          after = [
            "kwok-apply-manifests.service"
            "rio-scheduler.service"
          ];
          requires = [ "kwok-apply-manifests.service" ];
          environment = {
            KUBECONFIG = kubeconfig;
            RIO_SCHEDULER__ADDR = "http://localhost:9001";
            RIO_STORE__ADDR = "http://localhost:9002";
            RIO_NODECLAIM_POOL__DATABASE_URL = common.databaseUrl;
            RIO_LOG_FORMAT = "pretty";
          }
          // lib.optionalAttrs coverage {
            LLVM_PROFILE_FILE = "/var/lib/rio/cov/rio-%%h-%%p-%%m.profraw";
          };
          serviceConfig = {
            Restart = "on-failure";
            RestartSec = 2;
            ExecStart = "${rio-workspace}/bin/rio-controller";
          };
        };

        # rio-scheduler boot-race guard (same as standalone.nix): block
        # until rio-store's gRPC port is open so the two `sqlx::migrate!`
        # runs don't deadlock on migration 011's CREATE INDEX CONCURRENTLY.
        rio-scheduler.preStart = ''
          for _ in $(seq 1 60); do
            ${pkgs.netcat}/bin/nc -z localhost 9002 && exit 0
            sleep 0.5
          done
          echo "rio-store port 9002 not open after 30s" >&2
          exit 1
        '';
      };
    };
in
rec {
  inherit
    ns
    nsStore
    nsBuilders
    nsFetchers
    helmRendered
    ;

  # rio-controller runs as a host systemd unit, not a pod. Scenarios
  # that restart the controller (componentscaler) branch on this so the
  # k3s variant keeps its `kubectl delete pod` semantics.
  controllerIsSystemd = true;

  nodes = {
    k3s-server = serverNode;
    client = common.mkClientNode {
      gatewayHost = "k3s-server";
      gatewayPort = 2222;
    };
  };

  # k3sFull-compatible kubectl()/leader_pod()/wait_worker_pod()/pf_exec().
  # pf_exec ignores the pod target — every rio-* binary is a co-located
  # systemd unit, so `port-forward` collapses to `curl localhost:<remote>`.
  # leader_pod() returns a sentinel; the systemd scheduler is a singleton
  # (no Lease — `lease_name` defaults to None outside k8s).
  kubectlHelpers = ''
    def kubectl(args, node=k3s_server, ns="${ns}"):
        return node.succeed(f"k3s kubectl -n {ns} {args}")

    def leader_pod():
        return "rio-scheduler"

    def wait_worker_pod(pool="x86-64", ns="${nsBuilders}", timeout=180):
        k3s_server.wait_until_succeeds(
            f"test -n \"$(k3s kubectl -n {ns} get pod -l rio.build/pool={pool} "
            "--field-selector=status.phase=Running -o name)\"",
            timeout=timeout,
        )
        return k3s_server.succeed(
            f"k3s kubectl -n {ns} get pod -l rio.build/pool={pool} "
            "-o jsonpath='{.items[0].metadata.name}'"
        ).strip()

    def pf_exec(target, remote, cmd, ns="${ns}", ok_nonzero=False):
        suffix = " || true" if ok_nonzero else ""
        return k3s_server.succeed(
            cmd.replace("__PORT__", str(remote)) + f" 2>&1{suffix}"
        )
  '';

  waitReady = ''
    ${common.waitForControlPlane "k3s_server"}
    k3s_server.wait_for_unit("kube-apiserver.service")
    k3s_server.wait_for_file("${kubeconfig}")
    k3s_server.wait_for_unit("kwok-apply-manifests.service")
    k3s_server.wait_for_unit("kwok-controller.service")
    k3s_server.wait_for_unit("rio-controller.service")
    k3s_server.wait_until_succeeds(
        "k3s kubectl get node kwok-node-0 "
        "-o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}' "
        "| grep -q True",
        timeout=60,
    )
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n ${nsBuilders} get pool x86-64 "
        "-o jsonpath='{.status}' | grep -q .",
        timeout=60,
    )
  '';

  # systemd-host variant: writes the gateway's authorized_keys file +
  # restarts the unit. `tenantComment` becomes the SSH key comment →
  # gateway's tenant_name.
  sshKeySetupFor = tenantComment: ''
    client.succeed(
        "mkdir -p /root/.ssh && "
        "ssh-keygen -t ed25519 -N ''' -C '${tenantComment}' -f /root/.ssh/id_ed25519"
    )
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    k3s_server.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    k3s_server.succeed("systemctl restart rio-gateway.service")
    k3s_server.wait_for_open_port(2222)
    client.wait_until_succeeds(
        "(${pkgs.netcat}/bin/nc -w2 k3s-server 2222 </dev/null 2>&1 || true) "
        "| grep -q ^SSH-",
        timeout=30,
    )
  '';
  sshKeySetup = sshKeySetupFor "";

  pyNodeVars = "k3s_server";
}
