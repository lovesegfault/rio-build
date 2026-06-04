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
# Membership is NOT free-floating: the derived on-disk set
# (filesets.nix fuzzWorkspaces) is asserted equal to Cargo.toml's
# `[workspace] exclude` — the two views (disk, cargo/xtask) cannot
# diverge silently. Per-workspace config (member crate + targets) is
# DERIVED from the committed fuzz/<ws>/Cargo.json, not hand-listed.
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
  # bi-source membership assert below.
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

  # Bi-source membership assert: the derived on-disk set must equal
  # the fuzz/-prefixed `[workspace] exclude` entries. Forced through
  # every consumer (fuzzWorkspaceConfig below is genAttrs over this
  # list, and builds/targets/runs all read it), so a divergence fails
  # EVAL with this named error instead of a workspace silently fuzzing
  # stale (or not at all).
  checkedFuzzWorkspaces =
    let
      diskSet = lib.naturalSort fuzzWorkspaces;
      excludeSet = lib.naturalSort (
        map (lib.removePrefix "fuzz/") (lib.filter (lib.hasPrefix "fuzz/") workspaceExclude)
      );
    in
    if diskSet != excludeSet then
      throw ''
        nix/fuzz.nix: fuzz workspace sets diverged —
          fuzz/<dir>/Cargo.toml on disk:          [${toString diskSet}]
          Cargo.toml [workspace] exclude (fuzz/): [${toString excludeSet}]
        Adding/removing a fuzz workspace needs both: the fuzz/<ws>/
        directory (Cargo.{toml,lock,json} + fuzz_targets/, tracked by
        git — an un-added dir is invisible to flake eval) and the
        `fuzz/<ws>` line in Cargo.toml's [workspace] exclude (what
        `cargo xtask regen cargo-json`/`regen fuzz-lock` discover).
      ''
    else
      diskSet;

  # Per-workspace fuzz config, keyed by fuzz/<ws> directory name —
  # DERIVED from the workspace's committed Cargo.json (the same file
  # fuzzBuilds consumes for the build graph, so the json is the single
  # source for both graph and target membership; the
  # crate2nix-drift-fuzz-<ws> checks and the crate2nix-check hook gate
  # its staleness). `fuzzCrateName` is the sole `workspaceMembers` key
  # (a fuzz workspace contains exactly one member crate); `targets`
  # are that crate's `crateBin[].name` — the [[bin]] fuzz targets,
  # which become the `fuzz-<target>` attr names in `runs` below
  # (uniqueness ACROSS workspaces is enforced at fuzzTargets). A
  # [[bin]] added to fuzz/<ws>/Cargo.toml reaches `runs` via
  # `cargo xtask regen cargo-json` — no hand mirror to forget.
  fuzzWorkspaceConfig = lib.genAttrs checkedFuzzWorkspaces (
    ws:
    let
      json = builtins.fromJSON (builtins.readFile (unfilteredRoot + "/fuzz/${ws}/Cargo.json"));
      memberNames = lib.attrNames json.workspaceMembers;
      fuzzCrateName =
        if lib.length memberNames == 1 then
          lib.head memberNames
        else
          throw "nix/fuzz.nix: fuzz/${ws}/Cargo.json has ${toString (lib.length memberNames)} workspace members [${toString memberNames}] — a fuzz workspace must contain exactly one member crate (one [package] per fuzz/<ws>; then `cargo xtask regen cargo-json`)";
      member =
        lib.findFirst (c: c.crateName == fuzzCrateName && (c.source.type or "") == "local")
          (throw "nix/fuzz.nix: fuzz/${ws}/Cargo.json workspace member ${fuzzCrateName} has no local crate entry — regenerate with `cargo xtask regen cargo-json`")
          (lib.attrValues json.crates);
    in
    {
      inherit fuzzCrateName;
      targets =
        let
          names = lib.naturalSort (map (b: b.name) (member.crateBin or [ ]));
        in
        if names == [ ] then
          throw "nix/fuzz.nix: fuzz/${ws}/Cargo.json records no [[bin]] targets for ${fuzzCrateName} — a fuzz workspace with nothing to fuzz is almost certainly a stale Cargo.json; run `cargo xtask regen cargo-json`"
        else
          names;
    }
  );

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
  ) fuzzWorkspaceConfig;

  # Flat list of (target, fuzzBins, corpusRoot) for generating the
  # per-target run derivations, derived from the keyed config.
  # `fuzzBins` is the workspace member's built crate — buildRustCrate
  # puts every `[[bin]]` under $out/bin/.
  #
  # Checked binding: target names must be unique ACROSS workspaces —
  # `runs` keys are `fuzz-<target>` and builtins.listToAttrs is
  # silently first-wins on duplicates, so without this throw a
  # cross-workspace duplicate would drop one workspace's run from
  # checks and from the CI fuzz matrix (flake.nix `fuzz = fuzz.runs`).
  fuzzTargets =
    let
      flat = lib.concatLists (
        lib.mapAttrsToList (
          ws: cfg:
          map (t: {
            target = t;
            fuzzBins = fuzzBuilds.${ws}.members.${cfg.fuzzCrateName};
            corpusRoot = unfilteredRoot + "/fuzz/${ws}/corpus";
          }) cfg.targets
        ) fuzzWorkspaceConfig
      );
      names = lib.naturalSort (map (t: t.target) flat);
      dups = lib.unique (lib.filter (n: lib.count (x: x == n) names > 1) names);
    in
    if dups != [ ] then
      throw "nix/fuzz.nix: duplicate fuzz target name(s) [${toString dups}] across fuzz workspaces — runs keys are fuzz-<target> and builtins.listToAttrs is silently first-wins, so a duplicate would drop one workspace's run from checks and the CI fuzz matrix (flake.nix `fuzz = fuzz.runs`). Rename the [[bin]] in one workspace, then `cargo xtask regen cargo-json`."
    else
      flat;

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
