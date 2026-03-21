# VM test coverage merge pipeline.
#
# Takes profraws from each coverage-mode VM test (collected via
# common.nix's collectCoverage → copy_from_vm → $out/coverage/
# <node>/profraw.tar.gz), converts to lcov via the toolchain's
# llvm-profdata + llvm-cov, normalizes source paths, and unions
# with the unit-test lcov.
#
# Outputs:
#   perTestLcov.vm-phaseXY  — one lcov per VM test
#   vmLcov                  — all VM tests unioned
#   unitLcov                — unit-test lcov, path-normalized
#   full                    — unit ∪ VM, HTML report, per-test breakdown
#
# CRITICAL: use the toolchain-bundled llvm-profdata/llvm-cov, NOT
# system llvm. Profile format versioning is tied to the rustc that
# compiled the instrumented binary.
{
  pkgs,
  rustStable,
  rio-workspace-cov,
  vmTestsCov,
  commonSrc,
  unitCoverage,
  # Source-path normalization pattern. buildRustCrate's
  # --remap-path-prefix maps the sandbox build dir to `/`, so
  # profraws reference `/rio-common/src/lib.rs`. Strip the leading
  # slash to get repo-relative paths genhtml can resolve.
  stripPrefix ? "s|^/||",
  # --ignore-filename-regex for llvm-cov export on VM-test profraws.
  # Dep paths like `/tokio-1.50.0/...` get filtered by the final
  # `lcov --extract 'rio-*'` step; this regex catches common build
  # artifacts that --extract misses.
  ignoreRegex ? "\\.cargo/registry|\\.cargo/git|/rustc/|/nix/store/.*-vendor|target/release/build",
  # Name suffix for derivations. Kept for callers that want
  # multiple coverage pipelines side by side.
  nameSuffix ? "",
}:
let
  inherit (pkgs) lib;

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
    "worker"
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
  mkPerTestLcov =
    name: vmTest:
    pkgs.runCommand "rio-cov-${name}${nameSuffix}" { } ''
      mkdir -p $TMPDIR/raw
      found=0
      for tarball in $(find ${vmTest}/coverage -name profraw.tar.gz 2>/dev/null); do
        # --skip-old-files: in case two nodes have the same
        # profraw filename (different VMs but same PID+module
        # signature — unlikely but possible).
        tar xzf "$tarball" -C $TMPDIR/raw --skip-old-files 2>/dev/null || true
        found=1
      done
      # Check for actual profraw files (tarballs may be empty).
      # nullglob: if no match, the glob expands to nothing instead
      # of a literal '*.profraw' — makes the array-length check
      # below reliable regardless of bash globbing defaults.
      shopt -s nullglob
      profraws=($TMPDIR/raw/*.profraw)
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
      # output). Source doesn't exist in commonSrc (build artifact),
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
  vmLcov = pkgs.runCommand "rio-cov-vm-total${nameSuffix}" { nativeBuildInputs = [ pkgs.lcov ]; } ''
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

  # Unit-test lcov, path-normalized the same way. Same `-a` trick.
  # --ignore-errors unused: crate2nix's unit lcov is already
  # repo-relative (c2n-checks.nix does `lcov --substitute 's|^/||'`)
  # so this pattern may not match — harmless, skip the error.
  unitLcov =
    pkgs.runCommand "rio-cov-unit-clean${nameSuffix}" { nativeBuildInputs = [ pkgs.lcov ]; }
      ''
        lcov --ignore-errors unused \
          --substitute '${stripPrefix}' \
          -a ${unitCoverage}/lcov.info -o $out
      '';
in
{
  inherit perTestLcov vmLcov unitLcov;

  # The headline target. result/lcov.info = unit ∪ VM, filtered to
  # workspace crates. result/html/ = genhtml. result/per-test/ =
  # individual breakdowns.
  full =
    pkgs.runCommand "rio-coverage-full${nameSuffix}"
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
        lcov --extract $TMPDIR/combined.lcov 'rio-*' -o $out/lcov.info

        # HTML report. cd into source so genhtml can find files
        # for the source view. --ignore-errors source: safety net
        # for any remaining build-time-generated paths that slip
        # through the regex (genhtml synthesizes a placeholder).
        cd ${commonSrc}
        genhtml $out/lcov.info --output-directory $out/html \
          --ignore-errors source

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
