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
  # this to pass distinct sizeClass per worker:
  #   workers = {
  #     wsmall1 = { sizeClass = "small"; };
  #     wlarge  = { sizeClass = "large"; };
  #   };
  # For the common 1-worker case: workers = { worker = { }; }
  workers,

  # HMAC keys for assignment+service tokens (lib/hmac-keys.nix).
  withHmac ? false,

  # opentelemetry-collector on control (OTLP gRPC :4317, file exporter
  # to /var/lib/otelcol/traces.json). Sets RIO_OTEL_ENDPOINT on all
  # services. For scenarios/observability.nix.
  withOtel ? false,

  # Pass-through to mkControlNode (size-class TOML, tick interval, etc).
  extraSchedulerConfig ? { },
  extraStoreConfig ? { },
  extraPackages ? [ ],
  # Scheduler-only systemd env (figment + fixture toggles like
  # RIO_ADMIN_TEST_FIXTURES). Merged on top of extraServiceEnv.
  extraSchedulerEnv ? { },
  # Gateway-only env (figment RIO_FOO__BAR=... style). substitute.nix
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
  hmacKeys = if withHmac then mkHmacKeys { } else null;

  # ── HMAC env (no-op {} when withHmac=false) ──────────────────────────
  # Scheduler+store share the assignment-token key; gateway+store share
  # the service-token key. Workers get neither (they receive assignment
  # tokens from the scheduler at dispatch, not from a key file).
  controlHmacEnv = lib.optionalAttrs withHmac {
    RIO_HMAC_KEY_PATH = "${hmacKeys}/hmac.key";
    RIO_SERVICE_HMAC_KEY_PATH = "${hmacKeys}/service-hmac.key";
  };

  gatewayHmacEnv = lib.optionalAttrs withHmac {
    RIO_SERVICE_HMAC_KEY_PATH = "${hmacKeys}/service-hmac.key";
  };

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
        (lib.optionalAttrs withHmac (lib.mapAttrs (_: lib.mkForce) gatewayHmacEnv)) // extraGatewayEnv;

      # Serialize migration runs — migration 011's CREATE INDEX
      # CONCURRENTLY deadlocks with sqlx's pg_advisory_lock when store
      # and scheduler race on a fresh DB. The module-level
      # After=rio-store.service (scheduler.nix) only orders the fork
      # (Type=simple), not readiness — both still hit migrate!() near-
      # simultaneously. Block scheduler until store's gRPC port is open,
      # which happens post-migration. k8s deployments dodge this via
      # pod startup jitter; standalone VM boot is deterministic enough
      # to trigger the race reliably. Restart=always (module-level)
      # covers any residual window.
      rio-scheduler.preStart = ''
        for _ in $(seq 1 60); do
          ${pkgs.netcat}/bin/nc -z localhost 9002 && exit 0
          sleep 0.5
        done
        echo "rio-store port 9002 not open after 30s" >&2
        exit 1
      '';

      # OTel ordering: rio-* services on control must start AFTER
      # otelcol. Without this, the services boot, try to connect to
      # each other during boot churn, and the restart dance adds ~10s
      # of flake. After= doesn't block startup if otelcol is disabled
      # (unit doesn't exist → no-op), so the mkIf guard is belt-and-
      # suspenders.
      rio-store.after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      rio-scheduler.after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
      rio-gateway.after = lib.mkIf withOtel [ "opentelemetry-collector.service" ];
    };
  };

  # ── Worker nodes ────────────────────────────────────────────────────
  # mapAttrs' renames to the worker's hostName while passing through
  # the scenario's per-worker args + fixture-level OTel.
  workerNodes = lib.mapAttrs (
    name: args:
    common.mkWorkerNode (
      args
      // {
        hostName = name;
        otelEndpoint = workerOtelEndpoint;
      }
    )
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
  + lib.optionalString withOtel ''
    control.wait_for_unit("opentelemetry-collector.service")
    control.wait_for_open_port(4317)
  ''
  + lib.concatMapStrings (w: ''
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
