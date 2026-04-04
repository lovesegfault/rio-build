{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.34.2";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };

    # Multi-version Nix compat matrix inputs (weekly tier — NOT in .#ci).
    # See nix/golden-matrix.nix + docs/src/verification.md § Protocol
    # Conformance. Each input provides a nix-daemon binary; the golden
    # conformance suite runs once per daemon to surface protocol-version
    # divergences early.
    #
    # No `nixpkgs.follows` — following our nixpkgs breaks both the 2.20
    # and Lix builds (they pin specific nixpkgs revs for their own
    # dependency constraints). Accepting the eval cost is cheap for a
    # weekly-only target; the lock entries are inert until
    # `.#golden-matrix` is built.
    nix-stable = {
      url = "github:NixOS/nix/2.20-maintenance";
      # 2.20's flake predates the flake-parts split — minimal follows.
      # Branch-deletion is survivable: the lockfile pins the rev, so
      # only explicit `nix flake update nix-stable` breaks if upstream
      # deletes the branch (and nixpkgs caches tarballs).
      inputs.flake-compat.follows = "flake-compat";
    };
    nix-unstable = {
      url = "github:NixOS/nix";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };
    lix = {
      url = "git+https://git.lix.systems/lix-project/lix";
      inputs.flake-compat.follows = "flake-compat";
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    # Spec-coverage tool (nix/tracey.nix). Flake input (not fetchFromGitHub)
    # so importCargoLock reads Cargo.lock from a pre-fetched path — no IFD.
    tracey-src = {
      url = "github:bearcove/tracey";
      flake = false;
    };

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # RustSec advisory DB for cargo-deny (hermetic — no network at
    # build time). Bump via `nix flake update advisory-db` to pick up
    # new advisories.
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    # Per-crate Nix builds (evaluation PoC — see
    # .claude/notes/crate2nix-migration-assessment.md). Pinned to master
    # for the experimental JSON output (Cargo.json + lib/build-from-json.nix:
    # feature resolution in Rust, no 6k+ line Cargo.nix checked in).
    # PR #453 added native devDependencies to the JSON output, so no
    # post-processing is needed for test builds.
    #
    # We consume two surfaces:
    #   - `lib/build-from-json.nix` as a source file (no inputs needed)
    #   - The CLI binary for `crate2nix generate --format json`
    #
    # Everything else in crate2nix's flake (devshell, cachix,
    # pre-commit-hooks, nix-test-runner, crate2nix_stable bootstrap)
    # is upstream dev tooling. Their flake-parts wiring imports
    # `inputs.devshell.flakeModule` unconditionally at the top level —
    # eager module eval means `follows = ""` on devshell breaks
    # `packages.default` even though the CLI build itself doesn't
    # touch devshell.
    #
    # `flake = false` sidesteps the whole thing: zero transitive
    # inputs in flake.lock. The CLI is built via the callPackage-
    # compatible `crate2nix/default.nix` entrypoint (checked-in
    # Cargo.nix + nixpkgs' buildRustCrate; same machinery as
    # crate2nix's own bootstrap). See `crate2nixCli` in the
    # perSystem let-block.
    crate2nix = {
      url = "github:nix-community/crate2nix";
      flake = false;
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-compat.follows = "flake-compat";
    };

    # Self-hosted binary cache push CLI. Pinned in flake.lock;
    # included in githubActions.build so the first CI job pushes
    # it to rio-nix-cache → subsequent jobs substitute from S3
    # in-region (fast, no curl/GitHub-cache round-trip).
    niks3 = {
      url = "github:Mic92/niks3/v1.4.0";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-parts.follows = "flake-parts";
        treefmt-nix.follows = "treefmt-nix";
        # process-compose isn't a dep of the package build path;
        # leave it unfollowed rather than polluting our inputs.
      };
    };

    # Helm charts as Nix derivations (FODs — hash-pinned, cached). The
    # bitnami PG subchart + rook-ceph operator + cluster charts come from
    # here. Alternative was vendoring .tgz into git (ugly) or hand-rolling
    # a `helm pull` FOD (nixhelm already did that work). Only the
    # chartsDerivations output is used; nixhelm's transitive inputs
    # (pyproject-nix etc) are unused but pulled into flake.lock — cost of
    # one flake input.
    nixhelm = {
      url = "github:farcaller/nixhelm";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks-nix.flakeModule
      ];

      # NixOS modules for deploying rio services. These are consumed by the
      # standalone-fixture VM tests (nix/tests/fixtures/standalone.nix) and
      # can be reused
      # for real deployments. Each module reads `services.rio.package` for
      # binaries, so callers must set that to a workspace build.
      flake.nixosModules = {
        store = ./nix/modules/store.nix;
        scheduler = ./nix/modules/scheduler.nix;
        gateway = ./nix/modules/gateway.nix;
        worker = ./nix/modules/builder.nix;
      };

      # CI integration — see the perSystem githubActions definition.
      # Linux-only CI runners, so hardcode x86_64-linux.
      flake.githubActions = inputs.self.legacyPackages.x86_64-linux.githubActions;

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          # Read version from Cargo.toml
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          inherit (cargoToml.workspace.package) version;

          # --------------------------------------------------------------
          # Rust toolchains
          # --------------------------------------------------------------
          #
          # Stable: single source of truth for CI (clippy, nextest,
          # workspace build, coverage, docs). Read from
          # rust-toolchain.toml so `rustup` users and Nix users agree.
          # Guarantees releases are stable-compatible.
          rustStable = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # Nightly: used by the default dev shell and fuzz builds.
          # selectLatestNightlyWith auto-picks the most recent nightly
          # that has all requested components, so we're never blocked on
          # a bad nightly.
          #
          # NOTE: non-hermetic by design — bumping rust-overlay changes
          # the nightly date and invalidates the fuzz-build cache. If
          # this becomes a problem, pin to rust-bin.nightly."YYYY-MM-DD".
          rustNightly = pkgs.rust-bin.selectLatestNightlyWith (
            toolchain:
            toolchain.default.override {
              extensions = [
                "rust-src"
                "llvm-tools-preview"
                "rustfmt"
                "clippy"
                "rust-analyzer"
              ];
            }
          );

          # nixpkgs rustPlatform wired to our rust-overlay toolchains.
          # Stable: used by nix/tracey.nix (external tool, edition-2024
          # capable toolchain). Nightly: used by nix/fuzz.nix
          # (libfuzzer-sys needs -Zsanitizer=address).
          rustPlatformStable = pkgs.makeRustPlatform {
            rustc = rustStable;
            cargo = rustStable;
          };
          rustPlatformNightly = pkgs.makeRustPlatform {
            rustc = rustNightly;
            cargo = rustNightly;
          };

          # Source root for filesets
          unfilteredRoot = ./.;

          # Shared fileset for the crate2nix workspaceSrc. buildRustCrate
          # works on per-crate directories, so each workspace member's
          # subtree must be included verbatim (no commonCargoSources
          # filter — that strips proto/, which rio-proto's build.rs
          # needs as ./proto/).
          workspaceFileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./rio-bench
            ./rio-cli
            ./rio-common
            ./rio-controller
            ./rio-crds
            ./rio-gateway
            ./rio-nix/src
            ./rio-nix/Cargo.toml
            ./rio-nix/proptest-regressions
            ./rio-proto
            ./rio-scheduler
            ./rio-store/src
            ./rio-store/tests
            ./rio-store/Cargo.toml
            ./rio-store/build.rs
            ./rio-test-support
            ./rio-builder
            ./xtask
            ./workspace-hack
            ./migrations
            # sqlx offline query cache — content-addressed JSON per
            # query!(...) callsite. Generated by `cargo xtask regen sqlx`.
            # Required at compile time when SQLX_OFFLINE=1 so the macro
            # doesn't try to connect to PG during the crate2nix build.
            # maybeMissing: resilience against accidental `.sqlx/`
            # deletion during dev — `cargo sqlx prepare` regenerates.
            # (.sqlx/*.json is committed, so a fresh clone WILL have it.)
            (pkgs.lib.fileset.maybeMissing ./.sqlx)
            # Seccomp profile JSON (embedded via include_str! in
            # rio-controller tests — build-time presence check so a
            # missing profile fails compile, not silently at deploy).
            ./infra/helm/rio-build/files
            # observability.md — build.rs greps the per-component
            # metrics tables to derive SPEC_METRICS for the
            # spec→describe check (see rio-test-support
            # emit_spec_metrics_grep). Adding a row must break
            # the nextest drv hash so drift is caught.
            ./docs/src/observability.md
          ];
          workspaceSrc = pkgs.lib.fileset.toSource {
            root = unfilteredRoot;
            fileset = workspaceFileset;
          };

          # Prefix every key in an attrset. Used to surface per-member
          # derivations under flake packages.
          prefixed = p: pkgs.lib.mapAttrs' (n: v: pkgs.lib.nameValuePair "${p}${n}" v);

          # ──────────────────────────────────────────────────────────────
          # sys-crate linkage: per-crate single source of truth
          # ──────────────────────────────────────────────────────────────
          #
          # Each sys-crate that system-links instead of vendoring C gets
          # its env-var escape hatch + system lib here. Per-crate shape
          # so crate2nix crateOverrides can reference .crates.<name>
          # directly; devShell consumes the derived .allEnv/.allLibs
          # aggregates.
          #
          # Adding a sys-crate: add a .crates.<name> entry here, add the
          # override in nix/crate2nix.nix referencing it, done.
          sysCrateEnv =
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
            };

          # Workspace binaries (crate2nix per-crate build, stripped in
          # nix/crate2nix.nix). What docker images, VM tests,
          # worker-vm, and crdgen consume.
          rio-workspace = crateBuild.workspaceBins;

          # Coverage-instrumented workspace. crate2nix parallel tree
          # with globalExtraRustcOpts=["-Cinstrument-coverage"]. Used
          # by vmTestsCov + nix/coverage.nix. NOT stripped (stripping
          # removes the __llvm_covfun/__llvm_covmap sections llvm-cov
          # needs). remap-path-prefix at compile time collapses the
          # closure to glibc+syslibs — fits k3s containerd tmpfs.
          rio-workspace-cov = crateBuildCov.workspaceBinsCov;

          # Source tree for genhtml (nix/coverage.nix cd's here so
          # genhtml can resolve repo-relative lcov paths to source
          # lines). workspaceSrc has all rio-*/src/.
          commonSrc = workspaceSrc;

          # --------------------------------------------------------------
          # Fuzz build pipeline (extracted to nix/fuzz.nix)
          # --------------------------------------------------------------
          #
          # Produces:
          #   fuzz.builds.rio-{nix,store}-fuzz-build  — compiled target binaries
          #   fuzz.runs   — 2min checks, keyed fuzz-<target>
          fuzz = import ./nix/fuzz.nix {
            inherit
              pkgs
              rustNightly
              rustPlatformNightly
              unfilteredRoot
              workspaceFileset
              ;
          };

          # Spec-coverage CLI + web dashboard. The SPA is built via
          # fetchPnpmDeps in nix/tracey.nix and embedded at compile time.
          traceyPkg = import ./nix/tracey.nix {
            inherit pkgs;
            rustPlatform = rustPlatformStable;
            inherit (inputs) tracey-src;
          };

          # crate2nix CLI built from source against OUR nixpkgs.
          # inputs.crate2nix is `flake = false` (bare source tree) so
          # its 8 transitive flake inputs (devshell, cachix,
          # pre-commit-hooks, nix-test-runner, crate2nix_stable, …)
          # don't bloat flake.lock. `crate2nix/default.nix` is the
          # callPackage-compatible entrypoint — same one upstream's
          # bootstrap uses — reads the checked-in Cargo.nix and
          # builds via pkgs.buildRustCrate.
          #
          # The only nixpkgs-version risk here is `callPackage
          # Cargo.nix` — if upstream's Cargo.nix template references
          # a buildRustCrate attr our nixpkgs lacks, the CLI build
          # fails. In practice the template surface is stable (the
          # template itself is what crate2nix generates for every
          # user, so it's tested against a wide nixpkgs range). If
          # this does break on a nixpkgs bump: pin
          # `inputs.crate2nix-nixpkgs` separately and pass that
          # through as `pkgs` here.
          crate2nixCli = pkgs.callPackage "${inputs.crate2nix}/crate2nix/default.nix" { };

          # ──────────────────────────────────────────────────────────
          # crate2nix JSON-mode build
          # ──────────────────────────────────────────────────────────
          #
          # Per-crate build pipeline using pkgs.buildRustCrate + a
          # pre-resolved Cargo.json. See nix/crate2nix.nix and
          # .claude/notes/crate2nix-migration-assessment.md for the
          # rationale and caveats. Exposed below as
          # packages.workspace + packages.rio-<crate>.
          mkCrateBuild =
            extra:
            import ./nix/crate2nix.nix (
              {
                inherit
                  pkgs
                  rustStable
                  sysCrateEnv
                  workspaceSrc
                  ;
                inherit (pkgs) lib;
                crate2nixSrc = inputs.crate2nix;
              }
              // extra
            );
          crateBuild = mkCrateBuild { };

          # Coverage-instrumented tree: re-import with
          # globalExtraRustcOpts=["-Cinstrument-coverage"]. Doubles the
          # derivation count (645 normal + 645 instrumented), but each
          # half caches independently — touching a workspace crate only
          # rebuilds that crate's two variants + dependents.
          crateBuildCov = mkCrateBuild { globalExtraRustcOpts = [ "-Cinstrument-coverage" ]; };

          # ──────────────────────────────────────────────────────────
          # crate2nix check backends: clippy, tests, doc
          # ──────────────────────────────────────────────────────────
          #
          # Per-crate checks layered on the crate2nix build graph.
          # Deps are built once (regular rustc, 645 cached drvs);
          # workspace members are rebuilt per-check with the
          # appropriate driver (clippy-driver, rustc --test, rustdoc).
          # See nix/checks.nix for the wrapper mechanics — notably the
          # clippy wrapper strips lib.sh's hardcoded `--cap-lints
          # allow` (which rustc treats as non-overridable) before
          # forwarding to clippy-driver.
          #
          # Each workspace member gets its own check derivation →
          # touching rio-scheduler only re-clippy's rio-scheduler +
          # its dependents, not the full workspace.
          #
          # Exposed below as checks.* and packages.clippy-* / test-* /
          # doc-* for targeted invocation.
          crateChecks = import ./nix/checks.nix {
            inherit
              pkgs
              rustStable
              crateBuild
              crateBuildCov
              ;
            inherit (pkgs) lib;
            # Runtime inputs for test execution. Mirrors crane's
            # cargoNextest nativeCheckInputs — postgres for ephemeral
            # PG bootstrap (rio-test-support), nix-cli for golden
            # conformance tests (nix-store --dump, nix-instantiate),
            # openssh for rio-gateway SSH accept tests.
            runtimeTestInputs = with pkgs; [
              inputs.nix.packages.${system}.nix
              openssh
              postgresql_18
            ];
            # Env vars for test runners. PG_BIN so rio-test-support
            # finds initdb/postgres; RIO_GOLDEN_* so golden tests
            # don't try to `nix build` their fixture in-sandbox.
            testEnv = goldenTestEnv // {
              PG_BIN = "${pkgs.postgresql_18}/bin";
            };
            # nextest reuse-build runner. Synthesizes --cargo-metadata
            # and --binaries-metadata JSON from the crate2nix test
            # binaries; runs with the `ci` profile (retries, test
            # groups from .config/nextest.toml). Per-test-process
            # isolation — no PDEATHSIG/libtest thread race, so
            # wrapper-level PG bootstrap not needed. `--no-tests=warn`
            # because rio-cli has zero tests (bin-only crate).
            #
            # Fileset = workspaceSrc PLUS .config/nextest.toml
            # (--workspace-remap needs to find it relative to the
            # workspace root). The base fileset omits .config/
            # because buildRustCrate doesn't need it.
            workspaceSrc = pkgs.lib.fileset.toSource {
              root = unfilteredRoot;
              fileset = pkgs.lib.fileset.unions [
                workspaceFileset
                ./.config/nextest.toml
              ];
            };
            nextestExtraArgs = [
              "--profile"
              "ci"
              "--no-tests=warn"
            ];
          };

          # rio-dashboard Svelte SPA (lint + test + svelte-check + vite build
          # in sandbox). src is scoped to rio-dashboard/ — Rust changes don't
          # invalidate this drv.
          rioDashboard = import ./nix/dashboard.nix { inherit pkgs; };

          # --------------------------------------------------------------
          # Golden conformance test fixtures
          # --------------------------------------------------------------
          #
          # Precomputed store paths for live-daemon golden tests. In hermetic
          # remote build sandboxes, `nix eval`/`nix build` fail because
          # /nix/var is read-only. Building these as nativeCheckInputs makes
          # them available in the sandbox store; env vars tell the tests
          # where they are. Tests compute narHash/narSize themselves via
          # `nix-store --dump` (legacy, no state dir needed). Locally, tests
          # fall back to `nix eval` if the env var is unset.
          goldenTestPath = pkgs.writeText "rio-golden-test" "golden test data\n";

          # CA-path fixture: fixed-output derivation with a known flat hash.
          # FODs don't need the ca-derivations experimental feature, so this
          # builds on any Nix. Its ca field (`fixed:sha256:...`) is what the
          # query_path_from_hash_part_ca test validates.
          # Hash is sha256("ca-golden-test-data") in SRI format.
          goldenCaPath = pkgs.runCommand "rio-ca-golden" {
            outputHashMode = "flat";
            outputHashAlgo = "sha256";
            outputHash = "sha256-ZofhPTz/XO99Dn3kQMcBaG3vHoMFiD9kHTTtuvf2KNM=";
          } "echo -n ca-golden-test-data > $out";

          # Golden-test env vars — shared by nextest check, mutants,
          # and golden-matrix. Adding a new golden-fixture env var
          # here propagates to all runners. (Previously duplicated at
          # each site; a new var would be easy to add to one and
          # forget the rest.)
          goldenTestEnv = {
            RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
            RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
            RIO_GOLDEN_FORCE_HERMETIC = "1";
          };

          # --------------------------------------------------------------
          # Non-rustc check derivations (shared by checks.* and ci aggregate)
          # --------------------------------------------------------------
          #
          # Rust checks (clippy/nextest/doc/coverage) are in crateChecks
          # (per-crate caching, deps built once). These are the rest:
          # workspace-level policy checks that don't invoke rustc.
          miscChecks = {
            # License + advisory audit. Policy: deny GPL-3.0 (project is
            # MIT/Apache), fail on RustSec advisories with a curated
            # ignore list in .cargo/deny.toml. The advisory DB is a
            # flake input (hermetic — no network). Bump via `nix flake
            # update advisory-db` to pick up new advisories.
            #
            # cargo-deny internally runs `cargo metadata` to resolve
            # the full dep tree for license/advisory analysis. That
            # needs vendored sources (cargoSetupHook writes a source-
            # replacement config so cargo finds crates.io deps in the
            # vendored dir instead of the registry index).
            deny = pkgs.stdenv.mkDerivation {
              pname = "rio-deny";
              inherit version;
              src = pkgs.lib.fileset.toSource {
                root = unfilteredRoot;
                fileset = pkgs.lib.fileset.unions [
                  ./.cargo/deny.toml
                  workspaceFileset
                ];
              };
              cargoDeps = rustPlatformStable.importCargoLock {
                lockFile = ./Cargo.lock;
              };
              nativeBuildInputs = with pkgs; [
                cargo-deny
                rustStable
                rustPlatformStable.cargoSetupHook
                git
              ];
              # cargoSetupHook writes .cargo/config.toml with vendored
              # source replacement. cargo metadata reads it; no registry
              # access needed.
              buildPhase = ''
                # HOME defaults to /homeless-shelter (RO). deny.toml's
                # db-path = "~/.cargo/advisory-db" resolves against
                # HOME. cargo-deny expects the DB as a GIT REPO (reads
                # HEAD to determine DB version for the report). The
                # flake input is a plain dir (flake=false strips .git),
                # so we init a throwaway repo with the content.
                export HOME=$TMPDIR
                db=$HOME/.cargo/advisory-db/advisory-db-3157b0e258782691
                mkdir -p "$db"
                cp -r ${inputs.advisory-db}/. "$db"/
                chmod -R u+w "$db"
                git -C "$db" init -q
                git -C "$db" add -A
                git -C "$db" \
                  -c user.name=nix -c user.email=nix@localhost \
                  commit -q -m snapshot
                cargo deny \
                  --manifest-path ./Cargo.toml \
                  --offline \
                  check \
                  --config ./.cargo/deny.toml \
                  --disable-fetch \
                  advisories licenses bans sources \
                  2>&1 | tee deny.out
              '';
              installPhase = ''
                cp deny.out $out
              '';
            };

            # Spec-coverage validation: fails on broken r[...]
            # references, duplicate requirement IDs, or unparseable
            # include files. Does NOT fail on uncovered/untested — those
            # are informational.
            #
            # Uses cleanSource because tracey needs docs/**/*.md and
            # .config/tracey/config.styx. tracey's daemon writes
            # .tracey/daemon.sock under the working dir, so we cp to a
            # writable tmpdir first.
            #
            # .claude/ is excluded via fileset.difference — tracey's config
            # doesn't scan it, so including it in the drv src causes spurious
            # rebuilds on every agent-file edit. Exclusion makes the
            # clause-4(a) fast-path premise ("`.claude/`-only edits are
            # hash-identical to `.#ci`") TRUE rather than merely
            # behavioral-identity. Saves one rebuild per `.claude/` commit.
            tracey-validate =
              pkgs.runCommand "rio-tracey-validate"
                {
                  src = pkgs.lib.fileset.toSource {
                    root = ./.;
                    fileset = pkgs.lib.fileset.difference (pkgs.lib.fileset.fromSource (pkgs.lib.cleanSource ./.)) ./.claude;
                  };
                  nativeBuildInputs = [ traceyPkg ];
                }
                ''
                  cp -r $src $TMPDIR/work
                  chmod -R +w $TMPDIR/work
                  cd $TMPDIR/work
                  rm -rf .tracey/
                  export HOME=$TMPDIR
                  set -o pipefail
                  # Retry once: `tracey query validate` auto-starts a daemon
                  # and waits 5s for the socket. Under sandbox parallel-build
                  # load the socket-wait races ("Daemon failed to start within
                  # 5s" / "Error getting status: Cancelled"). tracey 1.3.0 has
                  # no --no-daemon mode and no TRACEY_DAEMON_TIMEOUT knob, so
                  # retry-once is the minimal fix. P0490.
                  tracey query validate 2>&1 | tee $out || {
                    echo "retry: first tracey attempt failed, retrying once" >&2
                    rm -rf .tracey/  # clear partial daemon state
                    sleep 2
                    tracey query validate 2>&1 | tee $out
                  }
                '';

            # Helm chart lint + template for all value profiles. Catches
            # Go-template syntax errors, missing required values, bad
            # YAML in rendered output. Subcharts symlinked from nixhelm
            # (FOD) — `helm dependency build` needs network.
            helm-lint =
              let
                chart = pkgs.lib.cleanSource ./infra/helm/rio-build;
              in
              pkgs.runCommand "rio-helm-lint"
                {
                  nativeBuildInputs = [
                    pkgs.kubernetes-helm
                    pkgs.yq-go
                    pkgs.jq
                  ];
                }
                ''
                  cp -r ${chart} $TMPDIR/chart
                  chmod -R +w $TMPDIR/chart
                  cd $TMPDIR/chart
                  mkdir -p charts
                  ln -s ${subcharts.postgresql} charts/postgresql
                  ln -s ${subcharts.rustfs} charts/rustfs
                  helm lint .
                  # Default (prod) profile: tag must be set (empty → bad image ref).
                  helm template rio . --set global.image.tag=test > /tmp/default.yaml
                  helm template rio . -f values/dev.yaml > /dev/null
                  helm template rio . -f values/kind.yaml > /dev/null
                  helm template rio . -f values/vmtest-full.yaml > /dev/null
                  # monitoring-on: ServiceMonitor/PodMonitor/PrometheusRule
                  # templates are gated and otherwise never rendered by CI.
                  helm template rio . --set global.image.tag=test \
                    --set monitoring.enabled=true > /tmp/monitoring.yaml
                  for k in ServiceMonitor PodMonitor PrometheusRule; do
                    grep -qx "kind: $k" /tmp/monitoring.yaml || {
                      echo "FAIL: monitoring.enabled=true did not render kind: $k" >&2
                      exit 1
                    }
                  done

                  # dash-on: all CRD kinds + third-party images present. Rendered
                  # here (before the digest-pin loop below and the Gateway API
                  # CRD checks after) so one render feeds both.
                  helm template rio . \
                    --set dashboard.enabled=true \
                    --set global.image.tag=test \
                    --set postgresql.enabled=false \
                    > /tmp/dash-on.yaml

                  # ── Third-party image digest-pin enforcement ────────────────
                  # Every image that isn't a rio-build image (those get `:test`
                  # from --set global.image.tag=test above) MUST be digest-
                  # pinned. A floating third-party tag that doesn't exist /
                  # gets deleted / gets overwritten upstream → ImagePullBackOff
                  # → component-specific silent brick:
                  #   - devicePlugin: smarter-devices/fuse never registers →
                  #     worker pods Pending (a prior default pointed at a
                  #     never-published upstream tag; r[sec.pod.fuse-device-plugin])
                  #   - envoyImage: gRPC-Web translation dead → dashboard loads
                  #     but every RPC fails (r[dash.envoy.grpc-web-translate])
                  #   - <future>: same failure mode, this loop catches it pre-merge
                  #
                  # yq drills into every container spec (DaemonSet + Deployment
                  # + StatefulSet + Job) plus the EnvoyProxy CRD's image path
                  # (different shape — .spec.provider.kubernetes.envoyDeployment
                  # .container.image, not .spec.template.spec.containers[]).
                  # Runs over BOTH default.yaml (prod profile) and dash-on.yaml
                  # (dashboard-enabled superset). Filters out :test-tagged rio
                  # images and fails on any remaining bare-tag image. @sha256:
                  # is the pin marker. Subchart images (bitnami/ PG) are a
                  # separate supply-chain boundary — postgresql.enabled=false
                  # in both renders so they don't appear here.
                  thirdparty=$(yq eval-all '
                    ( select(.kind=="DaemonSet" or .kind=="Deployment"
                             or .kind=="StatefulSet" or .kind=="Job")
                      | .spec.template.spec.containers[].image ),
                    ( select(.kind=="EnvoyProxy")
                      | .spec.provider.kubernetes.envoyDeployment.container.image )
                  ' /tmp/default.yaml /tmp/dash-on.yaml \
                    | grep -v ':test$' \
                    | grep -v '^---$' | grep -v '^null$' \
                    | sort -u)
                  echo "third-party images in default+dash-on renders:" >&2
                  echo "$thirdparty" >&2
                  bad=$(echo "$thirdparty" | grep -v '@sha256:' || true)
                  if [ -n "$bad" ]; then
                    echo "FAIL: third-party image(s) not digest-pinned:" >&2
                    echo "$bad" >&2
                    exit 1
                  fi

                  # ── dashboard Gateway API CRDs ─────────────────────────────
                  # dashboard.enabled=true MUST render exactly one each of
                  # GatewayClass/Gateway/GRPCRoute/EnvoyProxy/SecurityPolicy/
                  # ClientTrafficPolicy (+ BackendTLSPolicy when tls.enabled).
                  # Any Go-template syntax error, bad nindent, or Values typo
                  # surfaces here before the VM test has to spend 5min on k3s
                  # bring-up to discover a YAML parse error. (/tmp/dash-on.yaml
                  # rendered above, before the third-party image loop.)
                  for k in GatewayClass Gateway GRPCRoute EnvoyProxy \
                           SecurityPolicy ClientTrafficPolicy BackendTLSPolicy; do
                    grep -qx "kind: $k" /tmp/dash-on.yaml || {
                      echo "FAIL: dashboard.enabled=true did not render kind: $k" >&2
                      exit 1
                    }
                  done
                  # grpc_web filter auto-inject is a runtime property of Envoy
                  # Gateway's xDS translator, not something helm-lint can prove
                  # — the GRPCRoute existence + Gateway single-listener is the
                  # static contract.
                  n=$(yq 'select(.kind=="Gateway" and .metadata.name=="rio-dashboard")
                          | .spec.listeners | length' /tmp/dash-on.yaml)
                  test "$n" -eq 1 || {
                    echo "FAIL: rio-dashboard Gateway must have exactly 1 listener (dodges #7559), got $n" >&2
                    exit 1
                  }

                  # ── r[dash.auth.method-gate] fail-closed proof ─────────────
                  # Default values (enableMutatingMethods=false) MUST NOT render
                  # the mutating GRPCRoute. If this assert fails, a values.yaml
                  # typo (or a template guard regression) has fail-OPENED
                  # ClearPoison/DrainWorker/CreateTenant/TriggerGC to any
                  # browser that can reach the gateway.
                  #
                  # `grep -x >/dev/null`, NOT `grep -qx`: stdenv runs with
                  # pipefail. grep -q exits on first match → closes pipe → yq's
                  # next write SIGPIPEs → yq exit 141 → pipeline fails → false
                  # FAIL. Go's stdout is unbuffered to a pipe, so each output
                  # line is a separate write() — grep can race-close between
                  # them. ~120 bytes fits the 64K pipe buffer normally; under
                  # 192-core scheduler contention it doesn't. Observed: same
                  # drv flapped FAIL at different yq sites on consecutive runs.
                  # Dropping -q makes grep drain the pipe; no SIGPIPE.
                  ! yq 'select(.kind=="GRPCRoute") | .metadata.name' /tmp/dash-on.yaml \
                    | grep -x rio-scheduler-mutating >/dev/null || {
                    echo "FAIL: rio-scheduler-mutating GRPCRoute rendered with default values (enableMutatingMethods should default false)" >&2
                    exit 1
                  }
                  # Readonly route MUST render and MUST carry ClusterStatus
                  # (proves the route-split didn't drop the load-bearing
                  # unary-test target — dashboard-gateway.nix curl depends
                  # on ClusterStatus routing).
                  yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly")
                      | .spec.rules[].matches[].method.method' /tmp/dash-on.yaml \
                    | grep -x ClusterStatus >/dev/null || {
                    echo "FAIL: rio-scheduler-readonly missing ClusterStatus match" >&2
                    exit 1
                  }
                  # ClearPoison must NOT leak into the readonly route — a
                  # one-line yaml indent mistake could silently attach it.
                  ! yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly")
                        | .spec.rules[].matches[].method.method' /tmp/dash-on.yaml \
                    | grep -x ClearPoison >/dev/null || {
                    echo "FAIL: ClearPoison leaked into readonly GRPCRoute" >&2
                    exit 1
                  }
                  # CORS allowOrigins MUST NOT be wildcard by default. The
                  # earlier MVP had "*" — regression guard. yq-go `select()`
                  # doesn't short-circuit like jq; pipe to grep -x instead.
                  ! yq 'select(.kind=="SecurityPolicy" and .metadata.name=="rio-dashboard-cors")
                        | .spec.cors.allowOrigins[]' /tmp/dash-on.yaml \
                    | grep -x '\*' >/dev/null || {
                    echo "FAIL: SecurityPolicy rio-dashboard-cors allowOrigins contains wildcard" >&2
                    exit 1
                  }
                  # Positive: flipping enableMutatingMethods=true DOES render
                  # the mutating route + ClearPoison. Proves the flag is
                  # wired (not a typo'd Values path that evals to nil —
                  # helm treats undefined as false, so a bad path silently
                  # gates forever-off).
                  helm template rio . \
                    --set dashboard.enabled=true \
                    --set dashboard.enableMutatingMethods=true \
                    --set global.image.tag=test \
                    --set postgresql.enabled=false \
                    > /tmp/dash-mut.yaml
                  yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-mutating")
                      | .spec.rules[].matches[].method.method' /tmp/dash-mut.yaml \
                    | grep -x ClearPoison >/dev/null || {
                    echo "FAIL: enableMutatingMethods=true did not render mutating GRPCRoute with ClearPoison" >&2
                    exit 1
                  }

                  # ── JWT mount assertions (r[sec.jwt.pubkey-mount]) ──────────
                  # jwt.enabled=true MUST render the ConfigMap mount in
                  # scheduler+store and the Secret mount in gateway.
                  # Without the mount, RIO_JWT__KEY_PATH stays unset → the
                  # interceptor is inert → silent fail-open (every JWT passes
                  # unverified). The ConfigMap/Secret OBJECTS exist; the MOUNT
                  # was missing. --set-string for the base64 values — trailing
                  # '=' padding is fine (everything after the first '=' is the
                  # value); --set would try to parse it as YAML.
                  helm template rio . \
                    --set jwt.enabled=true \
                    --set-string jwt.publicKey=dGVzdA== \
                    --set-string jwt.signingSeed=dGVzdA== \
                    --set global.image.tag=test \
                    --set postgresql.enabled=false \
                    > $TMPDIR/jwt-on.yaml

                  # 3 = scheduler + store + gateway. Each gets exactly one
                  # RIO_JWT__KEY_PATH env var. >3 would mean a template got
                  # included twice (leaky nindent loop); <3 = missing include.
                  n=$(grep -c RIO_JWT__KEY_PATH $TMPDIR/jwt-on.yaml)
                  test "$n" -eq 3 || {
                    echo "FAIL: expected 3 RIO_JWT__KEY_PATH (sched+store+gw), got $n" >&2
                    exit 1
                  }

                  # yq: structural asserts. grep would match the ConfigMap
                  # resource's own `name: rio-jwt-pubkey` — yq drills into
                  # the Deployment's pod spec so we're asserting the MOUNT,
                  # not the object existing.
                  for dep in rio-scheduler rio-store; do
                    # volumes: entry — configMap ref to rio-jwt-pubkey.
                    yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
                        | .spec.template.spec.volumes[]
                        | select(.name==\"jwt-pubkey\")
                        | .configMap.name" $TMPDIR/jwt-on.yaml \
                      | grep -x rio-jwt-pubkey >/dev/null || {
                      echo "FAIL: $dep missing jwt-pubkey configMap volume" >&2
                      exit 1
                    }
                    # volumeMounts: entry — path + readOnly.
                    yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
                        | .spec.template.spec.containers[0].volumeMounts[]
                        | select(.name==\"jwt-pubkey\")
                        | .mountPath" $TMPDIR/jwt-on.yaml \
                      | grep -x /etc/rio/jwt >/dev/null || {
                      echo "FAIL: $dep missing jwt-pubkey volumeMount at /etc/rio/jwt" >&2
                      exit 1
                    }
                    # env: RIO_JWT__KEY_PATH points at the file the Rust side reads.
                    yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
                        | .spec.template.spec.containers[0].env[]
                        | select(.name==\"RIO_JWT__KEY_PATH\")
                        | .value" $TMPDIR/jwt-on.yaml \
                      | grep -x /etc/rio/jwt/ed25519_pubkey >/dev/null || {
                      echo "FAIL: $dep RIO_JWT__KEY_PATH != /etc/rio/jwt/ed25519_pubkey" >&2
                      exit 1
                    }
                  done

                  # Gateway: Secret mount (signing side).
                  yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
                      | .spec.template.spec.volumes[]
                      | select(.name=="jwt-signing")
                      | .secret.secretName' $TMPDIR/jwt-on.yaml \
                    | grep -x rio-jwt-signing >/dev/null || {
                    echo "FAIL: gateway missing jwt-signing Secret volume" >&2
                    exit 1
                  }
                  yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
                      | .spec.template.spec.containers[0].env[]
                      | select(.name=="RIO_JWT__KEY_PATH")
                      | .value' $TMPDIR/jwt-on.yaml \
                    | grep -x /etc/rio/jwt/ed25519_seed >/dev/null || {
                    echo "FAIL: gateway RIO_JWT__KEY_PATH != /etc/rio/jwt/ed25519_seed" >&2
                    exit 1
                  }

                  # Negative: jwt.enabled=false renders NO mount. The or-gate
                  # in scheduler/store/gateway elides the volumeMounts/volumes
                  # keys when nothing is enabled, and the self-guarded
                  # templates render nothing. "! grep" exits 0 on no-match,
                  # 1 on match → && flips to fail-fast.
                  helm template rio . \
                    --set global.image.tag=test \
                    --set tls.enabled=false \
                    --set postgresql.enabled=false \
                    > $TMPDIR/jwt-off.yaml
                  ! grep -q 'RIO_JWT__KEY_PATH\|jwt-pubkey\|jwt-signing' $TMPDIR/jwt-off.yaml || {
                    echo "FAIL: jwt mount rendered with jwt.enabled=false (default)" >&2
                    grep -n 'RIO_JWT__KEY_PATH\|jwt-pubkey\|jwt-signing' $TMPDIR/jwt-off.yaml >&2
                    exit 1
                  }

                  # ── rio.optBool: with-on-bool footgun guard ─────────────────
                  # Explicit `false` MUST render. Helm's `with` is falsy-skip —
                  # `hostUsers: false` in values produced NO key (controller
                  # default applied instead of the explicit override). hasKey
                  # renders the key whenever it's SET, regardless of value.
                  helm template rio . \
                    --set fetcherPool.enabled=true \
                    --set fetcherPool.hostUsers=false \
                    --set global.image.tag=test \
                    --set postgresql.enabled=false \
                    > $TMPDIR/fp-false.yaml
                  yq 'select(.kind=="FetcherPool") | .spec.hostUsers' $TMPDIR/fp-false.yaml \
                    | grep -x false >/dev/null || {
                    echo "FAIL: fetcherPool.hostUsers=false did not render (with-on-bool bug)" >&2
                    exit 1
                  }
                  # Unset stays unset — no spurious key. --set key=null deletes
                  # the key from the values map (Helm deep-merge semantics), so
                  # hasKey sees it as absent and the template renders nothing.
                  helm template rio . \
                    --set fetcherPool.enabled=true \
                    --set fetcherPool.hostUsers=null \
                    --set global.image.tag=test \
                    --set postgresql.enabled=false \
                    > $TMPDIR/fp-unset.yaml
                  test "$(yq 'select(.kind=="FetcherPool") | .spec | has("hostUsers")' $TMPDIR/fp-unset.yaml)" = false || {
                    echo "FAIL: fetcherPool.hostUsers unset but key rendered (spurious key)" >&2
                    exit 1
                  }

                  # ── r[obs.metric.builder-util] dashboard regex ──────────────
                  # builder-utilization.json's cAdvisor queries select pods by
                  # regex. STS naming is `sts_name()` = `rio-builder-{pool}` →
                  # pods `rio-builder-{pool}-{ordinal}`; ephemeral Jobs use
                  # `rio-builder-{pool}-{6char}` (I-104). Assert the
                  # `-builder-` infix so a future dashboard or controller
                  # rename that desyncs the two fails here, not silently at
                  # "why is this Grafana panel empty".
                  jq -r '.panels[].targets[]?.expr' \
                    ${./infra/helm/grafana/builder-utilization.json} \
                    | grep 'container_cpu_usage\|container_memory' \
                    | grep -- '-builder-' >/dev/null \
                    || { echo "FAIL: builder-utilization.json pod regex doesn't match controller naming (rio-builder-{pool}-{N})" >&2; exit 1; }

                  # ── r[sec.psa.control-plane-restricted] bootstrap-job ───────
                  # P0460 missed bootstrap-job.yaml (default-off → CI never
                  # rendered it). First prod install with bootstrap.enabled=true
                  # failed PSA admission at the pre-install hook. Assert both
                  # pod- and container-level securityContext render so future
                  # helper drift can't re-break it.
                  helm template rio . \
                    --set bootstrap.enabled=true \
                    --set global.image.tag=test \
                    --set postgresql.enabled=false \
                    > $TMPDIR/bootstrap-on.yaml
                  yq 'select(.kind=="Job" and .metadata.name=="rio-bootstrap")
                      | .spec.template.spec.securityContext.runAsNonRoot' \
                    $TMPDIR/bootstrap-on.yaml | grep -x true >/dev/null || {
                    echo "FAIL: rio-bootstrap Job pod securityContext.runAsNonRoot != true" >&2
                    exit 1
                  }
                  yq 'select(.kind=="Job" and .metadata.name=="rio-bootstrap")
                      | .spec.template.spec.containers[0].securityContext.capabilities.drop[0]' \
                    $TMPDIR/bootstrap-on.yaml | grep -x ALL >/dev/null || {
                    echo "FAIL: rio-bootstrap Job container securityContext.capabilities.drop[0] != ALL" >&2
                    exit 1
                  }

                  touch $out
                '';

            # CRD drift: crdgen output (split per-CRD) must equal the
            # committed infra/helm/crds/. Catches the "Rust CRD struct
            # changed but nobody ran cargo xtask regen crds" drift — the committed
            # YAML is what Argo syncs, so a stale file means the deployed
            # schema diverges from what the controller expects.
            #
            # Calls scripts/split-crds.py (same script xtask uses) to split
            # multi-doc → one file per metadata.name, into $TMPDIR.
            # diff -r: recursive, exits non-zero on any difference.
            crds-drift =
              let
                crdsYaml = config.packages.crds;
                py = pkgs.python3.withPackages (p: [ p.pyyaml ]);
              in
              pkgs.runCommand "rio-crds-drift"
                {
                  nativeBuildInputs = [
                    py
                    pkgs.diffutils
                  ];
                }
                ''
                  mkdir -p $TMPDIR/split
                  python3 ${./scripts/split-crds.py} ${crdsYaml} $TMPDIR/split
                  diff -r $TMPDIR/split ${./infra/helm/crds} > $TMPDIR/diff || {
                    echo "FAIL: crdgen output drifted from infra/helm/crds/" >&2
                    echo "Run: cargo xtask regen crds" >&2
                    cat $TMPDIR/diff >&2
                    exit 1
                  }
                  touch $out
                '';

            # infra/eks/generated.auto.tfvars.json must match nix/pins.nix.
            # Same pattern as crds-drift: regenerate-then-diff. jq on both
            # sides so key-order and whitespace don't matter (the committed
            # file is pretty-printed, writeText output is compact).
            tfvars-fresh =
              pkgs.runCommand "rio-tfvars-fresh"
                {
                  nativeBuildInputs = [
                    pkgs.jq
                    pkgs.diffutils
                  ];
                }
                ''
                  jq -S . ${config.packages.tfvars} > $TMPDIR/gen
                  jq -S . ${./infra/eks/generated.auto.tfvars.json} > $TMPDIR/committed
                  diff $TMPDIR/gen $TMPDIR/committed || {
                    echo "FAIL: nix/pins.nix drifted from infra/eks/generated.auto.tfvars.json" >&2
                    echo "Run: nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json" >&2
                    exit 1
                  }
                  touch $out
                '';

            # Onibus state-machine tests (DAG runner / merger / plan-doc validation).
            # Source: .claude/lib/test_scripts.py + .claude/lib/onibus/.
            #
            # Why this gates: test_tracey_domains_matches_spec catches TRACEY_DOMAINS
            # drift vs docs/src/ spec markers. 120bab69 (worker→builder+fetcher rename)
            # desynced the frozenset; onibus plan tracey-markers silently dropped
            # r[builder.*]/r[fetcher.*] for weeks. The test was red on local pytest,
            # green on .#ci — nobody saw it. Gates now.
            #
            # The whole suite gates (~107 tests), not just the drift detector —
            # test_scripts.py IS the onibus tooling test suite. A red test there
            # means the merger / followup pipeline / state models have a bug.
            #
            # DEV-SHELL DIVERGENCE: `nix develop -c pytest` shows 10 MORE failures
            # than `nix develop -c python3 -m pytest`. The bare `pytest` binary is
            # a nixpkgs bash-wrapper that prepends bare-python3 (no site-packages)
            # to PATH; subprocess tests hit `#!/usr/bin/env python3` in onibus and
            # get no pydantic. `python -m pytest` below bypasses the wrapper — PATH
            # stays clean, subprocesses find the withPackages env. This check's
            # result is authoritative; a local bare-pytest run is NOT.
            onibus-pytest = pkgs.stdenv.mkDerivation {
              pname = "rio-onibus-pytest";
              inherit version;
              src = pkgs.lib.fileset.toSource {
                root = unfilteredRoot;
                fileset = pkgs.lib.fileset.unions [
                  ./.claude/lib
                  ./.claude/bin
                  # test_tracey_domains_matches_spec scans docs/src for r[domain.*]
                  # prefixes — needs the spec files present.
                  ./docs/src
                  # _no_dag skipif at test_scripts.py:3415 reads this directly.
                  # Absent → test_dag_deps_cli etc. skip instead of run.
                  ./.claude/dag.jsonl
                  # onibus/__init__.py reads this at import time.
                  ./.claude/integration-branch
                ];
              };
              nativeBuildInputs = [
                (pkgs.python3.withPackages (ps: [
                  ps.pytest
                  ps.pydantic
                ]))
                # conftest.py:18 tmp_repo fixture + several tests subprocess git.
                pkgs.git
              ];
              dontConfigure = true;
              dontBuild = true;
              doCheck = true;
              checkPhase = ''
                # onibus shebang is `#!/usr/bin/env python3`. The nix sandbox
                # has /bin/sh but NOT /usr/bin/env — subprocess exec fails
                # with ENOENT (reported against the script path, not the
                # shebang, which makes diagnosis confusing). patchShebangs
                # rewrites to the absolute withPackages-env python3 path.
                # _copy_harness copies this patched file into tmp_repo, so
                # the subprocess tests get a working shebang too.
                patchShebangs .claude/bin

                # `python -m pytest`, NOT bare `pytest` — see DEV-SHELL DIVERGENCE
                # note above. The bash wrapper for `pytest` prepends bare python3
                # to PATH; this derivation's PATH is clean going in, but the -m
                # form is defensive against nixpkgs python-wrapping changes.
                #
                # -x: stop at first failure. Suite runs ~60s; -x means the CI
                # log shows the ONE test that broke, not a cascade.
                python -m pytest .claude/lib/test_scripts.py -x -v
              '';
              installPhase = "touch $out";
            };
          };

          # Container images (Linux-only — dockerTools uses Linux VM
          # namespaces for layering). Worker image includes nix + fuse3
          # + util-linux + passwd stubs; others are minimal.
          #
          # Factored into a function so the coverage pipeline can rebuild
          # images with the instrumented workspace (dockerImagesCov below).
          mkDockerImages =
            {
              rio-workspace,
              coverage ? false,
            }:
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              import ./nix/docker.nix {
                inherit pkgs rio-workspace coverage;
                # Dashboard only for the non-coverage image set.
                # nginx+static has no LLVM instrumentation and the
                # coverage VM fixture doesn't deploy it — passing
                # null elides the `dashboard` attr (docker.nix
                # optionalAttrs guard) so the linkFarm doesn't
                # reference a redundant drv.
                rioDashboard = if coverage then null else rioDashboard;
              }
            );
          dockerImages = mkDockerImages { inherit rio-workspace; };

          # Subcharts from nixhelm (FODs — hash-pinned `helm pull`).
          # Referenced by: helm-lint check (symlinked into charts/ in-sandbox),
          # packages.helm-* (`cargo xtask {eks deploy,dev apply}` symlink from
          # the result path into the working-tree charts/ — gitignored).
          subcharts = import ./nix/helm-charts.nix {
            inherit (inputs) nixhelm;
            inherit system pkgs;
          };

          # --------------------------------------------------------------
          # Scenario×fixture VM tests (Linux-only — need NixOS VMs + KVM)
          # --------------------------------------------------------------
          #
          #   vm-protocol-{warm,cold}-standalone — 3 VMs: opcode coverage
          #   vm-scheduling-{core,disrupt}-standalone — 5 VMs: fanout, size-class, cgroup
          #   vm-security-standalone — 3 VMs: mTLS, HMAC, tenant-resolve
          #   vm-observability-standalone — 5 VMs: metrics, traces, logs
          #   vm-ca-cutoff-standalone — CA-on-CA cutoff propagation
          #   vm-chaos-standalone — fault injection
          #   vm-lifecycle-{core,recovery,autoscale,wps}-k3s
          #   vm-le-{stability,build}-k3s — 2-node k3s fixture (fragment splits)
          #   vm-security-nonpriv-k3s — privileged-hardening e2e
          #   vm-cli-k3s — rio-cli integration
          #   vm-dashboard-k3s, vm-dashboard-gateway-k3s — gRPC-Web + envoy
          #   vm-fod-proxy-k3s — fixed-output derivation proxy
          #   vm-netpol-k3s — NetworkPolicy enforcement
          #
          # mkVmTests: build the attrset for a given (workspace,
          # dockerImages, coverage) triple. vmTests uses the normal
          # build; vmTestsCov uses the instrumented build + coverage=
          # true so common.nix sets LLVM_PROFILE_FILE and appends
          # collectCoverage to each testScript.

          # Request a minimum CPU allocation from the remote builder. Each
          # VM has `virtualisation.cores = 4` in common.nix; without
          # this, the builder's heuristic allocation can under-provision
          # (vm-scheduling-core once got 5 CPUs for 4 VMs → 16 vCPUs on 5
          # physical, 2 VMs fell back to TCG, worker1's kernel boot
          # starved at PCI enumeration → Shell disconnected flake).
          #
          # Floor of 64 vCPU / 128GB: prevents KVM contention across
          # concurrent VM-test builds on the same host. With ~60-190
          # CPU hosts, 64 vCPU floor caps at ~1-3 concurrent VM-test
          # builds per host (previously ~9 at old ×4 formula → up to
          # ~45 concurrent qemu KVM_CREATE_VM → some lose the race,
          # "failed to initialize kvm: Permission denied" → TCG
          # fallback or hard fail). 128GB floor ensures the k3s tests
          # (≈20GB peak) plus qemu+test-driver overhead have headroom.
          # cpuHints is still consulted for the ×4 formula when it
          # exceeds the floor (future >16-VM tests).
          withMinCpu =
            numVMs: test:
            let
              byVMs = numVMs * 4 + 1;
              cpuFloor = 64;
              memFloor = 131072;
            in
            test.overrideTestDerivation {
              NIXBUILDNET_MIN_CPU = toString (pkgs.lib.max byVMs cpuFloor);
              NIXBUILDNET_MIN_MEM = toString memFloor;
            };

          mkVmTests =
            {
              rio-workspace,
              dockerImages,
              coverage,
            }:
            let
              allTests = import ./nix/tests {
                inherit
                  pkgs
                  rio-workspace
                  dockerImages
                  system
                  coverage
                  ;
                rioModules = inputs.self.nixosModules;
                inherit (inputs) nixhelm;
              };
              # Per-test builder CPU hint. withMinCpu sets
              # NIXBUILDNET_MIN_CPU (numVMs × 4 + 1) to prevent
              # oversubscription → TCG fallback → qemu stall. Fallthrough:
              # 8 for -k3s suffix, else 4 (see mapAttrs below).
              cpuHints = {
                # 3 VMs (control+worker+client). Control is 4-core.
                vm-protocol-warm-standalone = 3;
                vm-protocol-cold-standalone = 3;
                # 5 VMs: control + wsmall1/wsmall2/wlarge + client.
                # Both scheduling splits boot the full 3-worker fixture.
                vm-scheduling-core-standalone = 5;
                vm-scheduling-disrupt-standalone = 5;
                # 3 VMs: control + worker + client.
                vm-security-standalone = 3;
                # 3 VMs: control + worker + client. Single-worker
                # standalone fixture (ca-cutoff chain is serial anyway).
                vm-ca-cutoff-standalone = 3;
                # 3 VMs: control + worker + client. toxiproxy runs as a
                # systemd unit on control, not a separate VM.
                vm-chaos-standalone = 3;
                # 5 VMs: control + worker1/2/3 + client.
                vm-observability-standalone = 5;
                # 3 VMs but k3s-server is 8-core 6GB + k3s-agent 8-core 4GB.
                # All lifecycle + leader-election splits boot the same
                # 2-node k3s fixture.
                vm-lifecycle-core-k3s = 8;
                vm-lifecycle-recovery-k3s = 8;
                vm-lifecycle-autoscale-k3s = 8;
                vm-lifecycle-wps-k3s = 8;
                vm-le-stability-k3s = 8;
                vm-le-build-k3s = 8;
                # k3s + one extra airgap image (squid). Does builds.
                vm-fod-proxy-k3s = 8;
                # k3s + smarter-device-manager image. Nonpriv e2e
                # (device-plugin + hostUsers:false + cgroup rw-remount).
                vm-security-nonpriv-k3s = 8;
                # k3s + envoy-gateway operator (+2 images). No builds.
                vm-dashboard-gateway-k3s = 8;
                # Same fixture + rio-dashboard nginx image. curl via
                # nginx → envoy → scheduler (SPA + proxy_buffering gate).
                vm-dashboard-k3s = 8;
                # k3s base fixture. rio-cli AdminService smoke.
                vm-cli-k3s = 8;
                # k3s base fixture. Worker egress NetworkPolicy enforce.
                vm-netpol-k3s = 8;
                # Same 2-node k3s fixture + bootstrap Job backoff.
                # Asserts PSA-restricted — NOT in vmTestsCov (see removeAttrs below).
                vm-lifecycle-prod-parity-k3s = 8;
              };
            in
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              pkgs.lib.mapAttrs (
                name:
                withMinCpu (
                  cpuHints.${name}
                    # k3s fixture: 2-node cluster, k3s-server 8-core + k3s-agent
                    # 8-core. Every -k3s test in the table is 8; encode that as
                    # the suffix default so new -k3s tests don't fall through to
                    # 4. Catchup-fix precedent: d6f74e27 + fa55ef13 both added
                    # forgotten -k3s entries. T539 catches the OTHER direction
                    # (dead entries for deleted tests).
                    or (if pkgs.lib.hasSuffix "-k3s" name then 8 else 4)
                )
              ) allTests
            );

          vmTests = mkVmTests {
            inherit rio-workspace dockerImages;
            coverage = false;
          };

          # Coverage-mode VM tests. Not in `checks` (too slow for flake
          # check) — exposed as packages.cov-vm-<scenario> for manual runs
          # + consumed by nix/coverage.nix for the merged lcov.
          vmTestsCov =
            removeAttrs
              (mkVmTests {
                rio-workspace = rio-workspace-cov;
                dockerImages = mkDockerImages {
                  rio-workspace = rio-workspace-cov;
                  coverage = true;
                };
                coverage = true;
              })
              # prod-parity asserts readOnlyRootFilesystem=true (PSA-restricted);
              # coverage-mode bumps PSA to privileged → assertion deterministically
              # fails. The test is ABOUT PSA — running it under a mode that changes
              # PSA defeats the point. No coverage delta lost: PSA rendering is
              # Helm+YAML, no r[impl]-annotated Rust.
              [ "vm-lifecycle-prod-parity-k3s" ];

          # --------------------------------------------------------------
          # Coverage merge pipeline (Linux-only — depends on vmTestsCov)
          # --------------------------------------------------------------
          #
          # nix/coverage.nix merges profraws from each coverage-mode VM
          # test with the unit-test lcov, producing combined + per-test
          # lcov + genhtml report.
          #
          # stripPrefix: buildRustCrate's --remap-path-prefix maps
          # sandbox → `/`, so profraws reference `/rio-store/src/...`.
          # Strip the leading slash to get repo-relative paths that
          # genhtml can resolve against commonSrc.
          #
          # Coverage mode uses workspaceBinsCov (not workspaceBins) —
          # same closure-scrub but skips strip so the __llvm_covfun /
          # __llvm_covmap sections llvm-cov needs stay intact.
          coverage = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            import ./nix/coverage.nix {
              inherit
                pkgs
                rustStable
                rio-workspace-cov
                vmTestsCov
                commonSrc
                ;
              unitCoverage = crateChecks.coverage;
            }
          );

          # --------------------------------------------------------------
          # CI aggregate target
          # --------------------------------------------------------------
          #
          # Single-target validation bundle. Built via linkFarmFromDrvs —
          # result is a directory of symlinks to each constituent's output
          # (inspectable with `ls result/`).
          #
          # Derived from config.checks — every check is a CI constituent.
          # Before P0525 this was a manual list that had drifted to 44/45:
          # codecov-matrix-sync (the one guarding codecov.yml drift) was
          # missing, so after_n_builds went 2 commits stale before ee957551
          # hand-patched it. `nix flake check` caught it; `.#ci` (the merge
          # gate) did not. Deriving closes the class.
          #
          # config.checks is the flake-parts merged result: our checks
          # attrset + git-hooks module's pre-commit. That attrset already
          # //-merges vmTests and fuzz.runs, so they appear here without
          # explicit addition. On non-Linux it's smaller (vmTests, fuzz,
          # codecov-matrix-sync are all optionalAttrs isLinux upstream) —
          # ci degrades to Rust checks + pre-commit automatically.
          #
          # builtins.attrValues is attr-name sorted → stable linkFarm hash.
          ci = pkgs.linkFarmFromDrvs "rio-ci" (
            builtins.attrValues config.checks
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              # cov-smoke: one coverage-mode VM scenario, asserts
              # profraw→lcov pipeline works. ~5min. Catches
              # "coverage infra broken" at merge-gate instead of
              # 118 commits later via backgrounded coverage-full.
              # NOT a check (too slow for `nix flake check` on a
              # non-KVM host) — CI-aggregate only.
              coverage.smoke
            ]
          );

          # --------------------------------------------------------------
          # Multi-Nix golden conformance matrix (weekly tier)
          # --------------------------------------------------------------
          #
          # Runs golden_conformance against 4 daemon variants: pinned Nix,
          # Nix 2.20-maintenance, Nix master, Lix. Weekly cron invokes
          # `nix build .#golden-matrix`. NOT in `.#ci` — building three
          # extra Nix source trees is a 60-90min cold-cache tax. Exported
          # Linux-only at the `packages` site (the flake inputs eval fine
          # on Darwin but `nix-daemon` needs a real unix socket + /nix
          # layout, and we don't run the matrix on macs anyway).
          goldenMatrix = import ./nix/golden-matrix.nix {
            inherit pkgs inputs system;
            inherit (crateChecks) mkNextestRun;
          };

          # --------------------------------------------------------------
          # Mutation testing (weekly tier — NOT in .#ci)
          # --------------------------------------------------------------
          #
          # cargo-mutants mutates source (swap < for <=, delete a
          # statement, replace a return with Default::default()),
          # reruns the test suite, flags mutations that SURVIVE —
          # code paths the tests don't actually constrain. Tracey
          # answers "is this spec rule covered"; mutants answers
          # "does the test that covers it actually catch bugs".
          #
          # Scoped via .config/mutants.toml to high-signal targets
          # (scheduler state machine, wire primitives, ATerm parser,
          # HMAC verify, manifest encoding — ~320 mutations). Weekly
          # cron invokes `nix build .#mutants`; survived-count is
          # diffed week-over-week. Exit 2 (survived) and 3 (timeouts
          # only) are EXPECTED and swallowed; everything else
          # propagates. A baseline-health jq gate additionally fails
          # the derivation if zero mutations were tested. Findings
          # are a trend metric, not a gate.
          #
          # `packages` not `checks` (same as golden-matrix): hours
          # per run, not something `nix flake check` should touch.
          #
          # crate2nix port: cargo-mutants fundamentally needs a
          # writable cargo workspace (it mutates source in-place and
          # re-invokes `cargo build` + `cargo nextest run` per
          # mutation). crate2nix's per-crate-drv model doesn't map to
          # that workflow — so this derivation BYPASSES crate2nix
          # entirely and uses the same stdenv.mkDerivation +
          # importCargoLock + cargoSetupHook pattern as the `deny`
          # check and nix/fuzz.nix. The vendored dep tree is cached
          # (same Cargo.lock as the main build), so the only
          # per-invocation cost is the baseline cargo build +
          # per-mutation rebuilds — same as it ever was under crane.
          # No dep-level caching across invocations, but that was true
          # of crane's buildDepsOnly too (weekly-tier, cold cache each
          # cron run is acceptable).
          mutants = pkgs.stdenv.mkDerivation (
            sysCrateEnv.allEnv
            // {
              pname = "rio-mutants";
              inherit version;

              src = pkgs.lib.fileset.toSource {
                root = unfilteredRoot;
                fileset = pkgs.lib.fileset.unions [
                  workspaceFileset
                  ./.config/mutants.toml
                  ./.config/nextest.toml
                ];
              };

              cargoDeps = rustPlatformStable.importCargoLock {
                lockFile = ./Cargo.lock;
              };

              nativeBuildInputs = with pkgs; [
                rustStable
                rustPlatformStable.cargoSetupHook
                cargo-mutants
                cargo-nextest
                jq
                pkg-config
                protobuf
                cmake
                # Test-time deps (baseline run hits the whole workspace).
                # Same set as crateChecks' runtimeTestInputs.
                inputs.nix.packages.${system}.nix
                openssh
                postgresql_18
              ];

              buildInputs =
                with pkgs;
                [
                  openssl
                  llvmPackages.libclang.lib
                ]
                ++ sysCrateEnv.allLibs;

              # cmake is in nativeBuildInputs for aws-lc-sys's build.rs,
              # not for this derivation's configurePhase. The cmake setup
              # hook would otherwise look for CMakeLists.txt at source
              # root — there isn't one.
              dontUseCmakeConfigure = true;

              PROTOC = "${pkgs.protobuf}/bin/protoc";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              PG_BIN = "${pkgs.postgresql_18}/bin";
              # Golden fixture paths — the baseline run hits
              # golden_conformance (workspace-wide). `inherit` from
              # the shared goldenTestEnv attrset so new golden
              # fixture vars propagate here automatically.
              inherit (goldenTestEnv)
                RIO_GOLDEN_TEST_PATH
                RIO_GOLDEN_CA_PATH
                RIO_GOLDEN_FORCE_HERMETIC
                ;
              NEXTEST_HIDE_PROGRESS_BAR = "1";

              # `--in-place`: mutate the unpacked source in $PWD
              # (cargoSetupHook unpacks to a writable tmpdir). Cheaper
              # than the default copy-per-mutation mode when running
              # inside a throwaway sandbox anyway.
              #
              # `--no-shuffle` is the default in current cargo-mutants
              # but kept explicit for the week-over-week diff guarantee.
              #
              # `--output $out`: cargo-mutants creates mutants.out/
              # INSIDE the given dir, so result/mutants.out/outcomes.json.
              #
              # Exit-code contract (mutants.rs/exit-codes.html): 0 = all
              # caught, 2 = mutants survived (EXPECTED — no codebase is
              # 100% mutation-killed), 3 = timeouts only, 4 = baseline
              # failed, 1 = usage/internal error. We swallow only 2 and
              # 3 (expected non-zero), propagate everything else, AND
              # belt-and-braces jq-check that the mutation phase
              # actually ran (non-baseline outcomes > 0).
              buildPhase = ''
                runHook preBuild
                mkdir -p $out
                cargo mutants \
                  --in-place --no-shuffle \
                  --config .config/mutants.toml \
                  --output $out \
                  --timeout-multiplier 2.0 \
                  || { rc=$?; [ $rc -eq 2 ] || [ $rc -eq 3 ] || exit $rc; }

                # Baseline-health gate: if outcomes.json has zero
                # MUTATION outcomes (everything that isn't the baseline
                # Success/Failure entry), the baseline failed and the
                # run is void. Fail loud — cat debug.log so the build
                # log shows the actual nextest failure. Catches the
                # case where cargo-mutants exits 0 with an empty
                # outcomes list (graceful baseline-skip) as well as
                # the file-missing case (jq → stderr → tested=0).
                tested=$(jq '[.outcomes[] | select(.summary != "Success" and .summary != "Failure")] | length' \
                  $out/mutants.out/outcomes.json 2>/dev/null || echo 0)
                if [ "$tested" -eq 0 ]; then
                  echo "mutants baseline failed — zero mutations tested" >&2
                  cat $out/mutants.out/debug.log >&2 2>/dev/null || true
                  exit 1
                fi
                runHook postBuild
              '';

              installPhase = ''
                runHook preInstall
                # Extract caught/missed counts from the JSON outcome
                # stream for the weekly-diff step. No `|| echo 0`
                # fallback: the baseline-health gate above already
                # fails if outcomes.json is missing — a jq failure
                # here is a real error (malformed JSON).
                jq '[.outcomes[] | select(.summary == "CaughtMutant")] | length' \
                  $out/mutants.out/outcomes.json > $out/caught-count
                jq '[.outcomes[] | select(.summary == "MissedMutant")] | length' \
                  $out/mutants.out/outcomes.json > $out/missed-count
                runHook postInstall
              '';
            }
          );

          # Smoke check: assert .#mutants produced ≥1 mutation
          # outcome. Belt-and-braces with the baseline-health gate
          # inside the mutants derivation itself — if that gate is
          # accidentally relaxed, this still catches a void run.
          # NOT in .#ci (transitively builds mutants, hours). The
          # weekly cron sequences `nix build .#mutants .#mutants-smoke`
          # — nix substitutes the mutants output from cache if already
          # built, so the smoke check adds O(seconds).
          mutants-smoke =
            pkgs.runCommand "mutants-smoke"
              {
                nativeBuildInputs = [ pkgs.jq ];
              }
              ''
                tested=$(jq '[.outcomes[] | select(.summary != "Success" and .summary != "Failure")] | length' \
                  ${mutants}/mutants.out/outcomes.json)
                echo "mutants-smoke: $tested mutations tested" >&2
                if [ "$tested" -eq 0 ]; then
                  echo "FAIL: mutants baseline failed — zero mutations tested" >&2
                  cat ${mutants}/mutants.out/debug.log >&2 2>/dev/null || true
                  exit 1
                fi
                caught=$(cat ${mutants}/caught-count)
                missed=$(cat ${mutants}/missed-count)
                echo "mutants-smoke: caught=$caught missed=$missed" >&2
                echo "$tested" > $out
              '';

          # ──────────────────────────────────────────────────────────
          # GitHub Actions integration
          # ──────────────────────────────────────────────────────────
          #
          # Structured attrset consumed by .github/workflows/ci.yml.
          # Keeps "what runs in CI" policy in Nix — the workflow is a
          # thin consumer that evaluates this to generate matrices.
          #
          # matrix.<name>: attrsets where keys → GHA matrix entries and
          #   values → derivations to build. Add/remove entries here;
          #   the workflow picks them up automatically via `nix eval`.
          #
          # Runner selection by naming convention: entries with a `vm-`
          # prefix run on `rio-ci-kvm` (bare-metal, /dev/kvm mounted);
          # everything else on `rio-ci` (spot). This keeps the flake
          # emitting simple name→drv maps without per-entry metadata.
          #
          # CI runners are Linux-only. vmTests/coverage/fuzz.runs are
          # all optionalAttrs isLinux upstream, so this whole block is
          # too — on Darwin it's {} (harmless).
          githubActions = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            matrix = {
              # Rust + static checks. Excludes `coverage` (superseded
              # by the coverage matrix below — Codecov merges flags).
              checks = {
                clippy = crateChecks.clippyCheck;
                doc = crateChecks.docCheck;
                inherit (crateChecks) nextest;
                inherit (miscChecks)
                  deny
                  tracey-validate
                  helm-lint
                  crds-drift
                  tfvars-fresh
                  onibus-pytest
                  ;
                inherit (config.checks) pre-commit;
                dashboard = rioDashboard;
              };
              # 2min fuzz runs, one matrix entry per target. Keys are
              # fuzz-<target> (from nix/fuzz.nix). On a cold cache each
              # entry rebuilds the shared fuzz-build derivation, but
              # spot CPU is cheap and the cache fills after first green.
              fuzz = fuzz.runs;
              # Normal VM tests. Keys: vm-<scenario>-<fixture>. Per-test
              # red/green signal in the GHA UI.
              vm-test = vmTests;
              # lcov-producing jobs, one per Codecov flag. `unit`
              # runs on spot; `vm-*` need KVM (instrumented VM tests
              # → profraw → lcov). Workflow picks runs-on by prefix.
              coverage = {
                unit = coverage.unitLcov;
              }
              // coverage.perTestLcov;
            };
            # niks3 CLI for cache pushes. niks3-push action builds
            # this via `nix build --print-out-paths` and includes
            # the store path in its push — so the first job to
            # complete uploads it to S3, subsequent jobs substitute.
            inherit (inputs.niks3.packages.${system}) niks3;
          };
        in
        {
          # Exported via legacyPackages (free-form, not checked by
          # `nix flake check`). The top-level `flake.githubActions`
          # alias above makes it accessible as `.#githubActions.*`.
          legacyPackages = { inherit githubActions; };

          # Import rust-overlay
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          # Configure treefmt
          treefmt.config = {
            flakeCheck = false;
            projectRootFile = "flake.nix";

            programs = {
              nixfmt.enable = true;

              # Rust formatting (stable rustfmt — nightly rustfmt can
              # produce different output, and we want CI/dev parity here)
              rustfmt = {
                enable = true;
                package = rustStable;
              };

              # TOML formatting
              taplo.enable = true;
            };
            settings.global.excludes = [
              # cargo-hakari owns this file's format. taplo and hakari
              # disagree on array layout → `hakari generate` sees drift
              # after every treefmt pass, breaking regen idempotency.
              "workspace-hack/Cargo.toml"
            ];
          };

          # Configure git hooks
          pre-commit = {
            check.enable = true;

            settings.excludes = [
              "docs/mermaid\\.min\\.js$"
              # Fuzz corpus seeds are exact binary/text inputs; trailing
              # newlines would change what the fuzzer sees.
              "^rio-nix/fuzz/corpus/"
              "^rio-store/fuzz/corpus/"
            ];

            settings.hooks = {
              treefmt.enable = true;
              convco.enable = true;
              ripsecrets.enable = true;
              check-added-large-files = {
                enable = true;
                # Cargo.json is the crate2nix pre-resolved dependency
                # graph (~500 KB, grows with dep count). Treated like
                # Cargo.lock: generated + checked in, reviewed on
                # regeneration. See nix/crate2nix.nix.
                excludes = [ "^Cargo\\.json$" ];
              };
              check-merge-conflicts.enable = true;

              # Reject commits containing cargo-mutants dirty markers.
              # `cargo xtask mutants` mutates source in-place; if it crashes or is
              # interrupted, the mutated line (with `/* ~ changed by
              # cargo-mutants ~ */`) survives in the worktree. A blind
              # commit then ships a mutant. The marker string is reliable
              # — cargo-mutants always wraps its mutations with it.
              check-mutants-marker = {
                enable = true;
                name = "check-mutants-marker";
                entry = toString (
                  pkgs.writeShellScript "check-mutants-marker" ''
                    # Marker verified for cargo-mutants 26.2.0 —
                    # MUTATION_MARKER_COMMENT const at src/mutate.rs
                    # upstream. If a cargo-mutants bump changes the
                    # marker string, this hook SILENTLY passes —
                    # re-verify on major-version bumps.
                    # Only scan .rs files (cargo-mutants only touches Rust).
                    # grep -l for file-list, exit 1 if any match.
                    if git diff --cached --name-only -- '*.rs' \
                       | xargs -r grep -l 'changed by cargo-mutants' 2>/dev/null \
                       | grep -q .; then
                      echo 'error: cargo-mutants marker found in staged .rs files'
                      echo 'cargo-mutants left a dirty mutation — `git checkout -- <file>` to revert'
                      git diff --cached --name-only -- '*.rs' \
                        | xargs -r grep -l 'changed by cargo-mutants' 2>/dev/null
                      exit 1
                    fi
                  ''
                );
                files = "\\.rs$";
                language = "system";
                pass_filenames = false;
              };

              end-of-file-fixer.enable = true;
              trim-trailing-whitespace.enable = true;
              deadnix.enable = true;
              nil.enable = true;
              statix.enable = true;

              # Reject commits that change a query! SQL string without
              # regenerating .sqlx/. With SQLX_OFFLINE=true, any query!
              # whose SQL hash no longer matches a .sqlx/*.json file
              # fails to compile — so `cargo check` on the crates that
              # use query! is the definitive staleness check. ~5s
              # incremental. Fires only on .rs changes to skip docs-only
              # commits. CI (`.#ci`) catches the same failure via the
              # clippy/nextest builds, so this hook is dev-ergonomics:
              # fail at commit time instead of 10min later.
              sqlx-prepare-check = {
                enable = true;
                name = "sqlx-prepare-check";
                entry = toString (
                  pkgs.writeShellScript "sqlx-prepare-check" ''
                    # Only check if any staged .rs file touches a query! macro.
                    # Otherwise this is a no-op (e.g. pure-refactor commits
                    # that don't change SQL).
                    if git diff --cached --name-only -- '*.rs' \
                       | xargs -r grep -l 'query!\|query_as!\|query_scalar!' \
                       | grep -q .; then
                      SQLX_OFFLINE=true cargo check --quiet -p rio-scheduler -p rio-store \
                        || { echo 'sqlx query cache stale — run `cargo xtask regen sqlx`'; exit 1; }
                    fi
                  ''
                );
                files = "\\.rs$";
                language = "system";
                pass_filenames = false;
              };

              # Reject commits that change Cargo.toml/Cargo.lock without
              # regenerating Cargo.json. crate2nix reads Cargo.lock to
              # produce the per-crate build graph; a stale Cargo.json
              # means nix builds use the OLD dep set while cargo uses
              # the new one — silent divergence until a nix-only build
              # fails with "crate foo not found". File-gated on
              # Cargo.toml/Cargo.lock so unrelated commits don't pay
              # the ~10s regeneration cost.
              crate2nix-check = {
                enable = true;
                name = "crate2nix-check";
                entry = toString (
                  pkgs.writeShellScript "crate2nix-check" ''
                    set -euo pipefail
                    # Gate on staged Cargo.{toml,lock}. In the hermetic
                    # check derivation (pre-commit run --all-files on a
                    # clean checkout), nothing is staged → no-op. This
                    # also keeps the hook off the hot path for commits
                    # that don't touch the dep graph.
                    if ! git diff --cached --name-only \
                       | grep -qE '(^|/)Cargo\.(toml|lock)$'; then
                      exit 0
                    fi
                    tmp=$(mktemp -d)
                    trap 'rm -rf "$tmp"; rm -f Cargo.json.check' EXIT
                    # Snapshot Cargo.lock — `cargo metadata` inside
                    # crate2nix can bump transitive deps if the local
                    # cache is cold. Restore afterward so the check
                    # has no side effects.
                    cp Cargo.lock "$tmp/Cargo.lock.orig"
                    # Generate in workspace root — crate2nix emits path
                    # fields relative to the output file's directory, so
                    # -o $tmp/... would produce ../../root/... paths that
                    # never match the committed Cargo.json.
                    ${crate2nixCli}/bin/crate2nix generate --format json -o Cargo.json.check 2>/dev/null
                    echo >> Cargo.json.check  # match end-of-file-fixer
                    cp "$tmp/Cargo.lock.orig" Cargo.lock
                    if ! diff -q Cargo.json Cargo.json.check >/dev/null; then
                      echo 'error: Cargo.json is stale — run `cargo xtask regen cargo-json`'
                      exit 1
                    fi
                  ''
                );
                files = "(^|/)Cargo\\.(toml|lock)$";
                language = "system";
                pass_filenames = false;
              };

              # Reject commits that change Cargo.toml/Cargo.lock without
              # regenerating workspace-hack. A stale workspace-hack means
              # per-package builds use a different feature set than the
              # workspace build → cache thrash. `hakari verify` is fast
              # (metadata-only, no compile).
              hakari-check = {
                enable = true;
                name = "hakari-check";
                entry = toString (
                  pkgs.writeShellScript "hakari-check" ''
                    set -euo pipefail
                    if ! git diff --cached --name-only \
                       | grep -qE '(^|/)Cargo\.(toml|lock)$'; then
                      exit 0
                    fi
                    ${pkgs.cargo-hakari}/bin/cargo-hakari hakari verify 2>/dev/null || {
                      echo 'error: workspace-hack is stale — run `cargo xtask regen hakari`'
                      exit 1
                    }
                  ''
                );
                files = "(^|/)Cargo\\.(toml|lock)$";
                language = "system";
                pass_filenames = false;
              };

              # No kubeconform hook: it fetches ~300MB of schemas from
              # raw.githubusercontent.com at runtime, which fails in the
              # hermetic remote build sandbox (config.checks.pre-commit
              # runs all hooks there). Run it interactively if needed:
              #   helm template rio infra/helm/rio-build --set global.image.tag=x \
              #     | kubeconform -strict -skip CustomResourceDefinition,Certificate,...
              # The helm-lint flake check above catches template syntax
              # errors without network.
            };
          };

          # --------------------------------------------------------------
          # Dev shells
          # --------------------------------------------------------------
          #
          # Default = nightly so `cargo fuzz run` works out of the box.
          # CI builds use stable (rustStable via crate2nix), so if you
          # write nightly-only code, checks.clippy / checks.nextest will
          # catch it.
          #
          # Use `nix develop .#stable` for strict CI-parity dev.
          devShells =
            let
              shellPackages = with pkgs; [
                # Cargo tools
                cargo-edit
                cargo-expand
                cargo-fuzz # works in default (nightly) shell; errors on stable
                cargo-hakari # workspace-hack regen — `cargo xtask regen hakari`
                cargo-mutants # weekly tier — see `cargo xtask mutants` / `.#mutants`
                cargo-nextest
                cargo-outdated
                cargo-watch

                # Debugging tools
                lldb
                gdb
                lcov # `lcov --summary`/`--list` on the coverage output
                stress-ng # flake-repro under load (rio-ci-flake-fixer/validator)

                # Documentation
                mdbook
                mdbook-mermaid

                # Integration test deps
                postgresql_18
                sqlx-cli # `cargo xtask regen sqlx` + `cargo sqlx migrate`

                # Local dev stack (`process-compose up`)
                process-compose

                # Formatting (nix fmt also works, but direct treefmt is handy)
                config.treefmt.build.wrapper

                # Spec-coverage: `tracey query validate`, `tracey web`
                traceyPkg

                # crate2nix CLI for regenerating Cargo.json after
                # Cargo.lock changes. PoC — see
                # .claude/notes/crate2nix-migration-assessment.md.
                crate2nixCli

                # Dashboard dev: `pnpm install --lockfile-only` (hash bumps),
                # `pnpm run dev` (vite dev server with Envoy proxy). Proto
                # stubs regen: `cd rio-dashboard && buf generate --template
                # buf.gen.yaml ../rio-proto/proto` (src/gen/ is gitignored).
                nodejs
                pnpm_10
                buf
                protoc-gen-es

                # Deploy tooling for infra/eks/. Large closures (awscli2
                # pulls python3 + botocore) but the user asked for
                # everything-in-one-shell over a separate .#deploy.
                # Scripts under infra/eks/ also carry nix-shell shebangs
                # pointing at these same packages, so they work even if
                # someone runs them outside `nix develop`.
                awscli2
                ssm-session-manager-plugin # cargo xtask k8s -p eks smoke — SSM tunnel to NLB
                lsof # cargo xtask k8s rsb — reap stale tunnel listeners on :2222
                # opentofu (not terraform: BSL license → unfree in nixpkgs)
                # with providers bundled via withPlugins. No `tofu init`
                # download step — providers are in the nix store, pinned by
                # nixpkgs rev. .terraform.lock.hcl is gitignored (nix is the
                # lock). The provider set must cover transitive module deps
                # too (EKS module pulls cloudinit + null).
                (opentofu.withPlugins (p: [
                  p.hashicorp_aws
                  p.hashicorp_helm
                  p.hashicorp_kubernetes
                  p.hashicorp_random
                  p.hashicorp_tls
                  p.hashicorp_time
                  p.hashicorp_cloudinit # transitive: terraform-aws-modules/eks
                  p.hashicorp_null # transitive: terraform-aws-modules/eks
                ]))
                kubectl
                kind # cargo xtask k8s -p kind — fast-iteration local cluster
                skopeo # cargo xtask k8s push -p eks — docker-archive → ECR
                manifest-tool # cargo xtask k8s push -p eks — multi-arch OCI index
                kubernetes-helm
                kubeconform # ad-hoc schema validation (no pre-commit hook — fetches 300MB, sandbox blocks)
                yq-go # nix/helm-render.nix
                grpcurl # manual AdminService poking when rio-cli isn't enough
                openssl # openssl rand 32 → HMAC key
                git

                # Python env for .claude/lib/ + co-located skill scripts —
                # pydantic models are the agent-boundary contracts (each
                # script has `--schema` to print JSON Schema).
                (python3.withPackages (ps: [
                  ps.pydantic
                  ps.pytest
                  ps.pyyaml # cargo xtask regen crds → scripts/split-crds.py
                ]))
              ];
              # Shared mkShell builder. Lists build deps explicitly
              # (openssl, libclang, sys-crate libs for pkg-config
              # probes, protobuf+cmake for rio-proto's codegen).
              mkRioShell =
                rust:
                pkgs.mkShell (
                  sysCrateEnv.allEnv
                  // {
                    packages = [ rust ] ++ shellPackages;
                    nativeBuildInputs = with pkgs; [
                      pkg-config
                      protobuf
                      cmake
                    ];
                    buildInputs =
                      with pkgs;
                      [
                        openssl
                        llvmPackages.libclang.lib
                      ]
                      ++ sysCrateEnv.allLibs;
                    RUST_BACKTRACE = "1";
                    PROTOC = "${pkgs.protobuf}/bin/protoc";
                    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
                    PG_BIN = "${pkgs.postgresql_18}/bin";
                    # sqlx query! macros read .sqlx/ instead of connecting
                    # to PG. `cargo build` works without a live DB.
                    # `cargo xtask regen sqlx` unsets this locally to regenerate.
                    SQLX_OFFLINE = "true";
                    RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
                    # Repo-local kubeconfig: xtask k8s writes here, so
                    # direct kubectl/helm in the shell hits the same
                    # cluster. Matches xtask/src/sh.rs:kubeconfig_path().
                    shellHook = ''
                      export KUBECONFIG="$PWD/.kube/config"
                      ${config.pre-commit.installationScript}
                    '';
                  }
                );
            in
            {
              default = mkRioShell rustNightly;
              stable = mkRioShell rustStable;
            };

          # --------------------------------------------------------------
          # Packages
          # --------------------------------------------------------------
          packages = {
            default = rio-workspace;
            # Instrumented build (inspection: `objdump -h result/bin/rio-store
            # | grep llvm_prf`). Used by vmTestsCov and nix/coverage.nix.
            inherit rio-workspace-cov;
            # debug: nix build .#fuzz-build-nix / .#fuzz-build-store
            fuzz-build-nix = fuzz.builds.rio-nix-fuzz-build;
            fuzz-build-store = fuzz.builds.rio-store-fuzz-build;
            # Helm charts from nixhelm (unpacked dirs). `cargo xtask {dev apply,
            # eks deploy}` build these and symlink/install from the result
            # path. PG must be in charts/ even when
            # condition: postgresql.enabled is false — Helm validates
            # charts/ against Chart.yaml BEFORE evaluating conditions.
            helm-postgresql = subcharts.postgresql;
            helm-rook-ceph = subcharts.rook-ceph;
            helm-rook-ceph-cluster = subcharts.rook-ceph-cluster;
            # Envoy Gateway operator (dashboard gRPC-Web translation).
            # `cargo xtask k8s envoy -p k3s` installs this before the rio chart
            # so Gateway API / EnvoyProxy CRDs exist when dashboard-
            # gateway*.yaml templates are applied.
            helm-envoy-gateway = subcharts.gateway-helm;
            helm-envoy-gateway-crds = subcharts.gateway-crds-helm;
            # RustFS S3 backend for the kind provider. Subchart of
            # rio-build (condition: rustfs.enabled) — symlinked into
            # charts/ by xtask's chart_deps() for all providers since
            # helm validates charts/ before evaluating conditions.
            helm-rustfs = subcharts.rustfs;
            # nix/pins.nix rendered as *.auto.tfvars.json. snake_case
            # keys in pins.nix → direct toJSON passthrough, no mapping
            # layer. Regenerate the committed copy:
            #   nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json
            tfvars = pkgs.writeText "generated.auto.tfvars.json" (builtins.toJSON (import ./nix/pins.nix));
          }
          # Container images: docker-{gateway,scheduler,store,worker}
          # plus a linkFarm aggregate at `.#dockerImages` (milestone
          # target per docs/src/phases/phase2b.md:46).
          # Linux-only — optionalAttrs means these simply don't exist
          # on Darwin, rather than failing evaluation.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            docker-gateway = dockerImages.gateway;
            docker-scheduler = dockerImages.scheduler;
            docker-store = dockerImages.store;
            docker-builder = dockerImages.builder;
            docker-fetcher = dockerImages.fetcher;
            docker-controller = dockerImages.controller;
            docker-bootstrap = dockerImages.bootstrap;
            docker-dashboard = dockerImages.dashboard;
            docker-all = dockerImages.all;
            dockerImages = pkgs.linkFarm "rio-docker-images" (
              pkgs.lib.mapAttrsToList (name: drv: {
                name = "${name}.tar.zst";
                path = drv;
              }) dockerImages
            );

            # Dev worker VM (QEMU + NixOS). Reuses nix/modules/builder.nix
            # with SLiRP networking to reach the host's control plane.
            # Run: result-worker-vm/bin/run-rio-builder-dev-vm
            worker-vm =
              (nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  ./nix/dev-builder-vm.nix
                  { services.rio.package = rio-workspace; }
                ];
              }).config.system.build.vm;

            # CRD YAML for kustomize. runCommand invokes the crdgen
            # binary (serde_yaml write-only) and dumps two YAML
            # documents (BuilderPool + Build) to $out. Kustomize
            # references this via `nix build .#crds` → result is a
            # file; `cargo xtask regen crds` splits it into
            # one-file-per-CRD under infra/helm/crds/.
            #
            # NOT a derivation that the kustomize base depends on
            # directly (kustomize needs real files, not /nix/store
            # paths). It's a convenience for regeneration:
            #   nix build .#crds && cp result infra/k8s/base/crds.yaml
            #
            # Why not auto-regenerate in CI: the committed YAML is
            # what operators `kubectl apply`. Regenerating on every
            # commit means a CRD schema change silently updates the
            # deployed file — we want that change REVIEWED (it may
            # be backward-incompatible).
            crds = pkgs.runCommand "rio-crds.yaml" { } ''
              ${rio-workspace}/bin/crdgen > $out
            '';

            # ──────────────────────────────────────────────────────────
            # VM coverage targets (manual — NOT in .#ci)
            # ──────────────────────────────────────────────────────────
            #
            # coverage-full: unit + all VM tests merged. ~25min,
            # needs KVM (run via nix-build-remote). Output:
            #   result/lcov.info   — combined, stripped to workspace paths
            #   result/html/       — genhtml report
            #   result/per-test/   — vm-<scenario>.lcov individual breakdowns
            coverage-full = coverage.full;
            # cov-smoke: fast (~5min) one-scenario coverage-infra
            # smoke. Also in .#ci (blocking). Manual run for
            # debugging: `nix build .#cov-smoke && cat result/summary`.
            cov-smoke = coverage.smoke;
            # Same data as coverage-full, HTML-only output at result/
            # (no lcov.info / per-test subdirs). Mirrors coverage-html's
            # relationship to the unit-test coverage check.
            coverage-full-html = pkgs.runCommand "rio-coverage-full-html" { } ''
              ln -s ${coverage.full}/html $out
            '';
            # VM-only combined (no unit-test merge). Debugging.
            coverage-vm = coverage.vmLcov;
          }
          # Per-test lcovs: coverage-vm-<scenario> etc. Useful for
          # "why is X not covered" — inspect one VM test's
          # contribution in isolation. `or {}`: coverage is
          # optionalAttrs isLinux → empty on Darwin → no attr error.
          // prefixed "coverage-" (coverage.perTestLcov or { })
          # Coverage-mode VM test runs: cov-vm-<scenario> etc. Build
          # one to get the raw profraws at result/coverage/<node>/.
          # Used during smoke debugging.
          // prefixed "cov-" vmTestsCov
          // {
            # HTML coverage report from the unit-test lcov.
            # crateChecks.coverage already emits repo-relative paths
            # (`rio-*/src/...`) — no strip needed, just genhtml.
            coverage-html = pkgs.runCommand "rio-coverage-html" { } ''
              cd ${commonSrc}
              ${pkgs.lcov}/bin/genhtml ${crateChecks.coverage}/lcov.info \
                --output-directory $out
            '';
            inherit ci;
          }
          # Per-member crate2nix derivations. Keys are the crate
          # names (rio-scheduler, rio-common, ...). See
          # .claude/notes/crate2nix-migration-assessment.md.
          // crateBuild.members
          # Per-member check derivations for targeted runs:
          #   nix build .#clippy-rio-scheduler
          #   nix build .#test-rio-common
          #   nix build .#doc-rio-nix
          // prefixed "clippy-" crateChecks.clippy
          // prefixed "clippy-test-" crateChecks.clippyTest
          // prefixed "test-" crateChecks.tests
          // prefixed "test-bin-" crateChecks.testBins
          // prefixed "doc-" crateChecks.doc
          // prefixed "cov-profraw-" crateChecks.covProfraw
          // {
            # Raw symlinkJoin of all built crate outputs. References
            # the intermediate .rlib tree — use workspace-bins for
            # docker/VM tests.
            inherit (crateBuild) workspace;
            # Stripped binary-only variant — what VM tests/docker
            # consume. Closure ~glibc+syslibs.
            workspace-bins = crateBuild.workspaceBins;
            # crate2nix CLI for the dev shell (`crate2nix generate
            # --format json -o Cargo.json` regenerates after lockfile
            # changes).
            crate2nix-cli = crate2nixCli;
            # Aggregate check derivations (same as checks.* but
            # exposed as packages for --print-out-paths convenience).
            clippy-all = crateChecks.clippyCheck;
            test-all = crateChecks.testCheck;
            doc-all = crateChecks.docCheck;
            # nextest reuse-build runner — characteristic
            # `PASS [Xs] crate::test` output, test groups, retries.
            # Binaries synthesized from crate2nix testBinDrvs, no
            # cargo invocation. nextest-meta is the cached metadata
            # derivation for debugging / manual `cargo-nextest run
            # --binaries-metadata result/binaries-metadata.json`.
            nextest-all = crateChecks.nextest;
            nextest-meta = crateChecks.nextestMetadata;
            # Coverage output (lcov.info at $out/lcov.info).
            inherit (crateChecks) coverage;
            # Toolchain wrappers for debugging the arg-filtering:
            #   nix build .#clippy-rustc
            #   ./result/bin/rustc --version   # → clippy version
            clippy-rustc = crateChecks.clippyRustc;
            rustdoc-rustc = crateChecks.rustdocRustc;
            # Instrumented workspace (symlinkJoin). Inspection:
            #   objdump -h result/bin/rio-store | grep llvm_prf
            workspace-cov = crateBuildCov.workspace;
          }
          # Per-test VM packages (Linux-only — mkVmTests wraps in
          # optionalAttrs isLinux):
          #   nix build .#vm-protocol-warm-standalone
          #   nix build .#cov-vm-lifecycle-core-k3s
          // vmTests
          # Multi-Nix golden matrix (weekly). Exported Linux-only:
          # nix-daemon needs a unix socket; macOS matrix not supported.
          # Under `packages` not `checks` → `nix flake check` won't
          # build the three extra Nix source trees on every push.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            golden-matrix = goldenMatrix;
            inherit mutants mutants-smoke;
          };

          # --------------------------------------------------------------
          # Apps (nix run .#<name>)
          # --------------------------------------------------------------
          apps.bench = {
            type = "app";
            # `nix run .#bench` → `cargo bench -p rio-bench` with the dev
            # shell's env. Runs against $PWD (the user's checkout), NOT
            # against ${self}: cargo bench writes target/criterion/ and
            # needs a mutable working tree. ${self} is a /nix/store copy
            # — read-only, no target/, and no live Cargo.lock.
            #
            # Stable toolchain (CI parity) — nightly-only code in a
            # bench would pass here but fail nix flake check.
            #
            # Forwards "$@" so `nix run .#bench -- --bench submit_build`
            # and criterion flags like `-- --test` work.
            program = pkgs.lib.getExe (
              pkgs.writeShellApplication {
                name = "rio-bench";
                # pkg-config + fuse3 + protobuf: rio-scheduler's
                # transitive deps compile under cargo bench (fresh
                # target/ profile). PG_BIN: rio-test-support's
                # ephemeral postgres bootstrap.
                runtimeInputs = [
                  rustStable
                  pkgs.pkg-config
                  pkgs.protobuf
                  pkgs.fuse3
                  pkgs.postgresql_18
                ];
                text = ''
                  export PROTOC=${pkgs.protobuf}/bin/protoc
                  export LIBCLANG_PATH=${pkgs.llvmPackages.libclang.lib}/lib
                  export PG_BIN=${pkgs.postgresql_18}/bin
                  exec cargo bench -p rio-bench "$@"
                '';
              }
            );
          };

          # --------------------------------------------------------------
          # Checks (run with 'nix flake check')
          # --------------------------------------------------------------
          checks = {
            build = rio-workspace;
            clippy = crateChecks.clippyCheck;
            doc = crateChecks.docCheck;
            inherit (crateChecks) nextest coverage;
            dashboard = rioDashboard;
          }
          // miscChecks
          # 2min fuzz runs (Linux-only). Compiled binaries shared
          # across targets via rio-{nix,store}-fuzz-build.
          // fuzz.runs
          # Per-phase milestone VM tests (Linux-only, need KVM).
          # Debug interactively:
          #   nix build .#checks.x86_64-linux.vm-protocol-warm-standalone.driverInteractive
          #   ./result/bin/nixos-test-driver
          // vmTests
          # Eval-time assertion: codecov.yml after_n_builds must equal the
          # coverage matrix length. Catches drift when vm-* fragments are
          # added without bumping the Codecov gate. See
          # docs/src/remediations/phase4a/05-coverage-session-drop.md.
          # Linux-only because githubActions is optionalAttrs isLinux.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            codecov-matrix-sync =
              let
                expected = builtins.length (builtins.attrNames githubActions.matrix.coverage);
                declared = pkgs.lib.toInt (
                  builtins.head (
                    builtins.match ".*after_n_builds: ([0-9]+).*" (builtins.readFile ./.github/codecov.yml)
                  )
                );
              in
              assert pkgs.lib.assertMsg (expected == declared) ''
                .github/codecov.yml after_n_builds=${toString declared} but coverage matrix has ${toString expected} entries.
                Update .github/codecov.yml → codecov.notify.after_n_builds to ${toString expected}.
              '';
              # Named (not pkgs.emptyFile) so `ls result/` of .#ci shows
              # which constituent this is — eval-time asserts are invisible
              # otherwise once they pass.
              pkgs.runCommand "rio-codecov-matrix-sync" { } "touch $out";
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}
