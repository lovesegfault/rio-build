# Plan 0320: Restructure P0253 per ADR-018 — dependentRealisations is dead upstream

[P0247](plan-0247-spike-ca-wire-capture-schema-adr.md)'s spike landed [ADR-018](../../docs/src/decisions/018-ca-resolution.md), which proved `dependentRealisations` is dead upstream: Nix removed the field from the data model ([`realisation.hh:53-74`](https://github.com/NixOS/Nix/blob/26842787/src/libstore/include/nix/store/realisation.hh) has no such field), the serializer stubs `{}` with a `// back-compat` comment ([`realisation.cc:105`](https://github.com/NixOS/Nix/blob/26842787/src/libstore/realisation.cc)), and the deserializer ignores it entirely ([`realisation.cc:85-97`](https://github.com/NixOS/Nix/blob/26842787/src/libstore/realisation.cc)).

[P0253](plan-0253-ca-resolution-dependentrealisations.md) was written before the spike. Its T1 at [`:19-26`](plan-0253-ca-resolution-dependentrealisations.md) still prescribes gateway parse+INSERT of `dependentRealisations` into `realisation_deps` — dead work (wire is always `{}`; Nix ignores anything we send back). Worse: it couples the gateway to PG, violating [ADR-007](../../docs/src/decisions/007-postgresql-scheduler-state.md) (scheduler owns PG). And the exit criterion at [`:81`](plan-0253-ca-resolution-dependentrealisations.md) (`rg 'dependentRealisations.*\{\}' ... → 0`) would verify the **wrong thing** — it would force removal of the correct `{}` stub, breaking wire-format compatibility with Nix's deserializer which expects the key present.

ADR-018's Downstream Impact table at [`:88-94`](../../docs/src/decisions/018-ca-resolution.md) already says what to do: strike P0253 T1; T2/T3 absorb `realisation_deps` INSERT as a resolve-time side-effect per [`:62`](../../docs/src/decisions/018-ca-resolution.md). The ADR also flagged two stale spec references ([`:93-94`](../../docs/src/decisions/018-ca-resolution.md)). This plan executes all of that.

## Entry criteria

- [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) merged (ADR-018 exists with the Downstream Impact table) — **DONE**

## Tasks

### T1 — `docs:` rewrite plan-0253 — strike T1, absorb junction-table INSERT into T2

MODIFY [`.claude/work/plan-0253-ca-resolution-dependentrealisations.md`](plan-0253-ca-resolution-dependentrealisations.md) → RENAME to `plan-0253-ca-resolution.md`:

**Title** (`:1`): `# Plan 0253: CA resolution + dependentRealisations populate` → `# Plan 0253: CA resolution — resolve-time inputDrvs rewrite`

**Opening prose** (`:5-7`): Replace the paragraph — it claims the gateway "currently discards `dependentRealisations` on write" as if that's wrong. Per ADR-018, discard is **correct**. New framing:

> When a CA derivation's inputs are themselves CA, the scheduler must rewrite `inputDrvs` placeholder paths to realized store paths before dispatch. The gateway's `{}` stub for `dependentRealisations` ([`opcodes_read.rs:586`](../../rio-gateway/src/handler/opcodes_read.rs)) is **correct** — Nix removed the field upstream ([ADR-018](../../docs/src/decisions/018-ca-resolution.md)). The `realisation_deps` junction table is populated by the scheduler at resolve time (not by the gateway from wire payloads) — each successful `(drv_hash, output_name) → output_path` lookup during resolution IS the dependency edge.

**Strike T1** (`:19-26`) entirely. The gateway stays as-is.

**Renumber + expand T2→T1** (`:28-48`): the resolve module now also inserts edges. Per [ADR-018:57-64](../../docs/src/decisions/018-ca-resolution.md) — step (1)(e) "Side effect: insert edge into `realisation_deps`". Add to the function body sketch:

```rust
// r[impl sched.ca.resolve]
pub async fn resolve_ca_inputs(
    drv: &DerivationState,
    realisations: &RealisationStore,
    db: &SchedulerDb,               // ← new: for realisation_deps INSERT
) -> Result<ResolvedDerivation> {
    // Per ADR-018 Appendix B (tryResolve @ derivations.cc:1215-1239):
    // For each (inputDrv, outputNames):
    //   1. Compute input drv's hash modulo (drv_hash half of realisations PK)
    //   2. Query realisations for (drv_hash, output_name) → output_path
    //   3. Insert output_path into resolved.inputSrcs
    //   4. Record rewrite: DownstreamPlaceholder(input, output).render() → output_path
    //   5. SIDE EFFECT: INSERT INTO realisation_deps (realisation_id, dep_realisation_id)
    //      — this IS rio's derived-build-trace per ADR-018:45. Never crosses the wire.
    // Then: string-replace all placeholder renderings through env/args/builder
    // Finally: drop inputDrvs (resolved derivation is a BasicDerivation)
    todo!("shape per ADR-018 Appendix B")
}
```

