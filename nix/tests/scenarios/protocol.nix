# Protocol scenario: real nix client against rio gateway, cold or warm store.
#
# The `cold=true` variant is the regression test for 3 of the 4 bugs from
# 5786f82 ("smoke test has never passed on a cold store"):
#
#   wopQueryMissing `!out` — assert_set_eq on willBuild catches the extra
#     DerivedPath suffix; the old unit test used .contains() which masked it.
#
#   system="builtin" — cold-bootstrap's busybox FOD has system=builtin;
#     if workers don't advertise it, can_build() is always false and the
#     DAG leaf never dispatches. Build-completes assertion catches this.
#
#   BuildResult builtOutputs outPath basename — opcode 47
#     (wopBuildPathsWithResults). Build succeeding requires the client to
#     parse BuildResult, whose builtOutputs[].outPath is a basename.
#     Full-path → "illegal base-32 char '/'" → build fails.
#     NOT opcode 43 (wopQueryRealisation) — see phase4a §1.6 golden test.
#
# The `cold=false` variant subsumes phase1a (read-only opcodes, narHash/
# narSize exact equality) + phase1b (single trivial build) + cache-hit.
{
  pkgs,
  common,
  fixture,
  cold ? false,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Warm-path derivation: distinct marker so it doesn't DAG-dedup with
  # any other scenario's builds.
  trivialDrv = drvs.mkTrivial { marker = "proto-warm"; };

  name = if cold then "protocol-cold" else "protocol-warm";

  # Coverage mode: graceful-stop + profraw tar + copy_from_vm adds ~30s.
  # protocol-warm entered collectCoverage at 287s with a 300s budget.
  covTimeoutHeadroom = if common.coverage then 300 else 0;

  coldScript = ''
    # ══════════════════════════════════════════════════════════════════
    # COLD PATH: empty store, builtin:fetchurl leaf, exact willBuild set
    # ══════════════════════════════════════════════════════════════════

    # Do NOT seedBusybox. The store must be empty so wopQueryMissing
    # returns a non-trivial willBuild set.

    # Workers are airgapped. drvs.coldBootstrapServer (passed via
    # extraClientModules in nix/tests/default.nix) serves the pre-
    # fetched busybox on client:8000. builtin:fetchurl does a real
    # HTTP fetch — same codepath as EKS, just to a VM-local endpoint.
    client.wait_for_unit("busybox-http.service")
    client.wait_for_open_port(8000)

    # Instantiate on the CLIENT, copy the .drv closure (not outputs).
    # --impure: builtins.currentSystem in cold-bootstrap.nix.
    drv = client.succeed(
        "nix-instantiate --impure --argstr tag smoke "
        "--argstr url 'http://client:8000/busybox' "
        "${drvs.coldBootstrap} 2>&1 | tail -1"
    ).strip()
    assert drv.endswith(".drv"), f"expected .drv path, got {drv!r}"

    # The FOD .drv is an inputDrv of the consumer. Discover it from
    # nix-store -q --references (exactly one .drv ref: the busybox FOD).
    deps = client.succeed(f"nix-store -q --references {drv}").strip().split()
    drv_deps = [d for d in deps if d.endswith(".drv")]
    assert len(drv_deps) == 1, f"expected exactly 1 .drv inputDrv, got {drv_deps!r}"
    busybox_fod_drv = drv_deps[0]

    # Copy .drv closure to rio (wopAddToStoreNar for each .drv + sources).
    client.succeed(
        f"nix copy --no-check-sigs --derivation --to '{store_url}' {drv}"
    )

    with subtest("cold build from empty store (catches 3/4 5786f82 bugs)"):
        # wopQueryMissing fires INSIDE nix build (precursor to dispatch),
        # not as a separately-observable dry-run. All three bugs manifest
        # as build failures:
        #
        #   wopQueryMissing `!out` → client fails StorePath parse on '!'
        #     before dispatch. Stderr: "invalid character in store path".
        #
        #   system="builtin" not advertised → busybox FOD can_build()
        #     always false → DAG leaf never dispatches → hang (globalTimeout).
        #
        #   BuildResult builtOutputs outPath full-path (opcode 47) →
        #     client's BuildResult parser rejects with "illegal base-32
        #     character '/'" after build.
        #
        # Build succeeding = all three absent. Capture stderr to make any
        # failure mode diagnosable from CI logs.
        result = client.succeed(
            f"nix build --no-link --print-out-paths --store '{store_url}' "
            f"'{drv}^*' 2>&1"
        )
        # Last non-empty line is the output path (earlier lines are
        # build progress to stderr via 2>&1).
        lines = [l.strip() for l in result.strip().splitlines() if l.strip()]
        assert lines, f"empty build output: {result!r}"
        out = lines[-1]
        assert out.startswith("/nix/store/"), (
            f"expected store path, got {out!r}\nfull output:\n{result}"
        )
        assert "rio-cold-smoke" in out, f"wrong output name: {out!r}"
        # Proves wopQueryMissing returned a well-formed StorePathSet: if
        # it had `!out` suffixes, we'd never get here (client aborts at
        # parse, pre-dispatch). busybox_fod_drv is unused after this
        # point but kept above — asserting the .drv closure shape
        # (exactly 1 inputDrv) is independently valuable.
        _ = busybox_fod_drv

    with subtest("output round-trips (BuildResult builtOutputs parsed)"):
        # Opcode 47 coverage, NOT opcode 43. The build succeeding requires
        # the client's BuildResult parser to accept builtOutputs[].outPath
        # as a basename. Opcode 43 (wopQueryRealisation) has wire-level
        # coverage in ca_roundtrip.rs and wire_opcodes/opcodes_read.rs;
        # TODO(phase4b): golden conformance test against live nix-daemon.
        # Prove the output is also
        # queryable — a separate opcode path (wopQueryPathInfo) that
        # goes through the store.
        info = client.succeed(
            f"nix path-info --json --store '{store_url}' {out}"
        )
        # `echo ok > $out` → output is exactly "ok\n" → 3 bytes → fixed
        # narSize (NAR framing adds ~100 bytes). Not asserting exact size
        # (NAR format version dependent) but >0 proves the path registered.
        import json as _json
        parsed = _json.loads(info)
        assert parsed[out]["narSize"] > 0, f"narSize=0 for {out!r}: {info}"

    with subtest("metric accounting (per-submit vs per-dispatch)"):
        # scheduler_builds_total is per-SubmitBuild RPC: one submit,
        # one count — the whole DAG succeeded. worker_builds_total is
        # per-derivation-dispatched: FOD + consumer = 2.
        assert_metric_exact(
            ${gatewayHost}, 9091,
            "rio_scheduler_builds_total", 1.0,
            labels='{outcome="success"}',
        )
        assert_metric_exact(
            worker, 9093,
            "rio_worker_builds_total", 2.0,
            labels='{outcome="success"}',
        )
  '';

  warmScript = ''
    # ══════════════════════════════════════════════════════════════════
    # WARM PATH: phase1a read opcodes + phase1b trivial build + cache-hit
    # ══════════════════════════════════════════════════════════════════

    ${common.seedBusybox gatewayHost}

    # ── phase1a: wopQueryPathInfo exact narHash/narSize ──────────────
    with subtest("path-info exact narHash/narSize (vs local ground truth)"):
        path_info = client.succeed(
            f"nix path-info --store '{store_url}' ${common.busybox}"
        ).strip()
        assert path_info == "${common.busybox}", (
            f"path-info returned {path_info!r}, expected busybox path"
        )

        # Ground truth from client's LOCAL store.
        import json as _json
        local = _json.loads(client.succeed(
            "nix path-info --json ${common.busybox}"
        ))["${common.busybox}"]
        gw = _json.loads(client.succeed(
            f"nix path-info --json --store '{store_url}' ${common.busybox}"
        ))["${common.busybox}"]
        # Exact. If gateway returns a different hash/size, the
        # wopQueryPathInfo handler is corrupting data.
        assert gw["narHash"] == local["narHash"], (
            f"narHash MISMATCH: gw={gw['narHash']!r} local={local['narHash']!r}"
        )
        assert gw["narSize"] == local["narSize"], (
            f"narSize MISMATCH: gw={gw['narSize']} local={local['narSize']}"
        )

    # ── phase1a: wopNarFromPath ──────────────────────────────────────
    with subtest("store ls (wopNarFromPath parses directory)"):
        ls_output = client.succeed(
            f"nix store ls --store '{store_url}' ${common.busybox}/bin"
        )
        assert "busybox" in ls_output, f"missing busybox binary: {ls_output!r}"

    # ── phase1a: wopIsValidPath ──────────────────────────────────────
    with subtest("store verify (wopIsValidPath)"):
        # --no-trust skips signature checks; --no-contents skips NAR
        # hash recomputation. What's left: wopIsValidPath for the path.
        client.succeed(
            f"nix store verify --no-trust --store '{store_url}' ${common.busybox}"
        )

    # ── phase1a: negative path ───────────────────────────────────────
    with subtest("nonexistent path: clean error, no hang"):
        client.fail(
            f"nix path-info --store '{store_url}' "
            "/nix/store/0000000000000000000000000000000a-nonexistent"
        )

    # ── phase1b: single trivial build ────────────────────────────────
    with subtest("trivial build end-to-end"):
        out = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${trivialDrv}"
        ).strip()
        assert out.startswith("/nix/store/"), f"unexpected output: {out!r}"
        assert "rio-test-proto-warm" in out, f"wrong drv name: {out!r}"
        # Output queryable (round-trips through store).
        client.succeed(f"nix path-info --store '{store_url}' {out}")
        # Exactly one build succeeded. baseline-delta not needed here —
        # this is the first build in the test.
        assert_metric_exact(
            ${gatewayHost}, 9091,
            "rio_scheduler_builds_total", 1.0,
            labels='{outcome="success"}',
        )

    # ── cache hit on rebuild ─────────────────────────────────────────
    with subtest("rebuild hits scheduler cache"):
        before = scrape_metrics(${gatewayHost}, 9091)
        # Same expression → same .drv → cache hit.
        out2 = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${trivialDrv}"
        ).strip()
        assert out2 == out, f"cache hit should return same path: {out2!r} != {out!r}"
        after = scrape_metrics(${gatewayHost}, 9091)
        # out2 == out (above) proves same path returned. cache_hits
        # corroborates. scheduler_builds_total still increments —
        # SubmitBuild RPC was handled; cache check IS the handling.
        #
        # source="existing": DAG node already in Completed state from
        # the first build (merge.rs:283). source="scheduler" would fire
        # if the store had the output but the scheduler's DAG didn't
        # (e.g., resubmit after scheduler restart).
        assert_metric_delta(
            before, after,
            "rio_scheduler_cache_hits_total", 1.0,
            labels='{source="existing"}',
        )
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-${name}";
  skipTypeCheck = true;
  # Cold: builtin:fetchurl is a real network fetch inside the sandbox
  # (FOD). ~60s boot + ~30s fetchurl + build + assertions.
  globalTimeout = (if cold then 600 else 300) + covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}
    ${common.sshKeySetup gatewayHost}

    store_url = "ssh-ng://${gatewayHost}"

    with subtest("ssh-ng handshake (magic exchange, version, STDERR_LAST)"):
        client.succeed(f"nix store info --store '{store_url}'")

    ${if cold then coldScript else warmScript}

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
