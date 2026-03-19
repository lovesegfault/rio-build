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
    hash = pkgs.lib.fakeHash; # replaced after first /nixbuild (T5)
  };

  nativeBuildInputs = with pkgs; [
    nodejs
    pnpm_10
    pnpmConfigHook
  ];

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
