{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "shiroa";
  # lovesegfault/shiroa@rio-pin: upstream main@fea5b750 + two patches
  # carried as upstream PRs:
  #   - Myriad-Dreamin/shiroa#238 (typst 0.14.2 / typst.ts v0.7.0-rc2)
  #   - Myriad-Dreamin/shiroa#239 (sidebar items.sum(default: []))
  # Drop the fork once both land upstream.
  version = "0.3.1-unstable-2026-05-15";
  src = fetchFromGitHub {
    owner = "lovesegfault";
    repo = "shiroa";
    rev = "a8363c406e9451e83bb7297dd2fc8685f0e45101";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-nAS/MaXLOciH0jPXBJbeN/bj0tXKZC/j4EZH06oo5Io=";
  };
  cargoHash = "sha256-D9BLf8KBJ1nxsci+vkE1bVr9z40OZlq8Be/GVivsKfA=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
