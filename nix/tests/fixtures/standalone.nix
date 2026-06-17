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

  # Static RIO_EXECUTOR_TOKEN for standalone workers. In k8s the
  # scheduler signs ExecutorClaims{intent_id,kind,expiry} per
  # SpawnIntent and the controller injects it as a pod env var;
  # standalone has no SpawnIntent flow, so mint one here with
  # intent_id="" (matches the worker's empty Config.intent_id →
  # heartbeat body check passes), the worker's kind (proto wire i32:
  # 0=Builder, 1=Fetcher — require_executor() rejects a heartbeat
  # whose body kind differs from the token's, so a fetcher worker
  # needs a Fetcher-kind token), and a far-future expiry. Signed with
  # the SAME hmac.key the scheduler loads so require_executor()
  # verifies. Written as a systemd EnvironmentFile (KEY=value) — NOT
  # readFile-into-env: eval-time readFile of a derivation output is
  # an IFD anti-pattern even now that the hmacKeys derivation is
  # deterministic. r[sec.executor.identity-token].
  executorTokenEnvFor =
    kind:
    pkgs.runCommand "rio-executor-token-env"
      {
        nativeBuildInputs = [ pkgs.python3 ];
      }
      ''
        python3 - ${hmacKeys}/hmac.key ${toString kind} > $out <<'EOF'
        import base64, hashlib, hmac, json, sys
        key = open(sys.argv[1], "rb").read()
        # Mirror rio-auth load_key() trailing-newline trim.
        for suf in (b"\r\n", b"\n"):
            if key.endswith(suf):
                key = key[: -len(suf)]
                break
        claims = json.dumps(
            {"intent_id": "", "kind": int(sys.argv[2]), "expiry_unix": 9999999999},
            separators=(",", ":"),
        ).encode()
        sig = hmac.new(key, claims, hashlib.sha256).digest()
        b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
        print(f"RIO_EXECUTOR_TOKEN={b64(claims)}.{b64(sig)}")
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
    systemd.services = {
      # Gateway-only HMAC env override. mkControlNode's extraServiceEnv
      # applies controlHmacEnv to ALL three services (including gateway).
      # NixOS module merge of two string values for the same key →
      # conflict. mapAttrs mkForce makes the gateway env win
      # unambiguously. extraGatewayEnv merges alongside (no mkForce —
      # it's gateway-only, no conflict with extraServiceEnv's shared
      # keys).
      rio-gateway.environment =
        (lib.optionalAttrs hmacEnabled (lib.mapAttrs (_: lib.mkForce) gatewayHmacEnv)) // extraGatewayEnv;

      # OTel ordering: rio-* services on control must start AFTER
      # otelcol. Without this, the services boot, try to connect to
      # each other during boot churn, and the restart dance adds ~10s
      # of flake. After= doesn't block startup if otelcol is disabled
      # (unit doesn't exist → no-op), so the mkIf guard is belt-and-
      # suspenders.
      rio-store.after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      rio-scheduler.after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      rio-gateway.after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];

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
  # on, also mount the static executor-token EnvironmentFile so
  # rio-builder presents x-rio-executor-token (otherwise the
  # scheduler's require_executor() rejects the BuildExecution stream
  # and every heartbeat with Unauthenticated). The token's kind claim
  # follows the worker's RIO_EXECUTOR_KIND (fetcher workers heartbeat
  # kind=1; a Builder-kind token would be rejected as a kind mismatch).
  workerNodes = lib.mapAttrs (
    name: args:
    let
      workerExecKind = (args.extraServiceEnv or { }).RIO_EXECUTOR_KIND or "builder";
      executorKind = if workerExecKind == "fetcher" then 1 else 0;
    in
    {
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
        "${executorTokenEnvFor executorKind}"
      ];
    }
  ) workers;

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
  + lib.concatMapStrings (w: ''
    ${w}.wait_for_unit("rio-mountd.service")
    ${w}.wait_for_unit("rio-builder.service")
  '') workerNames
  # All workers registered at scheduler. Exact count, not `[1-9]`.
  # Handles the stream-then-heartbeat gauge race (58c0145) by waiting
  # instead of asserting immediately — but the WAIT uses an exact
  # match, so if the gauge is still wrong, this times out loudly.
  + ''
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -x 'rio_scheduler_workers_active ${toString (builtins.length workerNames)}'",
        timeout=30,
    )
  '';

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