**Renumber T3→T2, T4→T3** (`:50-75`) — unchanged content.

**Exit criteria** (`:77-81`): Replace the `rg` line at `:81` (would verify the wrong thing) with:

```markdown
- `rg 'realisation_deps' rio-scheduler/src/ca/resolve.rs` → ≥1 hit (junction-table INSERT lives in the scheduler, not gateway)
- `rg 'dependentRealisations' rio-gateway/src/handler/opcodes_read.rs` → still present (the `{}` stub is CORRECT per ADR-018; do not remove)
```

**Files fence** (`:90-96`): Drop the `opcodes_read.rs` row entirely. The gateway stays untouched:

```json
[
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "NEW", "note": "T1: resolve_ca_inputs() + realisation_deps INSERT side-effect"},
  {"path": "rio-scheduler/src/ca/mod.rs", "action": "NEW", "note": "T1: module decl"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T2: call resolve before worker assignment"}
]
```

**Deps fence** (`:111-113`): Update note — `opcodes_read.rs` no longer touched, so "opcodes_read.rs low collision" is moot. The deps list stays (P0247/250/249/251/230).

### T2 — `docs:` update dag.jsonl row 253 — title + crate

MODIFY [`.claude/dag.jsonl`](../../.claude/dag.jsonl) at `:253`. Current row:

```json
{"plan":253,"title":"CA resolution + dependentRealisations populate (T0 per A10)","deps":[247,250,249,251,230],...,"crate":"rio-gateway,rio-scheduler",...}
```

Two field edits:
- `title`: drop "`+ dependentRealisations populate`" → `"CA resolution — resolve-time inputDrvs rewrite (T0 per A10)"`
- `crate`: drop `rio-gateway` → `"rio-scheduler"`

Preserve all other fields (deps unchanged — P0247 still a dep for the ADR-018 reference; `tracey_total`/`tracey_covered` may shift by 1 if the implementer re-counts exit criteria, but that's a P0253-merge-time concern).

**Mechanism:** `jq` rewrite (dag.jsonl is compact-JSON one-per-line — no `onibus dag` verb for field edits):

```bash
jq -c 'if .plan == 253 then .title = "CA resolution — resolve-time inputDrvs rewrite (T0 per A10)" | .crate = "rio-scheduler" else . end' \
  .claude/dag.jsonl > .claude/dag.jsonl.tmp && mv .claude/dag.jsonl.tmp .claude/dag.jsonl
```

### T3 — `docs:` tracey bump r[sched.ca.resolve] — population source changed

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) at `:267`. Current marker text:

> When a CA derivation's inputs are themselves CA (CA-depends-on-CA), the scheduler MUST rewrite `inputDrvs` placeholder paths to realized store paths before dispatch. Resolution queries the `realisation_deps` junction table populated by `wopRegisterDrvOutput`.

The last clause is **wrong** per ADR-018:43. New text:

> When a CA derivation's inputs are themselves CA (CA-depends-on-CA), the scheduler MUST rewrite `inputDrvs` placeholder paths to realized store paths before dispatch. Each successful `(drv_hash, output_name) → output_path` lookup during resolution is inserted into the `realisation_deps` junction table as a side-effect — this table is rio's derived-build-trace cache (per [ADR-018](../decisions/018-ca-resolution.md)), populated by the scheduler at resolve time. It never crosses the wire; `wopRegisterDrvOutput`'s `dependentRealisations` field is always `{}` from current Nix.

Then run `nix develop -c tracey bump` with `docs/src/components/scheduler.md` staged — version-bumps the marker (e.g., `r[sched.ca.resolve]` → `r[sched.ca.resolve+2]`). Any existing `// r[impl sched.ca.resolve]` annotation becomes stale until someone reviews it against the new text. There are **zero** existing annotations (P0253 is UNIMPL), so no stale annotations to chase.

### T4 — `docs:` gateway.md:642 — remove stale dependentRealisations parenthetical

MODIFY [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) at `:642`. Current note ends with:

> Full CA early cutoff (dependentRealisations tracking in the DAG) is Phase 5.

The parenthetical is wrong — early cutoff compares `nar_hash` via `content_index` (per [P0251](plan-0251-ca-cutoff-compare.md)), nothing to do with `dependentRealisations`. Replace the last sentence:

> `dependentRealisations` is always `{}` from current Nix ([ADR-018](../decisions/018-ca-resolution.md) — field removed upstream in schema v2); the gateway's `{}` stub is correct and should not be changed. Full CA early cutoff (nar_hash comparison via content_index) is Phase 5.

