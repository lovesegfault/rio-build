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
  lixPackage,
}:
let
  # Shared arg set for common.nix + every fixture. Fixtures take `...`
  # so the unused k3s-only attrs (dockerImages, nixhelm, system) are
  # ignored by standalone/toxiproxy.
  fixtureArgs = {
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
  common = import ./common.nix fixtureArgs;
  standalone = import ./fixtures/standalone.nix fixtureArgs;
  toxiproxy = import ./fixtures/toxiproxy.nix fixtureArgs;
  k3sFull = import ./fixtures/k3s-full.nix fixtureArgs;
  # Prod-parity overlay: bootstrap.enabled=true on top of k3s-full.
  # Three prod regressions from P0493/P0494 all had the same root
  # cause: bootstrap Job never renders in CI. See plan-0500 +
  # fixtures/k3s-prod-parity.nix header for the full rationale.
  k3sProdParity = import ./fixtures/k3s-prod-parity.nix fixtureArgs;

  protocol = import ./scenarios/protocol.nix;
  scheduling = import ./scenarios/scheduling.nix;
  # security exports { standalone, privileged-hardening-e2e } — two
  # scenario functions sharing the same file. standalone uses the
  # systemd fixture (HMAC/tenant/validation); e2e uses k3sFull
  # with the nonpriv values overlay (base_runtime_spec /dev/fuse +
  # cgroup remount).
  security = import ./scenarios/security.nix { inherit pkgs common; };
  observability = import ./scenarios/observability.nix;
  lifecycle = import ./scenarios/lifecycle.nix;
  leader-election = import ./scenarios/leader-election.nix;
  cli = import ./scenarios/cli.nix;
  dashboard-gateway = import ./scenarios/dashboard-gateway.nix;
  dashboard = import ./scenarios/dashboard.nix;
  netpol = import ./scenarios/netpol.nix;
  cilium-encrypt = import ./scenarios/cilium-encrypt.nix;
  ingress-v4v6 = import ./scenarios/ingress-v4v6.nix;
  fetcher-split = import ./scenarios/fetcher-split.nix;
  chaos = import ./scenarios/chaos.nix;
  ca-cutoff = import ./scenarios/ca-cutoff.nix;
  componentscaler = import ./scenarios/componentscaler.nix;
  substitute = import ./scenarios/substitute.nix;
  sla-sizing = import ./scenarios/sla-sizing.nix;
  drvs = import ./lib/derivations.nix { inherit pkgs; };

  # SLA-sizing fixture: one worker with RIO_BUILDER_SCRIPT pointing at
  # the scripted-telemetry TOML, scheduler with [sla] configured + the
  # InjectBuildSample fixture gate. tickIntervalSecs=2 so the estimator
  # refit fires fast enough for wait_until_succeeds.
  slaSizingFixture = standalone {
    workers = {
      worker = {
        extraServiceEnv = {
          RIO_BUILDER_SCRIPT = "${./fixtures/sla-builder-script.toml}";
        };
      };
    };
    extraSchedulerEnv = {
      RIO_ADMIN_TEST_FIXTURES = "1";
    };
    extraSchedulerConfig = {
      tickIntervalSecs = 2;
      extraConfig = ''
        [sla]
        default_tier = "normal"
        hw_cost_source = "static"
        max_cores = 64
        max_mem = 274877906944
        max_disk = 214748364800
        default_disk = 21474836480

        [[sla.tiers]]
        name = "normal"
        p90 = 1200

        [sla.probe]
        cpu = 4
        mem_per_core = 2147483648
        mem_base = 4294967296
      '';
    };
    extraPackages = [
      pkgs.postgresql
      pkgs.grpcurl
    ];
  };

  # Shared fixture for both scheduling splits — identical VM topology.
  schedulingFixture = standalone {
    workers = {
      # maxSilentTime enforcement on ALL scheduling workers. Every drv
      # that lands here MUST stay non-silent for ≥10s — cancelDrv echoes
      # every 5s (scheduling.nix); reassignDrv echoes every 5s; the rest
      # sleep ≤3s or echo immediately.
      #
      # Worker-side config because the Nix ssh-ng client does NOT send
      # wopSetOptions (protocol 1.38) — client --max-silent-time cannot
      # propagate to the gateway.
      worker1 = {
        extraServiceEnv = {
          RIO_MAX_SILENT_TIME_SECS = "10";
        };
      };
      worker2 = {
        extraServiceEnv = {
          # Non-passthrough FUSE: exercises open_files tracking,
          # userspace read(), release(). fuse/ops.rs read() at 33%
          # coverage before this — passthrough bypasses the kernel
          # callback entirely.
          RIO_FUSE_PASSTHROUGH = "false";
          RIO_MAX_SILENT_TIME_SECS = "10";
        };
      };
      worker3 = {
        extraServiceEnv = {
          RIO_MAX_SILENT_TIME_SECS = "10";
        };
      };
    };
    extraSchedulerConfig = {
      tickIntervalSecs = 2;
      # [sla] is mandatory; sized for 3× tiny VM workers (2 GiB each).
      extraConfig = ''
        [sla]
        default_tier = "normal"
        max_cores = 2
        max_mem = 2147483648
        max_disk = 6442450944
        default_disk = 2147483648

        [[sla.tiers]]
        name = "normal"

        [sla.probe]
        cpu = 1
        mem_per_core = 1073741824
        mem_base = 1073741824
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
    # (no withHmac). ssh-ng:// doesn't surface build_id to the
    # client, and client-disconnect mid-wopBuildDerivation doesn't fire
    # session.rs's EOF-cancel path (handler/build.rs:462 removes the
    # build_id before bubbling). gRPC SubmitBuild + CancelBuild is the
    # only deterministic cancel-a-running-build path in this fixture.
    extraPackages = [
      pkgs.postgresql_18
      pkgs.grpcurl
    ];
  };

  # Shared lifecycle module for all lifecycle splits.
  #
  # jwtEnabled: mounts the rio-jwt-pubkey ConfigMap into scheduler+store
  # and the rio-jwt-signing Secret into gateway (lib/jwt-keys.nix fixed
  # test keypair). SchedulerGrpc.require_tenant() rejects tokenless
  # SchedulerService calls in JWT mode, so lifecycle.nix's
  # prelude creates a vm-lifecycle tenant, mints a matching JWT for
  # grpcurl-direct calls, and gives the SSH key that tenant's name as
  # its comment so the gateway mints a JWT for ssh-ng builds. Turned on
  # here for jwt-mount-present; the other splits inherit it via the
  # shared module and exercise the full tenant-authz path as a bonus.
  lifecycleMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull { jwtEnabled = true; };
  };

  leMod = leader-election {
    inherit pkgs common;
    fixture = k3sFull { };
  };

  # Prod-parity lifecycle module. No jwtEnabled — bootstrap-job-ran +
  # bootstrap-tenant don't touch JWT. Bare k3sProdParity {} just flips
  # bootstrap on and preloads the rio-bootstrap image.
  lifecycleProdParityMod = lifecycle {
    inherit pkgs common;
    fixture = k3sProdParity { };
  };
in
{
  # ── nixos-node AMI bootstrap (mocked IMDS, no AWS) ────────────────────
  # r[verify infra.node.nixos-ami]
  #   Single-node test, no fixture/scenario split. Boots the
  #   nix/nixos-node module tree (NOT the disk image) under QEMU with
  #   a mocked IMDSv2 on lo. nodeadm-init must parse the multipart
  #   NodeConfig + write /etc/eks/kubelet/environment; containerd's
  #   ActiveEnterTimestamp must precede nodeadm-init's; kubelet forks
  #   under NODEADM_KUBELET_ARGS. Would have caught the Phase-1
  #   `-d kubelet` short-flag collision that only surfaced on live EC2.
  #   Gates Phase-2 boot-path changes (initrd-networkd, UKI, perlless).
  vm-nixos-node = import ./nixos-node.nix { inherit pkgs; };

  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone { };
    cold = false;
  };

  # r[verify gw.compat.version-range+2]
  #   Identical to vm-protocol-warm-standalone but the client VM runs
  #   Lix. Lix is policy-frozen at daemon protocol 1.35, so this
  #   exercises rio's MIN_CLIENT_VERSION floor and the ≥1.37
  #   BuildResult.cpu_* gate against a real ssh-ng client end-to-end.
  #   Single Lix test in .#ci — wire-level Lix-as-daemon coverage lives
  #   in the weekly .#golden-matrix.
  vm-protocol-warm-lix-standalone = protocol {
    inherit pkgs common;
    nameSuffix = "-lix";
    fixture = standalone {
      clientNixPackage = lixPackage;
      extraClientModules = [
        {
          # Lix rejects ca-derivations as "unknown experimental
          # feature" at nix.conf validation. mkClientNode sets it
          # unconditionally for ca-cutoff's benefit; protocol-warm
          # doesn't need it.
          nix.settings.experimental-features = pkgs.lib.mkForce [
            "nix-command"
            "flakes"
          ];
        }
      ];
    };
    cold = false;
  };

  # r[verify sched.ca.cutoff-propagate+2]
  # r[verify sched.ca.resolve+3]
  #   Build CA-on-CA chain (A→B→C, all __contentAddressed=true),
  #   then resubmit with a different marker (A's drv hash differs,
  #   but A's output content is marker-independent → same nar_hash).
  #   Asserts rio_scheduler_ca_cutoff_saves_total ≥ 2 (B+C skipped)
  #   AND second-build elapsed <15s (vs ~24s serial). Also asserts
  #   saves=0 after build-1 (P0397 self-match exclusion regression
  #   guard — realisation lookup must not match the just-uploaded
  #   output against itself).
  #   Single worker: the chain is serial anyway; multi-worker would
  #   only add boot cost.
  vm-ca-cutoff-standalone = ca-cutoff {
    inherit pkgs common;
    fixture = standalone {
      # GAP-1 regression guard: floating-CA output paths are computed
      # post-build, so the scheduler's HMAC token has
      # expected_outputs=[""]. Without Claims.is_ca, the store's
      # path-in-claims check rejects the realized path →
      # PERMISSION_DENIED on every CA upload. withHmac=true enables
      # HMAC on this fixture — build-1 failing here means the is_ca
      # bypass at rio-store/src/grpc/mod.rs regressed.
      withHmac = true;
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
  # r[verify store.substitute.tenant-sig-visibility+2]
  # r[verify store.substitute.find-missing-gated]
  # r[verify store.api.batch-manifest+2]
  #   substitute-cross-tenant-gate: tenant C (untrusted key) → NotFound
  #   on A-substituted path via QueryPathInfo/GetPath/FindMissingPaths;
  #   PermissionDenied via BatchGetManifest (builder-internal). Tenant
  #   B (trusts same key) → visible. Dynamic re-trust proves per-request
  #   trusted_keys read.
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

  # ── sla-sizing (standalone fixture, scripted-telemetry worker) ───────
  vm-sla-sizing-standalone =
    (sla-sizing {
      inherit pkgs common;
      fixture = slaSizingFixture;
    }).mkTest
      {
        name = "default";
        subtests = [
          # convergence proves build_samples plumbing only; the
          # explore-{x4-first-bump,saturation-gate,freeze} rules are
          # verified at unit level (sla/explore.rs).
          "convergence"
          # r[verify sched.sla.outlier-mad-reject]
          "outlier"
          # r[verify sched.sla.override-precedence]
          "override-precedence"
          # r[verify sched.sla.hw-ref-seconds]
          "hw-normalize"
          # r[verify sched.sla.solve-per-band-cap]
          "cost-solve"
          # r[verify sched.sla.tier-envelope]
          "ice-backoff"
          # r[verify sched.sla.prior-partial-pool]
          "seed-corpus"
        ];
      };

  # ── scheduling splits (2 tests, standalone fixture) ──────────────────
  # Same 3-worker fixture (worker1/worker2/worker3) for both — the
  # fragment architecture changes what RUNS, not what's BOOTED.
  # fanout→fuse-direct cache-state chain stays in core; reassign is
  # disruptive (SIGKILL) → own test.
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
          # r[verify builder.fuse.lookup-caches+2]
          # r[verify builder.fuse.jit-lookup]
          # r[verify builder.fuse.jit-register]
          # r[verify builder.fuse.passthrough]
          "fanout"
          "fuse-direct"
          # r[verify builder.fuse.listxattr-empty]
          "fuse-listxattr"
          "overlay-readdir"
          # r[verify store.inline.threshold]
          # r[verify obs.metric.transfer-volume]
          "chunks"
          "cgroup"
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
          # r[verify builder.silence.timeout-kill]
          # r[verify sched.timeout.promote-on-exceed+2]
          "max-silent-time"
          # r[verify gw.opcode.set-options.propagation+2]
          # setoptions-unreachable greps ALL gateway journal history —
          # placed after max-silent-time so it also covers ITS ssh-ng
          # sessions (no --option passed, but the handshake's virtual
          # setOptions() call runs regardless).
          "setoptions-unreachable"
          "cancel-timing"
          # r[verify sched.sla.reactive-floor+2]
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
          # r[verify builder.shutdown.sigint+2]
          # sigint-graceful AFTER reassign: reassign already disturbs a
          # worker (SIGKILL + wait_for_unit restart); sigint is the
          # gentler sibling. Uses worker2 only — no cache-chain coupling.
          # ~35s: SIGINT + 30s inactive-wait + restart + FUSE remount.
          #
          # sigint-graceful LAST: restarts worker2 (systemctl start) but
          # doesn't wait for scheduler re-registration (HEARTBEAT_INTERVAL
          # = 10s at rio-common/src/limits.rs:51). If load-50drv ran AFTER
          # it'd see 2 slots not 4 → ~26 waves instead of ~13 → ~2×
          # walltime. Placing sigint last makes the re-registration
          # window non-load-bearing (collectCoverage reads profraw from
          # the host fs, doesn't need worker2 registered with scheduler).
          "sigint-graceful"
        ];
        # Default 600s is tight now: max-silent-time ~25s + cancel-timing
        # ~40s + reassign ~60s + load-50drv ~60s + sigint ~35s ≈ 220s
        # subtests + ~120s boot. load-50drv under TCG could stretch to
        # 150s (13 waves × tick=2s × TCG overhead). 900s is comfortable
        # without being an open-ended escape hatch.
        globalTimeout = 900;
      };

  # r[verify gw.jwt.dual-mode+2]
  # r[verify sec.boundary.grpc-hmac]
  # r[verify gw.reject.nochroot]
  # r[verify gw.rate.per-tenant]
  # r[verify store.gc.tenant-quota-enforce]
  # r[verify sec.executor.identity-token+2]
  #   Single-test scenario (no subtests list). Markers at the wiring
  #   point per P0341 convention — scenario header prose explains which
  #   subtest proves each rule.
  vm-security-standalone = security.standalone {
    fixture = standalone {
      withHmac = true;
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
  #   Non-privileged VM e2e. hostUsers:false NOT exercised here —
  #   k3s's containerd (systemd cgroup driver) doesn't chown the pod
  #   cgroup to the userns root; worker mkdir /sys/fs/cgroup/leaf
  #   fails EACCES → CrashLoopBackOff. The sec.pod.host-users-false
  #   marker stays verified by the builders.rs unit test (renders-
  #   shape check). Every other k3s fixture uses vmtest-full.yaml
  #   privileged:true (containerd mounts /sys/fs/cgroup rw already,
  #   hostPath /dev/fuse works) — the rw-remount and base_runtime_spec
  #   paths were never exercised until this scenario. builders.rs unit
  #   tests prove pod SHAPE renders correctly; this proves it WORKS
  #   (base_runtime_spec /dev/fuse → worker pod Ready → cgroup/leaf
  #   exists + subtree_control writable → build completes over FUSE).
  vm-security-nonpriv-k3s = security.privileged-hardening-e2e {
    fixture = k3sFull {
      # Layer vmtest-full-nonpriv.yaml for workerPool.privileged:false.
      # /dev/fuse comes from k3s containerd base_runtime_spec (the
      # containerdConfigTemplate in fixtures/k3s-full.nix). No extra
      # airgap images needed.
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml
      ];
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
      # r[verify ctrl.health.ready-gates-connect]
      "health-shared"
      # r[verify builder.cancel.cgroup-kill]
      "cancel-cgroup-kill"
      # r[verify builder.cgroup.kill-on-teardown]
      # r[verify builder.timeout.no-reassign]
      "build-timeout"
      "gc-dry-run"
      # r[verify store.gc.tenant-retention]
      "gc-sweep"
      # r[verify builder.upload.references-scanned]
      # r[verify builder.upload.deriver-populated]
      # r[verify store.gc.two-phase]
      "refs-end-to-end"
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

  vm-lifecycle-autoscale-k3s = lifecycleMod.mkTest {
    name = "autoscale";
    subtests = [
      # r[verify ctrl.pool.ephemeral]
      # r[verify ctrl.ephemeral.intent-deadline]
      # r[verify ctrl.crd.host-users-network-exclusive]
      # ~180s: two builds × (reconcile tick + pod schedule + FUSE +
      # heartbeat + build + exit). Subtest deletes the default x86-64
      # Pool first so it doesn't steal dispatch.
      "ephemeral-pool"
    ];
    # ephemeral ~180s.
    globalTimeout = 1400;
  };

  #
  # Own split (not folded into autoscale): fresh fixture → clean
  # state → fast finalizers. ~4min boot + ~3min subtests.
  # r[verify ctrl.scaler.component]
  # r[verify ctrl.scaler.ratio-learn+2]
  # r[verify store.admin.get-load]
  # r[verify obs.metric.store-pg-pool]
  #   ComponentScaler e2e: CR status populated → 30-leaf slowFanout
  #   drives predicted=ceil(30/seedRatio=10)=3 → store Deployment
  #   /scale patched > min within 90s; controller pod restart
  #   preserves .status.learnedRatio; helm-rendered store has no
  #   .spec.replicas under any manager except the reconciler's.
  #
  # Own scenario (not a lifecycle fragment): needs componentScaler.
  # store.enabled=true in the fixture, which changes the rendered
  # store Deployment shape — would invalidate every other lifecycle
  # subtest's "store has 1 replica" assumption. seedRatio=10 +
  # min=1 max=4 keeps the scale-up provable inside the 2-node VM's
  # pod budget.
  vm-componentscaler-k3s = componentscaler {
    inherit pkgs common;
    fixture = k3sFull {
      extraValuesTyped = {
        "componentScaler.store.enabled" = true;
        "componentScaler.store.min" = 1;
        "componentScaler.store.max" = 4;
        "componentScaler.store.seedRatio" = 10;
      };
    };
  };

  vm-lifecycle-pool-k3s = lifecycleMod.mkTest {
    name = "pool";
    subtests = [
      # r[verify ctrl.pool.reconcile]
      # r[verify ctrl.crd.pool]
      "pool-lifecycle"
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
    subtests = [
      "build-during-failover"
      # r[verify sched.lease.k8s-lease]
      # r[verify sched.lease.generation-fence]
      #   True ungraceful death: SIGKILL the leader's host PID via
      #   crictl (no SIGTERM, no step_down, no FIN). Kubelet restarts
      #   the container in-place; restarted process sees holder==our_id
      #   → Renew (tx+0), OR standby observed-rv-expiry steals (tx+1)
      #   if restart >TTL. The `failover` subtest does NOT reach the
      #   observed-record-expiry branch — step_down wins the SIGTERM
      #   race post-a5b06ef. Ordered after build-during-failover:
      #   reuses its sshKeySetup (ssh-keygen is not idempotent).
      "sigkill-mid-build"
    ];
  };

  # r[verify sched.admin.create-tenant]
  # r[verify sched.admin.list-tenants]
  # r[verify sched.admin.list-executors]
  # r[verify sched.admin.list-builds]
  # r[verify sched.admin.clear-poison]
  # r[verify cli.cmd.sla]
  # rio-cli had 0% coverage — never invoked by any test. This runs
  # status + create-tenant + list-tenants against the live scheduler's
  # AdminService. ~5min (mostly k3s bring-up).
  vm-cli-k3s = cli {
    inherit pkgs common;
    fixture = k3sFull { };
  };

  # r[verify dash.envoy.grpc-web-translate+3]
  #   gRPC-Web end-to-end via Cilium Gateway → scheduler tonic-web.
  #   curl with application/grpc-web+proto against the Cilium-
  #   provisioned Gateway Service; asserts DATA frame 0x00 prefix
  #   (unary ClusterStatus) + trailer frame prefix 80 00 00 00
  #   (streaming GetBuildLogs). The frame-prefix grep proves
  #   tonic-web doesn't buffer server-streams — load-bearing for
  #   WatchBuild / live log tail. ~6min (k3s bring-up + Cilium
  #   Gateway reconcile). No separate Envoy Gateway operator —
  #   Cilium's embedded envoy handles the GRPCRoute.
  # r[verify dash.auth.method-gate+2]
  #   The fixture doesn't set dashboard.enableMutatingMethods so the
  #   rio-scheduler-mutating HTTPRoute is absent — `kubectl get
  #   httproute rio-scheduler-mutating` fails. Proves the helm-template
  #   fail-closed holds at runtime through the operator's reconcile.
  # r[verify dash.journey.build-to-logs]
  #   The GetBuildLogs 0x80 trailer assertion proves server-streaming
  #   works through the nginx→Cilium Gateway→scheduler chain. Handler
  #   returns errors as in-stream items (not tonic Trailers-Only) so
  #   tonic-web encodes them as 0x80 body frames browser fetch can read.
  vm-dashboard-gateway-k3s = dashboard-gateway {
    inherit pkgs common;
    fixture = k3sFull { gatewayEnabled = true; };
  };

  # Builder + store egress NetworkPolicy: IMDS + public internet + k8s
  # API all blocked. networkPolicy.enabled via extraValues (--set-string
  # "true" is truthy for `{{ if }}`).
  # vmtest-full.yaml defaults it to false; the override renders
  # networkpolicy.yaml into 02-workloads.yaml.
  # Cilium enforces (eBPF) — k3s's bundled kube-router netpol controller
  # is disabled (--disable-network-policy in k3s-full.nix).
  #
  # r[verify store.netpol.egress+2]
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

  # Cilium WireGuard transparent encryption: cilium_wg0 peers + encrypt
  # status on both nodes. Data-plane property the security model leans
  # on (rio components speak plaintext gRPC). Also asserts the GUA-v6 NodePort
  # frontend exists (regression guard for the EKS NLB RST bug — auto-
  # detect skips global-unicast; in-cluster tests are socket-LB false
  # positives).
  # r[verify sec.transport.cilium-wireguard]
  vm-cilium-encrypt-k3s = cilium-encrypt {
    inherit pkgs common;
    fixture = k3sFull { };
  };

  # 2×2 ingress/egress on the v6-only k3s fixture. client-v6 → NodePort
  # direct, client-v4 → edge:22 socat → NodePort over v6; both nix-build
  # over ssh-ng. Then egress: k3s host reaches upstream-v4 via 64:ff9b::
  # (Jool), pod resolves upstream-v4 → 64:ff9b:: AAAA (CoreDNS dns64).
  # ~6min (k3s bring-up + two trivial builds + curl probes).
  # r[verify gw.ingress.v6-direct]
  # r[verify gw.ingress.v4-via-nat]
  vm-ingress-v4v6-k3s = ingress-v4v6 {
    inherit pkgs common;
    fixture = k3sFull { };
  };

  # ADR-019 builder/fetcher split end-to-end. FIRST test running both
  # kind=Builder + kind=Fetcher pods. Proves: FOD→fetcher routing, non-
  # FOD→builder routing, builder airgap holds, fetcher egress open but
  # IMDS-blocked, fetcher node-dedication wired.
  #
  # Fetcher pod needs the nonpriv path (hard-coded privileged:false +
  # Localhost seccomp at reconcilers/pool/mod.rs Fetcher arm) — same
  # nonpriv overlay as vm-security-nonpriv-k3s. Seccomp profile
  # delivered via systemd-tmpfiles (k3sBase, same as the NixOS
  # AMI). The kind=Fetcher pool is enabled via extraValuesFiles with
  # name="x86-64-fetcher" and image=rio-fetcher (per-component ref
  # from the vmTestSeed preload). Systems
  # includes "builtin" so builtin:fetchurl's system=builtin passes
  # hard_filter(). nodeSelector/tolerations left
  # at reconciler defaults — scenario labels k3s-agent at runtime.
  #
  # r[verify sched.dispatch.fod-to-fetcher]
  #   dispatch-fod+nonfod subtest: one nix-build, FOD routes to
  #   fetcher pod, consumer routes to builder pod. Wrong routing →
  #   queue-forever → timeout. kubectl-logs grep confirms placement.
  # r[verify builder.netpol.airgap]
  #   builder-airgap subtest: builder netns curl to upstream-v4 via
  #   NAT64 (64:ff9b::<v4>:80) → rc≠0. Positive control: scheduler
  #   ClusterIP connects (NetPol allow fires).
  # r[verify fetcher.netpol.egress-open+2]
  #   fetcher-egress + fetcher-imds-blocked subtests: SAME origin,
  #   fetcher netns → rc==0 (toEntities:[world]:80 allow fires). Then
  #   IMDS → rc≠0 (host entity, NOT world → denied). The origin probe
  #   is the non-vacuous differentiator vs builder.
  # r[verify fetcher.node.dedicated]
  #   fetcher-node-dedicated subtest: pod spec has the rio.build/
  #   fetcher toleration + nodeSelector (reconciler defaults), and
  #   actually scheduled on the labeled k3s-agent node. Karpenter
  #   NodePool enforcement is EKS-only; this proves the params→
  #   podspec chain.
  # r[verify fetcher.nixconf.hashed-mirrors]
  #   fod-dead-origin subtest: flat-hash FOD with a 404 origin URL
  #   builds via {mirror}/sha256/{hex}. nixConf.hashedMirrors below
  #   points the rio-nix-conf ConfigMap at the in-VM upstream-v4 node
  #   (reached via DNS64+NAT64 from the v6-only fetcher pod).
  # r[verify builder.fod.verify-hash]
  #   fod-dir subtest: recursive-hash FOD with directory output
  #   (`mkdir $out`). Regression: a whiteout at the output path
  #   makes overlayfs mkdir return EIO.
  # r[verify builder.fuse.jit-lookup]
  #   fod-fail subtest: failing FOD propagates within 60s. Daemon's
  #   post-fail stat($out) hits FUSE; NotInput → ENOENT without
  #   store contact. P0308 hang would push elapsed past timeout 90.
  vm-fetcher-split-k3s = fetcher-split {
    inherit pkgs common drvs;
    fixture = k3sFull {
      extraValues = {
        "networkPolicy.enabled" = "true";
        "nixConf.hashedMirrors" = "http://upstream-v4/";
      };
      # pools via values file (not --set-string) so types stay correct.
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml
        (pkgs.writeText "fetcher-pool-vm.yaml" ''
          pools:
            - name: x86-64
              kind: Builder
              systems: [x86_64-linux]
              maxConcurrent: 2
            - name: x86-64-fetcher
              kind: Fetcher
              image: rio-fetcher
              # builtin:fetchurl FOD has system=builtin.
              systems: [x86_64-linux, builtin]
              maxConcurrent: 1
              # CEL forbids privileged/seccomp for Fetcher; null-clear
              # so poolDefaults inheritance doesn't trip admission.
              # hostUsers inherits poolDefaults.hostUsers:true (k3s
              # containerd cgroup-chown gap; vmtest-full-nonpriv.yaml).
              privileged: null
              seccompProfile: null
        '')
      ];
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
    fixture = k3sFull { gatewayEnabled = true; };
  };
}
