# tracey: spec-coverage CLI (https://github.com/bearcove/tracey)
#
# Links r[req.id] markers in markdown specs to // r[impl …] / // r[verify …]
# code annotations, surfaces uncovered/untested/stale via CLI/LSP/MCP.
# rio-build uses `tracey query validate` as a CI check (grep for "0 total error(s)" — v1.3.0 exit code is always 0).
#
# Build note: upstream's build.rs runs `pnpm install` + `pnpm build` to
# compile the web dashboard, which breaks in the nix sandbox (network
# access, no node/pnpm). BUT build.rs has an early-return if stub dist
# assets already exist. We write stubs in preConfigure — `tracey web`
# shows a placeholder, but `tracey query`/`mcp`/`lsp` work unaffected.
# CI only uses `query validate`, so this is lossless for our pipeline.
{
  craneLib,
  pkgs,
}:
let
  src = pkgs.fetchFromGitHub {
    owner = "bearcove";
    repo = "tracey";
    rev = "v1.3.0";
    hash = "sha256-QEtOCy1+tvWHWn8yrAJM7unWq0AIP/+k8wEOH+B2V6M=";
  };

  # The dashboard assets are embedded via `include_str!` from the SOURCE
  # tree (crates/tracey/src/bridge/http/dashboard/dist/), not OUT_DIR.
  # build.rs also writes to OUT_DIR/dashboard/dist/ for the watch-mode
  # serve path. Stub both: write assets into the source tree here, and
  # replace build.rs with a minimal version that writes to OUT_DIR.
  patchedSrc = pkgs.runCommand "tracey-src-patched" { } ''
    cp -r ${src} $out
    chmod -R +w $out

    # Stub build.rs: keep the Styx schema generation (pure Rust,
    # needed for config parsing), drop everything else.
    cat > $out/crates/tracey/build.rs <<'EOF'
    fn main() {
        println!("cargo:rustc-env=TRACEY_GIT_COMMIT=nix-build");
        println!("cargo:rustc-env=TRACEY_BUILD_DATE=1970-01-01");
        facet_styx::GenerateSchema::<tracey_config::Config>::new()
            .crate_name("tracey-config")
            .version("1")
            .cli("tracey")
            .write("schema.styx");
        // OUT_DIR stub — some code may include_dir! from there
        let out = std::env::var("OUT_DIR").unwrap();
        let d = std::path::Path::new(&out).join("dashboard/dist/assets");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.parent().unwrap().join("index.html"), "").unwrap();
        std::fs::write(d.join("index.js"), "").unwrap();
        std::fs::write(d.join("index.css"), "").unwrap();
    }
    EOF

    # Stub source-tree dist — include_str! reads from here
    dist=$out/crates/tracey/src/bridge/http/dashboard/dist
    mkdir -p $dist/assets
    cat > $dist/index.html <<'HTML'
    <!doctype html><title>tracey</title><body style="font-family:monospace;padding:2em">
    <h1>dashboard disabled</h1>
    <p>This tracey build was compiled without the web dashboard (nix sandbox cannot run pnpm).</p>
    <p>Use <code>tracey query status</code> / <code>uncovered</code> / <code>validate</code> instead.</p>
    </body>
    HTML
    touch $dist/assets/index.js
    touch $dist/assets/index.css

    # api-types.ts is written by the TS generator in the real build.rs.
    # The crate embeds it via include_str! — ensure a stub exists.
    touch $out/crates/tracey/src/bridge/http/dashboard/src/api-types.ts
  '';

  commonArgs = {
    src = patchedSrc;
    pname = "tracey";
    version = "1.3.0";
    strictDeps = true;
    # Build only the CLI crate; skip the tantivy search feature (heavy,
    # only used by `tracey web` Cmd+K fuzzy search — CLI queries don't need it).
    cargoExtraArgs = "-p tracey --no-default-features";
    # arborium (tree-sitter) needs a C compiler; openssl for hyper-util TLS
    nativeBuildInputs = with pkgs; [
      pkg-config
      cmake
    ];
    buildInputs = with pkgs; [
      openssl
    ];
    # Tests hit the real dashboard paths — skip.
    doCheck = false;
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; })
