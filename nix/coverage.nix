# VM test coverage merge pipeline.
#
# Takes profraws from each coverage-mode VM test (collected via
# common.nix's collectCoverage → copy_from_vm → $out/coverage/
# <node>/profraw.tar.gz), converts to lcov via the toolchain's
# llvm-profdata + llvm-cov, normalizes source paths, and unions
# with the unit-test lcov.
#
# Outputs:
#   perTestLcov.vm-<scenario>  — one lcov per VM test
#   vmLcov                  — all VM tests unioned
#   full                    — unit ∪ VM, HTML report, per-test breakdown
#
# CRITICAL: use the toolchain-bundled llvm-profdata/llvm-cov, NOT
# system llvm. Profile format versioning is tied to the rustc that
# compiled the instrumented binary.
#
# Do NOT add -Z coverage-options=branch to RUSTFLAGS — llvm-cov export
# segfaults at ~15GB RSS with 20+ object files (tried 8126dcf, reverted
# 4c8365d, diagnostic in 395c049).
{
  pkgs,
  rustStable,
  rio-workspace-cov,
  vmTestsCov,
  workspaceSrc,
  unitCoverage,
  # lcov --extract patterns (one `<crateName>/*` per workspace member,
  # workspace-hack excluded). Derived from memberFilesets in flake.nix
  # so non-rio-prefixed members (xtask) aren't dropped by a literal.
  covExtractPatterns,
}:
let
  inherit (pkgs) lib;

  # Source-path normalization pattern. buildRustCrate's
  # --remap-path-prefix maps the sandbox build dir to `/`; the
  # per-crate localRemap in crate2nix.nix maps the unpacked
  # `source/` dir (fileset.toSource's fixed name) to `/<crateName>`
  # so profraws reference `/rio-common/src/lib.rs`. Strip the leading
  # slash to get repo-relative paths genhtml/codecov can resolve.
  stripPrefix = "s|^/||";

  # --ignore-filename-regex for llvm-cov export on VM-test profraws.
  # Dep paths like `/tokio-1.50.0/...` get filtered by the per-test
  # `lcov --extract <covExtractPatterns>` step in mkPerTestLcov; this
  # regex catches build artifacts that --extract would let through
  # (the per-member pattern matches `rio-proto/target/build/...`
  # generated proto code, which genhtml can't resolve against
  # workspaceSrc).
  ignoreRegex = "\\.cargo/registry|\\.cargo/git|/rustc/|/nix/store/.*-vendor|target/.*build";

  # Toolchain llvm tools. rustStable is the rust-bin derivation;
  # its lib/rustlib/<target>/bin/ has llvm-profdata + llvm-cov
  # (from the llvm-tools-preview component).
  sysroot = "${rustStable}/lib/rustlib/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/bin";

  # Instrumented binaries. llvm-cov needs these to read the
  # embedded coverage map (the __llvm_covfun/__llvm_covmap sections).
  covBins = map (n: "${rio-workspace-cov}/bin/rio-${n}") [
    "store"
    "scheduler"
    "gateway"
    "builder"
    "controller"
    "cli"
    "mountd"
  ];
  objectFlags = lib.concatMapStringsSep " " (b: "--object ${b}") covBins;

  # profraw → lcov for one VM test. Input: the VM test derivation's
  # $out (contains coverage/<node>/profraw.tar.gz). Output: a single
  # path-normalized lcov file.
  #
  # Empty-tarball / no-profraws guard: some nodes run no rio
  # services (client), some tests may not exercise all binaries.
  # Emit an empty lcov and move on — `lcov -a` handles empty
  # inputs gracefully (warns, continues).
  #
  # Extraction is factored into a string so the cov-extract-nocollide
  # check below can exercise the same code path against synthetic
  # tarballs (no KVM needed).
  extractProfraws = covRoot: ''
    # Per-node subdir: tarballs live at <covRoot>/<node>/profraw.tar.gz.
    # Standalone-fixture profraws are named rio-%h-%p-%m.profraw; %h is
    # the in-VM hostname, but identically-configured workers share the
    # same boot sequence → correlated PIDs, same binary → same %m. The
    # per-node extract dir guarantees no cross-node filename collision
    # regardless of in-VM naming. (A flat extract with --skip-old-files
    # sat here previously and silently dropped colliding profraws —
    # the k3s fixture solved this with $(POD_NAME) in the filename;
    # standalone never got the equivalent until %h was added.)
    mkdir -p $TMPDIR/raw
    for tarball in $(find ${covRoot} -name profraw.tar.gz 2>/dev/null); do
      node=$(basename "$(dirname "$tarball")")
      mkdir -p "$TMPDIR/raw/$node"
      tar xzf "$tarball" -C "$TMPDIR/raw/$node" 2>/dev/null || true
    done
    # nullglob: if no match, the glob expands to nothing instead
    # of a literal '*.profraw' — makes the array-length check
    # reliable regardless of bash globbing defaults. globstar:
    # `**` recurses into per-node subdirs.
    shopt -s nullglob globstar
    profraws=($TMPDIR/raw/**/*.profraw)
  '';

  # Smoke scenario for the cov-smoke gate. Picked for broadest
  # coverage-infrastructure surface per minute: protocol-warm
  # exercises store+scheduler+gateway together in ~5min at 3 vCPU
  # (k3s scenarios are 8 vCPU, ~2× slower). If a future break class
  # only surfaces in k3s fixtures, swap this — but the primary job
  # is "prove profraw→lcov pipeline works end-to-end", and any
  # scenario that produces non-empty profraws does that.
  #
  # Defined before mkPerTestLcov so the smoke scenario's lcov drv can
  # self-assert (hard-fail on empty/malformed data) instead of needing
  # a separate wrapper drv. That keeps the "coverage infra broken"
  # gate inside `ciMatrix.coverage.${smokeScenario}` — the entry CI
  # already builds — instead of a second `ciMatrix.vm-test.cov-smoke`
  # entry that rebuilds the same ~5-10min instrumented VM scenario on
  # a parallel KVM runner.
  smokeScenario = "vm-protocol-warm-standalone";

  mkPerTestLcov =
    name: vmTest:
    # Smoke gate: the smoke scenario's lcov MUST contain real coverage
    # data — empty or SF:-less lcov means the profraw→lcov pipeline is
    # broken, not "this test exercised nothing". A PSA break went 118
    # commits undetected because `.#coverage` is run on demand and its
    # failures were triaged as individual test gaps instead of a
    # pipeline-level halt; folding the assertion into this drv makes
    # the GHA `coverage` job (which already builds it) the merge gate.
    # Other scenarios keep the soft warn-and-continue path so a
    # single VM test producing no profraws (e.g., a node that runs no
    # rio services) doesn't blank the merged report.
    let
      isSmoke = name == smokeScenario;
    in
    pkgs.runCommand "rio-cov-${name}"
      {
        # Reachable as `.#coverage.vm-<scenario>.raw` — the
        # coverage-mode VM run itself (result/coverage/<node>/
        # profraw.tar.gz). Debugging the profraw→lcov pipeline
        # without re-evaluating the whole NixOS config to find
        # the input drv.
        passthru.raw = vmTest;
      }
      ''
          ${extractProfraws "${vmTest}/coverage"}
        if [ "''${#profraws[@]}" -eq 0 ]; then
          ${
            if isSmoke then
              ''
                echo "FAIL: ${name} produced no profraws — coverage infra broken" >&2
                echo "Check graceful-shutdown flush + collectCoverage in nix/tests/common.nix" >&2
                exit 1
              ''
            else
              ''
                echo "WARNING: no profraws for ${name}, emitting empty lcov"
                touch $out
                exit 0
              ''
          }
        fi
        ${sysroot}/llvm-profdata merge -sparse "''${profraws[@]}" -o $TMPDIR/m.profdata
        # 2>/dev/null: llvm-cov writes warnings ("N functions have
        # mismatched data") to stdout, which corrupts the lcov file.
        # These warnings are expected (shared libs between binaries);
        # stderr of lcov step shows any real issues.
        # target/release/build/: generated proto code (tonic-prost-build
        # output). Source doesn't exist in workspaceSrc (build artifact),
        # so genhtml would fail. These are wrapper code, not ours —
        # the real coverage signal is in rio-*/src/.
        ${sysroot}/llvm-cov export \
          --format=lcov \
          --instr-profile=$TMPDIR/m.profdata \
          ${objectFlags} \
          --ignore-filename-regex='${ignoreRegex}' \
          2>/dev/null > $TMPDIR/raw.lcov
        # `-a` (add tracefile) is the operation; `--substitute`
        # piggybacks on it. lcov requires one of -z/-c/-a/-e/-r/-l
        # alongside --substitute (it's a modifier, not standalone).
        # --ignore-errors unused: lcov 2.x errors on an unmatched
        # --substitute pattern by default; crate2nix's already-
        # normalized unit lcov may not match the VM stripPrefix.
        ${pkgs.lcov}/bin/lcov --ignore-errors unused \
          --substitute '${stripPrefix}' \
          -a $TMPDIR/raw.lcov -o $TMPDIR/stripped.lcov
        # Extract here, not in the aggregate: each raw VM lcov is
        # ~165MB (every dep crate, ~16k SF entries) and shrinks ~160×
        # to ~1MB after filtering to workspace paths. Doing it per-test
        # makes vmLcov's `lcov -a` operate on ~24MB instead of ~4GB and
        # cuts what gets cached/substituted by the same factor.
        # Pattern list derived from memberFilesets — includes xtask.
        ${pkgs.lcov}/bin/lcov --ignore-errors unused,empty \
          --extract $TMPDIR/stripped.lcov ${lib.escapeShellArgs covExtractPatterns} -o $out
        ${lib.optionalString isSmoke ''
          # Smoke assertion: profraws existed, but the lcov pipeline can
          # still produce garbage (failed llvm-cov export → header-only
          # file, or --extract dropping every record). Catch that here
          # so the GHA coverage job fails — not a downstream consumer
          # silently ingesting an empty report.
          if [ ! -s $out ]; then
            echo "FAIL: ${name} produced no coverage data (empty lcov)" >&2
            echo "Coverage infrastructure broken — profraws collected but pipeline produced nothing" >&2
            exit 1
          fi
          if ! grep -q '^SF:' $out; then
            echo "FAIL: ${name} lcov has no SF: records (malformed)" >&2
            exit 1
          fi
        ''}
      '';

  perTestLcov = lib.mapAttrs mkPerTestLcov vmTestsCov;

  # Union all per-test lcovs. `lcov -a` is additive — a line hit
  # in ANY VM test is hit in the union.
  vmLcov = pkgs.runCommand "rio-cov-vm-total" { nativeBuildInputs = [ pkgs.lcov ]; } ''
    args=""
    ${lib.concatMapStringsSep "\n" (p: ''
      # Skip empty lcovs (guard above emitted touch $out).
      if [ -s "${p}" ]; then
        args="$args -a ${p}"
      fi
    '') (builtins.attrValues perTestLcov)}
    if [ -z "$args" ]; then
      echo "WARNING: all per-test lcovs empty"
      touch $out
      exit 0
    fi
    lcov $args -o $out
  '';

  # Unit-test lcov. checks.nix already path-normalized it
  # (`lcov --substitute 's|^/||'`), so no re-parse needed — but keep
  # a file-shaped derivation (not a dir/lcov.info path) so the GHA
  # coverage matrix sees the same shape as perTestLcov entries.
  unitLcov = pkgs.runCommand "rio-cov-unit" { } ''
    ln -s ${unitCoverage}/lcov.info $out
  '';
