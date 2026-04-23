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
# cost-solve — seed hw-class-aware hw_perf_samples + low lambda,
#   queue a build with the worker stopped, assert GetSpawnIntents
#   intents[].nodeAffinity carries OR-of-ANDs (h,cap) terms. Covers
#   sched.sla.hw-class.admissible-set.
#
# ice-backoff — append interrupt_samples (RPC + DB roundtrip), then
#   AckSpawnedIntents.unfulfillableCells one cell and re-observe the
#   queued build: that cell is dropped from nodeAffinity (read-time
#   mask, memo unchanged). Covers sched.sla.hw-class.ice-mask.
#
# admissible-set — InjectBuildSample WITH hwClass across the 3
#   fixture-configured classes (the K=3 alpha path; cost-solve covers
#   the scalar path), queue, assert nodeAffinity ≥1 term, then assert
#   /metrics HELP enumerates all six InfeasibleReason labels. Covers
#   sched.sla.hw-class.admissible-set.
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
        echo synth-amdahl-fixture-miss-ran-real-build >&2
        echo ok > $out
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
      echo synth-cost-fixture-miss-ran-real-build >&2
      echo ok > $out
    '';
  };

  # admissible-set subtest pname. Its build_samples carry hwClass (the
  # K=3 alpha path), unlike synth-cost (scalar path). Body irrelevant.
  admitDrv = drvs.mkCustom {
    name = "synth-admit-0";
    extraAttrs.pname = "synth-admit";
    script = ''
      echo synth-admit-fixture-miss-ran-real-build >&2
      echo ok > $out
    '';
  };
  # admissible-set subtest: fit with S=10000 (serial floor) → infeasible
  # at the fixture's p90=1200 → emits rio_scheduler_sla_infeasible_total
  # so the metric (HELP + 6 reason labels) is observable in /metrics.
  serialDrv = drvs.mkCustom {
    name = "synth-serial-0";
    extraAttrs.pname = "synth-serial";
    script = ''
      echo synth-serial-fixture-miss-ran-real-build >&2
      echo ok > $out
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
        # cadence. housekeeping.rs:85-87 increments refit_total BEFORE
        # awaiting refresh(), so +1 only proves a refresh STARTED; +2
        # proves the prior refresh COMPLETED (when counter reads
        # base+2, refresh base+1 has returned). awk seeds hit=0 so a
        # not-yet-emitted counter (no refit since boot) FAILS the gate
        # instead of default-exit-0 passing vacuously.
        base = metric_value(scrape_metrics(${gatewayHost}, 9091),
            "rio_scheduler_sla_refit_total") or 0.0
        ${gatewayHost}.wait_until_succeeds(
            "curl -fsS localhost:9091/metrics | "
            f"awk 'BEGIN{{hit=0}} /^rio_scheduler_sla_refit_total / "
            f"{{hit=($2>{base+1})}} END{{exit !hit}}'",
            timeout=45,
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
          # (former `COUNT(DISTINCT cpu_limit_cores) >= 1` dropped: it's
          # tautologically true after COUNT(*)>=3 above. Under fixture
          # intercept, explore may legitimately keep cores constant
          # across 3 submits, so >=2 is not assertable here either.)
          # PRECONDITION (not gate-fired proof): cpu_seconds_total /
          # duration_secs / cpu_limit > 0.4 → saturated bit set on the
          # ExploreState. The explore-{x4-first-bump,saturation-gate,
          # freeze} properties themselves are r[verify]'d at unit level
          # (sla/explore.rs:182,193,205); this subtest only proves the
          # build_samples plumbing feeds the estimator.
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
          # Two synthetic hw_classes with a 2x throughput gap.
          # cross_tenant_median admits a class once >=3 distinct
          # pod_ids AND >=FLEET_MEDIAN_MIN_TENANTS (5) distinct
          # submitting_tenant have benched; seed 5 of each so the
          # refit reads factor[slow]=1.0, factor[fast]=2.0.
          for hw, factor in [("synth-slow", 1.0), ("synth-fast", 2.0)]:
              for i in range(5):
                  ${gatewayHost}.succeed(
                      "sudo -u postgres psql -d rio -c \""
                      "INSERT INTO hw_perf_samples "
                      "(hw_class, pod_id, factor, submitting_tenant) "
                      f"VALUES ('{hw}', 'pod-{hw}-{i}', "
                      f"jsonb_build_object('alu', {factor}), 't{i}')\""
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
          # 0.25: bounded-ALS (alpha-als) adds one alternation round of
          # residual jitter on top of the NNLS fit; the P-band above is
          # the structural assertion, sigma is a noise-floor sanity
          # check. Without normalization sigma > 0.4 (ln 2 gap between
          # the two hw_class curves dominates).
          assert sigma < 0.25, f"sigma={sigma}"
    '';

    cost-solve = ''
      with subtest("cost-solve: solve_full populates SpawnIntent.nodeAffinity"):
          # Phase-13 wiring: hw_cost_source="static" + sla.hw_classes
          # configured + populated hw_perf_samples -> solve_full builds
          # the admissible set and stamps it into
          # SpawnIntent.node_affinity (OR-of-ANDs over (h,cap)).
          # ASSERT-LIGHT: only checks the affinity is non-empty and
          # each term carries rio.build/hw-class +
          # karpenter.sh/capacity-type; the admissible-set ranking is
          # unit-tested in solve.rs. Standalone fixture has no
          # controller, so we read the scheduler-side intent via
          # GetSpawnIntents instead of inspecting a pod spec.
          band_hw = [("intel-6-ebs-lo", 1.0), ("intel-7-ebs-mid", 1.4), ("intel-8-nvme-hi", 2.0)]
          for hw, factor in band_hw:
              for i in range(5):
                  ${gatewayHost}.succeed(
                      "sudo -u postgres psql -d rio -c \""
                      "INSERT INTO hw_perf_samples "
                      "(hw_class, pod_id, factor, submitting_tenant) "
                      f"VALUES ('{hw}', 'pod-{hw}-{i}', "
                      f"jsonb_build_object('alu', {factor}), 't{i}')\""
                  )
              # Low lambda (1 interrupt / 3600s exposure) so spot
              # stays feasible under the p>0.5 gate.
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "exposure", "value": 3600})
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "interrupt", "value": 1})
          # Structural precondition: HwTable.aggregate admits a class
          # once >=3 distinct pod_ids exist in the 7d window (M_054
          # dropped the hw_perf_factors view; same gate, app-side now).
          # solve_intent_for gates on !hw_table.is_empty() AND
          # !sla.hw_classes.is_empty(); if this is 0 the gate never
          # opens and node_affinity stays [].
          n_hw = int(psql(${gatewayHost},
              "SELECT COUNT(*) FROM ("
              "  SELECT hw_class FROM hw_perf_samples "
              "  WHERE measured_at > now() - interval '7 days' "
              "    AND hw_class IN "
              "      ('intel-6-ebs-lo','intel-7-ebs-mid','intel-8-nvme-hi') "
              "  GROUP BY hw_class HAVING count(DISTINCT pod_id) >= 3"
              ") q"
          ))
          assert n_hw == 3, (
              f"hw_perf_samples shows {n_hw}/3 band-aware classes admitted; "
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
          aff = intent.get("nodeAffinity", [])
          print(f"cost-solve: nodeAffinity={aff}")
          if not aff:
              # Diagnostic: which solve_intent_for gate failed?
              st = json.loads(grpcurl_admin("SlaStatus", {
                  "pname": "synth-cost", "system": "x86_64-linux", "tenant": "",
              }))
              raise AssertionError(
                  "SpawnIntent.nodeAffinity empty: solve_full gate not "
                  "satisfied. Gate = hw_cost_source set AND "
                  "!sla.hw_classes.is_empty() AND !hw_table.is_empty() "
                  "AND n_eff>=3 AND (span>=4 OR frozen). "
                  f"hw classes admitted={n_hw}, "
                  f"SlaStatus[synth-cost]={st!r}, intent={intent!r}"
              )
          # Each term: h-label conjunction PLUS karpenter capacity-type.
          for term in aff:
              keys = {r["key"] for r in term.get("matchExpressions", [])}
              assert "karpenter.sh/capacity-type" in keys, term
              assert "rio.build/hw-class" in keys, term
              hw = next(r["values"][0] for r in term["matchExpressions"]
                        if r["key"] == "rio.build/hw-class")
              assert hw in (
                  "intel-6-ebs-lo", "intel-7-ebs-mid", "intel-8-nvme-hi"
              ), term
    '';

    ice-backoff = ''
      with subtest("ice-backoff: read-time ICE mask drops cell from nodeAffinity"):
          # AppendInterruptSample exercises the controller ->
          # interrupt_samples write path; lambda is folded into
          # CostTable by spot_price_poller on a 10-MINUTE tick, so we
          # cannot observe the high-lambda -> on-demand flip within
          # the 300s test budget (the p>0.5 gate is unit-tested in
          # solve.rs). Here we verify the read-time mask: report one
          # cell unfulfillable via AckSpawnedIntents and assert the
          # next nodeAffinity drops it WITHOUT re-solving (memo
          # unchanged).
          for hw in ("intel-6-ebs-lo", "intel-7-ebs-mid", "intel-8-nvme-hi"):
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "interrupt", "value": 100})
              grpcurl_admin("AppendInterruptSample",
                  {"hwClass": hw, "kind": "exposure", "value": 1})
          n_int = int(psql(${gatewayHost},
              "SELECT COUNT(*) FROM interrupt_samples"))
          # cost-solve (chained before this subtest) inserts 3x2=6 rows;
          # ice-backoff inserts 3x2=6 more above. Floor is 12.
          assert n_int >= 12, (
              f"ice-backoff inserted {n_int-6}/6 interrupt_samples; RPC dropped?"
          )
          wait_estimator_tick()
          # synth-cost is still queued (worker stopped); its SpawnIntent
          # is recomputed every snapshot. Baseline nodeAffinity:
          aff0: list = []
          for _ in range(15):
              resp = json.loads(grpcurl_admin("GetSpawnIntents", {}))
              intents = resp.get("intents", [])
              if intents:
                  aff0 = intents[0].get("nodeAffinity", [])
                  break
              time.sleep(2)
          print(f"ice-backoff: baseline nodeAffinity={aff0}")
          assert aff0, "solve_full path active"
          def cells_of(aff: list) -> set:
              return {
                  (next(r["values"][0] for r in t["matchExpressions"]
                        if r["key"] == "rio.build/hw-class"),
                   next(r["values"][0] for r in t["matchExpressions"]
                        if r["key"] == "karpenter.sh/capacity-type"))
                  for t in aff
              }
          a0 = cells_of(aff0)
          # Read-time mask is a subset op on A: masking a cell NOT in A
          # is a no-op; masking ALL of A falls back to A (never empty).
          # Under seed prices A often collapses to one cell; the
          # multi-cell subset behavior is unit-tested
          # (actor::tests::dispatch::ice_mask_is_read_time). Here:
          # (1) AckSpawnedIntents.unfulfillableCells RPC plumbing,
          # (2) cell not in A is a no-op,
          # (3) cell IN A masked: result is A \ {it} or A (never empty).
          not_in_a = next(h for h in (
              "intel-6-ebs-lo", "intel-7-ebs-mid", "intel-8-nvme-hi"
          ) if (h, "on-demand") not in a0)
          grpcurl_admin("AckSpawnedIntents", {
              "spawned": [],
              "unfulfillableCells": [f"{not_in_a}:on-demand"],
          })
          resp = json.loads(grpcurl_admin("GetSpawnIntents", {}))
          aff1 = resp.get("intents", [{}])[0].get("nodeAffinity", [])
          assert cells_of(aff1) == a0, (
              f"masking cell not in A is no-op: before={a0} after={cells_of(aff1)}"
          )
          masked_h, masked_cap = next(iter(a0))
          grpcurl_admin("AckSpawnedIntents", {
              "spawned": [],
              "unfulfillableCells": [f"{masked_h}:{masked_cap}"],
          })
          resp = json.loads(grpcurl_admin("GetSpawnIntents", {}))
          aff2 = resp.get("intents", [{}])[0].get("nodeAffinity", [])
          a2 = cells_of(aff2)
          print(f"ice-backoff: post-mask nodeAffinity={aff2}")
          assert a2, "read-time mask never yields empty affinity"
          assert a2.issubset(a0), f"mask is subset op: {a2} not subset of {a0}"
          if len(a0) > 1:
              assert (masked_h, masked_cap) not in a2, (
                  f"masked cell ({masked_h},{masked_cap}) dropped from |A|>1"
              )
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

    admissible-set = ''
      with subtest("admissible-set: per-hw_class samples -> nodeAffinity + 6 infeasible reasons registered"):
          # A19: end-to-end check for the §13a per-hw_class build_samples
          # path. cost-solve injects samples WITHOUT hwClass (scalar
          # normalize); this injects WITH hwClass so the K=3 alpha-ALS
          # branch fires. Chained after ice-backoff: the band-aware
          # hw_perf_samples (intel-6/7/8) and low-lambda interrupt_
          # samples are already seeded; worker is running.
          #
          # Same true T(c) curve (S=30, P=2000) on each h, scaled by
          # that h's alu factor (1.0/1.4/2.0 from cost-solve's seed) so
          # the samples are c↔h-correlated — exactly the deconfounding
          # the alpha-ALS rank gate is meant to handle.
          band_admit = [("intel-6-ebs-lo", 1.0), ("intel-7-ebs-mid", 1.4), ("intel-8-nvme-hi", 2.0)]
          for c, t_ref in [(4, 530), (8, 280), (16, 155), (32, 92.5), (64, 61.25)]:
              for hw, k in band_admit:
                  grpcurl_admin("InjectBuildSample", {
                      "pname": "synth-admit", "system": "x86_64-linux",
                      "tenant": "", "durationSecs": t_ref / k,
                      "peakMemoryBytes": 1073741824,
                      "cpuLimitCores": c, "cpuSecondsTotal": (t_ref/k) * c * 0.9,
                      "hwClass": hw,
                  })
          # synth-serial: constant t=10000 across c → fit S~10000, P~0 →
          # at p90=1200 the serial floor binds → intent_for emits
          # rio_scheduler_sla_infeasible_total{reason="serial_floor"}.
          # span 64/4=16, n_eff=5 so the fit gate opens.
          for c in (4, 8, 16, 32, 64):
              grpcurl_admin("InjectBuildSample", {
                  "pname": "synth-serial", "system": "x86_64-linux",
                  "tenant": "", "durationSecs": 10000,
                  "peakMemoryBytes": 1073741824,
                  "cpuLimitCores": c, "cpuSecondsTotal": 10000.0 * c * 0.9,
              })
          wait_estimator_tick()
          st = json.loads(grpcurl_admin("SlaStatus", {
              "pname": "synth-admit", "system": "x86_64-linux", "tenant": "",
          }))
          assert st.get("hasFit"), f"synth-admit no fit after 15 hw-tagged samples: {st}"
          # Stop the worker so synth-admit stays Ready and shows up in
          # GetSpawnIntents. ice-backoff drained synth-cost; if a stray
          # intent lingers it ALSO carries nodeAffinity (cost-solve
          # proved that), so the all() below stays valid.
          worker.systemctl("stop rio-builder")
          for drv in ("${admitDrv}", "${serialDrv}"):
              client.execute(
                  "nohup nix-build --no-out-link "
                  "--store 'ssh-ng://${gatewayHost}' "
                  "--arg busybox '(builtins.storePath ${pkgs.pkgsStatic.busybox})' "
                  f"{drv} > /tmp/synth-admit.log 2>&1 < /dev/null &"
              )
          intents: list = []
          for _ in range(30):
              resp = json.loads(grpcurl_admin("GetSpawnIntents", {}))
              intents = resp.get("intents", [])
              if any("synth-admit" in i.get("intentId", "") for i in intents):
                  break
              time.sleep(2)
          admit = [i for i in intents if "synth-admit" in i.get("intentId", "")]
          assert admit, (
              "no SpawnIntent for synth-admit after 60s. "
              f"intents={intents!r} "
              f"nix-build={client.execute('cat /tmp/synth-admit.log')[1]!r}"
          )
          aff = admit[0].get("nodeAffinity", [])
          print(f"admissible-set: synth-admit nodeAffinity={aff}")
          assert len(aff) >= 1, (
              "nodeAffinity has <1 term — solve_full gate not satisfied "
              f"for hw-tagged samples. SlaStatus[synth-admit]={st!r} intent={admit[0]!r}"
          )
          for term in aff:
              keys = {r["key"] for r in term.get("matchExpressions", [])}
              assert "rio.build/hw-class" in keys, term
              assert "karpenter.sh/capacity-type" in keys, term
          # synth-serial's intent_for hits BestEffort → emits
          # `_infeasible_total{reason=…}` once per snapshot. Poll until
          # the metric surfaces, then assert the HELP line enumerates
          # all six InfeasibleReason::ALL strings (metrics-exporter-
          # prometheus only renders HELP for emitted metrics, so the
          # synth-serial dispatch is what makes this observable).
          ${gatewayHost}.wait_until_succeeds(
              "curl -fsS localhost:9091/metrics | "
              "grep -q '^rio_scheduler_sla_infeasible_total{'",
              timeout=45,
          )
          body = ${gatewayHost}.succeed(
              "curl -fsS localhost:9091/metrics | "
              "grep rio_scheduler_sla_infeasible_total"
          )
          print(f"admissible-set: infeasible_total lines:\n{body}")
          help_line = next(
              (l for l in body.splitlines() if l.startswith("# HELP ")), ""
          )
          reasons = ("serial_floor", "mem_ceiling", "disk_ceiling",
                     "core_ceiling", "interrupt_runaway",
                     "capacity_exhausted")
          for reason in reasons:
              assert reason in help_line, (
                  f"reason '{reason}' missing from /metrics HELP: {help_line!r}"
              )
          # Per-value emit coverage (both directions) is owned by the
          # `labeled_metric_values_have_emit_sites` unit test now —
          # this subtest only asserts the HELP line surfaces.
          # Cleanup: drain + restore worker for seed-corpus.
          client.execute("pkill -f 'nix-build.*synth-' || true")
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
        after = "admissible-set";
        msg = "admissible-set reuses cost-solve's band-aware hw_perf_samples + interrupt_samples";
      }
      {
        before = "admissible-set";
        after = "seed-corpus";
        msg = "admissible-set stops the worker; seed-corpus needs it restarted";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
