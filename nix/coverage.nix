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
#   unitLcov                — unit-test lcov, path-normalized
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
}:
let
  inherit (pkgs) lib;

  # Source-path normalization pattern. buildRustCrate's
  # --remap-path-prefix maps the sandbox build dir to `/`, so
  # profraws reference `/rio-common/src/lib.rs`. Strip the leading
  # slash to get repo-relative paths genhtml can resolve.
  stripPrefix = "s|^/||";

  # --ignore-filename-regex for llvm-cov export on VM-test profraws.
  # Dep paths like `/tokio-1.50.0/...` get filtered by the final
  # `lcov --extract 'rio-*'` step; this regex catches common build
  # artifacts that --extract misses. `target/.*build` covers
  # crate2nix's target/build/ (buildRustCrate puts generated proto
  # code at target/build/<crate>.out/, genhtml can't resolve it
  # against workspaceSrc).
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

  mkPerTestLcov =
    name: vmTest:
    pkgs.runCommand "rio-cov-${name}" { } ''
      ${extractProfraws "${vmTest}/coverage"}
      if [ "''${#profraws[@]}" -eq 0 ]; then
        echo "WARNING: no profraws for ${name}, emitting empty lcov"
        touch $out
        exit 0
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
        -a $TMPDIR/raw.lcov -o $out
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
  # Smoke scenario for the cov-smoke gate. Picked for broadest
  # coverage-infrastructure surface per minute: protocol-warm
  # exercises store+scheduler+gateway together in ~5min at 3 vCPU
  # (k3s scenarios are 8 vCPU, ~2× slower). If a future break class
  # only surfaces in k3s fixtures, swap this — but the primary job
  # is "prove profraw→lcov pipeline works end-to-end", and any
  # scenario that produces non-empty profraws does that.
  smokeScenario = "vm-protocol-warm-standalone";
in
{
  inherit perTestLcov vmLcov unitLcov;

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

  # Fast coverage-infrastructure smoke. ONE scenario in coverage
  # mode, asserts the profraw→lcov pipeline produced real data.
  # ~5min. In checks (blocking) — catches "coverage infra broken"
  # without the 25min coverage-full cost. A PSA break went 118
  # commits undetected because coverage-full is backgrounded and
  # its failures were triaged as individual test-gaps instead of a
  # pipeline-level halt. With cov-smoke in checks, infra breaks
  # fail the merge gate directly.
  smoke =
    let
      lcov = perTestLcov.${smokeScenario};
    in
    pkgs.runCommand "rio-cov-smoke" { nativeBuildInputs = [ pkgs.lcov ]; } ''
      # mkPerTestLcov emits `touch $out` (empty file) on zero
      # profraws — that's a WARNING for the merge pipeline but a
      # FAILURE for smoke: empty = coverage infra didn't collect.
      if [ ! -s ${lcov} ]; then
        echo "FAIL: ${smokeScenario} produced no coverage data (empty lcov)" >&2
        echo "Coverage infrastructure broken — profraws not collected or pipeline failed" >&2
        exit 1
      fi
      # Structural sanity: at least one SF: (source file) record.
      # Guards against a non-empty-but-garbage lcov (e.g., only a
      # header line from a failed llvm-cov export).
      if ! grep -q '^SF:' ${lcov}; then
        echo "FAIL: ${smokeScenario} lcov has no SF: records (malformed)" >&2
        exit 1
      fi
      mkdir -p $out
      cp ${lcov} $out/smoke.lcov
      echo "${smokeScenario}" > $out/scenario
      lcov --summary ${lcov} | tee $out/summary
    '';

  # The headline target. result/lcov.info = unit ∪ VM, filtered to
  # workspace crates. result/html/ = genhtml. result/per-test/ =
  # individual breakdowns.
  full =
    pkgs.runCommand "rio-coverage-full"
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
        # --extract filters to workspace crates (drops any stray
        # deps that made it through --ignore-filename-regex).
        lcov --extract $TMPDIR/combined.lcov 'rio-*' -o $TMPDIR/extracted.lcov
        # --remove filters out generated build artifacts that
        # --extract let through (rio-proto/target/build/... matches
        # 'rio-*' but the generated .rs doesn't exist in workspaceSrc).
        # crate2nix puts tonic-prost-build output at target/build/;
        # genhtml can't resolve it against workspaceSrc. unused: don't
        # error if pattern
        # doesn't match (clean unit-only runs may not have these).
        lcov --ignore-errors unused \
          --remove $TMPDIR/extracted.lcov '*/target/*' -o $out/lcov.info

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
