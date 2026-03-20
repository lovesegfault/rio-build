# Plan 0250: CA detect — plumb is_ca flag end-to-end

Per GT4 (`.claude/notes/phase5-partition.md` §1): detection **already exists**. [`rio-nix/src/derivation/mod.rs:222`](../../rio-nix/src/derivation/mod.rs) has `has_ca_floating_outputs()` parsing `hash_algo set + hash empty`, tested at `hash.rs:164,291`. It's just not plumbed to the scheduler. Per GT3: `rg 'is_ca|content_addressed|ca_mode' rio-scheduler/src/` returns only `is_cancelled` false-positives — `DerivationState` has no CA field.

**Scope shrink vs. naive estimate: ~30 lines.** No ATerm parser work. Call existing function, set proto field, persist on `DerivationState`.

Per R6 in the partition: `has_ca_floating_outputs()` covers floating-CA but not fixed-output-CA. `is_ca = is_fixed_output() || has_ca_floating_outputs()` covers both. Verify at dispatch that both functions exist at `mod.rs:218-226`.

> **DISPATCH-NOTE (bughunt-mc161):** The `TODO(P0250)` sites at [`translate.rs:379,413`](../../rio-gateway/src/translate.rs) call these methods on **`BasicDerivation`** (single-node fallback path at `:370-381`) AND on **`Derivation`** (full-DAG path at `:385-416`). But [`mod.rs:250-305`](../../rio-nix/src/derivation/mod.rs) `impl BasicDerivation` LACKS both `has_ca_floating_outputs()` and `is_fixed_output()` — they only exist on [`mod.rs:211-226`](../../rio-nix/src/derivation/mod.rs) `impl Derivation`. The compiler will flag this as method-not-found on the `:379` callsite when T1 lands. Fix: add both methods to `impl BasicDerivation` (identical bodies — both inspect `self.outputs`, which `BasicDerivation` has). **T1 scope extends to:** MODIFY [`rio-nix/src/derivation/mod.rs`](../../rio-nix/src/derivation/mod.rs) — add `is_fixed_output()` + `has_ca_floating_outputs()` to the `impl BasicDerivation` block at `:250-305`, copying from `:211-226`.
>
> **SECONDARY — naming:** [`mod.rs:119`](../../rio-nix/src/derivation/mod.rs) `DerivationOutput::is_fixed_output()` is misnamed — it returns `!self.hash_algo.is_empty()`, which is true for fixed-output (hash_algo+hash both set), CA-floating (hash_algo set, hash empty), AND impure derivations. Semantically that's `is_content_addressed()`, not `is_fixed_output()`. The `Derivation::is_fixed_output()` at `:211-216` is correctly named (checks single `out` output + both hash fields). **No rename in this plan** — `DerivationOutput::is_fixed_output()` has unknown callers, renaming is its own plan if grep surfaces friction. Note the ambiguity in a doc-comment at `:118` so future readers don't assume FOD semantics.

## Entry criteria

- [P0248](plan-0248-types-proto-is-ca-field.md) merged (`DerivationInfo.is_content_addressed` proto field exists)
- [P0249](plan-0249-migration-batch-014-015-016.md) merged (`derivations.is_ca` column exists)
- [P0229](plan-0229-build-samples-ema-rs.md) merged (4c last `db.rs` touch — file-serial)

## Tasks

### T1 — `feat(gateway):` set is_ca at translate

MODIFY [`rio-gateway/src/translate.rs`](../../rio-gateway/src/translate.rs) — where `DerivationInfo` is constructed from the parsed `.drv`:

```rust
// r[impl sched.ca.detect]
// Both CA kinds: floating (hash_algo set, hash empty) and fixed-output
// (hash also set). Cutoff applies to either — the output's nar_hash is
// what gets compared, not the input addressing.
is_content_addressed: drv.is_fixed_output() || drv.has_ca_floating_outputs(),
```

### T2 — `feat(scheduler):` add is_ca to DerivationState

MODIFY [`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs):

```rust
pub is_ca: bool,
```

Default `false` in any constructor/`From` impl.

### T3 — `feat(scheduler):` populate from proto at DAG merge

MODIFY [`rio-scheduler/src/actor/merge.rs`](../../rio-scheduler/src/actor/merge.rs) — where proto `DerivationInfo` becomes `DerivationState`:

```rust
// r[impl sched.ca.detect]
is_ca: proto_drv.is_content_addressed,
```

### T4 — `feat(scheduler):` persist to db.rs query

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) — add `is_ca` to the INSERT and SELECT for derivations. Column exists via P0249 T4. This is a `.sqlx/` regen only (schema already present).

**At dispatch:** `git log -p --since='2 months' -- rio-scheduler/src/db.rs | head -80` — find where P0229 left the derivations queries. Don't trust pre-P0229 line numbers.

### T5 — `test(scheduler):` CA fixture → assert is_ca

Unit test: submit a CA drv fixture (use `__contentAddressed = true;` in a test `.drv` or mock `has_ca_floating_outputs()` to return true), assert `state.is_ca == true` after merge.

```rust
// r[verify sched.ca.detect]
#[test]
fn ca_drv_sets_is_ca_flag() { ... }
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule sched.ca.detect` shows both `impl` (translate.rs + merge.rs) and `verify` (unit test)

## Tracey

References existing markers:
- `r[sched.ca.detect]` — T1+T3 implement, T5 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T1: call is_fixed_output()||has_ca_floating_outputs(), set proto field"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T2: add pub is_ca: bool"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "T3: populate from proto at DAG merge"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T4: add is_ca to INSERT/SELECT (column exists via P0249)"},
  {"path": "rio-scheduler/.sqlx/placeholder", "action": "MODIFY", "note": "T4: cargo sqlx prepare regen (proxy entry for workspace .sqlx/)"},
  {"path": "rio-nix/src/derivation/mod.rs", "action": "MODIFY", "note": "T1 (dispatch-note): +is_fixed_output() +has_ca_floating_outputs() on impl BasicDerivation :250-305 (copy from :211-226). Also :118 doc-comment note re is_fixed_output misnomer"}
]
```

```
rio-gateway/src/
└── translate.rs                  # T1: is_ca detection call
rio-scheduler/src/
├── state/derivation.rs           # T2: +is_ca field
├── actor/merge.rs                # T3: populate from proto
└── db.rs                         # T4: INSERT/SELECT (column via P0249)
```

## Dependencies

```json deps
{"deps": [248, 249, 229], "soft_deps": [], "note": "db.rs count=29 — serial after P0229. derivation.rs + merge.rs low collision. GT4 scope-shrink: detection fn already exists at rio-nix mod.rs:222."}
```

**Depends on:** [P0248](plan-0248-types-proto-is-ca-field.md) — proto field. [P0249](plan-0249-migration-batch-014-015-016.md) — `derivations.is_ca` column. [P0229](plan-0229-build-samples-ema-rs.md) — 4c last `db.rs` touch (file-serial on hottest scheduler file).
**Conflicts with:** `db.rs` count=29. Serial after P0229 (dep enforces). `translate.rs`, `merge.rs`, `derivation.rs` low collision.
