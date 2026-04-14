# SLA-sizing scenario: explore-ladder convergence + MAD outlier reject.
#
# Uses the standalone fixture with one worker carrying RIO_BUILDER_SCRIPT
# (rio-builder feature `test-fixtures`) so each "build" reports scripted
# (wall, peak_mem, peak_cpu, cpu_seconds) keyed on (pname, cpu_limit) —
# the explore ladder is driven deterministically without nix-daemon or
# wall-clock waits. The scheduler runs with [sla] configured (probe.cpu=4,
# max_cores=64, p90=1200) and RIO_ADMIN_TEST_FIXTURES=1 so the test can
# InjectBuildSample for the outlier check.
#
# Subtest contract (markers live at default.nix:subtests, NOT here):
#
# convergence — submits synth-amdahl 3× with cpu_limit forced to 4/16/64
#   via the worker's effective_cores. After the 3rd, asserts
#   build_samples has n≥3 distinct cpu_limit_cores and span≥4 (the
#   freeze gate). Covers sched.sla.explore-x4-first-bump and -freeze.
#
# outlier — InjectBuildSample(synth-amdahl, wall=10000, c=16) then waits
#   one estimator tick and asserts outlier_excluded=TRUE on that row in
#   PG. Covers sched.sla.outlier-mad-reject.
#
# CAVEAT: standalone-fixture workers read effective_cores from the host
# cgroup, NOT from a per-dispatch SpawnIntent (no controller). The
# convergence subtest therefore asserts the SAMPLES landed (proving the
# fixture intercept + completion → build_samples wiring), and asserts
# the SlaEstimator refit produced span≥4 by querying PG; it does NOT
# assert that the 4th dispatch's SpawnIntent matched solve_mvp — that
# requires the k3s fixture (controller spawns per-intent pods). Tracked
# as a follow-up once a k3s sla-sizing variant lands.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  # Scripted-telemetry drv. pname must match an [[entry]] in
  # sla-builder-script.toml; the body is irrelevant (fixture intercept
  # fires before nix-daemon spawn) but echoes a marker so a fixture
  # MISS (e.g. typo'd pname) is visible as a real build in the log.
  amdahlDrv = drvs.mkCustom {
    name = "synth-amdahl-0.1";
    extraAttrs.pname = "synth-amdahl";
    script = ''
      echo synth-amdahl-fixture-miss-ran-real-build > $out
    '';
  };

  prelude = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    all_workers = [worker]

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}] + all_workers)";
    }}

    def grpcurl_admin(method: str, payload: dict) -> str:
        return ${gatewayHost}.succeed(
            f"grpcurl -plaintext -protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:9001 rio.admin.AdminService/{method}"
        )

  '';

  fragments = {
    convergence = ''
      with subtest("convergence: scripted samples land in build_samples"):
          # Three submits; fixture short-circuits each, scheduler writes
          # build_samples on completion. tickIntervalSecs=2 → estimator
          # refit within ~2s of each.
          for i in range(3):
              build("${amdahlDrv}", marker="synth-amdahl", timeout=60)
          ${gatewayHost}.wait_until_succeeds(
              "test $(sudo -u postgres psql -d rio -tAc "
              "\"SELECT COUNT(*) FROM build_samples WHERE pname='synth-amdahl'\") -ge 3",
              timeout=30,
          )
          n = int(psql(${gatewayHost},
              "SELECT COUNT(DISTINCT cpu_limit_cores) FROM build_samples "
              "WHERE pname='synth-amdahl' AND NOT outlier_excluded"
          ))
          assert n >= 1, f"distinct cpu_limit_cores={n}; fixture intercept did not fire?"
          # cpu_seconds_total / duration_secs / cpu_limit > 0.4 →
          # saturated bit set on the ExploreState.
          util = float(psql(${gatewayHost},
              "SELECT cpu_seconds_total/duration_secs/cpu_limit_cores "
              "FROM build_samples WHERE pname='synth-amdahl' "
              "ORDER BY completed_at LIMIT 1"
          ))
          assert util > 0.4, f"util={util}: saturated gate would not fire"
    '';

    outlier = ''
      with subtest("outlier: 100x sample flagged outlier_excluded"):
          # Seed enough on-curve samples that n_eff>=5 (MAD gate). The
          # fixture builds above gave us 3; inject 3 more on-curve via
          # InjectBuildSample so the gate opens, then one 100x outlier.
          for c, t in [(4, 530), (16, 155), (64, 61.25)]:
              grpcurl_admin("InjectBuildSample", {
                  "pname": "synth-amdahl", "system": "x86_64-linux",
                  "tenant": "", "durationSecs": t,
                  "peakMemoryBytes": 1073741824,
                  "cpuLimitCores": c, "cpuSecondsTotal": t * c * 0.95,
              })
          # Wait one estimator tick so refit runs and caches the fit.
          import time; time.sleep(3)
          grpcurl_admin("InjectBuildSample", {
              "pname": "synth-amdahl", "system": "x86_64-linux",
              "tenant": "", "durationSecs": 53000,
              "peakMemoryBytes": 1073741824,
              "cpuLimitCores": 4, "cpuSecondsTotal": 200000,
          })
          # Next tick: is_outlier scores 53000 against prev fit → flag.
          ${gatewayHost}.wait_until_succeeds(
              "test $(sudo -u postgres psql -d rio -tAc "
              "\"SELECT COUNT(*) FROM build_samples "
              "WHERE pname='synth-amdahl' AND outlier_excluded\") -ge 1",
              timeout=15,
          )
          # Metric incremented (registered + emitted).
          assert_metric_ge(${gatewayHost}, 9091,
              "rio_scheduler_sla_outlier_rejected_total", 1.0)
    '';
  };

  mkTest = common.mkFragmentTest {
    scenario = "sla-sizing";
    inherit prelude fragments fixture;
    defaultTimeout = 300;
    chains = [
      {
        before = "convergence";
        after = "outlier";
        msg = "outlier needs convergence's samples for n_eff>=5";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
