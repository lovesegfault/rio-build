# Fuzz target build + check pipeline.
#
# Extracted from flake.nix for the same reason as nix/docker.nix: ~200
# lines of self-contained machinery that doesn't belong in the flake root.
#
# Fuzz crates are their own workspace roots — excluded from the main
# workspace, each with its own Cargo.lock, needing nightly for
# libfuzzer-sys. They depend on in-tree crates by path, so the source
# must include the full workspace Cargo.toml tree. We vendor from the
# fuzz-specific lockfile.
#
# Two fuzz workspaces:
#   rio-nix/fuzz    — protocol/wire parsers (lean deps)
#   rio-store/fuzz  — manifest parser (pulls full rio-store dep
#                     tree: tonic, sqlx, aws-sdk-s3, fuse3, protobuf)
{
  pkgs,
  craneLib,
  craneLibNightly,
  unfilteredRoot,
}:
let
  rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;

  # Builder for a fuzz-workspace build derivation + its runner.
  # `fuzzDir` is relative to the repo root (e.g., "rio-nix/fuzz").
  # `cargoLock` is the Nix path to that workspace's lockfile.
  # `targets` is the list of `[[bin]]` names in its Cargo.toml.
  # `extraNativeBuildInputs` / `extraBuildInputs` extend the base
  #   (rio-store fuzz needs protobuf+cmake+fuse3 because rio-store
  #   transitively builds rio-proto's build.rs and links fuse3).
  mkFuzzWorkspace =
    {
      fuzzDir,
      cargoLock,
      targets,
      extraNativeBuildInputs ? [ ],
      extraBuildInputs ? [ ],
    }:
    let
      src = pkgs.lib.fileset.toSource {
        root = unfilteredRoot;
        fileset = pkgs.lib.fileset.unions [
          (craneLib.fileset.commonCargoSources unfilteredRoot)
          cargoLock
          # rio-store fuzz transitively builds rio-proto — needs
          # .proto sources for prost codegen.
          (pkgs.lib.fileset.fileFilter (file: file.hasExt "proto") unfilteredRoot)
        ];
      };

      fuzzArgs = {
        inherit src;
        strictDeps = true;
        pname = "rio-fuzz-${builtins.replaceStrings [ "/" ] [ "-" ] fuzzDir}";
        version = "0.0.0";

        cargoVendorDir = craneLibNightly.vendorCargoDeps {
          inherit cargoLock;
        };

        nativeBuildInputs =
          (with pkgs; [
            pkg-config
            cargo-fuzz
          ])
          ++ extraNativeBuildInputs;

        buildInputs =
          (with pkgs; [
            openssl
            llvmPackages.libclang.lib
          ])
          ++ extraBuildInputs;

        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
        PROTOC = "${pkgs.protobuf}/bin/protoc";
      };
    in
    # Compile all fuzz target binaries with sancov instrumentation.
    # Expensive but cached by source hash; shared by all fuzz runs.
    #
    # No dep-layer (cargoArtifacts=null): cargo-fuzz's sancov flags
    # produce incompatible object files with a non-instrumented
    # buildDepsOnly layer, so dep caching would be a pure miss.
    craneLibNightly.mkCargoDerivation (
      fuzzArgs
      // {
        cargoArtifacts = null;

        buildPhaseCargoCommand = ''
          cd ${fuzzDir}
          cargo fuzz build --release
        '';

        doInstallCargoArtifacts = false;
        installPhaseCommand = ''
          mkdir -p $out/bin
          for t in ${pkgs.lib.concatStringsSep " " targets}; do
            cp target/${rustTarget}/release/$t $out/bin/
          done
        '';
      }
    );

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
  ];

  # rio-nix fuzz: wire/protocol parsers. Lean — rio-nix has few deps.
  rio-nix-fuzz-build = mkFuzzWorkspace {
    fuzzDir = "rio-nix/fuzz";
    cargoLock = unfilteredRoot + "/rio-nix/fuzz/Cargo.lock";
    targets = rioNixFuzzTargets;
  };

  # rio-store fuzz: manifest parser. Heavy — pulls in the full
  # rio-store dep tree (it's path = ".." in the fuzz Cargo.toml).
  # Needs the same native deps as the main workspace build.
  rio-store-fuzz-build = mkFuzzWorkspace {
    fuzzDir = "rio-store/fuzz";
    cargoLock = unfilteredRoot + "/rio-store/fuzz/Cargo.lock";
    targets = [
      "manifest_deserialize"
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

      # -fork=N spawns N libFuzzer workers that share corpus.
      # Workers write to fuzz-*.log; dump those on failure so
      # crash stacks land in the Nix build log.
      ${fuzzBuild}/bin/${target} "$workCorpus" \
        -max_total_time=120 \
        -timeout=30 \
        -print_final_stats=1 \
        -artifact_prefix=artifacts/ \
        -fork=''${NIX_BUILD_CORES:-1} || {
          echo "--- worker logs ---"
          cat fuzz-*.log 2>/dev/null || true
          exit 1
        }

      echo "${target}: 120s, no crashes" > $out
    '';
in
{
  # Per-crate build derivations, exposed as packages.fuzz-build-{nix,store}
  # for debugging (`nix build .#fuzz-build-nix && ls result/bin/`).
  builds = { inherit rio-nix-fuzz-build rio-store-fuzz-build; };

  # 2min fuzz runs (Linux-only — libFuzzer).
  # Keys: "fuzz-<target>". Spliced into `checks.*`.
  runs = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
    builtins.listToAttrs (
      map (t: {
        name = "fuzz-${t.target}";
        value = mkFuzzCheck t;
      }) fuzzTargets
    )
  );
}
