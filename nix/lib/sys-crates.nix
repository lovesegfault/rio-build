# sys-crate linkage: per-crate single source of truth.
#
# Each sys-crate that system-links instead of vendoring C gets its
# env-var escape hatch + system lib here. Per-crate shape so crate2nix
# crateOverrides can reference .crates.<name> directly; devShell
# consumes the derived .allEnv/.allLibs aggregates.
#
# Adding a sys-crate: add a .crates.<name> entry here, add the
# override in nix/crate2nix.nix referencing it, done.
#
# Extracted from flake.nix's perSystem `let` block — pure function,
# no cross-binding deps (only reads pkgs).
{ pkgs }:
let
  crates = {
    # build.rs:49-53 escape hatch: routes build_linked →
    # pkg-config probe instead of compiling the bundled
    # amalgamation (sqlx's `sqlite` → sqlx-sqlite/bundled
    # feature chain otherwise forces vendoring).
    # bundled_bindings stays — precompiled Rust bindings,
    # no bindgen; SQLite 3.x ABI stability makes them work
    # against any 3.x system lib.
    libsqlite3-sys = {
      env.LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
      libs = [ pkgs.sqlite ];
    };
    # build.rs:30 escape hatch: probe → system libzstd.
    zstd-sys = {
      env.ZSTD_SYS_USE_PKG_CONFIG = "1";
      libs = [ pkgs.zstd ];
    };
    # No escape-hatch env var — fuser's build.rs already
    # defaults to pkg-config (never bundles).
    fuser = {
      env = { };
      libs = [ pkgs.fuse3 ];
    };
  };
in
{
  inherit crates;
  # Derived aggregates for the dev shell (workspace-wide
  # buildInputs + env).
  allEnv = pkgs.lib.foldl' (a: c: a // c.env) { } (pkgs.lib.attrValues crates);
  allLibs = pkgs.lib.concatMap (c: c.libs) (pkgs.lib.attrValues crates);
}
