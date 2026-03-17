# tracey: spec-coverage CLI + web dashboard (https://github.com/bearcove/tracey)
#
# Links r[req.id] markers in markdown specs to # r[impl …] / # r[verify …]
# code annotations, surfaces uncovered/untested/stale via CLI/LSP/MCP.
# rio-build uses `tracey query validate` as a CI check (checks.tracey-validate).
#
# The web dashboard is a Vite+Preact SPA. Upstream's build.rs shells out
# to `pnpm install && pnpm build` — breaks in the sandbox. We build the
# dashboard as a separate derivation (fetchPnpmDeps → offline pnpm store →
# `pnpm run build`), then patch build.rs to copy the pre-built dist/ into
# $OUT_DIR instead of invoking pnpm. Assets are embedded via
# include_str!(concat!(env!("OUT_DIR"), "/dashboard/dist/...")) on main.
{
  craneLib,
  pkgs,
  tracey-src,
}:
let
  # Flake input (not fetchFromGitHub): crane reads Cargo.lock from a
  # pre-fetched path, so evaluation doesn't need allow-import-from-derivation.
  src = tracey-src;
  # Workspace Cargo.toml on main still reports 1.3.0 — no tag yet past
  # the Nix-lang-support / validate-exit-code commits we want.
  version = "1.3.0-unstable-2026-03-13";

  dashboardRoot = "crates/tracey/src/bridge/http/dashboard";

  # Vite+Preact SPA → $out/{index.html,assets/index.{js,css}}.
  # Uses the committed api-types.ts — build.rs regenerates it from Rust
  # types, but the checked-in file is the source of truth for the npm
  # build (upstream's CI keeps them in sync).
  traceyDashboard = pkgs.stdenvNoCC.mkDerivation {
    pname = "tracey-dashboard";
    inherit version src;
    # Flake inputs are /nix/store/<hash>-source; stripHash → "source".
    sourceRoot = "source/${dashboardRoot}";

    pnpmDeps = pkgs.fetchPnpmDeps {
      pname = "tracey-dashboard";
      inherit version src;
      sourceRoot = "source/${dashboardRoot}";
      pnpm = pkgs.pnpm_10;
      fetcherVersion = 3;
      hash = "sha256-PtaMB8FSS4vNZMcRiGCqzm5tug9CFvzx3O8GLlv/xyk=";
    };

    nativeBuildInputs = with pkgs; [
      nodejs
      pnpm_10
      pnpmConfigHook
    ];

    # `pnpm run build` is `tsc && vite build`. @typescript/native-preview
    # (a devDep) fetches a platform binary via postinstall, which
    # --ignore-scripts in fetchPnpmDeps/pnpmConfigHook skips. Vite's own
    # esbuild transform handles TS, so skip the standalone tsc step.
    buildPhase = ''
      runHook preBuild
      pnpm exec vite build
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  };

  commonArgs = {
    inherit src;
    pname = "tracey";
    inherit version;
    strictDeps = true;
    # Default features include `search` (tantivy) — enables Cmd+K fuzzy
    # search in `tracey web`. tantivy bundles zstd/lz4 (cmake handles
    # the vendored C build; no extra buildInputs needed).
    cargoExtraArgs = "-p tracey";

    # arborium (tree-sitter) needs a C compiler + cmake.
    nativeBuildInputs = with pkgs; [
      pkg-config
      cmake
    ];
    buildInputs = with pkgs; [
      openssl
    ];

    # Tests hit the live dashboard + need a real repo layout — skip.
    doCheck = false;
  };

  # Deps-only build: crane dummies out the source tree (no build.rs), so
  # no patch and no dashboard dependency here — just vendor cargo deps.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;

    # build.rs: emit_tracey_version_metadata() falls back to `git rev-parse`
    # (fails in sandbox). Setting this makes `tracey --version` show the pin.
    TRACEY_GIT_COMMIT = tracey-src.rev;

    # build.rs: build_dashboard() is patched below to copy from here into
    # $OUT_DIR/dashboard/dist and return, skipping pnpm entirely. We can't
    # prepopulate $OUT_DIR ourselves — cargo creates it per-crate right
    # before invoking build.rs.
    TRACEY_PREBUILT_DASHBOARD = "${traceyDashboard}";

    # --replace-fail: hard-fail on anchor drift rather than silently
    # falling through to pnpm (which would hang on sandbox network).
    # copy_dir_recursive is defined by build.rs itself — it creates the
    # dst dir and skips entries named node_modules/dist (our prebuilt has
    # index.html + assets/ at the top level, neither name is filtered).
    postPatch = ''
      substituteInPlace crates/tracey/build.rs --replace-fail \
        'let dist_dir = dashboard_out.join("dist");' \
        'let dist_dir = dashboard_out.join("dist");
        if let Ok(prebuilt) = std::env::var("TRACEY_PREBUILT_DASHBOARD") {
            copy_dir_recursive(std::path::Path::new(&prebuilt), &dist_dir);
            return;
        }'
    '';
  }
)
