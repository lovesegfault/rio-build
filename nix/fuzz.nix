# Fuzz target build + check pipeline.
#
# Fuzz crates are their own workspace roots — excluded from the main
# workspace, each with its own Cargo.lock, needing nightly for
# libfuzzer-sys + `-Zsanitizer`. They depend on in-tree crates by path.
#
# Two fuzz workspaces:
#   rio-nix/fuzz    — protocol/wire parsers (~450 sancov crates)
#   rio-store/fuzz  — manifest parser (~590 sancov crates; pulls
#                     rio-store's full dep tree)
#
# Build: per-crate via crate2nix (third + fourth instantiations
# alongside the main + coverage trees). Same `globalExtraRustcOpts`
# mechanism as crateBuildCov, but with the exact RUSTFLAGS that
# `cargo fuzz build --release` injects (extracted from cargo-fuzz
# src/project.rs — see `fuzzRustcOpts` below).
#
# The sancov-instrumented rlibs CAN'T share with the release tree
# (different codegen), but they cache per-crate within their own tree:
# editing rio-nix/src rebuilds rio-nix-sancov + the rio-nix-fuzz
# member (one drv → 9 bins), not the ~450 transitive deps.
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

  rio-nix-fuzz-build = mkFuzzBuild {
    resolvedJson = ../rio-nix/fuzz/Cargo.json;
    fuzzCrateName = "rio-nix-fuzz";
    fuzzCrateSrc = fileset.toSource {
      root = ../rio-nix/fuzz;
      fileset = fileset.unions [
        ../rio-nix/fuzz/Cargo.toml
        ../rio-nix/fuzz/fuzz_targets
      ];
    };
  };

  rio-store-fuzz-build = mkFuzzBuild {
    resolvedJson = ../rio-store/fuzz/Cargo.json;
    fuzzCrateName = "rio-store-fuzz";
    fuzzCrateSrc = fileset.toSource {
      root = ../rio-store/fuzz;
      fileset = fileset.unions [
        ../rio-store/fuzz/Cargo.toml
        ../rio-store/fuzz/fuzz_targets
      ];
    };
  };

  rioNixFuzzTargets = [
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

  # Flat list of (target, fuzzBins, corpusRoot) for generating the
  # per-target run derivations. All target names must be unique
  # across workspaces (they become attr names in `runs` below).
  # `fuzzBins` is the workspace member's built crate — buildRustCrate
  # puts every `[[bin]]` under $out/bin/.
  fuzzTargets =
    (map (t: {
      target = t;
      fuzzBins = rio-nix-fuzz-build.members.rio-nix-fuzz;
      corpusRoot = unfilteredRoot + "/rio-nix/fuzz/corpus";
    }) rioNixFuzzTargets)
    ++ [
      {
        target = "manifest_deserialize";
        fuzzBins = rio-store-fuzz-build.members.rio-store-fuzz;
        corpusRoot = unfilteredRoot + "/rio-store/fuzz/corpus";
      }
    ];

  # Per-target fuzz run: 2 minutes, seed-corpus only. Cheap
  # runCommand wrapper over the prebuilt binary. For deep runs
  # with accumulated corpus, `cd <crate>/fuzz && cargo fuzz run`
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
      # the checks gate (10 targets × 192 cores = 1920 procs on the
      # big box).
      # Workers write to fuzz-*.log; dump those on failure so crash
      # stacks land in the Nix build log.
      cores=''${NIX_BUILD_CORES:-1}
      ${fuzzBins}/bin/${target} "$workCorpus" \
        -max_total_time=120 \
        -timeout=30 \
        -print_final_stats=1 \
        -artifact_prefix=artifacts/ \
        -fork=$(( cores <= 16 ? cores : 16 )) || {
          echo "--- worker logs ---"
          cat fuzz-*.log 2>/dev/null || true
          exit 1
        }

      echo "${target}: 120s, no crashes" > $out
    '';
in
{
  # Per-workspace crate2nix tree, exposed under
  # legacyPackages.fuzz-builds for debugging
  # (`nix build .#fuzz-builds.rio-nix-fuzz && ls result/bin/`).
  builds = {
    inherit (rio-nix-fuzz-build.members) rio-nix-fuzz;
    inherit (rio-store-fuzz-build.members) rio-store-fuzz;
  };

  # 2min fuzz runs. Keys: "fuzz-<target>". Spliced into `checks.*`.
  runs = builtins.listToAttrs (
    map (t: {
      name = "fuzz-${t.target}";
      value = mkFuzzCheck t;
    }) fuzzTargets
  );
}
