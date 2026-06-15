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
  # The rio-eval eval-parent binary (nix/rio-eval.nix), the C++ half
  # of the `rio build` pair for vm-build-client-standalone.
  rioEval,
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
  quota-probe = import ./scenarios/quota-probe.nix;
  kubelet-projquota = import ./scenarios/kubelet-projquota.nix;
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
  standby-burst = import ./scenarios/standby-burst.nix;
  cli = import ./scenarios/cli.nix;
  dashboard-gateway = import ./scenarios/dashboard-gateway.nix;
  netpol = import ./scenarios/netpol.nix;
  ingress-v4v6 = import ./scenarios/ingress-v4v6.nix;
  fetcher-split = import ./scenarios/fetcher-split.nix;
  chaos = import ./scenarios/chaos.nix;
  ca-cutoff = import ./scenarios/ca-cutoff.nix;
  put-path-chunked = import ./scenarios/put-path-chunked.nix;
  build-client = import ./scenarios/build-client.nix;
  substitute = import ./scenarios/substitute.nix;
  log-service = import ./scenarios/log-service.nix;
  substitute-scale = import ./scenarios/substitute-scale.nix;
  wipe-burst = import ./scenarios/wipe-burst.nix;
  materialize = import ./scenarios/materialize.nix;
  materialize-failover = import ./scenarios/materialize-failover.nix;
  sla-sizing = import ./scenarios/sla-sizing.nix;
  forecast-provisioning = import ./scenarios/forecast-provisioning.nix;
  kwok = import ./fixtures/kwok.nix { inherit pkgs; };
  mountd = import ./scenarios/mountd.nix;
  # castore-fuse exports { fuseClientModule, mkTest } — the client VM
  # doubles as the FUSE/mountd machine, so the node config and the
  # testScript that depends on it live in the same file.
  castore-fuse = import ./scenarios/castore-fuse.nix { inherit pkgs common; };
  # xfstests ports against the castore-FUSE — reuses castore-fuse's
  # fuseClientModule (the client doubles as the FUSE/mountd machine).
  # Selection + tiering: rio-builder/tests/xfstests_port/PLAN.md.
  castore-fuse-xfstests = import ./scenarios/castore-fuse-xfstests.nix { inherit pkgs common; };
  castore-e2e = import ./scenarios/castore-e2e.nix;
  drvs = import ./lib/derivations.nix { inherit pkgs; };

  # SLA-sizing fixture: one worker with RIO_BUILDER_SCRIPT pointing at
  # the scripted-telemetry TOML, scheduler with [sla] configured + the
  # InjectBuildSample fixture gate. tickIntervalSecs=2 so the estimator
  # refit fires fast enough for wait_until_succeeds.
  slaSizingFixture = standalone {
    # The scripted-telemetry intercept fires AFTER the castore mount +
    # overlay setup (executor/mod.rs), so even these fake builds need
    # the tenant-scoped DAG prefetch to succeed — see the
    # vm-castore-e2e fixture comment.
    defaultTenant = "vmtest";
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
        hw_explore_epsilon = 0.0
        max_cores = 64
        max_mem = 274877906944
        max_disk = 214748364800
        default_disk = 21474836480
        reference_hw_class = "intel-7-ebs-mid"

        [[sla.tiers]]
        name = "normal"
        p90 = 1200

        [sla.probe]
        cpu = 4
        mem_per_core = 2147483648
        mem_base = 4294967296

        [sla.hw_classes.intel-6-ebs-lo]
        labels = [{ key = "rio.build/hw-class", value = "intel-6-ebs-lo" }]
        requirements = [{ key = "kubernetes.io/os", operator = "In", values = ["linux"] }]
        node_class = "rio-default"
        max_cores = 64
        max_mem = 17179869184
        [sla.hw_classes.intel-7-ebs-mid]
        labels = [{ key = "rio.build/hw-class", value = "intel-7-ebs-mid" }]
        requirements = [{ key = "kubernetes.io/os", operator = "In", values = ["linux"] }]
        node_class = "rio-default"
        max_cores = 64
        max_mem = 17179869184
        [sla.hw_classes.intel-8-nvme-hi]
        labels = [{ key = "rio.build/hw-class", value = "intel-8-nvme-hi" }]
        requirements = [{ key = "kubernetes.io/os", operator = "In", values = ["linux"] }]
        node_class = "rio-default"
        max_cores = 64
        max_mem = 17179869184
      '';
    };
    extraPackages = [
      pkgs.postgresql
      pkgs.grpcurl
    ];
  };

  # Shared fixture for both scheduling splits — identical VM topology.
  schedulingFixture = standalone {
    # Builds materialize inputs through the tenant-scoped castore reads
    # — see the vm-castore-e2e fixture comment. grpcurl-direct submits
    # (cancel-timing) pick the same tenant up via
    # scheduling.nix:submit_build_grpc.
    defaultTenant = "vmtest";
    workers = {
      # maxSilentTime enforcement on ALL scheduling workers. Every drv
      # that lands here MUST stay non-silent for ≥10s — the seeded
      # builds either sleep ≤3s or echo every ≤5s.
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
          # Passthrough disabled: castore open() replies KEEP_CACHE and
          # serves read() from userspace (open_mode_total{keep_cache}
          # path) — passthrough would bypass the kernel read callback
          # entirely, leaving that branch uncovered in VM runs.
          RIO_DISABLE_PASSTHROUGH = "true";
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
      # [sla] is mandatory. ProbeShape::validate requires
      # cpu ∈ [4, max_cores/4] (span≥4 reach for both explore paths) →
      # max_cores ≥ 16. mem_per_core/mem_base sized so a 4-core probe
      # (4×128Mi + 256Mi = 768Mi) fits the 2 GiB worker VMs. Mirrors
      # values/vmtest-full.yaml.
      extraConfig = ''
        [sla]
        default_tier = "normal"
        hw_cost_source = "static"
        reference_hw_class = "vmtest"
        max_cores = 16
        max_mem = 2147483648
        max_disk = 6442450944
        default_disk = 2147483648

        [[sla.tiers]]
        name = "normal"

        [sla.probe]
        cpu = 4
        mem_per_core = 134217728
        mem_base = 268435456

        [sla.hw_classes.vmtest]
        labels = [{ key = "rio.build/hw-class", value = "vmtest" }]
        requirements = [{ key = "kubernetes.io/os", operator = "In", values = ["linux"] }]
        node_class = "rio-default"
        max_cores = 16
        max_mem = 2147483648
      '';
    };
    # grpcurl: kept for the plaintext gRPC :9001 admin/scheduler
    # probes the remaining subtests use (no withHmac).
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
  #
  # defaultTenant: P0560 stopgap — installs the rio_vmtest_* triggers
  # in the k3s PG so vm-lifecycle builds get path_tenants rows for
  # their castore inputs (the scenario keeps its own JWT/tenant
  # attribution on top; gc-tenant-test has a real retention window and
  # does its own input attribution in gc-sweep.nix).
  lifecycleMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull {
      jwtEnabled = true;
      defaultTenant = "vmtest";
    };
  };

  # Pull-canary lifecycle module: same scenario module as lifecycleMod,
  # but its own fixture instantiation — values/vmtest-pull-canary.yaml
  # pins scheduler.sla.probe.deadlineSecs to the 180s config floor so
  # the pull-mode establishment window (solved deadline + 120s report
  # slack ≈ 300s) fits a VM-test budget. (The overlay's former
  # poolDefaults.dispatchMode=Stream pin retired with the scenario's
  # stream-baseline arm at the 1c' session-machinery deletion.) Kept
  # OUT of the shared lifecycleMod fixture so the other lifecycle
  # splits keep vmtest-full.yaml's 3600s probe deadline (their Jobs'
  # activeDeadlineSeconds and worker timeouts are unchanged).
  lifecyclePullCanaryMod = lifecycle {
    inherit pkgs common;
    fixture = k3sFull {
      jwtEnabled = true;
      defaultTenant = "vmtest";
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-pull-canary.yaml
      ];
    };
  };

  leMod = leader-election {
    inherit pkgs common;
    # build-during-failover / sigkill-mid-build run real builds —
    # P0560 stopgap, see the vm-castore-e2e fixture comment.
    fixture = k3sFull { defaultTenant = "vmtest"; };
  };

  # ── standby-burst scenario builder (round-9 live_055(e)) ────────────
  # Scheduler lease stability under a store scale-out burst at
  # replicas=2 + the standby's Trailers-Only Unavailable posture. The
  # KEDA closed loop is EKS-only (no operator in the airgapped image
  # set) — the scenario drives the same actuation via kubectl scale;
  # see the scenario header for the disclosed VM-scale limits.
  standbyBurstMod = standby-burst {
    inherit pkgs common;
    fixture = k3sFull { defaultTenant = "vmtest"; };
  };

  # Prod-parity lifecycle module. No jwtEnabled — bootstrap-job-ran +
  # bootstrap-tenant don't touch JWT. Bare k3sProdParity {} just flips
  # bootstrap on and preloads the rio-bootstrap image.
  lifecycleProdParityMod = lifecycle {
    inherit pkgs common;
    fixture = k3sProdParity { };
  };

  # ── substitute scenario builder (both flag states — PD-B3/PD-B13) ────
  # One builder, two attrs: the materialization flag is threaded to BOTH
  # the scenario (selects the assertion branch) and the fixture (sets the
  # deployment env), so each attr's assertions and deployment posture
  # flip together — a commit can never leave either attr red because the
  # fixture flipped without its assertions (commit rule 3).
  #
  # Fixture notes (shared by both attrs):
  #   - Service-HMAC so the scheduler's substitution probe / walk mints
  #     `x-rio-service-token` + `x-rio-probe-tenant-id`; without it the
  #     store's `request_tenant_id` returns None → NotFound. Flag-on, the
  #     same key authenticates the store's materialization executor
  #     against the scheduler's ExecutorService.
  #   - Gateway-only signing seed so mint_session_jwt works (ssh auth →
  #     resolve_tenant → mint JWT → attach to outbound gRPC; P0465).
  #   - grpcurl + postgresql on control for direct probing + psql.
  #   - Client :8080 open for the fake-upstream http.server.
  substituteStandaloneTest =
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
        withHmac = true;
        extraStoreConfig = {
          signingKeyFile = "${rioSigningKey}";
          extraConfig = ''
            # substitute-stall-abort fixture: shrink the owner-side
            # stall window (default 180 s) so the netem-wedged subtest
            # aborts in about a minute. 60 s is the validation FLOOR
            # (substitute_stall >= 2x the 30 s placeholder heartbeat,
            # merged_bug_082; the placeholderClaimSubHeartbeatWindow
            # quint regime shows sub-2x windows depose live advancing
            # owners on stamp lag) — still ≫ the per-read cadence under
            # the 8mbit throttle and ≪ the detached fetch's
            # -max-time 240. TOP-LEVEL key — it must precede the [jwt]
            # table header or TOML parses it as jwt.substitute_stall_secs
            # and store config load fails at boot. Scenario-wide: this
            # window also governs every other subtest's NAR
            # response-header wait and per-read clock — safe on the
            # VM-local network (ms-scale reads), but a future
            # heavyweight subtest in this scenario inherits it.
            substitute_stall_secs = 60
            [jwt]
            key_path = "${jwtPubkey}"
          '';
        };
        extraGatewayEnv.RIO_JWT__KEY_PATH = "${jwtSeed}";
        extraPackages = [
          pkgs.grpcurl
          pkgs.postgresql_18
        ];
        extraClientModules = [
          { networking.firewall.allowedTCPPorts = [ 8080 ]; }
        ];
      };
    };

  # ── materialize scenario builder (T-3.1) ────────────────────────────
  # The §2.4 routing-arms / §2.5 park / §5.3 gc-pin scenario
  # (scenarios/materialize.nix) — the substitution failure paths the
  # substitute scenarios' success paths never reach. Fixture knobs that
  # are load-bearing for determinism:
  #   - withHmac: the consumption re-probe can only CONFIRM a missing
  #     path with service-auth probes (x-rio-service-token +
  #     x-rio-probe-tenant-id); without it, arm 3's fail-fast is
  #     structurally unreachable (B3's conservative direction) and the
  #     scenario would hang instead of failing fast.
  #   - tickIntervalSecs=600: keeps the dispatch-time re-probe
  #     (housekeeping-advanced probe generation) out of every subtest
  #     window, so flag-on the §2.4 consumption routing is the ONLY
  #     decision path for reported outcomes — mechanism assertions
  #     (job end-states) are deterministic, not racing the as-built
  #     dispatch-probe fail-fast cell. All subtest progress is
  #     event-driven (claim/report/consumption, park-expiry re-claim,
  #     completion hooks), never tick-driven.
  #   - PARK_BACKOFF_BASE_SECS=5: test-speed park expiry (default 30 s).
  #   - Store [jwt] config: the scenario submits gRPC-direct WITH a
  #     minted tenant JWT, because the topdown-prune probe
  #     (check_roots_topdown) propagates ONLY the client JWT to the
  #     store — without it the prune can never fire and the
  #     marked-root fail-fast sequence is unreachable. The store
  #     verifies that JWT for its upstream probes (the same
  #     gateway-JWT-propagation path substitute.nix exercises).
  materializeStandaloneTest =
    let
      jwtKeys = import ./lib/jwt-keys.nix;
      jwtPubkey = pkgs.writeText "jwt-pubkey" jwtKeys.pubkeyB64;
    in
    materialize {
      inherit pkgs common;
      fixture = standalone {
        workers = { };
        withHmac = true;
        extraStoreConfig = {
          # [jwt]: verify the scenario's minted tenant tokens (see the
          # builder comment above).
          #
          # [materialization] poll_interval_secs=3600: the executor
          # polls (and therefore claims) ONLY at store startup — each
          # claim wave is an explicit `restart_store()` step in the
          # scenario, so a claim can never be in flight when the
          # scenario restarts the store and no open attempt can be
          # orphaned mid-execution (establishment is tick-driven and
          # the tick is 600 s here). This file key only paces the
          # executor.
          extraConfig = ''
            [jwt]
            key_path = "${jwtPubkey}"

            [materialization]
            poll_interval_secs = 3600
          '';
        };
        extraSchedulerEnv = {
          RIO_MATERIALIZATION__PARK_BACKOFF_BASE_SECS = "5";
        };
        extraSchedulerConfig = {
          tickIntervalSecs = 600;
        };
        extraPackages = [
          pkgs.grpcurl
          pkgs.postgresql_18
        ];
        extraClientModules = [
          { networking.firewall.allowedTCPPorts = [ 8080 ]; }
        ];
      };
    };

  # ── materialize-failover scenario builder (T-3.3) ───────────────────
  # Materialization under leader failover on the k3s fixture (2
  # scheduler replicas per vmtest-full.yaml — the failover needs a
  # standby).
  materializeFailoverTest = materialize-failover {
    inherit pkgs common;
    fixture = k3sFull {
      jwtEnabled = true;
      defaultTenant = "vmtest";
    };
  };

  # ── substitute-scale scenario builder ───────────────────────────────
  # Substitution → autoscaling-signal path on the k3s fixture; the
  # deployed materialization posture comes straight from the chart's
  # values.yaml default (no --set override — the rendered env IS the
  # default-plumb proof). The KEDA closed loop itself is EKS-only
  # (no operator in the airgapped image set; helm fragment
  # 26-store-scaling.sh covers the ScaledObject render); the scenario
  # asserts the SIGNAL KEDA consumes — the scheduler's backlog gauge.
  substituteScaleTest = substitute-scale {
    inherit pkgs common;
    fixture = k3sFull {
      jwtEnabled = true;
      defaultTenant = "vmtest";
      extraValuesTyped = {
        # live_047/R-C: 2 is the validate() floor (cap 1 would make the
        # executor path-slot pool P = cap/2 = 0 and is boot-rejected).
        # P = 1 slot-serializes the walks, which preserves this
        # scenario's stretched-cascade intent — see the scenario-builder
        # comment below.
        "store.substituteAdmissionPermits" = 2;
      };
    };
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

  # r[verify infra.node.kubelet-prjquota+1]
  # r[verify sys.gate.static-cadence-witness]
  #   The merged_bug_024 AMI-variant dimension + the merged_bug_045
  #   timing dimension: rio-kubelet-mount (settled-udev dispatcher)
  #   against the REAL udev by-id namespaces — fake-root disks
  #   partitioned to the locked make-disk-image.nix recipes
  #   (legacy+gpt: never-mounted no-fs bios_grub; efi: mounted ESP),
  #   drift-pinned against the producer source; by-id links DELAYED
  #   15s past every early unit's job-start instant (the
  #   deterministic coldplug window). Asserts the typed predicate
  #   selects the quota volume on BOTH EBS variants (pre-fix ami-bios
  #   counted n_bare=2 -> kubelet Requires= hard-fail every boot),
  #   the by-class rejection trail, bios_grub never formatted, the
  #   instance-store node classified ONCE on the settled view (RAID0
  #   branch; the EBS branch provably never ran), and kubelet active
  #   on all three (the Requires= consequence tier). The unit-tier
  #   plant battery is misc-checks `quota-volume-select`.
  vm-ami-variant-quota = import ./scenarios/ami-variant-quota.nix { inherit pkgs; };

  # ── rio-mountd (P0567): the privileged broker, end-to-end ───────────
  # The real rio-mountd binary against an XFS-prjquota staging loopback,
  # driven over the SOCK_SEQPACKET protocol by spike_mountd_client.
  # Subtest map and why perf is printed-not-gated: the scenario header.
  # r[verify builder.mountd.fuse-handoff]
  # r[verify builder.mountd.backing-broker]
  # r[verify builder.mountd.concurrency]
  # r[verify builder.mountd.build-id-validated]
  # r[verify builder.mountd.build-id-unique]
  # r[verify builder.mountd.one-mount]
  # r[verify builder.mountd.staging-quota]
  # r[verify builder.mountd.promote-verified]
  # r[verify builder.mountd.promote-bounded-copy]
  # r[verify builder.mountd.orphan-scan]
  vm-mountd = mountd { inherit pkgs common; };

  # ── castore-FUSE (ADR-022 §2): production session against a real store ─
  # serve-castore (spike_mountd_client) drives the production
  # castore_fuse::session assembly on the client VM: rio-mountd fd
  # handoff, tenant-scoped Directory-DAG prefetch from rio-store,
  # whole-file and streaming reads, shared-cache passthrough hits, and
  # clean SIGTERM teardown — all over fuse-over-io_uring, the session's
  # only transport (the client VM boots with fuse.enable_uring=1; the
  # kernel routes ALL requests over the rings once ready, so the
  # cache-hit phase's passthrough/backing-id assertions double as the
  # proof that open() replies carry backing ids over the ring). The
  # uring-required leg flips enable_uring off at runtime and asserts
  # the mount fails hard with the kernel requirement named — same code
  # path as a pre-6.14 kernel. Subtest map: the scenario header.
  # r[verify builder.fs.digest-fuse-open]
  # r[verify builder.fs.shared-backing-cache]
  # r[verify builder.fs.io-uring-transport]
  # r[verify builder.fs.io-uring-required]
  # r[verify builder.fs.castore-inode-digest+2]
  vm-castore-fuse = castore-fuse.mkTest {
    fixture = standalone {
      # P0560 stopgap (implies HMAC, so the store can verify the
      # x-rio-assignment-token the testScript mints and the gateway gets
      # the service key for the seed/build uploads). The scenario's seed
      # builds dispatch to the worker, whose per-build castore mount needs
      # the full tenancy chain (tenant row + key-comment attribution +
      # tenant claim in the assignment token + path_tenants rows for the
      # inputs) — without it the DAG prefetch is rejected with
      # "assignment token has no tenant claim". The serve-castore
      # subtests still create their own 'castore-vm' tenant + token on
      # top; path_tenants is keyed (hash, tenant) so both attributions
      # coexist.
      defaultTenant = "vmtest";
      # psql() on control for the scenario's own tenant + path_tenants
      # setup.
      extraPackages = [ pkgs.postgresql_18 ];
      extraClientModules = [ castore-fuse.fuseClientModule ];
    };
  };

  # ── xfstests ports against the castore-FUSE ─────────────────────────
  # POSIX-conformance checks (readdir resume, byte-exact names, ELOOP,
  # exec/access semantics, write protection, errno contracts, read
  # integrity) re-expressed from xfstests tests/generic/ as Rust syscall
  # probes (the xfstests_runner binary) against a serve-castore mount of
  # a purpose-built fixture tree, plus one dispatched build for the
  # overlay-lowerdir leg. The testScript is a thin harness; every
  # filesystem assertion lives in the runner. Same fixture shape as
  # vm-castore-fuse (the wiring rationale above applies unchanged);
  # ranked selection: rio-builder/tests/xfstests_port/PLAN.md.
  #
  # Passthrough stacking: the consumer build dispatched to the worker
  # reads the dep through the production overlay-over-castore mount
  # (overlay on FUSE = depth 2, only mountable with max_stack_depth=1
  # negotiated), and the runner's warm-read checks exercise
  # BACKING_OPEN passthrough against the depth-0 ext4 backing.
  # r[verify builder.fs.passthrough-stack-depth]
  vm-castore-xfstests = castore-fuse-xfstests.mkTest {
    fixture = standalone {
      defaultTenant = "vmtest";
      extraPackages = [ pkgs.postgresql_18 ];
      extraClientModules = [ castore-fuse.fuseClientModule ];
    };
  };

  # ── castore cutover e2e (P0560): scheduler-dispatched builds over the
  # per-build castore-FUSE lower, on the reworked NixOS worker module
  # (rio-mountd as a host systemd service). Subtest map: the scenario
  # header. Markers here cover the rules this scenario genuinely proves
  # end-to-end: the build's /nix/store stack (overlay over the per-build
  # castore mount, fed by the mountd fd handoff in the load-bearing
  # order — a wrong order deadlocks the cold build).
  # r[verify builder.fs.castore-stack+1]
  # r[verify builder.fs.fd-handoff-ordering]
  # r[verify builder.overlay.castore-lower]
  vm-castore-e2e = castore-e2e {
    inherit pkgs common;
    fixture = standalone {
      # The castore read surface (DirectoryService/ReadBlob/StatBlob) is
      # tenant-scoped and refuses anonymous callers, so dispatched builds
      # only materialize inputs when the whole tenancy chain is wired:
      # tenant row + key-comment attribution + HMAC assignment token +
      # gateway/store JWT mode for the client-side seed/.drv uploads.
      defaultTenant = "vmtest";
      workers = {
        worker = {
          extraServiceEnv = {
            # 64 KiB threshold so the scenario's 300 KiB input exercises
            # the streaming-open path (and fills /var/rio/chunks)
            # without needing a multi-MiB input under TCG.
            RIO_STREAM_THRESHOLD = "65536";
          };
        };
      };
      # Snappier dispatch + re-dispatch after the deliberate
      # store-outage infrastructure failure.
      extraSchedulerConfig = {
        tickIntervalSecs = 2;
      };
    };
  };

  # r[verify gw.conn.exit-status+3]
  #   nom-exit subtest: client ssh_config has ControlMaster auto +
  #   ControlPersist 600. `timeout 60 nom build` must exit 0 (gateway
  #   sends exit-status before eof); `connections_active` must return
  #   to 0 within 90s (gateway disconnects only after the 60s
  #   empty-connection grace once the last protocol session ends — NOT
  #   on last-session-close, which would kill a ControlMaster
  #   mid-batch); `ssh gateway echo` (rejected exec) must exit ≠124.
  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      # The trivial build's inputs (seeded busybox + the .drv) are served
      # to the worker through the tenant-scoped castore-FUSE read path —
      # see the vm-castore-e2e fixture comment.
      defaultTenant = "vmtest";
      extraClientModules = [
        {
          environment.systemPackages = [ pkgs.nix-output-monitor ];
          programs.ssh.extraConfig = ''
            Host *
              ControlMaster auto
              ControlPath /tmp/cm-%C
              ControlPersist 600
          '';
        }
      ];
    };
    cold = false;
    withNomExitTest = true;
  };

  # r[verify gw.compat.version-range+2]
  #   Identical to vm-protocol-warm-standalone but the client VM runs
  #   Lix. Lix is policy-frozen at daemon protocol 1.35, so this
  #   exercises rio's MIN_CLIENT_VERSION floor and the ≥1.37
  #   BuildResult.cpu_* gate against a real ssh-ng client end-to-end.
  #   Single Lix VM test in checks — wire-level Lix-as-daemon coverage
  #   lives in checks.golden-lix (golden_conformance against pkgs.lix).
  vm-protocol-warm-lix-standalone = protocol {
    inherit pkgs common;
    nameSuffix = "-lix";
    fixture = standalone {
      # Same tenancy stopgap as vm-protocol-warm-standalone above.
      defaultTenant = "vmtest";
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
      # CA chain inputs are read through the tenant-scoped castore —
      # see the vm-castore-e2e fixture comment.
      defaultTenant = "vmtest";
    };
  };

  # ── PutPathChunked end-to-end (ADR-022 §6, P0586) ────────────────────
  # Real scheduler-dispatched builds on a real worker upload through
  # PutPathChunked with scheduler-minted HMAC assignment claims; subtest
  # map and the omitted (handler-level-covered) cases: the scenario
  # header. withHmac: the chunked claims checks (deriver hash,
  # input-closure digest, expected_outputs, is_ca) and the authenticated
  # HasChunks probe only exist with real assignment tokens.
  # r[verify builder.upload.chunked-manifest]
  # r[verify builder.upload.batch+3]
  # r[verify store.put.chunked-ca]
  # r[verify store.chunk.has-chunks-tenant]
  # (the cross-derivation dedup subtest proves the positive arm end to
  # end: build A's completion binds the vmtest tenant's chunk_tenants
  # rows, and build B's tenant-scoped HasChunks probe then dedups; the
  # cross-tenant negative arm is unit-tested in chunk_service.rs)
  vm-put-path-chunked = put-path-chunked {
    inherit pkgs common;
    fixture = standalone {
      withHmac = true;
      # P0560 tenancy stopgap (P0593 deletes): the worker's tenant-scoped
      # castore reads need the seeded inputs and the consumer .drv to be
      # tenant-visible, like every other build-running scenario.
      defaultTenant = "vmtest";
      # psql() on control for the narinfo refs/deriver assertions.
      extraPackages = [ pkgs.postgresql_18 ];
    };
  };

  # ── rio build end-to-end (ADR-024 P3): the real coordinator+rio-eval
  # pair on the client VM against the real cluster — external-door JWT
  # auth, chunked source upload, drv-blob negotiation, digest-bearing
  # SubmitBuild, worker execution, BuildEvents render, --out-link
  # fetch, and the cached-failure replay (fail-fast culprit attribution
  # + GetDerivationLog tail). Single-test scenario; markers at the
  # wiring point per P0341 convention — scenario header maps subtests
  # to rules.
  # r[verify bc.submit.all-acked]
  # r[verify bc.fetch.narhash-verify+2]
  # r[verify store.drv.getpath-fallback]
  # r[verify bc.render.stdout-results]
  # r[verify bc.render.plain-default]
  # r[verify bc.render.failure-log-tail]
  # r[verify sched.merge.failfast-culprit]
  # r[verify bc.upload.source-root-kinds]
  # r[verify bc.upload.cas-read]
  vm-build-client-standalone =
    let
      jwtKeys = import ./lib/jwt-keys.nix;
      jwtPubkey = pkgs.writeText "jwt-pubkey" jwtKeys.pubkeyB64;
    in
    build-client {
      inherit
        pkgs
        common
        rioEval
        ;
      fixture = standalone {
        # P0560 tenancy stopgap (P0593 deletes): the worker's
        # tenant-scoped castore reads need the client-uploaded sources
        # and outputs tenant-visible, like every build-running
        # scenario. Implies HMAC (assignment tokens).
        defaultTenant = "vmtest";
        # Snappier dispatch for the 2-node chain.
        extraSchedulerConfig = {
          tickIntervalSecs = 2;
        };
        # JWT verify on the scheduler: SubmitBuild/WatchBuild require
        # x-rio-tenant-token once a pubkey is configured
        # (require_tenant). Env (not extraConfig) so the [sla] block
        # in common.nix's scheduler defaults survives.
        extraSchedulerEnv.RIO_JWT__KEY_PATH = "${jwtPubkey}";
        # Poison on the FIRST failure so the cached-failure-replay
        # subtest reaches the merge fail-fast within four submissions
        # instead of needing per-attempt retries first. Env for the
        # same reason as the JWT key above.
        extraSchedulerEnv.RIO_POISON__THRESHOLD = "1";
        # JWT verify on the store's castore door (the client's
        # PutPathChunked/Has*/PutDrvBlobs/GetPath rung). extraConfig
        # REPLACES the mkControlNode default, so [chunk_backend] must
        # be restated — without it every builder output upload is
        # rejected with FAILED_PRECONDITION.
        extraStoreConfig.extraConfig = ''
          [chunk_backend]
          kind = "filesystem"
          base_dir = "/var/lib/rio/store/chunks"

          [jwt]
          key_path = "${jwtPubkey}"
        '';
        # psql() on control for the tenant UUID + drv_blobs assertions.
        extraPackages = [ pkgs.postgresql_18 ];
      };
    };

  vm-protocol-cold-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      # Same tenancy stopgap as vm-protocol-warm-standalone above —
      # the fetcher's FOD .drv and the consumer's inputs go through
      # the tenant-scoped castore reads.
      defaultTenant = "vmtest";
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
  # chain at the store-gRPC level plus the ssh-ng path (P0465 JWT
  # propagation). Fixture/JWT/signing notes live on
  # substituteStandaloneTest (the builder).
  #
  # Scheduler-owned substitution routes through materialization jobs:
  # merge probe → cache_opportunity job rows (in the merge transaction)
  # → store-executor claim/fetch/report → consumption. The walk never
  # spawns for fresh work (criterion 3 — through D3, when the walk
  # machinery deletes).
  #
  # r[verify sched.materialize.job+2]
  #   substitute-scheduler-owned: a direct (gateway-bypassing) submission
  #   of 4 substitutable nodes creates exactly 4 cache_opportunity jobs in
  #   the merge transaction; all resolve resolved_success; the
  #   jobs-created metric moves by 4 while the walk-spawn metric stays 0.
  # r[verify sched.materialize.routing+7]
  #   substitute-scheduler-owned + materialization-active: every job's
  #   Success outcome is consumed — nodes complete, the build succeeds,
  #   no unresolved jobs remain (all-resolved assertion).
  # r[verify sched.materialize.pinning]
  #   materialization-active: pin-at-ingest rows (pin_kind=
  #   'materialization') are released once the interested builds go
  #   terminal (§5.3 / T-1.8 wiring), with the scheduler's release log
  #   line as the non-vacuity witness.
  # r[verify gw.activity.subst-progress+4]
  #   substitute-progress-e2e: 4-path closure submitted via ssh-ng;
  #   captured internal-json wire stream asserts every actCopyPath
  #   start has a matching stop, every resProgress has done≤expected,
  #   and per-aid done is monotone non-decreasing — the §8-B
  #   basic-scenario pair-rendering obligation.
  # r[verify store.substitute.upstream]
  #   substitute-cold-fetch: miss → HTTP GET narinfo → sig-verify →
  #   GET nar → CAS ingest → narinfo INSERT. Metric + psql assertions.
  # r[verify store.substitute.sig-mode]
  #   substitute-sig-mode-add: sig_mode=add → BOTH upstream AND rio
  #   sigs in narinfo.signatures.
  # r[verify store.substitute.tenant-sig-visibility+2]
  # r[verify store.substitute.find-missing-gated]
  # r[verify store.api.batch-manifest+3]
  #   substitute-cross-tenant-gate: tenant C (untrusted key) → the
  #   typed FailedPrecondition refusal via QueryPathInfo/GetPath
  #   (merged_bug_005: C's own upstream serves the narinfo but C's
  #   keys can't verify it — present-but-untrusted is never a silent
  #   miss); still reported missing via FindMissingPaths (sig-blind
  #   HEAD leg, no refusal there); PermissionDenied via
  #   BatchGetManifest (builder-internal). Tenant B (trusts same key)
  #   → visible. Dynamic re-trust proves per-request trusted_keys read.
  # r[verify store.tenant.narinfo-filter]
  #   built-path-cross-tenant-gate: I-217 — gate hides A's BUILT path
  #   from non-owners. D (no upstream) → NotFound. B (has upstream) →
  #   visible via try_substitute_on_miss (B substitutes independently).
  # r[verify gw.opcode.query-missing]
  # r[verify gw.opcode.query-path-info]
  #   substitute-ssh-ng: gateway propagates JWT through wopQueryPathInfo
  #   → store's try_substitute_on_miss fires → path substitutable via
  #   the real ssh-ng protocol path (not grpcurl backdoor).
  # r[verify sched.merge.substitute-probe-indeterminate+2]
  #   substitute-scheduler-owned: the merge probe classifies 4
  #   substitutable (probe-indeterminate) nodes and routes them to
  #   materialization jobs created in the merge transaction (zero
  #   walks) — the indeterminate→job disposition at deployment level.
  # r[verify store.substitute.progress-heartbeat]
  #   substitute-stall-abort: psql observes durable fetched_bytes>0
  #   mid-transfer through the 30s placeholder heartbeat (the
  #   cross-process progress witness); the netem wedge freezing it
  #   while the connection stays alive is the stuck≠slow
  #   discrimination.
  # r[verify store.substitute.stall-abort+2]
  #   substitute-stall-abort: loss-100% wedge on a live ~96MiB
  #   transfer → owner-side abort at the 15s fixture window — journal
  #   warn pair, stale_reclaimed_total{reason="stall_abort"} == 1, row
  #   released in place (claim/progress NULL, stall_count=1,
  #   status='uploading' preserved).
  # r[verify store.substitute.stale-reclaim+4]
  #   substitute-stall-abort: the post-heal re-claim completes
  #   (status='complete' far below the 300s heartbeat-death threshold
  #   proves the released-in-place arm), with the stall_count=1 strike
  #   surviving the handoff and finalize.
  vm-substitute-standalone = substituteStandaloneTest;

  # ── materialization routing/park/gc-pin (T-3.1) ─────────────────────
  # The substitution FAILURE paths at deployment level: the §2.4
  # Unobtainable routing arms, the §2.5 infra park, and the §5.3 pin
  # lifecycle against a real GC sweep — what the substitute scenarios'
  # success paths never reach. One mode-switched fake upstream drives
  # every arm (per-path 404/503/head-only narinfo answers).
  #
  # r[verify sched.materialize.routing+7]
  #   routing-fail-fast: a topdown-pruned root whose output is confirmed
  #   missing upstream fails every interested build with the
  #   resubmit-directing error (arm 3: Unobtainable → consumption
  #   re-probe confirms → fail-fast); the job resolves
  #   resolved_unobtainable, exactly one materialization_unobtainable
  #   charge row, zero build-kind rows.
  #   routing-vouched-from-source: the same Unobtainable verdict on a
  #   node whose only dep is produced routes ResolveFromSource (arm 1) —
  #   the node returns to from-source eligibility, the build stays live,
  #   never a fail-fast.
  #   infra-park: upstream 5xx burns the materialization budget → the
  #   job parks (pending + park_until, claimable after backoff) and the
  #   build NEVER fails (B3); healing the upstream completes the build.
  # r[verify sched.materialize.pinning]
  #   gc-pin: pin-at-ingest rows (pin_kind='materialization') protect
  #   materialized paths from a real TriggerGC sweep while any
  #   interested build is live; the §5.3 all-interest-terminal release
  #   frees them and the next sweep collects. The unpinned control path
  #   collected in sweep #1 is the non-vacuity proof that the sweep ran.
  # r[verify sched.materialize.job+2]
  #   routing-fail-fast: the topdown prune creates the origin=pruned job
  #   inside the merge transaction (and the pruned dep never enters the
  #   DAG); routing-vouched-from-source: the indeterminate probe creates
  #   the cache_opportunity job (B3's optimistic creation).
  vm-materialization-standalone = materializeStandaloneTest;

  # ── log-service (standalone fixture, no workers) ─────────────────────
  # rio-store LogService end-to-end: authenticated AppendLog ingest →
  # filesystem-backed chunks + PG manifest → TailLog read-back → store
  # restart survival → a second session resuming the same execution →
  # cross-session dedup. No workers: grpcurl drives the store's gRPC
  # port directly the way the builder will from the cutover commit on.
  # withHmac so the binding gate verifies a REAL assignment token
  # against a seeded assignments/derivations row — the dev-mode path
  # would skip the gate entirely.
  vm-log-service-standalone = log-service {
    inherit pkgs common;
    fixture = standalone {
      workers = { };
      withHmac = true;
      extraStoreConfig = {
        extraConfig = ''
          [chunk_backend]
          kind = "filesystem"
          base_dir = "/var/lib/rio/store/chunks"
        '';
      };
      extraPackages = [
        pkgs.grpcurl
        pkgs.postgresql_18
      ];
    };
  };

  # prjquota satisfiability witness (merged_bug_074 / WO-S2-2, OQ-13):
  # a real XFS project quota filled to its hard limit, driven through
  # the production classifier chain (the quota_probe bin). Single
  # node, no k8s. Asserts: the kernel clamp on the in-project statvfs;
  # the retired coupled vantage's structural FALSE (the dead letter);
  # the decoupled vantage's TRUE (the letter fires); the post-cleanup
  # usage collapse that motivates the during-build peak monitor.
  # r[verify builder.disk.satisfiable-letter+2]
  # r[verify builder.disk.quota-classified+2]
  vm-quota-probe-standalone = quota-probe {
    inherit pkgs common;
  };

  # The kubelet-half END-TO-END witness (live_060-d, WO-S8-16): the
  # peak_disk_bytes producer chain from the node config the fleet
  # boots — prjquota volume (the WO-S8-15 provisioning shape) + the
  # kubelet featureGate + projid registry → a REAL hostUsers:false
  # emptyDir pod scheduled BY KUBELET → the production reader
  # (quota_probe) returns Some. ZERO manual projid assignment (the
  # discrimination quota-probe-standalone structurally lacks — it
  # witnesses the kernel half; this witnesses the kubelet half). The
  # un-provisioned twin node reproduces today's fleet (None) so the
  # red stays red.
  # live_063 extends it with the fourth-decline-mode cell: the same
  # provisioned node × the PRODUCTION hostUsers:true posture
  # (drift-pinned to values.yaml poolDefaults), where kubelet refuses
  # quota assignment and the BUILDER-MINTED projid (quota.rs
  # ensure_project_quota — the cell invokes the production acquisition
  # face via quota_probe --ensure) is the only Some-path.
  # r[verify infra.node.kubelet-prjquota+1]
  vm-kubelet-projquota-standalone = kubelet-projquota {
    inherit pkgs common;
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
          # r[verify sched.sla.hw-class.admissible-set]
          "cost-solve"
          # r[verify sched.sla.hw-class.ice-mask]
          "ice-backoff"
          # r[verify sched.sla.hw-class.admissible-set]
          "admissible-set"
          # r[verify sched.sla.prior-partial-pool]
          "seed-corpus"
        ];
      };

  # ── scheduling splits (2 tests, standalone fixture) ──────────────────
  # Same 3-worker fixture (worker1/worker2/worker3) for both — the
  # fragment architecture changes what RUNS, not what's BOOTED.
  # The disrupt split holds the long/disruptive subtests.
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
          "fanout"
          "overlay-readdir"
          # r[verify builder.fuse.canonical-metadata+2]
          "canonical-meta"
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
          # r[verify sched.timeout.promote-on-exceed+3]
          "max-silent-time"
          # r[verify gw.opcode.set-options.propagation+2]
          # setoptions-unreachable greps ALL gateway journal history —
          # placed after max-silent-time so it also covers ITS ssh-ng
          # sessions (no --option passed, but the handshake's virtual
          # setOptions() call runs regardless).
          "setoptions-unreachable"
          # cancel-timing and reassign retired with the stream session
          # machinery (1c' deletion commit A): the stream cancel-dispatch
          # budget and the disconnect→reassign detection they timed no
          # longer exist. Pull-path successors: the pull-canary cancel
          # arm (AD5 composite cancel bound) and the killed-mid-build /
          # establishment pull arms; the lifecycle cancel-cgroup-kill
          # subtest carries the cgroup-kill assertion under default
          # values.
          # r[verify obs.metric.scheduler+2]
          # r[verify obs.metric.builder]
          # r[verify obs.metric.store+2]
          "load-50drv"
          # warm-gate and sigint-graceful were unwired at the T-1c.2b
          # standalone re-point (delivery is pull now; neither can run
          # against a pull-mode fixture). The warm-gate fragment, rule
          # and mechanism retired with the 1c' placement-layer deletion
          # (deletion commit B); the sigint-graceful fragment retired
          # with the 1d builder collapse (T-1d.1). Carriers:
          #   - builder.shutdown.sigint+4 (SIGTERM/SIGINT = abort): the
          #     pull-loop signal unit battery in
          #     rio-builder/src/runtime/pull.rs (idle-shutdown exit and
          #     the mid-build abort + bounded report attempt);
          #   - the process-level teardown half (FUSE unmount, profraw
          #     flush on return from main) is exercised by every
          #     standalone scenario's normal one-shot exit path, and the
          #     mid-build pod-termination abort is asserted end-to-end by
          #     the lifecycle killed-mid-build / cancel arms (k3s).
        ];
        # Default 600s is tight: max-silent-time ~75-150s (I-200
        # retries × pull cycle) + setoptions ~5s + load-50drv
        # ~60-130s (pull is one attempt per worker at a time) +
        # ~120s boot. 900s keeps headroom without being an
        # open-ended escape hatch.
        globalTimeout = 900;
      };

  # r[verify gw.jwt.dual-mode+2]
  # r[verify sec.boundary.grpc-hmac]
  # r[verify gw.reject.nochroot]
  # r[verify gw.rate.per-tenant]
  # r[verify store.gc.tenant-quota-enforce]
  #   Single-test scenario (no subtests list). Markers at the wiring
  #   point per P0341 convention — scenario header prose explains which
  #   subtest proves each rule. The stream-era executor-kind-spoof
  #   probe retired with the session machinery (1c' deletion commit A);
  #   sec.executor.identity-token's per-unary token↔intent binding is
  #   carried by the scheduler unit batteries and the pull-canary VM
  #   arms.
  vm-security-standalone = security.standalone {
    fixture = standalone {
      withHmac = true;
      # P0560 stopgap: the default SSH key gets the vmtest comment and
      # the rio_vmtest_* triggers attribute every path to every
      # retention-0 tenant — the scenario creates team-test with
      # gc_retention_hours = 0 to opt in, so the team-test / quota /
      # rate-limit builds can read their castore inputs. The
      # empty-comment (NULL tenant) boundary is still asserted at
      # submission level in tenant-resolve case 3.
      defaultTenant = "vmtest";
      extraPackages = [
        pkgs.grpcurl
        pkgs.grpc-health-probe
        pkgs.postgresql_18
      ];
    };
  };

  # r[verify sec.pod.fuse-device-plugin+1]
  # r[verify builder.cgroup.ns-root-remount]
  # r[verify sec.psa.control-plane-restricted]
  # r[verify builder.seccomp.localhost-profile+3]
  #   seccomp: nonpriv-admitted asserts the worker container references
  #   the Localhost profile (pod-level RuntimeDefault + container-level
  #   Localhost, operator/rio-builder.json); seccomp-profile-content
  #   asserts the INSTALLED profile keeps the read-side trace syscalls
  #   (ptrace/process_vm_readv) in an ALLOW block and process_vm_writev
  #   out of every ALLOW block.
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
      # P0560 stopgap: the e2e build's inputs go through the
      # tenant-scoped castore reads — see the k3s-full.nix
      # defaultTenant comment.
      defaultTenant = "vmtest";
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
    # Every chaos subtest drives a real build — same tenancy stopgap
    # as the rest of the build matrix (passthrough to standalone).
    fixture = toxiproxy { defaultTenant = "vmtest"; };
  };

  # r[verify obs.metric.gateway]
  #   EXPECTED_METRICS[(gateway,9090)] asserts rio_gateway_* metric
  #   names after a build — presence proves describe_*! wiring AND
  #   actual increments (metrics-rs registers on first increment).
  # r[verify obs.trace.scheduler-id-in-metadata]
  #   trace-id-propagation subtest: the `(trace <hex>)` suffix on the
  #   `rio: build <id>` STDERR_NEXT line is the scheduler's
  #   x-rio-trace-id (not the gateway's own span); asserted to appear
  #   on both scheduler AND worker spans in the collector file —
  #   proves the header carries the USEFUL id.
  # r[verify sched.trace.assignment-traceparent]
  #   span_from_traceparent parenting-vs-link observation: checks
  #   whether the worker build-executor span's parentSpanId matches a
  #   scheduler spanId. The PRECONDITION (both services share the
  #   emitted trace_id) proves the WorkAssignment.traceparent data-carry
  #   delivers context end to end. Resolves the doc's open question.
  vm-observability-standalone = observability {
    inherit pkgs common;
    fixture = standalone {
      # Chain builds materialize inputs through the tenant-scoped
      # castore reads — see the vm-castore-e2e fixture comment.
      defaultTenant = "vmtest";
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

  # ── lifecycle splits (5 tests, k3s-full fixture) ─────────────────────
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
      # r[verify sec.jwt.pubkey-mount+2]
      #   jwt-mount-present: scheduler+store have rio-jwt-pubkey ConfigMap
      #   at /etc/rio/jwt; gateway has rio-jwt-signing Secret. Placed
      #   FIRST — pure precondition check, no pod disruption, ~5s.
      #   Everything else (health-shared onward) assumes the same stable
      #   2-replica state so ordering is immaterial wrt those.
      "jwt-mount-present"
      # r[verify ctrl.probe.named-service]
      # r[verify ctrl.health.ready-gates-connect+1]
      "health-shared"
      # r[verify builder.cancel.cgroup-kill+2]
      "cancel-cgroup-kill"
      # r[verify builder.cgroup.kill-on-teardown]
      # r[verify builder.timeout.no-reassign]
      "build-timeout"
      # r[verify ctrl.pool.reconcile]
      # r[verify ctrl.crd.pool+2]
      #   pool-lifecycle: apply Pool CRD → wait status → delete
      #   --wait=false. Non-disruptive (no shared-state interference
      #   with the subtests above), so it folds into core rather than
      #   paying a separate k3s boot.
      "pool-lifecycle"
      # r[verify sched.materialize.job+2]      (kind boundary: flag-on build traffic mints only build-kind rows; the wanted relation is written)
      # r[verify sched.materialize.pinning]  (kind boundary: flag-on builds write only build_input pins)
      #   materialization-boundary: against a flag-on deployment whose
      #   builder traffic (cancel-cgroup-kill, build-timeout above) HAS
      #   minted executions — the durable wanted relation is written
      #   (>0, design §6/AS-1) while jobs/attempts/executions/pins stay
      #   strictly build-kind (zero materialization rows), and the helm
      #   default plumb renders =true on both deployments. Placed LAST:
      #   it audits the residue of everything before it.
      "materialization-boundary"
    ];
  };

  # gc + refs split out of core. gc-sweep (~86s, includes a gateway
  # scale-0→1 bounce) and refs-end-to-end were half of core's subtest
  # budget, which made core the longest VM test in CI and therefore
  # the tail of the pipeline's critical path. Both fragments build
  # their own paths (no shared state with core's remaining subtests),
  # so the split costs one more k3s boot and nothing else.
  vm-lifecycle-gc-k3s = lifecycleMod.mkTest {
    name = "gc";
    subtests = [
      "gc-dry-run"
      # r[verify store.gc.tenant-retention]
      "gc-sweep"
      # r[verify builder.upload.references-scanned+2]
      # r[verify builder.upload.deriver-populated]
      # r[verify store.gc.two-phase+2]
      # r[verify builder.fs.parity]
      #   P0562 parity witness: the consumer build reads its dep through
      #   the castore-FUSE lower and the dep's store path is scanned into
      #   PG narinfo."references" exactly as the pre-ADR-022 lifecycle
      #   suite asserted.
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
      # r[verify sched.dispatch.input-roots+3]
      #   fod-substituted-inputs: the 1331-FOD-stuck regression shape.
      #   Build dep → rollout restart deploy/rio-scheduler → submit a
      #   parent whose only inputDrv is dep → attested_input_seeds
      #   resolves dep WITHOUT degrading to None. Asserts seeds_unknown
      #   stays flat across the parent dispatch and no Input/output
      #   error in builder logs. After store-rollout (settled cluster);
      #   does its own scheduler rollout + drain so authz-matrix below
      #   starts clean. The PG-fallback arm specifically is unit-covered
      #   (a fresh submit re-merges dep into the DAG, so the VM path
      #   takes the DAG arm — see fragment header).
      "fod-substituted-inputs"
      # r[verify store.log.tail-ownership]
      # r[verify store.log.method-credential+2]
      # r[verify sched.tenant.authz+3]
      #   authz-matrix: the deployed-cluster leg of the slot-4 authz
      #   matrix against the jwtEnabled fixture — own-tenant TailLog
      #   served via the production build-membership chain (the leg
      #   the dead derivations.tenant_id gate could never pass),
      #   foreign TailLog absence-shaped NotFound, tokenless
      #   TailLog/TenantQuota layer-rejected, foreign WatchBuild
      #   denied. LAST: builds its own drv, no shared state.
      "authz-matrix"
    ];
  };

  vm-lifecycle-autoscale-k3s = lifecycleMod.mkTest {
    name = "autoscale";
    subtests = [
      # r[verify ctrl.pool.ephemeral+2]
      # r[verify ctrl.ephemeral.intent-deadline+2]
      # r[verify ctrl.crd.host-users-network-exclusive]
      # ~180s: two builds × (reconcile tick + pod schedule + FUSE +
      # pull + build + exit). Subtest deletes the default x86-64
      # Pool first so it doesn't steal dispatch.
      "ephemeral-pool"
      # r[verify sched.executor.pull-transaction+2]
      # r[verify builder.pull.exit-codes]
      # r[verify sched.attempt.no-attempt-no-op]
      # r[verify sched.admin.list-open-attempts+4]
      #   pull-mode: ~360s — never-pulled kill + respawned pull build
      #   (30s sleep) + report + killed-mid-build arm (45s sleep +
      #   force-kill + requeue + 45s rebuild to a delivered store
      #   path) + cleanup, each behind a reconcile tick + pod
      #   schedule + FUSE.
      "pull-mode"
    ];
    # ephemeral ~180s + pull-mode ~360s + ~240s k3s bring-up ≈ 780s
    # expected; TCG tail headroom on top.
    globalTimeout = 1200;
  };

  # ── pull-canary (dedicated check, T-1b.8/T-1b.9) ──────────────────────
  # Pull-vs-stream retry-feed equivalence (the same scripted
  # success+failure sequence on both pool kinds, asserted over the
  # fold input: same outcome class, one charge per failure, no double
  # charges, exclusion keying per AD2), plus the AD5 cancel/preempt VM
  # timing bounds and the sh-021 TerminalAbsent reap. Dedicated check
  # rather than another vm-lifecycle-autoscale-k3s subtest: the
  # fixture overlay (probe deadline pinned to 180s) changes every
  # builder Job's activeDeadlineSeconds.
  #
  # r[verify ctrl.ephemeral.reap-terminal-absent]
  #   reap arm: a pull-mode pod whose builder is SIGKILLed from the
  #   host (no SIGTERM-abort report, plain Error pod) sends the Job
  #   Failed; the controller's TerminalAbsent arm (AbsentFromDemand ∧
  #   !is_active_job, two-tick strike) deletes the Job INSTANCE and
  #   synthesizes reason=Reaped — the attempt closes UNCHARGED as
  #   disconnected/reaped by the controller within 90s, well inside
  #   the establishment slack (the sweep is the backstop, never the
  #   closer for this shape) — and the requeued drv still delivers its
  #   store path under a fresh exec with no further row. The
  #   establishment-window timing itself stays unit-covered
  #   (rio-scheduler/src/actor/tests/establishment.rs); on a live
  #   cluster the controller reaps before the window can close.
  # r[verify ctrl.drain.disruption-target+4]
  #   preempt arm: patching DisruptionTarget=True on a pull-mode pod
  #   makes the controller synthesize the preempted report and
  #   foreground-delete the owning Job (report-then-delete; the
  #   retired DrainExecutor hop plays no part); pod+Job gone and the attempt closed at the
  #   report fold within 90s — never the establishment sweep — the
  #   preempted exec charged exactly once with a non-success
  #   disruption class, and the requeued drv still delivers. cancel
  #   arm: the same closed-attempt evidence drives the
  #   scheduler-cancel successor — CancelBuild closes the attempt, the
  #   controller foreground-deletes the still-active Job on the
  #   closed→active edge, the build cgroup/pod/Job are gone within 90s
  #   and the cancelled exec is charged nothing.
  vm-pull-canary-k3s = lifecyclePullCanaryMod.mkTest {
    name = "pull-canary";
    subtests = [
      "pull-canary"
      # r[verify sched.executor.pull-transaction+2]
      # r[verify sched.sla.fod-feature-derivation+3]
      #   pull-fetcher (T-1c.2): fetcher-kind pull coverage for the 1c
      #   gate's "pool kinds the canary did not cover". A kind=Fetcher
      #   Pool builds a network-free FOD on the pull path (one open
      #   attempt minted by the pull transaction on a
      #   RIO_EXECUTOR_KIND=fetcher pod, charges nothing) and the pod
      #   follows the OA3 one-pull default — it completes after its
      #   single report instead of retaining a session. Runs after
      #   pull-canary's cleanup (no Pools left).
      "pull-fetcher"
    ];
    # Budget: ~240s k3s bring-up + ~80s prelude + pool swap & pull
    # retry-feed ~175s + cancel arm ~150s + preempt arm ~200s +
    # TerminalAbsent reap arm ~150-200s (two-tick reap ~30-40s + 60s
    # rebuild; sh-021 retired the ~300s establishment-window wait) +
    # fetcher pull arm ~150-220s (pool create + Job spawn + 15s FOD
    # build + report + pod-completion wait + cleanup) + cleanup ~30s
    # ≈ 1050-1250s expected on a loaded KVM runner; 2400s leaves tail
    # headroom without being open-ended.
    globalTimeout = 2400;
  };

  #
  # ADR-023 §13b nodeclaim_pool reconciler under KWOK fake-Karpenter.
  # Karpenter is faked: KWOK Stage rules progress NodeClaim status
  # (Launched→Registered, populate allocatable from spec.resources.
  # requests). The kube-build-scheduler Deployment runs for real
  # (registry.k8s.io/kube-scheduler preloaded) so builder Jobs'
  # `schedulerName: kube-build-scheduler` resolves.
  #
  # Distinct runNixOSTest name `rio-forecast-provisioning` (NOT a
  # `vm-sla-sizing-*` variant — sla-sizing.nix is standalone-tied;
  # this is k3s+kubectl-only).
  #
  # nodeclaim_pool config flows through the chart's first-class values:
  # `scheduler.sla.{hwClasses,leadTimeSeed,maxFleetCores,...}`
  # render into the rio-controller-config
  # ConfigMap's `[nodeclaim_pool]` TOML table (lead_time_seed is a nested
  # map — the RIO_ env layer only yields strings, so the ConfigMap
  # mount is the ONLY load path). The 12 prod hwClasses + 24-cell
  # leadTimeSeed are per-subkey-nulled in the fixture overlay so hwClasses
  # / leadTimeSeed key-sets all = {vmtest}.
  #
  # r[verify ctrl.nodeclaim.ffd-sim]
  # r[verify ctrl.nodeclaim.shim-nodepool]
  # r[verify ctrl.nodeclaim.anchor-bulk+7]
  # r[verify ctrl.nodeclaim.priority-bucket]
  # r[verify ctrl.nodeclaim.placeable-gate+5]
  vm-sla-sizing-kwok = forecast-provisioning {
    inherit pkgs common;
    fixture = k3sFull {
      extraImages = kwok.airgapImages;
      extraManifests = kwok.manifests;
      extraValuesTyped = {
        "buildScheduler.enabled" = true;
      };
      extraValues = {
        "buildScheduler.image" = kwok.kubeSchedulerRef;
      };
      extraValuesFiles =
        let
          # B16: Helm deep-merges values.yaml's 12 hwClasses + 24-cell
          # leadTimeSeed with vmtest-full.yaml's `vmtest`,
          # giving 13 hwClasses. The scheduler's solve_full draws
          # SpawnIntent.hw_class_names from that 13-set excluding
          # `vmtest`; `assign_to_cells` then never produces a
          # `vmtest:spot` key and cover_deficit emits created=0. Helm
          # only honours `null` deletion against CHART values.yaml (not
          # prior `-f` files), so a whole-map `hwClasses: null` in one
          # file followed by `hwClasses: {vmtest:...}` in the next
          # coalesces to `{vmtest:...}` user-side then deep-merges back
          # to 13 against chart defaults. Per-subkey nulls are the only
          # way to delete chart-default map entries while keeping
          # `vmtest` — generated here so the prod hwClass list stays
          # single-sourced.
          prodHw = [
            "hi-nvme-x86"
            "hi-nvme-arm"
            "hi-ebs-x86"
            "hi-ebs-arm"
            "mid-nvme-x86"
            "mid-nvme-arm"
            "mid-ebs-x86"
            "mid-ebs-arm"
            "lo-nvme-x86"
            "lo-nvme-arm"
            "lo-ebs-x86"
            "lo-ebs-arm"
          ];
          prodCells = pkgs.lib.concatMap (h: [
            "${h}:spot"
            "${h}:od"
          ]) prodHw;
          nullKeys = indent: ks: pkgs.lib.concatMapStringsSep "\n" (k: "${indent}\"${k}\": null") ks;
        in
        [
          # `[sla]` vmtest-only. Per-subkey nulls wipe the 12 prod
          # hwClasses + 24 prod leadTimeSeed cells from chart defaults
          # so leadTimeSeed / hwClasses key-sets all = {vmtest}.
          # max_fleet_cores capped at 64 so a
          # runaway tick can't request more than the KWOK fixture
          # synthesizes. Colon in `vmtest:spot` cell key needs YAML
          # key-quoting.
          #
          # leadTimeSeed = 120 (NOT kwok's ~5s nominal boot): the seed
          # feeds health::classify's unregistered-reap grace
          # (`2 x seed`), and the grace must STRICTLY DOMINATE the
          # scenario's registration-wait budget or the reaper races
          # the waits under load. With the old 5.0 the grace was 10s
          # while KWOK's Stage reconcile can stall past it under
          # full-gate builder load: the controller (correctly, per its
          # predicate — same-frame `age > 2 x seed` arithmetic) reaped
          # the unregistered claim, the BootTimeout reap ICE-masked
          # vmtest:spot, cover_deficit minted nothing for the mask
          # TTL, and the scenario's 60s Registered-wait timed out
          # staring at `items: []` (the vm-sla-sizing-kwok strike-1
          # flake). Dominance: grace 2 x 120 = 240s > 90 (creation
          # wait) + 60 (Registered wait) + 10 (tick) = 160s worst
          # case, so the reap structurally cannot fire while either
          # wait runs. The scenario is explicitly timing-insensitive
          # ("asserts CREATED + PROGRESSED + metric pipeline, not
          # specific timings"), so nothing else reads the seed.
          (pkgs.writeText "kwok-nodeclaim-pool.yaml" ''
            scheduler:
              sla:
                maxFleetCores: 64
                maxNodeClaimsPerCellPerTick: 4
                hwClasses:
            ${nullKeys "      " prodHw}
                  vmtest:
                    nodeClass: rio-default
                    maxCores: 16
                    maxMem: 2147483648
                    labels:
                      - {key: rio.build/vmtest, value: "true"}
                    requirements:
                      - {key: kubernetes.io/os, operator: In, values: [linux]}
                referenceHwClass: vmtest
                leadTimeSeed:
            ${nullKeys "      " prodCells}
                  "vmtest:spot": 120.0
                  "vmtest:od": 120.0
          '')
        ];
    };
  };

  # ── substitute-scale ─────────────────────────────────────────────────
  #   Substitution → autoscaling-signal path. 30-leaf substitutable
  #   fanout against a 1-permit store admission gate: the merge creates
  #   30 materialization jobs → the scheduler leader publishes the
  #   backlog gauge (rio_scheduler_substituting_derivations) → the
  #   gauge RISES with the cascade and returns to 0 as the jobs drain;
  #   admission utilization observable on the store metrics surface;
  #   zero builder pods for the leaves. Plus the depth-50 deep-chain
  #   eager-burst proof. ~7min (k3s + cache-seed + poll).
  #
  #   This is the signal half of the store-scaling loop: KEDA consumes
  #   exactly this gauge (templates/store-scaledobject.yaml), but the
  #   prometheus→KEDA→replica half cannot run in the k3s fixture (the
  #   airgapped image set carries no KEDA operator) — it is covered by
  #   the helm-template render checks (26-store-scaling.sh) and the
  #   post-wipe deployment checklist's P10 observation on EKS.
  #
  # Distinct runNixOSTest name (rio-substitute-scale) — NOT a variant
  # of rio-substitute, so the derivation names don't collide.
  #
  # jwtEnabled: substitution is tenant-scoped (try_substitute_on_miss
  # short-circuits without x-rio-tenant-token); the gateway must mint
  # it from the SSH key comment. substituteAdmissionPermits=2 — the
  # validate() floor (live_047/R-C: cap 1 would make the executor
  # path-slot pool P = 0 and is boot-rejected) — gives P = cap/2 = 1
  # path slot: the 30 walks SLOT-serialize (one in-flight path fleet-
  # wide — the store.materialize.gate-share+1 law), so with 200ms tc-netem on
  # upstream-v6 the cascade still outlives the scheduler's 10s
  # housekeeping tick (the gauge publication cadence) — at the derived
  # default (pg_max×3≥64, P=32), tiny NARs drain in <1s and no tick
  # would observe a nonzero backlog. The executor's lawful full-draw
  # ceiling is P/cap = 0.5 of the gate (it can never saturate it —
  # that inversion was the pre-law harness device this fixture used to
  # encode as admission=1). Set via the chart key (not extraEnv) so
  # the values.yaml → store.yaml templating is exercised.
  #
  # r[verify obs.metric.scheduler-substituting+2]
  #   cascade: the §2.6 substituting bucket (pending unclaimed jobs)
  #   reaches Prometheus — the backlog gauge rises while the cascade
  #   drains and returns to 0 after it; the scrape surface IS the
  #   autoscaling signal path.
  # r[verify store.substitute.admission+2]
  #   cascade: the store executors' fetches go through the per-replica
  #   admission gate — utilization observable at 1.0 on the 1-permit
  #   gate mid-cascade, and the serialized drain outlives the gauge
  #   tick.
  # r[verify obs.metric.store-gauge-ownership]
  #   cascade (4b): the admission gauge DECAYS to 0.0 within 45s of
  #   the drain — the permit drop edge + the 30s store gauge tick own
  #   the fall; pre-fix the acquire-only gauge froze at 1.0 forever
  #   (bug_245), feeding KEDA a permanently-saturated replica.
  # r[verify sched.substitute.eager-probe]
  # r[verify sched.materialize.job+2]
  #   deep-chain: one merge burst classifies all 49 seeded links —
  #   rio_scheduler_materialization_jobs_created_total delta ≥45 + ≥49
  #   cache_opportunity job rows, while the walk-spawn counter stays at
  #   0 (criterion 3). The eager-vs-lazy property.
  vm-substitute-scale-k3s = substituteScaleTest;

  # ── wipe-burst (live_055(c)) ─────────────────────────────────────────
  #   The dead-claim wedge corpus: wipe (a planted dead zero-progress
  #   claim — the post-wipe stranded-claim shape) + min-floor (store at
  #   the 1-replica chart floor) + deep-chain-burst (a depth-24 RUNTIME-
  #   reference chain, links seeded upstream, every walk's closure
  #   passing through the wedged head). Asserts reclaim latency ≤ the
  #   chart-set stall window (completion budget 240s strictly under the
  #   300s heartbeat reap — the pre-live_055(a) tree fails the budget
  #   structurally), head-path-cannot-wedge>window (the burst drains),
  #   and the live_055(b) subscription plane engaging (parks counted
  #   during the pre-eligibility window). Structural counts only —
  #   counter entries + durable rows, never gauge samples (N13).
  #
  # r[verify store.substitute.stale-reclaim+4]
  #   head reclaimed: the zero-progress takeover arm under chart-
  #   rendered config (substituteStallSecs=60, the validate() floor) —
  #   stall_count=1 exactly, stale_reclaimed{stall_reclaim}=1 exactly,
  #   completion inside the under-reap budget.
  # r[verify store.substitute.raced-subscribe]
  #   burst parks: raced_parks_total ≥ 1 before the wedge's takeover
  #   eligibility opens — the walks subscribed instead of poll-racing;
  #   the eligibility cushion makes a takeover-laundered park
  #   unrepresentable in the observation window.
  vm-wipe-burst-k3s = wipe-burst {
    inherit pkgs common;
    fixture = k3sFull {
      jwtEnabled = true;
      defaultTenant = "vmtest";
      extraValuesTyped = {
        # The validate() floor (2 × the 30s placeholder heartbeat):
        # the smallest lawful window — keeps eligibility (cushion 30s
        # + window 60s) and the 240s completion budget inside one
        # scenario without touching the 300s reap separation.
        "store.substituteStallSecs" = 60;
      };
    };
  };

  # ── materialization under leader failover (T-3.3) ───────────────────
  # What the standalone scenarios cannot prove: materialization jobs are
  # PG-authoritative state that survives the scheduler leader's death.
  #
  # Wave-3-honest assertions (the acknowledged recovery limitation): the
  # new leader's in-memory job view is rebuilt indirectly (the dispatch
  # probe's create-job dedup re-feeds it from the surviving PG rows);
  # Wave 4's T-4.3 adds the direct recovery rebuild and only makes these
  # assertions pass sooner. The scenario asserts end states, not view
  # internals, precisely so T-4.3 extends rather than rewrites it.
  #
  # r[verify sched.materialize.job+2]
  #   failover: 10 jobs created in the merge tx; the leader is
  #   force-deleted while >=1 is still unresolved; the standby acquires;
  #   the job rows survive byte-identically (count + job_ids); all 10
  #   resolve and the build succeeds. PG is the authority — no job is
  #   lost with the leader.
  # r[verify sched.materialize.settlement]
  #   the armed-action totality across failover: the in-flight job is
  #   re-claimed/settled by the new leader (no unresolved job is left
  #   with no armed action by the failover).
  vm-materialization-failover-k3s = materializeFailoverTest;

  # ── leader-election splits (2 tests, k3s-full fixture) ───────────────
  # ~0 wall-clock savings (4min bootstrap dominates both) but failures
  # in build-during-failover no longer block the stability checks.
  vm-le-stability-k3s = leMod.mkTest {
    name = "stability";
    subtests = [
      "antiAffinity"
      "lease-acquired"
      # r[verify sched.lease.k8s-lease+2]
      "stable-leadership"
      # r[verify sched.lease.graceful-release+2]
      # r[verify sched.lease.deletion-cost+4]
      "graceful-release"
      "failover"
      # r[verify sched.lease.generation-claim+2]
      #   kubectl delete lease destroys the epoch source
      #   (leaseTransitions resets to 0 on the recreated Lease); the
      #   next acquisition's generation must still exceed every
      #   generation the old regime claimed. Asserted against the
      #   leader_generation_claims ledger (psql), not logs: MAX(gen)
      #   strictly increases, the ledger is append-only, and the
      #   lease-derived generation alone (transitions+1) is provably
      #   below the claimed one — the PG floor did the work.
      "lease-deletion"
    ];
  };

  # ── standby-burst (round-9 live_055(e) constituent, W9-AI) ───────────
  vm-standby-burst-k3s = standbyBurstMod.mkTest {
    name = "burst";
    subtests = [
      # r[verify sched.grpc.leader-guard]
      #   The standby's documented posture, end-to-end at the wire: a
      #   non-leader replica keeps its gRPC server up and refuses
      #   ClusterStatus as Trailers-Only Unavailable (grpc-status 14
      #   in the HTTP response headers, EMPTY body — the
      #   ci-failure-patterns standby shape) within the 10s answer
      #   budget, while the leader serves a DATA frame — the
      #   exactly-one-serving face, probed per-pod via port-forward
      #   (svc/ would round-robin onto an arbitrary replica).
      "standby-shape"
      # r[verify sched.lease.k8s-lease+2]
      #   live_055(e)'s structural inverse: across a store scale-out
      #   burst (kubectl scale 1→4→1 — the KEDA actuation; no operator
      #   at VM scale) plus a 4-build burst, leaseTransitions delta ==
      #   0 and holderIdentity is unchanged (EXACT equality — any
      #   movement is the flap), and the standby posture re-verifies
      #   post-burst. The EKS-scale CPU-contention face does not
      #   reproduce in a 2-node VM; this is the regression PIN for the
      #   lease-stability law (disclosed in the landing commit).
      "burst-stability"
    ];
  };

  vm-le-build-k3s = leMod.mkTest {
    name = "build";
    # r[verify sched.lease.non-blocking-acquire+2]
    subtests = [
      "build-during-failover"
      # r[verify sched.lease.k8s-lease+2]
      # r[verify sched.lease.generation-fence+3]
      #   True ungraceful death: SIGKILL the leader's host PID via
      #   crictl (no SIGTERM, no step_down, no FIN). Kubelet restarts
      #   the container in-place; restarted process sees holder==our_id
      #   → Renew (tx+0), OR standby observed-rv-expiry steals (tx+1)
      #   if restart exceeds STEAL_AFTER (19s). The `failover` subtest
      #   does NOT reach the observed-record-expiry branch — step_down
      #   wins the SIGTERM race post-a5b06ef. Ordered after
      #   build-during-failover: reuses its sshKeySetup (ssh-keygen is
      #   not idempotent).
      "sigkill-mid-build"
    ];
  };

  # r[verify sched.admin.create-tenant]
  # r[verify sched.admin.delete-tenant]
  # r[verify sched.admin.list-tenants]
  # r[verify sched.admin.list-executors+3]
  # r[verify sched.admin.list-builds]
  # r[verify sched.admin.clear-poison+3]
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
  #   (streaming TriggerGC). The frame-prefix grep proves
  #   tonic-web doesn't buffer server-streams — load-bearing for
  #   WatchBuild / live log tail. ~6min (k3s bring-up + Cilium
  #   Gateway reconcile). No separate Envoy Gateway operator —
  #   Cilium's embedded envoy handles the GRPCRoute.
  # r[verify dash.auth.method-gate+5]
  #   The fixture doesn't set dashboard.enableMutatingMethods so the
  #   rio-scheduler-mutating HTTPRoute is absent — `kubectl get
  #   httproute rio-scheduler-mutating` fails. Proves the helm-template
  #   fail-closed holds at runtime through the operator's reconcile.
  # r[verify dash.journey.build-to-logs+2]
  #   The LogViewer's log-read path (nginx → rio-store
  #   LogService/TailLog, the second upstream + cross-namespace Service)
  #   is asserted by the TailLog 0x80 trailer subtest in dashboard.nix:
  #   the handler returns errors as in-stream items (not tonic
  #   Trailers-Only) so tonic-web encodes them as 0x80 body frames
  #   browser fetch can read.
  #
  #   Appended (when `dockerImages ? dashboard`): the nginx-pod curl
  #   assertions from dashboard.nix — SPA served + try_files fallback
  #   + gRPC-Web 0x00/0x80 THROUGH nginx (scheduler AND store upstreams)
  #   + method-gate 404. Coverage
  #   mode (no rio-dashboard image) runs the gateway/EDS/tonic-web
  #   subtests only; the nginx pod is absent so its curls are skipped.
  # networkPolicy.enabled (bug_238): the dashboard's east-west edges
  # (nginx → scheduler:9001 / store:9002) are now generated from
  # files/dashboard-upstreams.json and ENFORCED here — the existing
  # nginx-path scheduler-RPC subtests are the regression test (RED at
  # the pre-registry tip: the hand-written egress lacked the
  # scheduler:9001 edge entirely, so every SPA scheduler RPC was
  # dropped under enforcement).
  # r[verify dash.stream.log-tail+6]
  #   live-tail-via-nginx subtest (4c): a line ingested AFTER the
  #   follow:true TailLog stream opened reaches the open connection
  #   through nginx — the incremental-delivery half of the rule's
  #   proxy_buffering-off requirement that the conf-guard alone cannot
  #   prove (the dashboard's B3 follow-mode LogViewer rides this path).
  vm-dashboard-k3s = dashboard-gateway {
    inherit pkgs common;
    fixture = k3sFull {
      gatewayEnabled = true;
      extraValues = {
        "networkPolicy.enabled" = "true";
      };
    };
    withDashboardCurls = dockerImages ? dashboard;
  };

  # Builder + store egress NetworkPolicy: IMDS + public internet + k8s
  # API all blocked. networkPolicy.enabled via extraValues (--set-string
  # "true" is truthy for `{{ if }}`).
  # vmtest-full.yaml defaults it to false; the override renders
  # networkpolicy.yaml into 02-workloads.yaml.
  # Cilium enforces (eBPF) — k3s's bundled kube-router netpol controller
  # is disabled (--disable-network-policy in k3s-full.nix).
  #
  # r[verify store.netpol.egress+3]
  #   store-egress IMDS-deny + postgres-allow probe via nsenter into
  #   rio-store pod netns (netpol-store-egress subtest).
  #   netpol-store-scheduler-egress: store pod → rio-scheduler:9001
  #   handshake allowed (the materialization executor edge —
  #   substitution-replacement campaign); IMDS still denied (existing
  #   assertion unchanged). Connectivity-only, flag-independent (PD-11).
  # r[verify builder.netpol.airgap]
  #   builder-egress IMDS-deny + k8s-API-deny + DNS-TCP-allow probes
  #   (netpol-kubeapi / netpol-imds / netpol-dns-tcp subtests).
  vm-netpol-k3s = netpol {
    inherit pkgs common;
    fixture = k3sFull {
      # The probe builds run on real builder pods — P0560 tenancy
      # stopgap, see k3s-full.nix.
      defaultTenant = "vmtest";
      extraValues = {
        "networkPolicy.enabled" = "true";
      };
    };
  };

  # 2×2 ingress/egress on the v6-only k3s fixture. client-v6 → NodePort
  # direct, client-v4 → edge:22 socat → NodePort over v6; both nix-build
  # over ssh-ng. Then egress: k3s host reaches upstream-v4 via 64:ff9b::
  # (Jool), pod resolves upstream-v4 → 64:ff9b:: AAAA (CoreDNS dns64).
  # ~6min (k3s bring-up + two trivial builds + curl probes).
  #
  # Prepended: cilium WireGuard encrypt + GUA-v6 NodePort frontend
  # (regression guard for the EKS NLB RST bug). ~10s of post-waitReady
  # assertions on the same bare k3sFull{} — folded in to save a boot.
  # r[verify sec.transport.cilium-wireguard]
  # r[verify gw.ingress.v6-direct]
  # r[verify gw.ingress.v4-via-nat]
  vm-ingress-v4v6-k3s = ingress-v4v6 {
    inherit pkgs common;
    fixture = k3sFull {
      withV4Nodes = true;
      # Both trivial builds go through the tenant-scoped castore reads
      # — P0560 tenancy stopgap, see k3s-full.nix.
      defaultTenant = "vmtest";
    };
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
  # name="x86-64-fetcher" (default rio-builder image; controller injects
  # RIO_EXECUTOR_KIND per-pod). Systems
  # includes "builtin" so builtin:fetchurl's system=builtin passes
  # hard_filter(). nodeSelector/tolerations left
  # at reconciler defaults — scenario labels k3s-agent at runtime.
  #
  # r[verify sched.dispatch.fod-to-fetcher+2]
  #   dispatch-fod+nonfod subtest: one nix-build, FOD routes to
  #   fetcher pod, consumer routes to builder pod. Wrong routing →
  #   queue-forever → timeout. kubectl-logs grep confirms placement.
  # r[verify sched.dispatch.fod-builtin-any-arch+2]
  #   dispatch-fod+nonfod subtest: the FOD half is builtin:fetchurl
  #   (system="builtin") — its intent lands on the kind=Fetcher pool
  #   (declared systems [x86_64-linux, builtin]) and the fetcher pod
  #   executes it (the daemon runs builtin:fetchurl internally), so an
  #   executor treating `builtin` as a supported system is exercised
  #   end-to-end. The "regardless of arch" half (no kubernetes.io/arch
  #   constraint derived from `builtin`; arch-typed pools pin theirs)
  #   is unit-tested in `fetcher_pod_arch_selector_from_systems` /
  #   `nix_systems_to_k8s_arch_mapping` (rio-controller pool/pod.rs).
  # r[verify builder.netpol.airgap]
  #   builder-airgap subtest: builder netns curl to upstream-v4 via
  #   NAT64 (64:ff9b::<v4>:80) → rc≠0. Positive control: scheduler
  #   ClusterIP connects (NetPol allow fires).
  # r[verify fetcher.netpol.egress-open+2]
  #   fetcher-egress + fetcher-imds-blocked subtests: SAME origin,
  #   fetcher netns → rc==0 (toEntities:[world]:80 allow fires). Then
  #   IMDS → rc≠0 (host entity, NOT world → denied). The origin probe
  #   is the non-vacuous differentiator vs builder.
  # r[verify fetcher.node.dedicated+4]
  #   fetcher-node-dedicated subtest: pod spec has the rio.build/
  #   fetcher toleration (§13e cold-start fallback) AND the pool-static
  #   nodeSelector{rio.build/fetcher: true} (§13e B4) — the LAST-RESORT
  #   restrictive constraint for builtin FODs in the window between
  #   intent-emit and node-provision (r35 B1). In this k3s fixture
  #   (`vmtest-full.yaml`'s `vmtest` hw-class declares no
  #   `providesFeatures`), `features_compatible(["fetcher"], [])` is
  #   false → `reference_hw_class_for_system("builtin", ..., ["fetcher"])`
  #   returns None → `hw_class_names=[]` → no per-intent nodeAffinity.
  #   This is a property of the FIXTURE (no fetcher hw-class), not a
  #   property of `system="builtin"` — the production path routes
  #   builtin FODs by feature to `fetcher-*` (r35 B1). The deleted
  #   rio.build/node-role convention must NOT reappear. Karpenter
  #   NodePool/NodeClaim enforcement is EKS-only.
  # r[verify ctrl.pool.fetcher-affinity-from-intent+5]
  #   fetcher-node-dedicated subtest: same shape check — pool-static
  #   nodeSelector present (the §13e B4 restore: it keys on
  #   pool.spec.kind, a Pool-level invariant the per-intent affinity is
  #   a projection of, so the two cannot drift). The positive half
  #   (per-intent nodeAffinity present for arch-typed FODs) is
  #   unit-tested in `builtin_fod_pod_has_pool_static_fetcher_node_selector`
  #   / `fetcher_pod_no_legacy_node_role_selector` and contract-tested
  #   in sla_contract.rs; this is the in-cluster shape check.
  # r[verify sched.sla.fod-feature-derivation+3]
  #   dispatch-fod+nonfod + fetcher-isolation subtests: FOD routes to
  #   the kind=Fetcher pod (passes_intent_filter reads
  #   effective_features(state)=[fetcher] for FODs and matches the
  #   fetcher pool's req.features=[fetcher]); the consumer routes to the
  #   builder pod. fetcher-isolation asserts the pod-level partition the
  #   chokepoint produces: fetcher pod tolerates rio.build/fetcher and
  #   NOT rio.build/kvm; builder pod tolerates NEITHER.
  # r[verify fetcher.nixconf.hashed-mirrors]
  #   fod-dead-origin subtest: flat-hash FOD with a 404 origin URL
  #   builds via {mirror}/sha256/{hex}. nixConf.hashedMirrors below
  #   points the rio-nix-conf ConfigMap at the in-VM upstream-v4 node
  #   (reached via DNS64+NAT64 from the v6-only fetcher pod).
  # r[verify builder.fod.verify-hash]
  #   fod-dir subtest: recursive-hash FOD with directory output
  #   (`mkdir $out`). Regression: a whiteout at the output path
  #   makes overlayfs mkdir return EIO.
  #   fod-fail subtest: failing FOD propagates without hanging.
  #   Daemon's post-fail stat($out) hits the castore lower; a
  #   non-input path gets ENOENT from the in-memory DAG without store
  #   contact. P0308 hang ⇒ the client's nix-build never sees a
  #   BuildResult and the shell `timeout` fires (rc=124); the
  #   structural assertion is rc != 124.
  vm-fetcher-split-k3s = fetcher-split {
    inherit pkgs common drvs;
    fixture = k3sFull {
      withV4Nodes = true;
      # Builder + fetcher pods both read inputs through the
      # tenant-scoped castore — P0560 tenancy stopgap, see k3s-full.nix.
      defaultTenant = "vmtest";
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
