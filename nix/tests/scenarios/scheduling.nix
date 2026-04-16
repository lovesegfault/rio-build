# Scheduling scenario: fanout distribution, size-class routing, chunked
# PutPath, worker-disconnect reassignment, cgroup→build_history.
#
# Ports phase2a + phase2c + phase3a(cgroup) to the fixture architecture.
# Needs the standalone fixture with 3 workers (2 small, 1 large) and
# size-class + chunk-backend TOML:
#
#   fixture = standalone {
#     workers = {
#       wsmall1 = { sizeClass = "small"; };
#       wsmall2 = { sizeClass = "small"; };
#       wlarge  = { sizeClass = "large"; };
#     };
#     extraSchedulerConfig = { tickIntervalSecs = 2; extraConfig = <size-classes>; };
#     extraStoreConfig = { extraConfig = <chunk_backend filesystem>; };
#     extraPackages = [ pkgs.postgresql ];  # psql for build_history queries
#   };
#
#
# Fragment architecture: returns { fragments, mkTest }. default.nix
# composes into 2 parallel VM tests (core, disrupt). fanout → fuse-direct
# chain via FUSE cache state; all else independent.
# worker.overlay.stacked-lower — verify marker at default.nix:subtests[fanout]
# worker.ns.order — verify marker at default.nix:subtests[fanout]
#   The writableStore=false pattern in common.nix:mkWorkerNode keeps the
#   worker VM's /nix/store as a plain 9p mount (not itself an overlay),
#   so the per-build overlay's lowerdir=/nix/store:{fuse} stack is valid.
#   A build succeeding also proves mount-namespace ordering: both overlayfs
#   and nix-daemon's sandbox need unshare(CLONE_NEWNS); wrong order → fail.
#
# obs.metric.scheduler — verify marker at default.nix:subtests[load-50drv]
# obs.metric.builder — verify marker at default.nix:subtests[load-50drv]
# obs.metric.store — verify marker at default.nix:subtests[load-50drv]
#
# worker.fuse.lookup-caches — verify marker at default.nix:subtests[fanout]
#   fanout asserts rio_builder_fuse_cache_misses_total ≥1 on each small
#   worker. Nonzero misses prove lookup()→ensure_cached()→materialize
#   ran and the inode→realpath mapping is cached (ops.rs:52+).
#
# store.inline.threshold — verify marker at default.nix:subtests[chunks]
#   chunks builds a 300 KiB blob (> INLINE_THRESHOLD=256 KiB) and asserts
#   chunk_after > chunk_baseline. Proves put_path.rs:494 nar_data.len()
#   >= INLINE_THRESHOLD gate fired (tiny-text builds go inline).
#
# obs.metric.transfer-volume — verify marker at default.nix:subtests[chunks]
#   chunks asserts rio_store_put_path_bytes_total delta ≥300000 after
#   bigblob upload. Proves the volume counter (put_path.rs:574) runs on
#   the chunked path.
#   Asserted end-to-end from /metrics scrapes via assert_metric_*: exact
#   values (not grep '[1-9]') so CI logs show actual-vs-expected on failure.
#
# worker.shutdown.sigint — verify marker at default.nix:subtests[sigint-graceful]
#   sigint-graceful sends SIGINT (not SIGTERM) to rio-builder on wsmall2
#   and asserts ExecMainCode=1 + ExecMainStatus=0 → main() RETURNED
#   (stack unwound, Drop ran) rather than death-by-signal. Also guards
#   .#coverage-full: main() returning → atexit fires → profraw flushes.
#   A main.rs refactor that breaks the select! cancellation arm would
#   silently zero out worker VM coverage.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  # reassign: slow build, no pname → estimator default → "small" class.
  # 25s gives the test ~20s to find+kill the assigned worker before the
  # build would naturally finish.
  #
  # I-177: after the SIGKILL, promote_size_class_floor bumps small→large
  # and the retry lands on wlarge — which has RIO_MAX_SILENT_TIME_SECS=10
  # (for the silence subtest). mkTrivial's single-sleep would TimedOut
  # there, so this drv echoes every 5s to keep the silence watchdog fed.
  reassignDrv = drvs.mkCustom {
    name = "rio-test-sched-reassign";
    script = ''
      i=0
      while [ $i -lt 5 ]; do
        echo sched-reassign-tick-$i
        ''${busybox}/bin/busybox sleep 5
        i=$((i+1))
      done
      echo sched-reassign > $out
    '';
  };

  # cancel-timing: 60×5s echo loop (300s total) — cancelled long
  # before natural end. No pname → default "small" class → lands on
  # wsmall1 OR wsmall2. 300s >> 5s budget: if cgroup-gone passes, the
  # kill DID it (not loop end). Echoes every 5s to feed
  # RIO_MAX_SILENT_TIME_SECS=10 (set on ALL scheduling workers since
  # max-silent-time was decoupled from classify routing); a single
  # 300s silent sleep would TimedOut at ~10s and the cgroup-gone
  # assertion would be vacuous (worker reaped it, not CancelBuild).
  cancelDrv = drvs.mkCustom {
    name = "rio-test-sched-cancel-timing";
    script = ''
      i=0
      while [ $i -lt 60 ]; do
        echo sched-cancel-timing-tick-$i
        ''${busybox}/bin/busybox sleep 5
        i=$((i+1))
      done
      echo sched-cancel-timing > $out
    '';
  };

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
  # seed + build() helper + size-class precondition asserts.
  prelude = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    all_workers = [wsmall1, wsmall2, wlarge]
    small_workers = [wsmall1, wsmall2]

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}] + all_workers)";
    }}

    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via plaintext gRPC direct to :9001. Returns buildId.
        Standalone fixture variant — no port-forward. Same
        `|| true` swallow-DeadlineExceeded as the k3s variant."""
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
      reassignDrv
      cancelDrv
      silenceDrv
      cgroupDrv
      ;
  };
  fragments = builtins.mapAttrs (_: f: f scope) (common.importDir ./scheduling);

  mkTest = common.mkFragmentTest {
    scenario = "scheduling";
    inherit prelude fragments fixture;
    defaultTimeout = 600;
    # fanout populates the FUSE cache that fuse-direct reads.
    chains = [
      {
        before = "fanout";
        after = "fuse-direct";
        msg = "fuse-direct requires fanout earlier (FUSE cache state)";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
