# Plan 0157: Phase4 VM tenant smoke + phase2b trace assertions

## Design

Section A of the phase-4a VM test plan: a standalone-fixture tenant-resolve subtest in what became `nix/tests/phase4.nix`. Three SSH keys: comment `team-test` → `SubmitBuild` → PG `builds` row has `tenant_id` matching a pre-seeded `tenants` row; comment `unknown-team` → rejected pre-insert with `"unknown tenant"` error; empty comment → `tenant_id IS NULL`. This exercised the full P0153 chain (SSH key comment → gateway plumb → scheduler resolve → PG) end-to-end.

Parallel work: `nix/tests/phase2b.nix` gained span-linking assertions for the new per-assignment traceparent flow (P0151). The initial assertion was "span count ≥3" via Tempo `/api/traces/{traceID}`.

**The phase2b assertion history is instructive:** `a858dab` first added it. It was tightened/loosened/fixed across multiple commits (`f3e64c5`, `9e0e6ba`, `b8b26f0`, `afa10ef`, `675f339`, `e4fe8e7`) as the trace infrastructure revealed bugs. Round 3 restored ≥3 after the trace-chain fix. Round 4 tightened to `{gateway,scheduler,worker} ⊆ service.names` — which FAILED with only `{gateway}`, proving the ≥3 assertion was worthless (gateway alone emits dozens of spans per ssh-ng session). Round 4 then loosened back to gateway-only + `TODO(phase4b)`.

Round-1 capstone: `20e557f` retroactively applied the 8 phase-4a tracey markers to the code written earlier in round 1, and `59d9771` updated the phase4a.md status line.

## Files

```json files
[
  {"path": "nix/tests/phase4.nix", "action": "NEW", "note": "Section A tenant smoke: 3 SSH keys, pre-seeded tenant row, assertions on builds.tenant_id"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "Tempo trace span assertion (≥3, then tightened/loosened across rounds)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "wire vm-phase4"},
  {"path": "docs/src/phases/phase4a.md", "action": "MODIFY", "note": "round-1 status line"}
]
```

## Tracey

The `20e557f` retroactive marker commit landed 8 annotations for round-1 code:
- `r[impl gw.auth.tenant-from-key-comment]` + verify
- `r[verify sched.trace.assignment-traceparent]`
- `r[impl sched.admin.clear-poison]`
- `r[impl sched.poison.ttl-persist]`
- `r[impl sched.tenant.resolve]` + verify
- `r[impl sched.trace.assignment-traceparent]`

These are counted in their respective feature plans (P0151, P0153, P0155, P0156) — this plan is the **vehicle** for the retroactive tagging commit. No new markers in the VM test files themselves at this point (tracey `.nix` parser lands later in `c02e0af`).

## Entry

- Depends on P0151: traceparent (phase2b asserts it)
- Depends on P0153: multi-tenancy (phase4 smoke tests it)
- Depends on P0156: admin RPCs (ClearPoison/ListWorkers tested)

## Exit

Merged as `5b16a34`, `bc5ec76`, `a858dab`, `f3e64c5`, `9e0e6ba`, `b8b26f0`, `afa10ef`, `675f339`, `e4fe8e7`, `20e557f`, `59d9771` (11 commits). `.#checks.x86_64-linux.vm-phase4` and `.#checks.x86_64-linux.vm-phase2b` pass. Both test files deleted in P0173 when scenario refactor landed.
