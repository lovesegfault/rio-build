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
    # Expensive but cached by source hash; shared by both the 30s
    # PR-tier checks and the 600s nightly runs.
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
  # the per-target smoke/nightly derivations. All target names
  # must be unique across workspaces (they become attr names in
  # `smoke` / `nightly` below).
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

  # Per-target, per-time-budget fuzz run. Cheap runCommand
  # wrapper over the prebuilt binary. The 30s and 600s variants
  # share the same fuzzBuild derivation.
  #
  # `s3CorpusSync`: when true, sync corpus from/to
  # $RIO_FUZZ_CORPUS_S3/<target>/ around the run. Nightly
  # builds set this so corpus persists across runs (coverage
  # accumulates instead of rediscovering the same inputs).
  # Smoke builds skip it — 30s is too short to care, and smoke
  # runs in PR CI which may not have AWS creds.
  #
  # Guard on $RIO_FUZZ_CORPUS_S3: unset = no-op (local runs
  # unaffected). CI sets it to s3://bucket/rio-fuzz-corpus/.
  mkFuzzCheck =
    {
      target,
      fuzzBuild,
      corpusRoot,
      maxTime,
      s3CorpusSync ? false,
    }:
    let
      seedCorpus = corpusRoot + "/${target}";
      hasCorpus = builtins.pathExists seedCorpus;
      # awscli2 for S3 sync, only when s3CorpusSync is true.
      # Added as buildInput (not nativeBuild — runs at check
      # time, not build time, but buildInputs is the one that
      # lands in the runCommand environment).
      syncInputs = pkgs.lib.optional s3CorpusSync pkgs.awscli2;
    in
    pkgs.runCommand "rio-fuzz-${target}-${toString maxTime}s"
      {
        buildInputs = syncInputs;
        # Pass through for impure builds (--impure). In pure
        # Nix sandbox this env var is filtered out → sync skipped.
        # Nightly CI runs with --impure so the var flows through.
        passthru.s3CorpusSync = s3CorpusSync;
      }
      ''
        workCorpus=$(mktemp -d)
        ${pkgs.lib.optionalString hasCorpus ''
          cp -r ${seedCorpus}/. "$workCorpus"/
          chmod -R u+w "$workCorpus"
        ''}

        ${pkgs.lib.optionalString s3CorpusSync ''
          # S3 corpus download (best-effort: `|| true` on sync
          # failure — don't block fuzz on S3 hiccups, just run
          # with seed corpus only). `|| true` also covers
          # "RIO_FUZZ_CORPUS_S3 unset" → aws errors → no-op.
          if [ -n "''${RIO_FUZZ_CORPUS_S3:-}" ]; then
            echo "Syncing corpus from $RIO_FUZZ_CORPUS_S3/${target}/"
            aws s3 sync "$RIO_FUZZ_CORPUS_S3/${target}/" "$workCorpus/" \
              --no-progress 2>&1 || echo "(corpus download failed, using seeds only)"
          fi
        ''}

        mkdir -p artifacts

        # -fork=N spawns N libFuzzer workers that share corpus.
        # Workers write to fuzz-*.log; dump those on failure so
        # crash stacks land in the Nix build log.
        ${fuzzBuild}/bin/${target} "$workCorpus" \
          -max_total_time=${toString maxTime} \
          -timeout=30 \
          -print_final_stats=1 \
          -artifact_prefix=artifacts/ \
          -fork=''${NIX_BUILD_CORES:-1} || {
            echo "--- worker logs ---"
            cat fuzz-*.log 2>/dev/null || true
            exit 1
          }

        ${pkgs.lib.optionalString s3CorpusSync ''
          # S3 corpus upload (after successful run — if the fuzz
          # crashed, the `|| exit 1` above already bailed). Upload
          # EVERYTHING (seed + discovered). Duplicates are handled
          # by S3 (same content = same key if you use content
          # addressing, or just overwrites same-named).
          if [ -n "''${RIO_FUZZ_CORPUS_S3:-}" ]; then
            echo "Uploading corpus to $RIO_FUZZ_CORPUS_S3/${target}/"
            aws s3 sync "$workCorpus/" "$RIO_FUZZ_CORPUS_S3/${target}/" \
              --no-progress 2>&1 || echo "(corpus upload failed, next run re-discovers)"
          fi
        ''}

        echo "${target}: ${toString maxTime}s, no crashes" > $out
      '';
in
{
  # Per-crate build derivations, exposed as packages.fuzz-build-{nix,store}
  # for debugging (`nix build .#fuzz-build-nix && ls result/bin/`).
  builds = { inherit rio-nix-fuzz-build rio-store-fuzz-build; };

  # 30s PR-tier fuzz smokes (Linux-only — libFuzzer).
  # Keys: "fuzz-smoke-<target>". Spliced into `checks.*`.
  smoke = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
    builtins.listToAttrs (
      map (t: {
        name = "fuzz-smoke-${t.target}";
        value = mkFuzzCheck (t // { maxTime = 30; });
      }) fuzzTargets
    )
  );

  # 10min nightly-tier fuzz runs (Linux-only).
  # Keys: "fuzz-nightly-<target>". Spliced into `packages.*`.
  #
  # s3CorpusSync=true: corpus persists across runs via
  # $RIO_FUZZ_CORPUS_S3 (CI sets it). Coverage accumulates —
  # each nightly run extends the corpus instead of rediscovering
  # from seeds. Smoke (30s) skips sync — too short to care,
  # and PR CI may lack AWS creds.
  nightly = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
    builtins.listToAttrs (
      map (t: {
        name = "fuzz-nightly-${t.target}";
        value = mkFuzzCheck (
          t
          // {
            maxTime = 600;
            s3CorpusSync = true;
          }
        );
      }) fuzzTargets
    )
  );
}
