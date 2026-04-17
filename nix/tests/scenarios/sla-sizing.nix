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
# cost-solve — seed band-aware hw_perf_samples + low lambda, queue a
#   build with the worker stopped, assert GetSpawnIntents
#   intents[].nodeSelector carries rio.build/hw-band. Covers
#   sched.sla.solve-per-band-cap. Requires the solve_full -> intent_for
#   node_selector wiring; fails until that lands.
#
# ice-backoff — append interrupt_samples (RPC + DB roundtrip) and
#   re-observe the queued build: nodeSelector still carries a valid
#   (band, cap) pair. lambda fold is on a 10min poller tick so the
#   on-demand flip is NOT observable here; covered by solve.rs unit
#   tests. Covers sched.sla.tier-envelope (selector envelope shape).
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

  # Distinct pname for the cost-solve subtest so its fit + queue state
  # are independent of synth-amdahl (which earlier subtests reset /
  # override). Body is irrelevant (worker is stopped while this is
  # queued); the fixture-miss marker is defensive only.
  costDrv = drvs.mkCustom {
    name = "synth-cost-0";
    extraAttrs.pname = "synth-cost";
    script = ''
      echo synth-cost-fixture-miss-ran-real-build > $out
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

    def cli(args: str) -> str:
        return ${gatewayHost}.succeed(
            "${common.covShellEnv}"
            "RIO_SCHEDULER_ADDR=localhost:9001 "
            f"${rioCli} {args} 2>&1"
        )

    import time

    def wait_estimator_tick() -> None:
        # ESTIMATOR_REFRESH_EVERY=6 ticks x tickIntervalSecs=2 = 12s
        # cadence; gate on the refresh-counter advancing rather than
        # sleeping a fixed amount (TCG variance).
        base = metric_value(scrape_metrics(${gatewayHost}, 9091),
            "rio_scheduler_sla_refit_total") or 0.0
        ${gatewayHost}.wait_until_succeeds(
            "curl -fsS localhost:9091/metrics | "
            f"awk '/^rio_scheduler_sla_refit_total / {{exit !($2>{base})}}'",
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
          sigma = st.get("sigmaResid", 0.0)
          assert sigma < 0.2, f"sigma={sigma}"
    '';

    cost-solve = ''
      with subtest("cost-solve: solve_full populates SpawnIntent.nodeSelector"):
          # Phase-13 wiring: hw_cost_source="static" + populated
          # hw_perf_factors + non-seed lambda -> solve_full picks a
          # (band, cap) and stamps it into SpawnIntent.node_selector.
          # ASSERT-LIGHT: only checks the selector is non-empty and
          # carries rio.build/hw-band; the per-band cost ranking is
          # unit-tested in solve.rs. Standalone fixture has no
          # controller, so we read the scheduler-side intent via
          # GetSpawnIntents instead of inspecting a pod spec.
          #
          # NOTE: requires solve_full -> intent_for -> intents
          # node_selector wiring (impl-todos commit 3). Until that
          # lands, nodeSelector stays {} and this subtest fails.
          band_hw = [("intel-6-ebs", 1.0), ("intel-7-ebs", 1.4), ("intel-8-nvme", 2.0)]
          for hw, factor in band_hw:
              for i in range(3):
                  ${gatewayHost}.succeed(
                      "sudo -u postgres psql -d rio -c \""
                      "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) "
                      f"VALUES ('{hw}', 'pod-{hw}-{i}', {factor})\""
                  )
              # Low lambda (1 interrupt / 3600s exposure) so spot
              # stays feasible under the p>0.5 gate.
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "exposure", "value": 3600})
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "interrupt", "value": 1})
          # Structural precondition: the hw_perf_factors view (HAVING
          # count(DISTINCT pod_id)>=3, 7d window) admits the band-aware
          # rows just inserted. solve_intent_for gates on
          # !hw_table.is_empty(); if this is 0 the gate never opens
          # and node_selector stays {}.
          n_hw = int(psql(${gatewayHost},
              "SELECT COUNT(*) FROM hw_perf_factors "
              "WHERE hw_class IN ('intel-6-ebs','intel-7-ebs','intel-8-nvme')"
          ))
          assert n_hw == 3, (
              f"hw_perf_factors view shows {n_hw}/3 band-aware classes; "
              "measured_at default or HAVING gate broken?"
          )
          # On-curve samples (S=30 P=2000) -> synth-cost gets a fit on
          # the next refresh tick so intent_for takes the solve branch.
          for c, t in [(4, 530), (8, 280), (16, 155), (32, 92.5), (64, 61.25)]:
              grpcurl_admin("InjectBuildSample", {
                  "pname": "synth-cost", "system": "x86_64-linux",
                  "tenant": "", "durationSecs": t,
                  "peakMemoryBytes": 1073741824,
                  "cpuLimitCores": c, "cpuSecondsTotal": t * c * 0.9,
              })
          wait_estimator_tick()
          # Stop the worker so the next submit stays Ready and shows
          # up in compute_spawn_intents -> intents.
          worker.systemctl("stop rio-builder")
          client.execute(
              "nohup nix-build --no-out-link "
              "--store 'ssh-ng://${gatewayHost}' "
              "--arg busybox '(builtins.storePath ${pkgs.pkgsStatic.busybox})' "
              "${costDrv} > /tmp/synth-cost.log 2>&1 < /dev/null &"
          )
          # Poll until the scheduler reports a SpawnIntent for the
          # queued synth-cost drv. GetSpawnIntents.intents is
          # populated iff [sla] is configured (it is, in
          # slaSizingFixture).
          intent: dict = {}
          resp: dict = {}
          for _ in range(30):
              resp = json.loads(grpcurl_admin("GetSpawnIntents", {}))
              intents = resp.get("intents", [])
              if intents:
                  intent = intents[0]
                  break
              time.sleep(2)
          assert intent, (
              "no SpawnIntent after 60s. "
              f"GetSpawnIntents={resp!r} "
              f"nix-build={client.execute('cat /tmp/synth-cost.log')[1]!r}"
          )
          sel = intent.get("nodeSelector", {})
          print(f"cost-solve: nodeSelector={sel}")
          if not sel:
              # Diagnostic: which solve_intent_for gate failed?
              st = json.loads(grpcurl_admin("SlaStatus", {
                  "pname": "synth-cost", "system": "x86_64-linux", "tenant": "",
              }))
              raise AssertionError(
                  "SpawnIntent.nodeSelector empty: solve_full gate not "
                  "satisfied. Gate = hw_cost_source set AND !hw_table."
                  "is_empty() AND n_eff>=3 AND (span>=4 OR frozen). "
                  f"hw_perf_factors rows={n_hw}, "
                  f"SlaStatus[synth-cost]={st!r}, intent={intent!r}"
              )
          assert "rio.build/hw-band" in sel, sel
          assert sel.get("rio.build/hw-band") in ("hi", "mid", "lo"), sel
    '';

    ice-backoff = ''
      with subtest("ice-backoff: per-(band,cap) selector survives interrupt-sample churn"):
          # ASSERT-LIGHT: AppendInterruptSample exercises the
          # controller -> interrupt_samples write path; lambda is
          # folded into CostTable by spot_price_poller on a 10-MINUTE
          # tick (cost.rs:587), so we cannot observe the high-lambda
          # -> on-demand flip within the 300s test budget. The
          # lambda-driven p>0.5 gate is unit-tested in solve.rs
          # (r[verify sched.sla.solve-per-band-cap]). Here we verify
          # that after seeding interrupt_samples + an estimator tick,
          # the queued build still receives a well-formed (band,cap)
          # nodeSelector — i.e. the solve_full path is stable across
          # the per-(band,cap) candidate enumeration.
          for hw in ("intel-6-ebs", "intel-7-ebs", "intel-8-nvme"):
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "interrupt", "value": 100})
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "exposure", "value": 1})
          n_int = int(psql(${gatewayHost},
              "SELECT COUNT(*) FROM interrupt_samples"))
          assert n_int >= 6, f"interrupt_samples rows={n_int}; RPC dropped?"
          wait_estimator_tick()
          # synth-cost is still queued (worker stopped); its SpawnIntent
          # is recomputed every snapshot.
          sel: dict = {}
          for _ in range(15):
              resp = json.loads(grpcurl_admin("GetSpawnIntents", {}))
              intents = resp.get("intents", [])
              if intents:
                  sel = intents[0].get("nodeSelector", {})
                  break
              time.sleep(2)
          print(f"ice-backoff: nodeSelector={sel}")
          assert sel.get("rio.build/hw-band") in ("hi", "mid", "lo"), sel
          assert sel.get("karpenter.sh/capacity-type") in ("spot", "on-demand"), sel
          # Cleanup: drain the backgrounded build + restore the worker
          # so seed-corpus (chained after) has a working fixture.
          client.execute("pkill -f 'nix-build.*synth-cost' || true")
          worker.systemctl("start rio-builder")
          worker.wait_for_unit("rio-builder.service")
          ${gatewayHost}.wait_until_succeeds(
              "curl -fsS localhost:9091/metrics | "
              "awk '/^rio_scheduler_workers_active / {exit !($2>=1)}'",
              timeout=60,
          )
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
      {
        before = "cost-solve";
        after = "ice-backoff";
        msg = "ice-backoff reuses cost-solve's queued drv + band-aware hw_perf_samples";
      }
      {
        before = "ice-backoff";
        after = "seed-corpus";
        msg = "ice-backoff stops the worker; seed-corpus needs it restarted";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
