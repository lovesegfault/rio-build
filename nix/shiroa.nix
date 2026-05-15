{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "shiroa";
  # lovesegfault/shiroa@rio-pin: upstream main@fea5b750 + three patches
  # carried as upstream PRs:
  #   - Myriad-Dreamin/shiroa#238 (typst 0.14.2 / typst.ts v0.7.0-rc2)
  #   - Myriad-Dreamin/shiroa#239 (sidebar items.sum(default: []))
  #   - Myriad-Dreamin/shiroa#240 (--input KEY=VALUE → sys.inputs)
  # Drop the fork once all three land upstream.
  version = "0.3.1-unstable-2026-05-15";
  src = fetchFromGitHub {
    owner = "lovesegfault";
    repo = "shiroa";
    rev = "4e42404b89d6b1d436097ba2fd9f4c611f7a15ff";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-623pzeI1El8K4SQbGEVnMXInMqhDXtutYSrSqoGMLUw=";
  };
  cargoHash = "sha256-D9BLf8KBJ1nxsci+vkE1bVr9z40OZlq8Be/GVivsKfA=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
