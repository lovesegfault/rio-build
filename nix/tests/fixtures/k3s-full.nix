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
  mkCiliumRender =
    gatewayEnabled:
    import ../../cilium-render.nix {
      inherit
        pkgs
        nixhelm
        system
        gatewayEnabled
        ;
    };
  pulled = import ../../docker-pulled.nix { inherit pkgs; };
  ciliumImages = [
    pulled.cilium-agent
    pulled.cilium-operator-generic
  ];
  jwtKeys = import ../lib/jwt-keys.nix;
  hmacKeys = import ../lib/hmac-keys.nix { inherit pkgs; } null;
  # HMAC Secrets for the namespaces that mount them. Gateway+scheduler
  # live in rio-system; store lives in rio-store. Both keys go to both
  # namespaces (scheduler signs assignment tokens, store verifies both;
  # gateway signs service tokens). runCommand keeps base64 at build
  # time — no IFD on a non-deterministic openssl-rand output.
  hmacSecretsManifest = pkgs.runCommand "rio-hmac-secrets.yaml" { } ''
    for ns in rio-system rio-store; do
      cat >> $out <<EOF
    ---
    apiVersion: v1
    kind: Secret
    metadata:
      name: rio-hmac
      namespace: $ns
    data:
      hmac.key: $(base64 -w0 < ${hmacKeys}/hmac.key)
    ---
    apiVersion: v1
    kind: Secret
    metadata:
      name: rio-service-hmac
      namespace: $ns
    data:
      service-hmac.key: $(base64 -w0 < ${hmacKeys}/service-hmac.key)
    EOF
    done
  '';