in
{
  inherit perTestLcov vmLcov;

  # Regression: two nodes producing IDENTICALLY-named profraws (same
  # PID + same binary signature — happens with identically-configured
  # workers under deterministic NixOS boot) MUST both survive
  # extraction. The old flat-extract `--skip-old-files` kept one and
  # dropped the rest. Runs without KVM — synthetic tarballs only.
  extractNoCollide = pkgs.runCommand "rio-cov-extract-nocollide" { } ''
    mkdir -p fake/worker1 fake/worker2
    echo a > rio-42-abc.profraw
    tar czf fake/worker1/profraw.tar.gz rio-42-abc.profraw
    echo b > rio-42-abc.profraw
    tar czf fake/worker2/profraw.tar.gz rio-42-abc.profraw
    ${extractProfraws "fake"}
    if [ "''${#profraws[@]}" -ne 2 ]; then
      echo "FAIL: lost a colliding profraw — got ''${#profraws[@]}, want 2" >&2
      ls -lR $TMPDIR/raw >&2
      exit 1
    fi
    # And the contents differ (proves both tarballs extracted, not one
    # twice).
    if diff -q "''${profraws[0]}" "''${profraws[1]}" >/dev/null; then
      echo "FAIL: both profraws identical — one tarball extracted twice?" >&2
      exit 1
    fi
    touch $out
  '';

  # Fast coverage-infrastructure smoke for `nix-fast-build .#checks`
  # (and `nix flake check` on KVM hosts). ONE scenario in coverage
  # mode, ~5min. The actual gate — empty/SF: assertion — lives inside
  # `perTestLcov.${smokeScenario}` (mkPerTestLcov self-asserts for the
  # smoke scenario), which is also the GHA `ciMatrix.coverage` matrix
  # entry. Folding the gate into the lcov drv means the GHA `coverage`
  # job (which already builds `perTestLcov.${smokeScenario}` for
  # codecov upload) IS the merge-gate guard — no second
  # `ciMatrix.vm-test.cov-smoke` entry rebuilding the same ~5-10min
  # instrumented VM scenario on a parallel KVM runner.
  #
  # This wrapper adds a human-readable `lcov --summary` for the local
  # build log and the `result/scenario` provenance marker — useful when
  # iterating on the coverage pipeline locally, dead weight in CI.
  smoke =
    let
      lcov = perTestLcov.${smokeScenario};
    in
    pkgs.runCommand "rio-cov-smoke" { nativeBuildInputs = [ pkgs.lcov ]; } ''
      mkdir -p $out
      cp ${lcov} $out/smoke.lcov
      echo "${smokeScenario}" > $out/scenario
      lcov --summary ${lcov} | tee $out/summary
    '';

  # The headline target. result/lcov.info = unit ∪ VM, filtered to
  # workspace crates. result/html/ = genhtml. result/per-test/ =
  # individual breakdowns.
  full =
    pkgs.runCommand "rio-coverage"
      {
        nativeBuildInputs = [ pkgs.lcov ];
      }
      ''
        mkdir -p $out/per-test $out/html

        # Per-test breakdown.
        ${lib.concatStringsSep "\n" (
          lib.mapAttrsToList (n: p: "cp ${p} $out/per-test/${n}.lcov") perTestLcov
        )}

        # Combined: unit ∪ VM. Guard against empty vmLcov (all VM
        # tests produced no profraws — shouldn't happen in practice
        # but the build should still succeed with unit-only data).
        if [ -s ${vmLcov} ]; then
          lcov -a ${unitLcov} -a ${vmLcov} -o $TMPDIR/combined.lcov
        else
          echo "WARNING: vmLcov is empty, using unit-only"
          cp ${unitLcov} $TMPDIR/combined.lcov
        fi
        # --remove drops generated build artifacts that the per-entry
        # --extract <covExtractPatterns> let through
        # (rio-proto/target/build/... matches the per-member pattern
        # but the generated .rs doesn't exist in workspaceSrc). Both
        # inputs are already member-filtered at source (covLcovs /
        # mkPerTestLcov), so the old --extract
        # pass here is gone — it was a no-op on ~2MB of pre-filtered
        # data anyway. unused: don't error if pattern doesn't match
        # (clean unit-only runs may not have these).
        lcov --ignore-errors unused \
          --remove $TMPDIR/combined.lcov '*/target/*' -o $out/lcov.info

        # HTML report. cd into source so genhtml can find files
        # for the source view. --ignore-errors source: safety net
        # for any remaining build-time-generated paths that slip
        # through the regex (genhtml synthesizes a placeholder).
        cd ${workspaceSrc}
        genhtml $out/lcov.info --output-directory $out/html \
          --ignore-errors source --synthesize-missing

        # Summary to build log for quick inspection.
        echo "=== Combined Coverage Summary ==="
        lcov --summary $out/lcov.info
        echo ""
        echo "=== Per-test coverage contribution ==="
        for f in $out/per-test/*.lcov; do
          echo "--- $(basename $f .lcov) ---"
          lcov --summary $f 2>/dev/null | grep -E "lines|functions" || echo "(empty)"
        done
      '';
}
