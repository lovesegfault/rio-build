{
  description = "rio-build - Nix build orchestration";

  inputs = {
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
            ];

            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            shellHook = config.pre-commit.installationScript;
          };

          # Packages
          packages = {
            default = rio-workspace;

            # OCI image for the FUSE+overlay+sandbox spike
            # Build: nix build .#spike-image
            # Load: docker load < result
            spike-image = pkgs.dockerTools.buildLayeredImage {
              name = "rio-spike";
              tag = "latest";
              maxLayers = 50;

              contents = with pkgs; [
                # The rio-spike binary
                rio-workspace

                # Nix tooling (nix-store, nix-build, nix path-info)
                nix

                # Runtime essentials
                coreutils
                bashInteractive
                fuse3

                # Needed by overlay/sandbox validation
                util-linux # mount, umount
                gnugrep
                findutils

                # CA certificates for potential network access
                cacert
              ];

              config = {
                Cmd = [
                  "rio-spike"
                  "validate"
                  "--all"
                ];
                Env = [
                  "RUST_LOG=info"
                  "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
                ];
              };
            };
          };

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
                  nix
                  openssh
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
                    nix
                    openssh
                  ]);
              }
            );
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}