in
{
  # --set overrides layered on top of vmtest-full.yaml. scenarios/
  # lifecycle.nix passes autoscaler tuning (pollSecs=3 etc).
  extraValues ? { },
  # Extra images for the airgap set. dockerImages.vmTestSeed covers every
  # rio-* component but NOT squid (dockerImages.fod-proxy is a separate
  # image). Scenarios that enable fodProxy.enabled=true need the squid
  # image preloaded or the pod goes ImagePullBackOff (airgapped — no pull).
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
  # yaml] to flip workerPool.privileged:false without duplicating the
  # full base values file.
  extraValuesFiles ? [ ],
  # --set (typed) overrides. bool/int values that MUST NOT be coerced
  # to string — e.g. bootstrap.enabled=true (a bool the template checks
  # with {{- if .Values... }}) or scheduler.replicas=2 (an int the
  # Deployment spec expects). --set-string "true" happens to be truthy
  # in Go templating (non-empty string) but --set-string "2" becomes
  # replicas: "2" in the rendered Deployment, which k8s API validation
  # rejects. prod-parity fixture passes bootstrap.enabled here.
  extraValuesTyped ? { },
  # Cilium Gateway API (dashboard routing). Flips gatewayAPI.enabled in
  # cilium-render.nix (vendors Gateway API CRDs + enables cilium-envoy
  # DaemonSet), preloads quay.io/cilium/cilium-envoy, sets dashboard.
  # enabled=true in the rio chart so the Gateway/GRPCRoute CRs render.
  # gRPC-Web translation is in-process at rio-scheduler (tonic-web, D3)
  # — the Gateway is plain HTTP routing. ~400MB extra image; only
  # enable for dashboard-specific scenarios.
  gatewayEnabled ? false,
}:
let
  ciliumRender = mkCiliumRender gatewayEnabled;

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
    // (pkgs.lib.optionalAttrs coverage {
      # PSA restricted (ADR-019 default for control-plane) blocks the
      # hostPath "cov" volume (_helpers.tpl rio.mounts cov family) that
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
    // pkgs.lib.optionalAttrs gatewayEnabled {
      # Renders dashboard-gateway.yaml (GatewayClass/Gateway/GRPCRoute).
      # These CRs sit in 02-workloads.yaml.
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

  # ── Airgap image set ────────────────────────────────────────────────
  # Same list on BOTH nodes — pods land on either via scheduler whims
  # (especially scheduler.replicas=2 antiAffinity). fod-proxy/bootstrap
  # excluded (disabled in vmtest-full.yaml).
  #
  # vmTestSeed is ONE multi-manifest oci-archive (six rio-<component>:dev
  # refs, blob-deduped layers). k3s imports serially before kubelet — one
  # tarball decompress registers all six refs, vs six per-component
  # docker-archives that would re-expand the same shared layers six times.
  # Per-component refs mean vmtest-full.yaml uses each image's own
  # Entrypoint (no `command:` override). Replaces the former `all`
  # aggregate (one image, all binaries) — see nix/docker.nix vmTestSeed.
  rioImages = [
    dockerImages.vmTestSeed
    pulled.bitnami-postgresql
  ]
  # (cilium-envoy preload removed — embedded mode, see cilium-render.nix)
  # nginx + SPA bundle (rio-dashboard:dev). dashboard.enabled=true
  # (set when gatewayEnabled) renders the nginx Deployment;
  # without this preload the pod goes ImagePullBackOff. The `?`
  # guard: coverage-mode dockerImages elides the dashboard attr
  # (nginx+static has no LLVM instrumentation — mkDockerImages
  # passes rioDashboard=null → docker.nix optionalAttrs drops it).
  # In coverage mode the nginx pod still backoffs, but only the
  # dashboard-curl scenario waits for it — and that scenario is
  # itself gated on the same `?` check (default.nix), so coverage-
  # mode vmTestsCov skips it entirely.
  ++ pkgs.lib.optional (gatewayEnabled && dockerImages ? dashboard) dockerImages.dashboard
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
  # gatewayEnabled adds cilium-envoy (~400MB) + dashboard (~100MB if
  # dockerImages?dashboard).
  extraImagesBumpGiB =
    builtins.length extraImages
    # Cilium agent (713M tar) + operator (113M) — always present, the
    # CNI is unconditional. Spike-A measured 88% of 2G with cilium-only;
    # with rio-* on top, +2 keeps headroom under the eviction threshold.
    + 2
    + (if gatewayEnabled then (if dockerImages ? dashboard then 2 else 1) else 0);
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
  # ── /dev/{fuse,kvm} via containerd base_runtime_spec ────────────────
  # Same JSON the EKS NixOS AMI uses (nix/nixos-node/containerd-config.
  # nix). k3s reads /var/lib/rancher/k3s/agent/etc/containerd/config.
  # toml.tmpl if present (via services.k3s.containerdConfigTemplate)
  # and renders it INSTEAD of its built-in base. We can't use the
  # `{{ template "base" . }}` + append pattern: the base already emits
  # `[plugins...runtimes.runc]` so a second header would be a TOML
  # duplicate-table error; nor can we use the config-v3.toml.d/ import
  # glob — containerd's mergeConfig() "Replace entire sections instead
  # of merging map's values" for Plugins (containerd/services/server/
  # config/config.go). So we write a FULL template — k3s's v3 base
  # (pkg/agent/templates/templates.go ContainerdConfigTemplateV3)
  # trimmed to what an airgapped VM test needs, with base_runtime_spec
  # spliced into the runc runtime.
  #
  # The Go-template `{{ . }}` interpolations are filled by k3s at
  # agent-startup with its NodeConfig (root/state paths under /var/
  # lib/rancher/k3s/agent, k3s-bundled pause image, CNI dirs). Nix
  # `''${` escapes literal `${` so k3s sees them.
  #
  # Resync against k3s ${k3sPinned.version} on bump:
  #   https://github.com/k3s-io/k3s/blob/${k3sPinned.version}/pkg/agent/templates/templates.go
  # Template-var drift surfaces as a vm-*-k3s CI failure; the
  # waitReady diagnostic dump cats the rendered config.toml.
  #
  # Both withKvm variants are built; k3s.service ExecStartPre (k3sBase
  # below → baseRuntimeSpec.pickExecStartPre) symlinks the matching one
  # to baseRuntimeSpec.runtimePath based on host /dev/kvm presence —
  # same script the EKS path uses (nix/nixos-node/eks-node.nix
  # containerd ExecStartPre).
  baseRuntimeSpec = import ../../base-runtime-spec.nix { inherit pkgs; };
  k3sContainerdConfigTmpl = ''
    version = 3
    root = {{ printf "%q" .NodeConfig.Containerd.Root }}
    state = {{ printf "%q" .NodeConfig.Containerd.State }}

    [grpc]
      address = {{ deschemify .NodeConfig.Containerd.Address | printf "%q" }}

    [plugins.'io.containerd.internal.v1.opt']
      path = {{ printf "%q" .NodeConfig.Containerd.Opt }}

    [plugins.'io.containerd.grpc.v1.cri']
      stream_server_address = "127.0.0.1"
      stream_server_port = "10010"

    [plugins.'io.containerd.cri.v1.runtime']
      enable_selinux = {{ .NodeConfig.SELinux }}
      enable_unprivileged_ports = {{ .EnableUnprivileged }}
      enable_unprivileged_icmp = {{ .EnableUnprivileged }}
      device_ownership_from_security_context = {{ .NonrootDevices }}

    {{ with .NodeConfig.AgentConfig.Snapshotter }}
    [plugins.'io.containerd.cri.v1.images']
      snapshotter = "{{ . }}"
      disable_snapshot_annotations = true
      use_local_image_pull = true
    {{ end }}

    {{ with .NodeConfig.AgentConfig.PauseImage }}
    [plugins.'io.containerd.cri.v1.images'.pinned_images]
      sandbox = "{{ . }}"
    {{ end }}

    [plugins.'io.containerd.cri.v1.runtime'.cni]
      {{ with .NodeConfig.AgentConfig.CNIBinDir }}bin_dirs = [{{ printf "%q" . }}]{{ end }}
      {{ with .NodeConfig.AgentConfig.CNIConfDir }}conf_dir = {{ printf "%q" . }}{{ end }}

    [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc]
      runtime_type = "io.containerd.runc.v2"
      base_runtime_spec = "${baseRuntimeSpec.runtimePath}"

    [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc.options]
      SystemdCgroup = {{ .SystemdCgroup }}

    [plugins.'io.containerd.cri.v1.images'.registry]
      config_path = {{ printf "%q" .NodeConfig.Containerd.Registry }}
  '';

  # Everything except services.k3s (which differs by role). Extracted
  # so the firewall/kernel/systemd bits don't duplicate. Flannel VXLAN
  # needs 8472/udp both ways; kubelet 10250 for `kubectl exec`/logs
  # (agent → server AND server → agent for 2-node).
  k3sBase = {
    services.k3s = {
      package = k3sPinned;
      containerdConfigTemplate = k3sContainerdConfigTmpl;
    };
    swapDevices = [ ];
    boot.kernelModules = [
      "fuse"
      "wireguard"
    ];
    boot.kernelParams = [ "systemd.unified_cgroup_hierarchy=1" ];

    networking.firewall = {
      allowedTCPPorts = [
        6443 # apiserver (server only, but opening on agent is a no-op)
        10250 # kubelet (kubectl exec/logs)
        32222 # gateway NodePort — cilium kube-proxy-replacement listens on every node
        4240 # cilium-health
        4244 # hubble (Phase 4)
      ];
      allowedUDPPorts = [
        8472 # cilium VXLAN tunnel (chart default routingMode=tunnel)
        51871 # cilium WireGuard
      ];
      # Pod→Service after socketLB redirect (10.43.0.1→192.168.1.3:6443)
      # arrives on cilium_host with a pod-CIDR source. Without trust, the
      # NixOS firewall INPUT chain + checkReversePath drop it. Cilium's
      # own datapath enforces policy; the host firewall on these
      # interfaces is redundant.
      trustedInterfaces = [
        "cilium_host"
        "cilium_net"
        "cilium_wg0"
        "lxc+"
      ];
      checkReversePath = false;
    };
    # cilium-agent creates cilium_host/cilium_net/lxc_health and per-pod
    # lxc* veths. dhcpcd default-config tries to DHCP on every new iface
    # → log spam + a few hundred ms of needless probe traffic per pod
    # churn. Cilium assigns IPs itself.
    networking.dhcpcd.denyInterfaces = [
      "cilium_*"
      "lxc*"
    ];

    systemd.services.k3s.serviceConfig = {
      # Pick the base_runtime_spec variant matching host /dev/kvm
      # presence (k3s embeds containerd, so this runs pre-containerd).
      # List form so it merges with the nixpkgs k3s module's preStart.
      ExecStartPre = [ baseRuntimeSpec.pickExecStartPre ];
      # containerd needs cgroup delegation for pod cgroups. Without:
      # ContainerCreating forever.
      Delegate = "yes";
      # Drop the 1Hz retry spam during the ~180s airgap-import window
      # (apiserver up, kubelet blocked on serial image import → node
      # not yet registered). 182 lines/run pre-filter. "~" prefix =
      # exclude matching from journal ingestion (systemd 253+) — never
      # reaches console→serial→testlog. Other k3s logs unaffected.
      LogFilterPatterns = "~Unable to set control-plane role label";
    };

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
      pkgs.wireguard-tools # `wg show cilium_wg0` (cilium-encrypt.nix)
    ];

    # r[impl builder.seccomp.localhost-profile+2]
    # Same tmpfiles delivery as the NixOS AMI (nix/nixos-node/hardening.nix):
    # profiles are store paths, copied into kubelet's seccomp dir before k3s
    # (and its embedded kubelet) starts. k3s passes `--root-dir
    # /var/lib/kubelet` to kubelet, so the path matches EKS — no
    # /var/lib/rancher indirection. By the time any pod schedules the file
    # is guaranteed present; rio-controller emits Localhost without a wait.
    # `C` (copy, not `L` symlink): runc opens the profile via the literal
    # localhostProfile path; a /nix/store symlink would have a different
    # store path on every fixture rebuild.
    systemd.tmpfiles.rules = [
      "d /var/lib/kubelet/seccomp/operator 0755 root root -"
      "C /var/lib/kubelet/seccomp/operator/rio-builder.json 0644 root root - ${../../nixos-node/seccomp/rio-builder.json}"
      "C /var/lib/kubelet/seccomp/operator/rio-fetcher.json 0644 root root - ${../../nixos-node/seccomp/rio-fetcher.json}"
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
        images = [ config.services.k3s.package.airgap-images ] ++ rioImages ++ ciliumImages;
        manifests = {
          # Cilium CNI — applied first (filename-alphabetical: `000-` sorts
          # before everything). Nothing else can schedule until cilium-agent
          # is Ready on the node. No CRDs file: cilium-operator installs
          # CRDs at runtime (cilium-render.nix comment).
          "000-cilium-rbac".source = "${ciliumRender}/01-cilium-rbac.yaml";
          "001-cilium".source = "${ciliumRender}/02-cilium.yaml";
          "00-rio-crds".source = "${helmRendered}/00-crds.yaml";
          "01-rio-rbac".source = "${helmRendered}/01-rbac.yaml";
          "02-rio-workloads".source = "${helmRendered}/02-workloads.yaml";
        }
        // pkgs.lib.optionalAttrs gatewayEnabled {
          # Gateway API CRDs (gateway.networking.k8s.io). Cilium expects
          # these pre-installed; gatewayAPI.enabled is a silent no-op
          # without them (research-A C1). 000-* prefix sorts before the
          # cilium manifests so cilium-operator sees CRDs at start.
          "000-cilium-gateway-api-crds".source = "${ciliumRender}/00-gateway-api-crds.yaml";
          # LB-IPAM pool so the per-Gateway type:LoadBalancer Service
          # gets an external IP → Gateway Programmed:True. Applied
          # after 001-cilium; k3s deploy controller retries until the
          # CRD exists (cilium-operator installs it at runtime).
          "002-cilium-lbipam-pool".source = "${ciliumRender}/03-cilium-lbipam-pool.yaml";
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
          # HMAC keys. Applied before 02-workloads so pods' Secret
          # volume mounts resolve on first start (no Pending →
          # ContainerCreating churn waiting for the Secret).
          "01-rio-hmac-secrets".source = hmacSecretsManifest;
        };
        extraFlags = [
          # Cilium IS the CNI — disable k3s's bundled flannel + kube-router
          # netpol controller + kube-proxy + servicelb (Cilium's eBPF
          # replaces all four; cilium-render.nix sets
          # kubeProxyReplacement=true with devices=eth1 pinned).
          "--flannel-backend=none"
          "--disable-network-policy"
          "--disable-kube-proxy"
          "--disable=servicelb"
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
          # binds, cilium NodePort) so existing v4-only assertions in
          # scenarios stay valid.
          "--cluster-cidr"
          "10.42.0.0/16,2001:db8:42::/56"
          "--service-cidr"
          "10.43.0.0/16,2001:db8:43::/112"
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
        images = [ config.services.k3s.package.airgap-images ] ++ rioImages ++ ciliumImages;
        extraFlags = [
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

    def worker_pod(pool="x86-64-tiny", ns="${nsBuilders}", node=k3s_server):
        """First Running worker pod for a pool. Ephemeral Jobs have no
        stable ordinal — resolve by label. Raises if none found."""
        name = node.succeed(
            f"k3s kubectl -n {ns} get pod -l rio.build/pool={pool} "
            "--field-selector=status.phase=Running "
            "-o jsonpath='{.items[0].metadata.name}'"
        ).strip()
        assert name, f"no Running pod for rio.build/pool={pool} in ns={ns}"
        return name

    def wait_worker_pod(pool="x86-64-tiny", ns="${nsBuilders}", timeout=180):
        """Poll until a worker pod is Running for the pool; return its
        name. With ephemeral Jobs, a build must be queued first."""
        try:
            k3s_server.wait_until_succeeds(
                f"test -n \"$(k3s kubectl -n {ns} get pod "
                f"-l rio.build/pool={pool} "
                "--field-selector=status.phase=Running -o name)\"",
                timeout=timeout,
            )
        except Exception:
            print(f"=== wait_worker_pod TIMEOUT pool={pool} ns={ns} ===")
            print(k3s_server.execute(
                f"k3s kubectl -n {ns} get builderpool,fetcherpool,job,pod -o wide 2>&1; "
                f"k3s kubectl -n {ns} describe builderpool 2>&1; "
                "k3s kubectl -n rio-system logs deploy/rio-controller --tail=60 2>&1; "
                "k3s kubectl -n rio-system logs deploy/rio-scheduler --tail=40 2>&1"
            )[1])
            raise
        return worker_pod(pool=pool, ns=ns)

    # ── Port-forward helpers ──────────────────────────────────────────
    # Long-lived: pf_open backgrounds kubectl port-forward, writes
    # /tmp/{tag}.pid + /tmp/{tag}.log, then nc-probes (NOT sleep — fails
    # fast if pf died: scheduler crashed, port in use). pf_close kills.
    # Ports are caller-chosen (long-lived → no TIME_WAIT contention; pick
    # distinct ports if you need >1 concurrent forward).
    def pf_open(target, local, remote, ns="${ns}", tag="pf"):
        k3s_server.succeed(
            f"k3s kubectl -n {ns} port-forward {target} {local}:{remote} "
            f">/tmp/{tag}.log 2>&1 & echo $! > /tmp/{tag}.pid"
        )
        k3s_server.wait_until_succeeds(
            f"${pkgs.netcat}/bin/nc -z localhost {local}", timeout=10
        )

    def pf_close(tag="pf"):
        k3s_server.execute(
            f"kill $(cat /tmp/{tag}.pid) 2>/dev/null; rm -f /tmp/{tag}.pid"
        )

    # Per-call: auto-allocates a fresh local port (TIME_WAIT-safe —
    # port-forward lacks SO_REUSEADDR, ~60s rebind), backgrounds with
    # trap-kill, sleeps 2s, runs cmd (literal `__PORT__` substituted —
    # NOT .format(): cmd is usually a JSON-bearing grpcurl line and
    # braces would break str.format). Returns stdout+stderr. ok_nonzero
    # appends `|| true` for swallow-DeadlineExceeded / AlreadyExists.
    _pf_port = iter(range(19500, 19600))

    def pf_exec(target, remote, cmd, ns="${ns}", ok_nonzero=False):
        port = next(_pf_port)
        suffix = " || true" if ok_nonzero else ""
        return k3s_server.succeed(
            f"k3s kubectl -n {ns} port-forward {target} {port}:{remote} "
            f">/dev/null 2>&1 & pf=$!; "
            f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
            + cmd.replace("__PORT__", str(port))
            + f" 2>&1{suffix}"
        )
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
    # fallback): system bundle (pause/CNI/bitnami) first, then the
    # rio vmTestSeed oci-archive. bitnami is NOT last — the seed
    # (~union of all 6 component layers) comes after. Gate on pause
    # (minimal kubelet pod infra) + rio-gateway (any one of the six
    # refs the seed registers — they all land from one import, so one
    # ref present ⇒ seed import done).
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
            "k3s ctr images ls -q | grep -q 'rio-gateway'", timeout=600
        )

    # ── Server node registered (kubelet up, images imported) ────────
    # Agent cannot join until the server's kubelet has registered the
    # server node with the apiserver. Pre-fix, agent-Ready absorbed
    # ~107s of rio-* import time in its 120s budget (successful run:
    # 106.70/120s = 89% burned; 1.8× slower builder blew it by ~72s).
    # This gate directly tests the actual precondition. EXISTS (not
    # Ready) is sufficient: Ready needs CNI which needs cilium-agent
    # which needs the node to exist first.
    # TODO(P0304): timeout=600 predates containerd-tmpfs (same story as
    # the rio-* seed gate above). ~180s typical under TCG; reduce to 300
    # once tmpfs is verified to have collapsed the builder-disk tail.
    k3s_server.wait_until_succeeds(
        "k3s kubectl get node k3s-server 2>/dev/null",
        timeout=600,
    )

    # ── Cilium CNI ready (DaemonSet rolled out + agent registered) ──
    # k3s auto-applies manifests as soon as the apiserver is up —
    # INDEPENDENTLY of CNI readiness. Pods scheduled before the
    # cilium-agent on that node is Running fail CreatePodSandbox with
    # `failed to find plugin "cilium-cni"`. Kubelet backoff
    # (exponential, up to 5m) then delays recovery past the worker-
    # Ready timeout. Gate on the DaemonSet rollout (both nodes' agents
    # Ready) + the agent's CiliumNode CR (cilium-agent has registered
    # with the operator).
    k3s_server.wait_until_succeeds(
        "k3s kubectl -n kube-system rollout status ds/cilium --timeout=240s",
        timeout=260,
    )
    k3s_server.wait_until_succeeds(
        "k3s kubectl get ciliumnode k3s-agent",
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

    # ── BuilderPool reconciled ──────────────────────────────────────
    # All worker pods are ephemeral Jobs — no standing pod to wait
    # for. The reconciler patches `.status` on first reconcile;
    # presence of status confirms the controller has seen the CR and
    # the Job-spawn loop is live. First build in each test triggers a
    # Job spawn (~10s reconcile tick + ~10s pod schedule).
    try:
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${nsBuilders} get builderpool x86-64-tiny "
            "-o jsonpath='{.status}' | grep -q .",
            timeout=60,
        )
    except Exception:
        print("=== builderpool .status TIMEOUT — diagnostic dump ===")
        print(k3s_server.execute(
            "k3s kubectl -n ${nsBuilders} get builderpool,job,pod -o wide 2>&1; "
            "k3s kubectl -n ${nsBuilders} describe builderpool 2>&1; "
            "k3s kubectl -n ${ns} logs deploy/rio-controller --tail=80 2>&1"
        )[1])
        raise
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
    # during the scale 0→1 endpoint churn). Observed: vm-lifecycle-bps
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
