# Toxiproxy chaos fixture: standalone topology + fault-injection proxy.
#
# Wraps the standalone fixture (PG + store + scheduler + gateway on one
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
#   │  rio-builder          │─────────────┘
#   │  storeAddr=control:29002                   ┌── client ──┐
#   │  schedulerAddr=control:9001  (unproxied)   │  nix       │
#   └──────────────────────┘                     └────────────┘
#
# WHY on control and not a separate toxiproxy VM:
#
#   The scheduler uses connect_store_lazy (main.rs:51) — store_client is
#   only None on a malformed addr/TLS config, never on a TCP race. So
#   systemd ordering is no longer load-bearing for store_client itself.
#   toxiproxy stays on control for the WORKER-side proxy (worker_store →
#   control:29002 must be listening before the worker connects) and for
#   admin-API locality (toxiproxy-cli on control via a single .succeed).
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
# Built ON TOP of fixtures/standalone.nix (since P0560): the chaos
# subtests drive real builds, so the fixture-level tenancy stopgap
# (defaultTenant — HMAC keys, tenant seed, key-comment attribution,
# narinfo trigger) must apply here too. Wrapping standalone instead of
# re-implementing the control/worker/client nodes keeps that machinery
# in one place for P0593 to delete.
#
# Returns the same shape as standalone.nix:
#   { nodes, waitReady, pyNodeVars, gatewayHost, sshKeySetup? }
# plus `toxiproxyHost` (= "control") for scenarios that want to be
# explicit about where toxiproxy-cli runs.
{
  pkgs,
  rio-workspace,
  rioModules,
  coverage ? false,
  ...
}:
let
  inherit (pkgs) lib;
  standalone = import ./standalone.nix {
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
{
  # P0560 fixture-tenancy stopgap passthrough (see standalone.nix
  # `defaultTenant`): the chaos subtests submit real builds, whose
  # inputs only materialize through the tenant-scoped castore reads.
  # default.nix sets "vmtest", same as the rest of the build matrix.
  defaultTenant ? null,
}:
let
  base = standalone {
    inherit defaultTenant;
    # Scheduler reaches the store through the scheduler_store proxy.
    # standalone passes this through to mkControlNode, where `//`
    # makes it win over the `storeAddr = "localhost:9002"` default.
    extraSchedulerConfig = {
      storeAddr = "localhost:19002";
    };
  };
in
{
  gatewayHost = "control";
  toxiproxyHost = "control";

  nodes = {
    control = {
      imports = [
        base.nodes.control
        toxiproxyModule
      ];
      # 29002: worker_store proxy listener (cross-VM).
      # 9093: worker metrics port (scraped in subtest 2/4 assertions).
      networking.firewall.allowedTCPPorts = [
        29002
        9093
      ];
      # Belt-and-suspenders: scheduler unit waits for toxiproxy unit.
      # The Before= on toxiproxy already implies this, but explicit
      # After= on the scheduler side survives if someone later drops
      # the Before= (which would otherwise silently reintroduce the race).
      systemd.services.rio-scheduler.after = [ "toxiproxy.service" ];
    };

    worker = {
      imports = [ base.nodes.worker ];
      # mkWorkerNode hardcodes storeAddr = "control:9002" with no override
      # hook. Layer a module-merge override instead of patching common.nix.
      # mkForce because the module's own value is also a plain string —
      # two plain strings at the same option = conflict.
      services.rio.worker.storeAddr = lib.mkForce "control:29002";
    };

    inherit (base.nodes) client;
  };

  # standalone's waitReady (control plane + seed-tenant + worker
  # registered), then the proxy checks. The systemd After=/Before=
  # chain (store → toxiproxy → scheduler) provides the boot-order
  # guarantee; these asserts prove both proxies are listening before
  # any subtest injects a toxic.
  # Note the ordering inversion vs the pre-P0560 self-contained fixture:
  # the proxy-port checks now run AFTER worker registration (which
  # already flowed through worker_store), so they are confirmation of
  # the systemd ordering, not the gate that establishes it.
  waitReady = base.waitReady + ''
    control.wait_for_unit("toxiproxy.service")
    control.wait_for_open_port(8474)   # admin API
    control.wait_for_open_port(19002)  # scheduler_store proxy
    control.wait_for_open_port(29002)  # worker_store proxy
  '';

  inherit (base) pyNodeVars;
}
# sshKeySetup only exists on the base when defaultTenant is set (the
# tenant-comment variant); pass it through so mkBootstrap picks it up.
// lib.optionalAttrs (base ? sshKeySetup) { inherit (base) sshKeySetup; }
