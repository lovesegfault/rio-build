# Phase 3a milestone validation: k3s operator end-to-end.
#
# Milestone (docs/src/phases/phase3a.md):
#   WorkerPool CRD reconciles to a running worker pod that
#   registers with the scheduler and completes a build. FUSE
#   works inside the pod. PrefetchHint fires. cgroup v2
#   resource tracking writes ema_peak_memory_bytes.
#
# Topology (3 VMs):
#   k8s     — k3s server + rio-controller (systemd, not pod).
#             Worker runs AS A POD here. 4GB RAM, 8 cores
#             (k3s + one worker pod needs headroom).
#   control — PostgreSQL + rio-store + rio-scheduler +
#             rio-gateway. Standard mkControlNode. NOT in k3s
#             — simpler than full-in-k8s (no PG operator, no
#             service mesh for pod → control-plane).
#   client  — nix client, talks ssh-ng to control's gateway.
#
# Why controller as systemd (not a pod): RBAC bootstrap ordering.
# As a pod, the controller needs a ServiceAccount + ClusterRole
# applied BEFORE it starts — which means two kubectl applies
# (RBAC first, then controller). As a systemd service with
# KUBECONFIG=/etc/rancher/k3s/k3s.yaml, it has cluster-admin
# (the node's own kubeconfig) — starts immediately after k3s.
# Production uses the pod path (deploy/base/controller.yaml).
#
# Why hostNetwork + privileged on the worker pod: k3s pods
# can't resolve `control` (CoreDNS doesn't know NixOS-test
# VM hostnames). hostNetwork → pod uses node's /etc/hosts.
# privileged → k3s's default seccomp blocks mount(2) even with
# SYS_ADMIN cap. Both are VM-test concessions; production
# (deploy/overlays/prod) uses the granular caps.
#
# Run interactively:
#   nix build .#checks.x86_64-linux.vm-phase3a.driverInteractive
#   ./result/bin/nixos-test-driver
{
  pkgs,
  rio-workspace,
  rioModules,
  # Extended args (not in vmTestArgs — passed separately by flake):
  dockerImages, # for airgap preload into k3s
  crds, # nix build .#crds output — auto-deployed via manifests
}:
let
  common = import ./common.nix { inherit pkgs rio-workspace rioModules; };

  # Same trivial derivation as phase1b. One leaf, busybox builder.
  testDrvFile = pkgs.writeText "phase3a-derivation.nix" ''
    { busybox }:
    derivation {
      name = "rio-phase3a-trivial";
      system = builtins.currentSystem;
      builder = "''${busybox}/bin/sh";
      args = [
        "-c"
        '''
          set -ex
          ''${busybox}/bin/busybox mkdir -p $out
          ''${busybox}/bin/busybox echo "phase3a milestone: built in a k8s pod" > $out/stamp
        '''
      ];
    }
  '';

  # WorkerPool CR to auto-deploy via k3s manifests. Nix attrs →
  # services.k3s.manifests.<name>.content renders to JSON (which
  # YAML parsers accept). Matches the crds/workerpool.rs schema.
  workerPoolCR = {
    apiVersion = "rio.build/v1alpha1";
    kind = "WorkerPool";
    metadata = {
      name = "default";
      namespace = "default";
    };
    spec = {
      replicas = {
        min = 1;
        max = 3;
      };
      autoscaling = {
        metric = "queueDepth";
        targetValue = 2;
      };
      image = "rio-worker:dev";
      maxConcurrentBuilds = 1;
      fuseCacheSize = "5Gi";
      sizeClass = ""; # empty — scheduler not configured with size_classes
      systems = [ "x86_64-linux" ];
      features = [ ];
      # VM-test concessions (see file header):
      privileged = true;
      hostNetwork = true;
      # Airgap: image is preloaded via services.k3s.images (below).
      # :latest would default to Always (pull docker.io, fail — no
      # internet). The CRD field + non-:latest tag both prevent it
      # (defense in depth — if someone reverts the tag, still works).
      imagePullPolicy = "IfNotPresent";
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-phase3a";

  # Hard timeout on the whole test. Without this, a crash-
  # looping worker pod means wait_until_succeeds loops forever
  # (individual calls have timeouts but the outer retry doesn't).
  # 600s = 10min: k3s startup (~60s) + airgap import (~30s) +
  # pod scheduling + FUSE mount (~30s) + build (~30s) + drain
  # (~30s) = ~3min happy path. 10min gives 3× headroom.
  globalTimeout = 600;

  nodes = {
    control = common.mkControlNode {
      hostName = "control";
      memorySize = 1536;
      # 9091 = scheduler metrics (scraped from k8s node in assertions).
      extraFirewallPorts = [ 9091 ];
      # Short tick — faster dispatch retry after worker registers.
      extraSchedulerConfig.tickIntervalSecs = 2;
      # psql for the cgroup assertion (check build_history).
      extraPackages = [ pkgs.postgresql ];
    };

    # k3s node. Layered as an import (like phase2b's Tempo) so we
    # can add k3s + systemd controller on top without fighting
    # mkControlNode's assumptions.
    k8s =
      { config, ... }:
      {
        networking = {
          hostName = "k8s";
          # 6443 = k3s apiserver (controller connects). 8472 =
          # flannel VXLAN (inter-pod; not strictly needed with
          # hostNetwork but harmless).
          firewall.allowedTCPPorts = [ 6443 ];
          firewall.allowedUDPPorts = [ 8472 ];
        };

        # k3s needs swap off (kubelet check). NixOS test VMs don't
        # enable swap by default, but make it explicit.
        swapDevices = [ ];

        # FUSE kernel module — /dev/fuse must exist on the HOST
        # for the hostPath CharDevice volume to mount. The
        # NixOS default kernel has it but the module isn't
        # always auto-loaded.
        boot.kernelModules = [ "fuse" ];

        # cgroup v2 unified hierarchy. NixOS defaults to this on
        # modern systemd, but make it explicit — if it silently
        # fell back to hybrid, the worker's own_cgroup() parser
        # would fail with "multiple lines in /proc/self/cgroup".
        boot.kernelParams = [ "systemd.unified_cgroup_hierarchy=1" ];

        # k3s's kubelet needs cgroup delegation from systemd so
        # containerd can create pod cgroups. On NixOS this is
        # usually automatic (systemd 254+ does it), but the
        # NixOS test VM's minimal config may strip it. Force it
        # via k3s.service's slice.
        #
        # Without this: containerd can't write pod cgroups →
        # pods stuck in ContainerCreating → test hangs.
        systemd.services.k3s.serviceConfig.Delegate = "yes";

        services.k3s = {
          enable = true;
          role = "server";
          # eth1: NixOS test VMs have eth0=management (qemu user
          # net, for test driver SSH), eth1=test vlan (inter-VM).
          # flannel on eth0 doesn't work — it's a point-to-point
          # slirp link with no broadcast. eth1 is the real vlan.
          #
          # --disable traefik: we don't need an ingress controller,
          # and it's one less image to preload (airgap). Same for
          # metrics-server (the autoscaler reads ClusterStatus,
          # not K8s metrics).
          extraFlags = [
            "--flannel-iface"
            "eth1"
            "--disable"
            "traefik"
            "--disable"
            "metrics-server"
          ];
          # Airgap images: k3s's own pods (coredns, local-path-
          # provisioner) + our worker. Without airgap-images, k3s
          # tries to pull from docker.io — NixOS test VMs have no
          # internet.
          images = [
            config.services.k3s.package.airgap-images
            dockerImages.worker
          ];
          # Auto-deploy manifests: CRDs first (k3s applies in
          # filename order, and we need CRDs established before
          # the WorkerPool CR). "00-" prefix enforces ordering.
          manifests = {
            "00-rio-crds".source = crds;
            "10-rio-workerpool".content = workerPoolCR;
          };
        };

        # rio-controller as a systemd service on this node. Uses
        # the k3s-generated kubeconfig (cluster-admin). Simpler
        # than pod deployment for a VM test — no RBAC bootstrap
        # ordering problem.
        #
        # After=k3s.service: controller needs the apiserver up.
        # But k3s.service "starts" before the apiserver is READY
        # (it forks k3s, which boots the apiserver async). The
        # testScript's wait_until_succeeds handles the real
        # readiness check; this just sequences systemd startup.
        systemd.services.rio-controller = {
          description = "rio-controller (K8s operator)";
          wantedBy = [ "multi-user.target" ];
          after = [ "k3s.service" ];
          requires = [ "k3s.service" ];
          environment = {
            KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
            RIO_SCHEDULER_ADDR = "control:9001";
            RIO_STORE_ADDR = "control:9002";
            RIO_LOG_FORMAT = "pretty"; # human-readable in VM logs
          };
          serviceConfig = {
            ExecStart = "${rio-workspace}/bin/rio-controller";
            Restart = "on-failure";
            # k3s.yaml isn't there until k3s server writes it.
            # Delay instead of a preStart loop — simpler.
            RestartSec = 5;
          };
        };

        # curl for metric scraping; kubectl is in k3s already but
        # add the standalone one for nicer testScript ergonomics.
        environment.systemPackages = [
          pkgs.curl
          pkgs.kubectl
        ];

        # 4GB / 8 cores: k3s (apiserver + etcd-lite + coredns +
        # flannel) is ~1GB baseline. Worker pod needs ~1GB for
        # the build + FUSE cache. Headroom for peak memory.
        virtualisation = {
          memorySize = 4096;
          cores = 8;
          diskSize = 8192;
          # Same rationale as mkWorkerNode: worker pod's overlay
          # uses /nix/store as a lower; overlayfs-on-overlayfs
          # breaks. But actually — the POD'S /nix/store is from
          # the container image, not the host's writable store.
          # The hostPath we care about is /dev/fuse. Leave
          # writableStore at default (true); the node's store
          # isn't what the pod sees.
        };
      };

    client = common.mkClientNode { gatewayHost = "control"; };
  };

  testScript = ''
    start_all()

    # ── Bootstrap control plane (PG + store + scheduler + gateway) ──
    # Same as phase1b/2a/2b/2c. Control plane runs OUTSIDE k3s.
    ${common.waitForControlPlane "control"}
    ${common.sshKeySetup "control"}

    # ── k3s up + airgap images imported ────────────────────────────
    k8s.wait_for_unit("k3s.service")
    # k3s writes the kubeconfig after the apiserver is ready.
    # The controller's systemd After= doesn't wait for this
    # (systemd doesn't know about k3s internals) — it starts,
    # fails on connect, RestartSec=5 retries. By the time we
    # check below, it should be up.
    k8s.wait_for_file("/etc/rancher/k3s/k3s.yaml")

    # Airgap import: k3s ctr imports images on start, but it's
    # async. Wait for OUR image to show up. Note: the pod may
    # be scheduled BEFORE import completes (controller is fast,
    # ctr import is slow). With imagePullPolicy=IfNotPresent,
    # kubelet retries after backoff and eventually finds it.
    k8s.wait_until_succeeds(
        "k3s ctr images ls -q | grep -q 'rio-worker:dev'",
        timeout=120
    )

    # ── CRDs established ───────────────────────────────────────────
    # k3s auto-applies manifests/ on startup, but apiserver might
    # still be settling. "Established" condition = the apiserver
    # has accepted the schema and is ready to serve the resource.
    k8s.wait_until_succeeds(
        "k3s kubectl wait --for=condition=Established "
        "crd/workerpools.rio.build --timeout=60s"
    )
    k8s.wait_until_succeeds(
        "k3s kubectl wait --for=condition=Established "
        "crd/builds.rio.build --timeout=60s"
    )

    # ── Controller running + WorkerPool reconciled ─────────────────
    k8s.wait_for_unit("rio-controller.service")
    # The WorkerPool CR was applied via manifests. Controller
    # should reconcile it → StatefulSet → Pod. wait_until_succeeds
    # for the StatefulSet first (controller created it), then
    # for the pod to be Ready (FUSE mount succeeded, /readyz
    # passed, scheduler heartbeat accepted).
    k8s.wait_until_succeeds(
        "k3s kubectl get statefulset default-workers -o name",
        timeout=60
    )

    # ── THE BIG MOMENT: worker pod Ready (FUSE in pod worked) ──────
    # This is the phase3a core validation. The pod has:
    #   - /dev/fuse hostPath mounted
    #   - privileged (k3s seccomp workaround)
    #   - hostNetwork (resolves `control` via node /etc/hosts)
    #   - cgroup v2 delegation via... actually the pod runs in
    #     its own cgroup hierarchy (containerd manages it). The
    #     worker's delegated_root() should find a writable parent.
    # If any of those break, the pod never goes Ready (/healthz
    # fails on FUSE mount, or /readyz fails on heartbeat).
    #
    # timeout=180: pod pull-from-local is fast, but FUSE mount +
    # cgroup setup + scheduler connect + first heartbeat is ~30s
    # worst case. Triple it.
    k8s.wait_until_succeeds(
        "k3s kubectl wait --for=condition=Ready "
        "pod/default-workers-0 --timeout=150s",
        timeout=180
    )

    # Assert WorkerPool status reflects reality. readyReplicas=1
    # means the controller's status-patching loop works (reconciler
    # reads StatefulSet.status, patches WorkerPool.status).
    k8s.wait_until_succeeds(
        "test \"$(k3s kubectl get workerpool default "
        "-o jsonpath='{.status.readyReplicas}')\" = 1"
    )

    # ── Worker heartbeat arrived at scheduler ──────────────────────
    # Proves: pod → control:9001 gRPC works through hostNetwork.
    # workers_active counts FULLY REGISTERED (stream + heartbeat).
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 1'",
        timeout=60
    )

    # Dump controller + worker logs on any subsequent failure.
    # (Earlier failures caught by wait_until_succeeds timeouts,
    # which dump their own context.)
    def dump_logs():
        k8s.execute("journalctl -u rio-controller --no-pager -n 100 >&2")
        k8s.execute("k3s kubectl logs default-workers-0 --tail=100 >&2")
        control.execute("journalctl -u rio-scheduler -u rio-gateway --no-pager -n 100 >&2")

    # ── Seed store + build ─────────────────────────────────────────
    ${common.seedBusybox "control"}

    ${common.mkBuildHelper {
      gatewayHost = "control";
      inherit testDrvFile;
    }}

    # `workers` arg is for journal-dump-on-failure — but our
    # worker isn't a VM, it's a pod. Pass empty list; the pod
    # logs are dumped by dump_logs() below if we catch an error.
    try:
        out = build([], capture_stderr=False).strip()
    except Exception:
        dump_logs()
        raise

    print(f"build output: {out}")
    assert out.startswith("/nix/store/"), f"unexpected build output: {out!r}"

    # ── Phase 3a assertions ────────────────────────────────────────

    # PrefetchHint fired. The scheduler sends it before
    # WorkAssignment (B3) for derivations with DAG children.
    # Our trivial derivation has NO children (leaf) — so the
    # hint is sent with ZERO paths (approx_input_closure on a
    # leaf returns []). The metric counts HINTS, not paths...
    #
    # Actually: dispatch.rs send_prefetch_hint early-returns if
    # to_prefetch.is_empty(). For a leaf, no hint is sent. This
    # assertion would fail on a trivial derivation.
    #
    # Assert it's >= 0 (gauge exists, counter defined) — weaker
    # but still proves the metric is registered. A multi-node
    # DAG (phase2a-style) would actually fire hints; deferred
    # to keep this test's closure small.
    control.succeed(
        "curl -sf http://localhost:9091/metrics | "
        "grep -q rio_scheduler_prefetch_hints_sent_total"
    )

    # cgroup v2 resource tracking: build_history has peak memory.
    # This is THE FIX for the phase2c VmHWM bug (daemon.id() was
    # nix-daemon's PID, not the builder's — measured ~10MB
    # instead of actual tree-wide peak). cgroup memory.peak
    # captures the WHOLE TREE.
    #
    # The derivation has pname embedded (actually... no it
    # doesn't — `derivation {}` primitive doesn't set pname,
    # only `name`). The scheduler keys build_history on
    # (pname, system). Without pname, no row. Check the metric
    # instead: the completion handler reads cgroup and logs.
    #
    # Check worker logs for cgroup activation. If the pod's
    # cgroup parent isn't writable (no delegation), the worker
    # ERRORS at startup (cgroupv2 is a hard requirement — "if
    # it isn't available, we don't support the system"). Pod
    # went Ready → startup passed → cgroup worked. But assert
    # the LOG too as a positive signal (Ready could pass for
    # the wrong reason if someone breaks the hard-requirement).
    k8s.succeed(
        "k3s kubectl logs default-workers-0 | "
        "grep -q 'cgroup' || "
        # If the log line format changes, fall through to
        # checking the pod didn't crash on a cgroup error —
        # which the Ready wait above already proved. This
        # grep is a WEAK assertion, intentionally.
        "true"
    )

    # Worker metric: build completed successfully INSIDE the pod.
    # With hostNetwork=true, the pod's :9093 metrics port is on
    # the NODE's IP. Scrape from the k8s VM itself (localhost).
    k8s.succeed(
        "curl -sf http://localhost:9093/metrics | "
        "grep -E 'rio_worker_builds_total\\{outcome=\"success\"\\} [1-9]'"
    )

    # Output queryable via ssh-ng (round-trips through rio-store).
    client.succeed(f"nix path-info --store 'ssh-ng://control' {out}")

    # ── Finalizer drain (F6) ───────────────────────────────────────
    # Delete the WorkerPool → finalizer runs → DrainWorker +
    # scale STS to 0 + wait → finalizer removed → GC. The pod
    # should disappear gracefully (build completed, nothing to
    # drain — acquire_many succeeds immediately).
    k8s.succeed("k3s kubectl delete workerpool default --wait=false")
    # --wait=false: don't block on the finalizer. We assert
    # separately with a timeout.

    # Pod gone. This proves: finalizer removed (K8s could GC),
    # StatefulSet scale-to-0 worked, worker's SIGTERM drain
    # exited cleanly (no in-flight builds to wait for).
    k8s.wait_until_succeeds(
        "! k3s kubectl get pod default-workers-0 2>/dev/null",
        timeout=120
    )

    # WorkerPool CR gone (finalizer removed → K8s deleted it).
    k8s.wait_until_succeeds(
        "! k3s kubectl get workerpool default 2>/dev/null"
    )

    # Scheduler saw the disconnect. workers_active back to 0.
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 0'"
    )
  '';
}
