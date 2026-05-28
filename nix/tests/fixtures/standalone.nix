# Standalone NixOS-modules fixture: PG + store + scheduler + gateway
# on one VM, N worker VMs, 1 client VM. No k8s.
#
# This is the ONLY non-k8s deployment path. Currently implicit in
# phase1/2 tests, never exercised as a deliverable — now it's a fixture
# in its own right.
#
# Returns an attrset with `nodes` (drop into runNixOSTest) and
# `waitReady` + `pyNodeVars` (Python snippets for testScript
# interpolation).
{
  pkgs,
  rio-workspace,
  rioModules,
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
  mkHmacKeys = import ../lib/hmac-keys.nix { inherit pkgs; };
in
{
  # Attrset of worker-node-name → mkWorkerNode args. Keys become
  # hostname + node var in testScript. scenarios/scheduling.nix uses
  # this to pass distinct service env per worker.
  workers ? {
    worker = { };
  },

  # HMAC keys for assignment+service tokens (lib/hmac-keys.nix).
  withHmac ? false,

  # Single-tenant wiring for build-running scenarios (P0560). The castore
  # read surface (DirectoryService GetDirectory/ReadBlob/StatBlob) is
  # tenant-scoped and refuses anonymous callers, so any scenario whose
  # builds materialize inputs through the castore-FUSE lower needs:
  # a tenant row, the client's authorized_keys comment naming it
  # (gateway → scheduler build attribution → AssignmentClaims.tenant),
  # HMAC keys (scheduler signs the assignment token, store verifies it),
  # and a path_tenants row for every input path the builder will read.
  # Nothing in the production chain writes path_tenants for
  # client-uploaded sources/.drvs yet (only the scheduler's
  # completion-time upsert covers built outputs), so the fixture installs
  # a narinfo INSERT trigger attributing every registered path to this
  # tenant — exactly the single-tenant semantics these scenarios model.
  # Setting this to a tenant name (e.g. "vmtest") wires all of that;
  # null keeps the legacy anonymous fixture.
  defaultTenant ? null,

  # opentelemetry-collector on control (OTLP gRPC :4317, file exporter
  # to /var/lib/otelcol/traces.json). Sets RIO_OTEL_ENDPOINT on all
  # services. For scenarios/observability.nix.
  withOtel ? false,

  # Pass-through to mkControlNode ([sla] TOML, tick interval, etc).
  extraSchedulerConfig ? { },
  extraStoreConfig ? { },
  extraPackages ? [ ],
  # Scheduler-only systemd env (config knobs + fixture toggles like
  # RIO_ADMIN_TEST_FIXTURES). Merged on top of extraServiceEnv.
  extraSchedulerEnv ? { },
  # Gateway-only env (RIO_FOO__BAR=... config style). substitute.nix
  # uses this to set RIO_JWT__KEY_PATH for the gateway's signing seed
  # without also applying it to store/scheduler (extraServiceEnv goes
  # to all three, which would conflict — store wants the PUBKEY path,
  # gateway wants the SEED path, same env var name).
  extraGatewayEnv ? { },
  # NixOS modules merged into the client VM. protocol-cold uses this
  # for drvs.coldBootstrapServer (Python http.server serving busybox).
  extraClientModules ? [ ],
  # Threaded to mkClientNode's nix.package. Default = nixpkgs CppNix.
  clientNixPackage ? pkgs.nix,
}:
let
  # defaultTenant implies HMAC: the assignment token is the only way the
  # builder can present the tenant to the store's castore RPCs.
  hmacEnabled = withHmac || defaultTenant != null;
  hmacKeys = if hmacEnabled then mkHmacKeys { } else null;

  # ── Materialization env ──────────────────────────────────────────────
  # The store's executor spawn condition (PD-D2): the scheduler
  # ExecutorService address — the standalone deployment co-locates
  # scheduler and store on `control`. The coexistence ENABLED flag died
  # with Phase D'; materialization is unconditional.
  storeMaterializationEnv = {
    RIO_MATERIALIZATION__SCHEDULER_ADDR = "localhost:9001";
  };

  # ── HMAC env (no-op {} when HMAC is off) ─────────────────────────────
  # Scheduler+store share the assignment-token key; gateway+store share
  # the service-token key. Workers get neither (they receive assignment
  # tokens from the scheduler at dispatch, not from a key file).
  controlHmacEnv = lib.optionalAttrs hmacEnabled {
    RIO_HMAC_KEY_PATH = "${hmacKeys}/hmac.key";
    RIO_SERVICE_HMAC_KEY_PATH = "${hmacKeys}/service-hmac.key";
  };

  gatewayHmacEnv = lib.optionalAttrs hmacEnabled {
    RIO_SERVICE_HMAC_KEY_PATH = "${hmacKeys}/service-hmac.key";
  };

  # Worker EnvironmentFile under HMAC. One credential:
  #
  #   RIO_PULL_SPAWNER_SERVICE_TOKEN — controller-role ServiceClaims
  #     {caller="rio-controller", far-future expiry} signed with
  #     service-hmac.key. The pull spawner (common.nix) presents it as
  #     `x-rio-service-token` on GetSpawnIntents / MintExecutorTokens /
  #     AckSpawnedIntents — exactly the credential rio-controller holds
  #     in k8s for the same calls. The per-intent executor token is NOT
  #     minted here: the spawner asks the scheduler (MintExecutorTokens),
  #     so the real signing/verification path is exercised end to end.
  #     r[sec.authz.service-token].
  #
  # (The legacy static stream-session executor token that used to sit
  # alongside it retired with the stream session machinery — 1c'
  # deletion commit A — together with the executor-kind-spoof probe
  # that read it.)
  #
  # Written as a systemd EnvironmentFile (KEY=value) — NOT
  # readFile-into-env: eval-time readFile of a derivation output is
  # an IFD anti-pattern even now that the hmacKeys derivation is
  # deterministic.
  executorTokenEnv =
    pkgs.runCommand "rio-executor-token-env"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 - ${hmacKeys}/service-hmac.key > $out <<'EOF'
        import base64, hashlib, hmac, json, sys

        def load_key(path):
            key = open(path, "rb").read()
            # Mirror rio-auth load_key() trailing-newline trim.
            for suf in (b"\r\n", b"\n"):
                if key.endswith(suf):
                    key = key[: -len(suf)]
                    break
            return key

        b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()

        def sign(key, claims_dict):
            claims = json.dumps(claims_dict, separators=(",", ":")).encode()
            sig = hmac.new(key, claims, hashlib.sha256).digest()
            return f"{b64(claims)}.{b64(sig)}"

        svc_key = load_key(sys.argv[1])
        print("RIO_PULL_SPAWNER_SERVICE_TOKEN=" + sign(
            svc_key, {"caller": "rio-controller", "expiry_unix": 9999999999}))
        EOF
      '';

  # ── OTel env ────────────────────────────────────────────────────────
  otelEnv = lib.optionalAttrs withOtel {
    RIO_OTEL_ENDPOINT = "http://localhost:4317";
  };
  workerOtelEndpoint = if withOtel then "http://control:4317" else null;

  # ── otelcol module (merged into control via imports) ────────────────
  # File exporter writes one ExportTraceServiceRequest JSON per line.
  # lib/assertions.py load_otel_spans() parses it. debug exporter
  # duplicates spans to journalctl for `systemctl status` debugging.
  #
  # GOTCHAS:
  #   - `file` exporter is in otelcol-CONTRIB, not the base package.
  #     Base package → "unknown exporters type: file" → service fails.
  #   - Service runs with DynamicUser=true + StateDirectory=
  #     opentelemetry-collector → only /var/lib/opentelemetry-collector
  #     is writable. Writing elsewhere → permission denied.
  otelModule = lib.optionalAttrs withOtel {
    services.opentelemetry-collector = {
      enable = true;
      package = pkgs.opentelemetry-collector-contrib;
      settings = {
        receivers.otlp.protocols.grpc.endpoint = "0.0.0.0:4317";
        exporters = {
          file = {
            path = "/var/lib/opentelemetry-collector/traces.json";
            format = "json";
          };
          debug.verbosity = "normal";
        };
        service.pipelines.traces = {
          receivers = [ "otlp" ];
          exporters = [
            "file"
            "debug"
          ];
        };
      };
    };
    networking.firewall.allowedTCPPorts = [ 4317 ];
  };

  workerNames = builtins.attrNames workers;

  # ── Control node ────────────────────────────────────────────────────
  # mkControlNode's extraServiceEnv goes to ALL three services (store,
  # scheduler, gateway). NixOS module merge then composes the gateway
  # override on top (same-key last-writer wins).
  controlNode = {
    imports = [
      (common.mkControlNode {
        hostName = "control";
        extraServiceEnv = controlHmacEnv // otelEnv;
        inherit
          extraSchedulerConfig
          extraStoreConfig
          extraPackages
          extraSchedulerEnv
          ;
        # Metrics ports open for cross-VM scraping (scheduling fanout
        # scenario asserts worker metrics from control).
        extraFirewallPorts = [
          9091
          9092
          9190
        ];
      })
      otelModule
    ];
    # Per-service knobs on top of mkControlNode's shared env:
    #   - environment: gateway-only HMAC override (mkControlNode's
    #     extraServiceEnv applies controlHmacEnv to ALL three services;
    #     mapAttrs mkForce makes the gateway env win unambiguously;
    #     extraGatewayEnv merges alongside) + the store's
    #     materialization scheduler-addr plumbing (never gateway —
    #     mirrors the helm chart's env surface).
    #   - rio-scheduler.preStart: serialize migration runs — migration
    #     011's CREATE INDEX CONCURRENTLY deadlocks with sqlx's
    #     pg_advisory_lock when store and scheduler race on a fresh DB.
    #     The module-level After=rio-store.service (scheduler.nix) only
    #     orders the fork (Type=simple), not readiness — both still hit
    #     migrate!() near-simultaneously. Block scheduler until store's
    #     gRPC port is open, which happens post-migration. k8s
    #     deployments dodge this via pod startup jitter; standalone VM
    #     boot is deterministic enough to trigger the race reliably.
    #     Restart=always (module-level) covers any residual window.
    #   - after: OTel ordering — rio-* services on control must start
    #     AFTER otelcol. Without this, the services boot, try to connect
    #     to each other during boot churn, and the restart dance adds
    #     ~10s of flake. After= doesn't block startup if otelcol is
    #     disabled (unit doesn't exist → no-op), so the mkIf guard is
    #     belt-and-suspenders.
    systemd.services = {
      rio-gateway = {
        environment =
          (lib.optionalAttrs hmacEnabled (lib.mapAttrs (_: lib.mkForce) gatewayHmacEnv)) // extraGatewayEnv;
        after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      };

      rio-scheduler = {
        preStart = ''
          for _ in $(seq 1 60); do
            ${pkgs.netcat}/bin/nc -z localhost 9002 && exit 0
            sleep 0.5
          done
          echo "rio-store port 9002 not open after 30s" >&2
          exit 1
        '';
        after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      };

      rio-store = {
        environment = storeMaterializationEnv;
        after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      };

      # Seed the default tenant + the path-attribution triggers (P0560
      # stopgap, P0593 deletes; SQL shared with the toxiproxy/k3s
      # fixtures via common.tenantStopgapSeedSql — see the scoping
      # rationale there). The gateway resolves the authorized_keys
      # comment to this row at SSH auth time (unknown name → connection
      # rejected), so it must exist before the testScript's first
      # ssh-ng use; waitReady waits for this unit. Retry loop: the
      # tenants/narinfo/path_tenants tables only exist once
      # rio-store/rio-scheduler finish their startup migrations.
      # Client reads stay anonymous (no JWT) and therefore unfiltered,
      # exactly as in the legacy fixture.
      rio-seed-tenant = lib.mkIf (defaultTenant != null) {
        description = "Seed the '${toString defaultTenant}' tenant for VM-test builds";
        wantedBy = [ "multi-user.target" ];
        after = [
          "postgresql.service"
          "rio-store.service"
          "rio-scheduler.service"
        ];
        path = [ pkgs.postgresql ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          for _ in $(seq 1 120); do
            if psql -h /run/postgresql -U postgres -d rio -v ON_ERROR_STOP=1 -1 -f ${common.tenantStopgapSeedSql (toString defaultTenant)}; then
              exit 0
            fi
            sleep 1
          done
          echo "tenant seed never applied (migrations not finished?)" >&2
          exit 1
        '';
      };
    };
  };

  # ── Worker nodes ────────────────────────────────────────────────────
  # mapAttrs' renames to the worker's hostName while passing through
  # the scenario's per-worker args + fixture-level OTel. When HMAC is
  # on, also mount the credentials EnvironmentFile so the pull spawner
  # can present the controller-role service token on the spawn-intent
  # admin calls.
  workerNodes = lib.mapAttrs (name: args: {
    imports = [
      (common.mkWorkerNode (
        args
        // {
          hostName = name;
          otelEndpoint = workerOtelEndpoint;
        }
      ))
    ];
    systemd.services.rio-builder.serviceConfig.EnvironmentFile = lib.mkIf hmacEnabled [
      "${executorTokenEnv}"
    ];
  }) workers;

in
{
  inherit hmacKeys;

  # SSH target for `ssh-ng://${gatewayHost}` + Python node var for
  # `${gatewayHost}.succeed(...)`. Scenarios interpolate into both.
  gatewayHost = "control";

  # Drop into runNixOSTest.
  nodes = {
    control = controlNode;
    client = {
      imports = [
        (common.mkClientNode {
          gatewayHost = "control";
          nixPackage = clientNixPackage;
        })
      ]
      ++ extraClientModules;
    };
  }
  // workerNodes;

  # ── testScript snippets ─────────────────────────────────────────────

  # Wait for control plane + all workers registered. Does NOT include
  # sshKeySetup — scenarios/security.nix needs to do multi-key setup
  # manually. Most scenarios should do `${waitReady} ${common.sshKeySetup "control"}`.
  waitReady = ''
    ${common.waitForControlPlane "control"}
  ''
  + lib.optionalString (defaultTenant != null) ''
    control.wait_for_unit("rio-seed-tenant.service")
  ''
  + lib.optionalString withOtel ''
    control.wait_for_unit("opentelemetry-collector.service")
    control.wait_for_open_port(4317)
  ''
  # Pull-era boot gate: pull-mode workers never register, so there is
  # no `workers_active` count to wait on. The harness-ready signal is
  # each worker's pull spawner having completed one successful
  # GetSpawnIntents poll against the scheduler (proves the unit is up,
  # grpcurl + protoset work, and the admin surface answers — the
  # things the old registration wait actually guarded). Work delivery
  # itself is asserted by the scenarios' own builds.
  + lib.concatMapStrings (w: ''
    ${w}.wait_for_unit("rio-mountd.service")
    ${w}.wait_for_unit("rio-builder.service")
    ${w}.wait_until_succeeds(
        "journalctl -u rio-builder --no-pager | "
        "grep -q 'rio-pull-spawner: scheduler reachable'",
        timeout=60,
    )
  '') workerNames;

  # For `${common.collectCoverage pyNodeVars}`.
  pyNodeVars = lib.concatStringsSep ", " ([ "control" ] ++ workerNames ++ [ "client" ]);
}
// lib.optionalAttrs (defaultTenant != null) {
  # Exposed so scenarios with grpcurl-direct submits (scheduling
  # cancel-timing) can attribute the SubmitBuildRequest to the same
  # tenant the SSH path resolves.
  inherit defaultTenant;

  # Drop-in for common.sshKeySetup (mkBootstrap prefers fixture.sshKeySetup
  # when present): same keygen + authorized_keys + gateway restart, but
  # the authorized_keys entry carries the tenant name as its comment, so
  # the gateway attributes every ssh-ng session — uploads AND build
  # submissions — to defaultTenant.
  sshKeySetup = ''
    client.succeed("mkdir -p /root/.ssh && ssh-keygen -t ed25519 -N ''' -C '${defaultTenant}' -f /root/.ssh/id_ed25519")
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    control.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
    control.succeed("systemctl restart rio-gateway.service")
    control.wait_for_unit("rio-gateway.service")
    control.wait_for_open_port(2222)
  '';
}
