{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "shiroa";
  # lovesegfault/shiroa@rio-pin: upstream main@fea5b750 + five patches.
  # First four are upstream PRs; the fifth supersedes #243 on the fork
  # only (live-merge instead of freeze; not yet sent upstream):
  #   - Myriad-Dreamin/shiroa#238 (typst 0.14.2 / typst.ts v0.7.0-rc2)
  #   - Myriad-Dreamin/shiroa#239 (sidebar items.sum(default: []))
  #   - Myriad-Dreamin/shiroa#240 (--input KEY=VALUE → sys.inputs)
  #   - Myriad-Dreamin/shiroa#241 (mdbook: search-js = search-enabled)
  #   - serve: persist SearchRenderer; per-chapter merge, live index
  # Drop the fork once all land upstream.
  version = "0.3.1-unstable-2026-05-16";
  src = fetchFromGitHub {
    owner = "lovesegfault";
    repo = "shiroa";
    rev = "e6531b801aa898320675191046ec86e8186f439c";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-B6FnNatMyDJPLzm3KkkbB1Jg+XmjkCgl9z2t7ZCVNXs=";
  };
  cargoHash = "sha256-D9BLf8KBJ1nxsci+vkE1bVr9z40OZlq8Be/GVivsKfA=";
  # assets/artifacts/svg_utils.js console.log()s "new svg util updated
  # 37" on every page load. The file lives in the assets/artifacts
  # SUBMODULE (Myriad-Dreamin/typst, upstream-owned) so a rio-pin commit
  # can't carry it without forking the submodule too — patch post-fetch
  # instead. The build-mode output strips the script tag entirely
  # (nix/docs-svg-dedup.py); this is for `shiroa serve` dev-loop UX.
  postPatch = ''
    sed -i 's/console\.log("new svg util updated[^;]*;//' \
      assets/artifacts/svg_utils.js
  '';
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
