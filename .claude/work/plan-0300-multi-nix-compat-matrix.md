# Plan 0300: Multi-Nix compatibility matrix — 2.20+ / unstable / Lix golden conformance

Originates from [`verification.md:10`](../../docs/src/verification.md) — the golden conformance tests currently run against whatever single `nix-daemon` the `inputs.nix` flake input pins. That's one point on a line. Protocol compat matters because (a) Nix's worker protocol version negotiation has real bugs across versions (`PROTOCOL_VERSION` bumps don't always gate new fields correctly), and (b) [Lix](https://lix.systems) diverges — it's a fork with its own opcode additions and serialization tweaks.

Current state: [`rio-gateway/tests/golden/daemon.rs:753`](../../rio-gateway/tests/golden/daemon.rs) spawns `nix-daemon` from `$PATH`. The `nativeCheckInputs` in [`flake.nix:349`](../../flake.nix) provides exactly one: `inputs.nix.packages.${system}.nix-cli`. One daemon binary → one protocol surface tested.

**The shape:** parameterize the daemon spawn over a set of daemon binaries. flake.nix gains additional nix-version inputs. Conformance tests become a matrix: each `test_wop*` runs N times (once per daemon). Expected divergences (Lix version string, Lix-specific opcodes) are encoded as per-daemon skip-lists — the same mechanism already used for `version_string` / `trusted` fields.

**Weekly tier:** the matrix multiplies test time by N (daemon count). CI's per-push `.#ci` keeps the single pinned daemon; the matrix run goes in the weekly scheduled tier ([`verification.md:90`](../../docs/src/verification.md)).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(nix):` additional nix-version flake inputs

MODIFY [`flake.nix`](../../flake.nix) — add inputs alongside the existing `nix` (currently `:5`):

```nix
inputs = {
  # ... existing
  nix = { ... };                     # current pin, stays the CI default
  nix-stable = {
    url = "github:NixOS/nix/2.20-maintenance";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  nix-unstable = {
    url = "github:NixOS/nix";        # master HEAD — surfaces breakage early
    inputs.nixpkgs.follows = "nixpkgs";
  };
  lix = {
    url = "git+https://git.lix.systems/lix-project/lix";
    inputs.nixpkgs.follows = "nixpkgs";
    flake = true;
  };
};
```

Check at impl: Lix may use `nixpkgs-stable` or a different follow pattern — their flake.nix is the source of truth. `inputs.nixpkgs.follows` avoids duplicate nixpkgs evaluations but may cause Lix build failures if they pin a specific nixpkgs — in that case drop the `follows` for Lix only and accept the eval cost.

### T2 — `feat(nix):` `.#checks.golden-matrix` derivation

NEW `nix/golden-matrix.nix` (or inline in flake.nix checks) — a derivation that runs `cargo nextest run --test golden_conformance` once per daemon, with `RIO_GOLDEN_DAEMON_BIN` pointing at each:

```nix
golden-matrix = let
  daemons = {
    nix-pinned = "${inputs.nix.packages.${system}.nix-cli}/bin/nix-daemon";
    nix-stable = "${inputs.nix-stable.packages.${system}.nix-cli}/bin/nix-daemon";
    nix-unstable = "${inputs.nix-unstable.packages.${system}.nix-cli}/bin/nix-daemon";
    lix = "${inputs.lix.packages.${system}.nix-cli}/bin/nix-daemon";
    # Lix may name its daemon differently (lix-daemon?). Check.
  };
  mkMatrixRun = name: daemonBin: craneLib.cargoNextest (commonArgs // {
    pname = "rio-golden-${name}";
    cargoNextestExtraArgs = "--test golden_conformance --profile ci";
    RIO_GOLDEN_DAEMON_BIN = daemonBin;
    RIO_GOLDEN_DAEMON_VARIANT = name;  # For per-variant skip-lists (T3)
    # ... other RIO_GOLDEN_* envs from commonArgs
  });
in pkgs.linkFarm "golden-matrix" (
  pkgs.lib.mapAttrsToList (n: d: { name = n; path = mkMatrixRun n d; }) daemons
);
```

**NOT in `.#ci`.** Add a separate `.#checks.golden-matrix` attr that `nix flake check` SKIPS by default (use `# skip-ci` comment convention or put it under `packages.` instead of `checks.`). Weekly cron invokes `nix-build-remote -- .#golden-matrix`.

### T3 — `test:` parameterize daemon spawn + per-variant skip-lists

MODIFY [`rio-gateway/tests/golden/daemon.rs`](../../rio-gateway/tests/golden/daemon.rs). Currently `:753` hardcodes `"nix-daemon"`. Parameterize:

```rust
/// Daemon binary path. Defaults to `nix-daemon` in PATH (dev shell,
/// single-version CI). Matrix runs set RIO_GOLDEN_DAEMON_BIN to a
/// full store path.
fn daemon_bin() -> String {
    std::env::var("RIO_GOLDEN_DAEMON_BIN")
        .unwrap_or_else(|_| "nix-daemon".into())
}

/// Variant label for per-daemon skip-lists. "nix-pinned" (default),
/// "nix-stable", "nix-unstable", "lix".
fn daemon_variant() -> &'static str {
    match std::env::var("RIO_GOLDEN_DAEMON_VARIANT").as_deref() {
        Ok("lix") => "lix",
        Ok("nix-stable") => "nix-stable",
        Ok("nix-unstable") => "nix-unstable",
        _ => "nix-pinned",
    }
}
```

Extend the existing skip-list mechanism (currently handles `version_string` / `trusted`) with per-variant entries. A `static SKIP_MATRIX: &[(&str, &str, &str)]` of `(variant, test_name, reason)` — iteration in the test harness skips at runtime based on `daemon_variant()`.

Known Lix divergences to seed the skip-list (verify at impl by actually running against Lix):
- `version_string` — Lix reports `"Lix N.N.N"` not `"nix (Nix) N.N.N"`
- Any opcodes Lix adds that rio-build doesn't recognize → rio sends unknown-opcode error, Lix daemon doesn't — that's a *rio-against-lix-client* scenario though, not *rio-golden-against-lix-daemon*. The golden tests have rio as client. Should mostly pass.

### T4 — `docs:` document the matrix in verification.md

MODIFY [`verification.md`](../../docs/src/verification.md) — replace the `:10` `> **Scheduled:**` block with the actual matrix description. Add a row to the CI-tier table (`:87-90`): `| Weekly | ... | + golden-matrix (4 daemons) | .#golden-matrix | ... |`.

## Exit criteria

- `RIO_GOLDEN_DAEMON_BIN=<path> cargo nextest run --test golden_conformance` works against all 4 daemon binaries locally
- `nix build .#golden-matrix` produces a linkfarm with 4 per-daemon test outputs
- `nix flake check` does NOT run the matrix (only the pinned daemon — per-push CI unchanged)
- At least one Lix skip-list entry documented with the observed divergence (version string at minimum)
- Weekly GitHub Actions (or equivalent) workflow entry invokes `.#golden-matrix`

## Tracey

Test-infra only — no domain markers. The conformance tests already carry `r[verify gw.*]` markers; this plan parameterizes the test runner, it doesn't add new spec coverage.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T1: nix-stable/nix-unstable/lix inputs; T2: .#golden-matrix checks attr"},
  {"path": "nix/golden-matrix.nix", "action": "NEW", "note": "T2: matrix derivation (or inline in flake.nix)"},
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "MODIFY", "note": "T3: RIO_GOLDEN_DAEMON_BIN env + variant skip-list"},
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "T4: replace :10 Scheduled block, add weekly-tier row"}
]
```

```
flake.nix                # T1+T2: inputs + matrix check
nix/
└── golden-matrix.nix    # T2: matrix derivation (NEW)
rio-gateway/tests/golden/
└── daemon.rs            # T3: parameterized spawn
docs/src/
└── verification.md      # T4: document matrix
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "phase-cleanup: verification.md:10 user-decided deferral. Protocol compat matters; Lix diverges. Weekly tier — multiplies golden-test time by 4. flake.nix inputs section is prepend-additive."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — phase4b fan-out root.

**Conflicts with:**
- [`flake.nix`](../../flake.nix) count=28, UNIMPL=[221, 274, 275, 282, 295]. T1 touches the `inputs` block at top-of-file (`:5`ish) — P0221 adds `.#bench` app (different section); P0274 adds pnpm deps (different section). T2 adds a `checks.` attr — EOF-append adjacent to other checks. Low contention despite high count.
- [`daemon.rs`](../../rio-gateway/tests/golden/daemon.rs) — not in the high-collision set. Isolated test file.
- [`verification.md`](../../docs/src/verification.md) — P0268 (chaos/toxiproxy, `:83`), P0221 (criterion, `:92`) both modify Scheduled blocks. This plan's `:10` block is distant. The CI-tier table at `:87-90` is shared — additive row, should merge cleanly.
