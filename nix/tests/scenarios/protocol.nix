# Protocol scenario: real nix client against rio gateway, cold or warm store.
#
# The `cold=true` variant is the regression test for 3 of the 4 bugs from
# 5786f82 ("smoke test has never passed on a cold store"):
#
#   wopQueryMissing `!out` — assert_set_eq on willBuild catches the extra
#     DerivedPath suffix; the old unit test used .contains() which masked it.
#
#   system="builtin" — cold-bootstrap's busybox FOD has system=builtin;
#     if workers don't advertise it, hard_filter() rejects every executor
#     and the DAG leaf never dispatches. Build-completes assertion catches this.
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
  # Exercise `r[gw.conn.exit-status+3]`: client has nix-output-monitor +
  # ControlMaster ssh config; subtest asserts `nom build` exits within
  # the timeout (vs hanging until ControlPersist) and gateway-side
  # `connections_active` returns to 0. Only the CppNix warm variant
  # opts in — Lix's nom compatibility isn't load-bearing for this fix.
  withNomExitTest ? false,
  nameSuffix ? "",
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # Warm-path derivation: distinct marker so it doesn't DAG-dedup with
  # any other scenario's builds.
  trivialDrv = drvs.mkTrivial { marker = "proto-warm"; };
  # Built ONLY via build-hook mode (--max-jobs 0 --builders), so the
  # hook subtest exercises a real dispatch (not a scheduler cache hit).
  hookDrv = drvs.mkTrivial { marker = "proto-hook"; };
  # Fixed-output derivation built ONLY via build-hook mode and never
  # copied as a .drv: build-remote sends it inline (wopBuildDerivation,
  # content-bound single-node fallback), so the gateway must carry the
  # serialized derivation in the submission for the fetcher to execute
  # (gw.hook.inline-drv-content). echo appends a newline — the declared
  # hash is over "rio hook inline fod\n".
  hookFodContent = "rio hook inline fod\n";
  hookFodDrv = drvs.mkCustom {
    name = "rio-proto-hook-inline-fod";
    script = ''
      echo 'rio hook inline fod' > $out
    '';
    extraAttrs = {
      outputHashMode = "flat";
      outputHashAlgo = "sha256";
      outputHash = builtins.hashString "sha256" hookFodContent;
    };
  };

  # Floating-CA derivation built ONLY via build-hook mode: build-remote
  # sends any CA derivation inline (wopBuildDerivation) and never copies
  # the .drv, so this exercises the content-bound fallback end to end —
  # including the consumable result: the gateway must return builtOutputs
  # keyed by the modular hash of the inline derivation and the realized
  # path must be registered (gw.hook.fallback-built-outputs).
  hookCaDrv = drvs.mkCustom {
    name = "rio-proto-hook-inline-ca";
    script = ''
      echo 'rio hook inline ca' > $out
    '';
    extraAttrs = {
      __contentAddressed = true;
      outputHashAlgo = "sha256";
      outputHashMode = "recursive";
    };
  };

  # Result-pipeline probes (run through the real builder → upload →
  # store path; the differential harness preps its own build dir and
  # never uploads, so these two properties need a production-path
  # scenario):
  #  - tmpdirProbeDrv: writes to $TMPDIR before producing $out — fails
  #    EACCES if the sandbox's /build is not writable for the build
  #    user.
  #  - strayProbeDrv: writes a stray store path next to $out — the
  #    stray must never be registered in the rio store.
  tmpdirProbeDrv = drvs.mkCustom {
    name = "rio-test-proto-tmpdir";
    script = ''
      set -e
      echo "tmpdir scratch" > "$TMPDIR/scratch-file"
      ''${busybox}/bin/cat "$TMPDIR/scratch-file" > $out
    '';
  };
  inherit (drvs) ergOnDrv;
  strayProbeDrv = drvs.mkCustom {
    name = "rio-test-proto-stray";
    script = ''
      set -e
      ''${busybox}/bin/mkdir -p /nix/store/cccccccccccccccccccccccccccccccc-rio-proto-stray
      echo leftover > /nix/store/cccccccccccccccccccccccccccccccc-rio-proto-stray/file
      echo "real output" > $out
    '';
  };

  name = if cold then "protocol-cold" else "protocol-warm";

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
        #   system="builtin" not advertised → busybox FOD rejected by
        #     hard_filter() → DAG leaf never dispatches → hang (globalTimeout).
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
        assert _path_info_one(info, out)["narSize"] > 0, f"narSize=0 for {out!r}: {info}"

    with subtest("metric accounting (per-submit vs per-dispatch + FOD routing)"):
        # scheduler_builds_total is per-SubmitBuild RPC: one submit,
        # one count — the whole DAG succeeded. builder_builds_total is
        # per-derivation-dispatched. P0452 hard-split routes the FOD
        # to the fetcher and the consumer to the builder — 1 each.
        # r[verify sched.dispatch.fod-to-fetcher]
        assert_metric_exact(
            ${gatewayHost}, 9091,
            "rio_scheduler_builds_total", 1.0,
            labels='{outcome="success"}',
        )
        # rio-builder is one-shot — per-process counter resets across
        # systemd restarts. journald is the persistent signal.
        nb = journal_builds_succeeded(worker)
        assert nb == 1, f"worker journald shows {nb} builds, expected 1"
        nf = journal_builds_succeeded(fetcher)
        assert nf == 1, f"fetcher journald shows {nf} builds, expected 1"

    # ── hook-mode FOD: inline derivation carried in the submission ─────
    # Lives in the COLD variant only: FODs hard-split-route to the
    # fetcher pool (P0452), and only this fixture provisions one — in
    # the warm fixture the build would sit unroutable until the global
    # timeout. Runs AFTER the exact-count metric assertions above, which
    # this extra build would otherwise perturb.
    with subtest("hook-mode FOD without .drv upload (inline drv_content)"):
        # A fixed-output derivation in build-hook mode takes the
        # content-bound single-node fallback: build-remote sends the
        # derivation inline via wopBuildDerivation and never uploads the
        # .drv, so the gateway must embed the serialized derivation in
        # the submission for the fetcher to execute it. Before
        # gw.hook.inline-drv-content this flow was accepted but always
        # failed at the worker ("derivation not found in store").
        out_fod = client.succeed(
            "nix build --no-link --print-out-paths "
            f"--max-jobs 0 --builders '{store_url} x86_64-linux' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "-f ${hookFodDrv} 2>&1 | tail -n1"
        ).strip()
        assert out_fod.startswith("/nix/store/"), (
            f"hook-mode FOD build did not produce a store path: {out_fod!r}"
        )
        assert "hook-inline-fod" in out_fod, (
            f"unexpected hook-mode FOD output name: {out_fod!r}"
        )
        # Registered in the rio store → the fetcher really executed the
        # inline derivation and uploaded the verified output.
        client.succeed(f"nix path-info --store '{store_url}' {out_fod}")

    # ── hook-mode floating-CA: consumable result via inline fallback ───
    with subtest("hook-mode floating-CA without .drv upload (consumable result)"):
        # A floating-CA derivation in build-hook mode also takes the
        # content-bound single-node fallback (build-remote sends any CA
        # derivation inline and never copies the .drv). The build only
        # helps the client if the result is consumable: the gateway must
        # return builtOutputs keyed by the modular hash of the inline
        # derivation so build-remote can register the realisation and
        # print the realized path. The builders spec advertises the
        # ca-derivations system feature — without it build-remote
        # declines the machine for CA derivations.
        out_ca = client.succeed(
            "nix build --no-link --print-out-paths "
            f"--max-jobs 0 --builders '{store_url} x86_64-linux - 1 1 ca-derivations' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "-f ${hookCaDrv} 2>&1 | tail -n1"
        ).strip()
        assert out_ca.startswith("/nix/store/"), (
            f"hook-mode floating-CA build did not produce a store path: {out_ca!r}"
        )
        assert "hook-inline-ca" in out_ca, (
            f"unexpected hook-mode floating-CA output name: {out_ca!r}"
        )
        # The realized path is registered in the rio store (uploaded by
        # the worker, realisation written by the scheduler under the
        # modular hash the gateway carried on the node).
        client.succeed(f"nix path-info --store '{store_url}' {out_ca}")
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
        local = _path_info_one(client.succeed(
            "nix path-info --json ${common.busybox}"
        ), "${common.busybox}")
        gw = _path_info_one(client.succeed(
            f"nix path-info --json --store '{store_url}' ${common.busybox}"
        ), "${common.busybox}")
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
            f"nix store verify --no-trust --no-contents --store '{store_url}' ${common.busybox}"
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

    # ── build-hook mode (untrusted handshake steers the .drv-upload flow) ──
    with subtest("build-hook mode (--max-jobs 0 --builders ssh-ng://)"):
        # The gateway reports itself NotTrusted, so build-remote
        # (Nix >= 2.16 / Lix) copies the .drv closure and drives
        # wopBuildPathsWithResults instead of sending an inline
        # input-addressed BasicDerivation (which the gateway refuses).
        # A successful hook-mode build of a fresh derivation therefore
        # proves the untrusted handshake + .drv-upload + full-DAG
        # pipeline end-to-end against a stock client.
        out_hook = client.succeed(
            "nix build --no-link --print-out-paths "
            f"--max-jobs 0 --builders '{store_url} x86_64-linux' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "-f ${hookDrv} 2>&1 | tail -n1"
        ).strip()
        assert out_hook.startswith("/nix/store/"), (
            f"hook-mode build did not produce a store path: {out_hook!r}"
        )
        assert "rio-test-proto-hook" in out_hook or "proto-hook" in out_hook, (
            f"unexpected hook-mode output name: {out_hook!r}"
        )
        # The output is registered in the rio store — the build went
        # through the gateway, not a local fallback builder.
        client.succeed(f"nix path-info --store '{store_url}' {out_hook}")

    ${pkgs.lib.optionalString withNomExitTest nomExitScript}

    # ── result-pipeline properties through the production path ────────
    # (the differential harness preps its own build dir and never
    # uploads; these assertions need the real builder → upload → store
    # chain.)

    with subtest("build that uses $TMPDIR succeeds (sandbox /build writable)"):
        out_tmp = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${tmpdirProbeDrv}"
        ).strip()
        assert "rio-test-proto-tmpdir" in out_tmp, f"unexpected output: {out_tmp!r}"
        client.succeed(f"nix path-info --store '{store_url}' {out_tmp}")

    with subtest("stray store path created by a build is not registered"):
        out_stray = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${strayProbeDrv}"
        ).strip()
        client.succeed(f"nix path-info --store '{store_url}' {out_stray}")
        # The stray scratch path the build wrote next to $out must not
        # have been uploaded or registered.
        client.fail(
            f"nix path-info --store '{store_url}' "
            "/nix/store/cccccccccccccccccccccccccccccccc-rio-proto-stray"
        )

    with subtest("erg-native: exportReferencesGraph through the scheduler path"):
        # Two fresh builds (inner + the graph consumer). The consumer
        # script greps the registration file for the inner .drv before
        # copying it to $out — closure expansion is asserted by the
        # build succeeding, not by trusting the file exists.
        out_erg = client.succeed(
            f"nix-build --no-out-link --store '{store_url}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            "${ergOnDrv}"
        ).strip()
        assert "rio-test-erg-native" in out_erg, f"unexpected output: {out_erg!r}"
        client.succeed(f"nix path-info --store '{store_url}' {out_erg}")

    with subtest("erg-native demand observability: zero residual graph fetches"):
        # The fleet-scale zero-residual property at VM level: across
        # the WHOLE journal — the ERG build above included — no build
        # performed a residual graph .drv fetch. The replaced
        # closure-membership prefetch fetched on EVERY deep-closure
        # build; the demand model fetches only what a declaration
        # demands beyond the already-retained input-drv texts, and for
        # the erg-native build that set is empty (drvPath context makes
        # the graph .drv a direct inputDrv, so its text was retained at
        # the input-drv loop). Demand UNDER-supply cannot hide here: it
        # fails the erg-native build subtest itself with a glue error.
        # journalctl|grep -c exits 1 on zero matches; execute()
        # tolerates it so the count is honest either way.
        rc, n = worker.execute(
            "journalctl -u rio-builder --no-pager | "
            "grep -c 'fetching declaration-demanded graph'"
        )
        n = int(n.strip() or "0")
        assert n == 0, (
            f"expected zero residual graph .drv fetches across the scenario, got {n} — "
            "a build fetched graph texts beyond the retained input-drv set "
            "(bug_081 closure-membership fetching returning, or input-drv "
            "retention regressed)"
        )
  '';

  # ── nom-exit / SSH connection teardown ────────────────────────────────
  #
  # Client ssh_config has ControlMaster auto + ControlPersist 600 (via
  # default.nix extraClientModules). Without the gateway sending
  # `exit-status` (RFC 4254 §6.10), openssh's foreground client process
  # never returns to nix → nix blocks in pipe-read → `nom build` hangs.
  # Without the empty-connection-grace disconnect, the TCP socket stays
  # ESTABLISHED until inactivity_timeout (3600s).
  nomExitScript = ''
    with subtest("nom build exits under ControlMaster (gateway sends exit-status)"):
        # Same trivial drv as above — already cached, so this is a fast
        # round-trip. nom wraps `nix build` (new CLI), so `-f` not the
        # nix-build positional. timeout 60s: a hang would hit 124.
        # client.execute (not .succeed): the test driver runs under
        # `set -e`, so a nonzero from `timeout` would short-circuit
        # before we could inspect the code.
        rc, out = client.execute(
            f"timeout 60 nom build --no-link --store '{store_url}' "
            "-f ${trivialDrv} --arg busybox "
            "'(builtins.storePath ${common.busybox})' 2>&1"
        )
        assert rc == 0, (
            f"nom build exited {rc} (124=timeout → gateway likely "
            f"missing exit-status; see r[gw.conn.exit-status+3]). out: {out}"
        )

    with subtest("gateway reaps the idle connection after the empty grace period"):
        # The mux daemon holds the TCP open for ControlPersist (600s)
        # client-side. The gateway MUST NOT disconnect the instant the
        # build's session ends (that kills a ControlMaster mid-batch
        # — see r[gw.conn.exit-status+3]); it disconnects after the
        # connection has had zero active protocol sessions for
        # EMPTY_CONNECTION_GRACE (60s). Budget 90s: 60s timer +
        # scrape/teardown tail under builder load. Scrape on the
        # gateway node (has curl; client may not).
        ${gatewayHost}.wait_until_succeeds(
            "curl -fsS http://localhost:9090/metrics | "
            "grep -qx 'rio_gateway_connections_active 0'",
            timeout=90,
        )

    with subtest("rejected exec exits promptly (exit-status on failure path)"):
        # `ssh gateway echo` is rejected (gateway only accepts
        # `nix-daemon --stdio`). Pre-fix this hung under ControlMaster.
        # Expect: nonzero, fast. 124 = timeout = fail.
        rc, _ = client.execute(
            "timeout 30 ssh ${gatewayHost} echo hi 2>&1"
        )
        assert rc != 124, "rejected exec timed out (no exit-status on reject path)"
        assert rc != 0, f"rejected exec unexpectedly succeeded (rc={rc})"
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-${name}${nameSuffix}";
  skipTypeCheck = true;
  # Cold: builtin:fetchurl is a real network fetch inside the sandbox
  # (FOD). ~60s boot + ~30s fetchurl + build + assertions.
  globalTimeout = (if cold then 600 else 300) + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
    }}

    store_url = "ssh-ng://${gatewayHost}"

    with subtest("ssh-ng handshake (magic exchange, version, STDERR_LAST)"):
        # `ping` not `info`: Lix (forked at 2.18) lacks the rename; CppNix accepts as alias.
        client.succeed(f"nix store ping --store '{store_url}'")

    import json as _json
    def _path_info_one(json_str: str, path: str) -> dict:
        # `nix path-info --json` schema diverged: CppNix ≥2.19 → {path: {...}},
        # Lix / CppNix <2.19 → [{path: ..., ...}]. Normalize to dict-keyed-by-path.
        d = _json.loads(json_str)
        if isinstance(d, list):
            d = {x["path"]: x for x in d}
        return d[path]

    ${if cold then coldScript else warmScript}

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
