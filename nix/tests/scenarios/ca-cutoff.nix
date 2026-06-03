# CA early-cutoff end-to-end: submit a CA-on-CA chain, complete once,
# resubmit with a different-marker-same-output root — assert
# rio_scheduler_ca_cutoff_saves_total ≥ 2 (B and C skipped) AND the
# second submit completes in <15s (vs ~24s for build-1's serial
# 3×8s sleeps).
#
# USER-A10: the chain is CA-depends-on-CA throughout. If
# saves_total stays at 0, either (a) resolve is broken (B dispatches
# with the unresolved placeholder → worker ENOENT), or (b)
# cutoff-compare is miscounting (self-match exclusion not yet
# landed). Check the worker journals for "placeholder" or
# "realisation".
#
# The marker-independence trick: `ca-chain.nix` bakes the marker into
# the ATerm env (so A's drv hash differs between build-1 and build-2,
# forcing a fresh submit) but NOT into `$out/chain` (so A's nar_hash
# is identical, cutoff fires).
#
# verify marker (scenario is single-test, so marker lives at the
# default.nix wiring-point per the tracey convention).
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
in
pkgs.testers.runNixOSTest {
  name = "rio-ca-cutoff";
  skipTypeCheck = true;
  # Build-1 ~24s (3×8s serial) + build-2 <25s + ia/modes subtests +
  # stripped-ca-resubmit (~60s: 1s-sleep IA builds + scheduler restart)
  # + VM boot ~30s + slack.
  globalTimeout = 720 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    import time

    store_url = "ssh-ng://${gatewayHost}"

    def build_ca_chain(marker, ia_levels=0, sleep_secs=8):
        """Build the floating-CA A→B→C chain. marker goes into the
        ATerm env (distinct drv hashes across calls) but NOT into
        $out/chain (identical nar_hash → cutoff fires). --impure for
        builtins.currentSystem in ca-chain.nix. ia_levels stacks N
        deferred-IA steps above the CA chain."""
        try:
            return client.succeed(
                "nix-build --no-out-link --impure "
                f"--store '{store_url}' "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                f"--argstr marker '{marker}' "
                f"--arg iaLevels {ia_levels} "
                f"--arg sleepSecs {sleep_secs} "
                "${drvs.caChain} 2>&1"
            )
        except Exception:
            dump_all_logs([${gatewayHost}, worker])
            raise

    # ══════════════════════════════════════════════════════════════════
    # Build 1: fresh CA chain, all three steps run (~24s @ 8s each).
    # ══════════════════════════════════════════════════════════════════
    with subtest("build-1: CA chain from cold"):
        out1 = build_ca_chain("b1")
        assert "/nix/store/" in out1, \
            f"expected a store-path result, got: {out1[:200]}"

    # Regression guard (P0397 self-match exclusion): saves_total must
    # be 0 after build-1. If nonzero, the realisation-based cutoff
    # check is matching the just-uploaded output against itself —
    # a missing self-exclusion would make every first-ever CA build
    # look like a cutoff trigger.
    m_after1 = scrape_metrics(${gatewayHost}, 9091)
    saves_after1 = metric_value(m_after1,
        "rio_scheduler_ca_cutoff_saves_total") or 0.0
    assert saves_after1 == 0.0, (
        f"build-1 (first-ever) should have saves=0; got {saves_after1}. "
        "Self-match exclusion not firing — see P0397."
    )

    # ══════════════════════════════════════════════════════════════════
    # Build 2: different marker → A's drv hash differs → scheduler
    # re-submits. But A's $out/chain content is marker-independent →
    # nar_hash identical → cutoff-compare matches → B+C Skipped.
    # ══════════════════════════════════════════════════════════════════
    with subtest("build-2: cutoff skips B+C"):
        t0 = time.monotonic()
        out2 = build_ca_chain("b2")
        elapsed = time.monotonic() - t0

        m_after2 = scrape_metrics(${gatewayHost}, 9091)
        saves_after2 = metric_value(m_after2,
            "rio_scheduler_ca_cutoff_saves_total") or 0.0
        # B and C both skipped → saves ≥ 2. ≥ not == because a
        # diamond-shaped chain (if ca-chain.nix grows one) could skip
        # more; the assertion cares that cutoff FIRED, not the exact
        # count.
        assert saves_after2 - saves_after1 >= 2.0, (
            f"expected ≥2 cutoff saves (B+C skipped); "
            f"got delta={saves_after2 - saves_after1} "
            f"(before={saves_after1}, after={saves_after2}). "
            "If 0: resolve broken (check worker logs for 'placeholder') "
            "or cutoff-compare not matching (check scheduler logs for "
            "'CA cutoff-compare: ... counting as miss')."
        )
        # A rebuilds (~8s), B+C skip (instant) → ~8s of build work.
        # One-shot worker overhead: build-1's worker exited; build-2
        # waits for systemd Restart=always (1s) + scheduler re-register
        # (~5-10s under VM load). 25s bound = 8s build + ~10s restart
        # + 7s slack. The saves_total check above is the cutoff PROOF;
        # this asserts "closer to one-build than three-builds" timing.
        assert elapsed < 25, (
            f"build-2 took {elapsed:.1f}s (expected <25s with cutoff). "
            f"saves_total delta={saves_after2 - saves_after1} — "
            "if saves≥2, B+C WERE skipped but something else is slow."
        )

        # Second build's output path must match build-1's (same CA
        # content → same CA output path). Lightweight cross-check that
        # the CA derivation model is wired end-to-end.
        path1 = next(
            (ln for ln in out1.splitlines() if ln.startswith("/nix/store/")),
            None,
        )
        path2 = next(
            (ln for ln in out2.splitlines() if ln.startswith("/nix/store/")),
            None,
        )
        assert path1 and path2 and path1 == path2, (
            f"CA chain outputs should be identical across builds "
            f"(same content → same CA path); got {path1!r} vs {path2!r}"
        )

    # ══════════════════════════════════════════════════════════════════
    # ia.deferred: IA-on-CA. C is input-addressed with floating-CA B as
    # input — C's own $out is empty in the ATerm (DerivationOutput::
    # Deferred). The scheduler must compute it post-resolve via
    # makeOutputPath; without that the build runs with $out="" and
    # `mkdir -p $out` prints busybox usage. ia_levels=2 additionally
    # stacks D (IA-on-deferred-IA) — regression target for the gateway's
    # has_unknown_output_paths predicate (translate.rs).
    # ══════════════════════════════════════════════════════════════════
    with subtest("ia-deferred: IA-on-CA builds with computed $out"):
        out_ia1 = build_ca_chain("ia1", ia_levels=1, sleep_secs=1)
        path_c = next(
            (ln for ln in out_ia1.splitlines() if ln.startswith("/nix/store/")),
            None,
        )
        assert path_c and "rio-ia-c" in path_c, (
            "deferred-IA C should produce a concrete rio-ia-c store path; "
            f"got output:\n{out_ia1[-400:]}"
        )

    with subtest("ia-deferred-2-level: IA-on-IA-on-CA resolves"):
        out_ia2 = build_ca_chain("ia2", ia_levels=2, sleep_secs=1)
        path_d = next(
            (ln for ln in out_ia2.splitlines() if ln.startswith("/nix/store/")),
            None,
        )
        assert path_d and "rio-ia-d" in path_d, (
            "deferred-IA D should produce a concrete rio-ia-d store path; "
            f"got output:\n{out_ia2[-400:]}"
        )
        # No unresolved /1<hash> placeholder leaked into the build log.
        assert " /1" not in out_ia2, (
            "unresolved downstream-placeholder leaked into IA build "
            f"output:\n{out_ia2[-400:]}"
        )

    # ══════════════════════════════════════════════════════════════════
    # Non-default CA modes: flat hashing and sha512. The builder's CA
    # finalization and the store's CA verification must agree on the
    # declared method, or the upload is rejected and the build fails.
    # ══════════════════════════════════════════════════════════════════
    with subtest("floating-CA flat + sha512 modes upload and register"):
        for attr, marker in (("flat", "rio-ca-flat"), ("sha512", "rio-ca-sha512")):
            out_ca = client.succeed(
                "nix-build --no-out-link --impure "
                f"--store '{store_url}' "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                f"-A {attr} "
                "${drvs.caModes} 2>&1"
            )
            ca_path = next(
                (ln for ln in out_ca.splitlines() if ln.startswith("/nix/store/")),
                None,
            )
            assert ca_path and marker in ca_path, (
                f"{attr}: expected a {marker} store path, got:\n{out_ca[-400:]}"
            )
            client.succeed(f"nix path-info --store '{store_url}' {ca_path}")

    # ══════════════════════════════════════════════════════════════════
    # stripped-ca-resubmit — round-16 merged_bug_038 regression pin
    # (deploy blocker). The evidence-free settled-row lifecycle end to
    # end under the production signing posture:
    #
    #  1. WARM deferred-IA submit over the realized b1 CA chain
    #     (sleep_secs=8 is LOAD-BEARING: sleepSecs is baked into every
    #     step's ATerm, so any other value mints FRESH A/B drvs and the
    #     chain submits inline-cold — recomputable, evidence-rich). The
    #     gateway's session cache does not hold realized B's bytes, so
    #     hash_derivation_modulo DEGRADES (WARN "caller will degrade")
    #     and C-ia is submitted with NO declared hash — the EVIDENCE-FREE RE-PRESENTATION
    #     side of the brick. The settled row's paths stay empty
    #     (deferred-IA) so path agreement is impossible; completion's
    #     CA bookkeeping backfills a live row hash (the realisation
    #     key), but a degraded re-presentation can never match it. (The sibling shape where a declared hash IS
    #     present and gets ingress-STRIPPED with M_070 preservation is
    #     pinned end-to-end by the PG-backed unit tests; this VM pins
    #     the production gateway shape.)
    #  2. Scheduler restart: settled rows become row-only (recovery
    #     rehydrates non-terminal rows only) — the exact post-reap /
    #     post-failover window that bricked.
    #  3. Resubmit a consumer (ia_levels=2 stacks D-ia on C-ia): C-ia
    #     arrives as a bare hash-less store-backed echo (the gateway
    #     degrades again — same session-cache shape) against its
    #     settled row: no path can agree (row paths empty), no hash
    #     can match (incoming has none). Pre-fix: no matchable
    #     evidence → byte-anchored Refuse arm → deterministic
    #     FAILED_PRECONDITION, no client-side escape. Post-fix: the
    #     dual-anchor basis rejoins
    #     (sched.persist.settled-identity-freeze+4) and the build
    #     proceeds; observable on
    #     rio_scheduler_merge_stripped_rejoin_total{basis=dual_anchor}
    #     — the wipe-deploy runbook's success signal.
    # ══════════════════════════════════════════════════════════════════
    with subtest("stripped-ca-resubmit: warm IA consumer settles with empty paths"):
        out_strip = build_ca_chain("b1", ia_levels=1, sleep_secs=8)
        path_cia = next(
            (ln for ln in out_strip.splitlines() if ln.startswith("/nix/store/")),
            None,
        )
        assert path_cia and "rio-ia-c" in path_cia, (
            "warm deferred-IA consumer over the realized b1 chain should "
            f"build rio-ia-c; got:\n{out_strip[-400:]}"
        )
        # The brick-precondition rows: settled deferred-IA rows whose
        # expected paths are ALL empty (path agreement structurally
        # impossible) at a byte-anchored rank (verified_built —
        # completion raised it; completion's CA bookkeeping also
        # backfills a live row hash as the realisation key, which a
        # DEGRADED hash-less re-presentation can never match). All
        # three rio-ia-c rows (ia1, ia2, b1-warm) share the shape.
        bare_rows = psql(
            ${gatewayHost},
            "SELECT count(*) FROM derivations "
            "WHERE pname = 'rio-ia-c' "
            "AND evidence_rank IN ('path_bound_bytes', 'verified_built') "
            "AND status IN ('completed', 'skipped') "
            "AND NOT EXISTS (SELECT 1 FROM unnest(expected_output_paths) p "
            "                WHERE length(p) > 0)",
        )
        if int(bare_rows) < 1:
            rows_dump = psql(
                ${gatewayHost},
                "SELECT pname, status, evidence_rank, "
                "ca_modular_hash IS NULL AS live_null, "
                "ca_modular_hash_stripped IS NULL AS preserved_null, "
                "array_to_string(expected_output_paths, ',') AS paths "
                "FROM derivations WHERE pname = 'rio-ia-c'",
            )
            raise AssertionError(
                f"expected settled empty-path byte-anchored rio-ia-c "
                f"row(s), got count={bare_rows!r}. All rio-ia-c rows:\n"
                f"{rows_dump}"
            )

    with subtest("stripped-ca-resubmit: row-only stripped closure rejoins after restart"):
        # Row-only: recovery loads non-terminal rows; settled rows stay
        # in PG with no resident node — the merged_bug_038 window.
        ${gatewayHost}.succeed("systemctl restart rio-scheduler.service")
        ${gatewayHost}.wait_until_succeeds(
            "curl -sf http://localhost:9091/metrics", timeout=60
        )

        # Rebuild-after-GC: invalidate every rio-ia-c OUTPUT (narinfo
        # is the store's validity authority; the .drv rows stay). The
        # client then needs C-ia rebuilt, so the next submission
        # RE-PRESENTS the settled hash — without this, the realized
        # output makes the gateway resolve C-ia away and the settled
        # row is never confronted (observed: empty rejoin series).
        deleted = psql(
            ${gatewayHost},
            "DELETE FROM narinfo "
            "WHERE store_path LIKE '%-rio-ia-c' RETURNING store_path",
        )
        assert "rio-ia-c" in deleted, (
            f"expected to invalidate at least one rio-ia-c output, "
            f"got: {deleted!r}"
        )

        # Stage the merged_bug_038 row shape: apply the strip writers'
        # single-statement move (live -> preservation column) to the
        # settled rows. This is byte-identical to what
        # persist_evidence_rank_and_strip_modular_hash and the ingress
        # strip leave behind (pinned against the real writers by the
        # M_070 unit tests); the VM cannot reach it through gateway
        # traffic alone because this fixture's gateway never DECLARES
        # a hash for warm consumers (session-cache degrade) — there is
        # nothing at ingress to strip — and completion then backfills
        # the live hash as the realisation key. Without this move the
        # resubmission self-heals via a classical HashMatch (observed:
        # run with live hash present rejoined silently). Pre-fix, a
        # row in THIS shape + the re-presentation below = deterministic
        # SettledIdentityConflict — the brick.
        moved = psql(
            ${gatewayHost},
            "UPDATE derivations "
            "SET ca_modular_hash_stripped = ca_modular_hash, "
            "    ca_modular_hash = NULL "
            "WHERE pname = 'rio-ia-c' "
            "AND status IN ('completed', 'skipped') "
            "AND ca_modular_hash IS NOT NULL "
            "RETURNING drv_hash",
        )
        assert moved.strip(), (
            "expected to move at least one settled rio-ia-c row's live "
            f"hash into the preservation column, got: {moved!r}"
        )

        # D-ia stacks on C-ia: C-ia must REBUILD (output invalidated),
        # so the submission re-presents the settled hash as a bare
        # HASH-LESS echo (gateway degrade, same session-cache shape as
        # step 1) with empty expected paths. Pre-fix this nix-build
        # failed FAILED_PRECONDITION at Step 0.5 — the brick. Post-fix
        # the dual-anchor basis rejoins and C-ia rebuilds normally.
        out_rejoin = build_ca_chain("b1", ia_levels=2, sleep_secs=8)
        path_dia = next(
            (ln for ln in out_rejoin.splitlines() if ln.startswith("/nix/store/")),
            None,
        )
        assert path_dia and "rio-ia-d" in path_dia, (
            "resubmission over the stripped settled row must succeed "
            f"(merged_bug_038); got:\n{out_rejoin[-400:]}"
        )

        # The rejoin is the deploy runbook's success signal — and the
        # BASIS label proves it came through an M_070 clause: against
        # a stripped row (live NULL, paths empty), a re-presentation
        # that re-declares the recomputed hash matches byte-equal on
        # the PRESERVED value (preserved_claim); a degraded hash-less
        # one falls to the byte-anchored rank (dual_anchor). Both are
        # production shapes (the gateway declares iff its session
        # cache can recompute) and both were bricks pre-fix — accept
        # either, require at least one.
        m_rejoin = scrape_metrics(${gatewayHost}, 9091)
        series = m_rejoin.get("rio_scheduler_merge_stripped_rejoin_total", {})
        rejoins = sum(
            v for k, v in series.items()
            if "preserved_claim" in k or "dual_anchor" in k
        )
        assert rejoins >= 1.0, (
            f"expected >=1 stripped rejoin (preserved_claim or "
            f"dual_anchor — the would-have-bricked rebuild admitted "
            f"through the M_070 bases); series: {series}"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
