# Plan 521: cluster-key rotation — design/impl contradiction at sig-visibility gate

Bughunter finding (mc=70): [`sig_visibility_gate`](../../rio-store/src/grpc/mod.rs) at `:534` pushes `ts.cluster().trusted_key_entry()` — derives the pubkey from the **current** live `Signer` ([`signing.rs:242-246`](../../rio-store/src/signing.rs)). No history.

**Design says rotation preserves old signatures.** [`store.md:210-217`](../../docs/src/components/store.md) Key Rotation:
- Step 4: "Existing active paths are re-signed during GC (mark phase signs reachable paths with the new key)"
- Step 5: "After a grace period (default: 30 days), remove the old key from `trusted-public-keys`"

The tenant-key system keeps history rows (`tenant_keys` table, [`tenant_keys.rs:15-18`](../../rio-store/src/tenant_keys.rs) doc-comment); cluster key does **not**. The [P0478](plan-0478-sig-visibility-gate-cluster-key-union.md) `trusted.push(cluster().trusted_key_entry())` made the gate accept the cluster key — but only the ONE currently loaded.

**Blast radius:**

| Trigger | Effect |
|---|---|
| PutPath → scheduler window during rotation | Small — path is rio-signed with old key, gate derives new key, path invisible until scheduler populates `path_tenants` |
| `path_tenants.tenant_id ON DELETE CASCADE` ([`012_path_tenants.sql:17`](../../rio-store/migrations/012_path_tenants.sql)) | Larger — tenant deletion zeros `path_tenants` row count for paths that tenant built alone → gate re-fires on next read → old rio sig fails verification against new key → path goes invisible to other tenants who never had a `path_tenants` row |

**Compounding:** step 4 (GC mark re-signs reachable paths with new key) is **not implemented** — zero hits for `sign|Signer|trusted_key` in [`gc/mark.rs`](../../rio-store/src/gc/mark.rs). So the spec's self-healing mechanism doesn't exist, and the grace-period (step 5) is an unfulfilled promise.

**Two fix routes, ~50-100L either way.** discovered_from=478. origin=bughunter.

## Entry criteria

- [P0478](plan-0478-sig-visibility-gate-cluster-key-union.md) merged (introduced the `cluster().trusted_key_entry()` push at `:534`)
- **DECISION REQUIRED** before T2: Route I (cluster-key history) vs Route II (GC re-sign). See `## Design decision` below.

## Design decision

**Route I — `TenantSigner` holds `Vec<Signer>` for prior cluster keys:**

[`TenantSigner`](../../rio-store/src/signing.rs) at `:261` gets a `prior_cluster: Vec<Signer>` field. `sig_visibility_gate` unions all prior cluster pubkeys into `trusted`. Prior keys are loaded from a new `cluster_key_history` table (or a file-based keyring — whichever fits the deployment model).

- **Pro:** matches the tenant-key architecture; no GC coupling; grace period is "just don't delete the history row yet"
- **Con:** new migration, new config surface, `Vec<Signer>` load path through the stack
- **Spec impact:** step 4 becomes moot (no re-sign needed if old keys stay trusted); step 5 becomes "remove from `cluster_key_history` after grace period"

**Route II — implement the spec's GC re-sign + document grace-period caveat:**

GC mark phase re-signs each reachable path with the current cluster key, appending to `narinfo.signatures`. Paths signed with the old key gradually acquire new-key signatures; after grace period + one full GC cycle, all reachable paths have both.

- **Pro:** matches the written spec; self-healing; no new tables
- **Con:** GC mark becomes a write pass (currently read-only CTE + advisory lock per `r[store.gc.two-phase]`); every GC run re-signs every reachable path (idempotent but not free); the caveat "visibility gate may fail during grace period before GC runs" stays
- **Spec impact:** none (spec already says this)

**Recommendation: Route I.** The tenant-key system already proved the history-row pattern works. Route II changes GC's write profile significantly (currently mark is a ~1s CTE; re-signing N paths is N SIGNATURE + N UPDATE). Route I is a readpath-only change.

## Tasks

### T1 — `docs(store):` add `r[store.key.rotation-cluster-history]` marker + amend Key Rotation section

