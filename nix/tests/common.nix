# Shared helpers for the per-phase VM tests.
#
# Duplication extracted from phase{1a,1b,2a,2b,2c}.nix:
#   - Control node config (5× near-identical → mkControlNode)
#   - PostgreSQL config block (5× identical → postgresqlConfig)
#   - SSH key setup Python (5× → sshKeySetup)
#   - Worker node config (3× in 2a/2b/2c → mkWorkerNode)
#   - Client node config (5× → mkClientNode)
#   - Control-plane wait testScript (5× → waitForControlPlane)
#   - Seed-busybox testScript (5× → seedBusybox)
#   - Build + journal-dump-on-failure (5× → mkBuildHelperV2)
#   - let-bindings (busybox, busyboxClosure, databaseUrl)
#
# Usage:
#   let
#     common = import ./common.nix { inherit pkgs rio-workspace rioModules; };
#   in pkgs.testers.runNixOSTest {
#     nodes.control = common.mkControlNode { hostName = "control"; };
#     nodes.worker1 = common.mkWorkerNode { hostName = "worker1"; maxBuilds = 2; };
#     nodes.client  = common.mkClientNode { gatewayHost = "control"; };
#     testScript = ''
#       start_all()
#       ${common.waitForControlPlane "control"}
#       ${common.sshKeySetup "control"}
#       ${common.seedBusybox "control"}
#       ${common.mkBuildHelperV2 {
#         gatewayHost = "control";
#         dumpLogsExpr = "dump_all_logs([control] + all_workers)";
#       }}
#       out = build("${testDrvFile}")
#     '';
#   }
{
  pkgs,
  rio-workspace,
  rioModules,
  # Coverage mode: rio-workspace is the instrumented build,
  # LLVM_PROFILE_FILE is set in all rio-* service environments, and
  # collectCoverage emits the profraw-collection testScript snippet.
  # false (default, vmTests) → all three are no-ops.
  coverage ? false,
}:
let
  inherit (pkgs) lib;

  # --- Coverage plumbing (all are no-ops when coverage=false) ---
  # LLVM_PROFILE_FILE template: %p = PID (handles service restarts —
  # phase3a/3b do `systemctl restart rio-*` multiple times), %m =
  # binary signature (each binary has a distinct coverage map; enables
  # safe on-line merging).
  #
  # DOUBLE-% ESCAPE: systemd's Environment= expands specifiers (%p =
  # unit prefix name, %m = machine ID, etc). Without escaping, systemd
  # replaces %p with e.g. "rio-gateway" before the binary sees it →
  # restarts overwrite the same file (no PID uniqueness). `%%` →
  # literal `%` → LLVM sees `%p-%m` and expands correctly.
  covEnv = lib.optionalAttrs coverage {
    LLVM_PROFILE_FILE = "/var/lib/rio/cov/rio-%%p-%%m.profraw";
  };
  covTmpfiles = lib.optional coverage "d /var/lib/rio/cov 0755 root root -";
  # Instrumented binaries are ~2× RSS; bump VM memory.
  covMemBump = if coverage then 256 else 0;
