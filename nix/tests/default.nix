# Wires fixtures × scenarios into the vmTests attrset.
#
# Returns `{ vm-<scenario>-<fixture> = <runNixOSTest>; }`. Coverage mode:
# same structure, `coverage=true` propagates to fixtures → covEnv in
# service envs → collectCoverage fires.
{
  pkgs,
  rio-workspace,
  rioModules,
  dockerImages,
  nixhelm,
  system,
  coverage ? false,
}:
let
  common = import ./common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  standalone = import ./fixtures/standalone.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  toxiproxy = import ./fixtures/toxiproxy.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  k3sFull = import ./fixtures/k3s-full.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      dockerImages
      nixhelm
      system
      coverage
      ;
  };

  protocol = import ./scenarios/protocol.nix;
  scheduling = import ./scenarios/scheduling.nix;
  # security exports { standalone, privileged-hardening-e2e } — two
  # scenario functions sharing the same file. standalone uses the
  # systemd fixture (mTLS/HMAC/tenant/validation); e2e uses k3sFull
  # with the nonpriv values overlay (device-plugin + cgroup remount).
  security = import ./scenarios/security.nix { inherit pkgs common; };
  observability = import ./scenarios/observability.nix;
  lifecycle = import ./scenarios/lifecycle.nix;
  leader-election = import ./scenarios/leader-election.nix;
  cli = import ./scenarios/cli.nix;
  dashboard-gateway = import ./scenarios/dashboard-gateway.nix;
  fod-proxy = import ./scenarios/fod-proxy.nix;
  netpol = import ./scenarios/netpol.nix;
  chaos = import ./scenarios/chaos.nix;
  drvs = import ./lib/derivations.nix { inherit pkgs; };
  pulled = import ../docker-pulled.nix { inherit pkgs; };

  # Shared fixture for both scheduling splits — identical VM topology.
  schedulingFixture = standalone {
    workers = {
      wsmall1 = {
        maxBuilds = 2;
        sizeClass = "small";
      };
      wsmall2 = {
        maxBuilds = 2;
        sizeClass = "small";
        # Non-passthrough FUSE: exercises open_files tracking,
        # userspace read(), release(). fuse/ops.rs read() at 33%
        # coverage before this — passthrough bypasses the kernel
        # callback entirely.
        extraServiceEnv = {
          RIO_FUSE_PASSTHROUGH = "false";
        };
      };
      wlarge = {
        maxBuilds = 1;
        sizeClass = "large";
        # maxSilentTime enforcement. Only wlarge: small workers run
        # reassignDrv (25s silent sleep) which would trip a silence
        # threshold. bigthing (the only other drv routed to wlarge)
        # echoes immediately — no silence exposure. The max-silent-time
        # subtest routes silenceDrv here via build_history seed.
        #
        # Worker-side config because the Nix ssh-ng client does NOT
        # send wopSetOptions (protocol 1.38) — client --max-silent-time
        # cannot propagate to the gateway.
        extraServiceEnv = {
          RIO_MAX_SILENT_TIME_SECS = "10";
        };
      };
    };
    extraSchedulerConfig = {
      tickIntervalSecs = 2;
      extraConfig = ''
        [[size_classes]]
        name = "small"
        cutoff_secs = 30.0
        mem_limit_bytes = 17179869184

        [[size_classes]]
        name = "large"
        cutoff_secs = 3600.0
        mem_limit_bytes = 68719476736
      '';
    };
    extraStoreConfig = {
      extraConfig = ''
        [chunk_backend]
        kind = "filesystem"
        base_dir = "/var/lib/rio/store/chunks"
      '';
    };
    # grpcurl: cancel-timing submits + cancels via plaintext gRPC :9001
    # (no withPki → no mTLS). ssh-ng:// doesn't surface build_id to the
    # client, and client-disconnect mid-wopBuildDerivation doesn't fire
    # session.rs's EOF-cancel path (handler/build.rs:462 removes the
    # build_id before bubbling). gRPC SubmitBuild + CancelBuild is the
    # only deterministic cancel-a-running-build path in this fixture.
    extraPackages = [
      pkgs.postgresql
      pkgs.grpcurl
    ];
  };

  # Shared lifecycle module for core/ctrlrestart/recovery/reconnect.
  # Plain k3sFull — no autoscaler env tuning. The autoscaler's default
  # poll/windows (~30s/600s) make it effectively inert in these splits.
  #
  # jwtEnabled: mounts the rio-jwt-pubkey ConfigMap into scheduler+store
  # and the rio-jwt-signing Secret into gateway (lib/jwt-keys.nix fixed
  # test keypair). The interceptor is DUAL-MODE (header-absent → pass-
  # through), so enabling it doesn't break existing lifecycle subtests
  # that call gRPC without the x-rio-tenant-token header. Turned on here
  # for jwt-mount-present; recovery+autoscale don't need it but the
  # shared module means they get it too (harmless — just an extra
  # ConfigMap mount).
  lifecycleMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull { jwtEnabled = true; };
  };

  # Autoscale split gets the fast-poll env so the scale-up/down cycle
  # completes within the test window. Same chart/images; only the
  # controller pod's env ConfigMap differs.
  lifecycleAutoscaleMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull {
      extraValues = {
        "controller.extraEnv[0].name" = "RIO_AUTOSCALER_POLL_SECS";
        "controller.extraEnv[0].value" = "3";
        "controller.extraEnv[1].name" = "RIO_AUTOSCALER_SCALE_UP_WINDOW_SECS";
        "controller.extraEnv[1].value" = "3";
        "controller.extraEnv[2].name" = "RIO_AUTOSCALER_MIN_INTERVAL_SECS";
        "controller.extraEnv[2].value" = "3";
        "controller.extraEnv[3].name" = "RIO_AUTOSCALER_SCALE_DOWN_WINDOW_SECS";
        "controller.extraEnv[3].value" = "10";
      };
    };
  };

  leMod = leader-election {
    inherit pkgs common;
    fixture = k3sFull { };
  };