**Check whether this paragraph has a tracey marker** above it (grep `^r\[` within 5 lines of `:642` at dispatch) — if so, the edit needs a `tracey bump` too. Grep says no marker at that line, so a plain edit suffices.

## Exit criteria

- `/nbr .#ci` green (tracey-validate picks up the bump)
- `ls .claude/work/plan-0253-ca-resolution-dependentrealisations.md` → no such file (renamed)
- `ls .claude/work/plan-0253-ca-resolution.md` → exists
- `grep -c '^### T' .claude/work/plan-0253-ca-resolution.md` → 3 (T1-T3, not T1-T4 — old T1 struck)
- `grep 'opcodes_read.rs' .claude/work/plan-0253-ca-resolution.md` → only in prose (pointer to the correct `{}` stub), NOT in the `json files` fence
- `jq -r 'select(.plan==253) | .crate' .claude/dag.jsonl` → `rio-scheduler` (no `rio-gateway`)
- `jq -r 'select(.plan==253) | .title' .claude/dag.jsonl` — does not contain `dependentRealisations`
- `grep 'populated by.*wopRegisterDrvOutput' docs/src/components/scheduler.md` → 0 (stale population-source clause removed)
- `nix develop -c tracey query rule sched.ca.resolve` — shows the new marker text with bumped version; no broken refs
- `grep 'dependentRealisations tracking in the DAG' docs/src/components/gateway.md` → 0 (stale parenthetical removed)
- `grep 'ADR-018' docs/src/components/gateway.md` → ≥1 hit (the note now cites its source)

## Tracey

References existing markers:
- `r[sched.ca.resolve]` — T3 edits the marker text + runs `tracey bump`. P0253 will carry `r[impl sched.ca.resolve]` when it lands, against the bumped version.

No new markers. This plan is pure doc-maintenance — it corrects what's written, doesn't add behavior.

## Files

```json files
[
  {"path": ".claude/work/plan-0253-ca-resolution-dependentrealisations.md", "action": "RENAME", "note": "T1: → plan-0253-ca-resolution.md + strike old-T1, renumber, rewrite exit criteria + files fence"},
  {"path": ".claude/dag.jsonl", "action": "MODIFY", "note": "T2: row 253 title drop 'dependentRealisations populate', crate drop rio-gateway"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T3: r[sched.ca.resolve] text — population source is scheduler-at-resolve-time, not gateway-from-wire; then tracey bump"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T4: :642 remove stale '(dependentRealisations tracking in the DAG)' parenthetical + cite ADR-018"}
]
```

```
.claude/work/
└── plan-0253-ca-resolution.md    # T1: RENAME + rewrite (strike T1, renumber)
.claude/dag.jsonl                 # T2: row 253 title+crate edit
docs/src/components/
├── scheduler.md                  # T3: r[sched.ca.resolve] text + tracey bump
└── gateway.md                    # T4: :642 stale parenthetical
```

## Dependencies

```json deps
{"deps": [247], "soft_deps": [253], "note": "Pure docs — corrects P0253 before it dispatches. P0247 DONE (ADR-018 is the source). MUST land BEFORE P0253 dispatches: if P0253's implementer reads the old doc they'll waste time on dead-work T1 (gateway parse+INSERT for an always-{} field) and break the correct {} stub chasing a wrong exit criterion. Soft-dep P0253: this plan EDITS P0253's doc, so P0253 is an anti-dep — land this first. scheduler.md (19 collisions) + gateway.md (23 collisions) are hot but these are single-paragraph surgical edits at known offsets."}
```

**Depends on:** [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) — **DONE** at [`934b45f6`](https://github.com/search?q=934b45f6&type=commits). ADR-018 is the authority.

**MUST land before:** [P0253](plan-0253-ca-resolution-dependentrealisations.md) dispatches. P0253 is UNIMPL with 5 hard deps (247/250/249/251/230) — only P0247 is DONE, so there's runway. If P0253 dispatches first, its implementer burns time on the gateway-parse dead-end and potentially commits a change that removes the correct `{}` stub.

**Conflicts with:** [`scheduler.md`](../../docs/src/components/scheduler.md) count=19 — single paragraph at `:267`, surgical. [`gateway.md`](../../docs/src/components/gateway.md) count=23 — single sentence at `:642`, surgical. [`dag.jsonl`](../../.claude/dag.jsonl) — every merger touches it for status flips; this is a 2-field jq edit on row 253, merges cleanly unless another plan rewrites that specific row. [P0256](plan-0256-per-tenant-signing-keys.md) T3 touches [`opcodes_read.rs:493`](../../rio-gateway/src/handler/opcodes_read.rs) — this plan **removes** `opcodes_read.rs` from P0253's files fence, which actually REDUCES the P0253/P0256 collision surface (was :418/:577/:586 vs :493; now zero overlap).
