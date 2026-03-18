# Plan 0075: cargo-deny license + advisory CI check

## Design

`dependencies.md:33` says GPL-3.0 is denied. Before this plan, nothing enforced that — a `cargo add` of a GPL crate would build fine. `cargo-deny` makes it a CI failure.

**`deny.toml`** (`d0e9de7`):
- `[licenses]`: explicit allowlist (MIT, Apache-2.0, BSD-*, ISC, MPL-2.0, etc). GPL-3.0 not in list → denied. `ring` gets a `clarify` block: its license is an OpenSSL+ISC+MIT combo that auto-detection can't classify confidently.
- `[advisories]`: RustSec DB, `yanked=deny`, `unmaintained=workspace`. **One accepted advisory**: RUSTSEC-2023-0071 (rsa Marvin Attack via russh). Exposure is nil — `rio-gateway` uses ed25519 host keys and ed25519 client auth exclusively (`load_or_generate_host_key` + `load_authorized_keys`). We never hold an RSA private key. The advisory concerns private-key timing leaks; not applicable. Full justification inline in `deny.toml` so future reviewers don't re-litigate.
- `[bans]`: `multiple-versions=warn` (AWS SDK legitimately pulls dual hyper 0.14/1.x during ecosystem migration), `wildcards=deny` with `allow-wildcard-paths` for workspace path deps.
- `[sources]`: only crates.io; git deps must be explicitly blessed.
- `[graph]`: pruned to 4 target platforms.

**Cargo.toml fixes found by cargo-deny** (`75d1c0c` prep + `d0e9de7`): all 8 crates get `publish=false`. These are application crates, not libraries — crates.io publication is not a goal. This also makes `allow-wildcard-paths` apply (it's a no-op for publishable crates, which was failing bans). `rio-test-support` was missing `license.workspace=true` — the only crate without it.

**CI wiring**: `craneLib.cargoDeny` in `cargoChecks` + `ciBaseDrvs`. Hermetic: advisory DB vendored at build time. `deny.toml` added to crane source fileset.

## Files

```json files
[
  {"path": "Cargo.toml", "action": "MODIFY", "note": "workspace publish=false, license.workspace=true"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "rio-nix/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "rio-proto/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "rio-scheduler/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "publish=false + license.workspace=true (was missing)"},
  {"path": "rio-worker/Cargo.toml", "action": "MODIFY", "note": "publish=false"},
  {"path": "flake.nix", "action": "MODIFY", "note": "craneLib.cargoDeny in cargoChecks + ciBaseDrvs; deny.toml in source fileset"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0056** (phase-2a terminal): all 8 crate manifests are 2a artifacts. Orthogonal to the rest of this phase.

## Exit

Merged as `75d1c0c..d0e9de7` (2 commits). `.#ci` green at merge; `cargo-deny` check passing with one documented accepted advisory.
