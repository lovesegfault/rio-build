{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "shiroa";
  # lovesegfault/shiroa@rio-pin: upstream main@fea5b750 + five patches
  # carried as upstream PRs:
  #   - Myriad-Dreamin/shiroa#238 (typst 0.14.2 / typst.ts v0.7.0-rc2)
  #   - Myriad-Dreamin/shiroa#239 (sidebar items.sum(default: []))
  #   - Myriad-Dreamin/shiroa#240 (--input KEY=VALUE → sys.inputs)
  #   - Myriad-Dreamin/shiroa#241 (mdbook: search-js = search-enabled)
  #   - Myriad-Dreamin/shiroa#243 (serve: don't clobber full searchindex)
  # Drop the fork once all five land upstream.
  version = "0.3.1-unstable-2026-05-16";
  src = fetchFromGitHub {
    owner = "lovesegfault";
    repo = "shiroa";
    rev = "f91ef138b6bc43b69f01cdf34280d093c464adcb";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-Hl08ja0z8hxaTb3dhnAlP2ykkaFgImMseFbOl/0uUc8=";
  };
  cargoHash = "sha256-D9BLf8KBJ1nxsci+vkE1bVr9z40OZlq8Be/GVivsKfA=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