The Key Rotation section at [`store.md:210-217`](../../docs/src/components/store.md) has no tracey marker — it's prose-only. Add a marker and (if Route I chosen) amend step 4:

```markdown
r[store.key.rotation-cluster-history]
The cluster signing key MAY be rotated. Prior cluster public keys MUST remain in the trusted set for `sig_visibility_gate` verification until the grace period expires — otherwise paths signed under the old key become invisible to cross-tenant reads when `path_tenants` row count hits zero (CASCADE on tenant deletion). Prior keys are loaded from `cluster_key_history` alongside the active `Signer`.
```

**Run `tracey bump`** — this is NEW spec text, but the marker is new so no existing `r[impl]` goes stale. If Route I is chosen, also amend step 4 at `:215`:

```markdown
4. ~~Existing active paths are re-signed during GC~~ Prior cluster public keys stay in the trusted set; no re-sign needed while `cluster_key_history` retains the old pubkey
```

### T2 — `feat(store):` Route-dependent implementation

**Route I:** migration `NNN_cluster_key_history.sql` (`pubkey TEXT PRIMARY KEY, created_at TIMESTAMPTZ, retired_at TIMESTAMPTZ NULL`); `TenantSigner::prior_cluster: Vec<VerifyingKey>` field + load path; `sig_visibility_gate` at `:534` extends the push to union all prior cluster pubkeys.

**Route II:** `gc/mark.rs` gains a `resign_reachable(signer: &Signer, reachable: &[StorePathHash])` pass; `store.md` gets a caveat note at step 5 ("visibility gate may reject old-key paths during grace period before first post-rotation GC").

Skeleton only — **DO NOT IMPLEMENT** until the design decision in the Entry Criteria is made. The implementer should flag which route was taken in their commit body.

```rust
// r[impl store.key.rotation-cluster-history]
// Route I sketch — TenantSigner gains prior-cluster history.
// sig_visibility_gate unions: {tenant upstreams} ∪ {cluster} ∪ {prior_cluster}.
```

### T3 — `test(store):` visibility-gate survives rotation

Regardless of route — the test is the same: sign a path with `cluster_key_A`, rotate to `cluster_key_B`, drop the `path_tenants` row (simulating CASCADE on tenant deletion), assert `sig_visibility_gate` still returns `true` for the path.

```rust
// r[verify store.key.rotation-cluster-history]
#[tokio::test]
async fn sig_gate_survives_cluster_key_rotation_with_cascaded_tenant() {
    // 1. Seed path signed by cluster_key_A (no tenant sig, no upstream sig)
    // 2. Seed path_tenants row for tenant T
    // 3. Rotate: active Signer → cluster_key_B; prior_cluster → [key_A_pubkey]
    //    (Route I: via cluster_key_history load. Route II: via GC re-sign.)
    // 4. DELETE FROM path_tenants WHERE tenant_id = T (simulate CASCADE)
    // 5. sig_visibility_gate(Some(other_tenant), path) → true
    //
    // Pre-fix: step 5 returns false — gate derives key_B only,
    // sig was made by key_A, no path_tenants row to bypass.
}
```

Extend the existing `sig_visibility_gate_cross_tenant` test at [`grpc/mod.rs:1197`](../../rio-store/src/grpc/mod.rs) or add sibling.

## Exit criteria

- `/nbr .#ci` green
- Design decision recorded in this plan's `## Design decision` section (strike-through the rejected route)
- `grep 'r\[store.key.rotation-cluster-history\]' docs/src/components/store.md` → 1 hit (marker added)
- `nix develop -c tracey query rule store.key.rotation-cluster-history` shows spec text + ≥1 `r[impl]` + ≥1 `r[verify]` site
- `cargo nextest run -p rio-store sig_gate_survives_cluster_key_rotation` → 1 passed
- Route I only: `grep 'cluster_key_history' rio-store/migrations/*.sql` → ≥1 hit; new migration pinned in [`rio-store/tests/migrations.rs`](../../rio-store/tests/migrations.rs) `PINNED` table
- Route II only: `grep 'resign\|re.sign\|Signer' rio-store/src/gc/mark.rs` → ≥1 hit; `grep 'grace period.*before.*GC' docs/src/components/store.md` → ≥1 hit (caveat)
- `nix develop -c tracey query rule store.substitute.tenant-sig-visibility` still shows `r[impl]` + `r[verify]` (no regression — [P0295](plan-0295-doc-rot-batch-sweep.md)-T496 bumps this to `+2` independently; coordinate merge order)

