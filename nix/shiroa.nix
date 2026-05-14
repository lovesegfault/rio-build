{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage rec {
  pname = "shiroa";
  version = "0.3.1-rc4";
  src = fetchFromGitHub {
    owner = "Myriad-Dreamin";
    repo = "shiroa";
    rev = "v${version}";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-adrKcGLgKYExyqPk8jiINhw1ClryL0ajqmdDtbM2rC4=";
  };
  cargoHash = "sha256-uFICiSNZGho1K+9sGyokDyrSZTpg9HfJSmbatNebFjg=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
