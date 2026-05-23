# Builds the Kani verifier toolchain from source: kani-compiler (a
# rustc_driver plugin that emits CBMC goto-C), kani-driver (the verify
# orchestrator), and the kani sysroot (kani lib + always-encode-mir std).
#
# Pinning: Kani is tightly coupled to a specific rustc nightly because
# kani-compiler links against rustc_private (librustc_driver.so). The
# nightly is read from Kani's rust-toolchain.toml at the pinned tag.
# Bumping `kaniVersion` MUST be accompanied by re-reading that file and
# bumping `kaniNightlyDate` — and that, in turn, invalidates every
# downstream Kani derivation (different nightly → different sysroot
# ABI → all goto-C artifacts rebuild). Bump deliberately, never via
# `nix flake update`.
#
# As of the lovesegfault/kani/rio-build pin (`6f9696f4f`), `nix/kani.nix`
# no longer replicates kani-driver's verify pipeline (it delegates to
# `kani verify-artifacts`), and `kaniBaseFlags` no longer transcribes the
# soundness-critical rustc flags (kani-compiler sets them as defaults).
# The surviving `kaniBaseFlags` transcription is install paths and the
# `-Zcrate-attr` pair, which are stable across kani releases.
#
# Bump checklist:
#   1. Has kani 0.68.0+ shipped with both upstream PRs merged?
#      (`kani verify-artifacts`, kani-compiler default rustc flags.)
#      If yes: swap `kaniSrcRaw` back to the release tag, drop this
#      comment block.
#   2. Re-read `rust-toolchain.toml` from the new kani source → bump
#      `kaniNightlyDate`.
#   3. Re-read `kani-dependencies` → check whether the NEW kani still
#      expects CBMC 6.8.0; the eval-time `assertMsg` on the cbmc pin
#      fails on any `kaniVersion` bump until that block is re-derived
#      (or the pin is dropped per its DROP condition).
#
# Pins (verified against the kani-0.67.0 tag):
#   kani:    rio-build branch (lovesegfault/kani) = kani-0.67.0 + 2 patches
#            (verify-artifacts subcommand, compiler default rustc flags;
#            upstream PRs to model-checking/kani in flight)
#   nightly: 2025-11-21        (rust-toolchain.toml)
#   cbmc:    6.8.0             (kani-dependencies; pinned locally below —
#                               nixpkgs has moved on to a newer cbmc)
#   kissat:  >= 4.0.1          (kani-dependencies; nixpkgs 4.0.4 satisfies)
#
# Layout (release-shaped, so kani-driver classifies as InstallType::Release):
#   $out/bin/{kani-compiler,kani-driver,kani,cargo-kani}
#   $out/lib/                  verification sysroot (always-encode-mir std + libkani)
#   $out/playback/lib/         concrete-playback sysroot
#   $out/no_core/lib/          no_core sysroot
#   $out/library/{kani,kani_macros,std}/   sources kani-driver re-reads at run time
#   $out/toolchain -> kaniNightly          where kani-driver finds cargo
#   $out/rust-toolchain-version, $out/rustc-version
{
  pkgs,
  lib,
}:
let
  kaniVersion = "0.67.0";
  kaniNightlyDate = "2025-11-21";

  # kani's workspace path-deps `charon` (a git submodule). Fetched
  # separately rather than `fetchSubmodules = true` — that would also pull
  # `firecracker` (~hundreds of MB) and `tests/perf/s2n-quic`, both of
  # which are workspace-excluded test fixtures the build never reads.
  # The submodule SHA is from `git ls-tree kani-0.67.0 charon`.
  charonSrc = pkgs.fetchFromGitHub {
    owner = "AeneasVerif";
    repo = "charon";
    rev = "30cab88265206f4fa849736e704983e39a404d96";
    hash = "sha256-T4cRt5gSW4mpOePDWu/oaBkVJYgd04Fyn93qjw33CbI=";
  };

  kaniSrcRaw = pkgs.fetchFromGitHub {
    owner = "lovesegfault";
    repo = "kani";
    # rio-build branch: kani-0.67.0 + 'kani verify-artifacts' subcommand +
    # kani-compiler default rustc flags. Both are upstream PRs to
    # model-checking/kani. Swap back to the release tag (and re-derive
    # nightly/CBMC pins) when kani 0.68.0 ships with both merged.
    rev = "6f9696f4fd126f6d50001ec5053fe109a61c4452";
    hash = "sha256-RFYTgVKazZQeXsEsrArL3bPrtc+q+/AGuO0hfmS/BQE=";
  };

  kaniSrc = pkgs.runCommand "kani-${kaniVersion}-src" { } ''
    cp -a ${kaniSrcRaw} $out
    chmod -R u+w $out
    rm -rf $out/charon
    cp -a ${charonSrc} $out/charon
  '';

  # The exact nightly Kani's rust-toolchain.toml pins. `rustc-dev` provides
  # the rustc_private crates (rustc_driver, rustc_middle, …) kani-compiler
  # links against; `rust-src` lets `-Z build-std` rebuild std with
  # `-Z always-encode-mir`. NOT the project's `rustNightly`
  # (selectLatestNightlyWith) — Kani's rustc_private ABI dependency means
  # the date must be pinned exactly, not "latest".
  kaniNightly = pkgs.rust-bin.nightly.${kaniNightlyDate}.default.override {
    extensions = [
      "llvm-tools"
      "rustc-dev"
      "rust-src"
      "rustfmt"
    ];
  };

  kaniRustPlatform = pkgs.makeRustPlatform {
    rustc = kaniNightly;
    cargo = kaniNightly;
  };

  # Kani 0.67.0 pins CBMC 6.8.0 and requires Kissat >= 4.0.1
  # (kani-dependencies). Kissat still floats with the project's nixpkgs
  # lock, so its eval-time assert below is what turns a routine
  # `nix flake update` that drops below the requirement into a loud
  # eval failure instead of a silent kani-driver break. CBMC no longer
  # floats: it is pinned locally below (`cbmcPinned`, with its own DROP
  # condition), so its assert instead guards the pairing of that pin
  # with the `kaniVersion` it was derived from and exists to force this
  # block to be revisited when `kaniVersion` moves. When bumping
  # `kaniVersion`, re-read kani-dependencies at the new tag and update
  # the expected versions here in the same commit.
  #
  # nixpkgs' cbmc has moved past the 6.8.0 release kani 0.67.0 pins
  # (kani-driver checks the version at startup and refuses a mismatch),
  # so rebuild the same nixpkgs recipe at 6.8.0: the 6.8.0 source, the
  # cadical-2.0.0 vendored solver source that release expects, and the
  # matching do-not-download cmake patches (vendored under
  # nix/patches/cbmc-6.8.0/, taken from the last nixpkgs revision that
  # shipped 6.8.0). DROP this override — back to plain `pkgs.cbmc`, and
  # delete nix/patches/cbmc-6.8.0/ — when the kani fork is refreshed to
  # a release whose kani-dependencies accepts the cbmc nixpkgs ships;
  # the bump checklist above governs that refresh.
  cbmcPinned = pkgs.cbmc.overrideAttrs {
    version = "6.8.0";
    src = pkgs.fetchFromGitHub {
      owner = "diffblue";
      repo = "cbmc";
      tag = "cbmc-6.8.0";
      hash = "sha256-PT6AYiwkplCeyMREZnGZA0BKl4ZESRC02/9ibKg7mYU=";
    };
    srccadical = (pkgs.cadical.override { version = "2.0.0"; }).src;
    patches = [
      (pkgs.replaceVars ./patches/cbmc-6.8.0/0001-Do-not-download-sources-in-cmake.patch {
        cudd = pkgs.cudd.src;
      })
      ./patches/cbmc-6.8.0/0002-Do-not-download-sources-in-cmake.patch
    ];
  };
  cbmc =
    assert lib.assertMsg (kaniVersion == "0.67.0")
      "the local CBMC 6.8.0 pin above was derived from kani 0.67.0's kani-dependencies; kaniVersion is now ${kaniVersion} — re-read kani-dependencies at the new tag and update or drop the pin (see its DROP condition)";
    cbmcPinned;
  kissat =
    assert lib.assertMsg (lib.versionAtLeast pkgs.kissat.version "4.0.1")
      "kani ${kaniVersion} needs Kissat >= 4.0.1, nixpkgs has ${pkgs.kissat.version}";
    pkgs.kissat;
  # PATH prefix the kani/cargo-kani/kani-driver wrappers carry. kani-driver
  # invokes everything by bare name on PATH:
  #   - cbmc, goto-cc, goto-instrument  (cbmc package)
  #   - kissat                          (SAT backend, kani's default)
  #   - gcc                             (goto-cc execvp's it for `-E`
  #                                      preprocessing of kani_lib.c —
  #                                      nixpkgs cbmc does NOT propagate or
  #                                      patch in a compiler)
  # `nix shell`/devshell mask a missing gcc here because the host PATH has
  # one; a runCommand sandbox does not. Keep this list complete so the
  # wrapped `kani` is hermetic.
  kaniRuntimePath = lib.makeBinPath [
    cbmc
    kissat
    pkgs.gcc
  ];

  # `cargo build-dev` runs `-Z build-std` to rebuild std/core/alloc with
  # always-encode-mir. That fetches std's own dependency graph — which is
  # NOT in kani's Cargo.lock; it lives in the toolchain's
  # lib/rustlib/src/rust/library/Cargo.lock. Vendor both lockfiles and
  # union the crate dirs so cargo-in-offline-mode can resolve everything.
  kaniVendor = kaniRustPlatform.importCargoLock {
    # kaniSrcRaw, not kaniSrc: identical today (the charon swap doesn't
    # touch Cargo.lock), and reading from the FOD avoids re-vendoring on
    # unrelated kaniSrc changes — but a future patch to kaniSrc's
    # Cargo.lock would be invisible here. Switch to ${kaniSrc} if so.
    lockFile = "${kaniSrcRaw}/Cargo.lock";
    outputHashes = {
      # Sole git dep in kani 0.67.0's lockfile. The 0.4.1 crates.io entry
      # for the same crate name is content-addressed and needs no hash.
      "tracing-tree-0.4.0" = "sha256-YjkXsOn/aLEYxvys9TFZjTyUMPxk3WsI6bbJsbwSiKY=";
    };
  };
  rustSrcVendor = kaniRustPlatform.importCargoLock {
    lockFile = "${kaniNightly}/lib/rustlib/src/rust/library/Cargo.lock";
  };
  # The drv name MUST be `cargo-vendor-dir` — the importCargoLock-emitted
  # config.toml hardcodes `directory = "cargo-vendor-dir"`, and
  # cargoSetupPostUnpackHook materialises $cargoDeps at
  # $NIX_BUILD_TOP/$(stripHash $cargoDeps). A different name → cargo
  # can't find the vendored crates.
  cargoDeps = pkgs.runCommand "cargo-vendor-dir" { } ''
    # Keep kani's Cargo.lock and .cargo/config.toml as the canonical ones
    # (cargoSetupPostPatchHook validates Cargo.lock against the source
    # tree) and union the std crate dirs in.
    cp -a ${kaniVendor} $out
    chmod -R u+w $out
    for crate in ${rustSrcVendor}/*/; do
      name=$(basename "$crate")
      [ -e "$out/$name" ] || cp -a "$crate" "$out/$name"
    done
  '';

  kaniBuild = pkgs.stdenv.mkDerivation {
    pname = "kani";
    version = kaniVersion;
    src = kaniSrc;

    inherit cargoDeps;

    nativeBuildInputs = [
      kaniRustPlatform.cargoSetupHook
      kaniNightly
      pkgs.makeWrapper
      # build-std links the rebuilt std; needs a real `cc`.
      pkgs.stdenv.cc
    ];

    # kani-compiler/build.rs unconditionally reads RUSTUP_HOME and
    # RUSTUP_TOOLCHAIN (`env::var().unwrap()` — panics if either is unset)
    # and emits two `-Wl,-rpath` link args for kani-compiler:
    #
    #   1. `$RUSTUP_HOME/toolchains/$RUSTUP_TOOLCHAIN/lib`
    #   2. `$ORIGIN/../toolchain/lib`
    #
    # Entry (1) is dead under nix: `PathBuf` does NOT normalize `..`, and
    # the kernel will not walk `..` through the nonexistent
    # `${kaniNightly}/toolchains/` directory — so the literal string
    # `${kaniNightly}/toolchains/../lib` never resolves at build OR run
    # time. The values below only need to be non-empty to satisfy
    # build.rs; they otherwise produce a harmless dead rpath entry.
    #
    # What actually finds librustc_driver.so at run time:
    #   - rpath entry (2), paired with the `$out/toolchain ->
    #     ${kaniNightly}` symlink installed in installPhase. That symlink
    #     IS load-bearing — remove it and kani-compiler can't load.
    #   - `${kaniNightly}/lib/rustlib/x86_64-unknown-linux-gnu/lib`,
    #     which rustc auto-appends to the rpath because kani-compiler
    #     does `extern crate rustc_driver`.
    #
    # At build time (build-kani re-execs the freshly built
    # `target/kani/bin/kani-compiler` to assemble the sysroot) the binary
    # isn't installed yet, so neither rpath entry resolves; the loader
    # finds librustc_driver.so via the LD_LIBRARY_PATH that `cargo run`
    # sets implicitly. No patchelf needed at any stage.
    RUSTUP_HOME = "${kaniNightly}";
    RUSTUP_TOOLCHAIN = "..";

    # `cargo build-dev` (the alias in .cargo/config.toml the Kani docs
    # describe) is `cargo run --target-dir target/tools -p build-kani --
    # build-dev`. The build-kani orchestrator then:
    #   1. cargo build --bins -Z unstable-options --artifact-dir target/kani/bin
    #   2. for each of {no_core, verification, playback}:
    #        cargo build -Z build-std=… --target-dir target/build-libs
    #          -p std -p kani -p kani_macros  with RUSTC=target/kani/bin/kani-compiler
    # cargoSetupHook's vendoring keeps both nested invocations offline.
    buildPhase = ''
      runHook preBuild
      cargo run --offline --target-dir target/tools -p build-kani -- build-dev
      runHook postBuild
    '';

    # No useful tests at this layer; `kani` self-tests live in compiletest.
    doCheck = false;

    # Assemble the InstallType::Release layout (bin/ at $out, not
    # $out/target/kani/bin). kani-driver classifies an install by whether
    # bin_folder() ends with `target/kani/bin` (DevRepo) or `bin` (Release);
    # only Release is path-pure in the nix sandbox.
    installPhase = ''
      runHook preInstall
      mkdir -p $out

      # build-dev assembled bin/ + the three lib trees in target/kani/.
      cp -a target/kani/bin $out/bin
      for d in lib playback no_core; do
        [ -d "target/kani/$d" ] && cp -a "target/kani/$d" "$out/$d"
      done

      # Drop the kani-verifier proxy bins (they download a release
      # bundle on first run — pure-network, useless in nix). The `kani`
      # and `cargo-kani` entry points are makeWrapper-generated in
      # postFixup with explicit --argv0 so kani-driver's
      # determine_invocation_type() dispatch works.
      rm -f $out/bin/kani $out/bin/cargo-kani

      # kani-driver re-reads kani_lib.c and friends from $out/library/.
      cp -a library $out/library

      # InstallType::Release expects $out/toolchain/bin/cargo for nested
      # `cargo kani` invocations.
      ln -s ${kaniNightly} $out/toolchain

      # setup.rs metadata files; the version check reads both.
      echo "nightly-${kaniNightlyDate}" > $out/rust-toolchain-version
      ${kaniNightly}/bin/rustc --version > $out/rustc-version

      runHook postInstall
    '';

    # CBMC + Kissat + a C compiler live in nixpkgs, not bundled. cbmc is
    # invoked as `Command::new("cbmc")`, goto-cc as `Command::new("goto-cc")`,
    # etc. — discovered via PATH. See `kaniRuntimePath` above for the full
    # set the wrappers carry.
    #
    # kani-driver dispatches CargoKani vs Standalone on argv[0]. A shebang
    # exec doesn't lose it — the kernel passes the original execve(2)
    # pathname as the interpreter's argv[1] (the wrapper script's `$0`).
    # The loss is in the wrapper's own `exec`: makeWrapper writes a bare
    # `exec "$real" "$@"` (no `-a`) unless given `--argv0`, so the wrapped
    # binary's argv[0] is the resolved $real path, not the name the caller
    # used. A naive chain `kani → kani-driver → .kani-driver-wrapped`
    # would therefore land argv[0] at `.kani-driver-wrapped`, falling into
    # the Standalone fallback — `cargo kani` silently misparses its args.
    # Avoid the chain: hide the real binary and emit one explicit
    # `--argv0` wrapper per entry point (makeWrapper then writes
    # `exec -a "$entry" …`).
    postFixup = ''
      mv $out/bin/kani-driver $out/bin/.kani-driver-real
      for entry in kani-driver kani; do
        makeWrapper $out/bin/.kani-driver-real $out/bin/$entry \
          --argv0 "$entry" \
          --prefix PATH : ${kaniRuntimePath}
      done
      # cargo-kani additionally needs a clean cargo environment. The dev
      # shell exports CARGO_BUILD_BUILD_DIR to a shared inter-worktree
      # build cache (nix/devshell.nix shellHook —
      # project_shared_build_dir_contamination policy), but kani-driver
      # passes its own --target-dir to the cargo it spawns and then
      # globs that dir for *.kani-metadata.json + goto binaries. With
      # both set, cargo splits intermediates to the shared build dir and
      # kani-driver can't find them — `cargo kani` fails with `No such
      # file or directory` inside `nix develop`. Unset is scoped to this
      # wrapper; everything else in the dev shell keeps the shared
      # cache. The standalone `kani` and `kani-driver` entries don't go
      # through cargo, so they don't need it.
      makeWrapper $out/bin/.kani-driver-real $out/bin/cargo-kani \
        --argv0 cargo-kani \
        --prefix PATH : ${kaniRuntimePath} \
        --unset CARGO_BUILD_BUILD_DIR
    '';

    passthru = {
      inherit kaniNightly cbmc kissat;
    };

    meta = {
      description = "Bit-precise model checker for Rust (Kani Rust Verifier)";
      homepage = "https://model-checking.github.io/kani/";
      license = with lib.licenses; [
        mit
        asl20
      ];
      platforms = [ "x86_64-linux" ];
      mainProgram = "kani";
    };
  };
in
{
  # Raw build output. `nix build .#kani-toolchain` → result/bin/{kani,
  # cargo-kani,kani-driver,kani-compiler}, result/lib (verify sysroot),
  # result/toolchain (nightly).
  kani = kaniBuild;

  # Stable alias for the wrapper-aware bins (cargo-kani/kani get a
  # PATH+CBMC env wrapper in postFixup). Today this is the same
  # derivation as `kani`; the alias exists so the wrapping can move out
  # of kaniBuild without touching consumers. Currently unconsumed —
  # nix/kani.nix and the devshell both reference `kaniToolchain.kani`
  # directly. Forward export only.
  kani-driver-wrapped = kaniBuild;

  # The "rust toolchain" a future crateBuildKani tree passes to
  # mkCrateBuild as `rust`. buildRustCrate calls `${rust}/bin/rustc` —
  # this makes that be kani-compiler. The kani sysroot and -L/--extern
  # flags are passed via globalExtraRustcOpts at the call site, not here.
  kani-rustc = pkgs.runCommand "kani-rustc-${kaniVersion}" { } ''
    mkdir -p $out/bin
    # A bare symlink is enough — no LD_LIBRARY_PATH wrapper. The dynamic
    # loader resolves '$ORIGIN' against the *readlink-resolved* binary, not
    # the symlink invoked: even when run as $out/bin/rustc, '$ORIGIN' is
    # ${kaniBuild}/bin, so the build.rs rpath '$ORIGIN/../toolchain/lib'
    # still hits ${kaniBuild}/toolchain/lib (the symlink to the nightly
    # installed in installPhase) and finds librustc_driver.so.
    ln -s ${kaniBuild}/bin/kani-compiler $out/bin/rustc
    ln -s ${kaniNightly}/bin/cargo $out/bin/cargo
  '';

  # The kani verify-sysroot dir: libkani.rlib, the always-encode-mir
  # std, and rustlib/<target>/lib/. This is the path `kaniBaseFlags`
  # passes as `-L` (the `--sysroot` arg there is the install root,
  # `${kaniToolchain.kani}`, not this dir). Currently unconsumed —
  # flake.nix interpolates `${kaniToolchain.kani}/lib` inline. Forward
  # export only.
  kani-sysroot = "${kaniBuild}/lib";

  # The pinned nightly toolchain kani-compiler links against. NOT
  # rio-build's rustStable or rustNightly (different pin). Currently
  # unconsumed — crateBuildKani passes `kani-rustc` (the kani-compiler
  # symlink shim) as its `rust`, not the bare nightly. Forward export
  # only.
  inherit kaniNightly;
}
