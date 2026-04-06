# rio-dashboard: Svelte 5 SPA, built via fetchPnpmDeps + vite.
#
# Pattern follows nix/tracey.nix:32-68 with one key deviation: tracey skips
# `tsc` because upstream uses @typescript/native-preview which fetches a
# platform binary via postinstall (--ignore-scripts breaks it). We use stock
# typescript@5 — no postinstall — so svelte-check works in the sandbox and
# runs as part of `pnpm run build`. Lint + test also run in-sandbox.
#
# HASH BUMPS: pnpmDeps.hash changes whenever rio-dashboard/pnpm-lock.yaml
# changes. The dance: (1) add/bump the dep in package.json, (2) regenerate
# lockfile via `nix develop -c bash -c 'cd rio-dashboard && pnpm install
# --lockfile-only'`, (3) set `hash = pkgs.lib.fakeHash` below, (4) run
# `/nixbuild .#checks.x86_64-linux.dashboard` — it fails with the real hash,
# (5) paste it here. One fakeHash, one paste — no recursive mismatches.
{ pkgs }:
let
  # Scoped to rio-dashboard/ (not repo root). cleanSource NOT cleanCargoSource
  # — crane's cargo filter drops .ts/.svelte/.json. In flake context, git-
  # untracked files (node_modules/, dist/) are already filtered by the flake
  # store-path mechanism; cleanSource additionally strips editor-backup cruft.
  #
  # Scoping to the subdirectory means Rust-only commits don't invalidate
  # the dashboard derivation (the plan's `../.` sketch would have made every
  # `cargo fmt` rebuild vite — FOD pnpmDeps short-circuits on output hash,
  # but the stdenvNoCC build wouldn't).
  src = pkgs.lib.cleanSource ../rio-dashboard;

  # Proto input for TS codegen. Kept SEPARATE from src so editing rio-proto's
  # Cargo.toml / build.rs doesn't rebuild vite — only the .proto files
  # themselves are hashed. When a .proto changes, this path changes, preBuild
  # re-runs buf generate, and svelte-check catches any resulting type breakage.
  # The inverse is also true: this drv doesn't see stale stubs if someone adds
  # an rpc and forgets to regen locally — the sandbox always generates fresh.
  protoSrc = pkgs.lib.cleanSource ../rio-proto/proto;
in
pkgs.stdenvNoCC.mkDerivation {
  pname = "rio-dashboard";
  version = "0.1.0";
  inherit src;

  pnpmDeps = pkgs.fetchPnpmDeps {
    pname = "rio-dashboard";
    version = "0.1.0";
    inherit src;
    pnpm = pkgs.pnpm_10;
    fetcherVersion = 3;
    hash = "sha256-tn2tl4rVMkxThV3HfjlVh7UQutwX6QyDsgRo2SvCph8=";
  };

  nativeBuildInputs = with pkgs; [
    nodejs
    pnpm_10
    pnpmConfigHook
    # TS proto codegen. Both from nixpkgs — no npm postinstall binary trap
    # (nixpkgs builds protoc-gen-es from source and wraps with Node). buf's
    # `generate` subcommand with a `local:` plugin ref needs no network:
    # it just execs protoc-gen-es off PATH.
    buf
    protoc-gen-es
  ];

  # TS stub generation. Runs BEFORE lint/test/build so svelte-check sees the
  # types. buf.gen.yaml declares `out: src/gen` relative to cwd (we're in the
  # unpacked src root by the time preBuild fires — pnpmConfigHook already ran).
  #
  # Assertion on admin_pb.ts: protoc-gen-es v2 emits ONE file per .proto with
  # both messages and the GenService descriptor — there is no separate
  # *_connect.ts (that's the deprecated v1 layout). The AdminService grep is
  # the real exit-criterion test: if buf silently no-ops (e.g., plugin not
  # found on PATH → empty output, exit 0) we fail here instead of in a cryptic
  # svelte-check cascade when App.svelte can't resolve the import.
  preBuild = ''
    buf generate --template buf.gen.yaml ${protoSrc}
    test -f src/gen/admin_pb.ts
    grep -q 'export const AdminService' src/gen/admin_pb.ts
  '';

  # lint → test → build. `pnpm run build` = `svelte-check && vite build`.
  # All three must pass for a green dashboard check in .#ci.
  buildPhase = ''
    runHook preBuild
    pnpm run lint
    pnpm run test
    pnpm run build
    runHook postBuild
  '';

  # Exit-criteria assertions inline: fail fast if vite produced an empty
  # shell (e.g., if the entry module silently failed to resolve).
  installPhase = ''
    runHook preInstall
    cp -r dist $out
    test -f $out/index.html
    test -n "$(ls $out/assets/*.js 2>/dev/null)"
    runHook postInstall
  '';
}
