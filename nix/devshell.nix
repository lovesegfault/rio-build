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
  shiroaPkg,
  # config.treefmt.build.wrapper — `treefmt` in PATH
  treefmtWrapper,
  # config.pre-commit.installationScript — installs git hooks on shell entry
  preCommitInstall,
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
    nix-fast-build

    # Parallel evaluator used by `rio-replay record` (build-replay
    # campaign recorder) for local/scoped archive recording; the same
    # pinned binary nix-fast-build and gen-matrix already use.
    nix-eval-jobs

    # DwarFS image tools (mkdwarfs): pack replay-archive staging
    # directories into the .dwarfs images the campaign engine consumes,
    # and (re)generate the committed archive test fixtures.
    dwarfs

    # Cargo tools
    cargo-edit
    cargo-expand
    cargo-fuzz # works in default (nightly) shell; errors on stable
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
    # baked in via TYPST_PACKAGE_CACHE_PATH), shiroa HTML generator,
    # and the typstyle formatter (wired into treefmt). typstEnv below
    # exports the same TYPST_* vars for shiroa's in-process resolver.
    docsLib.rioTypst
    # shiroa's embedded reflexo-typst ignores TYPST_PACKAGE_CACHE_PATH —
    # it resolves @preview/* via $XDG_DATA_HOME/typst/packages and WRITES
    # its bundled @preview/shiroa there on startup, so it needs a writable
    # copy of the rioTypst package closure. Wrap to sync the closure into
    # docs/.cache/typst-xdg/ (gitignored) on first run and whenever
    # rioTypst changes, then exec the real shiroa with XDG_DATA_HOME
    # pointed there. The sentinel file holds the source store path so a
    # `direnv reload` after a typstDeps change re-syncs.
    (writeShellScriptBin "shiroa" ''
      set -euo pipefail
      src="${docsLib.rioTypst}/lib/typst/packages"
      cache="''${RIO_TYPST_XDG:-$PWD/docs/.cache/typst-xdg}"
      sentinel="$cache/.rio-typst-src"
      if [[ ! -f "$sentinel" || "$(cat "$sentinel")" != "$src" ]]; then
        echo "shiroa: syncing typst package closure → $cache" >&2
        rm -rf "$cache"
        mkdir -p "$cache/typst"
        cp -rL "$src" "$cache/typst/packages"
        chmod -R u+w "$cache/typst"
        echo "$src" > "$sentinel"
      fi
      export XDG_DATA_HOME="$cache"
      # No stderr filtering — piping through grep would strip ANSI colour
      # (shiroa's logger checks isatty). The per-chapter "html export is
      # under active development" banner is tolerable noise in exchange.
      #
      # NOTE: `shiroa serve` output is NOT post-processed. The nix build
      # (`.#docs`) runs nix/docs-svg-dedup.py (glyph-sprite hoist,
      # dyn-render JS strip) and derives 404.html from intro.html.
      # Under `serve` you'll see: sla-sizing.html
      # ~10.5MB (vs ~4.4MB built); 404.html missing; refs.gh() links
      # carry `gh-sha=dirty`. All cosmetic — `nix build .#docs` is the
      # canonical output.
      exec ${shiroaPkg}/bin/shiroa "$@"
    '')
    typstyle

    # Integration test deps
    postgresql_18
    sqlxCli # `cargo xtask regen sqlx` + `cargo sqlx migrate` (0.9 pin, see above)

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
            # Anchor the shiroa wrapper's XDG cache to repo-root regardless
            # of invocation cwd (bug_022: `cd docs && shiroa serve .`
            # otherwise writes to docs/docs/.cache/). git rev-parse so
            # `nix develop` from a subdirectory also resolves correctly.
            export RIO_TYPST_XDG="$(git rev-parse --show-toplevel)/docs/.cache/typst-xdg"
            ${preCommitInstall}
          '';
        }
      );
in
{
  default = mkRioShell rustNightly;
  stable = mkRioShell rustStable;
}
