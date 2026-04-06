# Toxiproxy chaos fixture: standalone topology + fault-injection proxy.
#
# Wraps the standalone pattern (PG + store + scheduler + gateway on one
# control VM, 1 worker VM, 1 client VM) with a toxiproxy-server systemd
# unit ON THE CONTROL VM sitting between scheduler↔store and worker↔store.
#
# Topology (all on the NixOS-test vlan; hostnames auto-resolve):
#
#   ┌──────────────────────── control ────────────────────────┐
#   │  PG  rio-store:9002                                     │
#   │         ▲      ▲                                        │
#   │         │      └──── toxiproxy ────┐                    │
#   │         │       scheduler_store    │ worker_store       │
#   │         │       127.0.0.1:19002    │ 0.0.0.0:29002      │
#   │         │              ▲           │                    │
#   │  rio-scheduler ────────┘           │   rio-gateway:2222 │
#   │  (storeAddr=localhost:19002)       │                    │
#   └────────────────────────────────────┼────────────────────┘
#                                        │
#   ┌─────── worker ───────┐             │
#   │  rio-worker          │─────────────┘
#   │  storeAddr=control:29002                   ┌── client ──┐
#   │  schedulerAddr=control:9001  (unproxied)   │  nix       │
#   └──────────────────────┘                     └────────────┘
#
# WHY on control and not a separate toxiproxy VM:
#
#   connect_store() at scheduler startup (main.rs:225) is a real TCP
#   connect — not lazy. If the proxy isn't listening yet, the connect
#   fails → store_client = None → the cache-check breaker (merge.rs:519)
#   is never exercised (the let-else early-returns empty). A separate VM
#   means a cross-VM boot race that no amount of wait_for_* fixes cleanly
#   (scheduler has already started and cached store_client = None by the
#   time the testScript runs).
#
#   On control, systemd After=/Before= gives deterministic ordering:
#   store → toxiproxy → scheduler. The proxy is transparent at boot (no
#   toxics configured), so scheduler sees a working store. Tests then
#   add toxics mid-run via `control.succeed("toxiproxy-cli toxic add ...")`.
#
# WHY not also proxy worker↔scheduler:
#
#   The scheduler-worker relationship is a long-lived bidi stream
#   (BuildExecution). Injecting toxics mid-stream tests a different
#   failure domain (stream-close → reconnect loop, main.rs:413-425) that
#   lifecycle.nix already covers via controller-restart. The chaos
#   scenarios here target the unary-RPC retry/timeout/breaker paths.
#
# Returns the same shape as standalone.nix:
#   { nodes, waitReady, pyNodeVars, gatewayHost, pki }
# plus `toxiproxyHost` (= "control") for scenarios that want to be
# explicit about where toxiproxy-cli runs.
{
  pkgs,
  rio-workspace,
  rioModules,
  coverage ? false,
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

  # Proxy definitions. toxiproxy-server -config reads this JSON on boot
  # and creates both proxies with zero toxics → transparent pass-through.
  #
  # scheduler_store listens on 127.0.0.1 only: scheduler is on the same
  # VM, no need to expose to the vlan. worker_store listens on 0.0.0.0:
  # worker is a separate VM connecting over the test network.
  proxyConfig = pkgs.writeText "toxiproxy.json" (
    builtins.toJSON [
      {
        name = "scheduler_store";
        listen = "127.0.0.1:19002";
        upstream = "127.0.0.1:9002";
        enabled = true;
      }
      {
        name = "worker_store";
        listen = "0.0.0.0:29002";
        upstream = "127.0.0.1:9002";
        enabled = true;
      }
    ]
  );

  # Toxiproxy as a NixOS module. Merged into control via imports.
  #
  # Ordering: After=rio-store → proxy can't start until the upstream
  # exists (toxiproxy doesn't retry upstream connect; it lazy-connects
  # on first client connection, but having store up first means the
  # wait_for_open_port checks below are sequentially meaningful).
  # Before=rio-scheduler → scheduler's connect_store() finds the proxy
  # listening. Without this, scheduler boots with store_client = None
  # and the breaker path (merge.rs:551-586) is dead code in this fixture.
  toxiproxyModule = {
    systemd.services.toxiproxy = {
      description = "Toxiproxy chaos-injection server";
      wantedBy = [ "multi-user.target" ];
      after = [ "rio-store.service" ];
      before = [ "rio-scheduler.service" ];
      serviceConfig = {
        ExecStart = "${pkgs.toxiproxy}/bin/toxiproxy-server -config ${proxyConfig}";
        Restart = "on-failure";
        RestartSec = "2s";
      };
    };
    # toxiproxy-cli for testScript `control.succeed("toxiproxy-cli ...")`.
    # Admin API defaults to 127.0.0.1:8474; cli defaults match (no -h needed).
    environment.systemPackages = [ pkgs.toxiproxy ];
  };
in
# No per-test knobs for now. `_:` keeps the call shape consistent
# with standalone (callsite: `toxiproxy { }`). Not `{ }:` — statix
# W10 flags empty destructuring patterns.
_: {
  # PKI not wired here — chaos scenarios test transport-layer faults,
  # not authz. null matches standalone's `withPki = false` shape.
  pki = null;

  gatewayHost = "control";
  toxiproxyHost = "control";

  nodes = {
    control = {
      imports = [
        (common.mkControlNode {
          hostName = "control";
          # Override scheduler's storeAddr through the proxy. `//` in
          # mkControlNode (common.nix:327) makes this win over the
          # `storeAddr = "localhost:9002"` default.
          extraSchedulerConfig = {
            storeAddr = "localhost:19002";
          };
          # 29002: worker_store proxy listener (cross-VM).
          # 9093: worker metrics port (scraped in subtest 2/4 assertions).
          extraFirewallPorts = [
            29002
            9093
          ];
        })
        toxiproxyModule
      ];
      # Belt-and-suspenders: scheduler unit waits for toxiproxy unit.
      # The Before= on toxiproxy already implies this, but explicit
      # After= on the scheduler side survives if someone later drops
      # the Before= (which would otherwise silently reintroduce the race).
      systemd.services.rio-scheduler.after = [ "toxiproxy.service" ];
    };

    worker = {
      imports = [
        (common.mkWorkerNode {
          hostName = "worker";
          maxBuilds = 1;
        })
      ];
      # mkWorkerNode hardcodes storeAddr = "control:9002" with no override
      # hook. Layer a module-merge override instead of patching common.nix.
      # mkForce because the module's own value is also a plain string
      # (common.nix:402) — two plain strings at the same option = conflict.
      services.rio.worker.storeAddr = lib.mkForce "control:29002";
    };

    client = common.mkClientNode { gatewayHost = "control"; };
  };

  # Boot sequence mirrors standalone.waitReady, plus the proxy checks
  # slotted between store-ready and scheduler-ready (same order as the
  # systemd After=/Before= chain). Verifying 19002/29002 open BEFORE
  # asserting scheduler-ready proves the ordering held: if scheduler
  # somehow started before toxiproxy, it would have logged "scheduler-
  # side cache check disabled" and the chaos scenarios' breaker
  # assertions would fail confusingly later. Fail here instead.
  waitReady = ''
    control.wait_for_unit("postgresql.service")
    control.wait_for_unit("rio-store.service")
    control.wait_for_open_port(9002)
    control.wait_for_unit("toxiproxy.service")
    control.wait_for_open_port(8474)   # admin API
    control.wait_for_open_port(19002)  # scheduler_store proxy
    control.wait_for_open_port(29002)  # worker_store proxy
    control.wait_for_unit("rio-scheduler.service")
    control.wait_for_open_port(9001)
    # Hard check: scheduler's store_client is Some, not None. If
    # systemd ordering broke and scheduler raced ahead of toxiproxy,
    # connect_store() failed and logged the warning at main.rs:234.
    # Absence of that log line = connect succeeded = breaker is live.
    control.fail(
        "journalctl -u rio-scheduler --no-pager | "
        "grep -q 'scheduler-side cache check disabled'"
    )
    worker.wait_for_unit("rio-worker.service")
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -x 'rio_scheduler_workers_active 1'",
        timeout=30,
    )
  '';

  pyNodeVars = "control, worker, client";
}
