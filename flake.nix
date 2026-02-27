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

          # Rust toolchain from rust-toolchain.toml (single source of truth)
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # Crane library for building Rust packages
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

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
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
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

              # Rust formatting
              rustfmt = {
                enable = true;
                package = rustToolchain;
              };

              # TOML formatting
              taplo.enable = true;
            };
          };

          # Configure git hooks
          pre-commit = {
            check.enable = true;

            settings.excludes = [ "docs/mermaid\\.min\\.js$" ];

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

          # Development shell
          devShells.default = craneLib.devShell {
            inherit (config) checks;

            packages = with pkgs; [
              # Cargo tools
              cargo-edit
              cargo-expand
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
            ];

            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            PG_BIN = "${pkgs.postgresql_18}/bin";

            shellHook = config.pre-commit.installationScript;
          };

          # Packages
          packages.default = rio-workspace;

          # Checks (run with 'nix flake check')
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
