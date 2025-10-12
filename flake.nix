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
            ];

            buildInputs =
              with pkgs;
              [
                openssl
              ]
              ++ lib.optionals stdenv.isDarwin [
                darwin.apple_sdk.frameworks.Security
                libiconv
              ];
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
          devShells.default = craneLib.devShell {
            # Inherit inputs from the package build
            inputsFrom = [ rio-workspace ];

            # Inherit inputs from checks
            checks = config.checks;

            # Additional development packages
            packages = with pkgs; [
              # Cargo tools
              cargo-edit
              cargo-watch
              cargo-expand
              cargo-outdated

              # Debugging tools
              lldb
              gdb
            ];

            # Shell hook for pre-commit
            shellHook = config.pre-commit.installationScript;

            # Environment variables
            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            PROTOC = "${pkgs.protobuf}/bin/protoc";
          };

          # Packages
          packages = {
            default = rio-workspace;

            # Individual packages built separately for better modularity
            rio-dispatcher = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = "rio-dispatcher";
                cargoExtraArgs = "-p rio-dispatcher";
                doCheck = false;
              }
            );

            rio-builder = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = "rio-builder";
                cargoExtraArgs = "-p rio-builder";
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
              }
            );
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;

          # Apps
          apps = {
            dispatcher = {
              type = "app";
              program = "${self'.packages.rio-dispatcher}/bin/rio-dispatcher";
              meta.description = "Rio distributed build dispatcher - manages build fleet and SSH frontend";
            };

            builder = {
              type = "app";
              program = "${self'.packages.rio-builder}/bin/rio-builder";
              meta.description = "Rio distributed build worker - executes Nix builds on worker nodes";
            };
          };
        };
    };
}
