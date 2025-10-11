{
  description = "Rio Build - Rust development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

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

        # Build inputs for Rust projects
        buildInputs = with pkgs; [
          openssl
          pkg-config
        ];

        # Native build inputs
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

              # Rust-specific hooks (only run if Cargo.toml exists)
              cargo-check = {
                enable = true;
                description = "Run cargo check";
                entry = "${pkgs.writeShellScript "cargo-check-hook" ''
                  if [ -f "Cargo.toml" ]; then
                    ${rustToolchain}/bin/cargo check
                  fi
                ''}";
                files = "\\.rs$";
                pass_filenames = false;
              };

              clippy = {
                enable = true;
                description = "Run cargo clippy";
                entry = "${pkgs.writeShellScript "clippy-hook" ''
                  if [ -f "Cargo.toml" ]; then
                    ${rustToolchain}/bin/cargo clippy -- -D warnings
                  fi
                ''}";
                files = "\\.rs$";
                pass_filenames = false;
              };
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

        # Formatter for 'nix fmt'
        formatter = config.treefmt.build.wrapper;

        # Apps
        apps = {
          format = {
            type = "app";
            program = "${config.treefmt.build.wrapper}/bin/treefmt";
          };
        };
      };
    };
}
