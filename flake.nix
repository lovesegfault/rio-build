{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.34.1";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    # Spec-coverage tool (nix/tracey.nix). Flake input (not fetchFromGitHub)
    # so importCargoLock reads Cargo.lock from a pre-fetched path — no IFD.
    tracey-src = {
      url = "github:bearcove/tracey/2446b4f7433c6220c18737737970f6eccbe2081d";
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
        worker = ./nix/modules/worker.nix;
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
          c2nWorkspaceFileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./rio-cli
            ./rio-common
            ./rio-controller
            ./rio-crds
            ./rio-gateway
            ./rio-nix/src
            ./rio-nix/Cargo.toml
            ./rio-proto
            ./rio-scheduler
            ./rio-store/src
            ./rio-store/tests
            ./rio-store/Cargo.toml
            ./rio-test-support
            ./rio-worker
            ./migrations
            # Seccomp profile JSON (embedded via include_str! in
            # rio-controller tests — build-time presence check so a
            # missing profile fails compile, not silently at deploy).
            ./infra/helm/rio-build/files
          ];
          c2nWorkspaceSrc = pkgs.lib.fileset.toSource {
            root = unfilteredRoot;
            fileset = c2nWorkspaceFileset;
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

          # Workspace binaries (crate2nix per-crate build, stripped +
          # patchelf'd closure-shrink in nix/crate2nix.nix). What
          # docker images, VM tests, worker-vm, and crdgen consume.
          rio-workspace = c2n.workspaceBins;

          # Coverage-instrumented workspace. crate2nix parallel tree
          # with globalExtraRustcOpts=["-Cinstrument-coverage"]. Used
          # by vmTestsCov + nix/coverage.nix. Closure-scrubbed via
          # patchelf+remove-references-to but NOT stripped — stripping
          # removes the __llvm_covfun/__llvm_covmap sections llvm-cov
          # needs. Previously used raw c2nCov.workspace (2.3GB closure
          # via dead rust-toolchain RPATH) → k3s containerd tmpfs
          # ENOSPC. workspaceBinsCov collapses the closure to glibc+
          # syslibs while keeping the instrumentation sections.
          rio-workspace-cov = c2nCov.workspaceBinsCov;

          # Source tree for genhtml (nix/coverage.nix cd's here so
          # genhtml can resolve repo-relative lcov paths to source
          # lines). c2nWorkspaceSrc has all rio-*/src/.
          commonSrc = c2nWorkspaceSrc;

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
              c2nWorkspaceFileset
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
          # crate2nix JSON-mode PoC
          # ──────────────────────────────────────────────────────────
          #
          # Parallel build pipeline using pkgs.buildRustCrate + a
          # pre-resolved Cargo.json. See nix/crate2nix.nix and
          # .claude/notes/crate2nix-migration-assessment.md for the
          # rationale and caveats. Exposed below as
          # packages.c2n-workspace + packages.c2n-rio-<crate>.
          mkC2n =
            extra:
            import ./nix/crate2nix.nix (
              {
                inherit pkgs rustStable sysCrateEnv;
                inherit (pkgs) lib;
                crate2nixSrc = inputs.crate2nix;
                workspaceSrc = c2nWorkspaceSrc;
              }
              // extra
            );
          c2n = mkC2n { };

          # Coverage-instrumented tree: re-import with
          # globalExtraRustcOpts=["-Cinstrument-coverage"]. Doubles the
          # derivation count (645 normal + 645 instrumented), but each
          # half caches independently — touching a workspace crate only
          # rebuilds that crate's two variants + dependents.
          c2nCov = mkC2n { globalExtraRustcOpts = [ "-Cinstrument-coverage" ]; };

          # ──────────────────────────────────────────────────────────
          # crate2nix check backends: clippy, tests, doc
          # ──────────────────────────────────────────────────────────
          #
          # Per-crate checks layered on the c2n build graph. Deps are
          # built once (regular rustc, 645 cached drvs); workspace
          # members are rebuilt per-check with the appropriate driver
          # (clippy-driver, rustc --test, rustdoc). See
          # nix/c2n-checks.nix for the wrapper mechanics — notably the
          # clippy wrapper strips lib.sh's hardcoded `--cap-lints
          # allow` (which rustc treats as non-overridable) before
          # forwarding to clippy-driver.
          #
          # These are the crate2nix-native replacements for crane's
          # cargoClippy/cargoNextest/cargoDoc. Unlike crane's
          # workspace-wide derivations, each workspace member gets its
          # own check derivation → touching rio-scheduler only
          # re-clippy's rio-scheduler + its dependents, not the full
          # workspace.
          #
          # Exposed below as checks.c2n-* and packages.c2n-clippy-* /
          # c2n-test-* / c2n-doc-* for targeted invocation.
          c2nChecks = import ./nix/c2n-checks.nix {
            inherit
              pkgs
              rustStable
              c2n
              c2nCov
              ;
            inherit (pkgs) lib;
            # Runtime inputs for test execution. Mirrors crane's
            # cargoNextest nativeCheckInputs — postgres for ephemeral
            # PG bootstrap (rio-test-support), nix-cli for golden
            # conformance tests (nix-store --dump, nix-instantiate),
            # openssh for rio-gateway SSH accept tests.
            runtimeTestInputs = with pkgs; [
              inputs.nix.packages.${system}.nix-cli
              openssh
              postgresql_18
            ];
            # Env vars for test runners. PG_BIN so rio-test-support
            # finds initdb/postgres; RIO_GOLDEN_* so golden tests
            # don't try to `nix build` their fixture in-sandbox.
            testEnv = {
              PG_BIN = "${pkgs.postgresql_18}/bin";
              RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
              RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
              RIO_GOLDEN_FORCE_HERMETIC = "1";
            };
            # nextest reuse-build runner. Synthesizes --cargo-metadata
            # and --binaries-metadata JSON from the crate2nix test
            # binaries; runs with the `ci` profile (retries, test
            # groups from .config/nextest.toml). Per-test-process
            # isolation — no PDEATHSIG/libtest thread race, so
            # wrapper-level PG bootstrap not needed. `--no-tests=warn`
            # because rio-cli has zero tests (bin-only crate).
            #
            # Fileset = c2n's workspaceSrc PLUS .config/nextest.toml
            # (--workspace-remap needs to find it relative to the
            # workspace root). c2n's own fileset omits .config/
            # because buildRustCrate doesn't need it.
            workspaceSrc = pkgs.lib.fileset.toSource {
              root = unfilteredRoot;
              fileset = pkgs.lib.fileset.unions [
                c2nWorkspaceFileset
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
          # sandboxes (nixbuild.net), `nix eval`/`nix build` fail because
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

          # --------------------------------------------------------------
          # Non-rustc check derivations (shared by checks.* and ci aggregate)
          # --------------------------------------------------------------
          #
          # Rust checks (clippy/nextest/doc/coverage) are in c2nChecks
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
                  c2nWorkspaceFileset
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
            tracey-validate =
              pkgs.runCommand "rio-tracey-validate"
                {
                  src = pkgs.lib.cleanSource ./.;
                  nativeBuildInputs = [ traceyPkg ];
                }
                ''
                  cp -r $src $TMPDIR/work
                  chmod -R +w $TMPDIR/work
                  cd $TMPDIR/work
                  rm -rf .tracey/
                  export HOME=$TMPDIR
                  set -o pipefail
                  tracey query validate 2>&1 | tee $out
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
                  nativeBuildInputs = [ pkgs.kubernetes-helm ];
                }
                ''
                  cp -r ${chart} $TMPDIR/chart
                  chmod -R +w $TMPDIR/chart
                  cd $TMPDIR/chart
                  mkdir -p charts
                  ln -s ${subcharts.postgresql} charts/postgresql
                  helm lint .
                  helm template rio . --set global.image.tag=test > /dev/null
                  helm template rio . -f values/dev.yaml > /dev/null
                  helm template rio . -f values/vmtest-full.yaml > /dev/null
                  touch $out
                '';
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
              }
            );
          dockerImages = mkDockerImages { inherit rio-workspace; };

          # Subcharts from nixhelm (FODs — hash-pinned `helm pull`).
          # Referenced by: helm-lint check (symlinked into charts/ in-sandbox),
          # packages.helm-* (`just eks deploy` / `just dev apply` symlink from
          # the result path into the working-tree charts/ — gitignored).
          subcharts = import ./nix/helm-charts.nix {
            inherit (inputs) nixhelm;
            inherit system;
          };

          # --------------------------------------------------------------
          # Scenario×fixture VM tests (Linux-only — need NixOS VMs + KVM)
          # --------------------------------------------------------------
          #
          #   vm-protocol-{warm,cold}-standalone — 3 VMs: opcode coverage
          #   vm-scheduling-{core,disrupt}-standalone — 5 VMs: fanout, size-class, cgroup
          #   vm-security-standalone — 3 VMs: mTLS, HMAC, tenant-resolve
          #   vm-observability-standalone — 5 VMs: metrics, traces, logs
          #   vm-lifecycle-{core,ctrlrestart,recovery,reconnect,autoscale}-k3s
          #   vm-le-{stability,build}-k3s — 2-node k3s fixture (fragment splits)
          #
          # mkVmTests: build the attrset for a given (workspace,
          # dockerImages, coverage) triple. vmTests uses the normal
          # build; vmTestsCov uses the instrumented build + coverage=
          # true so common.nix sets LLVM_PROFILE_FILE and appends
          # collectCoverage to each testScript.

          # Request a minimum CPU allocation from nixbuild.net. Each
          # VM has `virtualisation.cores = 4` in common.nix; without
          # this, nixbuild.net's heuristic allocation can under-provision
          # (vm-phase2a once got 5 CPUs for 4 VMs → 16 vCPUs on 5
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
              # oversubscription → TCG fallback → qemu stall. Default 4
              # for anything not in the table.
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
                # 5 VMs: control + worker1/2/3 + client.
                vm-observability-standalone = 5;
                # 3 VMs but k3s-server is 8-core 6GB + k3s-agent 8-core 4GB.
                # All lifecycle + leader-election splits boot the same
                # 2-node k3s fixture.
                vm-lifecycle-core-k3s = 8;
                vm-lifecycle-recovery-k3s = 8;
                vm-lifecycle-autoscale-k3s = 8;
                vm-le-stability-k3s = 8;
                vm-le-build-k3s = 8;
                # k3s + one extra airgap image (squid). Does builds.
                vm-fod-proxy-k3s = 8;
              };
            in
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              pkgs.lib.mapAttrs (name: withMinCpu (cpuHints.${name} or 4)) allTests
            );

          vmTests = mkVmTests {
            inherit rio-workspace dockerImages;
            coverage = false;
          };

          # Coverage-mode VM tests. Not in `checks` (too slow for flake
          # check) — exposed as packages.cov-vm-phaseXY for manual runs
          # + consumed by nix/coverage.nix for the merged lcov.
          vmTestsCov = mkVmTests {
            rio-workspace = rio-workspace-cov;
            dockerImages = mkDockerImages {
              rio-workspace = rio-workspace-cov;
              coverage = true;
            };
            coverage = true;
          };

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
              unitCoverage = c2nChecks.coverage;
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
          # All VM tests (including k3s scenarios) + 2min fuzz. On
          # non-Linux, degrades to Rust checks + pre-commit only (VM
          # tests and fuzz are both optionalAttrs isLinux upstream).
          # Linux+KVM required for the full set.
          ciBaseDrvs = [
            rio-workspace
            c2nChecks.clippyCheck
            c2nChecks.nextest
            c2nChecks.docCheck
            c2nChecks.coverage
            miscChecks.deny
            miscChecks.tracey-validate
            miscChecks.helm-lint
            rioDashboard
            config.checks.pre-commit
          ];

          ci = pkgs.linkFarmFromDrvs "rio-ci" (
            ciBaseDrvs
            ++ builtins.attrValues fuzz.runs
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux (builtins.attrValues vmTests)
          );

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
                clippy = c2nChecks.clippyCheck;
                doc = c2nChecks.docCheck;
                inherit (c2nChecks) nextest;
                inherit (miscChecks) deny tracey-validate helm-lint;
                inherit (config.checks) pre-commit;
                dashboard = rioDashboard;
              };
              # 2min fuzz runs, one matrix entry per target. Keys are
              # fuzz-<target> (from nix/fuzz.nix). On a cold cache each
              # entry rebuilds the shared fuzz-build derivation, but
              # spot CPU is cheap and the cache fills after first green.
              fuzz = fuzz.runs;
              # Normal VM tests. Keys: vm-phase1a etc. Per-test
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
              end-of-file-fixer.enable = true;
              trim-trailing-whitespace.enable = true;
              deadnix.enable = true;
              nil.enable = true;
              statix.enable = true;

              # No kubeconform hook: it fetches ~300MB of schemas from
              # raw.githubusercontent.com at runtime, which fails in the
              # nixbuild.net sandbox (config.checks.pre-commit runs all
              # hooks there). Run it interactively if needed:
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
                skopeo # nix build .#docker-* | skopeo copy docker-archive:... docker://ECR
                kubernetes-helm
                kubeconform # ad-hoc schema validation (no pre-commit hook — fetches 300MB, sandbox blocks)
                yq-go # scripts/split-crds.sh + nix/helm-render.nix
                grpcurl # manual AdminService poking when rio-cli isn't enough
                jq # push-images.sh parses terraform output + smoke-test.sh
                openssl # openssl rand 32 → HMAC key
                git # push-images.sh dirty-tree check + rev-parse for image tag
                just # deploy workflow recipes (see justfile at repo root)

                # Python env for .claude/lib/ + co-located skill scripts —
                # pydantic models are the agent-boundary contracts (each
                # script has `--schema` to print JSON Schema).
                (python3.withPackages (ps: [
                  ps.pydantic
                  ps.pytest
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
                    RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
                    shellHook = config.pre-commit.installationScript;
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
            # Helm charts from nixhelm (unpacked dirs). `just dev apply` and
            # `just eks deploy` build these and symlink/install from the
            # result path. PG must be in charts/ even when
            # condition: postgresql.enabled is false — Helm validates
            # charts/ against Chart.yaml BEFORE evaluating conditions.
            helm-postgresql = subcharts.postgresql;
            helm-rook-ceph = subcharts.rook-ceph;
            helm-rook-ceph-cluster = subcharts.rook-ceph-cluster;
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
            docker-worker = dockerImages.worker;
            docker-controller = dockerImages.controller;
            docker-fod-proxy = dockerImages.fod-proxy;
            docker-bootstrap = dockerImages.bootstrap;
            docker-all = dockerImages.all;
            dockerImages = pkgs.linkFarm "rio-docker-images" (
              pkgs.lib.mapAttrsToList (name: drv: {
                name = "${name}.tar.zst";
                path = drv;
              }) dockerImages
            );

            # Dev worker VM (QEMU + NixOS). Reuses nix/modules/worker.nix
            # with SLiRP networking to reach the host's control plane.
            # Run: result-worker-vm/bin/run-rio-worker-dev-vm
            worker-vm =
              (nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  ./nix/dev-worker-vm.nix
                  { services.rio.package = rio-workspace; }
                ];
              }).config.system.build.vm;

            # CRD YAML for kustomize. runCommand invokes the crdgen
            # binary (serde_yaml write-only) and dumps two YAML
            # documents (WorkerPool + Build) to $out. Kustomize
            # references this via `nix build .#crds` → result is a
            # file; ./scripts/split-crds.sh result splits it into
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
            # coverage-full: unit + all 7 VM tests merged. ~25min,
            # needs KVM (run via nix-build-remote). Output:
            #   result/lcov.info   — combined, stripped to workspace paths
            #   result/html/       — genhtml report
            #   result/per-test/   — vm-phase*.lcov individual breakdowns
            coverage-full = coverage.full;
            # Same data as coverage-full, HTML-only output at result/
            # (no lcov.info / per-test subdirs). Mirrors coverage-html's
            # relationship to the unit-test coverage check.
            coverage-full-html = pkgs.runCommand "rio-coverage-full-html" { } ''
              ln -s ${coverage.full}/html $out
            '';
            # VM-only combined (no unit-test merge). Debugging.
            coverage-vm = coverage.vmLcov;
          }
          # Per-test lcovs: coverage-vm-phase1a etc. Useful for
          # "why is X not covered" — inspect one VM test's
          # contribution in isolation. `or {}`: coverage is
          # optionalAttrs isLinux → empty on Darwin → no attr error.
          // prefixed "coverage-" (coverage.perTestLcov or { })
          # Coverage-mode VM test runs: cov-vm-phase1a etc. Build
          # one to get the raw profraws at result/coverage/<node>/.
          # Used during smoke debugging.
          // prefixed "cov-" vmTestsCov
          // {
            # HTML coverage report from the unit-test lcov.
            # c2nChecks.coverage already emits repo-relative paths
            # (`rio-*/src/...`) — no strip needed, just genhtml.
            coverage-html = pkgs.runCommand "rio-coverage-html" { } ''
              cd ${commonSrc}
              ${pkgs.lcov}/bin/genhtml ${c2nChecks.coverage}/lcov.info \
                --output-directory $out
            '';
            inherit ci;
            # Backward-compat aliases for scripts that target the old
            # c2n-suffixed names.
            ci-c2n = ci;
            coverage-full-c2n = coverage.full;
            coverage-vm-c2n = coverage.vmLcov;
          }
          # Per-member crate2nix derivations. Prefixed c2n- so they
          # sort together in `nix flake show`. See
          # .claude/notes/crate2nix-migration-assessment.md.
          // prefixed "c2n-" c2n.members
          # Per-member check derivations for targeted runs:
          #   nix build .#c2n-clippy-rio-scheduler
          #   nix build .#c2n-test-rio-common
          #   nix build .#c2n-doc-rio-nix
          // prefixed "c2n-clippy-" c2nChecks.clippy
          // prefixed "c2n-clippy-test-" c2nChecks.clippyTest
          // prefixed "c2n-test-" c2nChecks.tests
          // prefixed "c2n-test-bin-" c2nChecks.testBins
          // prefixed "c2n-doc-" c2nChecks.doc
          // prefixed "c2n-cov-profraw-" c2nChecks.covProfraw
          // {
            c2n-workspace = c2n.workspace;
            # Stripped binary-only variant — what VM tests/docker
            # consume. ~300MB closure vs c2n-workspace's ~2.3GB
            # (buildRustCrate's default out references the full
            # rust toolchain via debug-info strings).
            c2n-workspace-bins = c2n.workspaceBins;
            # crate2nix CLI for the dev shell (`crate2nix generate
            # --format json -o Cargo.json` regenerates after lockfile
            # changes).
            crate2nix-cli = crate2nixCli;
            # Aggregate check derivations (same as checks.c2n-* but
            # exposed as packages for --print-out-paths convenience).
            c2n-clippy-all = c2nChecks.clippyCheck;
            c2n-test-all = c2nChecks.testCheck;
            c2n-doc-all = c2nChecks.docCheck;
            # nextest reuse-build runner — characteristic
            # `PASS [Xs] crate::test` output, test groups, retries.
            # Binaries synthesized from c2n testBinDrvs, no cargo
            # invocation. nextest-meta is the cached metadata
            # derivation for debugging / manual `cargo-nextest run
            # --binaries-metadata result/binaries-metadata.json`.
            c2n-nextest-all = c2nChecks.nextest;
            c2n-nextest-meta = c2nChecks.nextestMetadata;
            # Coverage output (lcov.info at $out/lcov.info).
            c2n-coverage = c2nChecks.coverage;
            # Toolchain wrappers for debugging the arg-filtering:
            #   nix build .#c2n-clippy-rustc
            #   ./result/bin/rustc --version   # → clippy version
            c2n-clippy-rustc = c2nChecks.clippyRustc;
            c2n-rustdoc-rustc = c2nChecks.rustdocRustc;
            # Instrumented workspace (symlinkJoin). Inspection:
            #   objdump -h result/bin/rio-store | grep llvm_prf
            c2n-workspace-cov = c2nCov.workspace;
          }
          # Per-test VM packages (Linux-only — mkVmTests wraps in
          # optionalAttrs isLinux):
          #   nix build .#vm-protocol-warm-standalone
          #   nix build .#cov-vm-lifecycle-core-k3s
          # Plus c2n- prefixed aliases for backward-compat.
          // vmTests
          // prefixed "c2n-" vmTests
          // prefixed "c2n-cov-" vmTestsCov
          // prefixed "c2n-coverage-" (coverage.perTestLcov or { })
          # KVM race validation probes (nix/tests/kvm-probe.nix).
          # Not in .#ci — run manually to compare concurrent vs
          # staggered VM start. Hypothesis: start_all() races qemu
          # KVM_init, some VMs fall to TCG.
          #   nix build -L .#kvm-probe-concurrent .#kvm-probe-staggered
          #   grep "KVM-PROBE" <log> | sort | uniq -c
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            pkgs.lib.mapAttrs (_: withMinCpu 5) (import ./nix/tests/kvm-probe.nix { inherit pkgs; })
          );

          # --------------------------------------------------------------
          # Checks (run with 'nix flake check')
          # --------------------------------------------------------------
          checks = {
            build = rio-workspace;
            clippy = c2nChecks.clippyCheck;
            doc = c2nChecks.docCheck;
            inherit (c2nChecks) nextest coverage;
            dashboard = rioDashboard;
          }
          // miscChecks
          # 2min fuzz runs (Linux-only). Compiled binaries shared
          # across targets via rio-{nix,store}-fuzz-build.
          // fuzz.runs
          # Per-phase milestone VM tests (Linux-only, need KVM).
          # Debug interactively:
          #   nix build .#checks.x86_64-linux.vm-phase2a.driverInteractive
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
              pkgs.emptyFile;
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}
