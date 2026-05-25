# Workspace Cargo.lock vendoring arguments for nixpkgs'
# `importCargoLock`, shared by every check that bypasses crate2nix and
# vendors the dep tree wholesale (`deny`, `hakari-drift`, the
# cargo-mutants builders).
#
# The workspace has TWO vendoring paths and therefore TWO hash
# registries for git-pinned dependencies:
#
#   - crate2nix (the main build graph) reads `crate-hashes.json`,
#     maintained automatically by `cargo xtask regen cargo-json`.
#   - `importCargoLock` (this file's consumers) requires an explicit
#     `outputHashes."<name>-<version>"` entry per git dependency and
#     fails AT EVAL with "No hash was found while vendoring the git
#     dependency <name>-<version>" when one is missing.
#
# Every future `[patch.crates-io] foo = { git = ...; rev = ...; }`
# needs an entry in BOTH places. The hash is the same value in two
# encodings (both paths end in `pkgs.fetchgit { url, rev, hash }` of
# the same checkout): copy the base32 from `crate-hashes.json` and
# `nix hash convert --hash-algo sha256 --to sri <base32>`, or let the
# eval/build error of either path tell you the hash it expected.
{
  lockFile = ../../Cargo.lock;
  outputHashes = {
    # Pinned to upstream master for PR #660's raw BackingId
    # marshalling — see the [patch.crates-io] comment in the workspace
    # Cargo.toml for the rationale and the drop condition.
    "fuser-0.17.0" = "sha256-iBSHT73HH6KpqMUtAvbpJcGQNeR3ss8RYajGq08NNl4=";
  };
}
