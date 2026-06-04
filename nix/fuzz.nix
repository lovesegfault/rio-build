# Fuzz target build + check pipeline.
#
# Fuzz crates are their own workspace roots — excluded from the main
# workspace, each with its own Cargo.lock, needing nightly for
# libfuzzer-sys + `-Zsanitizer`. They depend on in-tree crates by path.
#
# Two fuzz workspaces today:
#   fuzz/rio-nix    — protocol/wire parsers
#   fuzz/rio-store  — manifest parser (pulls rio-store's full dep
#                     tree, so its sancov closure is much larger)
# Membership is NOT free-floating: fuzzWorkspaceConfig below is
# asserted equal to the derived on-disk set (filesets.nix
# fuzzWorkspaces) and to Cargo.toml's `[workspace] exclude` — the
# three views (nix config, disk, cargo/xtask) cannot diverge silently.
#
# Build: per-crate via crate2nix (third + fourth instantiations
# alongside the main + coverage trees). Same `localExtraRustcOpts`
# mechanism as crateBuildCov, but with the exact RUSTFLAGS that
# `cargo fuzz build --release` injects (extracted from cargo-fuzz
# src/project.rs — see `fuzzRustcOpts` below).
#
# The sancov-instrumented rlibs CAN'T share with the release tree
# (different codegen), but they cache per-crate within their own tree:
# editing rio-nix/src rebuilds rio-nix-sancov + the rio-nix-fuzz
# member (one drv, all its [[bin]] targets), not the hundreds of
# transitive sancov deps.
{
  pkgs,
  lib,
  rustNightly,
  crate2nixSrc,
  sysCrateEnv,
  unfilteredRoot,
  # Main-workspace per-crate srcs (rio-nix, rio-store, rio-auth, …).
  # Reused so the fuzz tree's path-dep crates get the SAME isolated
  # store hashes as the release tree's — editing rio-cli leaves
  # rio-nix-sancov untouched.
  memberSrcs,
  # Derived on-disk fuzz workspace list (nix/lib/filesets.nix:
  # fuzz/<dir> entries containing a Cargo.toml). One leg of the
  # tri-source membership assert below.
  fuzzWorkspaces,
  # Cargo.toml `[workspace] exclude` — the list xtask's discover_dirs()
  # iterates for `regen cargo-json`/`regen fuzz-lock`. Second assert leg.
  workspaceExclude,
}:
let
  inherit (lib) fileset;

  # cargo-fuzz's RUSTFLAGS for `cargo fuzz build --release` on Linux,
  # default sanitizer (address). Extracted from cargo-fuzz
  # src/project.rs::cargo() — defaults: trace-compares ON, branch-
  # folding disabled, stack-depth ON (linux-only), cfg fuzzing ON,
  # debug-assertions OFF (--release), codegen-units=1 (release default
  # — sancov breaks ThinLTO function imports otherwise).
  # has_sanitizers_on_stable() is u32::MAX-gated → -Zsanitizer (not
  # -Csanitizer) and no -Zunstable-options.
  fuzzRustcOpts = [
    "-Cpasses=sancov-module"
    "-Cllvm-args=-sanitizer-coverage-level=4"
    "-Cllvm-args=-sanitizer-coverage-inline-8bit-counters"
    "-Cllvm-args=-sanitizer-coverage-pc-table"
    "-Cllvm-args=-sanitizer-coverage-trace-compares"
    "--cfg=fuzzing"
    "-Cllvm-args=-simplifycfg-branch-fold-threshold=0"
    "-Zsanitizer=address"
    "-Cllvm-args=-sanitizer-coverage-stack-depth"
    "-Ccodegen-units=1"
  ];

  # Per-fuzz-workspace crate2nix instantiation. `resolvedJson` is the
  # workspace's checked-in Cargo.json (regenerated alongside the root
  # one by `cargo xtask regen cargo-json`). `fuzzCrateSrc` is the
  # fileset for the fuzz crate itself; `memberSrcs` covers the
  # path-dep in-tree crates (and workspace-hack, which the
  # crate2nix.nix interceptor stubs to zero deps regardless).
  #
  # `workspaceSrc` is set to the fuzz crate's own src — this is what
  # build-from-json.nix uses for `source.path: "."` (the fuzz crate).
  # All other locals (rio-nix, rio-store, …) have `source.path:
  # "../.."`-style paths that would resolve outside the store, but the
  # `memberSrcs` interceptor in crate2nix.nix replaces those by
  # crateName before build-from-json's bad path is ever read.
  mkFuzzBuild =
    {
      resolvedJson,
      fuzzCrateName,
      fuzzCrateSrc,
    }:
    import ./crate2nix.nix {
      inherit
        pkgs
        lib
        sysCrateEnv
        crate2nixSrc
        resolvedJson
        ;
      rust = rustNightly;
      localExtraRustcOpts = fuzzRustcOpts;
      workspaceSrc = fuzzCrateSrc;
      memberSrcs = memberSrcs // {
        ${fuzzCrateName} = fuzzCrateSrc;
      };
      # libFuzzer's main() is in the asan-linked binary; stripping
      # would drop the sancov counter sections libFuzzer reads.
      stripBins = false;
    };

  # Per-workspace fuzz config, keyed by fuzz/<ws> directory name.
  # `fuzzCrateName` is the workspace member crate ([package] name in
  # fuzz/<ws>/Cargo.toml); `targets` are its [[bin]] fuzz targets
  # (names must be unique ACROSS workspaces — they become the
  # `fuzz-<target>` attr names in `runs` below).
  fuzzWorkspaceConfig = {
    rio-nix = {
      fuzzCrateName = "rio-nix-fuzz";
      targets = [
        "wire_primitives"
        "opcode_parsing"
        "derivation_parsing"
        "nar_parsing"
        "derived_path_parsing"
        "narinfo_parsing"
        "build_result_parsing"
        "refscan"
        "stderr_message_parsing"
      ];
    };
    rio-store = {
      fuzzCrateName = "rio-store-fuzz";
      targets = [ "manifest_deserialize" ];
    };
  };

  # Tri-source membership assert: the hand config above must equal the
  # derived on-disk set AND the fuzz/-prefixed `[workspace] exclude`
  # entries. Forced through every consumer (builds/targets/runs all
  # read checkedFuzzWorkspaceConfig), so a divergence fails EVAL with
  # this named error instead of a workspace silently fuzzing stale (or
  # not at all).
  checkedFuzzWorkspaceConfig =
    let
      configSet = lib.naturalSort (lib.attrNames fuzzWorkspaceConfig);
      diskSet = lib.naturalSort fuzzWorkspaces;
      excludeSet = lib.naturalSort (
        map (lib.removePrefix "fuzz/") (lib.filter (lib.hasPrefix "fuzz/") workspaceExclude)
      );
    in
    if configSet != diskSet || configSet != excludeSet then
      throw ''
        nix/fuzz.nix: fuzz workspace sets diverged —
          fuzzWorkspaceConfig (nix/fuzz.nix):     [${toString configSet}]
          fuzz/<dir>/Cargo.toml on disk:          [${toString diskSet}]
          Cargo.toml [workspace] exclude (fuzz/): [${toString excludeSet}]
        Adding/removing a fuzz workspace needs all three: the fuzz/<ws>/
        directory (Cargo.{toml,lock,json} + fuzz_targets/, tracked by
        git — an un-added dir is invisible to flake eval), a
        fuzzWorkspaceConfig entry here (crate name + targets), and the
        `fuzz/<ws>` line in Cargo.toml's [workspace] exclude (what
        `cargo xtask regen cargo-json`/`regen fuzz-lock` discover).
      ''
    else
      fuzzWorkspaceConfig;

  # One crate2nix instantiation per fuzz workspace.
  fuzzBuilds = lib.mapAttrs (
    ws: cfg:
    mkFuzzBuild {
      resolvedJson = unfilteredRoot + "/fuzz/${ws}/Cargo.json";
      inherit (cfg) fuzzCrateName;
      fuzzCrateSrc = fileset.toSource {
        root = unfilteredRoot + "/fuzz/${ws}";
        fileset = fileset.unions [
          (unfilteredRoot + "/fuzz/${ws}/Cargo.toml")
          (unfilteredRoot + "/fuzz/${ws}/fuzz_targets")
        ];
      };
    }
  ) checkedFuzzWorkspaceConfig;

  # Flat list of (target, fuzzBins, corpusRoot) for generating the
  # per-target run derivations, derived from the keyed config.
  # `fuzzBins` is the workspace member's built crate — buildRustCrate
  # puts every `[[bin]]` under $out/bin/.
  fuzzTargets = lib.concatLists (
    lib.mapAttrsToList (
      ws: cfg:
      map (t: {
        target = t;
        fuzzBins = fuzzBuilds.${ws}.members.${cfg.fuzzCrateName};
        corpusRoot = unfilteredRoot + "/fuzz/${ws}/corpus";
      }) cfg.targets
    ) checkedFuzzWorkspaceConfig
  );

  # Per-target fuzz run: 2 minutes, seed-corpus only. Cheap
  # runCommand wrapper over the prebuilt binary. For deep runs
  # with accumulated corpus, `cd fuzz/<crate> && cargo fuzz run`
  # in the dev shell (libFuzzer persists corpus in ./corpus/).
  mkFuzzCheck =
    {
      target,
      fuzzBins,
      corpusRoot,
    }:
    let
      seedCorpus = corpusRoot + "/${target}";
      hasCorpus = builtins.pathExists seedCorpus;
    in
    pkgs.runCommand "rio-fuzz-${target}" { } ''
      workCorpus=$(mktemp -d)
      ${lib.optionalString hasCorpus ''
        cp -r ${seedCorpus}/. "$workCorpus"/
        chmod -R u+w "$workCorpus"
      ''}

      mkdir -p artifacts

      # -fork=N spawns N libFuzzer workers that share corpus. Cap at
      # 16: wall time is fixed (-max_total_time), so more workers =
      # more inputs covered but also more CPU stolen from the rest of
      # the checks gate (one fork pool per fuzz target, all running
      # concurrently — uncapped that's targets × cores procs on the
      # big box).
      # In -fork mode each worker's stdout/stderr goes to
      # $TMPDIR/libFuzzerTemp.FuzzWithFork<pid>.dir/<job>.log — NOT to
      # ./fuzz-*.log (that's -jobs mode). The parent only echoes the
      # log of the job it decides crashed; a worker that dies before
      # producing an artifact (sanitizer init failure, LSan fatal
      # error under a restrictive seccomp profile, OOM) leaves its
      # diagnostics only in those files. Dump every worker log on
      # failure so the crash stacks land in the Nix build log.
      cores=''${NIX_BUILD_CORES:-1}
      ${fuzzBins}/bin/${target} "$workCorpus" \
        -max_total_time=120 \
        -timeout=30 \
        -print_final_stats=1 \
        -artifact_prefix=artifacts/ \
        -fork=$(( cores <= 16 ? cores : 16 )) || {
          echo "--- fork-mode worker logs ---"
          found=
          for log in "''${TMPDIR:-/tmp}"/libFuzzerTemp.*/*.log; do
            [ -e "$log" ] || continue
            found=1
            echo "=== $log ==="
            cat "$log"
          done
          [ -n "$found" ] || echo "(no libFuzzerTemp.*/*.log found under ''${TMPDIR:-/tmp})"
          exit 1
        }

      echo "${target}: 120s, no crashes" > $out
    '';
in
{
  # 2min fuzz runs. Keys: "fuzz-<target>". Spliced into `checks.*`.
  # The compiled fuzz binaries (fuzzBuilds.<ws>.members.<crate>) are
  # run-time inputs of these derivations, not re-exported standalone.
  runs = builtins.listToAttrs (
    map (t: {
      name = "fuzz-${t.target}";
      value = mkFuzzCheck t;
    }) fuzzTargets
  );
}
