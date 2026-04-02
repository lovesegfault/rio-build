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

  # Prod-parity overlay: bootstrap.enabled=true on top of k3s-full.
  # Three prod regressions from P0493/P0494 all had the same root
  # cause: bootstrap Job never renders in CI. See plan-0500 +
  # fixtures/k3s-prod-parity.nix header for the full rationale.
  k3sProdParity = import ./fixtures/k3s-prod-parity.nix {
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
  dashboard = import ./scenarios/dashboard.nix;
  # fod-proxy scenario removed per ADR-019 — Squid proxy deleted;
  # fetchers get direct egress. The FOD hash check is the integrity
  # boundary. See fetcher.netpol.egress-open (follow-on plan).
  netpol = import ./scenarios/netpol.nix;
  fetcher-split = import ./scenarios/fetcher-split.nix;
  chaos = import ./scenarios/chaos.nix;
  ca-cutoff = import ./scenarios/ca-cutoff.nix;
  substitute = import ./scenarios/substitute.nix;
  drvs = import ./lib/derivations.nix { inherit pkgs; };
  pulled = import ../docker-pulled.nix { inherit pkgs; };

  # Shared fixture for both scheduling splits — identical VM topology.
  schedulingFixture = standalone {
    workers = {
      wsmall1 = {
        sizeClass = "small";
      };
      wsmall2 = {
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
      pkgs.postgresql_18
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

  # Prod-parity lifecycle module. No jwtEnabled — bootstrap-job-ran +
  # bootstrap-tenant don't touch JWT. No autoscaler extraEnv — these
  # subtests don't scale. Bare k3sProdParity {} just flips bootstrap
  # on and preloads the rio-bootstrap image.
  lifecycleProdParityMod = lifecycle {
    inherit pkgs common;
    fixture = k3sProdParity { };
  };
in
{
  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
        };
      };
    };
    cold = false;
  };

  # r[verify sched.ca.cutoff-propagate]
  #   Build CA-on-CA chain (A→B→C, all __contentAddressed=true),
  #   then resubmit with a different marker (A's drv hash differs,
  #   but A's output content is marker-independent → same nar_hash).
  #   Asserts rio_scheduler_ca_cutoff_saves_total ≥ 2 (B+C skipped)
  #   AND second-build elapsed <15s (vs ~24s serial). Also asserts
  #   saves=0 after build-1 (P0397 self-match exclusion regression
  #   guard — ContentLookup must not match the just-uploaded output).
  #   Single worker: the chain is serial anyway; multi-worker would
  #   only add boot cost.
  vm-ca-cutoff-standalone = ca-cutoff {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
        };
      };
      # GAP-1 regression guard: floating-CA output paths are computed
      # post-build, so the scheduler's HMAC token has
      # expected_outputs=[""]. Without Claims.is_ca, the store's
      # path-in-claims check rejects the realized path →
      # PERMISSION_DENIED on every CA upload. withPki=true enables
      # HMAC on this fixture — build-1 failing here means the is_ca
      # bypass at rio-store/src/grpc/mod.rs regressed.
      withPki = true;
    };
  };

  vm-protocol-cold-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      workers = {
        worker = {
        };
        # P0452 hard-split: FODs only dispatch to fetchers. The cold
        # DAG includes a busybox FOD + non-FOD consumer — needs both
        # kinds or hard_filter never matches and the build hangs
        # until globalTimeout.
        fetcher = {
          extraServiceEnv.RIO_EXECUTOR_KIND = "fetcher";
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

  # Upstream binary-cache substitution: fake cache on client VM, store
  # fetches + ingests on QueryPathInfo miss. Validates the P0462/P0463
  # chain at the store-gRPC level (NOT through ssh-ng — gateway read-
  # opcode handlers don't yet propagate x-rio-tenant-token; see
  # TODO(P0465) in the scenario file).
  #
  # JWT: store needs RIO_JWT__KEY_PATH set so the interceptor attaches
  # TenantClaims → request_tenant_id() → Some(tid) → substitution fires.
  # jwt-keys.nix test pubkey (seed=0x42×32) via pkgs.writeText →
  # store-path in VM closure. The scenario signs matching JWTs with the
  # seed via PyJWT.
  #
  # signingKeyFile: sig_mode=add needs a rio-side Signer. Fixed test
  # seed → key name "rio-vm-test-1" (the scenario asserts this exact
  # name in narinfo.signatures). Nix secret-key format: name:base64seed.
  #
  # 0 workers: no builds, pure store-side test. workers={} → empty
  # workerNodes attrset → just control+client VMs.
  #
  # r[verify store.substitute.upstream]
  #   substitute-cold-fetch: miss → HTTP GET narinfo → sig-verify →
  #   GET nar → CAS ingest → narinfo INSERT. Metric + psql assertions.
  # r[verify store.substitute.sig-mode]
  #   substitute-sig-mode-add: sig_mode=add → BOTH upstream AND rio
  #   sigs in narinfo.signatures.
  # r[verify store.substitute.tenant-sig-visibility]
  #   substitute-cross-tenant-gate: tenant C (untrusted key) → NotFound
  #   on A-substituted path; tenant B (trusts same key) → visible.
  #   Dynamic re-trust proves per-request trusted_keys read.
  # r[verify gw.opcode.query-missing]
  # r[verify gw.opcode.query-path-info]
  # r[verify store.tenant.narinfo-filter]
  #   substitute-ssh-ng: gateway propagates JWT through wopQueryPathInfo
  #   → store's try_substitute_on_miss fires → path substitutable via
  #   the real ssh-ng protocol path (not grpcurl backdoor).
  vm-substitute-standalone =
    let
      jwtKeys = import ./lib/jwt-keys.nix;
      jwtPubkey = pkgs.writeText "jwt-pubkey" jwtKeys.pubkeyB64;
      # Gateway's signing seed — same keypair as the store verifies
      # against. Gateway SIGNS with seed, store VERIFIES with pubkey.
      jwtSeed = pkgs.writeText "jwt-seed" jwtKeys.seedB64;
      # 32×0x55 seed, base64-encoded. Distinct from jwtKeys (0x42) so
      # a JWT-sig/narinfo-sig mixup would fail loudly.
      rioSigningKey = pkgs.writeText "rio-signing-key" "rio-vm-test-1:VVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVVU=";
    in
    substitute {
      inherit pkgs common;
      fixture = standalone {
        workers = { };
        extraStoreConfig = {
          signingKeyFile = "${rioSigningKey}";
          extraConfig = ''
            [jwt]
            key_path = "${jwtPubkey}"
          '';
        };
        # Gateway-only: signing seed so mint_session_jwt works. With
        # this set, ssh auth (tenant-name comment) → scheduler
        # resolve_tenant → UUID → mint JWT → attach to all outbound
        # gRPC (P0465 threaded this through opcodes_read.rs).
        extraGatewayEnv.RIO_JWT__KEY_PATH = "${jwtSeed}";
        # grpcurl + postgresql (psql) on control for direct store
        # probing + narinfo table inspection.
        extraPackages = [
          pkgs.grpcurl
          pkgs.postgresql_18
        ];
        # Open :8080 on client for the fake-upstream http.server. The
        # store (on control) fetches http://client:8080/<hash>.narinfo.
        extraClientModules = [
          { networking.firewall.allowedTCPPorts = [ 8080 ]; }
        ];
      };
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
          # r[verify builder.overlay.stacked-lower+2]
          # r[verify builder.ns.order+2]
          # r[verify builder.fuse.lookup-caches]
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
          # r[verify builder.silence.timeout-kill]
          "max-silent-time"
          # r[verify gw.opcode.set-options.propagation+2]
          # setoptions-unreachable greps ALL gateway journal history —
          # placed after sizeclass + max-silent-time so it also covers
          # THEIR ssh-ng sessions (neither passed --option, but the
          # handshake's virtual setOptions() call runs regardless).
          "setoptions-unreachable"
          "cancel-timing"
          "reassign"
          # r[verify obs.metric.scheduler]
          # r[verify obs.metric.builder]
          # r[verify obs.metric.store]
          "load-50drv"
          # r[verify sched.assign.warm-gate]
          #   Placed AFTER load-50drv so the per-assignment PrefetchHint
          #   → worker-ACK → rio_scheduler_warm_prefetch_paths histogram
          #   has had many opportunities to fire. Passive check (~0s).
          "warm-gate"
          # r[verify builder.shutdown.sigint]
          # sigint-graceful AFTER reassign: reassign already disturbs a
          # worker (SIGKILL + wait_for_unit restart); sigint is the
          # gentler sibling. Uses wsmall2 only — no cache-chain coupling.
          # ~35s: SIGINT + 30s inactive-wait + restart + FUSE remount.
          #
          # sigint-graceful LAST: restarts wsmall2 (systemctl start) but
          # doesn't wait for scheduler re-registration (HEARTBEAT_INTERVAL
          # = 10s at rio-common/src/limits.rs:51). If load-50drv ran AFTER
          # it'd see 2 slots not 4 → ~26 waves instead of ~13 → ~2×
          # walltime. Placing sigint last makes the re-registration
          # window non-load-bearing (collectCoverage reads profraw from
          # the host fs, doesn't need wsmall2 registered with scheduler).
          "sigint-graceful"
        ];
        # Default 600s is tight now: sizeclass ~30s + max-silent-time
        # ~25s + cancel-timing ~40s + reassign ~60s + load-50drv ~60s +
        # sigint ~35s ≈ 250s subtests + ~120s boot. load-50drv under
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
        pkgs.postgresql_18
      ];
    };
  };

  # r[verify sec.pod.fuse-device-plugin]
  # r[verify builder.cgroup.ns-root-remount]
  # r[verify sec.psa.control-plane-restricted]
  #   Non-privileged + device-plugin VM e2e. hostUsers:false NOT
  #   exercised here — k3s's containerd (systemd cgroup driver)
  #   doesn't chown the pod cgroup to the userns root; worker mkdir
  #   /sys/fs/cgroup/leaf fails EACCES → CrashLoopBackOff. The
  #   sec.pod.host-users-false marker stays verified by the
  #   builders.rs unit test (renders-shape check).
  # Every other k3s fixture
  #   uses vmtest-full.yaml privileged:true (containerd mounts
  #   /sys/fs/cgroup rw already, hostPath /dev/fuse works) — the
  #   rw-remount and device-plugin paths were never exercised until
  #   this scenario. builders.rs unit tests prove pod SHAPE renders
  #   correctly; this proves it WORKS (DS Ready → extended resource
  #   advertised → worker pod Ready with hostUsers:false → cgroup/leaf
  #   exists + subtree_control writable → build completes over FUSE).
  vm-security-nonpriv-k3s = security.privileged-hardening-e2e {
    fixture = k3sFull {
      # Layer vmtest-full-nonpriv.yaml for workerPool.privileged:false +
      # devicePlugin.enabled + nodeSelector/tolerations null. The
      # devicePlugin.image override is passed via extraValues below
      # (DERIVED from the preload FOD — no drift window).
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml
      ];
      # containerd airgap cache is tag-indexed (pullImage's finalImageTag),
      # not digest-indexed. Chart default is digest-pinned (@sha256:…) →
      # exact-string miss → ImagePullBackOff. destNameTag is
      # "${finalImageName}:${finalImageTag}" — the preloaded cache key.
      extraValues = {
        "devicePlugin.image" = pulled.smarter-device-manager.destNameTag;
      };
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
  # r[verify obs.metric.bloom-fill-ratio]
  #   metrics-registered subtest asserts rio_builder_bloom_fill_ratio
  #   gauge present on the busy worker with 0.0 < fill < 0.5 after
  #   a chain build — proves the 10s heartbeat emit is wired AND
  #   FUSE inserts populated the filter.
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
        };
        worker2 = {
        };
        worker3 = {
        };
      };
      withOtel = true;
    };
  };

  # ── lifecycle splits (4 tests, k3s-full fixture) ─────────────────────
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
      # r[verify builder.cancel.cgroup-kill]
      "cancel-cgroup-kill"
      # r[verify builder.cgroup.kill-on-teardown]
      # r[verify builder.timeout.no-reassign]
      "build-timeout"
      "gc-dry-run"
      "reconciler-replicas"
      # r[verify store.gc.tenant-retention]
      "gc-sweep"
      # r[verify builder.upload.references-scanned]
      # r[verify builder.upload.deriver-populated]
      # r[verify store.gc.two-phase]
      "refs-end-to-end"
      # r[verify ctrl.drain.disruption-target]
      #   LAST: evicts rio-builder-x86_64-0 (STS recreates it ~120s later,
      #   but core has no subsequent subtests needing a ready worker).
      #   Proves the watcher (disruption.rs) fires DrainWorker{force=
      #   true} — before P0285, force=true had ZERO prod callers.
      "disruption-drain"
    ];
  };

  vm-lifecycle-recovery-k3s = lifecycleMod.mkTest {
    name = "recovery";
    subtests = [
      "recovery"
      # r[verify sched.store-client.reconnect]
      #   store-rollout: rollout restart deploy/rio-store → scheduler's
      #   lazy Channel re-resolves DNS and reconnects to the new pod.
      #   Post-rollout build succeeds WITHOUT scheduler restart.
      #   After recovery (not before): recovery leaves the cluster in
      #   a settled state (q==0 r==0 drain at recovery's end), so
      #   store-rollout starts from a clean baseline.
      "store-rollout"
    ];
  };

  vm-lifecycle-autoscale-k3s = lifecycleAutoscaleMod.mkTest {
    name = "autoscale";
    subtests = [
      # r[verify obs.metric.controller]
      "autoscaler"
      # r[verify ctrl.autoscale.skip-deleting]
      "finalizer"
      # r[verify ctrl.pool.ephemeral]
      # r[verify ctrl.pool.ephemeral-deadline]
      # r[verify ctrl.crd.host-users-network-exclusive]
      # After finalizer: workers_active=0, clean slate for the
      # ephemeral pool (no STS worker steals dispatch before
      # reconcile_ephemeral's 10s tick spawns a Job). ~180s:
      # two builds × (reconcile tick + pod schedule + FUSE +
      # heartbeat + build + exit). Chain enforced by assertChains.
      # ephemeral-deadline: negative kubectl apply asserts CEL
      # rejects ephemeralDeadlineSeconds on non-ephemeral pools.
      "ephemeral-pool"
      # r[verify ctrl.pool.manifest-reconcile]
      # r[verify ctrl.pool.manifest-labels]
      # r[verify ctrl.pool.manifest-long-lived]
      # After ephemeral-pool: workers_active=0 again (ephemeral
      # cleaned up its own pool via ownerRef GC). Manifest-mode
      # pod is long-lived (no RIO_EPHEMERAL) — ONE cold-start
      # floor Job spawns and persists. ~150s: workers_active=0
      # drain wait + reconcile tick + Job schedule + pod start
      # + heartbeat + build + status_patch assertion (needs 2
      # reconcile ticks) + ownerRef delete cascade.
      "manifest-pool"
    ];
    # autoscaler ~238s + finalizer 300s + ephemeral ~180s + manifest ~150s.
    globalTimeout = 1600;
  };

  # r[verify ctrl.pdb.workers]
  #   pdb-ownerref: fixture's `rio` BuilderPool → reconciler
  #   SSA-applies `rio-pdb` with maxUnavailable=1 + ownerRef
  #   [0]→BuilderPool. Delete `rio` → ownerRef cascade GCs the
  #   PDB. Unit test (tests.rs:550) proves struct shape; this proves
  #   SSA-apply + K8s GC end-to-end.
  # r[verify ctrl.wps.reconcile]
  #   wps-lifecycle: apply 3-class WPS → 3 child WorkerPools named
  #   `{wps}-{class}` each with sizeClass=class.name + ownerRef[0]=
  #   BuilderPoolSet (controller=true). Delete WPS → finalizer
  #   cleanup explicitly deletes children; ownerRef GC as fallback.
  # r[verify ctrl.wps.autoscale]
  #   wps-lifecycle asserts each child's .spec.autoscaling.targetValue
  #   = class.targetQueuePerReplica (default 5). Proves the per-class
  #   autoscaler wiring (builders.rs:92-99); scale-up/down mechanics
  #   are unit-tested in scaling.rs.
  #
  # Own split (not folded into core/autoscale): pdb-ownerref deletes
  # the `rio` BuilderPool, which core's disruption-drain needs
  # intact and autoscale's finalizer already deletes (can't check
  # exists+ownerRef after). Fresh fixture → clean state → fast
  # finalizers (no in-flight builds to drain). ~4min boot + ~3min
  # subtests.
  vm-lifecycle-wps-k3s = lifecycleMod.mkTest {
    name = "wps";
    subtests = [
      "pdb-ownerref"
      "wps-lifecycle"
      # r[verify ctrl.fetcherpool.reconcile]
      # r[verify fetcher.sandbox.strict-seccomp]
      #   FetcherPool CR → STS with rio.build/role:fetcher label,
      #   readOnlyRootFilesystem:true, rio-fetcher.json seccomp,
      #   fetcher nodeSelector+toleration. STS-shape-only — pod
      #   readiness lives in vm-fetcher-split-k3s below (needs
      #   device-plugin + seccomp-profile-on-node + labeled node).
      "fetcherpool-sts"
    ];
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
    # r[verify sched.lease.non-blocking-acquire]
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
  #   ClusterStatus) + trailer frame prefix 80 00 00 00 (streaming
  #   GetBuildLogs). The frame-prefix grep proves the grpc_web filter
  #   doesn't buffer server-streams — load-bearing for WatchBuild /
  #   live log tail. Also greps the envoy /config_dump for
  #   envoy.filters.http.grpc_web to prove the GRPCRoute auto-inject
  #   (listener.go:424-425) actually fired. ~6min (k3s bring-up +
  #   operator reconcile). Heavy: +2 images (envoyproxy/gateway +
  #   envoy:distroless) — dedicated test so vm-cli-k3s stays fast.
  # r[verify dash.auth.method-gate]
  #   Same curl shape against ClearPoison → expects 404. The fixture
  #   doesn't set dashboard.enableMutatingMethods so the mutating
  #   GRPCRoute is absent. Proves the helm-template fail-closed holds
  #   at runtime through the operator's xDS reconcile. Positive
  #   control: ClusterStatus on the readonly route returns 200.
  # r[verify dash.journey.build-to-logs]
  #   The GetBuildLogs 0x80 trailer assertion proves server-streaming
  #   works through the nginx→Envoy Gateway→scheduler chain. Handler
  #   returns errors as in-stream items (not tonic Trailers-Only) so
  #   Envoy encodes them as 0x80 body frames browser fetch can read.
  vm-dashboard-gateway-k3s = dashboard-gateway {
    inherit pkgs common;
    fixture = k3sFull { envoyGatewayEnabled = true; };
  };

  # vm-fod-proxy-k3s removed per ADR-019 — Squid proxy deleted. FODs
  # route to FetcherPool with direct egress + hash-check integrity
  # boundary. Scenario file deleted too.

  # Builder + store egress NetworkPolicy: IMDS + public internet + k8s
  # API all blocked. networkPolicy.enabled via extraValues (--set-string
  # "true" is truthy for `{{ if }}`).
  # vmtest-full.yaml defaults it to false; the override renders
  # networkpolicy.yaml into 02-workloads.yaml.
  # Stock k3s kube-router enforces (P0220) — no Calico preload.
  #
  # r[verify store.netpol.egress]
  #   store-egress IMDS-deny + postgres-allow probe via nsenter into
  #   rio-store pod netns (netpol-store-egress subtest).
  # r[verify builder.netpol.airgap]
  #   builder-egress IMDS-deny + k8s-API-deny + DNS-TCP-allow probes
  #   (netpol-kubeapi / netpol-imds / netpol-dns-tcp subtests).
  vm-netpol-k3s = netpol {
    inherit pkgs common;
    fixture = k3sFull {
      extraValues = {
        "networkPolicy.enabled" = "true";
      };
    };
  };

  # ADR-019 builder/fetcher split end-to-end. FIRST test running both
  # BuilderPool + FetcherPool pods. Proves: FOD→fetcher routing, non-
  # FOD→builder routing, builder airgap holds, fetcher egress open but
  # IMDS-blocked, fetcher node-dedication wired.
  #
  # Fetcher pod needs the nonpriv path (hard-coded privileged:false +
  # Localhost seccomp at reconcilers/fetcherpool/mod.rs) — same
  # device-plugin overlay as vm-security-nonpriv-k3s. Seccomp profile
  # delivered at runtime by testScript (security-profiles-operator
  # not airgapped). fetcherPool enabled via extraValues with name=
  # "default" (I-104 naming → pod rio-fetcher-default-0)
  # and image=rio-all (same aggregate image all pods use). Systems
  # includes "builtin" so builtin:fetchurl's system=builtin passes
  # the hard_filter can_build check. nodeSelector/tolerations left
  # at reconciler defaults — scenario labels k3s-agent at runtime.
  #
  # r[verify sched.dispatch.fod-to-fetcher]
  #   dispatch-fod+nonfod subtest: one nix-build, FOD routes to
  #   fetcher pod, consumer routes to builder pod. Wrong routing →
  #   queue-forever → timeout. kubectl-logs grep confirms placement.
  # r[verify builder.netpol.airgap]
  #   builder-airgap subtest: builder netns curl to TEST-NET-3 origin
  #   (203.0.113.1:80) → rc≠0. Positive control: scheduler ClusterIP
  #   connects (NetPol allow fires).
  # r[verify fetcher.netpol.egress-open]
  #   fetcher-egress + fetcher-imds-blocked subtests: SAME origin,
  #   fetcher netns → rc==0 (0.0.0.0/0:80 allow fires). Then IMDS
  #   → rc≠0 (169.254.0.0/16 except-clause). The origin probe is the
  #   non-vacuous differentiator vs builder.
  # r[verify fetcher.node.dedicated]
  #   fetcher-node-dedicated subtest: pod spec has the rio.build/
  #   fetcher toleration + nodeSelector (reconciler defaults), and
  #   actually scheduled on the labeled k3s-agent node. Karpenter
  #   NodePool enforcement is EKS-only; this proves the params→
  #   podspec chain.
  vm-fetcher-split-k3s = fetcher-split {
    inherit pkgs common drvs;
    fixture = k3sFull {
      extraValues = {
        "networkPolicy.enabled" = "true";
        "devicePlugin.image" = pulled.smarter-device-manager.destNameTag;
      };
      # fetcherPool via values file (not --set-string) so hostUsers
      # stays bool true. --set-string would coerce to the STRING
      # "true" which the CRD Option<bool> field rejects.
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml
        (pkgs.writeText "fetcherpool-vm.yaml" ''
          fetcherPool:
            enabled: true
            name: default
            # P0541: ephemeral defaults true at CRD level. This test
            # exercises fetcher-split routing via the STS path (stable
            # pod name for the kubectl-exec assertions below).
            ephemeral: false
            # I-014: replicas is now {min, max} for autoscaling. min=max
            # pins the replica count — this test exercises fetcher-split
            # routing, not autoscaling. autoscaling block inherits from
            # base values.yaml (helm coalesce).
            replicas: {min: 1, max: 1}
            image: rio-all
            # builtin:fetchurl FOD has system=builtin; hard_filter's
            # can_build check needs the fetcher to advertise it.
            systems: [x86_64-linux, builtin]
            # k3s containerd doesn't chown the pod cgroup under
            # hostUsers:false → rio-builder's mkdir /sys/fs/cgroup/
            # leaf EACCES → CrashLoop. Same escape hatch builderPool
            # uses (via privileged:true which implies hostUsers:true).
            hostUsers: true
        '')
      ];
      extraImages = [ pulled.smarter-device-manager ];
    };
  };

  # ── prod-parity: bootstrap Job + leader-guard under replicas=2 ────────
  # Three prod regressions (a28e4b65, abef66c7, 5b98e311) shared a
  # root cause: VM tests use minimal config; prod uses bootstrap.
  # enabled=true. The bootstrap Job never rendered in CI. This fixture
  # flips it on so the PSA-restricted exec path (readOnlyRootFilesystem
  # + HOME=/tmp for awscli2 cache) runs at merge-gate. The Job will
  # FAIL (aws secretsmanager unreachable in the airgapped VM) —
  # expected; bootstrap-job-ran asserts no-EROFS + script-progress,
  # not completion. ~5min (k3s bring-up + bootstrap Job backoff).
  vm-lifecycle-prod-parity-k3s = lifecycleProdParityMod.mkTest {
    name = "prod-parity";
    subtests = [
      # r[verify sec.psa.control-plane-restricted]
      #   bootstrap-job-ran: Job's pod-template has
      #   readOnlyRootFilesystem=true + HOME=/tmp, logs show
      #   "[bootstrap] generating rio/hmac" (past env-check +
      #   awscli2 init), logs DON'T contain "Read-only file
      #   system". The a28e4b65 regression signature.
      #   vm-security-nonpriv-k3s above verifies PSA on the
      #   builder side; this verifies it on control-plane Jobs.
      "bootstrap-job-ran"
      # r[verify sched.grpc.leader-guard]
      #   bootstrap-tenant: standby explicitly rejects CreateTenant
      #   with UNAVAILABLE (positive guard test), Lease-routed
      #   leader accepts 3/3 (abef66c7 determinism). First VM-level
      #   verify for leader-guard under replicas>1 — guards_tests.rs
      #   proves interceptor shape, this proves the 2-replica
      #   end-to-end. scheduler.replicas=2 is already vmtest-full.
      #   yaml's default (line 99) so this subtest works under the
      #   base k3s-full fixture too; co-located here with
      #   bootstrap-job-ran since both exercise prod-config-only.
      "bootstrap-tenant"
    ];
  };
}
# r[verify dash.journey.build-to-logs]
#   nginx → Envoy Gateway → scheduler chain end-to-end. Four curl
#   assertions against the rio-dashboard Service (the nginx pod the
#   browser actually talks to, not the envoy Service directly):
#     SPA served (id="app") + SPA routing fallback (try_files) +
#     unary gRPC-Web 0x00 + streaming 0x80 trailer THROUGH nginx's
#     proxy_buffering-off location block.
#   vm-dashboard-gateway-k3s proves the envoy half; this proves the
#   nginx half AND the full chain. ~6min (same fixture cost).
#
# optionalAttrs gate: dockerImages.dashboard doesn't exist in
# coverage mode (mkDockerImages passes rioDashboard=null → docker.nix
# elides the attr). Gating the scenario registration on `?` keeps
# vmTestsCov consistent — the coverage pipeline never sees this test,
# perTestLcov doesn't include it, codecov after_n_builds stays at
# its current count. (The nginx pod has no LLVM instrumentation
# anyway — coverage would yield zero profraws.)
// pkgs.lib.optionalAttrs (dockerImages ? dashboard) {
  vm-dashboard-k3s = dashboard {
    inherit pkgs common;
    fixture = k3sFull { envoyGatewayEnabled = true; };
  };
}
