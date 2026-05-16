{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "shiroa";
  # lovesegfault/shiroa@rio-pin: upstream main@fea5b750 + four patches
  # carried as upstream PRs:
  #   - Myriad-Dreamin/shiroa#238 (typst 0.14.2 / typst.ts v0.7.0-rc2)
  #   - Myriad-Dreamin/shiroa#239 (sidebar items.sum(default: []))
  #   - Myriad-Dreamin/shiroa#240 (--input KEY=VALUE → sys.inputs)
  #   - Myriad-Dreamin/shiroa#241 (mdbook: search-js = search-enabled)
  # Drop the fork once all four land upstream.
  version = "0.3.1-unstable-2026-05-16";
  src = fetchFromGitHub {
    owner = "lovesegfault";
    repo = "shiroa";
    rev = "c8a03a05b721f1fe0d0408ad981d79ca066cef2d";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-9LKgw65fexzNkAFL0ZecGfPOxgp5y8EdAsf7GbgiB5Y=";
  };
  cargoHash = "sha256-D9BLf8KBJ1nxsci+vkE1bVr9z40OZlq8Be/GVivsKfA=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
