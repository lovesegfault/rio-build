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

  outputs = inputs @ {
    flake-parts,
    nixpkgs,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
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

      perSystem = {
        config,
        self',
        inputs',
        pkgs,
        system,
        ...
      }: let
        # Rust toolchain configuration
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
          ];
          targets = [pkgs.hostPlatform.rust.rustcTarget];
        };

        # Crane library for building Rust packages
        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Source filter to include proto files
        protoFilter = path: _type: builtins.match ".*proto$" path != null;
        protoOrCargo = path: type:
          (protoFilter path type) || (craneLib.filterCargoSources path type);

        # Common arguments for all crane builds
        commonArgs = {
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = protoOrCargo;
          };
          strictDeps = true;

          pname = "rio";
          version = "0.1.0";

          nativeBuildInputs = with pkgs; [
            pkg-config
            protobuf
          ];

          buildInputs = with pkgs; [
            openssl
          ];
        };

        # Build dependencies only (for caching)
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the workspace
        rio-workspace = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false; # We'll run checks separately
          });

        # Build inputs for Rust projects (for devShell)
        buildInputs = with pkgs; [
          openssl
          pkg-config
        ];

        # Native build inputs (for devShell)
        nativeBuildInputs = with pkgs; [
          pkg-config
        ];
      in {
        # Import rust-overlay
        _module.args.pkgs = import nixpkgs {
          inherit system;
          overlays = [inputs.rust-overlay.overlays.default];
        };

        # Configure treefmt
        treefmt.config = {
          projectRootFile = "flake.nix";

          programs = {
            # Nix formatting
            alejandra.enable = true;

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
          check.enable = false; # Disable check until we have a Rust project

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
        devShells.default = pkgs.mkShell {
          inherit buildInputs nativeBuildInputs;

          packages = with pkgs; [
            # Rust toolchain
            rustToolchain

            # Cargo tools
            cargo-edit
            cargo-watch
            cargo-expand
            cargo-outdated

            # Build dependencies
            protobuf
            pkg-config
            openssl

            # Development tools
            git
            just

            # Debugging tools
            lldb
            gdb
          ];

          shellHook = ''
            ${config.pre-commit.installationScript}

            echo "🦀 Rust development environment loaded!"
            echo "Rust version: $(rustc --version)"
            echo ""
            echo "Available commands:"
            echo "  cargo build      - Build the project"
            echo "  cargo test       - Run tests"
            echo "  cargo run        - Run the project"
            echo "  cargo clippy     - Run linter"
            echo "  treefmt          - Format all files"
            echo ""
          '';

          # Environment variables
          RUST_BACKTRACE = "1";
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };

        # Packages
        packages = {
          default = rio-workspace;
          rio-workspace = rio-workspace;

          # Individual binaries (using workspace build)
          rio-dispatcher = pkgs.runCommand "rio-dispatcher" {} ''
            mkdir -p $out/bin
            cp ${rio-workspace}/bin/rio-dispatcher $out/bin/
          '';

          rio-builder = pkgs.runCommand "rio-builder" {} ''
            mkdir -p $out/bin
            cp ${rio-workspace}/bin/rio-builder $out/bin/
          '';
        };

        # Checks (run with 'nix flake check')
        checks = {
          # Clippy lints
          rio-clippy = craneLib.cargoClippy (commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            });

          # Run tests
          rio-test = craneLib.cargoTest (commonArgs
            // {
              inherit cargoArtifacts;
            });

          # Documentation check
          rio-doc = craneLib.cargoDoc (commonArgs
            // {
              inherit cargoArtifacts;
            });

          # Test coverage with tarpaulin
          rio-coverage = craneLib.cargoTarpaulin (commonArgs
            // {
              inherit cargoArtifacts;
            });
        };

        # Formatter for 'nix fmt'
        formatter = config.treefmt.build.wrapper;

        # Apps
        apps = {
          format = {
            type = "app";
            program = "${config.treefmt.build.wrapper}/bin/treefmt";
          };

          dispatcher = {
            type = "app";
            program = "${self'.packages.rio-dispatcher}/bin/rio-dispatcher";
          };

          builder = {
            type = "app";
            program = "${self'.packages.rio-builder}/bin/rio-builder";
          };
        };
      };
    };
}
