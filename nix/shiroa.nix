{
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  openssl,
}:
rustPlatform.buildRustPackage {
  pname = "shiroa";
  # main@fea5b750: post-v0.3.1-rc4. Includes 4240a2b1 (reflexo-typst
  # git-rev bump, "Fixed HTML export issues") and 83144846 (module
  # reorganization). Embedded typst is still 0.14.0.
  version = "0.3.1-unstable-2025-12-14";
  src = fetchFromGitHub {
    owner = "Myriad-Dreamin";
    repo = "shiroa";
    rev = "fea5b750fb5e6e1ba6841b25f5bc1e7d08f3fa90";
    # assets/artifacts/ (renderer wasm + frontend JS) is a submodule;
    # cli/src/project.rs include_bytes!()s from it at compile time.
    fetchSubmodules = true;
    hash = "sha256-kvovTHi1WI/LMHUxBI6B1PcGb9DNdXjMxTvDMO51hwQ=";
  };
  cargoHash = "sha256-Gi5Dx8xbCOBpfUTdi3zQTfqFkk5QNSB++lukSw9K7gU=";
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];
  doCheck = false;
  meta.mainProgram = "shiroa";
}
