# Plan 0247: SPIKE — CA wire-capture + dependentRealisations ADR

8-hour timebox. **NOT implementation** — this is a wire-capture + ADR spike. [P0249](plan-0249-migration-batch-014-015-016.md) and [P0253](plan-0253-ca-resolution-dependentrealisations.md) consume the outcome.

[`opcodes_read.rs:418`](../../rio-gateway/src/handler/opcodes_read.rs) discards `dependentRealisations` on write; `:577` returns empty; `:586` hardcodes `{}`. The comment says "phase 5's early cutoff uses it" — but we don't know the real wire shape or cardinality until we capture it from a live nix-daemon building a CA chain.

**USER DECISIONS applied:** Per Q3, storage is already decided — **junction table** `realisation_deps(realisation_id, dep_realisation_id)`, not JSONB. This spike still captures wire shape (so P0253 knows what to parse) and samples nixpkgs CA-on-CA frequency (informational — per A10, P0253 is T0 regardless). The spike is **informational, not decision-gating** for storage.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (markers seeded; CA context in phase5.md corrected)

## Tasks

### T1 — `test:` build CA chain in VM interactive shell

```bash
nix-build-remote -- .#checks.x86_64-linux.vm-phase3a.driverInteractive
```

Inside the VM shell, construct a 2-deep CA derivation chain with `__contentAddressed = true;` and build with `nix build --rebuild`.

### T2 — `test:` capture wopRegisterDrvOutput wire payloads

Use `strace -f -e trace=write -s 65536 -o /tmp/daemon.strace` on the nix-daemon socket writes, OR reuse the golden-capture harness in [`rio-gateway/tests/golden/`](../../rio-gateway/tests/golden/). Record:
- Actual `dependentRealisations` JSON shape (flat `{drvout_hash: store_path}` map? nested?)
- Cardinality in a 2-deep chain (1 entry? N entries for N inputs?)
- Whether it's present on ALL `wopRegisterDrvOutput` calls or only CA-on-CA

### T3 — `docs:` diff resolved vs unresolved ATerm

Capture both the pre-resolution and post-resolution `.drv` ATerm. What does `inputDrvs` look like before/after? This informs [P0253](plan-0253-ca-resolution-dependentrealisations.md)'s rewrite logic.

### T4 — `docs:` sample CA-on-CA frequency in nixpkgs

Heuristic:
```bash
rg '__contentAddressed' $(nix-instantiate '<nixpkgs>' -A stdenv 2>/dev/null) | wc -l
```

**Informational only** — per USER A10, P0253 is T0 regardless of frequency. The number goes in the ADR as context.

### T5 — `docs:` write ADR-018

NEW `docs/src/decisions/018-ca-resolution.md` (~2 pages):
- Captured wire samples as appendix (hex dumps + decoded JSON)
- `dependentRealisations` shape documented
- Junction-table schema confirmed (per USER Q3 — already decided, this ADR records the rationale + wire evidence)
- Resolved-vs-unresolved ATerm diff
- CA-on-CA frequency number from T4

### T6 — `test:` save captured bytes for golden conformance

NEW `rio-gateway/tests/golden/corpus/ca-register-*.bin`, `ca-query-*.bin` — raw wire bytes for future golden tests.

### T7 — `docs:` record outcome in partition note

Write to `.claude/notes/phase5-partition.md` §0:
```markdown
<!-- P0247-OUTCOME: dependent_shape={flat|nested}, cardinality=N, ca_on_ca_pct=X% -->
```

## Exit criteria

- `docs/src/decisions/018-ca-resolution.md` merged with wire-capture appendix
- `rio-gateway/tests/golden/corpus/ca-*.bin` committed (binary corpus)
- `.claude/notes/phase5-partition.md` has P0247-OUTCOME comment
- `/nbr .#ci` green

## Tracey

none — spike produces an ADR, not code. No markers referenced or added.

## Files

```json files
[
  {"path": "docs/src/decisions/018-ca-resolution.md", "action": "NEW", "note": "T5: ADR with wire-capture appendix"},
  {"path": "rio-gateway/tests/golden/corpus/ca-register-2deep.bin", "action": "NEW", "note": "T6: captured wopRegisterDrvOutput bytes"},
  {"path": "rio-gateway/tests/golden/corpus/ca-query-2deep.bin", "action": "NEW", "note": "T6: captured wopQueryRealisation bytes"},
  {"path": ".claude/notes/phase5-partition.md", "action": "MODIFY", "note": "T7: P0247-OUTCOME comment"}
]
```

```
docs/src/decisions/
└── 018-ca-resolution.md          # T5: ADR
rio-gateway/tests/golden/corpus/
├── ca-register-2deep.bin         # T6: wire capture
└── ca-query-2deep.bin            # T6: wire capture
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [], "note": "SPIKE, informational not decision-gating (Q3/A10 already user-decided). Parallel with P0248 (proto touch — disjoint). 8h timebox."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — `r[sched.ca.resolve]` marker seeded so ADR can reference it.
**Conflicts with:** none. ADR + corpus. Parallel with everything else in Wave 0.
