# Materialization routing arms, park/backoff, and GC-pin interaction at
# deployment level (substitution-replacement Phase B, design §8-B's
# vm-materialization-{unobtainable,gc-pin}; plan T-3.1).
#
# The substitute scenarios prove the SUCCESS path (cache-hit submission
# completes via materialization jobs). This scenario exercises what
# they cannot: the §2.4 Unobtainable routing arms (fail-fast,
# durable-Vouched from-source), the §2.5 infra park/backoff path, and
# the §5.3 pin lifecycle against a real GC sweep.
#
# Wired once in default.nix as vm-materialization-standalone. Each
# subtest asserts the client-visible outcome triple (build verdict,
# final node statuses, store end-state) through the `assert_outcome`
# helper, plus the per-mechanism job/ledger assertions.
#
# ── The sequence ↔ subtest map ────────────────────────────────────────
#   unobtainable-then-fail-fast   routing-fail-fast
#   unobtainable-then-from-source routing-vouched-from-source
#   infra-retry                   infra-park
#
# ── The mode-switched fake upstream ───────────────────────────────────
# One Python cache server whose narinfo answers are controlled per-path
# by mode files (/srv/cache-mode/<hash>):
#   404        HEAD and GET answer 404  (confirmed-missing)
#   503        HEAD and GET answer 503  (infra trouble, never confirms)
#   head-only  HEAD serves from disk, GET answers 503 (the probe sees
#              the path; the fetch cannot ingest it — how a topdown
#              prune is made to fire for a path that then fails)
#   (absent)   serve from disk (200/404 by file presence)
# This is what lets one VM boot drive every routing arm: the probe-time
# answer and the execution-time answer are independently scriptable.
#
# ── Determinism notes ─────────────────────────────────────────────────
# * Submissions are gRPC-direct WITH a minted tenant JWT: the
#   topdown-prune probe (check_roots_topdown) forwards only the client
#   JWT to the store, so without it the prune (and therefore the
#   marked-root fail-fast pair) is unreachable.
# * tickIntervalSecs=600 (fixture): the dispatch-time re-probe
#   (batch_probe_cached_ready, advanced by housekeeping's
#   probe_generation) never fires inside a subtest window, so the §2.4
#   CONSUMPTION routing is the only decision path for a reported
#   outcome — the mechanism assertions are deterministic, not racing
#   the dispatch-probe cells. Everything the subtests need (job
#   claim/report/consumption, build completion, pin release) is
#   event-driven, not tick-driven.
# * Store executor poll interval = 3600 s (fixture): the executor polls
#   (= claims) only at store startup, so every claim wave is an explicit
#   restart_store() step. This makes claim timing fully scriptable (the
#   upstream can be flipped between probe-time and execution-time with
#   no race), guarantees no claim is in flight when the store restarts
#   (no orphaned open attempts — establishment is tick-driven and the
#   tick is 600 s), and clears the store's 1 h HEAD-probe cache so each
#   wave classifies against the upstream's CURRENT answers.
#
# Markers live at the wiring point (nix/tests/default.nix) per the
# tracey VM convention; this header is prose only.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;

  protoset = import ../lib/protoset.nix { inherit pkgs; };
  grpcurl = "${pkgs.grpcurl}/bin/grpcurl";
  rioCli = "${common.rio-workspace}/bin/rio-cli";

  jwtKeys = import ../lib/jwt-keys.nix;

  # PyJWT for signing the test tenant token (the substitute.nix pattern).
  # The token rides every SubmitBuild as `x-rio-tenant-token`: the
  # scheduler (dev mode) passes the RAW token through to its merge-time
  # store probes, and the topdown-prune probe (check_roots_topdown)
  # forwards ONLY this client JWT — without it the prune can never fire
  # and the marked-root fail-fast sequence is unreachable.
  pyWithJwt = pkgs.python3.withPackages (
    ps: with ps; [
      pyjwt
      cryptography
    ]
  );
  signJwt = pkgs.writeScript "sign-jwt-materialize" ''
    #!${pyWithJwt}/bin/python3
    import sys, time, base64, jwt
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    seed = base64.b64decode("${jwtKeys.seedB64}")
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    now = int(time.time())
    claims = {"sub": sys.argv[1], "iat": now, "exp": now + 3600, "jti": "vm-mat-test"}
    print(jwt.encode(claims, sk, algorithm="EdDSA"))
  '';

  # ── Test derivations ──────────────────────────────────────────────────
  # Eight trivial derivations, one per subtest role. Deps are passed as
  # extra builder args so they land in inputDrvs (real DAG edges) but
  # are NEVER written into $out — each output's narinfo has an empty
  # References list, so publishing one output to the fake cache
  # publishes exactly one path and closure walks stay single-path.
  matClosure = pkgs.writeText "materialize-closure.nix" ''
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
      ffdep       = mk "rio-mat-ffdep" [ ];
      ffroot      = mk "rio-mat-ffroot" [ ffdep ];
      vouchdep    = mk "rio-mat-vouchdep" [ ];
      vouchparent = mk "rio-mat-vouchparent" [ vouchdep ];
      park        = mk "rio-mat-park" [ ];
      gcdep       = mk "rio-mat-gcdep" [ ];
      gcroot      = mk "rio-mat-gcroot" [ gcdep ];
      gcctl       = mk "rio-mat-gcctl" [ ];
    }
  '';
  bbArg = "--arg busybox '(builtins.storePath ${common.busybox})'";

  # ── Mode-switched fake binary cache ───────────────────────────────────
  modeCacheServer = pkgs.writeText "mode-cache-server.py" ''
    """Fake binary cache with per-path scriptable narinfo answers.

    Serves /srv/cache (a `nix copy --to file://` layout). narinfo
    requests consult /srv/cache-mode/<hash> first:
        404        -> HEAD and GET answer 404
        503        -> HEAD and GET answer 503
        head-only  -> HEAD serves from disk, GET answers 503
        (absent)   -> serve from disk
    NAR files and /nix-cache-info are always served from disk.
    """
    import http.server
    import os

    CACHE_DIR = "/srv/cache"
    MODE_DIR = "/srv/cache-mode"


    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=CACHE_DIR, **kwargs)

        def _mode(self):
            name = os.path.basename(self.path)
            if not name.endswith(".narinfo"):
                return None
            try:
                with open(os.path.join(MODE_DIR, name[: -len(".narinfo")])) as f:
                    return f.read().strip()
            except FileNotFoundError:
                return None

        def do_GET(self):
            mode = self._mode()
            if mode == "404":
                self.send_error(404, "narinfo 404 (mode)")
            elif mode in ("503", "head-only"):
                self.send_error(503, "narinfo 503 (mode)")
            else:
                super().do_GET()

        def do_HEAD(self):
            mode = self._mode()
            if mode == "404":
                self.send_error(404, "narinfo 404 (mode)")
            elif mode == "503":
                self.send_error(503, "narinfo 503 (mode)")
            else:
                super().do_HEAD()


    http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
  '';

  # ── Shared prelude: cache, tenant, helpers ────────────────────────────
  # Identical in both flag states. The outcome-assertion helpers ARE the
  # criterion-1 equivalence statement: both branches call the same
  # functions with the same expected values; a mechanism that produces a
  # different client-visible outcome fails here, in whichever branch.
  prelude = ''
    # ════════════════════════════════════════════════════════════════════
    # Fake upstream + tenant + helpers (shared by both flag states)
    # ════════════════════════════════════════════════════════════════════
    SYSTEM = "${pkgs.stdenv.hostPlatform.system}"

    client.succeed("mkdir -p /srv/cache /srv/cache-mode /tmp/mat")
    client.succeed(
        "nix key generate-secret --key-name mat-cache-1 > /tmp/mat/sec && "
        "nix key convert-secret-to-public < /tmp/mat/sec > /tmp/mat/pub"
    )
    test_pubkey = client.succeed("cat /tmp/mat/pub").strip()
    assert test_pubkey.startswith("mat-cache-1:"), f"bad pubkey: {test_pubkey!r}"

    # The mode-switched cache server (see scenario header).
    client.succeed(
        "systemd-run --unit=mat-cache "
        "${pkgs.python3}/bin/python3 ${modeCacheServer}"
    )
    client.wait_for_open_port(8080)

    # nix-cache-info must exist before any probe (nix copy writes it on
    # first publish, but the first PROBE can race the first publish).
    client.succeed(
        "printf 'StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n' "
        "> /srv/cache/nix-cache-info"
    )

    # Positive control: the store host can reach the upstream. Without
    # this every probe/fetch below is vacuously indeterminate.
    ${gatewayHost}.succeed("curl -sf http://client:8080/nix-cache-info | grep -q StoreDir")

    # Tenant whose upstream config drives every probe and fetch here,
    # plus its signed JWT (attached to every SubmitBuild so the
    # scheduler's merge-time probes — including the topdown-prune probe,
    # which forwards only the client JWT — can resolve the tenant's
    # upstreams at the store).
    tid = psql(
        ${gatewayHost},
        "INSERT INTO tenants (tenant_name) VALUES ('mat-tenant') RETURNING tenant_id",
    )
    tenant_jwt = ${gatewayHost}.succeed(f"${signJwt} {tid}").strip()

    def cli(args):
        return ${gatewayHost}.succeed(
            "${common.covShellEnv}"
            "RIO_STORE_ADDR=localhost:9002 "
            "RIO_SCHEDULER_ADDR=localhost:9001 "
            "RIO_SERVICE_HMAC_KEY_PATH=${fixture.hmacKeys}/service-hmac.key "
            f"${rioCli} {args} 2>&1"
        )

    out = cli(
        f"upstream add --tenant {tid} "
        "--url http://client:8080 --priority 50 "
        f"--trusted-key '{test_pubkey}' --sig-mode keep"
    )
    assert "added upstream http://client:8080" in out, out

    # ── Path / mode helpers ─────────────────────────────────────────────
    def hash_part(store_path):
        return store_path.removeprefix("/nix/store/").split("-", 1)[0]

    def set_mode(store_path, mode):
        client.succeed(f"echo {mode} > /srv/cache-mode/{hash_part(store_path)}")

    def clear_mode(store_path):
        client.succeed(f"rm -f /srv/cache-mode/{hash_part(store_path)}")

    def drv_info(attr):
        """Instantiate one attr of the test closure: (drv_path, out_path)."""
        drv = client.succeed(
            "nix-instantiate ${bbArg} ${matClosure} -A " + attr + " 2>/dev/null"
        ).strip()
        out_path = client.succeed(f"nix-store -q --outputs {drv}").strip()
        return drv, out_path

    def build_and_publish(attr):
        """Build one attr locally, sign it, publish it to the fake cache."""
        drv, out_path = drv_info(attr)
        client.succeed(f"nix-store --realise {drv} > /dev/null")
        client.succeed(f"nix store sign --key-file /tmp/mat/sec {out_path}")
        client.succeed(
            "nix copy --no-check-sigs "
            f"--to 'file:///srv/cache?compression=none' {out_path}"
        )
        return drv, out_path

    # ── Submission / build-state helpers ────────────────────────────────
    def submit_dag(unit, node_specs, edge_specs=()):
        """SubmitBuild via gRPC (dev-mode tenant_name resolution). Returns
        the new build's UUID. Submissions are serial in this scenario, so
        newest-row-after-count-bump is race-free."""
        n_before = int(psql(${gatewayHost}, "SELECT count(*) FROM builds"))
        nodes = [
            {"drvPath": drv, "drvHash": drv, "system": SYSTEM,
             "outputNames": ["out"], "expectedOutputPaths": [out_path]}
            for drv, out_path in node_specs
        ]
        edges = [{"parentDrvPath": p, "childDrvPath": c} for p, c in edge_specs]
        payload = json.dumps(
            {"nodes": nodes, "edges": edges, "tenantName": "mat-tenant"}
        )
        ${gatewayHost}.succeed(f"cat > /tmp/dag-{unit}.json <<'EOF'\n{payload}\nEOF")
        ${gatewayHost}.succeed(
            f"systemd-run --unit=dag-{unit} sh -c "
            "'${grpcurl} -plaintext -max-time 600 "
            "-protoset ${protoset}/rio.protoset "
            f"-H \"x-rio-tenant-token: {tenant_jwt}\" "
            "-d @ "
            "localhost:9001 rio.scheduler.SchedulerService/SubmitBuild "
            f"< /tmp/dag-{unit}.json'"
        )
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc 'SELECT count(*) FROM builds'"
            f" | grep -qx {n_before + 1}",
            timeout=60,
        )
        return psql(
            ${gatewayHost},
            "SELECT build_id FROM builds ORDER BY submitted_at DESC LIMIT 1",
        )

    def build_status(build_id):
        return psql(
            ${gatewayHost}, f"SELECT status FROM builds WHERE build_id = '{build_id}'"
        )

    def wait_build_status(build_id, want, timeout=120):
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc "
            f"\"SELECT status FROM builds WHERE build_id = '{build_id}'\""
            f" | grep -qx {want}",
            timeout=timeout,
        )

    def drv_status(drv_path):
        return psql(
            ${gatewayHost},
            f"SELECT status FROM derivations WHERE drv_hash = '{drv_path}'",
        )

    def cancel_build(build_id):
        ${gatewayHost}.succeed(
            "${grpcurl} -plaintext -max-time 30 "
            "-protoset ${protoset}/rio.protoset "
            f'-d \'{{"buildId":"{build_id}"}}\' '
            "localhost:9001 rio.scheduler.SchedulerService/CancelBuild"
        )

    def narinfo_count(store_path):
        return int(psql(
            ${gatewayHost},
            f"SELECT count(*) FROM narinfo WHERE store_path = '{store_path}'",
        ))

    def mat_pin_count(store_path):
        return int(psql(
            ${gatewayHost},
            "SELECT count(*) FROM scheduler_live_pins"
            " WHERE pin_kind = 'materialization'"
            f" AND store_path_hash = sha256(convert_to('{store_path}', 'UTF8'))",
        ))

    def attempt_counts(drv_path):
        """(build-kind executions [deployment-wide], materialization_infra,
        materialization_unobtainable) for one node.

        The first element is the GLOBAL count of build-kind execution
        rows: this fixture has no workers, so any build-kind execution
        anywhere is a routing failure (drv_executions keys on the
        executor-facing hash, not derivation_id, so a per-node join is
        not possible — global is both simpler and stronger here). The
        ledger elements are per-node via drv_attempts, whose
        materialization_* outcome classes ARE the materialization
        ledger (drv_attempts has no kind column)."""
        row = psql(
            ${gatewayHost},
            "SELECT"
            " (SELECT count(*) FROM drv_executions WHERE attempt_kind = 'build'),"
            " (SELECT count(*) FROM drv_attempts a"
            "   JOIN derivations d USING (derivation_id)"
            f"  WHERE d.drv_hash = '{drv_path}'"
            "    AND a.outcome_class = 'materialization_infra'),"
            " (SELECT count(*) FROM drv_attempts a"
            "   JOIN derivations d USING (derivation_id)"
            f"  WHERE d.drv_hash = '{drv_path}'"
            "    AND a.outcome_class = 'materialization_unobtainable')",
        )
        return tuple(int(x) for x in row.split("|"))

    def job_row(drv_path):
        """(state, origin, parked) of the node's materialization job, or None."""
        row = psql(
            ${gatewayHost},
            "SELECT state, origin, (park_until IS NOT NULL)"
            f" FROM materialization_jobs WHERE drv_hash = '{drv_path}'"
            " ORDER BY created_at DESC LIMIT 1",
        )
        if not row:
            return None
        state, origin, parked = row.split("|")
        return state, origin, parked == "t"

    def scheduler_metric(name):
        """Sum a scheduler counter across all label series; 0.0 if absent."""
        return sum(scrape_metrics(${gatewayHost}, 9091).get(name, {}).values())

    def restart_store():
        """Trigger one materialization claim wave (flag-on only).

        The fixture pins the store executor's poll interval to 3600 s, so
        the executor polls — and therefore claims — ONLY at store
        startup. Each claim is an explicit, race-free scenario step: no
        claim can be in flight when this restart fires (the previous
        wave's executions completed and reported before the scenario
        moved on), so no open attempt is ever orphaned mid-execution.
        Restarting also clears the store's in-memory HEAD-probe cache, so
        post-restart classifications see the upstream's CURRENT answers,
        not merge-time ones."""
        ${gatewayHost}.succeed("systemctl restart rio-store.service")
        ${gatewayHost}.wait_for_unit("rio-store.service")
        ${gatewayHost}.wait_for_open_port(9002)

    def gc_collected(grace_hours=24):
        """One GC sweep through AdminService.TriggerGC; returns paths collected."""
        out = cli(f"gc --grace-hours {grace_hours}")
        m = re.search(r"GC complete: (\d+) scanned, (\d+) collected", out)
        assert m, f"rio-cli gc output not parseable: {out!r}"
        return int(m.group(2))

    def backdate_for_gc(store_paths):
        """Expire every GC protection window except pins for the given paths:
        narinfo grace (seed c) and tenant retention (seed f). What remains
        protecting them after this is exactly seeds (d)/(e) — live-build
        roots and scheduler_live_pins."""
        path_list = ", ".join(f"'{p}'" for p in store_paths)
        hash_list = ", ".join(
            f"sha256(convert_to('{p}', 'UTF8'))" for p in store_paths
        )
        psql(
            ${gatewayHost},
            "UPDATE narinfo SET created_at = now() - interval '25 hours'"
            f" WHERE store_path IN ({path_list})",
        )
        psql(
            ${gatewayHost},
            "UPDATE path_tenants SET first_referenced_at = now() - interval '200 hours'"
            f" WHERE store_path_hash IN ({hash_list})",
        )

    # ── Criterion-1 outcome assertions (IDENTICAL text, both branches) ──
    def assert_outcome(tag, build_id, verdict, node_statuses, present, absent):
        """The client-visible outcome triple (equivalence criterion 1):
        build verdict + final node statuses + store end-state. Both flag
        states call this with the SAME expected values for the same
        scripted sequence; the mechanism that produced the outcome is
        asserted separately per branch."""
        actual_verdict = build_status(build_id)
        assert actual_verdict == verdict, (
            f"{tag}: build verdict {actual_verdict!r}, expected {verdict!r}"
        )
        for drv, want in node_statuses.items():
            actual = drv_status(drv)
            assert actual in want, (
                f"{tag}: node {drv} status {actual!r}, expected one of {want!r}"
            )
        for p in present:
            n = narinfo_count(p)
            assert n == 1, f"{tag}: store end-state: {p} should be present, narinfo rows={n}"
        for p in absent:
            n = narinfo_count(p)
            assert n == 0, f"{tag}: store end-state: {p} should be absent, narinfo rows={n}"
        print(f"{tag}: outcome OK (verdict={verdict}, store end-state holds)")

    def assert_failed_with_resubmit_error(tag, build_id):
        """The fail-fast verdict's error class (review eq-7): the shared
        resubmit-directing wrapper format, NOT exact-string equality —
        the cause clause legitimately names the deciding mechanism."""
        err = psql(
            ${gatewayHost},
            f"SELECT error_summary FROM builds WHERE build_id = '{build_id}'",
        )
        assert re.search(r"topdown-pruned root .*resubmit", err), (
            f"{tag}: build error should match the resubmit-directing wrapper "
            f"('topdown-pruned root <hash>: ...; resubmit ...'); got: {err!r}"
        )
        print(f"{tag}: resubmit-directing error format OK")
  '';

  # ════════════════════════════════════════════════════════════════════
  # Subtests: the §2.4 routing arms + §2.5 park + §5.3 pins
  # ════════════════════════════════════════════════════════════════════
  routingSubtests = ''
    # ══════════════════════════════════════════════════════════════════
    # routing-fail-fast — §2.4 arm 3 (Broken + confirmed-missing)
    # ══════════════════════════════════════════════════════════════════
    # A topdown-pruned root (the shape where flag-on and flag-off agree
    # on the fail-fast verdict — the as-built walk only fail-fasts MARKED
    # roots): probe sees the root substitutable (head-only mode) -> the
    # prune drops the dep + marks the root + creates an origin=pruned job
    # in the merge tx -> the upstream flips to 404 -> restart_store()
    # triggers the one claim wave -> the executor confirms the path
    # missing -> Unobtainable -> consumption re-probe confirms -> arm 3
    # fail-fast. Exactly one claim, exactly one unobtainable charge row.
    with subtest("routing-fail-fast: confirmed-missing pruned root fails the build (arm 3)"):
        ff_root_drv, ff_root_out = build_and_publish("ffroot")
        ff_dep_drv, ff_dep_out = drv_info("ffdep")
        set_mode(ff_root_out, "head-only")

        ff_build = submit_dag(
            "ff", [(ff_root_drv, ff_root_out), (ff_dep_drv, ff_dep_out)],
            [(ff_root_drv, ff_dep_drv)],
        )

        # The merge transaction created the pruned-origin job (the in-tx
        # creation) and dropped the dep from the DAG.
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{ff_root_drv}' AND origin = 'pruned'\" | grep -qx 1",
            timeout=60,
        )
        dep_rows = psql(
            ${gatewayHost},
            f"SELECT count(*) FROM derivations WHERE drv_hash = '{ff_dep_drv}'",
        )
        assert dep_rows == "0", (
            f"the topdown prune must drop the dep from the DAG; found {dep_rows} row(s)"
        )

        # Flip the world: the path is now confirmed-missing upstream.
        # The claim wave that follows sees the 404 (the restart also
        # cleared the merge-time HEAD-probe cache entry).
        set_mode(ff_root_out, "404")
        restart_store()

        # Arm 3: the executor reports Unobtainable, the consumption
        # re-probe confirms missing, every live interested build fails
        # with the resubmit-directing error.
        wait_build_status(ff_build, "failed", timeout=120)
        assert_failed_with_resubmit_error("routing-fail-fast", ff_build)

        # Mechanism: the job resolved through the consumption routing
        # (resolved_unobtainable), exactly one unobtainable charge row,
        # zero infra rows (the single claim wave saw the 404, never a
        # 503), zero build-kind rows (the failure never touched build
        # budgets).
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT state FROM materialization_jobs"
            f" WHERE drv_hash = '{ff_root_drv}'\" | grep -qx resolved_unobtainable",
            timeout=30,
        )
        builds_kind, infra, unobtainable = attempt_counts(ff_root_drv)
        assert builds_kind == 0, (
            f"fail-fast must not touch build budgets: {builds_kind} build-kind row(s)"
        )
        assert unobtainable == 1 and infra == 0, (
            f"expected exactly 1 materialization_unobtainable charge row and 0 infra rows; "
            f"got unobtainable={unobtainable} infra={infra}"
        )

        # Client-visible outcome triple (criterion 1 — same values the
        # walk oracle asserts for this sequence).
        assert_outcome(
            "routing-fail-fast", ff_build,
            verdict="failed",
            node_statuses={},
            present=[], absent=[ff_root_out],
        )

    # ══════════════════════════════════════════════════════════════════
    # routing-vouched-from-source — §2.4 arm 1 (durable Vouched)
    # ══════════════════════════════════════════════════════════════════
    # A parent whose only dep is already produced: Unobtainable on the
    # parent routes ResolveFromSource (never fail-fast) — the node
    # becomes from-source dispatchable and the build keeps waiting for a
    # builder.
    with subtest("routing-vouched-from-source: produced deps route from-source (arm 1)"):
        vouch_dep_drv, vouch_dep_out = build_and_publish("vouchdep")
        vouch_parent_drv, vouch_parent_out = drv_info("vouchparent")

        # Step 1: materialize the dep via its own 1-node build (this is
        # also the basic Success-arm proof for this scenario: job ->
        # claim -> ingest -> consumption -> Completed -> build succeeded).
        dep_build = submit_dag("vouchdep", [(vouch_dep_drv, vouch_dep_out)])
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{vouch_dep_drv}'\" | grep -qx 1",
            timeout=60,
        )
        restart_store()
        wait_build_status(dep_build, "succeeded", timeout=120)
        dep_job = job_row(vouch_dep_drv)
        assert dep_job and dep_job[0] == "resolved_success", (
            f"the dep's job must consume as resolved_success; got {dep_job!r}"
        )

        # Step 2: the parent probes indeterminate (503 -> job created,
        # B3's optimistic creation), then the upstream confirms 404.
        set_mode(vouch_parent_out, "503")
        vouch_build = submit_dag(
            "vouch", [(vouch_parent_drv, vouch_parent_out), (vouch_dep_drv, vouch_dep_out)],
            [(vouch_parent_drv, vouch_dep_drv)],
        )
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{vouch_parent_drv}' AND state = 'pending'\" | grep -qx 1",
            timeout=60,
        )
        set_mode(vouch_parent_out, "404")
        restart_store()

        # Arm 1: dep is produced (Vouched closure evidence) -> the job
        # resolves from-source instead of failing the build.
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT state FROM materialization_jobs"
            f" WHERE drv_hash = '{vouch_parent_drv}'\" | grep -qx resolved_from_source",
            timeout=120,
        )
        builds_kind, _, _ = attempt_counts(vouch_parent_drv)
        assert builds_kind == 0, (
            f"no builder exists in this fixture; build-kind rows must be 0, got {builds_kind}"
        )

        # Client-visible outcome triple: build still waiting (active),
        # parent from-source eligible (ready/queued), dep present, parent
        # absent — same values the walk oracle asserts.
        assert_outcome(
            "routing-vouched-from-source", vouch_build,
            verdict="active",
            node_statuses={
                vouch_parent_drv: ("ready", "queued"),
                vouch_dep_drv: ("completed",),
            },
            present=[vouch_dep_out], absent=[vouch_parent_out],
        )

        # Cleanup so later subtests' build-table assertions stay scoped.
        cancel_build(vouch_build)
        wait_build_status(vouch_build, "cancelled", timeout=60)

    # ══════════════════════════════════════════════════════════════════
    # infra-park — §2.5 (budget exhaustion parks; never fails)
    # ══════════════════════════════════════════════════════════════════
    with subtest("infra-park: infra failures park the job and the build never fails (B3)"):
        park_drv, park_out = build_and_publish("park")
        set_mode(park_out, "503")

        park_build = submit_dag("park", [(park_drv, park_out)])
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{park_drv}'\" | grep -qx 1",
            timeout=60,
        )

        # Burn the materialization budget (max_attempts=3, the default):
        # each claim wave hits the 503 upstream -> InfraFailure. ONE
        # restart can charge MORE than one infra row: the executor runs
        # 8 concurrent claim workers, and a worker whose claim lands
        # after another worker's full claim->report cycle (the 503
        # answer is local and sub-millisecond) mints a FRESH attempt
        # against the re-armed job instead of the open attempt's
        # re-delivery. The budget cap still holds (one open attempt at
        # a time; the park fires at exactly max_attempts charges), but
        # the per-wave count can advance by 2 — so wait for MONOTONE
        # progress (>= wave) and stop as soon as the park lands. The
        # build stays live the whole way (B3: infra trouble is never
        # confirmation, never a fail-fast, never a from-source route).
        for wave in (1, 2, 3):
            restart_store()
            ${gatewayHost}.wait_until_succeeds(
                "test \"$(sudo -u postgres psql rio -qtAc \"SELECT"
                " count(*) FILTER (WHERE a.outcome_class = 'materialization_infra')"
                " FROM drv_attempts a JOIN derivations d USING (derivation_id)"
                f" WHERE d.drv_hash = '{park_drv}'\")\" -ge {wave}",
                timeout=60,
            )
            already_parked = psql(
                ${gatewayHost},
                "SELECT count(*) FROM materialization_jobs"
                f" WHERE drv_hash = '{park_drv}' AND park_until IS NOT NULL",
            )
            if already_parked == "1":
                break
        # Budget exhausted -> parked: pending + park_until set, claimable
        # again after the backoff (5 s in this fixture).
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{park_drv}' AND state = 'pending'"
            " AND park_until IS NOT NULL\" | grep -qx 1",
            timeout=60,
        )
        parked = psql(
            ${gatewayHost},
            "SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{park_drv}' AND state = 'pending'"
            " AND park_until IS NOT NULL",
        )
        assert parked == "1", f"the job must park after the third infra failure: {parked}"
        assert build_status(park_build) == "active", (
            f"the park must never fail the build; got {build_status(park_build)!r}"
        )
        builds_kind, infra, unobtainable = attempt_counts(park_drv)
        assert builds_kind == 0 and unobtainable == 0 and infra == 3, (
            f"park budget: expected exactly 3 materialization_infra rows and nothing else; "
            f"got build-kind={builds_kind} infra={infra} unobtainable={unobtainable}"
        )

        # Heal the upstream, let the park backoff expire, then trigger
        # the claim wave that succeeds.
        clear_mode(park_out)
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{park_drv}' AND park_until <= now()\" | grep -qx 1",
            timeout=30,
        )
        restart_store()
        wait_build_status(park_build, "succeeded", timeout=120)
        park_job = job_row(park_drv)
        assert park_job and park_job[0] == "resolved_success", (
            f"after healing, the parked job must resolve successfully; got {park_job!r}"
        )

        # Client-visible outcome triple: same values the walk oracle
        # asserts for the infra-retry sequence.
        assert_outcome(
            "infra-park", park_build,
            verdict="succeeded",
            node_statuses={park_drv: ("completed",)},
            present=[park_out], absent=[],
        )

    # ══════════════════════════════════════════════════════════════════
    # gc-pin — §5.3 (pin-at-ingest holds until all interest is terminal)
    # ══════════════════════════════════════════════════════════════════
    with subtest("gc-pin: materialization pins block GC until interest settles (B2-strong)"):
        gc_dep_drv, gc_dep_out = build_and_publish("gcdep")
        gc_root_drv, gc_root_out = drv_info("gcroot")
        gc_ctl_drv, gc_ctl_out = build_and_publish("gcctl")

        # Control path: materialized by its own build, which SUCCEEDS ->
        # all interest terminal -> its pins release (the §5.3 site-ii
        # release). It is the unpinned victim that proves the GC sweep
        # actually runs.
        ctl_build = submit_dag("gcctl", [(gc_ctl_drv, gc_ctl_out)])
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{gc_ctl_drv}'\" | grep -qx 1",
            timeout=60,
        )
        restart_store()
        wait_build_status(ctl_build, "succeeded", timeout=120)
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM scheduler_live_pins"
            " WHERE pin_kind = 'materialization' AND store_path_hash ="
            f" sha256(convert_to('{gc_ctl_out}', 'UTF8'))\" | grep -qx 0",
            timeout=60,
        )

        # Pinned path: materialized as the dep of a build that stays
        # ACTIVE (its root waits for a builder that never comes), so the
        # dep's pin must hold.
        gcpin_build = submit_dag(
            "gcpin", [(gc_root_drv, gc_root_out), (gc_dep_drv, gc_dep_out)],
            [(gc_root_drv, gc_dep_drv)],
        )
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM materialization_jobs"
            f" WHERE drv_hash = '{gc_dep_drv}'\" | grep -qx 1",
            timeout=60,
        )
        restart_store()
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT status FROM derivations"
            f" WHERE drv_hash = '{gc_dep_drv}'\" | grep -qx completed",
            timeout=120,
        )
        assert drv_status(gc_root_drv) in ("ready", "queued"), (
            f"the root must wait for a builder, got {drv_status(gc_root_drv)!r}"
        )
        assert build_status(gcpin_build) == "active", "the pinning build must stay live"
        assert mat_pin_count(gc_dep_out) >= 1, (
            "pin-at-ingest: the materialized dep must carry a "
            "pin_kind='materialization' row while its build is live"
        )

        # GC sweep #1: only pins/live-roots may protect the two
        # materialized paths now.
        backdate_for_gc([gc_dep_out, gc_ctl_out])
        collected = gc_collected(grace_hours=24)
        assert collected == 1, (
            f"GC #1 must collect exactly the unpinned control path, got {collected}"
        )
        assert narinfo_count(gc_ctl_out) == 0, "the unpinned control path must be swept"
        assert narinfo_count(gc_dep_out) == 1, (
            "the §5.3 pin must protect the materialized dep while interest is live"
        )

        # Settle the interest: cancel the pinning build -> T-1.8's
        # build-terminal release fires -> the pin disappears.
        cancel_build(gcpin_build)
        wait_build_status(gcpin_build, "cancelled", timeout=60)
        ${gatewayHost}.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc \"SELECT count(*) FROM scheduler_live_pins"
            " WHERE pin_kind = 'materialization' AND store_path_hash ="
            f" sha256(convert_to('{gc_dep_out}', 'UTF8'))\" | grep -qx 0",
            timeout=60,
        )

        # GC sweep #2: nothing protects the dep any more.
        backdate_for_gc([gc_dep_out])
        collected = gc_collected(grace_hours=24)
        assert collected == 1, (
            f"GC #2 must collect the now-unpinned dep, got {collected}"
        )
        assert narinfo_count(gc_dep_out) == 0, (
            "after the all-interest-terminal release the dep must be collectable"
        )
        print("gc-pin PASS: pin held under live interest, released at terminal, then swept")

    # ══════════════════════════════════════════════════════════════════
    # Flag-on deployment posture (criterion 3 is structural now: the
    # walk spawner and its rio_scheduler_substitute_spawned_total
    # metric were deleted — there is no walk to observe)
    # ══════════════════════════════════════════════════════════════════
    with subtest("materialize-no-walks: flag-on posture in both units"):
        for unit in ["rio-scheduler", "rio-store"]:
            env = ${gatewayHost}.succeed(f"systemctl show {unit} --property=Environment")
            assert "RIO_MATERIALIZATION__ENABLED=true" in env, f"{unit} not flag-on: {env}"
        print("materialize-no-walks PASS: flags on (walk spawner structurally absent)")
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-materialize";
  skipTypeCheck = true;

  # ~60 s boot + 4 subtests (fail-fast ~30 s, vouched ~40 s, park ~60 s
  # [3 budget-burning claim waves + backoff], gc-pin ~60 s [2 claim
  # waves + 2 sweeps]); each claim wave is a ~5 s store restart.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
    }}

    ${prelude}
    ${routingSubtests}

    # Stop the detached submission units + the cache server so the test
    # driver's shutdown isn't blocked on streaming grpcurl processes.
    ${gatewayHost}.execute("systemctl stop 'dag-*' 2>/dev/null || true")
    client.execute("systemctl stop mat-cache 2>/dev/null || true")

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