## Tracey

References existing markers:
- `r[store.substitute.tenant-sig-visibility]` — T2's `sig_visibility_gate` change is under this marker's scope; [P0295](plan-0295-doc-rot-batch-sweep.md)-T496 is bumping it to `+2` to add cluster-key to the trusted-set union text. Coordinate: T2's `r[impl]` annotation should target `+2` post-P0295-T496, or T2 can carry the bump if it lands first.

Adds new markers to component specs:
- `r[store.key.rotation-cluster-history]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) (see `## Spec additions`)

## Spec additions

New marker in [`store.md`](../../docs/src/components/store.md) after `:217` (end of Key Rotation section):

```markdown
r[store.key.rotation-cluster-history]
The cluster signing key MAY be rotated. Prior cluster public keys MUST remain in the trusted set for `sig_visibility_gate` verification until the grace period expires — otherwise paths signed under the old key become invisible to cross-tenant reads when `path_tenants` row count hits zero (CASCADE on tenant deletion). Prior keys are loaded from `cluster_key_history` alongside the active `Signer`.
```

(Route I text — if Route II is chosen, replace "Prior keys are loaded from `cluster_key_history`" with "GC mark phase re-signs reachable paths with the current key; the grace period must exceed at least one GC cycle.")

## Files

```json files
[
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T1: add r[store.key.rotation-cluster-history] after :217; amend step 4 at :215 (Route I). P0295-T496 touches :231 (diff section)"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T2 Route I: TenantSigner.prior_cluster Vec<VerifyingKey> field at :261. P0295-T496 touches :769 (r[verify] bump only, diff section)"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T2: sig_visibility_gate :534 unions prior cluster pubkeys. T3: test sibling after :1197. P0295-T496 touches r[impl] near :534 (SAME region — serialize)"},
  {"path": "rio-store/migrations/NNN_cluster_key_history.sql", "action": "NEW", "note": "T2 Route I only: cluster_key_history table. Pin in rio-store/tests/migrations.rs"},
  {"path": "rio-store/tests/migrations.rs", "action": "MODIFY", "note": "T2 Route I only: add PINNED entry for new migration checksum"},
  {"path": "rio-store/src/gc/mark.rs", "action": "MODIFY", "note": "T2 Route II only: resign_reachable pass. Currently zero Signer refs here"}
]
```

```
docs/src/components/store.md       # T1: new marker :217, amend :215
rio-store/src/
├── signing.rs                     # T2-I: TenantSigner.prior_cluster
├── grpc/mod.rs                    # T2: gate union :534; T3: test :1197
├── gc/mark.rs                     # T2-II only: resign_reachable
└── migrations/
    └── NNN_cluster_key_history.sql  # T2-I only
rio-store/tests/migrations.rs      # T2-I only: PINNED entry
```

## Dependencies

```json deps
{"deps": [478], "soft_deps": [295], "note": "P0478 introduced the cluster().trusted_key_entry() push. P0295-T496 (soft) bumps r[store.substitute.tenant-sig-visibility] → +2 and touches grpc/mod.rs near :534 — serialize; this plan's r[impl] should target +2 if P0295-T496 lands first."}
```

**Depends on:** [P0478](plan-0478-sig-visibility-gate-cluster-key-union.md) — added the `cluster().trusted_key_entry()` push at `:534` that this plan extends to include history.

**Conflicts with:** [P0295](plan-0295-doc-rot-batch-sweep.md)-T496 touches `grpc/mod.rs` near `:534` (bumps `r[impl]` annotation to `+2`) and `store.md:231` — serialize. T2's edit is at the SAME `:534` line; land P0295-T496 first (it's a doc-bump), then this plan targets `+2`.
