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
  rustPlatform,
  pkgs,
  tracey-src,
}:
let
  # Flake input (not fetchFromGitHub): Cargo.lock is a store path at
  # eval time, so importCargoLock reads it without IFD.
  src = tracey-src;
  # Workspace Cargo.toml reports 2.0.0-rc.0 — no tag yet past the
  # typst-spec fork commits we want.
  version = "2.0.0-rc.0-unstable-2026-06-18";

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
      hash = "sha256-4GDQkMyWfCD8sQxawVGh5IoTQ7YtFfFAnW++FXnJ76Y=";
    };

    nativeBuildInputs = with pkgs; [
      nodejs
      pnpm_10
      pnpmConfigHook
      patchelf
    ];

    # `pnpm run build` is `tsc && vite build`. @typescript/native-preview
    # (a devDep) fetches a platform binary via postinstall, which
    # --ignore-scripts in fetchPnpmDeps/pnpmConfigHook skips. Vite's own
    # esbuild transform handles TS, so skip the standalone tsc step.
    # sass-embedded-linux-x64 ships a prebuilt dart AOT runner linked
    # against /lib64/ld-linux — patch its interpreter so vite's sass
    # loader can spawn it (ENOENT otherwise).
    buildPhase = ''
      runHook preBuild
      for f in node_modules/.pnpm/sass-embedded-linux-x64@*/node_modules/sass-embedded-linux-x64/dart-sass/src/dart; do
        patchelf --set-interpreter ${pkgs.stdenv.cc.bintools.dynamicLinker} "$f"
      done
      pnpm exec vite build
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  };
in
rustPlatform.buildRustPackage {
  pname = "tracey";
  inherit version src;

  # Upstream's typst spec scanner treats every `#fn("str")` call as a
  # requirement marker (denylist of stdlib names only). unify's
  # `#qty("5","s")` etc. then collide as duplicate rule "5", config
  # validation fails, daemon falls back to empty config, and `validate`
  # exits 0. Patch narrows extraction to `#r`/`#req` and makes validate
  # exit nonzero when a config error left the spec set empty.
  # The typst-narrow-marker patch (allowlist + config-error exit) is now
  # upstream in the typst-spec branch as of 8e27ba6; no local patches needed.

  cargoLock.lockFile = "${src}/Cargo.lock";

  # Default features include `search` (tantivy) — enables Cmd+K fuzzy
  # search in `tracey web`. tantivy bundles zstd/lz4 (cmake handles
  # the vendored C build; no extra buildInputs needed).
  cargoBuildFlags = [
    "-p"
    "tracey"
  ];

  # arborium (tree-sitter) needs a C compiler + cmake.
  nativeBuildInputs = with pkgs; [
    pkg-config
    cmake
  ];
  buildInputs = with pkgs; [
    openssl
  ];

  # cmake is for tantivy/tree-sitter sub-builds, not this derivation's
  # configurePhase. Without this the cmake setup hook looks for a
  # top-level CMakeLists.txt.
  dontUseCmakeConfigure = true;

  # Tests hit the live dashboard + need a real repo layout — skip.
  doCheck = false;

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
      'let dashboard_out = Path::new(&out_dir).join("dashboard");' \
      'let dashboard_out = Path::new(&out_dir).join("dashboard");
      if let Ok(prebuilt) = std::env::var("TRACEY_PREBUILT_DASHBOARD") {
          copy_dir_recursive(std::path::Path::new(&prebuilt), &dashboard_out.join("dist"));
          return;
      }'
  '';
}
