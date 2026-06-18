# Dev shells.
#
# Default = nightly so `cargo fuzz run` works out of the box. CI builds
# use stable (rustStable via crate2nix), so if you write nightly-only
# code, checks.clippy-* / checks.nextest-* will catch it.
#
# Use `nix develop .#stable` for strict CI-parity dev.
{
  pkgs,
  rustStable,
  rustNightly,
  sysCrateEnv,
  traceyPkg,
  crate2nixCli,
  # nix/docs.nix — rioTypst (wrapped typst) + typstEnv (TYPST_* vars)
  docsLib,
  # config.treefmt.build.wrapper — `treefmt` in PATH
  treefmtWrapper,
  # config.pre-commit.installationScript — installs git hooks on shell entry
  preCommitInstall,
  # nix/kani-toolchain.nix — cargo-kani for local proof iteration
  kaniToolchain,
  # nix/quint-mcp.nix — hermetic quint-llm-kit MCP servers (KB search +
  # LSP bridge), invoked by the project-scoped .mcp.json.
  quintMcp,
}:
let
  # nixpkgs still ships sqlx-cli 0.8.6 while the workspace is on sqlx
  # 0.9. `cargo sqlx prepare` must match the library major (it drives
  # the .sqlx/ query cache that sqlx-macros 0.9 consumes, and 0.9
  # changed nullability inference), so pin 0.9.0 here. Feature set
  # mirrors the nixpkgs expression. Drop this override — and the
  # reference below — once nixpkgs ships sqlx-cli ≥ 0.9.
  sqlxCli =
    let
      version = "0.9.0";
      src = pkgs.fetchCrate {
        pname = "sqlx-cli";
        inherit version;
        hash = "sha256-XariusjsCgn0Qai0XWtr7EzSzDDTp1cCzjff1kJNO9Y=";
      };
    in
    pkgs.sqlx-cli.overrideAttrs (_old: {
      inherit version src;
      cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
        inherit src;
        name = "sqlx-cli-${version}-vendor";
        hash = "sha256-pHaMKuB9v3fjbgeVyLyRtfoQ9BkE6z+TjDfdBaVdbXM=";
      };
    });

  shellPackages = with pkgs; [
    # CI gate driver — `nix-fast-build --flake .#checks.x86_64-linux`
    # streams eval+build (per-attr nix-eval-jobs workers → builds
    # queue as drvs become known). Replaces the old `.#ci` aggregate.
    # TODO: drop override once nixpkgs has 1.6.0 (needed for --fail-fast).
    (nix-fast-build.overrideAttrs (_: rec {
      version = "1.6.0";
      src = fetchFromGitHub {
        owner = "Mic92";
        repo = "nix-fast-build";
        tag = version;
        hash = "sha256-PMBbenLBvn/0pSFOhwPVn171Vw7kU5YmBUNDhxllZ7c=";
      };
    }))

    # Cargo tools
    cargo-edit
    cargo-expand
    # cargo-fuzz, wrapped — works in default (nightly) shell; errors on
    # stable. Stock discovery walks up from $PWD to the first manifest
    # WITHOUT `package.metadata.cargo-fuzz = true` and expects
    # `fuzz/Cargo.toml` under THAT dir. The per-crate fuzz workspaces
    # (fuzz/<crate>/) have no parent package between them and the
    # virtual workspace root, so the walk lands on the repo root and
    # dies on the nonexistent <root>/fuzz/Cargo.toml. When the nearest
    # ancestor manifest IS a cargo-fuzz manifest, anchor the invocation
    # there via --fuzz-dir so the documented `cd fuzz/<crate> && cargo
    # fuzz run <target>` flow works. Everything else (explicit
    # --fuzz-dir, standard crate/fuzz layouts, init/help) passes
    # through untouched.
    (writeShellScriptBin "cargo-fuzz" ''
      set -euo pipefail
      real=${pkgs.cargo-fuzz}/bin/cargo-fuzz
      for arg in "$@"; do
        case "$arg" in
          --fuzz-dir | --fuzz-dir=*) exec "$real" "$@" ;;
        esac
      done
      dir=$PWD
      fuzz_dir=
      while :; do
        if [ -f "$dir/Cargo.toml" ]; then
          if grep -Eqs '^[[:space:]]*cargo-fuzz[[:space:]]*=[[:space:]]*true' "$dir/Cargo.toml"; then
            fuzz_dir=$dir
          fi
          break
        fi
        [ "$dir" = / ] && break
        dir=$(dirname "$dir")
      done
      if [ -n "$fuzz_dir" ] && [ "''${1-}" = fuzz ] && [ "$#" -ge 2 ]; then
        sub=$2
        shift 2
        case "$sub" in
          add | build | check | cmin | coverage | fmt | list | run | tmin)
            exec "$real" fuzz "$sub" --fuzz-dir "$fuzz_dir" "$@"
            ;;
          *)
            exec "$real" fuzz "$sub" "$@"
            ;;
        esac
      fi
      exec "$real" "$@"
    '')
    cargo-hakari # workspace-hack regen — `cargo xtask regen hakari`
    cargo-mutants # dev-only mutation testing — see `cargo xtask mutants` / `.#mutants`
    cargo-nextest
    cargo-outdated
    cargo-watch

    # Debugging tools
    lldb
    gdb
    lcov # `lcov --summary`/`--list` on the coverage output
    stress-ng # flake-repro under load (.claude/rules/ci-failure-patterns.md)

    # Documentation
    # Typst design book — wrapped typst (with @preview/* closure
    # baked in via TYPST_PACKAGE_CACHE_PATH), the typstyle formatter
    # (wired into treefmt), and pagefind for indexing the native HTML
    # bundle locally. `nix build .#docs` is the canonical output;
    # `nix run .#docs` serves it.
    docsLib.rioTypst
    typstyle
    pagefind

    # Integration test deps
    postgresql_18
    sqlxCli # `cargo xtask regen sqlx` + `cargo sqlx migrate` (0.9 pin, see above)

    # Local dev stack (`process-compose up`)
    process-compose

    # Formatting (nix fmt also works, but direct treefmt is handy)
    treefmtWrapper

    # Spec-coverage: `tracey query validate`, `tracey web`
    traceyPkg

    # Formal verification: `cargo kani -p rio-lease` for local proof
    # iteration. Wrapper unsets CARGO_BUILD_BUILD_DIR (see
    # nix/kani-toolchain.nix postFixup) so kani-driver can find its
    # goto-C artifacts despite the shared build cache below. NOTE:
    # ~3.5GB closure (pinned rust nightly + rustc-dev + cbmc). If
    # devshell entry becomes slow, gate behind an opt-in `.#kani` shell.
    kaniToolchain.kani

    # Quint — the formal-specification language for docs/spec/models/
    # (typed, effect-checked, simulator + Apalache symbolic verifier +
    # the exhaustive TLC backend the CI checks use). Run a model
    # locally: `quint verify --backend=tlc --main=<module>
    # --invariant=<i> docs/spec/models/M.qnt`. `quint verify` finds
    # Apalache (and the TLC inside it) via QUINT_HOME inside the
    # package — a store path, no runtime download, which is why the
    # checks in nix/quint.nix work in the network-less sandbox.
    pkgs.quint

    # MCP servers launched by the project-scoped .mcp.json (tracey above
    # is the third): curated Quint knowledge-base search and LSP-grade
    # .qnt diagnostics. Both run offline from the store — see
    # nix/quint-mcp.nix.
    quintMcp.quint-kb-mcp
    quintMcp.quint-lsp-mcp

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
      p.hashicorp_http # addons.tf: fetch Gateway API CRD yaml
      p.gavinbunney_kubectl # addons.tf: apply Gateway API CRDs (kubernetes_manifest can't — plan-time API validation)
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
  ];

  # Shared mkShell builder. Lists build deps explicitly
  # (openssl, libclang, sys-crate libs for pkg-config
  # probes, protobuf+cmake for rio-proto's codegen).
  mkRioShell =
    rust:
    (pkgs.mkShell.override {
      # mold via cc-wrapper: rustc's linker is `cc`, so this
      # speeds dev-loop relinks without touching RUSTFLAGS
      # (target-dir fingerprints stay valid). crate2nix
      # uses its own stdenv — `nix build` stays on GNU ld.
      stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;
    })
      (
        sysCrateEnv.allEnv
        // docsLib.typstEnv
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
            ${preCommitInstall}
          '';
        }
      );
in
{
  default = mkRioShell rustNightly;
  stable = mkRioShell rustStable;
}
