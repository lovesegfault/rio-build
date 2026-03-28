# Plan 969256104: Upstream substitution — validation (VM test + integration)

[P969256103](plan-969256103-upstream-substitution-surface.md) surfaced the feature via CLI, gateway, and helm. This plan validates the full chain end-to-end: a NixOS VM test scenario that stands up a fake upstream cache, configures a tenant with that upstream, and verifies `cargo xtask k8s rsb p#hello` substitutes from it on a cold store. Plus a cross-tenant sig-visibility integration test.

**Fourth of four.** Feature is complete after this lands.

## Entry criteria

- [P969256103](plan-969256103-upstream-substitution-surface.md) merged — `rio-cli upstream` works, gateway wires `willSubstitute`, helm NetPol allows upstream egress

## Tasks

### T1 — `test(nix):` scenarios/substitute.nix — VM test with fake upstream cache

New scenario at [`nix/tests/scenarios/substitute.nix`](../../nix/tests/scenarios/substitute.nix) following the [`netpol.nix`](../../nix/tests/scenarios/netpol.nix) / [`protocol.nix`](../../nix/tests/scenarios/protocol.nix) shape. The scenario:

1. **Fake upstream node:** a second VM serving a static binary cache (nginx + pre-built `hello` narinfo/nar signed with a test key). Use [`nix/tests/lib/derivations.nix`](../../nix/tests/lib/derivations.nix) to generate the fixture NAR at build time.
2. **Cold store:** main rio-store VM starts with empty `narinfo` table.
3. **Configure upstream:** `rio-cli upstream add --tenant default --url http://upstream:8080 --trusted-key test-cache-1:... --sig-mode keep`
4. **Trigger substitution:** gateway `wopQueryMissing` for `hello` path → expects `willSubstitute` non-empty; then `GetPath` → expects the NAR bytes.
5. **Verify ingest:** `psql -c "SELECT store_path, signatures FROM narinfo WHERE store_path LIKE '%hello%'"` shows the row with the upstream's sig.

Subtests fragments: `substitute-cold-fetch`, `substitute-sig-mode-add` (same flow, `sig_mode=add`, verify both upstream AND rio sigs present), `substitute-cross-tenant-gate`.

### T2 — `test(nix):` default.nix — wire substitute scenario + tracey markers

At [`nix/tests/default.nix`](../../nix/tests/default.nix), wire the new scenario. Per [CLAUDE.md § VM-test `r[verify]` placement](../../CLAUDE.md): markers go at the `subtests = [...]` entries, NOT in the scenario file header:

```nix
substitute = mkScenario {
  scenario = ./scenarios/substitute.nix;
  fixture = "standalone";
  subtests = [
    # r[verify store.substitute.upstream]
    "substitute-cold-fetch"
    # r[verify store.substitute.sig-mode]
    "substitute-sig-mode-add"
    # r[verify store.substitute.tenant-sig-visibility]
    "substitute-cross-tenant-gate"
  ];
};
```

### T3 — `test(rio-store):` integration test — cross-tenant sig visibility

At [`rio-store/tests/`](../../rio-store/tests/), new `substitute_visibility.rs` integration test using [`rio-test-support::TestDb`](../../rio-test-support/src/lib.rs):

1. Seed tenant A with upstream U trusting key K1; tenant B trusting K1; tenant C trusting only K2.
2. Mock `Substituter::try_substitute` (or use a wiremock upstream) to ingest path P signed by K1 under tenant A.
3. `query_path_info(tenant=B, P)` → `Ok(Some)` (B trusts K1).
4. `query_path_info(tenant=C, P)` → `NotFound` (C doesn't trust K1).
5. Add K1 to C's `trusted_keys`; re-query → now `Ok(Some)`.

This is the `r[verify store.substitute.tenant-sig-visibility]` unit-level coverage (VM test covers the same at system level).

## Exit criteria

- `/nixbuild .#ci` green including the new `vm-substitute-standalone` check
- `nix build .#checks.x86_64-linux.vm-substitute-standalone` passes all three subtests
- `cargo nextest run -p rio-store substitute_visibility` green
- `tracey query status` shows `store.substitute.*` rules with both `impl` AND `verify` coverage
- `tracey query untested | grep substitute` → empty

## Tracey

References existing markers (verification coverage):
- `r[store.substitute.upstream]` — T1, T2 verify (cold-fetch subtest)
- `r[store.substitute.sig-mode]` — T1, T2 verify (sig-mode-add subtest)
- `r[store.substitute.tenant-sig-visibility]` — T1, T2, T3 verify (cross-tenant gate, both VM and unit)

## Files

```json files
[
  {"path": "nix/tests/scenarios/substitute.nix", "action": "NEW", "note": "T1: VM scenario with fake upstream"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: wire scenario + r[verify] markers"},
  {"path": "rio-store/tests/substitute_visibility.rs", "action": "NEW", "note": "T3: cross-tenant sig gate integration test"}
]
```

```
nix/tests/
├── scenarios/substitute.nix   # T1: NEW
└── default.nix                # T2: subtests wiring
rio-store/tests/
└── substitute_visibility.rs   # T3: NEW
```

## Dependencies

```json deps
{"deps": [969256103], "soft_deps": [], "note": "validates the full chain; needs CLI + gateway + NetPol"}
```

**Depends on:** [P969256103](plan-969256103-upstream-substitution-surface.md) — `rio-cli upstream`, gateway `willSubstitute`, helm NetPol.

**Conflicts with:** [`nix/tests/default.nix`](../../nix/tests/default.nix) is the only shared file; other two are new. Low collision risk.
