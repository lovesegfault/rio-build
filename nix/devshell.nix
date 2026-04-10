# Dev shells.
#
# Default = nightly so `cargo fuzz run` works out of the box. CI builds
# use stable (rustStable via crate2nix), so if you write nightly-only
# code, checks.clippy / checks.nextest will catch it.
#
# Use `nix develop .#stable` for strict CI-parity dev.
{
  pkgs,
  rustStable,
  rustNightly,
  sysCrateEnv,
  traceyPkg,
  crate2nixCli,
  # config.treefmt.build.wrapper — `treefmt` in PATH
  treefmtWrapper,
  # config.pre-commit.installationScript — installs git hooks on shell entry
  preCommitInstall,
}:
let
  shellPackages = with pkgs; [
    # Cargo tools
    cargo-edit
    cargo-expand
    cargo-fuzz # works in default (nightly) shell; errors on stable
    cargo-hakari # workspace-hack regen — `cargo xtask regen hakari`
    cargo-mutants # weekly tier — see `cargo xtask mutants` / `.#mutants`
    cargo-nextest
    cargo-outdated
    cargo-watch

    # Debugging tools
    lldb
    gdb
    lcov # `lcov --summary`/`--list` on the coverage output
    stress-ng # flake-repro under load (.claude/rules/ci-failure-patterns.md)

    # Documentation
    mdbook
    mdbook-mermaid

    # Integration test deps
    postgresql_18
    sqlx-cli # `cargo xtask regen sqlx` + `cargo sqlx migrate`

    # Local dev stack (`process-compose up`)
    process-compose

    # Formatting (nix fmt also works, but direct treefmt is handy)
    treefmtWrapper

    # Spec-coverage: `tracey query validate`, `tracey web`
    traceyPkg

    # crate2nix CLI for regenerating Cargo.json after
    # Cargo.lock changes. PoC — see
    # .claude/notes/crate2nix-migration-assessment.md.
    crate2nixCli

    # Dashboard dev: `pnpm install --lockfile-only` (hash bumps),
    # `pnpm run dev` (vite dev server with Envoy proxy). Proto
    # stubs regen: `cd rio-dashboard && buf generate --template
    # buf.gen.yaml ../rio-proto/proto` (src/gen/ is gitignored).
    nodejs
    pnpm_10
    buf
    protoc-gen-es

    # Deploy tooling for infra/eks/. Large closures (awscli2
    # pulls python3 + botocore) but the user asked for
    # everything-in-one-shell over a separate .#deploy.
    # Scripts under infra/eks/ also carry nix-shell shebangs
    # pointing at these same packages, so they work even if
    # someone runs them outside `nix develop`.
    awscli2
    coldsnap # cargo xtask k8s -p eks ami push — direct-to-EBS-snapshot upload (ADR-021)
    ssm-session-manager-plugin # cargo xtask k8s -p eks smoke — SSM tunnel to NLB
    lsof # cargo xtask k8s rsb — reap stale tunnel listeners on :2222
    # opentofu (not terraform: BSL license → unfree in nixpkgs)
    # with providers bundled via withPlugins. No `tofu init`
    # download step — providers are in the nix store, pinned by
    # nixpkgs rev. .terraform.lock.hcl is gitignored (nix is the
    # lock). The provider set must cover transitive module deps
    # too (EKS module pulls cloudinit + null).
    (opentofu.withPlugins (p: [
      p.hashicorp_aws
      p.hashicorp_helm
      p.hashicorp_kubernetes
      p.hashicorp_random
      p.hashicorp_tls
      p.hashicorp_time
      p.hashicorp_cloudinit # transitive: terraform-aws-modules/eks
      p.hashicorp_null # transitive: terraform-aws-modules/eks
    ]))
    kubectl
    skopeo # cargo xtask k8s push -p eks — docker-archive → ECR
    manifest-tool # cargo xtask k8s push -p eks — multi-arch OCI index
    kubernetes-helm
    cilium-cli # cilium status, cilium hubble ui (port-forward)
    hubble # hubble observe --server localhost:4245 (after port-forward to hubble-relay)
    kubeconform # ad-hoc schema validation (no pre-commit hook — fetches 300MB, sandbox blocks)
    yq-go # nix/helm-render.nix
    grpcurl # manual AdminService poking when rio-cli isn't enough
    openssl # openssl rand 32 → HMAC key
    git

    # cargo xtask regen crds → scripts/split-crds.py
    (python3.withPackages (ps: [ ps.pyyaml ]))
  ];

  # Shared mkShell builder. Lists build deps explicitly
  # (openssl, libclang, sys-crate libs for pkg-config
  # probes, protobuf+cmake for rio-proto's codegen).
  mkRioShell =
    rust:
    (pkgs.mkShell.override {
      # mold via cc-wrapper: rustc's linker is `cc`, so this
      # speeds dev-loop relinks without touching RUSTFLAGS
      # (shared build-dir fingerprints stay valid). crate2nix
      # uses its own stdenv — `nix build` stays on GNU ld.
      stdenv = if pkgs.stdenv.isLinux then pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv else pkgs.stdenv;
    })
      (
        sysCrateEnv.allEnv
        // {
          packages = [ rust ] ++ shellPackages;
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
            ++ sysCrateEnv.allLibs;
          RUST_BACKTRACE = "1";
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          PG_BIN = "${pkgs.postgresql_18}/bin";
          # sqlx query! macros read .sqlx/ instead of connecting
          # to PG. `cargo build` works without a live DB.
          # `cargo xtask regen sqlx` unsets this locally to regenerate.
          SQLX_OFFLINE = "true";
          RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
          # Repo-local kubeconfig: xtask k8s writes here, so
          # direct kubectl/helm in the shell hits the same
          # cluster. Matches xtask/src/sh.rs:kubeconfig_path().
          shellHook = ''
            export KUBECONFIG="$PWD/.kube/config"
            # Shared intermediate build cache across all worktrees
            # (~/src/rio-build/*). Per-worktree target/ keeps only
            # final artifacts. Fine-grain locking (nightly; ignored
            # on stable) lets concurrent `cargo check` run lock-free.
            export CARGO_BUILD_BUILD_DIR="''${CARGO_BUILD_BUILD_DIR:-$HOME/.cache/rio-build/build}"
            export CARGO_UNSTABLE_FINE_GRAIN_LOCKING=true
            ${preCommitInstall}
          '';
        }
      );
in
{
  default = mkRioShell rustNightly;
  stable = mkRioShell rustStable;
}