in
{
  # ── New scenario-based tests ────────────────────────────────────────
  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
    };
    cold = false;
  };

  vm-protocol-cold-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
      # Python http.server on :8000 serving the pre-fetched busybox.
      # cold-bootstrap.nix's url is overridden to http://client:8000/
      # busybox — builtin:fetchurl gets a real HTTP fetch (same codepath
      # as EKS) without needing internet egress.
      extraClientModules = [ drvs.coldBootstrapServer ];
    };
    cold = true;
  };

  # ── scheduling splits (2 tests, standalone fixture) ──────────────────
  # Same 3-worker fixture (wsmall1/wsmall2/wlarge + size-classes) for
  # both — the fragment architecture changes what RUNS, not what's BOOTED.
  # fanout→fuse-direct→fuse-slowpath cache-state chain stays in core;
  # sizeclass+reassign are disruptive (psql seed, SIGKILL) → own test.
  vm-scheduling-core-standalone =
    (scheduling {
      inherit pkgs common;
      fixture = schedulingFixture;
    }).mkTest
      {
        name = "core";
        subtests = [
          # r[verify worker.overlay.stacked-lower]
          # r[verify worker.ns.order]
          # r[verify worker.fuse.lookup-caches]
          "fanout"
          "fuse-direct"
          "overlay-readdir"
          # r[verify store.inline.threshold]
          # r[verify obs.metric.transfer-volume]
          "chunks"
          "cgroup"
          "fuse-slowpath"
        ];
      };

  vm-scheduling-disrupt-standalone =
    (scheduling {
      inherit pkgs common;
      fixture = schedulingFixture;
    }).mkTest
      {
        name = "disrupt";
        subtests = [
          "sizeclass"
          # r[verify worker.silence.timeout-kill]
          "max-silent-time"
          # r[verify gw.opcode.set-options.propagation+2]
          # setoptions-unreachable greps ALL gateway journal history —
          # placed after sizeclass + max-silent-time so it also covers
          # THEIR ssh-ng sessions (neither passed --option, but the
          # handshake's virtual setOptions() call runs regardless).
          "setoptions-unreachable"
          "cancel-timing"
          "reassign"
          # r[verify worker.shutdown.sigint]
          # sigint-graceful AFTER reassign: reassign already disturbs a
          # worker (SIGKILL + wait_for_unit restart); sigint is the
          # gentler sibling. Uses wsmall2 only — no cache-chain coupling.
          # ~35s: SIGINT + 30s inactive-wait + restart + FUSE remount.
          "sigint-graceful"
          # r[verify obs.metric.scheduler]
          # r[verify obs.metric.worker]
          # r[verify obs.metric.store]
          "load-50drv"
        ];
        # Default 600s is tight now: sizeclass ~30s + max-silent-time
        # ~25s + cancel-timing ~40s + reassign ~60s + sigint ~35s +
        # load-50drv ~60s ≈ 250s subtests + ~120s boot. load-50drv under
        # TCG could stretch to 150s (13 waves × tick=2s × TCG overhead).
        # 900s is comfortable without being an open-ended escape hatch.
        globalTimeout = 900;
      };

  # r[verify gw.jwt.dual-mode]
  # r[verify sec.boundary.grpc-hmac]
  # r[verify store.tenant.narinfo-filter]
  # r[verify gw.reject.nochroot]
  # r[verify gw.rate.per-tenant]
  # r[verify store.gc.tenant-quota-enforce]
  #   Single-test scenario (no subtests list). Markers at the wiring
  #   point per P0341 convention — scenario header prose explains which
  #   subtest proves each rule.
  vm-security-standalone = security.standalone {
    fixture = standalone {
      workers = {
        worker = {
          maxBuilds = 1;
        };
      };
      withPki = true;
      # cache-auth subtest curls /{hash}.narinfo + /nix-cache-info.
      # Store's cache_http_addr defaults to None → HTTP server not
      # spawned. cache_allow_unauthenticated stays at its default
      # (false) — the Bearer-required path is what we're testing.
      extraStoreConfig = {
        cacheHttpAddr = "0.0.0.0:8080";
      };
      extraPackages = [
        pkgs.grpcurl
        pkgs.grpc-health-probe
        pkgs.postgresql
      ];
    };
  };

  # r[verify sec.pod.fuse-device-plugin]
  # r[verify sec.pod.host-users-false]
  # r[verify worker.cgroup.ns-root-remount]
  #   Non-privileged + device-plugin VM e2e. Every other k3s fixture
  #   uses vmtest-full.yaml privileged:true (containerd mounts
  #   /sys/fs/cgroup rw already, hostPath /dev/fuse works) — the
  #   rw-remount and device-plugin paths were never exercised until
  #   this scenario. builders.rs unit tests prove pod SHAPE renders
  #   correctly; this proves it WORKS (DS Ready → extended resource
  #   advertised → worker pod Ready with hostUsers:false → cgroup/leaf
  #   exists + subtree_control writable → build completes over FUSE).
  vm-security-nonpriv-k3s = security.privileged-hardening-e2e {
    fixture = k3sFull {
      # Layer vmtest-full-nonpriv.yaml on top of the base vmtest-full.
      # yaml — flips workerPool.privileged:false + devicePlugin.enabled
      # + overrides devicePlugin.image to match docker-pulled.nix.
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml
      ];
      # smarter-device-manager is NOT in the default airgap set
      # (only bitnami-postgresql + rio-all). Without this, the DS pod
      # goes ImagePullBackOff (airgapped, no pull).
      extraImages = [ pulled.smarter-device-manager ];
    };
  };

  # ── chaos (toxiproxy fault injection, standalone topology) ──────────
  # 4 subtests: latency/reset/partition/bandwidth. The toxiproxy fixture
  # is standalone + a proxy systemd unit on control; see fixtures/
  # toxiproxy.nix for why not a separate VM (scheduler connect_store
  # boot race). ~4-5min.
  vm-chaos-standalone = chaos {
    inherit pkgs common;
    fixture = toxiproxy { };
  };

  # r[verify obs.metric.gateway]
  #   EXPECTED_METRICS[(gateway,9090)] asserts rio_gateway_* metric
  #   names after a build — presence proves describe_*! wiring AND
  #   actual increments (metrics-rs registers on first increment).
  # r[verify obs.trace.scheduler-id-in-metadata]
  #   trace-id-propagation subtest: STDERR_NEXT `rio trace_id:` line
  #   is the scheduler's x-rio-trace-id (not the gateway's own span);
  #   asserted to appear on both scheduler AND worker spans in the
  #   collector file — proves the header carries the USEFUL id.
  # r[verify sched.trace.assignment-traceparent]
  #   span_from_traceparent parenting-vs-link observation: checks
  #   whether the worker build-executor span's parentSpanId matches a
  #   scheduler spanId. The PRECONDITION (both services share the
  #   emitted trace_id) proves the WorkAssignment.traceparent data-carry
  #   delivers context end to end. Resolves the doc's open question.
  vm-observability-standalone = observability {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker1 = {
          maxBuilds = 1;
        };
        worker2 = {
          maxBuilds = 1;
        };
        worker3 = {
          maxBuilds = 1;
        };
      };
      withOtel = true;
    };
  };

  # ── lifecycle splits (3 tests, k3s-full fixture) ─────────────────────
  # Monolith was ~14min (13 subtests serially after ~4min bootstrap).
  # Split critical path ~8min (autoscale: 238s subtests + 4min boot).
  # The `initial` subtest was dropped — it only existed to seed out_pin
  # early for gc-sweep; gc-sweep now builds its own paths.
  #
  # P0294: ctrlrestart + reconnect splits removed (Build CRD rip).
  # build-crd-flow + build-crd-errors dropped from core.
  vm-lifecycle-core-k3s = lifecycleMod.mkTest {
    name = "core";
    subtests = [
      # r[verify sec.jwt.pubkey-mount]
      #   jwt-mount-present: scheduler+store have rio-jwt-pubkey ConfigMap
      #   at /etc/rio/jwt; gateway has rio-jwt-signing Secret. Placed
      #   FIRST — pure precondition check, no pod disruption, ~5s.
      #   Everything else (health-shared onward) assumes the same stable
      #   2-replica state so ordering is immaterial wrt those.
      "jwt-mount-present"
      # r[verify ctrl.probe.named-service]
      "health-shared"
      # r[verify worker.cancel.cgroup-kill]
      "cancel-cgroup-kill"
      # r[verify worker.cgroup.kill-on-teardown]
      # r[verify worker.timeout.no-reassign]
      "build-timeout"
      "gc-dry-run"
      "reconciler-replicas"
      # r[verify store.gc.tenant-retention]
      "gc-sweep"
      # r[verify worker.upload.references-scanned]
      # r[verify worker.upload.deriver-populated]
      # r[verify store.gc.two-phase]
      "refs-end-to-end"
      # r[verify ctrl.drain.disruption-target]
      #   LAST: evicts default-workers-0 (STS recreates it ~120s later,
      #   but core has no subsequent subtests needing a ready worker).
      #   Proves the watcher (disruption.rs) fires DrainWorker{force=
      #   true} — before P0285, force=true had ZERO prod callers.
      "disruption-drain"
    ];
  };

  vm-lifecycle-recovery-k3s = lifecycleMod.mkTest {
    name = "recovery";
    subtests = [ "recovery" ];
  };

  vm-lifecycle-autoscale-k3s = lifecycleAutoscaleMod.mkTest {
    name = "autoscale";
    subtests = [
      # r[verify obs.metric.controller]
      "autoscaler"
      # r[verify ctrl.autoscale.skip-deleting]
      "finalizer"
      # r[verify ctrl.pool.ephemeral]
      # After finalizer: workers_active=0, clean slate for the
      # ephemeral pool (no STS worker steals dispatch before
      # reconcile_ephemeral's 10s tick spawns a Job). ~180s:
      # two builds × (reconcile tick + pod schedule + FUSE +
      # heartbeat + build + exit). Chain enforced by assertChains.
      "ephemeral-pool"
    ];
    # autoscaler ~238s + finalizer 300s + ephemeral ~180s.
    globalTimeout = 1400;
  };

  # ── leader-election splits (2 tests, k3s-full fixture) ───────────────
  # ~0 wall-clock savings (4min bootstrap dominates both) but failures
  # in build-during-failover no longer block the stability checks.
  vm-le-stability-k3s = leMod.mkTest {
    name = "stability";
    subtests = [
      "antiAffinity"
      "lease-acquired"
      # r[verify sched.lease.k8s-lease]
      "stable-leadership"
      # r[verify sched.lease.graceful-release]
      # r[verify sched.lease.deletion-cost]
      "graceful-release"
      "failover"
    ];
  };

  vm-le-build-k3s = leMod.mkTest {
    name = "build";
    subtests = [ "build-during-failover" ];
  };

  # r[verify sched.admin.create-tenant]
  # r[verify sched.admin.list-tenants]
  # r[verify sched.admin.list-workers]
  # r[verify sched.admin.list-builds]
  # r[verify sched.admin.clear-poison]
  # rio-cli had 0% coverage — never invoked by any test. This runs
  # status + create-tenant + list-tenants against the live scheduler's
  # AdminService. ~5min (mostly k3s bring-up).
  vm-cli-k3s = cli {
    inherit pkgs common;
    fixture = k3sFull { };
  };

  # r[verify dash.envoy.grpc-web-translate+2]
  #   Envoy Gateway gRPC-Web → gRPC+mTLS end-to-end. curl with
  #   application/grpc-web+proto against the operator-managed envoy
  #   data-plane Service; asserts DATA frame 0x00 prefix (unary
  #   ClusterStatus) + trailer frame 0x80 byte (streaming
  #   GetBuildLogs). The 0x80 grep proves the grpc_web filter doesn't
  #   buffer server-streams — load-bearing for WatchBuild / live log
  #   tail. Also greps the envoy /config_dump for
  #   envoy.filters.http.grpc_web to prove the GRPCRoute auto-inject
  #   (listener.go:424-425) actually fired. ~6min (k3s bring-up +
  #   operator reconcile). Heavy: +2 images (envoyproxy/gateway +
  #   envoy:distroless) — dedicated test so vm-cli-k3s stays fast.
  vm-dashboard-gateway-k3s = dashboard-gateway {
    inherit pkgs common;
    fixture = k3sFull { envoyGatewayEnabled = true; };
  };

  # Squid forward proxy for FOD fetches — allowlist enforcement + the
  # rio-worker is_fod gate (http_proxy env only for FOD builds).
  # fodProxy.enabled via extraValues (--set-string "true" is truthy in
  # `{{ if }}`). allowedDomains[0]=k3s-server — despite the [0] syntax,
  # --set-string replaces the whole list (helm quirk: indexed set on a
  # values.yaml-defined list creates a fresh 1-element list instead of
  # merging). Exactly what the test wants: k3s-server is the ONLY
  # allowed dstdomain; the denied case (`k3s-agent`) is a clean
  # ACL miss. No ConfigMap patch + squid restart needed (R8).
  #
  # extraImages=[dockerImages.fod-proxy]: dockerImages.all has no squid
  # (it's rio-workspace binaries only). Without this, the rio-fod-proxy
  # pod goes ImagePullBackOff (airgapped VM — no pull).
  vm-fod-proxy-k3s = fod-proxy {
    inherit pkgs common;
    fixture = k3sFull {
      extraValues = {
        "fodProxy.enabled" = "true";
        "fodProxy.allowedDomains[0]" = "k3s-server";
      };
      extraImages = [ dockerImages.fod-proxy ];
    };
  };

  # Worker egress NetworkPolicy: IMDS + public internet + k8s API all
  # blocked. networkPolicy.enabled via extraValues (--set-string "true"
  # is truthy for `{{ if }}` — same quirk as fodProxy.enabled above).
  # vmtest-full.yaml defaults it to false; the override renders
  # networkpolicy.yaml → rio-worker-egress into 02-workloads.yaml.
  # Stock k3s kube-router enforces (P0220) — no Calico preload.
  vm-netpol-k3s = netpol {
    inherit pkgs common;
    fixture = k3sFull {
      extraValues = {
        "networkPolicy.enabled" = "true";
      };
    };
  };
}
