# Fuzz target build + check pipeline.
#
# Fuzz crates are their own workspace roots — excluded from the main
# workspace, each with its own Cargo.lock, needing nightly for
# libfuzzer-sys. They depend on in-tree crates by path, so the source
# must include the full workspace Cargo.toml tree.
#
# Two fuzz workspaces:
#   rio-nix/fuzz    — protocol/wire parsers (lean deps, ~70 crates)
#   rio-store/fuzz  — manifest parser (pulls full rio-store dep
#                     tree: tonic, sqlx, aws-sdk-s3, fuse3, protobuf;
#                     ~470 crates)
#
# Build: stdenv.mkDerivation + rustPlatformNightly.importCargoLock +
# cargoSetupHook. `cargo fuzz build` sets its own RUSTFLAGS (sancov
# instrumentation: -Cpasses=sancov-module -Zsanitizer=address etc.),
# so per-crate caching (crate2nix) wouldn't share rlibs with the main
# workspace anyway — the sancov-instrumented object files are
# incompatible. A monolithic cargo-fuzz build is the right shape here.
{
  pkgs,
  rustNightly,
  rustPlatformNightly,
  unfilteredRoot,
  # Full workspace fileset. Narrowed below to manifests-only for crates
  # outside each fuzz workspace's path-dep closure — cargo's resolver
  # needs every [workspace.members] entry's Cargo.toml present (sqlx-
  # macros runs `cargo metadata` to find the workspace root, and that
  # validates all members), but not their source.
  workspaceFileset,
}:
let
  inherit (pkgs.lib) fileset;
  rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;

  # All Cargo.toml files in the workspace. Gives cargo metadata what
  # it needs without dragging in full crate source.
  manifestFiles = fileset.intersection workspaceFileset (
    fileset.fileFilter (f: f.name == "Cargo.toml") unfilteredRoot
  );

  # Builder for a fuzz-workspace build derivation.
  # `fuzzDir` is relative to the repo root (e.g., "rio-nix/fuzz").
  # `cargoLock` is the Nix path to that workspace's lockfile.
  # `targets` is the list of `[[bin]]` names in its Cargo.toml.
  # `pathDeps` is the transitive in-tree path-dep closure (filesets,
  #   relative to unfilteredRoot) — narrowing src to this set instead
  #   of the full workspace means edits to unrelated crates don't
  #   invalidate the ~540-crate sancov-instrumented monolith.
  # `extraNativeBuildInputs` / `extraBuildInputs` extend the base
  #   (rio-store fuzz needs protobuf+cmake+fuse3 because rio-store
  #   transitively builds rio-proto's build.rs and links fuse3).
  mkFuzzWorkspace =
    {
      fuzzDir,
      cargoLock,
      targets,
      pathDeps,
      extraNativeBuildInputs ? [ ],
      extraBuildInputs ? [ ],
    }:
    let
      cargoDeps = rustPlatformNightly.importCargoLock {
        lockFile = cargoLock;
      };
    in
    pkgs.stdenv.mkDerivation {
      pname = "rio-fuzz-${builtins.replaceStrings [ "/" ] [ "-" ] fuzzDir}";
      version = "0.0.0";

      src = fileset.toSource {
        root = unfilteredRoot;
        fileset = fileset.unions (
          [
            (unfilteredRoot + "/Cargo.toml")
            (unfilteredRoot + "/Cargo.lock")
            manifestFiles
            (unfilteredRoot + "/workspace-hack")
            (unfilteredRoot + "/${fuzzDir}/Cargo.toml")
            (unfilteredRoot + "/${fuzzDir}/Cargo.lock")
            (unfilteredRoot + "/${fuzzDir}/fuzz_targets")
          ]
          ++ pathDeps
        );
      };

      # cargo metadata also validates each member's auto-detected
      # targets (src/lib.rs / src/main.rs) exist. Stub them for
      # members whose source we excluded — those crates aren't in the
      # fuzz dep graph so the stubs are never compiled.
      postPatch = ''
        for m in */Cargo.toml; do
          d=''${m%/Cargo.toml}
          [ -d "$d/src" ] && continue
          mkdir -p "$d/src"
          touch "$d/src/lib.rs" "$d/src/main.rs"
        done
      '';

      inherit cargoDeps;
      # cargoSetupHook validates $src/$cargoRoot/Cargo.lock against
      # the vendored lockfile. Without this, it compares the MAIN
      # workspace's Cargo.lock at source root — which differs (the
      # fuzz crate is its own workspace with its own lockfile).
      cargoRoot = fuzzDir;

      nativeBuildInputs =
        (with pkgs; [
          pkg-config
          cargo-fuzz
          rustPlatformNightly.cargoSetupHook
        ])
        ++ [ rustNightly ]
        ++ extraNativeBuildInputs;

      buildInputs =
        (with pkgs; [
          openssl
          llvmPackages.libclang.lib
        ])
        ++ extraBuildInputs;

      LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
      PROTOC = "${pkgs.protobuf}/bin/protoc";
      # sqlx::query! macros read .sqlx/ instead of a live DB. Without
      # this, the rio-store-fuzz build fails on queries.rs with
      # "set DATABASE_URL ... or run cargo sqlx prepare".
      SQLX_OFFLINE = "true";

      # cmake is in nativeBuildInputs for aws-lc-sys's build.rs, not
      # for this derivation's configurePhase. The cmake setup hook
      # auto-injects a cmake configurePhase that looks for
      # CMakeLists.txt at source root — there isn't one.
      dontUseCmakeConfigure = true;

      # cargoSetupHook vendors into cargo-vendor-dir/ at top level;
      # the cd into ${fuzzDir} below means cargo sees the vendored
      # deps via the .cargo/config.toml that cargoSetupHook writes.
      # cargo-fuzz itself doesn't read the hook's config — cargo does.
      #
      # cargo fuzz build sets RUSTFLAGS internally (sancov module,
      # -Zsanitizer=address). --release for optimized throughput;
      # the 2min check runs want coverage, not debug symbols.
      buildPhase = ''
        runHook preBuild
        cd ${fuzzDir}
        cargo fuzz build --release
        runHook postBuild
      '';

      # cargo-fuzz writes binaries to target/<triple>/release/ (the
      # --target flag is implicit — cargo-fuzz always cross-compiles
      # to the host triple so linking libfuzzer works on Linux).
      installPhase = ''
        runHook preInstall
        mkdir -p $out/bin
        for t in ${pkgs.lib.concatStringsSep " " targets}; do
          cp target/${rustTarget}/release/$t $out/bin/
        done
        runHook postInstall
      '';
    };

  # rio-nix fuzz target names. Used both as the `targets` list for
  # the build derivation and to generate the per-target
  # (target, fuzzBuild, corpusRoot) triples below.
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

  # rio-nix fuzz: wire/protocol parsers. Lean — rio-nix has zero
  # in-tree path-deps beyond workspace-hack.
  rio-nix-fuzz-build = mkFuzzWorkspace {
    fuzzDir = "rio-nix/fuzz";
    cargoLock = unfilteredRoot + "/rio-nix/fuzz/Cargo.lock";
    targets = rioNixFuzzTargets;
    pathDeps = [
      (unfilteredRoot + "/rio-nix/src")
      (unfilteredRoot + "/rio-nix/Cargo.toml")
    ];
  };

  # rio-store fuzz: manifest parser. Heavy — pulls in the full
  # rio-store dep tree (it's path = ".." in the fuzz Cargo.toml).
  # Needs the same native deps as the main workspace build. pathDeps
  # closure derived from rio-store/fuzz/Cargo.lock's `name = "rio-*"`
  # entries. .sqlx + migrations: rio-store unconditionally compiles
  # sqlx::query! / sqlx::migrate! which read these at compile time.
  rio-store-fuzz-build = mkFuzzWorkspace {
    fuzzDir = "rio-store/fuzz";
    cargoLock = unfilteredRoot + "/rio-store/fuzz/Cargo.lock";
    targets = [
      "manifest_deserialize"
    ];
    pathDeps = [
      (unfilteredRoot + "/rio-store/src")
      (unfilteredRoot + "/rio-store/Cargo.toml")
      (unfilteredRoot + "/rio-auth")
      (unfilteredRoot + "/rio-common")
      (unfilteredRoot + "/rio-proto")
      (unfilteredRoot + "/rio-nix/src")
      (unfilteredRoot + "/rio-nix/Cargo.toml")
      (unfilteredRoot + "/migrations")
      (fileset.maybeMissing (unfilteredRoot + "/.sqlx"))
    ];
    extraNativeBuildInputs = with pkgs; [
      protobuf
      cmake
    ];
    extraBuildInputs = with pkgs; [
      fuse3
    ];
  };

  # Flat list of (target, fuzzBuild, corpusRoot) for generating
  # the per-target run derivations. All target names must be unique
  # across workspaces (they become attr names in `runs` below).
  fuzzTargets =
    (map (t: {
      target = t;
      fuzzBuild = rio-nix-fuzz-build;
      corpusRoot = unfilteredRoot + "/rio-nix/fuzz/corpus";
    }) rioNixFuzzTargets)
    ++ [
      {
        target = "manifest_deserialize";
        fuzzBuild = rio-store-fuzz-build;
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
      fuzzBuild,
      corpusRoot,
    }:
    let
      seedCorpus = corpusRoot + "/${target}";
      hasCorpus = builtins.pathExists seedCorpus;
    in
    pkgs.runCommand "rio-fuzz-${target}" { } ''
      workCorpus=$(mktemp -d)
      ${pkgs.lib.optionalString hasCorpus ''
        cp -r ${seedCorpus}/. "$workCorpus"/
        chmod -R u+w "$workCorpus"
      ''}

      mkdir -p artifacts

      # -fork=N spawns N libFuzzer workers that share corpus. Cap at
      # 16: wall time is fixed (-max_total_time), so more workers =
      # more inputs covered but also more CPU stolen from the rest of
      # `.#ci` (10 targets × 192 cores = 1920 procs on the big box).
      # Workers write to fuzz-*.log; dump those on failure so crash
      # stacks land in the Nix build log.
      cores=''${NIX_BUILD_CORES:-1}
      ${fuzzBuild}/bin/${target} "$workCorpus" \
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
  # Per-crate build derivations, exposed under legacyPackages.fuzz-builds
  # for debugging (`nix build .#fuzz-builds.rio-nix-fuzz-build && ls result/bin/`).
  builds = { inherit rio-nix-fuzz-build rio-store-fuzz-build; };

  # 2min fuzz runs. Keys: "fuzz-<target>". Spliced into `checks.*`.
  runs = builtins.listToAttrs (
    map (t: {
      name = "fuzz-${t.target}";
      value = mkFuzzCheck t;
    }) fuzzTargets
  );
}
