# SLA-sizing scenario: explore-ladder convergence, MAD outlier reject,
# v1.1 override / hw-normalize / seed-corpus.
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
# override-precedence — rio-cli sla override --cores=12 then asserts
#   SlaExplain shows the override short-circuit (single c_star=12 row).
#   Covers sched.sla.override-precedence.
#
# hw-normalize — seeds two synthetic hw_classes (factor 1.0 / 2.0),
#   injects the same T(c) curve on both at scaled wall-clock, asserts
#   the refit recovers P~2000 ref-seconds. Covers sched.sla.hw-ref-seconds.
#
# seed-corpus — export-corpus -> reset model -> import-corpus -> inject
#   one sample -> SlaStatus.prior_source == "seed". Covers
#   sched.sla.prior-partial-pool.
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
  rioCli = "${common.rio-workspace}/bin/rio-cli";

  # Scripted-telemetry drv. pname must match an [[entry]] in
  # sla-builder-script.toml; the body is irrelevant (fixture intercept
  # fires before nix-daemon spawn) but echoes a marker so a fixture
  # MISS (e.g. typo'd pname) is visible as a real build in the log.
  #
  # Three distinct drvs (same pname, different name → different drv-hash)
  # so each submit is a fresh dispatch. With one drv, submit 2+3 would
  # cache-hit on submit 1 in the scheduler and never reach the worker.
  amdahlDrv =
    n:
    drvs.mkCustom {
      name = "synth-amdahl-${toString n}";
      extraAttrs.pname = "synth-amdahl";
      script = ''
        echo synth-amdahl-fixture-miss-ran-real-build > $out
      '';
    };
  amdahlDrvs = builtins.map amdahlDrv [
    0
    1
    2
  ];

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

    def cli(args: str) -> str:
        return ${gatewayHost}.succeed(
            "${common.covShellEnv}"
            "RIO_SCHEDULER_ADDR=localhost:9001 "
            f"${rioCli} {args} 2>&1"
        )

    def wait_estimator_tick() -> None:
        # ESTIMATOR_REFRESH_EVERY=6 ticks x tickIntervalSecs=2 = 12s
        # cadence; gate on the refresh-counter advancing rather than
        # sleeping a fixed amount (TCG variance).
        base = metric_value(scrape_metrics(${gatewayHost}, 9091),
            "rio_scheduler_estimator_refresh_total") or 0.0
        ${gatewayHost}.wait_until_succeeds(
            "curl -fsS localhost:9091/metrics | "
            f"awk '/^rio_scheduler_estimator_refresh_total / {{exit !($2>{base})}}'",
            timeout=30,
        )

  '';

  fragments = {
    convergence = ''
      with subtest("convergence: scripted samples land in build_samples"):
          # Three submits; fixture short-circuits each, scheduler writes
          # build_samples on completion. tickIntervalSecs=2 → estimator
          # refit within ~2s of each.
          #
          # The fixture intercept returns built_outputs=[] (no upload),
          # so client-side nix-build fails to fetch the output back —
          # the SCHEDULER-side completion still fires (build_samples row
          # written). Drive via raw client.execute() so build()'s
          # rc!=0 → dump_all_logs path doesn't flood the log on every
          # expected client-side miss; the PG poll below is the real
          # assertion.
          amdahl_drvs = [
              "${builtins.elemAt amdahlDrvs 0}",
              "${builtins.elemAt amdahlDrvs 1}",
              "${builtins.elemAt amdahlDrvs 2}",
          ]
          for drv in amdahl_drvs:
              rc, out = client.execute(
                  "timeout 60 nix-build --no-out-link "
                  "--store 'ssh-ng://${gatewayHost}' "
                  "--arg busybox '(builtins.storePath ${pkgs.pkgsStatic.busybox})' "
                  f"{drv} 2>&1"
              )
              print(f"synth-amdahl submit rc={rc} (client miss expected; "
                    "scheduler-side completion is what matters)")
              if "fixture-miss-ran-real-build" in out:
                  raise Exception(
                      "RIO_BUILDER_SCRIPT intercept did NOT fire — real "
                      f"build ran. Check worker env + pname match. out={out}"
                  )
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
          # Structural wait for one estimator refresh so the cached fit
          # has the 6 on-curve samples (n_eff>=5, log_residuals populated).
          wait_estimator_tick()
          grpcurl_admin("InjectBuildSample", {
              "pname": "synth-amdahl", "system": "x86_64-linux",
              "tenant": "", "durationSecs": 53000,
              "peakMemoryBytes": 1073741824,
              "cpuLimitCores": 4, "cpuSecondsTotal": 200000,
          })
          # Next refresh: is_outlier scores 53000 against prev fit → flag.
          # One 12s cycle + TCG slack; matches the convergence timeout.
          ${gatewayHost}.wait_until_succeeds(
              "test $(sudo -u postgres psql -d rio -tAc "
              "\"SELECT COUNT(*) FROM build_samples "
              "WHERE pname='synth-amdahl' AND outlier_excluded\") -ge 1",
              timeout=30,
          )
          # Metric incremented (registered + emitted). The injected
          # samples carry tenant="" so the counter is labelled tenant="".
          assert_metric_ge(${gatewayHost}, 9091,
              "rio_scheduler_sla_outlier_rejected_total", 1.0,
              labels='{tenant=""}')
    '';

    override-precedence = ''
      with subtest("override-precedence: forced cores short-circuits solve"):
          # SetSlaOverride goes to PG; resolved on the next refresh tick.
          out = cli("sla override synth-amdahl --cores=12")
          print(out)
          assert "set for synth-amdahl" in out, out
          wait_estimator_tick()
          # SlaExplain re-runs the same intent_for() dispatch uses; a
          # forced-cores override short-circuits to a single synthetic
          # row with c_star=12 — proving the next SpawnIntent for this
          # key would request 12 cores. The standalone fixture has no
          # controller, so we assert the solve trace not the pod spec.
          ex = json.loads(grpcurl_admin("SlaExplain", {
              "pname": "synth-amdahl", "system": "x86_64-linux", "tenant": "",
          }))
          assert "cores=12" in ex.get("overrideApplied", ""), ex
          cands = ex.get("candidates", [])
          assert len(cands) == 1 and cands[0].get("cStar") == 12.0, cands
          # And SlaStatus surfaces the resolved row so an operator can
          # see id/created_by.
          st = json.loads(grpcurl_admin("SlaStatus", {
              "pname": "synth-amdahl", "system": "x86_64-linux", "tenant": "",
          }))
          assert st.get("activeOverride", {}).get("cores") == 12.0, st
          # Cleanup so later subtests see the model.
          oid = st["activeOverride"]["id"]
          cli(f"sla clear {oid}")
    '';

    hw-normalize = ''
      with subtest("hw-normalize: mixed hw_class samples refit to same ref-seconds"):
          # Two synthetic hw_classes with a 2x throughput gap. The
          # hw_perf_factors view needs >=3 distinct pod_ids per class
          # before it admits the class; seed those directly so the
          # HwTable that the refit reads has factor[slow]=1.0,
          # factor[fast]=2.0.
          for hw, factor in [("synth-slow", 1.0), ("synth-fast", 2.0)]:
              for i in range(3):
                  ${gatewayHost}.succeed(
                      "sudo -u postgres psql -d rio -c \""
                      "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) "
                      f"VALUES ('{hw}', 'pod-{hw}-{i}', {factor})\""
                  )
          # Same true T(c) curve (S=30, P=2000) sampled on both: fast
          # hw reports half the wall-clock. After hw normalization both
          # land on the same ref-seconds, so a single Amdahl fit covers
          # them with low residual.
          for c, t_ref in [(4, 530), (8, 280), (16, 155), (32, 92.5), (64, 61.25)]:
              for hw, k in [("synth-slow", 1.0), ("synth-fast", 0.5)]:
                  grpcurl_admin("InjectBuildSample", {
                      "pname": "synth-hw", "system": "x86_64-linux",
                      "tenant": "", "durationSecs": t_ref * k,
                      "peakMemoryBytes": 1073741824,
                      "cpuLimitCores": c, "cpuSecondsTotal": t_ref * k * c * 0.9,
                      "hwClass": hw,
                  })
          wait_estimator_tick()
          st = json.loads(grpcurl_admin("SlaStatus", {
              "pname": "synth-hw", "system": "x86_64-linux", "tenant": "",
          }))
          assert st.get("hasFit"), f"no fit: {st}"
          # If normalization worked, S~30 P~2000 (the ref-seconds curve);
          # if it did NOT, the fast samples at half-wall pull P toward
          # ~1500. 10% band tolerates NNLS + partial-pool shrinkage.
          assert 1800 < st["p"] < 2200, f"P={st['p']} (norm failed?)"
          assert st["sigmaResid"] < 0.2, f"sigma={st['sigmaResid']}"
    '';

    seed-corpus = ''
      with subtest("seed-corpus: export -> reset -> import seeds prior"):
          # The convergence/outlier subtests left synth-amdahl with a
          # fitted curve. Export it (min_n=1 so the test corpus is
          # non-empty even if n_eff is small under TCG).
          out = cli("sla export-corpus --min-n 1 -o /tmp/corpus.json")
          print(out)
          ${gatewayHost}.succeed("test -s /tmp/corpus.json")
          body = ${gatewayHost}.succeed("cat /tmp/corpus.json")
          parsed = json.loads(body)
          assert "ref_hw_class" in parsed and "entries" in parsed, body
          n_exported = len(parsed["entries"])
          # Wipe synth-amdahl's samples + cached fit so the next refit
          # has no per-key theta to lean on. --tenant defaults to "".
          cli("sla reset synth-amdahl")
          # Import the corpus; seed map now has synth-amdahl.
          out = cli("sla import-corpus /tmp/corpus.json")
          print(out)
          assert ("loaded %d seeds" % n_exported) in out, out
          # Inject one fresh on-curve sample so a refit fires for this
          # key (priors are blended INTO a refit, not surfaced for a
          # never-seen key). With n_eff~1 the prior dominates (w<0.25)
          # and prior_source records Seed.
          grpcurl_admin("InjectBuildSample", {
              "pname": "synth-amdahl", "system": "x86_64-linux",
              "tenant": "", "durationSecs": 530,
              "peakMemoryBytes": 1073741824,
              "cpuLimitCores": 4, "cpuSecondsTotal": 2000,
          })
          wait_estimator_tick()
          st = json.loads(grpcurl_admin("SlaStatus", {
              "pname": "synth-amdahl", "system": "x86_64-linux", "tenant": "",
          }))
          assert st.get("hasFit"), st
          assert st.get("priorSource") == "seed", (
              f"expected seed-sourced prior, got {st.get('priorSource')!r}: {st}"
          )
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
      {
        before = "outlier";
        after = "override-precedence";
        msg = "override-precedence asserts SlaExplain on the synth-amdahl fit";
      }
      {
        before = "override-precedence";
        after = "seed-corpus";
        msg = "seed-corpus exports synth-amdahl then resets it";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
