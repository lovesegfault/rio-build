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
  # Build-1 ~24s (3×8s serial) + build-2 <25s + VM boot ~30s + slack.
  # 600s matches observability.nix's generous ceiling.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import time

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}
    ${common.sshKeySetup gatewayHost}
    ${common.seedBusybox gatewayHost}

    store_url = "ssh-ng://${gatewayHost}"

    def build_ca_chain(marker):
        """Build the floating-CA A→B→C chain. marker goes into the
        ATerm env (distinct drv hashes across calls) but NOT into
        $out/chain (identical nar_hash → cutoff fires). --impure for
        builtins.currentSystem in ca-chain.nix."""
        try:
            return client.succeed(
                "nix-build --no-out-link --impure "
                f"--store '{store_url}' "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                f"--argstr marker '{marker}' "
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

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
