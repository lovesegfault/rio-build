{
  description = "Rio Build - Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";

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

    # Fork with PR #387 applied (--show-required-system-features support)
    # TODO: Switch to upstream once PR is merged
    nix-eval-jobs = {
      url = "github:lovesegfault/nix-eval-jobs/for-rio";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.treefmt-nix.follows = "treefmt-nix";
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

      perSystem =
        {
          config,
          self',
          inputs',
          pkgs,
          system,
          ...
        }:
        let
          # Rust toolchain configuration
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
            targets = [ pkgs.hostPlatform.rust.rustcTarget ];
          };

          # Crane library for building Rust packages
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

          # Source root for filesets
          unfilteredRoot = ./.;

          # Common environment variables for builds and dev shell
          commonEnvVars = {
            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          # Common arguments for all crane builds
          commonArgs = {
            src = pkgs.lib.fileset.toSource {
              root = unfilteredRoot;
              fileset = pkgs.lib.fileset.unions [
                # All standard Cargo sources (Cargo.toml, Cargo.lock, .rs files, etc.)
                (craneLib.fileset.commonCargoSources unfilteredRoot)
                # Proto files for gRPC code generation
                (pkgs.lib.fileset.fileFilter (file: file.hasExt "proto") unfilteredRoot)
              ];
            };
            strictDeps = true;

            pname = "rio";
            version = "0.1.0";

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
              ]
              ++ lib.optionals stdenv.isDarwin [
                darwin.apple_sdk.frameworks.Security
                libiconv
              ];

            # Runtime dependencies (available when running rio-build and rio-agent)
            propagatedBuildInputs = with pkgs; [
              nix # Required by both rio-build and rio-agent for nix-store, nix-build, etc.
              inputs'.nix-eval-jobs.packages.default # Required by rio-build for evaluation
            ];
          }
          // commonEnvVars;

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

            settings = {
              hooks = {
                # Run treefmt on all files
                treefmt.enable = true;

                # Note: cargo-check and clippy hooks disabled in favor of crane checks
                # which run via 'nix flake check' and properly handle protoc dependency
              };
            };
          };

          # Development shell
          devShells.default = craneLib.devShell (
            commonEnvVars
            // {
              # Inherit inputs from the package build
              inputsFrom = [ rio-workspace ];

              # Inherit inputs from checks
              checks = config.checks;

              # Additional development packages
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

                # Nix tooling (fork with system-features support)
                inputs'.nix-eval-jobs.packages.default
              ];

              # Shell hook for pre-commit
              shellHook = config.pre-commit.installationScript;
            }
          );

          # Packages
          packages = {
            default = rio-workspace;

            # Individual packages built separately for better modularity
            rio-build = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = "rio-build";
                cargoExtraArgs = "-p rio-build";
                doCheck = false;
              }
            );

            rio-agent = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = "rio-agent";
                cargoExtraArgs = "-p rio-agent";
                doCheck = false;
              }
            );
          };

          # Checks (run with 'nix flake check')
          checks = {
            # Clippy lints
            rio-clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

            # Run tests
            rio-test = craneLib.cargoTest (
              commonArgs
              // {
                inherit cargoArtifacts;
                # Integration tests need nix commands available
                nativeCheckInputs = with pkgs; [
                  nix
                  inputs'.nix-eval-jobs.packages.default
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

            # Test coverage with tarpaulin
            rio-coverage = craneLib.cargoTarpaulin (
              commonArgs
              // {
                inherit cargoArtifacts;
                # Coverage tests also need nix commands available
                nativeCheckInputs = with pkgs; [
                  nix
                  inputs'.nix-eval-jobs.packages.default
                ];
              }
            );
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;

          # Apps
          apps = {
            build = {
              type = "app";
              program = "${self'.packages.rio-build}/bin/rio-build";
              meta.description = "Rio build CLI - submit builds to distributed agent cluster";
            };

            agent = {
              type = "app";
              program = "${self'.packages.rio-agent}/bin/rio-agent";
              meta.description = "Rio agent - cluster node that executes builds with Raft coordination";
            };
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            # Interactive VM test runner (Linux only)
            vm-test-interactive = {
              type = "app";
              program = toString (
                pkgs.writeShellScript "vm-test-interactive" ''
                  ${config.checks.vm-e2e.driverInteractive}/bin/nixos-test-driver
                ''
              );
            };
          };
        };

      # Flake-level outputs (not perSystem)
      flake = {
        # Export NixOS modules for use in other flakes
        nixosModules = {
          rio-dispatcher = import ./modules/dispatcher.nix;
          rio-builder = import ./modules/builder.nix;

          # Combined module that imports both
          default =
            { ... }:
            {
              imports = [
                (import ./modules/dispatcher.nix)
                (import ./modules/builder.nix)
              ];
            };
        };
      };
    };
}
