# Scheduling scenario: fanout distribution, chunked PutPath,
# cgroup-tracking, load.
#
# Ports phase2a + phase2c + phase3a(cgroup) to the fixture architecture.
# Needs the standalone fixture with 3 workers (the filesystem chunk
# backend is the mkControlNode default, so no extraStoreConfig needed):
#
#   fixture = standalone {
#     workers = {
#       worker1 = { };
#       worker2 = { };
#       worker3  = { };
#     };
#     extraSchedulerConfig = { tickIntervalSecs = 2; };
#     extraPackages = [ pkgs.postgresql ];  # psql for build_samples queries
#   };
#
#
# Fragment architecture: returns { fragments, mkTest }. default.nix
# composes into 2 parallel VM tests (core, disrupt); all fragments are
# independent (no cross-fragment cache-state chains).
# worker.overlay.stacked-lower — verify marker at default.nix:subtests[fanout]
# worker.ns.order — verify marker at default.nix:subtests[fanout]
#   The writableStore=false pattern in common.nix:mkWorkerNode keeps the
#   worker VM's /nix/store as a plain 9p mount (not itself an overlay),
#   so the per-build overlay's castore-FUSE lower stack is valid.
#   A build succeeding also proves mount-namespace ordering: both overlayfs
#   and nix-daemon's sandbox need unshare(CLONE_NEWNS); wrong order → fail.
#
# obs.metric.scheduler — verify marker at default.nix:subtests[load-50drv]
# obs.metric.builder — verify marker at default.nix:subtests[load-50drv]
# obs.metric.store — verify marker at default.nix:subtests[load-50drv]
#
# obs.metric.transfer-volume — verify marker at default.nix:subtests[chunks]
#   chunks asserts rio_store_put_path_bytes_total delta ≥300000 after
#   bigblob upload. Proves the volume counter (put_path.rs:574) runs on
#   the chunked path. The 300 KiB blob is also above INLINE_THRESHOLD
#   (256 KiB), so the assertion exercises the chunked (not inline)
#   upload path; store.inline.threshold itself is verified by the
#   rio-store unit test (tests/grpc/chunked.rs).
#   Asserted end-to-end from /metrics scrapes via assert_metric_*: exact
#   values (not grep '[1-9]') so CI logs show actual-vs-expected on failure.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  # max-silent-time: echoes ONCE then sleeps 60s. ALL scheduling
  # workers have RIO_MAX_SILENT_TIME_SECS=10 (default.nix fixture). The
  # worker's silence select! arm fires ~10s after the echo → TimedOut →
  # cgroup.kill reaps the sleep. 60s sleep proves the kill was at ~10s
  # SILENCE, not 60s wall-clock. mkTrivial echoes AFTER sleep, so
  # inline a custom drv with echo-then-sleep ordering.
  silenceDrv = drvs.mkCustom {
    name = "rio-sched-silence";
    extraAttrs.pname = "rio-sched-silence";
    script = ''
      echo start-silence-marker
      ''${busybox}/bin/busybox sleep 60
      echo unreachable > $out
    '';
  };

  # cgroup: needs pname in env (completion.rs:181 guards on state.pname;
  # gateway extracts from drv.env().get("pname")) AND sleep ≥2s (so the
  # 1Hz CPU poll in executor/mod.rs fires at least once). mkTrivial
  # doesn't set pname, so inline a custom drv.
  cgroupDrv = drvs.mkCustom {
    name = "rio-sched-cgroup";
    extraAttrs.pname = "rio-sched-cgroup";
    script = ''
      ''${busybox}/bin/busybox sleep 3
      echo cgroup > $out
    '';
  };

  # ── testScript prelude: bootstrap + Python helpers ────────────────────
  # Shared by all fragment compositions. start_all + waitReady + SSH +
  # seed + build() helper + resource-floor precondition asserts.
  prelude = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    all_workers = [worker1, worker2, worker3]

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}] + all_workers)";
    }}

    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via plaintext gRPC direct to :9001. Returns buildId.
        Standalone fixture variant — no port-forward. Same
        `|| true` swallow-DeadlineExceeded as the k3s variant."""
        # P0560 fixture-tenancy stopgap (P0593 deletes): grpcurl-direct
        # submits bypass the gateway's key-comment attribution, so
        # without an explicit tenant_name the assignment token carries
        # no tenant and the builder's tenant-scoped castore reads fail
        # closed. Attribute to the fixture's defaultTenant.
        payload.setdefault("tenantName", "${fixture.defaultTenant or ""}")
        out = ${gatewayHost}.succeed(
            f"grpcurl -plaintext -max-time {max_time} "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:9001 rio.scheduler.SchedulerService/SubmitBuild "
            f"2>&1 || true"
        )
        return _parse_submit_build_id(out)

    ${common.mkSubmitHelpers gatewayHost}
  '';

  # ── Subtest fragments ─────────────────────────────────────────────────
  # One file per subtest under scenarios/scheduling/. `scope` is the
  # closure each fragment sees via `with scope;`.
  scope = {
    inherit
      pkgs
      common
      drvs
      gatewayHost
      protoset
      silenceDrv
      cgroupDrv
      ;
  };
  fragments = builtins.mapAttrs (_: f: f scope) (common.importDir ./scheduling);

  mkTest = common.mkFragmentTest {
    scenario = "scheduling";
    inherit prelude fragments fixture;
    defaultTimeout = 600;
  };
in
{
  inherit fragments mkTest;
}