in
rec {
  # ── Shared let-bindings ─────────────────────────────────────────────

  # The workspace derivation itself. Scenarios interpolate
  # `${common.rio-workspace}/bin/rio-cli` (or any other bin) directly
  # into testScript — string interpolation pulls the store path into
  # the VM closure, same pattern as grpcurl in lifecycle.nix.
  inherit rio-workspace;

  # Re-exported so scenario mkTest can branch on coverage mode.
  inherit coverage;

  # Instrumented binaries + k3s airgap-image re-import are slower;
  # pad globalTimeout. 300s covers the observed k3s-full cold-import
  # delta under coverage (~4min vs ~1.5min) with slack. Additive so
  # explicit globalTimeout overrides stack. No-op in normal-mode CI.
  covTimeoutHeadroom = if coverage then 300 else 0;

  # Shell env prefix for non-systemd binary invocations (e.g., rio-cli
  # in scenarios/cli.nix). Single %: the shell doesn't expand %p/%m so
  # no escape needed — the double-%% above is systemd-specific. Empty
  # string when coverage=false → safe to interpolate unconditionally.
  covShellEnv = if coverage then "LLVM_PROFILE_FILE=/var/lib/rio/cov/rio-%p-%m.profraw " else "";

  # ── Fragment-test composition (lifecycle/scheduling/leader-election) ─
  # mkFragmentTest builds a runNixOSTest from a scenario-local `prelude`
  # (Python test-setup), a `fragments` attrset (name → `with subtest: ...`
  # body string), and a `subtests` selection list. Eval-time ordering
  # constraints go through `chains` (list of { before, after, msg } or
  # { name, last = true, msg } — see mkAssertChains below).
  #
  # Three scenarios share this exact shape; before P0378 each had a
  # verbatim ~20L let-binding. `scenario` prefixes the test name
  # (`rio-lifecycle-full`, `rio-scheduling-fuse`, ...). Curried: the
  # scenario file partially applies { scenario, prelude, fragments,
  # fixture, chains, defaultTimeout } once and re-exports the resulting
  # { name, subtests, globalTimeout? } → test function — same call
  # signature as the per-scenario let-bindings it replaces, so
  # default.nix callers are untouched.
  mkFragmentTest =
    {
      scenario,
      prelude,
      fragments,
      fixture,
      chains ? [ ],
      defaultTimeout ? 600,
    }:
    {
      name,
      subtests,
      globalTimeout ? defaultTimeout,
    }:
    assert mkAssertChains scenario chains subtests;
    pkgs.testers.runNixOSTest {
      name = "rio-${scenario}-${name}";
      skipTypeCheck = true;
      globalTimeout = globalTimeout + covTimeoutHeadroom;
      inherit (fixture) nodes;
      testScript = ''
        ${prelude}
        ${lib.concatMapStrings (s: fragments.${s} + "\n") subtests}
        ${collectCoverage fixture.pyNodeVars}
      '';
    };

  # Chain assertions: each entry is either
  #   { before = "a"; after = "b"; msg = "..."; }  → a must precede b
  #   { name = "x"; last = true; msg = "..."; }    → x must be last
  # Skipped if the constrained subtest (`after` or `name`) is not in
  # `subtests` — subset runs don't trip the chain. Both lifecycle's
  # finalizer←autoscaler and scheduling's fuse-slowpath=LAST fit;
  # leader-election has no chains (empty list → `all` returns true).
  #
  # lib.assertMsg throws with the message on failure, so the first
  # violated chain's message surfaces as the eval error. `last` is
  # bound lazily — only forced if a {last=true} chain is present AND
  # that name is in subtests, so empty subtests + empty chains is safe.
  mkAssertChains =
    scenario: chains: subtests:
    let
      idx = name: lib.lists.findFirstIndex (s: s == name) (-1) subtests;
      has = name: builtins.elem name subtests;
      last = builtins.elemAt subtests (builtins.length subtests - 1);
      checkOne =
        c:
        # (c.last or false) — truthiness, not presence. `?` checks presence;
        # {last=false} should behave like omitting last (falls to else-branch),
        # not assert bool==string. Common gotcha vs languages where ? is
        # null-safety.
        if (c.last or false) then
          lib.assertMsg (!(has c.name) || last == c.name) "${scenario}: ${c.msg}"
        else
          lib.assertMsg (
            !(has c.after) || (has c.before && idx c.before < idx c.after)
          ) "${scenario}: ${c.msg}";
    in
    builtins.all checkOne chains;

  # Static busybox: closure of exactly 1 path (no glibc, no runtime deps).
  # The sole input seed for all VM tests — FUSE fetches it on every worker,
  # validating the lazy-fetch path.
  inherit (pkgs.pkgsStatic) busybox;

  # closureInfo gives us store-paths + registration (narinfo) for seeding.
  # Even though pkgsStatic.busybox's closure should be {busybox} alone,
  # use closureInfo to be defensive against unexpected refs.
  busyboxClosure = pkgs.closureInfo { rootPaths = [ busybox ]; };

  # PostgreSQL connection URL — both store and scheduler run migrations on
  # startup (sqlx migrate, advisory-lock serialized), so no separate oneshot.
  databaseUrl = "postgres://postgres@localhost/rio";

  # Shared Python assertion helpers (scrape_metrics, assert_metric_exact,
  # assert_set_eq, psql, dump_all_logs, load_otel_spans). Scenarios prepend
  # `${common.assertions}` to their testScript. See lib/assertions.py.
  assertions = builtins.readFile ./lib/assertions.py;

  # KVM hard-fail gate — verifies /dev/kvm is openable RDWR and
  # KVM_CREATE_VM ioctl succeeds before start_all(). Hard-fails with
  # a clear message if not, instead of silently falling back to TCG
  # (TCG timing differs from KVM → false positives/negatives).
  # Scenarios prepend `${common.kvmCheck}` before start_all().
  kvmCheck = import ./lib/kvm-check.nix;

  # ── PostgreSQL config (5× identical across all tests) ───────────────

  postgresqlConfig = {
    services.postgresql = {
      enable = true;
      enableTCPIP = true;
      # Trust auth inside the VM (no password). Covers local unix socket
      # AND the 10.0.0.0/8 test network (nixosTest's default vlan range).
      authentication = lib.mkForce ''
        local all all trust
        host  all all 127.0.0.1/32 trust
        host  all all ::1/128 trust
      '';
      initialScript = pkgs.writeText "rio-init.sql" ''
        CREATE DATABASE rio;
      '';
    };
  };

  # ── Gateway tmpfiles (5× identical) ─────────────────────────────────
  # Gateway starts after store + scheduler via After= in the module, but
  # load_authorized_keys() bails on 0 keys (server.rs:90) → process
  # exit → Restart=on-failure churns every 5s until sshKeySetup runs
  # (each churn = gRPC connect to store+scheduler, then bail; on
  # coverage builds, each also flushes profraw). Seed a throwaway
  # ed25519 public key via tmpfiles so the unit starts cleanly. The
  # private half was discarded at generation — authorizes nothing.
  # sshKeySetup truncates with the client's real key + restarts before
  # any connect happens. Same fix as k3s-full.nix 03-gateway-ssh-
  # placeholder (6da3676).
  gatewayPlaceholderKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICOWXl9/32g/wAtRqYblAdI7wmPNL6phTBMlkn2o6psr placeholder-unused-vmtest";
  gatewayTmpfiles = [
    "d /var/lib/rio 0755 root root -"
    "d /var/lib/rio/gateway 0755 root root -"
    "f /var/lib/rio/gateway/authorized_keys 0600 root root - ${gatewayPlaceholderKey}"
  ];

  # ── Control node config (5× near-identical) ─────────────────────────
  #
  # Runs PostgreSQL + rio-store + rio-scheduler + rio-gateway on one VM.
  # All 5 phase tests share this topology for the control plane; only
  # per-phase knobs (memory, firewall, scheduler extras) vary.
  #
  # Use as a full node definition for simple cases:
  #   control = common.mkControlNode { hostName = "control"; };
  #
  # Or as an import when layering extras (phase2b adds Tempo on top —
  # NixOS module merging handles systemd.services, systemPackages etc.):
  #   control = {
  #     imports = [ (common.mkControlNode { hostName = "control"; ... }) ];
  #     systemd.services.tempo = { ... };
  #   };
  mkControlNode =
    {
      hostName,
      memorySize ? 1024,
      diskSize ? 4096,
      # Merged into services.rio.scheduler via // — phase2c passes
      # extraConfig + tickIntervalSecs for size-class TOML routing.
      extraSchedulerConfig ? { },
      # Merged into services.rio.store via // — phase2c passes
      # extraConfig for [chunk_backend] TOML.
      extraStoreConfig ? { },
      # Appended to the base set [ 2222 9001 9002 ]. phase2a/2c open
      # metrics ports; phase2b also opens Tempo's OTLP + query ports.
      extraFirewallPorts ? [ ],
      # Appended to the base [ pkgs.curl ] — every build-capable test
      # scrapes metrics via curl on the control node. phase1a doesn't
      # need curl but getting it is harmless.
      extraPackages ? [ ],
      # Merged into systemd.services.rio-{store,scheduler,gateway}.environment.
      # phase3b uses this to set RIO_TLS__* + RIO_HMAC_KEY_PATH without
      # extending the NixOS modules. NixOS attrsOf merge composes this
      # with each module's own `environment = {...}` block — figment reads
      # the union. Same env applied to all three services (TLS is the
      # same cert for the shared control VM; unknown vars are ignored).
      extraServiceEnv ? { },
    }:
    {
      imports = [
        rioModules.store
        rioModules.scheduler
        rioModules.gateway
        postgresqlConfig
      ];
      networking.hostName = hostName;

      # phase3b TLS/HMAC env injection + coverage env. Empty attrset
      # = no-op (NixOS module merge with {} is identity). When set,
      # the module system merges these keys with each module's own
      # `environment = {...}` — no risk of clobbering RIO_LISTEN_ADDR
      # etc. covEnv is {} when coverage=false.
      systemd.services = {
        rio-store.environment = extraServiceEnv // covEnv;
        rio-scheduler.environment = extraServiceEnv // covEnv;
        rio-gateway.environment = extraServiceEnv // covEnv;
      };

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty"; # human-readable in VM test logs
        store = {
          enable = true;
          inherit databaseUrl;
        }
        // extraStoreConfig;
        scheduler = {
          enable = true;
          storeAddr = "localhost:9002";
          inherit databaseUrl;
        }
        // extraSchedulerConfig;
        gateway = {
          enable = true;
          schedulerAddr = "localhost:9001";
          storeAddr = "localhost:9002";
          authorizedKeysPath = "/var/lib/rio/gateway/authorized_keys";
        };
      };

      systemd.tmpfiles.rules = gatewayTmpfiles ++ covTmpfiles;

      environment.systemPackages = [ pkgs.curl ] ++ extraPackages;

      # 2222 = gateway SSH (client), 9001 = scheduler gRPC (workers),
      # 9002 = store gRPC (workers). phase1a has no workers so 9001/9002
      # are technically unused cross-VM there, but opening them is a no-op.
      networking.firewall.allowedTCPPorts = [
        2222
        9001
        9002
      ]
      ++ extraFirewallPorts;

      virtualisation = {
        cores = 4;
        memorySize = memorySize + covMemBump;
        inherit diskSize;
      };
    };

  # ── Worker node config (3× near-identical in 2a/2b/2c) ──────────────
  #
  # Parameterized by:
  #   - hostName: VM hostname (also used as worker_id)
  #   - maxBuilds: concurrent build slots (phase2a=2, phase2b=1, phase2c=2)
  #   - sizeClass: optional size-class tag (phase2c only)
  #   - otelEndpoint: optional OTLP endpoint (phase2b only; worker spans
  #     not strictly needed for the milestone but make the trace tree
  #     look like the observability.md spec diagram)
  #
  # The writableStore=false setting is load-bearing —
  # see the inline rationale. The 4-core virtualisation setting is also
  # intentional (tokio multi_thread runtime uses num_cpus worker threads;
  # FUSE callbacks doing Handle::block_on(gRPC) need spare worker threads
  # to drive the reactor).
  mkWorkerNode =
    {
      hostName,
      maxBuilds,
      sizeClass ? null,
      otelEndpoint ? null,
      # Merged into systemd.services.rio-builder.environment. phase3b
      # uses this to set RIO_TLS__* (client cert for mTLS to scheduler/
      # store). Composed with the optional RIO_OTEL_ENDPOINT below via //.
      extraServiceEnv ? { },
      # Arbitrary store paths to pull into this VM's /nix/store. Used
      # by scenarios/protocol (cold) to pre-stage the busybox file for
      # builtin:fetchurl (workers are airgapped; file:// URL needs the
      # file to already be in /nix/store for the sandbox to read it).
      extraStorePaths ? [ ],
    }:
    {
      imports = [ rioModules.worker ];
      networking.hostName = hostName;

      services.rio = {
        package = rio-workspace;
        logFormat = "pretty"; # human-readable in test logs
        worker = {
          enable = true;
          schedulerAddr = "control:9001";
          storeAddr = "control:9002";
          inherit maxBuilds;
        }
        // lib.optionalAttrs (sizeClass != null) { inherit sizeClass; };
      };

      # OTel endpoint for the worker (phase2b). Worker spans aren't
      # strictly needed for the milestone (gateway→scheduler is the
      # critical trace hop), but having them in Tempo makes the trace
      # tree match the observability.md spec diagram.
      systemd.services.rio-builder.environment =
        lib.optionalAttrs (otelEndpoint != null) {
          RIO_OTEL_ENDPOINT = otelEndpoint;
        }
        // extraServiceEnv
        // covEnv;

      systemd.tmpfiles.rules = covTmpfiles;

      # curl for metric scraping.
      environment.systemPackages = [ pkgs.curl ];
      # Force extraStorePaths into the VM's store closure via a
      # string-context reference. The etc file is never read.
      environment.etc."rio/worker-extra-paths".text = lib.concatMapStrings (p: "${p}\n") extraStorePaths;

      # 4 cores: tokio multi_thread runtime uses num_cpus worker threads.
      # FUSE callbacks doing Handle::block_on(gRPC) need spare worker
      # threads to drive the reactor; 4 cores gives headroom at
      # maxBuilds=2.
      virtualisation = {
        memorySize = 1024 + covMemBump;
        diskSize = 4096;
        cores = 4;
        # Worker VMs must NOT have a writable /nix/store. With
        # writableStore=true (the NixOS-test default), /nix/store is
        # itself an overlayfs (tmpfs upper on 9p lower). Our per-build
        # overlay uses /nix/store as a lower; overlay-on-overlay breaks
        # copy-up → nix-daemon creates chroot dirs, builder writes $out,
        # but the parent can't see it → OutputRejected. With
        # writableStore=false, /nix/store is the plain 9p mount.
        writableStore = false;
        # /nix/var stays on the writable root fs (/): the 9p mount is at
        # /nix/.ro-store → /nix/store, NOT /nix. Host nix-daemon's gcroots
        # etc. work without any extra mount. (A `fileSystems."/nix/var"`
        # tmpfs sat here for months — dead code: qemu-vm.nix does
        # `fileSystems = mkVMOverride virtualisation.fileSystems` at
        # priority 10, silently dropping plain `fileSystems.*` defs. Never
        # applied, never needed, premise was wrong.)
      };
    };

  # ── Client node config (5× near-identical) ──────────────────────────
  #
  # Parameterized by `gatewayHost`: the SSH target hostname (phase1a
  # uses "gateway", all others use "control"). `extraPackages` lets
  # phase2a/2b/2c add curl.
  mkClientNode =
    {
      gatewayHost,
      # k3s-full fixture: gateway is a NodePort Service, not port 2222.
      gatewayPort ? 2222,
      # Chart-deployed gateway uses `rio` (see gateway.yaml); NixOS module
      # uses `root`.
      gatewayUser ? "root",
      extraPackages ? [ ],
    }:
    {
      networking.hostName = "client";

      # ca-derivations: ca-cutoff.nix evaluates `__contentAddressed = true`
      # on the client (eval-side feature gate, not build-side — the build
      # goes via ssh-ng to rio-gateway which doesn't check nix.conf).
      # Harmless for non-CA scenarios.
      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
        "ca-derivations"
      ];

      # Busybox + closure must be in the client's local store so
      # `nix copy` can read + upload them. Referencing them in the
      # config pulls them in.
      environment.systemPackages = [ busybox ] ++ extraPackages;
      # Force closureInfo into the VM's store (not otherwise a runtime dep).
      environment.etc."rio/busybox-closure".source = "${busyboxClosure}";

      # ssh-ng does not support the ?ssh-key= URL query param reliably
      # across Nix versions; use ~/.ssh/config instead.
      programs.ssh.extraConfig = ''
        Host ${gatewayHost}
          HostName ${gatewayHost}
          User ${gatewayUser}
          Port ${toString gatewayPort}
          IdentityFile /root/.ssh/id_ed25519
          StrictHostKeyChecking no
          UserKnownHostsFile /dev/null
      '';

      virtualisation.memorySize = 1024;
      virtualisation.cores = 4;
    };

  # ── SSH key setup testScript snippet ────────────────────────────────
  #
  # Generate client key, install on the gateway host, restart gateway so
  # load_authorized_keys() picks up the real key. The gateway started
  # with the placeholder key (gatewayTmpfiles above — authorizes
  # nothing); `>` truncates, then restart swaps it in.
  #
  # Interpolate as `${common.sshKeySetup "control"}` in testScript.
  # The `gatewayHost` arg is the Python variable name for the gateway
  # node (phase1a: `gateway`, all others: `control`).
  # -C "" sets an empty key comment. Without it, ssh-keygen defaults
  # to `user@host` which (since phase4a commit 2.3) the gateway treats
  # as a tenant name → scheduler rejects as "unknown tenant". Empty
  # comment = single-tenant mode (tenant_id = NULL).
  sshKeySetup = gatewayHost: ''
    client.succeed("mkdir -p /root/.ssh && ssh-keygen -t ed25519 -N ''' -C ''' -f /root/.ssh/id_ed25519")
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    ${gatewayHost}.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    ${gatewayHost}.succeed("systemctl restart rio-gateway.service")
    ${gatewayHost}.wait_for_unit("rio-gateway.service")
    ${gatewayHost}.wait_for_open_port(2222)
  '';

  # ── Control-plane wait (5× identical) ───────────────────────────────
  # Blocks until postgres + rio-store + rio-scheduler are all ready.
  # Gateway startup is handled separately by sshKeySetup (restart after
  # populating authorized_keys). `node` is the Python variable name for
  # the control node (phase1a: `gateway`, all others: `control`).
  waitForControlPlane = node: ''
    ${node}.wait_for_unit("postgresql.service")
    ${node}.wait_for_unit("rio-store.service")
    ${node}.wait_for_open_port(9002)
    ${node}.wait_for_unit("rio-scheduler.service")
    ${node}.wait_for_open_port(9001)
  '';

  # ── Seed busybox closure (5× identical modulo ssh target) ───────────
  # Uploads the static-busybox closure via `nix copy` over ssh-ng.
  # Exercises wopAddToStoreNar / wopAddMultipleToStore. `--no-check-sigs`
  # because the client's local store paths aren't signed. `gatewayHost`
  # is the SSH target hostname (phase1a: "gateway", others: "control").
  seedBusybox = gatewayHost: ''
    client.succeed("ls ${busybox}")
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://${gatewayHost}' "
        "$(cat ${busyboxClosure}/store-paths)"
    )
  '';

  # ── Build helper v2 (5× scenario copies consolidated) ───────────────
  #
  # Scenario build() helper. Supersedes the v1 helper which baked
  # drv_file at Nix-eval time (one drv per test). Scenarios need
  # multiple drvs per test, so drv_file is a PYTHON-runtime param now.
  #
  # Nix-eval config (varies by fixture, not by call):
  #   gatewayHost  — default ssh-ng://<this> store URL
  #   dumpLogsExpr — Python expression called in the except: arm
  #                  (differs for k3s-full vs standalone — see usage)
  #
  # Python-runtime params (vary per call):
  #   drv_file       — path to .nix file (or .drv)
  #   attr           — -A attribute name (default: build the file's
  #                    top-level expr; "" = no -A flag)
  #   extra_args     — arbitrary --arg/--argstr for FOD scenarios
  #   capture_stderr — 2>&1 (default True; False for stderr-separate
  #                    tests asserting on the clean stdout path)
  #   expect_fail    — use client.fail instead of client.succeed
  #   timeout_wrap   — `timeout N` outer shell wrapper (fod-proxy uses
  #                    60s as a regression hard bound)
  #   store_url      — override --store (default ssh-ng://${gatewayHost});
  #                    tenant/identity-file cases pass a different URL.
  #                    Folds in security.nix build_drv (identity_file →
  #                    ?ssh-key= querystring) AND lifecycle tenant-alias
  #                    builds. What was 3 outliers is now 1 param.
  #   strip_to_store_path — return last non-empty line (skips SSH
  #                    known_hosts warning + build progress under
  #                    2>&1); default True when capture_stderr=True.
  #                    Absorbs the inline last-line-extract that
  #                    security.nix + lifecycle.nix both did.
  #
  # Usage (k3s-full):
  #   ''${common.mkBuildHelperV2 {
  #     gatewayHost  = "k3s-server";
  #     dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="''${ns}")'';
  #   }}
  #
  # Usage (standalone):
  #   ''${common.mkBuildHelperV2 {
  #     gatewayHost  = gatewayHost;  # usually "control"
  #     dumpLogsExpr = "dump_all_logs([''${gatewayHost}] + all_workers)";
  #   }}
  mkBuildHelperV2 =
    { gatewayHost, dumpLogsExpr }:
    ''
      def build(drv_file, attr="", extra_args="", capture_stderr=True,
                expect_fail=False, timeout_wrap=None,
                store_url="ssh-ng://${gatewayHost}",
                strip_to_store_path=None):
          # Default strip_to_store_path follows capture_stderr: SSH
          # warnings only appear under 2>&1; with stderr separate the
          # stdout stream is already a clean store path. Callers that
          # need the FULL 2>&1 output (e.g. trace-id grep, 403 check)
          # pass strip_to_store_path=False explicitly.
          if strip_to_store_path is None:
              strip_to_store_path = capture_stderr
          cmd = (
              f"nix-build --no-out-link --store '{store_url}' "
              f"--arg busybox '(builtins.storePath ${busybox})' "
              f"{extra_args} {drv_file}"
          )
          if attr:
              cmd += f" -A {attr}"
          if capture_stderr:
              cmd += " 2>&1"
          if timeout_wrap is not None:
              cmd = f"timeout {timeout_wrap} {cmd}"
          if expect_fail:
              return client.fail(cmd)
          rc, out = client.execute(cmd)
          if rc != 0:
              print(f"=== nix-build failed rc={rc} ===\n{out}\n=== end output ===")
              ${dumpLogsExpr}
              raise Exception(f"build() failed rc={rc}, see output above")
          if strip_to_store_path:
              # Last non-empty line is the store path. Earlier
              # lines: SSH known_hosts warning + build progress.
              lines = [l.strip() for l in out.strip().split("\n")
                       if l.strip()]
              return lines[-1] if lines else ""
          return out
    '';

  # ── Coverage profraw collection (appended to end of testScript) ─────
  #
  # When coverage=true, stops all rio services (SIGTERM → graceful
  # drain via shutdown_signal → atexit → LLVM profraw flush), tars
  # /var/lib/rio/cov, and copies to $out/coverage/<node>/.
  #
  # Stop ORDER matters: workers first. A dead worker closes the
  # BuildExecution bidi stream → scheduler's worker-stream-reader
  # task breaks → sends WorkerDisconnected → actor drops that
  # worker's stream_tx. By the time the scheduler gets SIGTERM, its
  # serve_with_shutdown has no open response streams to wait on.
  # Belt-and-suspenders for the actor's own token-aware drain.
  #
  # `pyNodeVars` is a Python expression evaluating to a list of Machine
  # instances — typically comma-separated node var names, e.g.,
  # "gateway, client" or "control, worker1, worker2, client".
  #
  # systemctl stop is synchronous (returns when unit inactive). || true
  # tolerates missing services (not every node runs every service).
  # For k8s nodes (phase3a), also deletes STS so the pod terminates
  # cleanly (pod PID 1 gets SIGTERM → worker's existing drain →
  # profraw flushed to hostPath mount).
  collectCoverage =
    pyNodeVars:
    if !coverage then
      ""
    else
      ''
        with subtest("collect coverage profraws"):
            _cov_nodes = [${pyNodeVars}]
            # Pass 1: stop workers on ALL nodes FIRST. The worker's
            # SIGTERM path (main.rs:439 select! arm) only fires if the
            # worker is in the inner build-stream loop when SIGTERM
            # arrives — which requires the scheduler to still be alive.
            # If scheduler dies first (old single-pass ordering:
            # control iteration stops scheduler before worker
            # iterations run), workers see stream-close → enter the
            # reconnect retry loop (main.rs:413-425) which does NOT
            # poll sigterm → hang → systemd SIGKILL → no profraw.
            # Cascading: scheduler's serve_with_shutdown was ALSO
            # hung on the open worker streams → also SIGKILLed →
            # rio-scheduler per-test coverage = 0. Observed in v4:
            # scheduling-standalone worker total=154 (init-only, all
            # from early-exit restarts) vs lifecycle-k3s=1462.
            for n in _cov_nodes:
                n.execute("systemctl stop rio-builder 2>/dev/null || true")
            # Pass 2: control services + k3s + tar. Workers' bidi
            # streams are now closed — scheduler's serve_with_shutdown
            # unblocks immediately.
            for n in _cov_nodes:
                n.execute(
                    "systemctl stop rio-gateway rio-scheduler rio-store "
                    "rio-controller 2>/dev/null || true"
                )
                # k3s pods (phase3a worker-only, k3s-full all components):
                # delete by label → graceful SIGTERM → profraw flush via
                # atexit. Label selector avoids touching bitnami PG /
                # kube-system. Only the k3s SERVER runs this (agent
                # lacks kubeconfig); deletes affect pods on BOTH nodes.
                # Unlike standalone, kubectl deletes ALL rio pods at
                # once → concurrent SIGTERM → no ordering issue.
                #
                # CRITICAL: `kubectl delete deploy,sts --wait=true`
                # waits only for the DEPLOYMENT object to be gone.
                # Pods are still terminating when it returns. The
                # `kubectl wait --for=delete pods` below blocks until
                # pods are actually gone — which means the container
                # process has exited and profraws have flushed to the
                # hostPath. Without this, tar races with pod
                # termination → profraws incomplete → k3s per-test
                # coverage swings 5× between runs (observed 5.5% vs
                # 26.2% for leader-election on otherwise-identical
                # test runs).
                n.execute(
                    "[ -f /etc/rancher/k3s/k3s.yaml ] && {"
                    "  k3s kubectl delete deploy,sts -A "
                    "    -l 'app.kubernetes.io/part-of=rio-build' "
                    "    --wait=true --timeout=60s 2>/dev/null;"
                    "  k3s kubectl wait --for=delete pods -A "
                    "    -l 'app.kubernetes.io/part-of=rio-build' "
                    "    --timeout=60s 2>/dev/null;"
                    "} || true"
                )
                # Empty tarball if dir doesn't exist (e.g., client node
                # runs no rio services).
                n.execute(
                    "mkdir -p /var/lib/rio/cov && "
                    "tar czf /tmp/profraw.tar.gz -C /var/lib/rio/cov . "
                    "2>/dev/null || "
                    "tar czf /tmp/profraw.tar.gz --files-from=/dev/null"
                )
                n.copy_from_vm("/tmp/profraw.tar.gz", f"coverage/{n.name}")
      '';
}
