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
  put-path-chunked = import ./scenarios/put-path-chunked.nix;
  store-tiered = import ./scenarios/store-tiered.nix;
  store-compat = import ./scenarios/store-compat.nix;
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
  netpol = import ./scenarios/netpol.nix;
  ingress-v4v6 = import ./scenarios/ingress-v4v6.nix;
  fetcher-split = import ./scenarios/fetcher-split.nix;
  chaos = import ./scenarios/chaos.nix;
  ca-cutoff = import ./scenarios/ca-cutoff.nix;
  castore-e2e = import ./scenarios/castore-e2e.nix;
  componentscaler = import ./scenarios/componentscaler.nix;
  substitute = import ./scenarios/substitute.nix;
  substitute-scale = import ./scenarios/substitute-scale.nix;
  dag-delta-sync = import ./scenarios/dag-delta-sync.nix;
  sla-sizing = import ./scenarios/sla-sizing.nix;
  forecast-provisioning = import ./scenarios/forecast-provisioning.nix;
  kwok = import ./fixtures/kwok.nix { inherit pkgs; };
  kvm-hostpath-spike = import ./scenarios/kvm-hostpath-spike.nix;
  mountd = import ./scenarios/mountd.nix;
  drvs = import ./lib/derivations.nix { inherit pkgs; };

  # SLA-sizing fixture: one worker with RIO_BUILDER_SCRIPT pointing at
  # the scripted-telemetry TOML, scheduler with [sla] configured + the
  # InjectBuildSample fixture gate. tickIntervalSecs=2 so the estimator
  # refit fires fast enough for wait_until_succeeds.
  slaSizingFixture = standalone {
    # Castore tenancy recipe: the scripted-telemetry worker still
    # performs the real castore mount before the fixture intercept
    # (executor/mod.rs — "after overlay+input setup"), so its builds
    # need a tenant-bearing assignment token (withHmac) and a
    # tenant-attributed seed (withJwt). The scenario prelude bootstraps
    # as tenant vm-sla and adds the service token to its AdminService
    # grpcurl calls (withHmac gates those too).
    withHmac = true;
    withJwt = true;
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
  # withHmac: the scheduler signs per-build assignment tokens and the
  # store verifies them — castore-FUSE reads (GetDirectory/ReadBlob/…)
  # are tenant-scoped and only authenticate via that token, so a
  # build-dispatching fixture without HMAC cannot mount its inputs.
  # withJwt: the gateway mints the session JWT for the tenant-named
  # client key and the store verifies it, which is what attributes the
  # seeded busybox closure to the tenant at PutPath time (the
  # store.put.tenant-attribution rule) — without it the seed has no
  # path_tenants rows and every castore mount of it returns NotFound.
  # The scenario prelude pairs both with mkBootstrap's `tenant` so the
  # assignment token actually carries a tenant claim.
  schedulingFixture = standalone {
    withHmac = true;
    withJwt = true;
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
    extraStoreConfig = {
      extraConfig = ''
        [chunk_backend]
        kind = "filesystem"
        base_dir = "/var/lib/rio/store/chunks"
      '';
    };
    # grpcurl: cancel-timing submits + cancels via plaintext gRPC :9001.
    # Tokenless is fine: the scheduler is NOT in JWT mode (withJwt only
    # wires gateway+store), so SubmitBuild takes tenant_name from the
    # payload (the prelude's submit helper injects it) and an anonymous
    # CancelBuild is allowed. ssh-ng:// doesn't surface build_id to the
    # client, and client-disconnect mid-wopBuildDerivation doesn't fire
    # session.rs's EOF-cancel path (handler/build.rs:462 removes the
    # build_id before bubbling). gRPC SubmitBuild + CancelBuild is the
    # only deterministic cancel-a-running-build path in this fixture.
    # postgresql: psql for the mkBootstrap seed-attribution assert and
    # the cgroup fragment's build_samples queries.
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
    # jwtEnabled: the le-build split dispatches real builds through
    # executor pods, and tenant-scoped castore reads require the seeded
    # closure to be attributed to a tenant — which only happens when the
    # gateway mints the session JWT for the tenant-named client key
    # (store.put.tenant-attribution). The stability split shares the
    # fixture and simply never sends tokens (interceptor is dual-mode).
    fixture = k3sFull { jwtEnabled = true; };
  };

  # Prod-parity lifecycle module. No jwtEnabled — bootstrap-job-ran +
  # bootstrap-tenant don't touch JWT. Bare k3sProdParity {} just flips
  # bootstrap on and preloads the rio-bootstrap image.
  lifecycleProdParityMod = lifecycle {
    inherit pkgs common;
    fixture = k3sProdParity { };
  };

  # Shared castore-e2e module (P0560 §B). Prod-parity fixture (the §B
  # fixture rows — kernel.nix import, mountd DS, /var/rio hostPaths —
  # all live in k3s-full, which prod-parity wraps; the bootstrap Job's
  # expected AWS failure is irrelevant here).
  #
  # jwtEnabled: source attribution. The prelude's seed pushes run as
  # the vm-castore tenant (SSH key comment), the gateway mints the
  # session JWT for them, and the store writes path_tenants rows
  # (store.put.tenant-attribution) — without that every castore mount
  # of a client-pushed source returns NotFound and the whole scenario
  # dies at the first build.
  #
  # store.replicas=1 is restated (it is also the chart default) so the
  # eio-circuit-breaker subtest's scale-0/scale-1 outage stays
  # deterministic even if a values-file default ever changes.
  #
  # vmtest-castore.yaml pins all executor pods to k3s-agent so the
  # node-cache reuse assertions are same-node by construction (the
  # store stays pinned to k3s-server by vmtest-full.yaml).
  castoreMod = castore-e2e {
    inherit pkgs common;
    fixture = k3sProdParity {
      jwtEnabled = true;
      extraValuesTyped = {
        "store.replicas" = 1;
      };
      extraValuesFiles = [
        ../../infra/helm/rio-build/values/vmtest-castore.yaml
      ];
    };
  };

  composefs-spike = import ./scenarios/composefs-spike.nix;
  composefs-spike-scale = import ./scenarios/composefs-spike-scale.nix;
  composefs-spike-stream = import ./scenarios/composefs-spike-stream.nix;
  composefs-spike-priv = import ./scenarios/composefs-spike-priv.nix;
  spike-fuse-negdentry = import ./scenarios/spike-fuse-negdentry.nix;
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

  # ── Spikes (single-VM, no rio fixture) ──────────────────────────────
  vm-composefs-spike = composefs-spike { inherit pkgs rio-workspace; };
  vm-composefs-spike-scale = composefs-spike-scale { inherit pkgs rio-workspace; };
  vm-composefs-spike-stream = composefs-spike-stream { inherit pkgs rio-workspace; };
  # P0578 — Q7-Q12: passthrough under overlay, BACKING_OPEN broker
  # boundary, no-read-upcall, copy-up, reads-survive-server-kill.
  # r[verify builder.fs.passthrough-stack-depth]
  # r[verify builder.fs.passthrough-on-hit]
  # r[verify builder.mountd.backing-broker]
  vm-composefs-spike-priv = composefs-spike-priv { inherit pkgs rio-workspace; };
  vm-spike-fuse-negdentry = spike-fuse-negdentry { inherit pkgs rio-workspace; };

  # ── rio-mountd (P0567): the privileged broker, end-to-end ───────────
  # The real rio-mountd binary against an XFS-prjquota staging loopback,
  # driven over the SOCK_SEQPACKET protocol by spike_mountd_client.
  # Subtest map and how the P0578 perf criteria are gated under KVM
  # (regression envelopes) vs printed-only under TCG: the scenario
  # header.
  # r[verify builder.mountd.fuse-handoff+2]
  # r[verify builder.mountd.backing-broker]
  # r[verify builder.mountd.concurrency]
  # r[verify builder.mountd.build-id-validated]
  # r[verify builder.mountd.uid-bound]
  # r[verify builder.mountd.build-id-unique]
  # r[verify builder.mountd.one-mount]
  # r[verify builder.mountd.staging-quota]
  # r[verify builder.mountd.promote-verified]
  # r[verify builder.mountd.promote-bounded-copy]
  # r[verify builder.mountd.orphan-scan]
  # r[verify builder.mountd.token-admission+2]
  # r[verify builder.mountd.token-node-scoped]
  # r[verify builder.mountd.token-no-node-mint]
  vm-mountd = mountd { inherit pkgs rio-workspace common; };

  # r[verify gw.conn.exit-status]
  #   nom-exit subtest: client ssh_config has ControlMaster auto +
  #   ControlPersist 600. `timeout 60 nom build` must exit 0 (gateway
  #   sends exit-status before eof); `connections_active` must return
  #   to 0 within 15s (gateway disconnects on last-channel-close);
  #   `ssh gateway echo` (rejected exec) must exit ≠124.
  # r[verify store.index.putpath-eager]
  #   eager-nar-index subtest: seedBusybox's `nix copy` → legacy PutPath
  #   must increment rio_store_nar_index_eager_total{outcome=spawned}
  #   (≥1, skipped=0, error=0) and commit nar_indexed + the castore
  #   junction rows without a GetNarIndex call. Structural assertions,
  #   not the plan's literal <100ms stopwatch — see the scenario's
  #   eagerIndexScript header for why.
  vm-protocol-warm-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      # Castore tenancy recipe: HMAC so dispatched assignment tokens
      # carry the tenant claim the tenant-scoped castore reads need;
      # JWT so the inputs the client pushes during the build are
      # attributed to the tenant (store.put.tenant-attribution). The
      # scenario prelude bootstraps as tenant vm-protocol.
      withHmac = true;
      withJwt = true;
      # psql for the eager-nar-index subtest's manifests/castore-table
      # assertions.
      extraPackages = [ pkgs.postgresql_18 ];
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
    withEagerIndexTest = true;
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
      # Same tenancy recipe as vm-protocol-warm-standalone (the Lix
      # client only changes the ssh-ng peer, not the auth chain).
      withHmac = true;
      withJwt = true;
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
      # Castore tenancy recipe: JWT so the seeded busybox closure is
      # attributed to the prelude's tenant (vm-ca-cutoff) and the
      # tenant-scoped castore mounts can read it.
      withJwt = true;
      # psql for the mkBootstrap seed-attribution assert.
      extraPackages = [ pkgs.postgresql_18 ];
    };
  };

  vm-protocol-cold-standalone = protocol {
    inherit pkgs common;
    fixture = standalone {
      # Castore tenancy recipe (see vm-protocol-warm-standalone). The
      # cold DAG's FOD output is attributed by the scheduler at
      # completion, so the consumer's mount works without a seed.
      withHmac = true;
      withJwt = true;
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
  # r[verify store.api.batch-manifest+3]
  #   substitute-cross-tenant-gate: tenant C (untrusted key) → NotFound
  #   on A-substituted path via QueryPathInfo/GetPath/FindMissingPaths;
  #   PermissionDenied via BatchGetManifest (builder-internal). Tenant
  #   B (trusts same key) → visible. Dynamic re-trust proves per-request
  #   trusted_keys read.
  # r[verify store.tenant.narinfo-filter]
  #   built-path-cross-tenant-gate: I-217 — gate hides A's BUILT path
  #   from non-owners. D (no upstream) → NotFound. B (has upstream) →
  #   visible via try_substitute_on_miss (B substitutes independently).
  # r[verify gw.opcode.query-missing]
  # r[verify gw.opcode.query-path-info]
  #   substitute-ssh-ng: gateway propagates JWT through wopQueryPathInfo
  #   → store's try_substitute_on_miss fires → path substitutable via
  #   the real ssh-ng protocol path (not grpcurl backdoor).
  # r[verify gw.activity.subst-progress]
  # r[verify sched.merge.substitute-probe-indeterminate]
  #   substitute-progress-e2e: 4-path closure submitted via ssh-ng;
  #   captured internal-json wire stream asserts every actCopyPath
  #   start has a matching stop, every resProgress has done≤expected,
  #   and per-aid done is monotone non-decreasing. The store-side
  #   indeterminate→hits unit tests cover the probe; this proves the
  #   full scheduler→gateway→client wire path.
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
        # Service-HMAC so scheduler's `walk_substitute_closure` →
        # `SubstituteAuth::Service` mints `x-rio-service-token` +
        # `x-rio-probe-tenant-id`; without it `SubstituteAuth::Jwt([])`
        # → store's `request_tenant_id` returns None →
        # `substitute_path_impl` NotFound. Only the
        # substitute-progress-e2e subtest exercises this path.
        withHmac = true;
        extraStoreConfig = {
          signingKeyFile = "${rioSigningKey}";
          # Setting `extraConfig` replaces mkControlNode's default
          # wholesale, so the required `[chunk_backend]` table must be
          # restated alongside the scenario-specific `[jwt]` table.
          extraConfig = ''
            [chunk_backend]
            kind = "filesystem"
            base_dir = "/var/lib/rio/store/chunks"

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

  # ── dag-delta-sync (two-store standalone fixture) ────────────────────
  # Gateway directory-DAG delta-sync substituter (ADR-022 §8, P0574).
  # Two full control planes: `control` (store-A + the gateway under
  # test) and `storeb` (store-B, the remote peer). The client builds
  # closure v1 and v2 locally, pushes v1 → store-A and v2 → store-B,
  # then `nix copy --substitute-on-destination --to ssh-ng://control`
  # makes gateway-A delta-sync v2 from store-B instead of accepting the
  # whole NAR from the client.
  #
  # r[verify gw.substitute.dag-delta-sync]
  #   subtrees_pruned_total == 76 of 82 dirs (> 0.9 × total) AND
  #   blobs_fetched_total == 1 after syncing a closure that differs in
  #   one file five directories deep — O(changed-subtrees) discovery.
  #   The reassembled path round-trips byte-identically out of store-A.
  vm-dag-delta-sync =
    let
      jwtKeys = import ./lib/jwt-keys.nix;
      jwtPubkey = pkgs.writeText "jwt-pubkey" jwtKeys.pubkeyB64;
      jwtSeed = pkgs.writeText "jwt-seed" jwtKeys.seedB64;
      # Both stores need the JWT pubkey: the castore RPC surface
      # (HasDirectories/GetDirectory/HasBlobs/ReadBlob) is
      # tenant-scoped, and the gateway presents its session JWT to
      # BOTH the local store (the prune oracle) and the remote peer
      # (the fetch source).
      storeJwtConfig = {
        extraConfig = ''
          [chunk_backend]
          kind = "filesystem"
          base_dir = "/var/lib/rio/store/chunks"

          [jwt]
          key_path = "${jwtPubkey}"
        '';
      };
      twoStoreFixture = standalone {
        workers = { };
        extraStoreConfig = storeJwtConfig;
        # Gateway-A signs session JWTs with the seed and delta-syncs
        # from storeb's store.
        extraGatewayEnv = {
          RIO_JWT__KEY_PATH = "${jwtSeed}";
          RIO_SUBSTITUTE_STORE_ADDR = "storeb:9002";
        };
        extraPackages = [ pkgs.postgresql_18 ];
        # The client pushes closure v2 to storeb's gateway over ssh-ng;
        # mkClientNode's ssh_config only covers `control`, so add a
        # second Host block (lines-typed option — definitions concat).
        extraClientModules = [
          {
            programs.ssh.extraConfig = ''
              Host storeb
                HostName storeb
                User root
                Port 2222
                IdentityFile /root/.ssh/id_ed25519
                StrictHostKeyChecking no
                UserKnownHostsFile /dev/null
            '';
          }
        ];
      };
    in
    dag-delta-sync {
      inherit pkgs common;
      fixture = twoStoreFixture // {
        # Second, independent control plane: its own PG, store,
        # scheduler, and gateway. Only the store (the castore RPC
        # source) and the gateway (the ssh-ng push target for seeding
        # closure v2) are exercised; the scheduler just satisfies the
        # module's service dependencies. Same migration-race guard as
        # the standalone fixture's control node.
        nodes = twoStoreFixture.nodes // {
          storeb = {
            imports = [
              (common.mkControlNode {
                hostName = "storeb";
                extraStoreConfig = storeJwtConfig;
              })
            ];
            systemd.services.rio-scheduler.preStart = ''
              for _ in $(seq 1 60); do
                ${pkgs.netcat}/bin/nc -z localhost 9002 && exit 0
                sleep 0.5
              done
              echo "rio-store port 9002 not open after 30s" >&2
              exit 1
            '';
          };
        };
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
  # reassign is disruptive (SIGKILL) → own test. (The pre-castore
  # fuse-direct/fuse-listxattr fragments asserted behavior of the old
  # persistent whole-path FUSE mount; their castore equivalents are
  # vm-castore-e2e subtests — P0560 §B.)
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
          # gentler sibling. Uses worker2 only.
          # ~35s: SIGINT + 30s inactive-wait + restart.
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

  # ── PutPathChunked (ADR-022 §6, P0586) ──────────────────────────────
  # The builder-side fused walk + chunked upload, end-to-end: a real
  # nix build → the real rio-builder fused walk → HasChunks →
  # PutPathChunked → the real store's reconstruct-and-verify → servable
  # back through the gateway. Only the subtests that need the REAL
  # BUILDER live here; the malformed-Begin/tampered-Chunk rejection
  # matrix is rio-store/tests/grpc/put_path_chunked.rs and the
  # real-client-vs-real-server matrix is
  # rio-builder/tests/chunked_upload.rs (see the scenario header for
  # the full subtest disposition).
  #
  # The fixture carries a [chunk_backend] like every store (required
  # config since P0583); the roundtrip fragment asserts the outputs
  # landed with chunk manifests.
  vm-put-path-chunked =
    (put-path-chunked {
      inherit pkgs common;
      fixture = standalone {
        # Castore tenancy recipe (tenant vm-ppc in the prelude): HMAC
        # for the assignment-token tenant claim, JWT for the seeded
        # closure's path_tenants attribution.
        withHmac = true;
        withJwt = true;
        workers = {
          worker = { };
        };
        extraStoreConfig = {
          extraConfig = ''
            [chunk_backend]
            kind = "filesystem"
            base_dir = "/var/lib/rio/store/chunks"
          '';
        };
        # psql for the manifests/chunks/castore-table assertions.
        extraPackages = [ pkgs.postgresql_18 ];
      };
    }).mkTest
      {
        name = "default";
        subtests = [
          # r[verify builder.upload.fused-walk]
          # r[verify builder.upload.chunked-manifest]
          # r[verify builder.upload.batch+2]
          # r[verify builder.upload.references-scanned+2]
          # r[verify store.put.chunked]
          "roundtrip"
          # r[verify store.chunk.has-chunks-durable]
          "dedup"
        ];
      };

  # ── tiered chunk backend (ADR-023, P0555) ───────────────────────────
  # TieredChunkBackend cache semantics against a real S3 API: one
  # Garage server on the control VM hosting BOTH buckets
  # (authoritative + express stand-in), three store replicas sharing
  # PG and the buckets (A gateway-attached + B express-enabled, C with
  # express_bucket unset → local=None). Reads are driven per-replica
  # via grpcurl GetPath; assertions are the rio_store_tiered_*
  # counters + bucket key-set comparisons. See the scenario header for
  # why the plan's "two minio instances" sketch became one Garage with
  # two buckets (minio is insecure-flagged in nixpkgs; both tiers
  # share one AWS_ENDPOINT_URL) and why the express stand-in bucket is
  # not named *--x-s3.
  vm-store-tiered =
    (store-tiered {
      inherit pkgs common;
      fixture = standalone {
        workers = { };
        # Every replica's [chunk_backend] comes from env vars (tiered +
        # Garage endpoint, set by the scenario). An /etc/rio/store.toml
        # would leak replica A's express_bucket into the extra
        # replicas on the same host (the env layer can override a TOML
        # key but never unset one), so drop the fixture's default
        # filesystem TOML entirely.
        extraStoreConfig = {
          extraConfig = "";
        };
      };
    }).mkTest
      {
        name = "default";
        subtests = [
          # r[verify store.backend.tiered-put-remote-first]
          "put-remote-only"
          # r[verify store.backend.tiered-get-fallback]
          "cold-miss-fallback"
          # r[verify obs.metric.chunk-backend-tiered]
          "replica-warm-via-read-through"
          # r[verify store.backend.tiered-get-fallback]
          "local-none-passthrough"
        ];
      };

  # Stock-Nix binary-cache compat (ADR-022 §10, U6): rio-store on the
  # S3 (Garage) backend with binary_cache_compat at its defaults;
  # busybox + the hello closure are uploaded through the gateway, the
  # runtime toggle is flipped off and back on (drop-in env, the same
  # surface helm renders), and a stock CppNix client substitutes the
  # paths straight from the bucket after `systemctl stop rio-store` —
  # narinfo/NAR objects, References traversal, and `nix store verify`
  # all served by the bucket alone. The re-enable subtest pins the
  # reconciler backfilling the path uploaded while compat was off.
  vm-store-compat =
    (store-compat {
      inherit pkgs common;
      fixture = standalone {
        workers = { };
        # The chunk backend comes entirely from env vars (s3 → Garage,
        # set by the scenario); drop the fixture's default filesystem
        # TOML so the env layer is the whole [chunk_backend] config.
        extraStoreConfig = {
          extraConfig = "";
        };
      };
    }).mkTest
      {
        name = "default";
        subtests = [
          # r[verify store.compat.runtime-toggle]
          "compat-off-no-narinfo"
          # r[verify store.compat.reconcile+2]
          # r[verify obs.metric.compat]
          "reconciler-backfill-on-reenable"
          # r[verify store.compat.stock-nix-substitute]
          "stock-nix-substitute"
        ];
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
      # Castore tenancy: the scenario's builds run under the team-test
      # tenant, and the gateway's session JWT is what attributes the
      # seeded closure to it (store.put.tenant-attribution) — without
      # it no build in this scenario can mount its inputs. The
      # jwt-dual-mode subtest's framing moved with this (see the
      # scenario file).
      withJwt = true;
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
      # jwtEnabled: the warmup + e2e builds run through executor pods,
      # so the seeded closure must be tenant-attributed (castore reads
      # are tenant-scoped) — the scenario prelude provisions the
      # vm-sec-nonpriv tenant and keys the client to it.
      jwtEnabled = true;
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
      # Castore tenancy recipe (tenant vm-obs in the prelude).
      withHmac = true;
      withJwt = true;
      workers = {
        worker1 = {
        };
        worker2 = {
        };
        worker3 = {
        };
      };
      withOtel = true;
      # psql for the mkBootstrap seed-attribution assert.
      extraPackages = [ pkgs.postgresql_18 ];
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
      # r[verify ctrl.health.ready-gates-connect]
      "health-shared"
      # r[verify builder.cancel.cgroup-kill]
      "cancel-cgroup-kill"
      # r[verify builder.cgroup.kill-on-teardown]
      # r[verify builder.timeout.no-reassign]
      "build-timeout"
      # r[verify ctrl.pool.reconcile]
      # r[verify ctrl.crd.pool]
      #   pool-lifecycle: apply Pool CRD → wait status → delete
      #   --wait=false. Non-disruptive (no shared-state interference
      #   with the subtests above), so it folds into core rather than
      #   paying a separate k3s boot.
      "pool-lifecycle"
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
      # r[verify store.tenant.find-missing-attribution]
      # r[verify store.put.tenant-attribution+2]
      #   gc-sweep second-tenant tail: gc-tenant-test's `nix copy` of the
      #   already-complete seed closure must be told the paths are missing
      #   (attribution-scoped wopQueryValidPaths), re-stream the bytes, and
      #   earn path_tenants rows (content-verified re-upload) — otherwise
      #   the tenant build that follows poisons at castore mount.
      "gc-sweep"
      # r[verify builder.upload.references-scanned+2]
      # r[verify builder.upload.deriver-populated]
      # r[verify store.gc.two-phase]
      # r[verify builder.fs.parity]
      #   refs-end-to-end doubles as the cutover parity evidence: a real
      #   dep+consumer build reads its inputs through the castore lower
      #   and passes the same refscan/deriver/GC assertions it passed on
      #   the replaced per-build FUSE store, unchanged.
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
      # r[verify ctrl.pool.ephemeral+1]
      # r[verify ctrl.ephemeral.intent-deadline]
      # r[verify ctrl.crd.host-users-network-exclusive]
      # ~180s: two builds × (reconcile tick + pod schedule + FUSE +
      # heartbeat + build + exit). Subtest deletes the default x86-64
      # Pool first so it doesn't steal dispatch.
      "ephemeral-pool"
    ];
    # ephemeral ~180s + ~240s k3s bring-up ≈ 420s expected.
    globalTimeout = 700;
  };

  #
  # Own split (not folded into autoscale): fresh fixture → clean
  # state → fast finalizers. ~4min boot + ~3min subtests.
  # r[verify ctrl.scaler.component+2]
  # r[verify ctrl.scaler.ratio-learn+2]
  # r[verify store.admin.get-load+2]
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
  # r[verify ctrl.nodeclaim.anchor-bulk+5]
  # r[verify ctrl.nodeclaim.priority-bucket]
  # r[verify ctrl.nodeclaim.placeable-gate+4]
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
                  "vmtest:spot": 5.0
          '')
        ];
    };
  };

  vm-componentscaler-k3s = componentscaler {
    inherit pkgs common;
    fixture = k3sFull {
      # jwtEnabled: the 30-leaf slowFanout load must actually RUN its
      # 120s sleeps on builder pods (queued+running ≈ 30 is what drives
      # the predictive scale-up) — a tenant-less submission dies at the
      # tenant-scoped castore mount and the queue collapses. The
      # scenario prelude provisions the vm-cscaler tenant.
      jwtEnabled = true;
      extraValuesTyped = {
        "componentScaler.store.enabled" = true;
        "componentScaler.store.min" = 1;
        "componentScaler.store.max" = 4;
        "componentScaler.store.seedRatio" = 10;
      };
    };
  };

  # r[verify ctrl.scaler.signal-substituting]
  # r[verify store.substitute.admission]
  # r[verify store.admin.get-load+2]
  #   Substitution → ComponentScaler closed loop. 30-leaf substitutable
  #   fanout against a 1-permit store admission gate: scheduler reports
  #   substituting_derivations → ComponentScaler counts it (P1) →
  #   desiredReplicas RISES (never drops mid-cascade) → GetLoad's
  #   substitute_admission_utilization reaches CR.status (P2). Zero
  #   builder pods for the leaves. ~7min (k3s + cache-seed + 90s poll).
  #
  # Distinct runNixOSTest name (rio-substitute-scale) — NOT a variant
  # of rio-componentscaler / rio-substitute, so the derivation name
  # doesn't collide.
  #
  # jwtEnabled: substitution is tenant-scoped (try_substitute_on_miss
  # short-circuits without x-rio-tenant-token); the gateway must mint
  # it from the SSH key comment. seedRatio=10 → 30 substituting leaves
  # predict ceil(30/10)=3 > min=1. substituteAdmissionPermits=1
  # serializes the 30 fetches so (with 200ms tc-netem on upstream-v6)
  # the cascade outlives the controller's 10s reconcile tick — at the
  # derived default (pg_max×3≥64), tiny NARs drain in <1s and
  # desiredReplicas never moves. Set via the chart key (not extraEnv)
  # so the values.yaml → store.yaml templating is exercised.
  # r[verify sched.substitute.eager-probe]
  vm-substitute-scale-k3s = substitute-scale {
    inherit pkgs common;
    fixture = k3sFull {
      jwtEnabled = true;
      extraValuesTyped = {
        "componentScaler.store.enabled" = true;
        "componentScaler.store.min" = 1;
        "componentScaler.store.max" = 4;
        "componentScaler.store.seedRatio" = 10;
        "store.substituteAdmissionPermits" = 1;
      };
    };
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
  # r[verify sched.admin.delete-tenant]
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
  #   (streaming GetDerivationLogs). The frame-prefix grep proves
  #   tonic-web doesn't buffer server-streams — load-bearing for
  #   WatchBuild / live log tail. ~6min (k3s bring-up + Cilium
  #   Gateway reconcile). No separate Envoy Gateway operator —
  #   Cilium's embedded envoy handles the GRPCRoute.
  # r[verify dash.auth.method-gate+3]
  #   The fixture doesn't set dashboard.enableMutatingMethods so the
  #   rio-scheduler-mutating HTTPRoute is absent — `kubectl get
  #   httproute rio-scheduler-mutating` fails. Proves the helm-template
  #   fail-closed holds at runtime through the operator's reconcile.
  # r[verify dash.journey.build-to-logs]
  #   The GetDerivationLogs 0x80 trailer assertion proves server-streaming
  #   works through the nginx→Cilium Gateway→scheduler chain. Handler
  #   returns errors as in-stream items (not tonic Trailers-Only) so
  #   tonic-web encodes them as 0x80 body frames browser fetch can read.
  #
  #   Appended (when `dockerImages ? dashboard`): the nginx-pod curl
  #   assertions from dashboard.nix — SPA served + try_files fallback
  #   + gRPC-Web 0x00/0x80 THROUGH nginx + method-gate 404. Coverage
  #   mode (no rio-dashboard image) runs the gateway/EDS/tonic-web
  #   subtests only; the nginx pod is absent so its curls are skipped.
  vm-dashboard-k3s = dashboard-gateway {
    inherit pkgs common;
    fixture = k3sFull { gatewayEnabled = true; };
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
  # r[verify store.netpol.egress+2]
  #   store-egress IMDS-deny + postgres-allow probe via nsenter into
  #   rio-store pod netns (netpol-store-egress subtest).
  # r[verify builder.netpol.airgap]
  #   builder-egress IMDS-deny + k8s-API-deny + DNS-TCP-allow probes
  #   (netpol-kubeapi / netpol-imds / netpol-dns-tcp subtests).
  vm-netpol-k3s = netpol {
    inherit pkgs common;
    fixture = k3sFull {
      # jwtEnabled: the warmup / cross-ns probes need a builder pod that
      # stays Running on its long-sleep build, which means the castore
      # mount of the seeded busybox must succeed → tenant-attributed
      # seed (scenario prelude provisions the vm-netpol tenant).
      jwtEnabled = true;
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
      # jwtEnabled: both ingress halves run a real build, so each
      # client's seed push must be tenant-attributed for the castore
      # mounts (the scenario keys both clients to the vm-ingress tenant).
      jwtEnabled = true;
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
  #   fod-fail subtest: failing FOD propagates within 60s. Daemon's
  #   post-fail stat($out) hits the castore lower; a name outside the
  #   closure → ENOENT without store contact. P0308 hang would push
  #   elapsed past timeout 90.
  vm-fetcher-split-k3s = fetcher-split {
    inherit pkgs common drvs;
    fixture = k3sFull {
      withV4Nodes = true;
      # jwtEnabled: the consumer half of the dispatch build runs on a
      # Builder pod and mounts the seeded busybox via castore, so the
      # seed must be tenant-attributed (scenario prelude provisions the
      # vm-fetcher-split tenant and keys the client to it).
      jwtEnabled = true;
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

  # ── castore-e2e splits (2 tests, k3s prod-parity fixture) ────────────
  # P0560 §B: the castore-FUSE stack end-to-end on the production pod
  # path. Two tests built from one fragments-style scenario
  # (scenarios/castore-e2e.nix) — a monolith would not fit a 30 min
  # globalTimeout. Per-build metric assertions are taken during each
  # build's sleep tail (one-shot pods), cache assertions are host-side
  # probes of /var/rio on k3s-agent; no wall-clock gates anywhere.
  vm-castore-e2e-core = castoreMod.mkTest {
    name = "core";
    subtests = [
      "seed-core"
      # r[verify builder.fs.castore-stack]
      # r[verify builder.fs.castore-dag-source]
      # r[verify builder.fs.digest-fuse-open]
      # r[verify builder.fs.streaming-open]
      # r[verify builder.fs.streaming-open-threshold]
      # r[verify builder.overlay.castore-lower]
      # r[verify builder.fs.fd-handoff-ordering+2]
      # r[verify builder.mountd.fuse-handoff+2]
      # r[verify builder.fs.shared-backing-cache]
      # r[verify obs.metric.castore-fuse]
      # r[verify obs.metric.mountd]
      # r[verify store.index.putpath-bg-warm]
      #   cold-read: first build to touch a 32 MiB input — the open
      #   dispatches as miss_stream (streaming engaged above the 8 MiB
      #   threshold), the 4 KiB sibling as miss_small, exactly one DAG
      #   prefetch runs, the streamed prefix byte-compares against the
      #   separately-pushed head, the fill promotes into
      #   /var/rio/{cache,chunks}, and rio-mountd's Mount/Promote
      #   counters move. The prelude's nar_indexed gate plus this read
      #   is the putpath-bg-warm evidence; the build running at all on
      #   a castore lower under the one-shot pod path is the
      #   castore-stack / overlay-lower / fd-handoff evidence.
      "cold-read"
      # r[verify builder.fs.passthrough-on-hit]
      # r[verify builder.fs.shared-backing-cache]
      # r[verify builder.fs.node-digest-cache]
      #   warm-read: a different drv re-reads the same input on the same
      #   node — open_case=hit, served via passthrough, zero read
      #   upcalls, zero fetched bytes, no remote-path opens.
      "warm-read"
      # r[verify builder.fs.passthrough-on-hit]
      #   passthrough-small: a ≤threshold input opened twice (miss then
      #   hit) — both passthrough, zero read upcalls, never streamed.
      "passthrough-small"
      # r[verify builder.fs.node-digest-cache]
      # r[verify builder.fs.shared-backing-cache]
      #   cross-build-dedup: a third distinct drv re-reads cold-read's
      #   inputs — ≥32 MiB served from the node cache, zero remote bytes.
      "cross-build-dedup"
      # r[verify builder.fs.castore-inode-digest]
      #   inode-dedup: two store paths with identical bytes report the
      #   same inode inside the sandbox (stat -c %i) and one cache entry.
      "inode-dedup"
      # r[verify builder.fs.castore-cache-config]
      #   stat-dcache-absorbed: five traversals of a 50-file tree keep
      #   lookup/getattr/readdir upcalls at one-traversal counts
      #   (infinite TTLs + READDIRPLUS + FOPEN_CACHE_DIR absorb repeats).
      "stat-dcache-absorbed"
      # r[verify builder.fs.listxattr-size-branch]
      #   xattr-copy: GNU cp -a (llistxattr size probe + size>0 fetch)
      #   of a file and a tree out of the castore lower succeeds.
      "xattr-copy"
      # r[verify builder.fs.node-chunk-cache]
      #   chunk-cache-stream: evict the whole-file backing entry, leave
      #   the chunks, stream again — the fill sources node_ssd chunks,
      #   re-fetches (almost) nothing, and re-promotes the file.
      "chunk-cache-stream"
      #   cache-readonly: the sandbox's attempt to create
      #   /var/rio/cache/ab/test never lands (P0571 posture; asserted on
      #   passthrough-small's pod evidence + a host probe).
      "cache-readonly"
    ];
    globalTimeout = 1800;
  };

  vm-castore-e2e-faults = castoreMod.mkTest {
    name = "faults";
    subtests = [
      "seed-faults"
      #   chunk-warm: clean streaming baseline (fixture canary) + cache
      #   pre-warm; fills the chunk inventory integrity-fail corrupts.
      #   Under the castore_read_scope enforce default this build (and
      #   every other real build in the suite) only works because the
      #   builder presents its closure — the green run is the positive
      #   half of the scope-denied evidence below.
      "chunk-warm"
      # r[verify builder.castore.scope-present]
      # r[verify store.castore.closure-scope]
      #   scope-denied: ADR-022 P0591 negative evidence — a sibling
      #   assignment token (same tenant, minted in-VM with the cluster's
      #   own HMAC key, closure digest over a disjoint closure) presents
      #   its closure, gets served for an in-closure object (positive
      #   control), and gets NOT_FOUND + an out_of_scope deny count on
      #   build A's (chunk-warm's) 24 MiB input — the object a
      #   tenant-wide token could read before closure scoping.
      "scope-denied"
      # r[verify builder.fs.file-digest-integrity]
      #   integrity-fail: a size-preserving corruption of a node-cache
      #   chunk is caught by the streaming fill's whole-file BLAKE3 —
      #   integrity_fail_total == 1, the reader gets EIO, nothing is
      #   promoted, and the store layer never sees it.
      "integrity-fail"
      # r[verify builder.mountd.orphan-scan]
      # r[verify builder.fs.mountd-reconnect]
      # r[verify builder.fs.promote-degrade-staged]
      #   mountd-restart: a build holding a passthrough fd survives the
      #   broker force-restart and completes; the restarted daemon's
      #   startup scan reaps a planted orphan staging dir while leaving
      #   the in-flight build's (live-sentinel-held) staging and the
      #   shared /var/rio/{cache,chunks} trees alone; a fresh build then
      #   does a full miss → fetch → Promote against the new daemon.
      #   (Round 3b finding (c) was the empty expected_outputs HMAC
      #   claim on gRPC-direct submissions failing every upload, plus
      #   the orphan scan reaping the live build's staging — both fixed.)
      #   The cold-miss phase then kills the broker again mid-build so a
      #   COLD whole-file miss meets a dead connection: the build must
      #   complete via the client's reconnect (re-dial + re-Mount +
      #   retried Promote) or the degraded staged serve, with zero EIO
      #   (the round-3b reconnect-or-degrade follow-up).
      "mountd-restart"
      # r[verify builder.result.input-eio-is-infra]
      #   eio-infra-retry: with the store's chunk objects offline the
      #   daemon's execve of a castore-served builder fails; the
      #   executor reclassifies the daemon's exit-code-1 wrap (the
      #   `executing '<root>': Input/output error` log-tail shape) as
      #   InfrastructureFailure, the scheduler re-queues without
      #   poisoning, and the same derivation completes once the chunks
      #   are restored. (Round 3b finding (a) was the MiscFailure-only
      #   status gate missing the post-`\2` PermanentFailure wrap.)
      "eio-infra-retry"
      # r[verify builder.fs.fetch-circuit]
      #   eio-circuit-breaker: rio-store scaled to 0 mid-build — the six
      #   concurrent never-cached opens fail within their fetch budgets
      #   (served in two waves under fuse_threads=4) and the breaker
      #   opens during the real outage (circuit_open=1); that is what
      #   this subtest asserts. Fail-fast of subsequent opens against
      #   the open breaker is unit-verified (castore_fuse/tests.rs
      #   open_fails_fast_once_the_circuit_trips + circuit.rs tests).
      #   Last: the most disruptive fault (whole-store outage), so a
      #   restoration hiccup cannot poison a later subtest.
      "eio-circuit-breaker"
    ];
    globalTimeout = 1800;
  };
}
# Spike: P0564 — confirm /dev/kvm via extra-sandbox-paths (hostPath
# analogue) reaches the Nix sandbox; supports dropping the device-plugin
# approach for /dev/kvm. Rio-stack-independent → no profraws →
# excluded from coverage mode (keeps codecov after_n_builds stable).
// pkgs.lib.optionalAttrs (!coverage) {
  vm-kvm-hostpath-spike = kvm-hostpath-spike { inherit pkgs common; };
}
