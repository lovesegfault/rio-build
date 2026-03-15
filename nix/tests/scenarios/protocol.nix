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
#   Realisation outPath basename — build succeeding at all requires the
#     client to parse BuildResult (which contains the Realisation JSON).
#     Full-path outPath → "illegal base-32 char '/'" → build fails.
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
  coldScript = ''
    # ══════════════════════════════════════════════════════════════════
    # COLD PATH: empty store, builtin:fetchurl leaf, exact willBuild set
    # ══════════════════════════════════════════════════════════════════

    # Do NOT seedBusybox. The store must be empty so wopQueryMissing
    # returns a non-trivial willBuild set.

    # Instantiate on the CLIENT, copy the .drv closure (not outputs).
    # --impure: builtins.currentSystem in cold-bootstrap.nix.
    drv = client.succeed(
        "nix-instantiate --impure --argstr tag smoke "
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

    with subtest("wopQueryMissing: willBuild is an exact StorePathSet"):
        # --dry-run triggers wopQueryMissing. Output format:
        #   these N derivations will be built:
        #     /nix/store/...-busybox.drv
        #     /nix/store/...-rio-cold-smoke.drv
        # Parse lines that start with /nix/store/ and end .drv.
        dry = client.succeed(
            f"nix build --dry-run --store '{store_url}' '{drv}^*' 2>&1"
        )
        will_build = {
            line.strip()
            for line in dry.splitlines()
            if line.strip().startswith("/nix/store/") and line.strip().endswith(".drv")
        }
        # THE assertion that catches the `!out` bug (5786f82). If
        # willBuild contains `...drv!out` (DerivedPath syntax), the set
        # won't match and assert_set_eq shows it in `extra:`.
        assert_set_eq(
            will_build,
            {drv, busybox_fod_drv},
            context="wopQueryMissing willBuild",
        )

    with subtest("cold build completes (system=builtin advertised)"):
        # If workers don't advertise builtin, can_build() is always false
        # for the busybox FOD → permanently undispatchable → this hangs.
        # globalTimeout caps the hang; the assertion below gives a precise
        # error if it partially ran.
        out = client.succeed(
            f"nix build --no-link --print-out-paths --store '{store_url}' "
            f"'{drv}^*' 2>&1"
        ).strip().splitlines()[-1]
        assert out.startswith("/nix/store/"), f"expected store path, got {out!r}"
        assert "rio-cold-smoke" in out, f"wrong output name: {out!r}"

    with subtest("output round-trips (Realisation outPath basename parsed)"):
        # The build succeeding at all requires the client's BuildResult
        # parser to accept the Realisation JSON. But prove the output is
        # also queryable — a separate opcode path (wopQueryPathInfo) that
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

    with subtest("scheduler recorded both builds"):
        # Exact count: 1 FOD + 1 consumer = 2. Not `[1-9]`.
        assert_metric_exact(
            ${gatewayHost}, 9091,
            "rio_scheduler_builds_total", 2.0,
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
  # Cold: builtin:fetchurl is a real network fetch inside the sandbox
  # (FOD). ~60s boot + ~30s fetchurl + build + assertions.
  globalTimeout = if cold then 600 else 300;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

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
