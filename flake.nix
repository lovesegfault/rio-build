{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.33.3";
      inputs = {
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks-nix.flakeModule
      ];

      # NixOS modules for deploying rio services. These are consumed by the
      # phase-milestone VM tests (nix/tests/phase*.nix) and can be reused
      # for real deployments. Each module reads `services.rio.package` for
      # binaries, so callers must set that to a workspace build.
      flake.nixosModules = {
        store = ./nix/modules/store.nix;
        scheduler = ./nix/modules/scheduler.nix;
        gateway = ./nix/modules/gateway.nix;
        worker = ./nix/modules/worker.nix;
      };

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          # Read version from Cargo.toml
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          inherit (cargoToml.workspace.package) version;

          # --------------------------------------------------------------
          # Rust toolchains
          # --------------------------------------------------------------
          #
          # Stable: single source of truth for CI (clippy, nextest,
          # workspace build, coverage, docs). Read from
          # rust-toolchain.toml so `rustup` users and Nix users agree.
          # Guarantees releases are stable-compatible.
          rustStable = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # Nightly: used by the default dev shell and fuzz builds.
          # selectLatestNightlyWith auto-picks the most recent nightly
          # that has all requested components, so we're never blocked on
          # a bad nightly.
          #
          # NOTE: non-hermetic by design — bumping rust-overlay changes
          # the nightly date and invalidates the fuzz-build cache. If
          # this becomes a problem, pin to rust-bin.nightly."YYYY-MM-DD".
          rustNightly = pkgs.rust-bin.selectLatestNightlyWith (
            toolchain:
            toolchain.default.override {
              extensions = [
                "rust-src"
                "llvm-tools-preview"
                "rustfmt"
                "clippy"
                "rust-analyzer"
              ];
            }
          );

          # Crane for CI: stable toolchain, reproducible.
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustStable;

          # Crane for fuzz builds + default dev shell: nightly.
          craneLibNightly = (inputs.crane.mkLib pkgs).overrideToolchain rustNightly;

          # Source root for filesets
          unfilteredRoot = ./.;

          # Common arguments for all crane builds
          commonArgs = {
            src = pkgs.lib.fileset.toSource {
              root = unfilteredRoot;
              fileset = pkgs.lib.fileset.unions [
                # All standard Cargo sources (Cargo.toml, Cargo.lock, .rs files, etc.)
                (craneLib.fileset.commonCargoSources unfilteredRoot)
                # Proto files for gRPC code generation
                (pkgs.lib.fileset.fileFilter (file: file.hasExt "proto") unfilteredRoot)
                # SQL migrations (embedded at compile time via sqlx::migrate!)
                ./migrations
              ];
            };
            strictDeps = true;

            pname = "rio";
            inherit version;

            nativeBuildInputs = with pkgs; [
              pkg-config
              protobuf
              cmake
            ];

            buildInputs =
              with pkgs;
              [
                openssl
                llvmPackages.libclang.lib
                fuse3
              ]
              ++ lib.optionals stdenv.isDarwin [
                darwin.apple_sdk.frameworks.Security
                libiconv
              ];

            propagatedBuildInputs = with pkgs; [
              nix
            ];

            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${rustStable}/lib/rustlib/src/rust/library";
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            # Where rio-test-support finds initdb/postgres (falls back to PATH).
            PG_BIN = "${pkgs.postgresql_18}/bin";
          };

          # Build dependencies only (for caching)
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # Build the workspace
          rio-workspace = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = false; # We'll run checks separately
            }
          );

          # --------------------------------------------------------------
          # Fuzz build pipeline
          # --------------------------------------------------------------
          #
          # The fuzz crate (rio-nix/fuzz) is its own workspace root —
          # excluded from the main workspace, has its own Cargo.lock, and
          # needs nightly for libfuzzer-sys. It depends on rio-nix by
          # path, so the source must include the full workspace Cargo.toml
          # tree. We vendor from the fuzz-specific lockfile.

          fuzzSrc = pkgs.lib.fileset.toSource {
            root = unfilteredRoot;
            fileset = pkgs.lib.fileset.unions [
              (craneLib.fileset.commonCargoSources unfilteredRoot)
              ./rio-nix/fuzz/Cargo.lock
            ];
          };

          fuzzArgs = {
            src = fuzzSrc;
            strictDeps = true;
            pname = "rio-fuzz";
            version = "0.0.0";

            cargoVendorDir = craneLibNightly.vendorCargoDeps {
              cargoLock = ./rio-nix/fuzz/Cargo.lock;
            };

            nativeBuildInputs = with pkgs; [
              pkg-config
              cargo-fuzz
            ];

            buildInputs = with pkgs; [
              openssl
              llvmPackages.libclang.lib
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;

          fuzzTargets = [
            "wire_primitives"
            "opcode_parsing"
            "derivation_parsing"
            "nar_parsing"
            "derived_path_parsing"
            "narinfo_parsing"
            "build_result_parsing"
          ];

          # Compile all fuzz target binaries with sancov instrumentation.
          # Expensive but cached by source hash; shared by both the 30s
          # PR-tier checks and the 600s nightly runs.
          #
          # No dep-layer (cargoArtifacts=null): cargo-fuzz's sancov flags
          # produce incompatible object files with a non-instrumented
          # buildDepsOnly layer, so dep caching would be a pure miss.
          rio-fuzz-build = craneLibNightly.mkCargoDerivation (
            fuzzArgs
            // {
              pname = "rio-fuzz-build";
              cargoArtifacts = null;

              buildPhaseCargoCommand = ''
                cd rio-nix/fuzz
                cargo fuzz build --release
              '';

              doInstallCargoArtifacts = false;
              installPhaseCommand = ''
                mkdir -p $out/bin
                for t in ${pkgs.lib.concatStringsSep " " fuzzTargets}; do
                  cp target/${rustTarget}/release/$t $out/bin/
                done
              '';
            }
          );

          # Per-target, per-time-budget fuzz run. Cheap runCommand
          # wrapper over the prebuilt binary. The 30s and 600s variants
          # share the same rio-fuzz-build.
          mkFuzzCheck =
            { target, maxTime }:
            let
              seedCorpus = ./rio-nix/fuzz/corpus + "/${target}";
              hasCorpus = builtins.pathExists seedCorpus;
            in
            pkgs.runCommand "rio-fuzz-${target}-${toString maxTime}s" { } ''
              workCorpus=$(mktemp -d)
              ${pkgs.lib.optionalString hasCorpus ''
                cp -r ${seedCorpus}/. "$workCorpus"/
                chmod -R u+w "$workCorpus"
              ''}
              mkdir -p artifacts

              # -fork=N spawns N libFuzzer workers that share corpus.
              # Workers write to fuzz-*.log; dump those on failure so
              # crash stacks land in the Nix build log.
              ${rio-fuzz-build}/bin/${target} "$workCorpus" \
                -max_total_time=${toString maxTime} \
                -timeout=30 \
                -print_final_stats=1 \
                -artifact_prefix=artifacts/ \
                -fork=''${NIX_BUILD_CORES:-1} || {
                  echo "--- worker logs ---"
                  cat fuzz-*.log 2>/dev/null || true
                  exit 1
                }

              echo "${target}: ${toString maxTime}s, no crashes" > $out
            '';
        in
        {
          # Import rust-overlay
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          # Configure treefmt
          treefmt.config = {
            flakeCheck = false;
            projectRootFile = "flake.nix";

            programs = {
              nixfmt.enable = true;

              # Rust formatting (stable rustfmt — nightly rustfmt can
              # produce different output, and we want CI/dev parity here)
              rustfmt = {
                enable = true;
                package = rustStable;
              };

              # TOML formatting
              taplo.enable = true;
            };
          };

          # Configure git hooks
          pre-commit = {
            check.enable = true;

            settings.excludes = [
              "docs/mermaid\\.min\\.js$"
              # Fuzz corpus seeds are exact binary/text inputs; trailing
              # newlines would change what the fuzzer sees.
              "^rio-nix/fuzz/corpus/"
            ];

            settings.hooks = {
              treefmt.enable = true;
              convco.enable = true;
              ripsecrets.enable = true;
              check-added-large-files.enable = true;
              check-merge-conflicts.enable = true;
              end-of-file-fixer.enable = true;
              trim-trailing-whitespace.enable = true;
              deadnix.enable = true;
              nil.enable = true;
              statix.enable = true;
            };
          };

          # --------------------------------------------------------------
          # Dev shells
          # --------------------------------------------------------------
          #
          # Default = nightly so `cargo fuzz run` works out of the box.
          # CI builds still use stable (see craneLib above), so if you
          # write nightly-only code, checks.rio-clippy / rio-nextest will
          # catch it.
          #
          # Use `nix develop .#stable` for strict CI-parity dev.
          devShells =
            let
              shellPackages = with pkgs; [
                # Cargo tools
                cargo-edit
                cargo-expand
                cargo-fuzz # works in default (nightly) shell; errors on stable
                cargo-nextest
                cargo-outdated
                cargo-watch

                # Debugging tools
                lldb
                gdb

                # Documentation
                mdbook
                mdbook-mermaid

                # Integration test deps
                postgresql_18

                # Formatting (nix fmt also works, but direct treefmt is handy)
                config.treefmt.build.wrapper
              ];
              shellEnv = {
                RUST_BACKTRACE = "1";
                PROTOC = "${pkgs.protobuf}/bin/protoc";
                LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
                PG_BIN = "${pkgs.postgresql_18}/bin";
                shellHook = config.pre-commit.installationScript;
              };
            in
            {
              default = craneLibNightly.devShell (
                shellEnv
                // {
                  inherit (config) checks;
                  packages = shellPackages;
                  RUST_SRC_PATH = "${rustNightly}/lib/rustlib/src/rust/library";
                }
              );

              stable = craneLib.devShell (
                shellEnv
                // {
                  inherit (config) checks;
                  packages = shellPackages;
                  RUST_SRC_PATH = "${rustStable}/lib/rustlib/src/rust/library";
                }
              );
            };

          # --------------------------------------------------------------
          # Packages
          # --------------------------------------------------------------
          packages = {
            default = rio-workspace;
            inherit rio-fuzz-build; # debug: nix build .#rio-fuzz-build
          }
          # Nightly-tier fuzz runs: 10 min/target. NOT in `checks` —
          # explicitly invoked by the nightly pipeline, e.g.
          #   nix build .#fuzz-wire_primitives
          #
          # TODO(phase3b): corpus persistence (S3 upload/download) so
          # runs accumulate findings instead of discarding the work
          # corpus at the end of each derivation.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            builtins.listToAttrs (
              map (t: {
                name = "fuzz-${t}";
                value = mkFuzzCheck {
                  target = t;
                  maxTime = 600;
                };
              }) fuzzTargets
            )
          );

          # --------------------------------------------------------------
          # Checks (run with 'nix flake check')
          # --------------------------------------------------------------
          checks = {
            # Build the workspace
            inherit rio-workspace;

            # Clippy lints
            rio-clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

            # Run tests with nextest
            rio-nextest = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoNextestExtraArgs = "--no-tests=warn";
                nativeCheckInputs = with pkgs; [
                  inputs.nix.packages.${system}.default
                  openssh
                  postgresql_18
                ];
              }
            );

            # Documentation check
            rio-doc = craneLib.cargoDoc (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );

            # Test coverage with llvm-cov
            rio-coverage = craneLib.cargoLlvmCov (
              commonArgs
              // {
                inherit cargoArtifacts;
                dontFixup = true;
                # cargoLlvmCov runs tests during the build phase (not check phase),
                # so nativeCheckInputs won't be available. Use nativeBuildInputs.
                nativeBuildInputs =
                  (commonArgs.nativeBuildInputs or [ ])
                  ++ (with pkgs; [
                    inputs.nix.packages.${system}.default
                    openssh
                    postgresql_18
                  ]);
              }
            );
          }
          # PR-tier smoke fuzz: 30s/target with seed corpus.
          # Linux-only (libFuzzer). Compiled binaries shared with the
          # nightly-tier packages via rio-fuzz-build.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            builtins.listToAttrs (
              map (t: {
                name = "rio-fuzz-${t}";
                value = mkFuzzCheck {
                  target = t;
                  maxTime = 30;
                };
              }) fuzzTargets
            )
          )
          # Per-phase milestone VM tests (Linux-only: need KVM + NixOS VMs).
          # Each validates the corresponding phase milestone in docs/src/phases/.
          #
          #   rio-phase1a — 2 VMs: read-only opcodes (path-info, store ls)
          #   rio-phase1b — 3 VMs: single-worker end-to-end build
          #   rio-phase2a — 4 VMs: distributed build across 2+ workers
          #
          # Run:   nix build .#checks.x86_64-linux.rio-phase2a
          # Debug: nix build .#checks.x86_64-linux.rio-phase2a.driverInteractive
          #        && ./result/bin/nixos-test-driver
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            let
              vmTestArgs = {
                inherit pkgs rio-workspace;
                rioModules = inputs.self.nixosModules;
              };
            in
            {
              rio-phase1a = import ./nix/tests/phase1a.nix vmTestArgs;
              rio-phase1b = import ./nix/tests/phase1b.nix vmTestArgs;
              rio-phase2a = import ./nix/tests/phase2a.nix vmTestArgs;
            }
          );

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}
