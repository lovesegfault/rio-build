# Flag-transition rollouts: the FP-4 both-directions deployment proof
# (substitution-replacement Phase B, design §8-B / §4; plan T-3.2).
#
# The deployment-level revertability story (RFB-4): a deployment can flip
# the materialization flag ON over live walk-era state, and back OFF over
# live job-era state, and neither direction strands work, fails builds
# wrongfully, or leaks state into the other era's mechanisms.
#
#   OFF -> ON  (FP-4 b): walk-era state (Substituting nodes / unresolved
#       walk work) at flip time is absorbed: the new leader's recovery
#       resets it, the flag-on dispatch probe creates jobs for it, the
#       store executor completes the build. No node stays Substituting,
#       no build fails.
#   ON -> OFF  (FP-4 a): job-era state at flip time is absorbed: pending
#       (parked) jobs survive as INERT rows (never claimed, never
#       cancelled — §4 "leftover rows are inert") while the as-built walk
#       completes their build; topdown_pruned marks cleared by flag-on
#       job resolutions (the §4 clear-mirror) stay cleared, so nothing
#       wrongfully fail-fasts; flag-on-era materialization PINS keep
#       protecting their paths until interest goes terminal, at which
#       point the always-on release fires FLAG-OFF and the path becomes
#       collectable.
#
# ── Why k3s, not the standalone fixture ───────────────────────────────
# Recovery (recover_from_pg) runs ONLY on LeaderAcquired, which only
# fires in lease-based (k8s) deployments. A standalone (non-K8s)
# scheduler restart starts with an empty in-memory DAG and never reloads
# PG state — in-flight builds are permanently orphaned by ANY restart,
# flag flip or not. The FP-4 transition story is therefore a k8s-only
# operation by construction, and this scenario exercises it the way a
# real deployment performs it: env change + rolling restart of both
# Deployments, new leader election, recovery.
#
# ── Scenario structure (one k3s boot, two runtime flips) ─────────────
#   phase 1 (chart deployed flag-off)  walks against a blackholed
#                                      upstream + dormancy
#   FLIP ON   (store rollout, then scheduler rollout — AS-6 order)
#                                      recovery absorbs walk-era state ->
#                                      jobs -> build completes
#   phase 2 (flag-on)                  marks build (pruned mark set +
#                                      cleared), pins build (dep pinned,
#                                      build stays live), inert build
#                                      (jobs parked on a broken upstream)
#   FLIP OFF  (scheduler rollout, then store rollout — AS-6 order)
#                                      parked jobs survive untouched;
#                                      walks finish their build; cleared
#                                      marks stay cleared; pins release
#                                      at terminal interest; GC collects
#
# The builder Pool is deleted at scenario start: nothing here may build
# from source, so walk-era work that exhausts its retries waits
# (visibly) instead of falling through to builder pods.
#
# Markers live at the wiring point (nix/tests/default.nix).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) ns nsStore nsBuilders;

  rioCli = "${common.rio-workspace}/bin/rio-cli";
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";
  protoset = import ../lib/protoset.nix { inherit pkgs; };
  jwtKeys = import ../lib/jwt-keys.nix;

  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );
  signJwt = pkgs.writeScript "sign-jwt-transition" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-trans-test"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';

  # Test derivations. Deps are inputDrvs-only (never runtime refs) so
  # each output publishes alone and closure walks stay single-path.
  transClosure = pkgs.writeText "transition-closure.nix" ''
    { busybox }:
    let
      sh = "''${busybox}/bin/sh";
      bb = "''${busybox}/bin/busybox";
      mk = name: deps: derivation {
        inherit name;
        system = builtins.currentSystem;
        builder = sh;
        args = [
          "-c"
          "''${bb} mkdir -p $out && ''${bb} echo ''${name}-v1 > $out/x"
        ] ++ deps;
      };
    in rec {
      # Phase 1 (off->on): two independent substitutable nodes.
      t1a       = mk "rio-trans-t1a" [ ];
      t1b       = mk "rio-trans-t1b" [ ];
      # Marks build: prunable root + the dep the prune drops.
      markdep   = mk "rio-trans-markdep" [ ];
      markroot  = mk "rio-trans-markroot" [ markdep ];
      # Keep-alive build: shares markroot, blocked on a never-buildable node.
      blocker   = mk "rio-trans-blocker" [ ];
      # Pins build: unbuildable root + substitutable dep.
      pindep    = mk "rio-trans-pindep" [ ];
      pinroot   = mk "rio-trans-pinroot" [ pindep ];
      # Inert build (on->off): two independent substitutable nodes.
      i1        = mk "rio-trans-i1" [ ];
      i2        = mk "rio-trans-i2" [ ];
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-materialize-transition";
  skipTypeCheck = true;

  # k3s bring-up ~240 s + cache seed/tenant wiring ~90 s + off-phase
  # ~60 s + flip-on (2 rollouts + leader + recovery + jobs) ~240 s +
  # flag-on phase ~120 s + flip-off (2 rollouts + walk completion) ~240 s
  # + marks/pins/GC ~120 s.
  globalTimeout = 1800 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
      withSeed = false;
    }}

    # ════════════════════════════════════════════════════════════════════
    # Prelude: upstream cache, tenant, helpers
    # ════════════════════════════════════════════════════════════════════
    NIX = "nix --extra-experimental-features nix-command"

    # No from-source fallback anywhere in this scenario: delete the
    # builder Pool so walk-era work that exhausts its retries WAITS
    # instead of dispatching to builder pods (which could never build
    # these gRPC-direct submissions anyway).
    kubectl(
        "delete pool x86-64 --ignore-not-found --wait=true",
        ns="${nsBuilders}",
    )

    upstream_v6.succeed(
        "nix-store --load-db < ${common.busyboxClosure}/registration"
    )
    upstream_v6.succeed(
        f"{NIX} key generate-secret --key-name trans-1 > /tmp/sec && "
        f"{NIX} key convert-secret-to-public < /tmp/sec > /tmp/pub"
    )
    cache_pubkey = upstream_v6.succeed("cat /tmp/pub").strip()

    # Build + publish every test output on upstream_v6.
    upstream_v6.succeed(
        "nix-build --no-out-link --option substituters ''' "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${transClosure} -A t1a -A t1b -A markroot -A pindep -A i1 -A i2 2>&1"
    )
    publish_attrs = ["t1a", "t1b", "markroot", "pindep", "i1", "i2"]
    seed_outs = []
    for attr in publish_attrs:
        d = upstream_v6.succeed(
            "nix-instantiate "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${transClosure} -A " + attr + " 2>/dev/null"
        ).strip()
        seed_outs.append(upstream_v6.succeed(f"nix-store -q --outputs {d}").strip())
    outs_sh = " ".join(seed_outs)
    upstream_v6.succeed(f"{NIX} store sign --key-file /tmp/sec {outs_sh}")
    upstream_v6.succeed(
        f"{NIX} copy --no-check-sigs "
        f"--to 'file:///srv?compression=none' {outs_sh}"
    )
    upstream_v6.wait_for_open_port(8080)
    h0 = seed_outs[0].removeprefix("/nix/store/").split("-", 1)[0]
    k3s_server.succeed(
        f"curl -sf 'http://upstream-v6:8080/{h0}.narinfo' | grep -q 'Sig: trans-1:'"
    )

    # Upstream blackhole control: REJECT with tcp-reset makes every
    # probe/fetch fail FAST with a connection refused. ip6tables, NOT
    # iptables: upstream-v6 is a v6-only node (every store-pod fetch
    # arrives over IPv6), so an IPv4 rule matches nothing and the
    # "broken" upstream keeps serving — the walks then complete in
    # seconds and the in-flight-at-flip-time preconditions silently
    # evaporate. tcp-reset (a TCP packet, not ICMP) propagates reliably
    # through the CNI. Probes classify indeterminate -> walks/jobs are
    # still created optimistically per B3; fetches fail as infra
    # trouble. break/heal are strictly paired in the script below, so
    # the rule list never grows beyond one entry.
    def break_upstream():
        upstream_v6.succeed(
            "ip6tables -I INPUT 1 -p tcp --dport 8080 "
            "-j REJECT --reject-with tcp-reset"
        )

    def heal_upstream():
        upstream_v6.succeed(
            "ip6tables -D INPUT -p tcp --dport 8080 "
            "-j REJECT --reject-with tcp-reset 2>/dev/null || true"
        )

    def assert_upstream_broken():
        """Non-vacuity guard: the blackhole must actually take effect
        (a rule on the wrong address family silently breaks every
        in-flight precondition this scenario depends on)."""
        k3s_server.fail(f"curl -sf -m 5 'http://upstream-v6:8080/{h0}.narinfo'")

    # The client instantiates the same closure for the submissions.
    def drv_info(attr):
        d = client.succeed(
            "nix-instantiate "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${transClosure} -A " + attr + " 2>/dev/null"
        ).strip()
        out_path = client.succeed(f"nix-store -q --outputs {d}").strip()
        return d, out_path

    # ── Tenant + JWT + upstream registration ───────────────────────────
    pf_open(leader_pod(), 19001, 9001, tag="pf-sched")
    pf_open("svc/rio-store", 19002, 9002, ns="${nsStore}", tag="pf-store")
    k3s_server.succeed(
        "k3s kubectl -n ${ns} get secret rio-service-hmac "
        "-o jsonpath='{.data.service-hmac\\.key}' | base64 -d "
        "> /tmp/service-hmac.key"
    )
    CLI_ENV = (
        "${common.covShellEnv}"
        "RIO_SCHEDULER_ADDR=localhost:19001 "
        "RIO_STORE_ADDR=localhost:19002 "
        "RIO_SERVICE_HMAC_KEY_PATH=/tmp/service-hmac.key "
    )

    def cli(args):
        return k3s_server.succeed(f"{CLI_ENV}${rioCli} {args} 2>&1")

    out = cli("create-tenant trans-tenant")
    m = re.search(r"\(([0-9a-f-]{36})\)", out)
    assert m, f"create-tenant didn't echo a UUID:\n{out}"
    tid = m.group(1)
    tenant_jwt = k3s_server.succeed(f"${signJwt} {tid}").strip()

    out = cli(
        f"upstream add --tenant {tid} "
        "--url http://upstream-v6:8080 --priority 50 "
        f"--trusted-key '{cache_pubkey}' --sig-mode keep"
    )
    assert "added upstream http://upstream-v6:8080" in out, out

    # ── Submission / state helpers ──────────────────────────────────────
    def repoint_leader_pf():
        pf_close("pf-sched")
        pf_open(leader_pod(), 19001, 9001, tag="pf-sched")

    def submit_dag(unit, node_specs, edge_specs=()):
        n_before = int(psql_k8s(k3s_server, "SELECT count(*) FROM builds"))
        nodes = [
            {"drvPath": drv, "drvHash": drv,
             "system": "${pkgs.stdenv.hostPlatform.system}",
             "outputNames": ["out"], "expectedOutputPaths": [out_path]}
            for drv, out_path in node_specs
        ]
        edges = [{"parentDrvPath": p, "childDrvPath": c} for p, c in edge_specs]
        payload = json.dumps({"nodes": nodes, "edges": edges})
        k3s_server.succeed(f"cat > /tmp/dag-{unit}.json <<'EOF'\n{payload}\nEOF")
        k3s_server.succeed(
            f"systemd-run --unit=dag-{unit} sh -c "
            "'${grpcurl} -plaintext -max-time 900 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            "-d @ "
            "localhost:19001 rio.scheduler.SchedulerService/SubmitBuild "
            f"< /tmp/dag-{unit}.json'"
        )
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            "'SELECT count(*) FROM builds'"
            f" | grep -qx {n_before + 1}",
            timeout=60,
        )
        return psql_k8s(
            k3s_server,
            "SELECT build_id FROM builds ORDER BY submitted_at DESC LIMIT 1",
        )

    def build_status(build_id):
        return psql_k8s(
            k3s_server, f"SELECT status FROM builds WHERE build_id = '{build_id}'"
        )

    def wait_build_status(build_id, want, timeout=180):
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            f"\"SELECT status FROM builds WHERE build_id = '{build_id}'\""
            f" | grep -qx {want}",
            timeout=timeout,
        )

    def drv_status(drv_path):
        return psql_k8s(
            k3s_server,
            f"SELECT status FROM derivations WHERE drv_hash = '{drv_path}'",
        )

    def job_state(drv_path):
        return psql_k8s(
            k3s_server,
            "SELECT state FROM materialization_jobs"
            f" WHERE drv_hash = '{drv_path}' ORDER BY created_at DESC LIMIT 1",
        )

    def claim_count(drv_path):
        return int(psql_k8s(
            k3s_server,
            "SELECT count(*) FROM assignments a"
            " JOIN derivations d USING (derivation_id)"
            f" WHERE d.drv_hash = '{drv_path}'",
        ))

    def total_jobs():
        return int(psql_k8s(k3s_server, "SELECT count(*) FROM materialization_jobs"))

    def mat_pin_count(store_path):
        return int(psql_k8s(
            k3s_server,
            "SELECT count(*) FROM scheduler_live_pins"
            " WHERE pin_kind = 'materialization'"
            f" AND store_path_hash = sha256(convert_to('{store_path}', 'UTF8'))",
        ))

    def narinfo_count(store_path):
        return int(psql_k8s(
            k3s_server,
            f"SELECT count(*) FROM narinfo WHERE store_path = '{store_path}'",
        ))

    def cancel_build(build_id):
        k3s_server.succeed(
            "${grpcurl} -plaintext -max-time 30 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            f'-d \'{{"buildId":"{build_id}"}}\' '
            "localhost:19001 rio.scheduler.SchedulerService/CancelBuild"
        )

    # ── Finding-18 diagnostics ──────────────────────────────────────────
    # The two suspects the Wave-3 stall could not discriminate: (i) test
    # artifact -- the chart override never reached the store deployment,
    # so the flip was a no-op rollout and the executor never polled;
    # (ii) product gap -- the executor's gRPC connection did not survive
    # the scheduler Deployment rollout (standby pinning). The env
    # assertions kill (i) at every step; the log dump answers (ii).
    def deployment_env(deploy, deploy_ns, var="RIO_MATERIALIZATION__ENABLED"):
        """The Deployment SPEC's env value (what new pods will get)."""
        return k3s_server.succeed(
            f"k3s kubectl -n {deploy_ns} get deployment/{deploy} -o jsonpath="
            f"'{{.spec.template.spec.containers[0].env[?(@.name==\"{var}\")].value}}'"
        ).strip()

    def pods_env(name, deploy_ns, var="RIO_MATERIALIZATION__ENABLED"):
        """Every RUNNING pod's env value (what the live pods carry)."""
        return k3s_server.succeed(
            f"k3s kubectl -n {deploy_ns} get pods -l app.kubernetes.io/name={name}"
            " --field-selector=status.phase=Running -o jsonpath="
            f"'{{range .items[*]}}{{.spec.containers[0].env[?(@.name==\"{var}\")].value}} {{end}}'"
        ).strip()

    def assert_flag_posture(value, where, timeout=180):
        """Both Deployments' spec AND their running pods carry the flag
        value. A mismatch on the spec = the chart/set-env path is broken
        (the finding-18 test-artifact suspect). The pod check is a WAIT,
        not a point assertion: a replaced pod lingers in phase=Running
        while it drains its termination grace (the store's is ~120 s),
        so right after a rollout the old and new pods briefly coexist."""
        for deploy, name, deploy_ns in (
            ("rio-scheduler", "rio-scheduler", "${ns}"),
            ("rio-store", "rio-store", "${nsStore}"),
        ):
            spec = deployment_env(deploy, deploy_ns)
            assert spec == value, (
                f"[{where}] {deploy} deployment spec env must be {value!r}, got {spec!r}"
                " (the flag never reached the deployment -- chart override or"
                " set-env path broken)"
            )
            # Every running pod (once the replaced ones finish draining)
            # carries the value: the sorted-unique set of pod env values
            # must collapse to exactly {value}.
            k3s_server.wait_until_succeeds(
                f"test \"$(k3s kubectl -n {deploy_ns} get pods"
                f" -l app.kubernetes.io/name={name}"
                " --field-selector=status.phase=Running -o jsonpath="
                "'{range .items[*]}{.spec.containers[0].env[?(@.name==\"RIO_MATERIALIZATION__ENABLED\")].value}{\"\\n\"}{end}'"
                f" | sort -u | tr -d '\\n')\" = \"{value}\"",
                timeout=timeout,
            )
        print(f"[{where}] flag posture OK: both deployments + pods at {value!r}")

    def dump_mat_diagnostics(tag):
        """On a stall, print everything needed to discriminate where the
        claim pipeline broke: deployment/pod envs, store executor logs,
        scheduler leader logs, and the jobs/assignments/builds tables.
        Every section is independently best-effort so one broken query
        can never mask the rest."""
        def section(label, fn):
            print(f"--- {label}:")
            try:
                print(fn())
            except Exception as e:  # noqa: BLE001 — diagnostics must never raise
                print(f"    <diagnostics section failed: {e}>")

        print(f"==== FINDING-18 DIAGNOSTICS ({tag}) ====")
        for deploy, name, deploy_ns in (
            ("rio-scheduler", "rio-scheduler", "${ns}"),
            ("rio-store", "rio-store", "${nsStore}"),
        ):
            section(f"{deploy} env", lambda d=deploy, n=name, dns=deploy_ns: (
                f"spec={deployment_env(d, dns)!r} pods={pods_env(n, dns)!r}"
            ))
        section("builds", lambda: psql_k8s(
            k3s_server,
            "SELECT build_id, status, submitted_at FROM builds ORDER BY submitted_at",
        ))
        section("materialization_jobs", lambda: psql_k8s(
            k3s_server,
            "SELECT job_id, drv_hash, state, origin, park_until, created_at"
            " FROM materialization_jobs ORDER BY created_at",
        ))
        section("assignments (claims)", lambda: psql_k8s(
            k3s_server,
            "SELECT a.exec_id, d.drv_hash, a.builder_id, a.status, a.assigned_at"
            " FROM assignments a JOIN derivations d USING (derivation_id)"
            " ORDER BY a.assigned_at",
        ))
        section("drv_attempts (charges)", lambda: psql_k8s(
            k3s_server,
            "SELECT d.drv_hash, t.outcome_class, t.recorded_at"
            " FROM drv_attempts t JOIN derivations d USING (derivation_id)"
            " ORDER BY t.recorded_at",
        ))
        section("store pod logs (materialization executor)", lambda: k3s_server.execute(
            "k3s kubectl -n ${nsStore} logs -l app.kubernetes.io/name=rio-store"
            " --tail=400 2>&1 | grep -iE 'materializ|unavailable|claim|executor'"
            " | tail -60"
        )[1])
        section("scheduler pod logs (materialization/leader/cancel)", lambda: k3s_server.execute(
            "k3s kubectl -n ${ns} logs -l app.kubernetes.io/name=rio-scheduler"
            " --tail=600 2>&1 | grep -iE 'materializ|recovery|leader|orphan|cancel'"
            " | tail -80"
        )[1])
        print(f"==== END DIAGNOSTICS ({tag}) ====")

    # ── Build watchers (the orphan-watcher countermeasure) ─────────────
    # Builds submitted gRPC-direct have no gateway to re-watch them after
    # a scheduler rollout: the SubmitBuild stream (the build's only
    # event receiver) dies with the leader port-forward, and an Active
    # build with zero receivers for ORPHAN_BUILD_GRACE (5 min) is
    # auto-cancelled by the orphan watcher -- which then cancels the
    # build's materialization jobs (the zero-interest closer working as
    # designed). Production clients re-attach through the gateway's
    # WatchBuild reconnect (~111 s of retries); this scenario's flips
    # outlive the grace, so it re-attaches the same way, scripted: one
    # WatchBuild stream per live build, restarted after every scheduler
    # rollout.
    watched_builds = {}
    _watch_n = [0]

    def watch_build(tag, build_id):
        """(Re)start a WatchBuild stream for the build through the
        current leader port-forward."""
        watched_builds[tag] = build_id
        _watch_n[0] += 1
        unit = f"watch-{tag}-{_watch_n[0]}"
        k3s_server.succeed(
            f"echo '{{\"buildId\":\"{build_id}\"}}' > /tmp/{unit}.json"
        )
        k3s_server.succeed(
            f"systemd-run --unit={unit} sh -c "
            "'${grpcurl} -plaintext -max-time 3600 "
            f'-H "x-rio-tenant-token: {tenant_jwt}" '
            "-protoset ${protoset}/rio.protoset "
            "-d @ "
            "localhost:19001 rio.scheduler.SchedulerService/WatchBuild "
            f"< /tmp/{unit}.json'"
        )

    def unwatch_build(tag):
        watched_builds.pop(tag, None)

    def rewatch_builds():
        for tag, build_id in list(watched_builds.items()):
            watch_build(tag, build_id)

    # ── Rollout helpers ─────────────────────────────────────────────────
    def wait_rollout(deploy, deploy_ns, timeout=240):
        """Race-free rollout wait. `kubectl set env` bumps
        .metadata.generation synchronously, but the Deployment controller
        observes it asynchronously -- a bare `rollout status` issued in
        that window reports the OLD generation as 'successfully rolled
        out' and returns in ~1 s while the real rollout has not even
        started (the Wave-3 '1.55 s no-op rollout' observation). Wait for
        observedGeneration to catch up first, then for the rollout."""
        k3s_server.wait_until_succeeds(
            f"test \"$(k3s kubectl -n {deploy_ns} get deployment/{deploy}"
            " -o jsonpath='{.status.observedGeneration}')\" ="
            f" \"$(k3s kubectl -n {deploy_ns} get deployment/{deploy}"
            " -o jsonpath='{.metadata.generation}')\"",
            timeout=30,
        )
        kubectl(
            f"rollout status deployment/{deploy} --timeout={timeout}s",
            ns=deploy_ns,
        )

    # ── Store-executor claim-wave control (the T-3.1 determinism
    #    mechanism, finding 12) ────────────────────────────────────────
    # The executor's claim timing must be scriptable: with the chart's
    # 1 s poll interval, the executor claims new jobs the instant the
    # new flag-on leader creates them (mid-scheduler-rollout) -- racing
    # ahead of this scenario's upstream heal, charging infra failures
    # against the still-broken upstream, and parking the jobs with
    # compounding backoff (the store's 1 h HEAD-probe cache makes the
    # heal invisible to retries on the same pod). A 3600 s poll interval
    # turns the executor into poll-once-per-pod-start: each
    # `restart_store_deployment()` is exactly one claim wave with a
    # fresh probe cache, fired only when the test says so.
    def set_store_poll_interval(secs):
        kubectl(
            "set env deployment/rio-store"
            f" RIO_MATERIALIZATION__POLL_INTERVAL_SECS={secs}",
            ns="${nsStore}",
        )
        wait_rollout("rio-store", "${nsStore}")

    def restart_store_deployment():
        """One executor claim wave: fresh pod, fresh HEAD-probe cache,
        one startup poll."""
        kubectl("rollout restart deployment/rio-store", ns="${nsStore}")
        wait_rollout("rio-store", "${nsStore}")

    # ── The flip: env change + rolling restart of both Deployments ─────
    def set_flag(enabled):
        """Flip the materialization flag the way a real deployment does:
        change the env on both Deployments and roll them, store-first for
        the ON direction and scheduler-first for the OFF direction (the
        AS-6 ordering: the executor is on before creation and off after
        it). Each scheduler rollout elects a new leader, whose recovery
        absorbs whatever the previous era left in flight."""
        value = "true" if enabled else "false"
        order = (
            [("rio-store", "${nsStore}"), ("rio-scheduler", "${ns}")]
            if enabled
            else [("rio-scheduler", "${ns}"), ("rio-store", "${nsStore}")]
        )
        for deploy, deploy_ns in order:
            kubectl(
                f"set env deployment/{deploy} RIO_MATERIALIZATION__ENABLED={value}",
                ns=deploy_ns,
            )
            wait_rollout(deploy, deploy_ns)
            # Finding-18 diagnostic: the env change must land on the
            # deployment spec (the chart override / set-env path works).
            spec = deployment_env(deploy, deploy_ns)
            assert spec == value, (
                f"set env on {deploy} did not land: spec env is {spec!r},"
                f" wanted {value!r}"
            )
        # A new leader exists and serves (recovery ran on acquisition).
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} get lease rio-scheduler-leader "
            "-o jsonpath='{.spec.holderIdentity}' | grep -q rio-scheduler",
            timeout=120,
        )
        repoint_leader_pf()
        # Both deployments and their live pods now carry the new posture.
        assert_flag_posture(value, f"post-flip-{value}")
        # Re-attach every live build's event watcher to the new leader
        # (the production gateway-reconnect behavior, scripted) -- without
        # this the orphan watcher auto-cancels the builds 5 min after the
        # rollout killed their SubmitBuild streams.
        rewatch_builds()

    # ════════════════════════════════════════════════════════════════════
    # PHASE 1 (chart deployed flag-off): walk-era state + dormancy
    # ════════════════════════════════════════════════════════════════════
    with subtest("off-phase: walk-era work in flight, zero materialization state"):
        # Finding-18 discriminator (test-artifact suspect): the chart's
        # extraValuesTyped override MUST have deployed BOTH components
        # flag-off. A store that deployed flag-ON would make this phase
        # the AS-6 store-on/scheduler-off state and the later flip-on a
        # no-op store rollout.
        assert_flag_posture("false", "chart-deploy")

        t1a_drv, t1a_out = drv_info("t1a")
        t1b_drv, t1b_out = drv_info("t1b")

        # Blackhole the upstream so the walk-era work cannot complete
        # before the flip: the probes classify indeterminate (B3), the
        # walks spawn, and their fetches keep failing.
        break_upstream()
        assert_upstream_broken()

        b1 = submit_dag("b1", [(t1a_drv, t1a_out), (t1b_drv, t1b_out)])
        watch_build("b1", b1)

        # Both nodes enter the as-built walk path.
        for drv in (t1a_drv, t1b_drv):
            k3s_server.wait_until_succeeds(
                "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
                "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
                f"\"SELECT status FROM derivations WHERE drv_hash = '{drv}'\""
                " | grep -qx substituting",
                timeout=180,
            )
        # Dormancy at flip time (criterion 2).
        assert total_jobs() == 0, "flag-off phase must create no materialization jobs"
        wanted = psql_k8s(k3s_server, "SELECT count(*) FROM build_wanted_outputs")
        assert wanted == "0", f"flag-off phase must write no wanted rows, got {wanted}"
        print("off-phase PASS: walk-era work in flight, zero mat state")

    # ════════════════════════════════════════════════════════════════════
    # FLIP ON (FP-4 b): walk-era state absorbed by the job era
    # ════════════════════════════════════════════════════════════════════
    with subtest("flip-on: walk-era state absorbed, jobs created, build completes"):
        # Make the executor's claim timing scriptable BEFORE the flip
        # (poll-once-per-restart): the new flag-on leader creates jobs
        # mid-rollout, several seconds before set_flag() returns and the
        # upstream heals -- a 1 s-polling executor would claim them
        # against the still-blackholed upstream, charge infra failures,
        # and park them with compounding backoff.
        set_store_poll_interval(3600)

        set_flag(True)

        # The new leader's recovery + dispatch probe absorb the walk-era
        # nodes: no node stays Substituting, jobs are created for them.
        # (The executor is dormant between polls, so nothing has claimed
        # them yet -- the rows sit pending, unparked.)
        try:
            for drv in (t1a_drv, t1b_drv):
                k3s_server.wait_until_succeeds(
                    "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
                    "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
                    "\"SELECT count(*) FROM materialization_jobs"
                    f" WHERE drv_hash = '{drv}'\" | grep -qx 1",
                    timeout=120,
                )
        except Exception:
            dump_mat_diagnostics("flip-on: jobs never created (recovery/probe absorption)")
            raise
        # NOTE on the recovery reset: the new leader resets walk-era
        # Substituting nodes IN MEMORY (the §4 always-on reset) and the
        # dispatch probe then creates their jobs -- which is exactly what
        # the wait above proved (the probe skips Substituting nodes, so
        # jobs existing means the reset happened). The PG status row
        # keeps the stale 'substituting' value until the node's next
        # persisted transition (claim -> running -> completed); asserting
        # on it here would race that lazily-persisted reset.

        # Heal the upstream only now -- AFTER the old (flag-off) era is
        # gone (its walks died with its pods), so the walk path can never
        # complete this build: its completion must come from the job era.
        heal_upstream()

        # Fire exactly one executor claim wave against the healed
        # upstream (fresh pod, fresh probe cache). THE WAVE-3 STALL
        # POINT: with the transport pinned to a standby replica, claims
        # never happened at all; with the abandon-on-UNAVAILABLE fix the
        # wave converges on the leader and the jobs resolve.
        restart_store_deployment()
        try:
            wait_build_status(b1, "succeeded", timeout=300)
        except Exception:
            dump_mat_diagnostics("flip-on: jobs created but build never completed")
            raise
        for drv in (t1a_drv, t1b_drv):
            state = job_state(drv)
            assert state == "resolved_success", (
                f"{drv} job must resolve successfully after the flip, got {state!r}"
            )
            status = drv_status(drv)
            assert status == "completed", f"{drv} must complete, got {status!r}"
        unwatch_build("b1")
        print("flip-on PASS: walk-era state absorbed, build completed via materialization")

    # ════════════════════════════════════════════════════════════════════
    # PHASE 2 (flag-on): marks, pins, and parked-jobs setups
    # ════════════════════════════════════════════════════════════════════
    # Steady-state flag-on operation: short poll interval so the executor
    # claims newly created jobs automatically (the chart's production
    # posture, slightly slowed for VM determinism).
    set_store_poll_interval(5)

    with subtest("flag-on-marks: pruned mark set at merge, cleared by job resolution"):
        markroot_drv, markroot_out = drv_info("markroot")
        markdep_drv, markdep_out = drv_info("markdep")
        blocker_drv, blocker_out = drv_info("blocker")

        # The prunable DAG: root substitutable upstream, dep never
        # published -> the topdown prune fires, drops the dep, marks the
        # root, and creates the origin=pruned job in the merge tx.
        m1 = submit_dag(
            "m1", [(markroot_drv, markroot_out), (markdep_drv, markdep_out)],
            [(markroot_drv, markdep_drv)],
        )
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            "\"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{markroot_drv}' AND origin = 'pruned'\" | grep -qx 1",
            timeout=60,
        )

        # Keep-alive interest: a second build shares the marked node and
        # is blocked on a never-buildable sibling, so the node's PG row
        # outlives the first build's completion and the cleared mark
        # stays observable.
        m2 = submit_dag("m2", [(markroot_drv, markroot_out), (blocker_drv, blocker_out)])
        watch_build("m2", m2)

        # The executor claims the pruned job automatically; its
        # successful resolution runs the §4 clear-mirror.
        wait_build_status(m1, "succeeded", timeout=180)
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            f"\"SELECT topdown_pruned FROM derivations WHERE drv_hash = '{markroot_drv}'\""
            " | grep -qx f",
            timeout=60,
        )
        assert build_status(m2) == "active", "the keep-alive build must stay live"
        print("flag-on-marks PASS: mark stamped at merge, cleared by resolved_success")

    with subtest("flag-on-pins: materialized dep pinned under live interest"):
        pindep_drv, pindep_out = drv_info("pindep")
        pinroot_drv, pinroot_out = drv_info("pinroot")

        p = submit_dag(
            "p", [(pinroot_drv, pinroot_out), (pindep_drv, pindep_out)],
            [(pinroot_drv, pindep_drv)],
        )
        watch_build("p", p)
        # The dep materializes (auto-claimed) and its pin holds while the
        # build stays live (the root waits forever — no Pool exists).
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            f"\"SELECT status FROM derivations WHERE drv_hash = '{pindep_drv}'\""
            " | grep -qx completed",
            timeout=180,
        )
        assert mat_pin_count(pindep_out) >= 1, (
            "pin-at-ingest must hold while the interested build is live"
        )
        assert build_status(p) == "active", "the pinning build must stay live"
        print("flag-on-pins PASS: dep materialized + pinned, build stays active")

    with subtest("flag-on-parked-setup: jobs parked on a broken upstream at flip time"):
        i1_drv, i1_out = drv_info("i1")
        i2_drv, i2_out = drv_info("i2")

        # Break the upstream again: the jobs are created (B3 optimistic
        # creation on indeterminate probes) but every claim fails as
        # infra trouble until the budget parks them. Parked pending rows
        # are exactly the job-era state the OFF flip must leave inert.
        break_upstream()
        assert_upstream_broken()
        i_build = submit_dag("i", [(i1_drv, i1_out), (i2_drv, i2_out)])
        watch_build("i", i_build)
        for drv in (i1_drv, i2_drv):
            k3s_server.wait_until_succeeds(
                "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
                "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
                "\"SELECT count(*) FROM materialization_jobs"
                f" WHERE drv_hash = '{drv}' AND state = 'pending'"
                " AND park_until IS NOT NULL\" | grep -qx 1",
                timeout=180,
            )
        assert build_status(i_build) == "active", (
            "the parked build must stay live (B3: infra trouble never fails a build)"
        )
        print("flag-on-parked-setup PASS: both jobs parked, build still live")

    # ════════════════════════════════════════════════════════════════════
    # FLIP OFF (FP-4 a): job-era state absorbed by the walk era
    # ════════════════════════════════════════════════════════════════════
    with subtest("flip-off: parked jobs inert, walks complete the build, nothing fails"):
        # Make the executor dormant BEFORE healing the upstream: a
        # 5 s-polling executor would claim the parked jobs the moment
        # their backoff expires and complete the build via
        # MATERIALIZATION against the healed upstream — destroying this
        # subtest's premise (the build must complete via the as-built
        # WALK after the flip). Dormant executor + heal + flip is the
        # deterministic ordering; the §4 inertness window opens at the
        # flip itself.
        set_store_poll_interval(3600)
        # Heal the upstream now: nothing flag-on can exploit it (the
        # executor sleeps), and the walk era that the flip starts needs
        # it healthy from its very first probe (the store pod that the
        # OFF-flip rollout creates must never cache a broken answer).
        heal_upstream()

        jobs_at_flip = total_jobs()
        i1_claims = claim_count(i1_drv)
        i2_claims = claim_count(i2_drv)

        set_flag(False)

        # The new (flag-off) leader's recovery + dispatch probe spawn
        # walks for the recovered nodes and the build completes. The
        # walks' first fetches may hit the pre-rollout store pod's
        # poisoned HEAD-probe cache (the parked-setup broke the
        # upstream); the OFF flip's own store rollout replaces it with a
        # fresh-cache pod, and the walk retry ladder absorbs the gap.
        try:
            wait_build_status(i_build, "succeeded", timeout=300)
        except Exception:
            dump_mat_diagnostics("flip-off: walk era never completed the parked jobs' build")
            raise

        # §4 "leftover rows are inert": the parked jobs survived the flip
        # untouched — still pending, never claimed again, never cancelled.
        for drv, claims_before in ((i1_drv, i1_claims), (i2_drv, i2_claims)):
            state = job_state(drv)
            assert state == "pending", (
                f"flag-on-era parked job for {drv} must stay an inert pending row "
                f"after the flip; got {state!r}"
            )
            claims_now = claim_count(drv)
            assert claims_now == claims_before, (
                f"{drv} must not be claimed after the flip "
                f"(claims {claims_before} -> {claims_now})"
            )
        unwatch_build("i")
        # No new materialization rows appeared after the flip.
        assert total_jobs() == jobs_at_flip, (
            f"the flag-off era must create no new jobs: {jobs_at_flip} -> {total_jobs()}"
        )
        # Nothing failed in either era.
        failed = psql_k8s(
            k3s_server, "SELECT count(*) FROM builds WHERE status = 'failed'"
        )
        assert failed == "0", f"no build may fail across the transitions, got {failed}"
        print("flip-off PASS: parked jobs inert, build completed via walks, zero failures")

    with subtest("flip-off-marks: cleared mark does not wrongfully fail-fast a re-submission"):
        # Re-submit the node whose mark was cleared by the flag-on job
        # resolution. Its output is in the store and its DAG row is kept
        # alive by the m2 build, so the merge completes it inline; a
        # stale mark would have poisoned later probes into a fail-fast.
        m3 = submit_dag("m3", [(markroot_drv, markroot_out)])
        wait_build_status(m3, "succeeded", timeout=180)
        marked = psql_k8s(
            k3s_server,
            f"SELECT topdown_pruned FROM derivations WHERE drv_hash = '{markroot_drv}'",
        )
        assert marked == "f", (
            f"the cleared mark must stay cleared across the flip, got {marked!r}"
        )
        print("flip-off-marks PASS: re-submission completed, mark still clear")

    with subtest("flip-off-pins: flag-on-era pins release at terminal interest, then GC collects"):
        # The pin survived the flip (interest is still live).
        assert mat_pin_count(pindep_out) >= 1, (
            "the materialization pin must survive the flag flip while its "
            "interested build is live"
        )
        # Settle the interest FLAG-OFF: the always-on release (§5.3) must
        # fire over the flag-on-era pin rows.
        unwatch_build("p")
        cancel_build(p)
        wait_build_status(p, "cancelled", timeout=60)
        k3s_server.wait_until_succeeds(
            "k3s kubectl -n ${ns} exec rio-postgresql-0 -- "
            "env PGPASSWORD=rio psql -h 127.0.0.1 -U rio rio -qtAc "
            "\"SELECT count(*) FROM scheduler_live_pins"
            " WHERE pin_kind = 'materialization' AND store_path_hash ="
            f" sha256(convert_to('{pindep_out}', 'UTF8'))\" | grep -qx 0",
            timeout=90,
        )
        # And the path is now collectable: expire its grace windows and
        # sweep.
        psql_k8s(
            k3s_server,
            "UPDATE narinfo SET created_at = now() - interval '25 hours'"
            f" WHERE store_path = '{pindep_out}'",
        )
        psql_k8s(
            k3s_server,
            "UPDATE path_tenants SET first_referenced_at = now() - interval '200 hours'"
            f" WHERE store_path_hash = sha256(convert_to('{pindep_out}', 'UTF8'))",
        )
        cli("gc --grace-hours 24")
        assert narinfo_count(pindep_out) == 0, (
            "after the flag-off release the materialized path must be collectable"
        )
        # Cleanup: settle the keep-alive build too.
        unwatch_build("m2")
        cancel_build(m2)
        wait_build_status(m2, "cancelled", timeout=60)
        print("flip-off-pins PASS: pin held across the flip, released at terminal, swept")

    k3s_server.execute("systemctl stop 'dag-*' 'watch-*' 2>/dev/null || true")
    pf_close("pf-sched")
    pf_close("pf-store")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
