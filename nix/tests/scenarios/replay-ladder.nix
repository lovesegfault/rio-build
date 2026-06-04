# rio-replay canary-probe ladder, end to end against the real cluster.
#
# The campaign engine (rio-replay) drives the REAL gateway / scheduler /
# store / postgres over the real worker protocol, from a v1 replay
# archive recorded inside the VM by the engine's own recorder
# (`rio-replay record-drvs` — the producing crate's ArchiveWriter, never
# a hand-assembled consumer fixture). The test then scripts an OUTAGE
# (an iptables partition of the engine→gateway wire, dropped mid-
# campaign) and walks the infra-rate canary-probe ladder through its
# whole arc:
#
#   latch → single-job probes fail (outage held) → operator PAUSE file
#   → outage cleared + PAUSE removed → a probe succeeds VIA TARGET
#   SUBSTITUTION (warm upstream serving the canary) → ladder resets →
#   campaign drains and the final report carries no false PAUSE.
#
# Topology: standalone fixture with ZERO worker VMs (substitute.nix
# precedent). Phase-1 jobs and post-recovery canaries resolve via the
# scheduler's substitution walk against a fake binary cache on the
# client VM (the same end-to-end path vm-substitute-standalone proves);
# outage-phase jobs are cut off by the partition before anything could
# resolve them. No assertion in this scenario depends on a build
# executing, which removes builder timing (the TCG-variance class) from
# the latch/probe walk entirely — the probe ladder's semantics are
# identical whether the canary resolves by building or substituting.
#
# What is REAL: rio-gateway, rio-scheduler, rio-store, postgres, the
# rio-replay engine binary (plan/supply/submit/collect/watchdog/report
# stages), the worker-protocol transport (russh), the substitution walk
# (service-HMAC tokens), the archive (recorded by the production
# writer). What is SCRIPTED: the outage (iptables DROP of :2222), the
# upstream cache contents (which jobs are substitutable, and when), the
# operator actions (removing the PAUSE file), and the absence of
# builders (jobs that must fail can only fail).
#
# Subtest 2 (relay-breaker-neutrality) is a separate small campaign
# against the same booted cluster: the supply stage's relay rung fetches
# large payloads from an HTTPS relay whose NAR bodies are TRUNCATED
# (mid-body death, the payload-source shape). The gateway circuit
# breaker must stay closed — per-path FAILED settlements, no
# "gateway unreachable" skip-stamps, no "supply upload collapse" PAUSE —
# and the campaign completes with both roots substituted (the in-band
# success exemption keeps the failed-supply rollup from retiring them).
#
# Verify markers live at the default.nix subtests entries (house rule).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;

  rioReplay = "${common.rio-workspace}/bin/rio-replay";
  rioCli = "${common.rio-workspace}/bin/rio-cli";

  # ── Workload derivations ─────────────────────────────────────────────
  # One expression, evaluated to a LIST so a single nix-instantiate call
  # yields every drv path in list order. Tranche naming is load-bearing:
  # the engine sorts attemptable jobs by job name, so a-jobs are offered
  # first (phase-1, substitutable), then b-jobs (outage fuel), then the
  # w-jobs (sacrificial grab buffer for the latch race), then z-jobs
  # (the recovery canaries the latch must save).
  ladderJobNames =
    map (i: "a0${toString i}") (pkgs.lib.range 0 3)
    ++ map (i: "b${pkgs.lib.fixedWidthNumber 2 i}") (pkgs.lib.range 0 15)
    ++ map (i: "w0${toString i}") (pkgs.lib.range 0 3)
    ++ map (i: "z0${toString i}") (pkgs.lib.range 0 3);

  ladderJobsExpr = pkgs.writeText "replay-ladder-jobs.nix" ''
    { busybox }:
    let
      mk = name: derivation {
        name = "rio-replay-ladder-''${name}";
        system = builtins.currentSystem;
        builder = "''${busybox}/bin/sh";
        args = [
          "-c"
          "''${busybox}/bin/busybox mkdir -p $out && ''${busybox}/bin/busybox echo ''${name}-v1 > $out/x"
        ];
      };
    in
    map mk [ ${pkgs.lib.concatMapStringsSep " " (n: ''"${n}"'') ladderJobNames} ]
  '';

  # Two relay-leg jobs, each referencing 8 of the 16 relay blobs as
  # input sources (the `pads` env var puts the store paths into the
  # drv's inputSrcs, which is where the recorder's closure records and
  # the engine's supply planner read them from).
  relayJobsExpr = pkgs.writeText "replay-relay-jobs.nix" ''
    { busybox, relayPaths }:
    let
      mk = name: pads: derivation {
        name = "rio-replay-relay-''${name}";
        pads = toString pads;
        system = builtins.currentSystem;
        builder = "''${busybox}/bin/sh";
        args = [
          "-c"
          "''${busybox}/bin/busybox mkdir -p $out && ''${busybox}/bin/busybox echo ''${name}-v1 > $out/x"
        ];
      };
      first = n: list: if n == 0 then [ ] else [ (builtins.head list) ] ++ first (n - 1) (builtins.tail list);
      drop = n: list: if n == 0 then list else drop (n - 1) (builtins.tail list);
    in
    [
      (mk "r00" (first 8 relayPaths))
      (mk "r01" (drop 8 relayPaths))
    ]
  '';

  bbArg = "--arg busybox '(builtins.storePath ${common.busybox})'";

  # ── Engine knob rationale (subtest 1) ────────────────────────────────
  # cluster_status_poll_secs=1 / collect_poll_secs=2: the poller latches
  #   and scores within seconds of evidence landing; the post-settlement
  #   re-offer cool-down equals collect_poll_secs.
  # submit_concurrency=1: at most ONE batch in flight — after the latch
  #   trips, the loop can grab at most one more batch before the pause
  #   bit lands (the w-tranche absorbs it), so the z-tranche structurally
  #   survives to be probe canaries.
  # batch_max_jobs=4: tranche-aligned waves; outage terminals land 4 per
  #   settled batch, so the 20-record minimum sample (4 substituted a's
  #   + 16 infra b's) is crossed in whole-batch steps.
  # max_auto_retries=0: an outage-shaped submission failure terminalizes
  #   on the first charge — the latch fuel accrues one batch per failure
  #   instead of needing N sweeps (probe failures are exempt from this
  #   budget by the carve-out, which is exactly what subtest 1 proves).
  # max_engine_cancel_cycles=1 (validation floor): the one batch caught
  #   in flight by the partition gets its single cancel re-offer, whose
  #   import then dies against the partition and terminalizes.
  # batch_timeout_hours=0.012 (43s) > active_stall_hours=0.011 (40s):
  #   validation-ordered; bounds how long the partition-caught batch
  #   lives (engine-cancel at the deadline). Post-partition submissions
  #   die FASTER, at the transport's own 30s channel-setup budget
  #   (daemon session exec+handshake against the dropped wire), as an
  #   Err settle — op_timeout_secs=8 covers the in-channel ops behind
  #   it. Outage cadence is therefore ~30s per failed batch.
  # infra_pause_pct=25 (default): 16 infra terminals over a 20-record
  #   window is 80%, comfortably across.
  ladderKnobs = {
    cluster_status_poll_secs = 1;
    collect_poll_secs = 2;
    submit_concurrency = 1;
    batch_max_jobs = 4;
    max_auto_retries = 0;
    max_engine_cancel_cycles = 1;
    batch_timeout_hours = 1.2e-2;
    active_stall_hours = 1.1e-2;
    op_timeout_secs = 8;
  };

  # Subtest 2 knobs: upload_workers=3 puts the breaker collapse
  # threshold at max(2*3, 6) = 6 — the 16 serial relay-payload failures
  # are nearly 3× past it, so a regression that charges payload deaths
  # to the breaker trips it with certainty (and then wedges the campaign
  # on the supply-collapse PAUSE, which the engine-exit wait catches).
  # large_nar_threshold_mib=1 routes the 1.5 MiB relay blobs onto the
  # streamed lane (plan.large is relay-only and runs serially BEFORE the
  # batch lane — no interleaved success could reset a mischarged count).
  relayKnobs = {
    cluster_status_poll_secs = 1;
    collect_poll_secs = 2;
    submit_concurrency = 1;
    batch_max_jobs = 2;
    batch_timeout_hours = 1.2e-2;
    active_stall_hours = 1.1e-2;
    op_timeout_secs = 8;
    upload_workers = 3;
    large_nar_threshold_mib = 1;
  };

  prelude = ''
    ${common.assertions}

    # json + re come with lib/assertions.py; shlex is ours.
    import shlex

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}

    # ════════════════════════════════════════════════════════════════
    # Shared setup: tenant, SSH identity, fake upstream, host-key pin
    # ════════════════════════════════════════════════════════════════
    # Tenant the engine builds as (self-hosted mode pairs with the
    # conventional name `replay-selfhosted`). Created directly in PG —
    # the AdminService path is covered by vm-cli-k3s.
    tenant_id = psql(
        ${gatewayHost},
        "INSERT INTO tenants (tenant_name) VALUES ('replay-selfhosted') "
        "RETURNING tenant_id",
    )
    print(f"replay tenant: {tenant_id}")

    # Engine SSH identity. The COMMENT in the gateway's authorized_keys
    # line selects the tenant (the wire never carries it), so the key is
    # generated with the tenant name and installed verbatim.
    client.succeed(
        "mkdir -p /root/.ssh && "
        "ssh-keygen -t ed25519 -N ''' -C 'replay-selfhosted' "
        "-f /root/.ssh/replay_key"
    )
    replay_pubkey = client.succeed("cat /root/.ssh/replay_key.pub").strip()
    ${gatewayHost}.succeed(
        f"echo '{replay_pubkey}' > /var/lib/rio/gateway/authorized_keys"
    )
    ${gatewayHost}.succeed("systemctl restart rio-gateway.service")
    ${gatewayHost}.wait_for_unit("rio-gateway.service")
    ${gatewayHost}.wait_for_open_port(2222)

    # Host-key pin: the engine's transport refuses to dial without one.
    # The gateway auto-generates its key on first start; derive the
    # OpenSSH public line from the private half.
    host_key_pin = ${gatewayHost}.succeed(
        "ssh-keygen -y -f /var/lib/rio/gateway/host_key"
    ).strip()
    assert host_key_pin.startswith("ssh-"), f"bad host key pin: {host_key_pin!r}"

    # Fake upstream binary cache on the client (substitute.nix
    # mechanics): nix-generated narinfo+nar served over plain HTTP for
    # the TENANT upstream (rio-store fetches it), and the test signing
    # key the tenant trusts.
    client.succeed("mkdir -p /srv/cache /srv/relay /var/lib/replay")
    client.succeed(
        "nix key generate-secret --key-name test-cache-1 > /var/lib/replay/cache.sec && "
        "nix key convert-secret-to-public < /var/lib/replay/cache.sec > /var/lib/replay/cache.pub"
    )
    cache_pubkey = client.succeed("cat /var/lib/replay/cache.pub").strip()
    client.succeed(
        "systemd-run --unit=replay-cache "
        "${pkgs.python3}/bin/python3 -m http.server 8080 "
        "--bind 0.0.0.0 --directory /srv/cache"
    )
    client.wait_for_open_port(8080)
    # Positive control: the store (on control) can reach the cache. A
    # broken route here would make every later "substituted" assertion
    # vacuous in the worst way (silent queue-forever).
    ${gatewayHost}.succeed("curl -sf -o /dev/null http://client:8080/")

    def cli(args):
        return ${gatewayHost}.succeed(
            "${common.covShellEnv}"
            "RIO_STORE_ADDR=localhost:9002 "
            "RIO_SERVICE_HMAC_KEY_PATH=${fixture.hmacKeys}/service-hmac.key "
            f"${rioCli} {args} 2>&1"
        )

    out = cli(
        f"upstream add --tenant {tenant_id} "
        "--url http://client:8080 --priority 50 "
        f"--trusted-key '{cache_pubkey}' --sig-mode keep"
    )
    assert "added upstream http://client:8080" in out, out

    def publish(paths):
        """Sign store paths with the test key and publish them (plus
        their closures) to the fake upstream cache."""
        joined = " ".join(paths)
        client.succeed(
            f"nix store sign --key-file /var/lib/replay/cache.sec --recursive {joined}"
        )
        client.succeed(
            "nix copy --no-check-sigs "
            f"--to 'file:///srv/cache?compression=none' {joined}"
        )

    def write_json(path, obj):
        client.succeed(f"echo {shlex.quote(json.dumps(obj))} > {path}")

    def jsonl(path):
        rc, out = client.execute(f"cat {path} 2>/dev/null")
        if rc != 0:
            return []
        records = []
        for line in out.splitlines():
            line = line.strip()
            if line:
                records.append(json.loads(line))
        return records

    def latest_per_job(records):
        latest = {}
        for record in records:
            latest[record["job"]] = record
        return latest

    def campaign_spec(campaign_id, archive_id, knobs):
        """The dev-mode operator input: a self-hosted, timeless campaign
        spec against the in-VM cluster. Validated by the engine's own
        CampaignSpec::load, so a drifted field fails loudly at start."""
        return {
            "campaign_id": campaign_id,
            "mode": "self-hosted",
            "archive": {"digest": archive_id},
            "s3": {"bucket": None},
            "cluster": {
                "gateway_store_url":
                    "ssh-ng://rio@${gatewayHost}:2222?ssh-key=/root/.ssh/replay_key",
                "scheduler_addr": "${gatewayHost}:9001",
                "store_addr": "${gatewayHost}:9002",
                "service_hmac_key_path": "${fixture.hmacKeys}/service-hmac.key",
                "gateway_host_key": host_key_pin,
            },
            "tenants": {
                "build_tenant": "replay-selfhosted",
                "warm_tenant": "replay-warm",
            },
            "knobs": knobs,
        }

    def start_engine(unit, spec_path, archive_dir, state_dir):
        client.succeed(f"mkdir -p {state_dir}")
        client.succeed(
            f"systemd-run --unit={unit} "
            "--setenv=RUST_LOG=info "
            "--setenv=RIO_LOG_FORMAT=pretty "
            "--setenv=SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt "
            f"sh -c '${rioReplay} run --spec {spec_path} "
            f"--archive {archive_dir} --state-dir {state_dir} "
            f"--no-s3 --allow-unverified-tenants; "
            f"echo $? > {state_dir}.exit'"
        )

    def wait_engine_exit(unit, state_dir, timeout):
        client.wait_until_succeeds(f"test -f {state_dir}.exit", timeout=timeout)
        code = client.succeed(f"cat {state_dir}.exit").strip()
        if code != "0":
            _, log_tail = client.execute(
                f"journalctl -u {unit} --no-pager | tail -n 80"
            )
            raise Exception(f"{unit} exited {code}; journal tail:\n{log_tail}")
  '';

  fragments = {
    probe-ladder-walk = ''
      with subtest("probe-ladder-walk: latch, probes, PAUSE, recovery via substitution"):
          # ── Record the campaign archive with the production recorder ─
          drv_paths = [
              p.strip()
              for p in client.succeed(
                  "nix-instantiate ${bbArg} ${ladderJobsExpr} 2>/dev/null"
              ).splitlines()
              if p.strip()
          ]
          job_names = ${builtins.toJSON ladderJobNames}
          assert len(drv_paths) == len(job_names), (
              f"expected {len(job_names)} drvs, got {len(drv_paths)}"
          )
          jobs = {f"{name}.x86_64-linux": drv for name, drv in zip(job_names, drv_paths)}
          tranche = lambda prefix: sorted(j for j in jobs if j.startswith(prefix))

          busybox_refs = [
              r.strip()
              for r in client.succeed(
                  "nix-store -q --references ${common.busybox}"
              ).splitlines()
              if r.strip()
          ]
          write_json("/var/lib/replay/rec-ladder.json", {
              "units": [{"job": job, "drv": drv} for job, drv in sorted(jobs.items())],
              "embed": [{"path": "${common.busybox}", "references": busybox_refs}],
          })
          rec = json.loads(client.succeed(
              "${rioReplay} record-drvs "
              "--spec /var/lib/replay/rec-ladder.json "
              "--out /var/lib/replay/archive-ladder 2>/dev/null"
          ).strip().splitlines()[-1])
          assert rec["units"] == len(jobs) and rec["drvs"] == len(jobs), rec
          print(f"ladder archive: {rec['archiveId']}")

          # Phase-1 canaries: realize the a-jobs locally and publish
          # their outputs, so the first wave resolves via the
          # scheduler's substitution walk (no builders exist).
          a_outs = []
          for job in tranche("a"):
              a_outs.append(client.succeed(
                  f"nix-store --realise {jobs[job]} 2>/dev/null"
              ).strip())
          publish(a_outs)

          state = "/var/lib/replay/ladder"
          write_json(
              "/var/lib/replay/spec-ladder.json",
              campaign_spec("vm-ladder", rec["archiveId"], ${builtins.toJSON ladderKnobs}),
          )
          start_engine("replay-ladder", "/var/lib/replay/spec-ladder.json",
                       "/var/lib/replay/archive-ladder", state)

          # ── Scripted outage, mid-campaign ─────────────────────────
          # Wait for the phase-1 wave to land (the only 4 jobs that CAN
          # produce records while the cluster is healthy — b-jobs are
          # unsubstitutable and builderless), then drop the engine→
          # gateway wire. DROP (not REJECT): submissions must die
          # SLOWLY (op-timeout / batch-deadline shaped) like a real
          # outage, which is also what keeps terminal-record chunks
          # smaller than the poller's latch cadence.
          client.wait_until_succeeds(
              f"test $(wc -l < {state}/results.jsonl) -ge 4",
              timeout=240,
          )
          early = latest_per_job(jsonl(f"{state}/results.jsonl"))
          assert all(j.startswith("a") for j in early), (
              f"only phase-1 a-jobs can settle while healthy: {sorted(early)}"
          )
          assert all(
              r.get("disposition") == "target-substituted" for r in early.values()
          ), (
              "phase-1 jobs must resolve via the substitution walk; a "
              f"different class means the upstream wiring is broken: {early}"
          )
          # BOTH families: the test VMs are dual-stack and the engine
          # resolves `control` to whichever family getaddrinfo prefers —
          # a v4-only rule leaves the v6 path wide open (observed: every
          # "partitioned" submission still got a build id).
          client.succeed("iptables -I OUTPUT -p tcp --dport 2222 -j DROP")
          client.succeed("ip6tables -I OUTPUT -p tcp --dport 2222 -j DROP")
          print("partition: engine->gateway wire dropped (v4+v6)")

          # ── The ladder escalates to the operator PAUSE ────────────
          # Structural wait: the PAUSE file is the escalation artifact
          # itself. Generous bound — the walk is (in-flight batch
          # cancel) + 4-5 failing batches + 3 probe cycles, each
          # op-timeout/batch-deadline shaped.
          client.wait_until_succeeds(f"test -f {state}/PAUSE", timeout=480)
          pause_text = client.succeed(f"cat {state}/PAUSE").strip()
          assert pause_text == "infra-rate canary probes exhausted", (
              f"PAUSE file must name the ladder escalation, got {pause_text!r}"
          )

          # At escalation: exactly the three failed probe cycles were
          # released, every probe was a single job, and no probe
          # consumed budgets or journal entries.
          batches = jsonl(f"{state}/batches.jsonl")
          probes = [b for b in batches if b.get("probe")]
          assert len(probes) == 3, (
              f"exactly INFRA_PROBE_PAUSE_AFTER=3 probe batches before the "
              f"PAUSE, got {len(probes)}: {[b['batchId'] for b in probes]}"
          )
          assert all(len(b["jobs"]) == 1 for b in probes), (
              f"every canary probe is a single-job batch: {probes}"
          )
          requeues = jsonl(f"{state}/requeues.jsonl")
          assert not [r for r in requeues if r["why"] == "infra-probe"], (
              f"probe re-offers are never journaled: {requeues}"
          )
          z_jobs = set(tranche("z"))
          assert not [r for r in requeues if r["job"] in z_jobs], (
              "z-tranche jobs were never in a non-probe batch, so no "
              f"requeue may name them: {requeues}"
          )
          results = latest_per_job(jsonl(f"{state}/results.jsonl"))
          assert not (z_jobs & set(results)), (
              "failed probe cycles must not retire their conscripted "
              f"jobs: {sorted(z_jobs & set(results))}"
          )
          infra_at_pause = [
              j for j, r in results.items()
              if r.get("verdict") == "infra-indeterminate"
          ]
          substituted_at_pause = [
              j for j, r in results.items()
              if r.get("disposition") == "target-substituted"
          ]
          assert len(results) >= 20, (
              f"the latch needs the 20-record minimum sample: {len(results)}"
          )
          assert len(infra_at_pause) >= 16, (
              f"the 16 b-jobs must all have retired infra-shaped: "
              f"{sorted(infra_at_pause)}"
          )
          assert set(substituted_at_pause) == set(tranche("a")), (
              "nothing but the phase-1 a-jobs can substitute before "
              f"recovery: {sorted(substituted_at_pause)}"
          )
          # No probe batch produced a terminal record for its job (the
          # carve-out): every probed job is still record-less.
          probed = {j for b in probes for j in b["jobs"]}
          assert not (probed & set(results)), (
              f"infra-shaped probe failures never write terminals: "
              f"{sorted(probed & set(results))}"
          )

          # ── Recovery: outage cleared, canaries warmed, PAUSE lifted ─
          # Publish every SURVIVOR's output (any job without a terminal
          # record — the z-tranche by construction, plus whatever the
          # latch timing spared) so the next probe SUBSTITUTES: the
          # m049 leg, an in-band target substitution is cluster work
          # and must score the cycle as success.
          survivors_at_pause = sorted(set(jobs) - set(results))
          assert set(tranche("z")) <= set(survivors_at_pause), (
              f"the latch must save the z-tranche: {survivors_at_pause}"
          )
          survivor_outs = []
          for job in survivors_at_pause:
              survivor_outs.append(client.succeed(
                  f"nix-store --realise {jobs[job]} 2>/dev/null"
              ).strip())
          publish(survivor_outs)
          client.succeed("iptables -D OUTPUT -p tcp --dport 2222 -j DROP")
          client.succeed("ip6tables -D OUTPUT -p tcp --dport 2222 -j DROP")
          client.succeed(f"rm {state}/PAUSE")
          print("recovery: partition lifted, canary outputs published, PAUSE removed")

          # The engine must now drain on its own: probe succeeds via
          # substitution → ladder resets → remaining survivors retire
          # one probe cycle at a time (the latch never releases — the
          # window stays infra-dominated — so every survivor drains
          # through the probe path). A regressed scorer (the m049
          # false-PAUSE) re-escalates after 3 substituted probes and
          # wedges the campaign on a PAUSE nobody removes — caught
          # here as a non-exit.
          wait_engine_exit("replay-ladder", state, timeout=360)

          # ── Final structural accounting ───────────────────────────
          assert client.execute(f"test -f {state}/PAUSE")[0] != 0, (
              "the PAUSE file must not be re-written after the operator "
              "removed it (a substituted probe scores success and resets "
              "the ladder)"
          )
          results = latest_per_job(jsonl(f"{state}/results.jsonl"))
          assert len(results) == len(jobs), (
              f"every job terminal at drain: {len(results)}/{len(jobs)}"
          )
          by_class = {}
          for job, record in results.items():
              key = record.get("verdict") or record.get("disposition")
              by_class.setdefault(key, []).append(job)
          substituted = sorted(by_class.get("target-substituted", []))
          infra = sorted(by_class.get("infra-indeterminate", []))
          assert set(substituted) | set(infra) == set(jobs), (
              f"only substituted/infra classes exist in this walk: {by_class}"
          )
          # Exact split: phase-1 a-jobs and the recovery survivors
          # substituted; everything else retired infra-shaped during
          # the outage. The z-tranche is always on the survivor side
          # (the latch saved it); the b/w middle is decided by latch
          # timing and is asserted exactly, not assumed.
          assert set(substituted) == set(tranche("a")) | set(survivors_at_pause), (
              f"substituted set must be phase-1 + survivors: {by_class}"
          )
          assert set(infra) == set(jobs) - set(substituted), by_class
          assert set(tranche("z")) <= set(substituted), (
              f"the recovery canaries must substitute, got: {by_class}"
          )

          # Probe arithmetic: 3 failed cycles pre-PAUSE, then exactly
          # one successful probe per surviving job (the latch holds to
          # the end, so nothing else can submit them).
          batches = jsonl(f"{state}/batches.jsonl")
          probes = [b for b in batches if b.get("probe")]
          # 3 failed cycles pre-PAUSE + one successful probe per
          # survivor; +2 slack for transient post-recovery probe
          # failures (a re-dial hiccup costs one extra exempt cycle,
          # never an escalation — the m049 wedge is caught by the
          # engine-exit wait, the single-escalation count, and the
          # absent PAUSE regardless of this count).
          assert 3 + len(survivors_at_pause) <= len(probes) <= 5 + len(survivors_at_pause), (
              f"probe batches = 3 failed + {len(survivors_at_pause)} "
              f"survivor drains (+2 slack), got {len(probes)}"
          )
          assert all(len(b["jobs"]) == 1 for b in probes)
          # The z-tranche only ever rode probe batches.
          for b in batches:
              if not b.get("probe"):
                  assert not (set(b["jobs"]) & z_jobs), (
                      f"z-jobs must never ride a non-probe batch: {b}"
                  )
          # Probe budget exemption held for the whole run.
          requeues = jsonl(f"{state}/requeues.jsonl")
          assert not [r for r in requeues if r["why"] == "infra-probe"]
          assert not [r for r in requeues if r["job"] in z_jobs]

          # The escalation fired exactly once (the second walk's probes
          # all scored success), and at least one success was scored.
          journal = client.succeed(
              "journalctl -u replay-ladder --no-pager"
          )
          assert journal.count("consecutive canary probes failed") == 1, (
              "exactly one ladder escalation in the whole walk"
          )
          assert "canary probe cycle concluded" in journal
          # progress.json mirrors the final accounting.
          progress = json.loads(client.succeed(f"cat {state}/progress.json"))
          assert progress["stage"] == "done", progress["stage"]
          assert progress["dispositionCounts"].get("target-substituted") == len(substituted)
          assert progress["verdictCounts"].get("infra-indeterminate") == len(infra)
          client.succeed(f"test -f {state}/report/summary.md")
          print(
              f"probe-ladder-walk PASS: {len(infra)} infra, "
              f"{len(substituted)} substituted, {len(probes)} probes "
              f"({len(survivors_at_pause)} survivors)"
          )
    '';

    relay-breaker-neutrality = ''
      with subtest("relay-breaker-neutrality: payload deaths never trip the collapse"):
          # ── 16 relay blobs, published whole then truncated ────────
          # 1.5 MiB each (over the campaign's 1 MiB streamed-lane
          # threshold); the narinfos keep declaring the full NarSize
          # while the served bodies stop at 128 KiB — every streamed
          # upload's relay reader comes up short mid-body.
          relay_paths = []
          for i in range(16):
              client.succeed(
                  f"head -c 1572864 /dev/urandom > /tmp/relay-blob-{i} && "
                  f"printf 'relay-{i}' >> /tmp/relay-blob-{i}"
              )
              relay_paths.append(client.succeed(
                  f"nix-store --add /tmp/relay-blob-{i}"
              ).strip())
          client.succeed(
              "nix store sign --key-file /var/lib/replay/cache.sec "
              + " ".join(relay_paths)
          )
          client.succeed(
              "nix copy --no-check-sigs --to 'file:///srv/relay?compression=none' "
              + " ".join(relay_paths)
          )
          client.succeed(
              "for f in /srv/relay/nar/*.nar; do truncate -s 131072 $f; done"
          )
          client.wait_for_open_port(8443)
          # Positive control: the relay answers over TLS with the test
          # CA, and the body really is truncated. Without this, every
          # payload-source assertion below could pass vacuously on an
          # unreachable relay (a connect failure is a CHANNEL failure
          # and would charge the breaker — the opposite cell).
          some_hash = relay_paths[0].removeprefix("/nix/store/").split("-", 1)[0]
          narinfo_text = client.succeed(
              f"curl -sf https://client:8443/{some_hash}.narinfo"
          )
          assert "NarSize: 1572" in narinfo_text or "NarSize: 1573" in narinfo_text, (
              f"relay narinfo must declare the FULL NarSize: {narinfo_text}"
          )
          nar_url = next(
              line.split(":", 1)[1].strip()
              for line in narinfo_text.splitlines()
              if line.startswith("URL:")
          )
          got = client.succeed(
              f"curl -sf https://client:8443/{nar_url} | wc -c"
          ).strip()
          assert got == "131072", f"relay body must be truncated, got {got} bytes"

          # ── Jobs whose closures REQUIRE the relay paths ───────────
          relay_paths_nix = " ".join(
              f'(builtins.storePath "{p}")' for p in relay_paths
          )
          drvs = [
              p.strip()
              for p in client.succeed(
                  "nix-instantiate ${bbArg} "
                  f"--arg relayPaths '[ {relay_paths_nix} ]' "
                  "${relayJobsExpr} 2>/dev/null"
              ).splitlines()
              if p.strip()
          ]
          assert len(drvs) == 2, drvs
          jobs = {"r00.x86_64-linux": drvs[0], "r01.x86_64-linux": drvs[1]}
          # Both roots are pre-published on the TENANT upstream: the
          # campaign must complete via substitution even though every
          # relay-sourced input failed to deliver (the in-band success
          # exemption — a substituted root must not be retired by its
          # batch's settled-FAILED supply rows).
          outs = [
              client.succeed(f"nix-store --realise {d} 2>/dev/null").strip()
              for d in drvs
          ]
          publish(outs)

          busybox_refs = [
              r.strip()
              for r in client.succeed(
                  "nix-store -q --references ${common.busybox}"
              ).splitlines()
              if r.strip()
          ]
          write_json("/var/lib/replay/rec-relay.json", {
              "units": [{"job": job, "drv": drv} for job, drv in sorted(jobs.items())],
              "embed": [{"path": "${common.busybox}", "references": busybox_refs}],
              "substituters": {"relay": ["https://client:8443"]},
          })
          rec = json.loads(client.succeed(
              "${rioReplay} record-drvs "
              "--spec /var/lib/replay/rec-relay.json "
              "--out /var/lib/replay/archive-relay 2>/dev/null"
          ).strip().splitlines()[-1])
          print(f"relay archive: {rec['archiveId']}")

          state = "/var/lib/replay/relay"
          write_json(
              "/var/lib/replay/spec-relay.json",
              campaign_spec("vm-relay", rec["archiveId"], ${builtins.toJSON relayKnobs}),
          )
          start_engine("replay-relay", "/var/lib/replay/spec-relay.json",
                       "/var/lib/replay/archive-relay", state)

          # The campaign must run to completion on its own. The
          # regression shape — payload deaths charged to the breaker —
          # trips the collapse at 6 consecutive failures (16 here, with
          # the streamed lane serial so no success can interleave) and
          # parks the campaign on the "supply upload collapse" PAUSE,
          # which nobody removes: caught as a non-exit.
          wait_engine_exit("replay-relay", state, timeout=360)

          # ── Per-path degrade, breaker closed ──────────────────────
          supply = jsonl(f"{state}/supply.jsonl")
          payload_failures = [
              row for row in supply
              if row["outcome"] == "failed"
              and "payload source failed during upload" in (row.get("detail") or "")
          ]
          assert {row["path"] for row in payload_failures} == set(relay_paths), (
              f"every relay path settles its own payload-source FAILED row: "
              f"{len(payload_failures)} rows"
          )
          assert all(
              row["mechanism"] == "upload-stream" and row["source"] == "relay"
              for row in payload_failures
          ), payload_failures
          skip_stamped = [
              row for row in supply
              if "gateway unreachable; not attempted" in (row.get("detail") or "")
          ]
          assert not skip_stamped, (
              f"a failing relay must never skip-stamp uploads against a "
              f"healthy gateway (breaker mischarge): {skip_stamped}"
          )
          # The ONLY other failure-shaped rows are the bounded dependent
          # cascade: each relay-leg drv text references failed relay
          # blobs, so its own embedded upload settles failed-with-skip
          # ("reference X failed its earlier upload"). That cascade must
          # name a relay blob and reach nothing else — busybox and every
          # unrelated upload stay clean. (The drv texts still reach the
          # cluster through the per-batch submission import, which is
          # why the campaign completes regardless.)
          dependent_skips = [
              row for row in supply
              if row["outcome"] in ("failed", "refused")
              and row["path"] not in set(relay_paths)
          ]
          for row in dependent_skips:
              assert "failed its earlier upload" in (row.get("detail") or ""), (
                  f"unexpected non-relay supply failure: {row}"
              )
              assert any(p in row["detail"] for p in relay_paths), (
                  f"dependent skip must name a failed relay blob: {row}"
              )
              assert row["path"].endswith(".drv"), (
                  f"the cascade may only reach the relay-leg drv texts: {row}"
              )
          assert len(dependent_skips) <= 2, dependent_skips
          # The supply-collapse PAUSE never fired (no PAUSE file exists
          # now, and the engine exited instead of waiting on one).
          assert client.execute(f"test -f {state}/PAUSE")[0] != 0
          journal = client.succeed("journalctl -u replay-relay --no-pager")
          assert "circuit breaker tripped" not in journal, (
              "the breaker must stay closed across 16 serial relay "
              "payload deaths"
          )

          # The campaign itself completed cleanly: both roots
          # substituted (never retired by the failed supply rows).
          results = latest_per_job(jsonl(f"{state}/results.jsonl"))
          assert set(results) == set(jobs), sorted(results)
          for job, record in results.items():
              assert record.get("disposition") == "target-substituted", (
                  f"{job}: substituted root must classify "
                  f"target-substituted, got {record.get('verdict')}"
                  f"/{record.get('disposition')}"
              )
          print(
              f"relay-breaker-neutrality PASS: {len(payload_failures)} "
              "payload-source rows, breaker closed, campaign drained"
          )
    '';
  };
in
{
  inherit fragments;
  mkTest = common.mkFragmentTest {
    scenario = "replay-ladder";
    inherit prelude fragments fixture;
    defaultTimeout = 1800;
  };
}
