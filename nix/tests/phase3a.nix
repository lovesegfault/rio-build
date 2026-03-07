# Phase 3a milestone validation: k3s operator end-to-end.
#
# Milestone (docs/src/phases/phase3a.md):
#   WorkerPool CRD reconciles to a running worker pod that
#   registers with the scheduler and completes a 2-derivation
#   build chain. FUSE works inside the pod. PrefetchHint fires
#   (parent's dispatch → child's output path). cgroup v2
#   memory.peak → build_history.ema_peak_memory_bytes end-to-end
#   (the phase2c VmHWM fix, verified via psql). Finalizer drain.
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

  # 2-node chain: child → parent. Parent's dispatch triggers
  # send_prefetch_hint with child's output path (approx_input_closure
  # reads DAG children's expected_output_paths — assignment.rs:280).
  # Both have pname in env so build_history gets 2 rows — end-to-end
  # proof that cgroup memory.peak → CompletionReport → scheduler's
  # db.update_build_history (the phase3a headline deliverable).
  #
  # sleep 2: cgroup cpu.stat poll is 1Hz (executor/mod.rs:456).
  # Build exits <1s → peak_cpu_cores stays 0 → completion.rs:202
  # guard skips the write. 2s guarantees ≥1 sample.
  #
  # Escaping: ''${x} → ${x} in the WRITTEN file (inner .nix reads
  # its own let-bound child, its own busybox arg). ''' → '' (the
  # inner .nix's own indented string for builder args).
  # G3 autoscaler exercise: 5 independent derivations for queue
  # pressure. All leaves (no deps) → all go Ready immediately.
  # With maxConcurrentBuilds=1, 1 runs + 4 queue → queued=4.
  # compute_desired(4, target=2) = ceil(4/2) = 2 → scale to 2.
  #
  # sleep 15: enough for ~3 autoscaler poll cycles (3s each) to
  # see sustained queue pressure. Each drv has a unique name
  # so they're distinct derivations (nix-build would dedupe
  # identical ones otherwise).
  autoscaleDrvFile = pkgs.writeText "phase3a-autoscale.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mk = n: derivation {
        name = "rio-3a-queue-''${toString n}";
        pname = "rio-3a-queue-''${toString n}";
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" '''
          ''${bb} mkdir -p $out
          ''${bb} echo "queue pressure ''${toString n}" > $out/stamp
          ''${bb} sleep 15
        ''' ];
      };
    in {
      d1 = mk 1;
      d2 = mk 2;
      d3 = mk 3;
      d4 = mk 4;
      d5 = mk 5;
    }
  '';

  testDrvFile = pkgs.writeText "phase3a-derivation.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      child = derivation {
        name = "rio-3a-child";
        pname = "rio-3a-child";  # gateway reads env.pname (translate.rs:257)
        system = builtins.currentSystem;
        builder = sh;
        args = [ "-c" '''
          ''${bb} mkdir -p $out
          ''${bb} echo "phase3a child" > $out/stamp
          ''${bb} sleep 2
        ''' ];
      };
    in derivation {
      name = "rio-3a-parent";
      pname = "rio-3a-parent";
      system = builtins.currentSystem;
      builder = sh;
      args = [ "-c" '''
        ''${bb} mkdir -p $out
        ''${bb} cat ''${child}/stamp > $out/stamp
        ''${bb} echo "phase3a parent: built in a k8s pod" >> $out/stamp
        ''${bb} sleep 2
      ''' ];
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
  # 900s = 15min: k3s startup (~60s) + airgap import (~30s) +
  # pod scheduling + FUSE mount (~30s) + build (~30s) + Build CRD
  # exercise (~20s) + autoscaler exercise (~90s, 5×15s builds) +
  # drain (~30s) = ~5min happy path. 15min gives 3× headroom.
  globalTimeout = 900;

  nodes = {
    control = common.mkControlNode {
      hostName = "control";
      memorySize = 1536;
      # 9091 = scheduler metrics (scraped from k8s node in assertions).
      extraFirewallPorts = [ 9091 ];
      # Short tick — faster dispatch retry after worker registers.
      extraSchedulerConfig = {
        tickIntervalSecs = 2;
        # H1 lease smoke: scheduler talks to k3s apiserver for
        # leader election. kubeconfig is copied from k8s VM at
        # test time (see testScript). The scheduler starts in
        # standby (lease loop's kube client init fails → graceful
        # return, is_leader stays false → no dispatch). After
        # the test copies kubeconfig + restarts scheduler, it
        # acquires the lease and starts dispatching.
        #
        # This tests the OUT-OF-CLUSTER kubeconfig path
        # (KUBECONFIG env var) which is kube::Client::try_default's
        # fallback after in-cluster config fails — less tested
        # than the in-cluster path (no ServiceAccount here).
        lease = {
          name = "rio-scheduler-lease";
          namespace = "default";
          kubeconfigPath = "/etc/kube/config";
        };
      };
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
            # H1 lease smoke: scheduler on `control` VM needs to
            # reach this apiserver as https://k8s:6443. The
            # self-signed cert must include `k8s` as a SAN, or
            # the kube-rs client's TLS verification rejects it.
            # Without this: "tls: certificate is not valid for
            # k8s, only 127.0.0.1, localhost, ...".
            "--tls-san"
            "k8s"
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
            # G3 autoscaler exercise: 3s windows instead of 30s.
            # Defaults (30s poll × 30s up-window × 30s anti-flap)
            # would mean ~60s to first scale — too long for a VM
            # test. 3s each → ~9s, well within our queue-pressure
            # window (5 builds × 15s sleep = ~75s total).
            # Scale-DOWN stays at 600s (not configurable — see
            # scaling.rs SCALE_DOWN_WINDOW comment).
            RIO_AUTOSCALER_POLL_SECS = "3";
            RIO_AUTOSCALER_SCALE_UP_WINDOW_SECS = "3";
            RIO_AUTOSCALER_MIN_INTERVAL_SECS = "3";
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

    # ── Lease leader election smoke (H1) ───────────────────────────
    # The scheduler on `control` has lease config pointing at
    # /etc/kube/config (which doesn't exist yet). On startup, the
    # lease loop's kube client init failed → loop returned →
    # is_leader stays FALSE → dispatch_ready() returns early →
    # STANDBY. The scheduler merges DAGs (state warm for takeover)
    # but never dispatches.
    #
    # Now: copy kubeconfig from k8s VM, rewrite the server URL
    # (kubeconfig has 127.0.0.1, we need k8s:6443), restart
    # scheduler. The lease loop's kube client init succeeds →
    # try_acquire_or_renew → LeaseLockResult::Acquired →
    # generation increment + is_leader=true → dispatch enabled.
    #
    # This tests the OUT-OF-CLUSTER path (KUBECONFIG env, no
    # ServiceAccount). In-cluster is kube::Client::try_default's
    # first try; this is the fallback, less commonly tested.
    #
    # --tls-san k8s (set in k3s extraFlags) makes the apiserver
    # cert valid for hostname `k8s`, avoiding "certificate not
    # valid for k8s" TLS errors.

    # Copy + rewrite. Python string manipulation is cleaner than
    # sed in a heredoc. k8s.succeed returns stdout; we pass it to
    # control.succeed via stdin on a cat.
    kubeconfig = k8s.succeed("cat /etc/rancher/k3s/k3s.yaml")
    kubeconfig = kubeconfig.replace("127.0.0.1", "k8s")
    control.succeed("mkdir -p /etc/kube")
    # NOT an f-string: kubeconfig YAML may contain {} (empty maps)
    # which Python's f-string would mangle. str concat + heredoc-
    # with-quoted-delimiter ('EOF' = no shell expansion inside).
    control.succeed(
        "cat > /etc/kube/config << 'EOF'\n" + kubeconfig + "\nEOF"
    )
    control.succeed("chmod 600 /etc/kube/config")

    # Restart scheduler to pick up the now-existing kubeconfig.
    # The lease loop runs on a 5s renew interval; acquisition
    # happens on the first tick after kube client init succeeds.
    control.succeed("systemctl restart rio-scheduler")
    control.wait_for_unit("rio-scheduler.service")

    # Poll the lease object. holderIdentity should be "control"
    # (HOSTNAME env set by scheduler.nix from networking.hostName).
    # The lease object is CREATED on first acquire — if absent,
    # the lease loop hasn't succeeded yet (kube client init failed
    # or still waiting for first 5s tick).
    #
    # 30s timeout: lease loop ticks every 5s (RENEW_INTERVAL);
    # first tick after restart creates the lease. ~5-10s normally.
    k8s.wait_until_succeeds(
        "k3s kubectl get lease rio-scheduler-lease -n default "
        "-o jsonpath='{.spec.holderIdentity}' | grep -q control",
        timeout=30
    )

    # Metric: H1a added rio_scheduler_lease_acquired_total. This
    # proves the ACQUIRE TRANSITION ran (not just "lease exists"
    # — the scheduler might have restarted between create and
    # this check, losing the counter). Value ≥1 = at least one
    # acquire transition this process lifetime.
    #
    # lease_acquired → is_leader=true → dispatch_ready() unblocked.
    # No separate readiness check needed — acquired implies ready.
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E '^rio_scheduler_lease_acquired_total [1-9]'",
        timeout=15
    )

    # Gateway needs to reconnect after scheduler restart (its
    # gRPC channel to SchedulerService drops on scheduler exit).
    # wait_for_open_port confirms scheduler is accepting
    # connections again; gateway retries automatically.
    control.wait_for_open_port(9001)

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

    try:
        # ── PrefetchHint fired (B3) ───────────────────────────────
        # Parent has 1 DAG child (rio-3a-child). When parent
        # dispatches, approx_input_closure(dag, parent) returns
        # [child's output path]. send_prefetch_hint checks
        # worker's bloom — worker is cold (first build), bloom
        # miss → hint sent. Assert ≥1 (not just registered).
        #
        # Prometheus counters only appear in /metrics AFTER first
        # increment (describe_counter! just attaches help text).
        # So grep >= 1 is the only form that works.
        control.succeed(
            "curl -sf http://localhost:9091/metrics | "
            "grep -E 'rio_scheduler_prefetch_hints_sent_total [1-9]'"
        )

        # ── cgroup v2 per-build: the phase3a headline ─────────────
        # main.rs:289 info! after delegated_root() +
        # enable_subtree_controllers() both succeed. Fails fast
        # (worker exits) if cgroup v2 unavailable or delegation
        # broken. Pod-Ready already proves it at the coarse level;
        # this grep proves the CODEPATH (no silent hard-requirement
        # regression where Ready passes for wrong reasons).
        #
        # Note on de1cb87's "cgroup namespace root" codepath: that
        # fires when /proc/self/cgroup shows 0::/ (containerd cgroupns).
        # With privileged=true, containerd DOESN'T namespace cgroups
        # — we see the host's kubepods.slice/... path. The ns-root
        # fix matters for non-privileged production pods (granular
        # caps); we can't exercise it here (k3s seccomp blocks
        # mount(2) without privileged). "subtree ready" fires for
        # BOTH codepaths — it's the unified positive signal.
        #
        # NOT grep -q: `-q` exits on first match → kubectl logs
        # still writing → SIGPIPE → exit 141 → pipefail fails
        # the whole pipeline. Plain grep reads ALL input first.
        k8s.succeed(
            "k3s kubectl logs default-workers-0 | "
            "grep 'cgroup v2 subtree ready' >/dev/null"
        )

        # ── cgroup memory.peak → build_history (end-to-end) ───────
        # THE FIX for phase2c VmHWM bug: daemon.id() was nix-daemon's
        # PID, measured ~10MB. cgroup memory.peak captures the WHOLE
        # TREE (daemon + builder + every compiler subprocess).
        #
        # Chain: BuildCgroup.memory_peak() → ExecutionResult →
        # CompletionReport → scheduler handle_completion (filters
        # 0 → None at completion.rs:200) → db.update_build_history
        # COALESCE blend (db.rs:445; first sample → just $new).
        #
        # Both derivations have pname in env → 2 rows keyed on
        # (pname, system). Even a trivial busybox build + sleep
        # has ~3-10MB tree RSS (daemon alone).
        #
        # psql -tA = tuples-only, unaligned. One value per line.
        # NULL → empty (not "NULL"). grep matches ≥7 digits = ≥1MB.
        #
        # Assert >=1 not =2: completion.rs:181 guards on
        # state.pname.is_some(). The gateway extracts pname from the
        # ROOT derivation's env (the one nix-build directly targets);
        # intermediate deps' pname may be None depending on how the
        # DAG is walked. One row with ≥1MB still proves the full
        # cgroup → CompletionReport → DB chain for at least one build,
        # which IS the phase3a deliverable. (Observed value ~14MB —
        # daemon + builder + sleep process tree.)
        #
        # wait_until_succeeds: small window between client-sees-built
        # and actor-DB-commit. 10s is overkill but costs nothing.
        control.wait_until_succeeds(
            "sudo -u postgres psql rio -tA -c "
            "\"SELECT ema_peak_memory_bytes::bigint FROM build_history "
            "WHERE pname IN ('rio-3a-child','rio-3a-parent')\" | "
            "grep -qE '^[0-9]{7,}$'",
            timeout=10
        )

        # ── cgroup cpu.stat → build_history (G2) ──────────────────
        # The derivation sleeps 2s specifically so the 1Hz CPU poll
        # (executor/mod.rs:456) fires at least once. Previously only
        # memory was asserted — the CPU chain (poll → peak_cpu_cores
        # → CompletionReport → COALESCE blend → ema_peak_cpu_cores)
        # was never end-to-end tested. `> 0` (not =0): sleep uses
        # negligible CPU but the poll baseline captures daemon
        # overhead; a value > 0 proves the chain ran. Non-null is
        # the real signal (completion.rs:202 filters 0 → None).
        #
        # NUMERIC::text not DOUBLE PRECISION: psql's -tA prints
        # floats with full precision (e.g., "0.00123456") which
        # the regex matches; NULL → empty line, fails grep.
        control.wait_until_succeeds(
            "sudo -u postgres psql rio -tA -c "
            "\"SELECT ema_peak_cpu_cores FROM build_history "
            "WHERE pname IN ('rio-3a-child','rio-3a-parent') "
            "AND ema_peak_cpu_cores IS NOT NULL AND ema_peak_cpu_cores > 0\" | "
            "grep -qE '^[0-9]'",
            timeout=10
        )
    except Exception:
        dump_logs()
        raise

    # Worker metric: BOTH builds completed successfully INSIDE the
    # pod. With hostNetwork=true, the pod's :9093 metrics port is
    # on the NODE's IP. Scrape from the k8s VM itself (localhost).
    # [2-9] not [1-9]: we built 2 derivations (child + parent).
    k8s.succeed(
        "curl -sf http://localhost:9093/metrics | "
        "grep -E 'rio_worker_builds_total\\{outcome=\"success\"\\} [2-9]'"
    )

    # Output queryable via ssh-ng (round-trips through rio-store).
    client.succeed(f"nix path-info --store 'ssh-ng://control' {out}")

    # ── Build CRD reconciler exercise (G4) ─────────────────────────
    # The ssh-ng build above put both .drv files in rio-store. Now
    # submit a build via the K8s-native path: kubectl apply a Build
    # CR pointing at the parent .drv. The reconciler's apply() should:
    #   1. Fetch .drv from rio-store (fetch_and_build_node)
    #   2. Submit single-node DAG (outputs already built → Cached)
    #   3. Spawn drain_stream watch task
    #   4. Patch status with sentinel first, then real build_id
    #
    # This is the FIRST time the Build reconciler has run against a
    # real apiserver. Previously only unit-tested (4 pure-fn tests).
    # Validates B1's fix: sentinel patch before drain_stream spawn.
    #
    # nix-instantiate on client gives us the .drv path. The .drv is
    # ALREADY in rio-store (nix-build copied the closure). The
    # derivation outputs are also there (just built) → scheduler's
    # FindMissingPaths sees them → instant Cached phase.
    drv_path = client.succeed(
        "nix-instantiate "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${testDrvFile} 2>/dev/null"
    ).strip()
    print(f"Build CRD .drv: {drv_path}")
    assert drv_path.endswith(".drv"), f"not a drv path: {drv_path}"

    # heredoc → kubectl apply -f -. Build CR spec is minimal:
    # just derivation + priority. tenant/timeout optional.
    # Double-braces in f-string escape to literal braces for JSON.
    k8s.succeed(
        "k3s kubectl apply -f - <<'EOF'\n"
        "apiVersion: rio.build/v1alpha1\n"
        "kind: Build\n"
        "metadata:\n"
        "  name: test-crd-build\n"
        "  namespace: default\n"
        "spec:\n"
        f"  derivation: {drv_path}\n"
        "  priority: 0\n"
        "EOF"
    )

    # Poll for terminal phase. Outputs already in store → scheduler
    # sees empty FindMissingPaths → Cached (or Completed if the
    # scheduler's cache-check is async and the build re-runs —
    # either is success). 30s is generous for an already-cached
    # build; the reconciler's apply() runs in <1s once CRD
    # established.
    k8s.wait_until_succeeds(
        "k3s kubectl get build test-crd-build "
        "-o jsonpath='{.status.phase}' | "
        "grep -E '^(Completed|Cached|Succeeded)$'",
        timeout=30
    )

    # build_id should be a UUID (not empty, not "submitted"). This
    # validates B1's fix: if the sentinel patch ran AFTER the watch
    # task's first patch, build_id would be stuck at "submitted".
    # The regex checks for UUID-like shape (8-4-4-4-12 hex).
    build_id = k8s.succeed(
        "k3s kubectl get build test-crd-build "
        "-o jsonpath='{.status.buildId}'"
    ).strip()
    print(f"Build CRD build_id: {build_id}")
    assert build_id != "" and build_id != "submitted", \
        f"build_id stuck at sentinel/empty (B1 race?): {build_id!r}"
    import re
    assert re.match(r'^[0-9a-f]{8}-[0-9a-f]{4}-', build_id), \
        f"build_id not UUID-like: {build_id!r}"

    # Delete → finalizer's cleanup() runs → CancelBuild (NotFound
    # OK since already terminal) → finalizer removed → CR gone.
    # Tests the finalizer flow without blocking on an in-flight
    # build (already terminal = instant cleanup).
    k8s.succeed("k3s kubectl delete build test-crd-build --wait=false")
    k8s.wait_until_succeeds(
        "! k3s kubectl get build test-crd-build 2>/dev/null",
        timeout=15
    )

    # ── Reconciler preserves autoscaler replicas (regression) ──────
    # The reconciler was reverting STS.spec.replicas to min on every
    # reconcile (SSA with .force() re-claimed the field). Simulate
    # the autoscaler by scaling STS directly — the .owns() watch
    # triggers a reconcile. If the bug is present, replicas reverts
    # to 1 within ~5s. If fixed, it stays at 2.
    #
    # We don't wait for pod-1 to be Ready (4GB VM is tight) — just
    # assert the reconciler didn't stomp on spec.replicas. The
    # autoscaler's actual 30s+30s stabilization loop is unit-tested
    # in scaling.rs; this proves the SSA field-ownership handoff.
    k8s.succeed("k3s kubectl scale statefulset default-workers --replicas=2")
    # Wait for the reconciler to observe the change (triggered by
    # .owns() watch on STS). desiredReplicas=2 proves the reconcile
    # RAN (it reads STS.spec.replicas and patches WorkerPool.status).
    # Once this succeeds, the reconciler has had its chance to stomp
    # on STS.spec.replicas — the followup assertion checks it didn't.
    #
    # This ordering replaces the previous `time.sleep(5)` hack:
    # before, we slept hoping the reconciler ran in that window,
    # then asserted replicas=2. Now we POSITIVELY wait for the
    # reconciler's own status patch, then assert replicas. No
    # timing race; the negative assertion (replicas STILL 2) is
    # reliable once we've seen the positive signal (reconciler ran).
    k8s.wait_until_succeeds(
        "test \"$(k3s kubectl get workerpool default "
        "-o jsonpath='{.status.desiredReplicas}')\" = 2",
        timeout=15
    )
    # Reconciler ran + updated status. Now assert it did NOT revert
    # STS.spec.replicas back to min (the regression). This is a
    # synchronous check — if the reconciler were going to stomp,
    # it would have done so in the reconcile that patched status.
    k8s.succeed(
        "test \"$(k3s kubectl get statefulset default-workers "
        "-o jsonpath='{.spec.replicas}')\" = 2"
    )

    # Reset replicas to 1 for the autoscaler test below — we want
    # to observe 1→2, not 2→2 (no-op). The reconciler will update
    # desiredReplicas back to 1 as well.
    k8s.succeed("k3s kubectl scale statefulset default-workers --replicas=1")
    k8s.wait_until_succeeds(
        "test \"$(k3s kubectl get workerpool default "
        "-o jsonpath='{.status.desiredReplicas}')\" = 1",
        timeout=15
    )

    # ── Autoscaler real poll loop (G3) ─────────────────────────────
    # FIRST TIME the autoscaler's scale_one() runs against a real
    # apiserver. Previously: unit-tested only (scaling.rs has 10
    # tests). The kubectl-scale test above proved the reconciler
    # doesn't STOMP autoscaler replicas, but the autoscaler ITSELF
    # never patched anything.
    #
    # Controller env (set above in systemd.services.rio-controller)
    # overrides timing to 3s: poll/up-window/min-interval. With
    # defaults (30s each), first scale would take ~60s — too slow.
    # 3s each → first scale in ~6-9s after sustained queue pressure.
    #
    # This proves A1's fix (apiVersion+kind in SSA patch body) AND
    # C2's fix (autoscaler patches WorkerPool.status.lastScaleTime).
    # Without A1, the STS patch would 400 silently (warn log only).
    #
    # Flow:
    #   1. Launch 5 builds in background (all leaves, all Ready)
    #   2. With maxConcurrentBuilds=1, queued_derivations stabilizes
    #      at ~4 while 1 runs (each sleep 15s → ~75s total runtime)
    #   3. compute_desired(queued=4, target=2) = ceil(4/2) = 2
    #   4. Autoscaler polls (3s), sees desired=2, waits 3s
    #      up-window, passes 3s anti-flap → patches STS replicas=2
    #   5. Autoscaler also patches WorkerPool.status.lastScaleTime
    #      + conditions[Scaling=True, reason=ScaledUp]

    # Launch 5 builds. nix-build with all 5 attrs in one command
    # submits one DAG with 5 leaf nodes (no edges — all independent).
    # Background it: the SCRIPT continues to poll metrics while
    # builds run. `&` + `sleep 1` (let nix-build start before we
    # poll); we'll `wait` for it later.
    #
    # NIX_CONFIG: nix-build on the client VM needs experimental
    # features just like the earlier build() calls (common.nix
    # handles this for build()). Also need --store to route via
    # ssh-ng (builds execute on the pod, not the client).
    client.execute(
        "NIX_CONFIG='experimental-features = nix-command' "
        "nix-build --no-out-link "
        "--store 'ssh-ng://control' "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${autoscaleDrvFile} -A d1 -A d2 -A d3 -A d4 -A d5 "
        "> /tmp/autoscale-build.log 2>&1 &"
    )

    # Wait for queue depth to build up. rio_scheduler_derivations_
    # queued gauge (set on Tick, actor/worker.rs) shows Ready-but-
    # not-dispatched count. With 1 worker × 1 max_builds, the 5
    # builds queue: 1 Running, 4 Queued. We wait for ≥3 (not
    # exactly 4 — the first dispatch may happen between our poll
    # and the gauge update, so we'd see 3 briefly). ≥3 is enough
    # to trigger compute_desired=2.
    #
    # The gauge is set on Tick (every tickIntervalSecs=2s), so
    # there's up to 2s latency between merge and metric update.
    # The metric might show 0 if nothing has queued before — `||
    # echo 0` handles the grep-miss case.
    control.wait_until_succeeds(
        "q=$(curl -sf http://localhost:9091/metrics | "
        "grep -E '^rio_scheduler_derivations_queued ' | "
        "awk '{print $2}' || echo 0); "
        "test \"$q\" -ge 3",
        timeout=20
    )

    # THE BIG ONE: autoscaler patches STS replicas 1→2. This is
    # the A1 SSA fix proof. Previously: patch body {spec:{replicas:
    # 2}} → apiserver 400 "apiVersion must be set" → warn log →
    # autoscaler silently never scaled. Now: body has apiVersion+
    # kind → patch succeeds.
    #
    # Timing: 3s poll + 3s up-window + anti-flap (already past
    # due to earlier scale) = ~6-12s to first patch. 45s timeout
    # gives headroom for slow VM but fails fast if broken.
    k8s.wait_until_succeeds(
        "test \"$(k3s kubectl get statefulset default-workers "
        "-o jsonpath='{.spec.replicas}')\" = 2",
        timeout=45
    )

    # C2 proof: autoscaler patched WorkerPool.status.lastScaleTime.
    # Previously written as None on every reconcile (clobbered).
    # Now owned by autoscaler's separate SSA field-manager.
    # Non-empty string = set (K8s serializes Time as RFC3339).
    k8s.wait_until_succeeds(
        "sc=$(k3s kubectl get workerpool default "
        "-o jsonpath='{.status.lastScaleTime}'); "
        "test -n \"$sc\"",
        timeout=15
    )

    # Scaling conditions: reason=ScaledUp, status=True. C2 adds
    # these to explain WHY replicas changed.
    k8s.succeed(
        "k3s kubectl get workerpool default "
        "-o jsonpath='{.status.conditions[?(@.type==\"Scaling\")].reason}' | "
        "grep -q 'ScaledUp'"
    )

    # Existing metric (scaling.rs:257) incremented. Proves the
    # scale_one() success path ran to completion (not just the
    # patch — the info! log + metric increment come after).
    k8s.succeed(
        "curl -sf http://localhost:9094/metrics | "
        "grep -E 'rio_controller_scaling_decisions_total\\{direction=\"up\"\\} [1-9]'"
    )

    # Wait for background builds to finish (so drain doesn't wait
    # for them). 5 × 15s = 75s sequential; with 2 workers (scaled!)
    # and 1 max_builds each, ~40s parallel. 120s timeout is safe.
    #
    # `client.succeed("wait")` waits for the backgrounded
    # nix-build. The builds MAY fail (pod-1 not Ready yet — we
    # didn't wait for it) but that's OK: the autoscaler scaled on
    # QUEUE DEPTH, which is what we're testing. `|| true` swallows
    # build failures (they'd be infrastructure failures from the
    # not-ready pod, not our bug).
    client.succeed("wait || true")

    # ── Finalizer drain ────────────────────────────────────────────
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
