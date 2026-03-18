# Plan 0180: Rem-01 — Poisoned-orphan spurious Succeeded (P0 keystone id_to_hash insert)

## Design

**P0 — incorrect results, no alerting, silent until `nix copy` at use site.** The 126-item remediation sweep's most urgent finding: `sched-reap-collateral-poisoned` is STEADY-STATE — requires no crash, no failure. First build cleanup post-recovery reaps all recovered-poisoned nodes.

Three independent routes deposit the same corrupt PG state, all fixed by one keystone change: recovered poisoned derivations were NOT inserted into `id_to_hash`. The `check_build_completion` logic counts `0/0 derivations` as success when it can't find the derivation records → build transitions to Succeeded → client sees `nix build` succeed but the path is invalid at use site.

| Route | Where state goes wrong | Fixed by |
|---|---|---|
| Poison-persist crash window | `completion.rs:445→459` (4 awaits incl. rio-store RPC) | Keystone + atomic-poisoned SQL |
| Cancel-loop crash window | `build.rs:78→146` (per-drv persist, then build persist) | Keystone does NOT fix (Cancelled in TERMINAL_STATUSES, never loaded) — separate fix needed, noted in remediation doc §3.3 |
| In-mem-first PG timeout | `build.rs:414` mutates before `build.rs:442` PG write | Keystone + orphan-guard already present; PG-first closes the mint |
| Reap-collateral (no-crash) | `dag/mod.rs:507` reaps ALL empty+terminal, not just newly-emptied | `was_interested` predicate |

**Keystone fix:** `recovery.rs` inserts poisoned `derivation_id` into `id_to_hash`. Replace lying comment at the `bd_rows` fallthrough. `db.rs`: new `persist_poisoned()` combining status+timestamp atomically; drop `AND poisoned_at IS NOT NULL` filter (COALESCE fallback). `completion.rs`: two callsites swap `persist_status(Poisoned)`+`set_poisoned_at` → atomic `persist_poisoned()`.

Remediation doc: `docs/src/remediations/phase4a/01-poison-orphan-recovery.md` (977 lines, 10 findings collated).

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "insert poisoned derivation_id into id_to_hash; replace lying comment"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "persist_poisoned() atomic status+timestamp; drop AND poisoned_at IS NOT NULL filter"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "two callsites: persist_status(Poisoned)+set_poisoned_at → atomic persist_poisoned()"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "in-mem-first → PG-first ordering"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "was_interested predicate on reap"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "3 new verify tests for poisoned-failed-count"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "r[sched.recovery.poisoned-failed-count] spec marker"}
]
```

## Tracey

- `r[impl sched.recovery.poisoned-failed-count]` — `891a652` (keystone insert)
- `r[verify sched.recovery.poisoned-failed-count]` ×2 — `891a652` (regression + fault-inject tests)

3 marker annotations.

## Entry

- Depends on P0162: round 6 (this extends the poison recovery fix — round 6 removed `reset_from_poison`, this fixes `id_to_hash` population)

## Exit

Merged as `35171c7` (plan doc) + `891a652` (fix). `.#ci` green. `tracey query rule sched.recovery.poisoned-failed-count` shows 1 spec + 1 impl + 2 verify.
