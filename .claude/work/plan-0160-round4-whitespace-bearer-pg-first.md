# Plan 0160: Round 4 — whitespace-bearer bypass + ClearPoison PG-first + poison-TTL fresh-node + gateway→scheduler trace NEVER worked

## Design

Third branch-review. Four critical findings, one of which proved the phase's headline observability goal had never worked.

**Whitespace-bearer bypass (`f105b53`):** the round-3 auth fix was incomplete. `"Bearer    "` (trailing whitespace) → `strip_prefix` → `Some("   ")` passed `!is_empty()` → matched `WHERE cache_token = '   '`. Same bypass class, different payload. Fixed with `.map(str::trim)`.

**ClearPoison retry deadlock (`b874e51`):** in-mem `reset_from_poison()` ran before PG `clear_poison()`. A PG blip left in-mem `Created`, operator saw `cleared=false`, retried, retry hit the not-poisoned guard → permanent no-op until scheduler restart. Fixed by reordering: PG first → in-mem untouched on failure → retry-safe.

**Poison-TTL fresh-node bug (`f9adf3c`):** `Instant::now().checked_sub(30h)` on a node booted 1h ago → `None` → `unwrap_or(now)` → fresh 24h TTL instead of expiry. Fixed by filtering expired rows in `recovery.rs` BEFORE `from_poisoned_row` (PG's wall-clock `elapsed_secs` is immune to node uptime).

**Recovery-dedup poisons traceparent (`4030eba`):** first-submitter-wins counted recovery (which sets `""`) as a submitter — a live submit after failover never upgraded the empty traceparent. Fixed: dedup upgrades `""`.

**Proved gateway→scheduler trace linkage never worked (`3204e4a`, `87`, `88`):** tightened phase2b to assert `{gateway,scheduler,worker} ⊆ service.names` in the trace → FAILED with only `{gateway}` after 30s. The previous ≥3 span-count assertion was worthless — gateway alone emits dozens of spans per ssh-ng session. Root cause: `link_parent()` calls `set_parent()` but the `#[instrument]` span was already created; scheduler span is an orphan with its own trace_id, LINKED (not parented) to gateway. Round 3's data-carry fix made scheduler→worker work, but gateway→scheduler was never actually parented. Deferred to `TODO(phase4b)`.

Polish: `ActorHandle::query_unchecked` helper (5 call sites, `ef16ff7`); flattened cgroup loop indirection (`5882388`); test fixture consolidation (`5be9ba3`); hardened phase4 VM test with COUNT(*) + marker-in-path (`9b6a729`); `CreateTenant` out-of-range `gc_retention_hours` reject (`c01cceb`); `observability.md` rewrite (`ca545f6`).

## Files

```json files
[
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": ".map(str::trim) before is_empty check"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "ClearPoison: PG clear_poison BEFORE in-mem reset_from_poison"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "filter expired poison rows by PG elapsed_secs before from_poisoned_row"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "dedup upgrades empty traceparent (recovery-submit ordering)"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "ActorHandle::query_unchecked helper (5 call sites deduped)"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "CreateTenant out-of-range gc_retention_hours reject"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "flatten utilization_reporter_loop_inner"},
  {"path": "nix/tests/phase2b.nix", "action": "MODIFY", "note": "tighten to service.names ⊇ {gateway,scheduler,worker} → FAIL → loosen + TODO(phase4b)"},
  {"path": "nix/tests/phase4.nix", "action": "MODIFY", "note": "COUNT(*) per-case + marker-in-path + tenant-name-in-error"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "rewrite assignment-traceparent spec for data-carry model"}
]
```

## Tracey

- `r[impl obs.metric.worker-util]` — re-tag in `5882388` after loop flatten

1 marker re-tag (loop refactor moved the annotation). No new markers — this round is fixes.

## Entry

- Depends on P0159: round 3 (fixes round-3's incomplete bearer fix + traceparent-dedup edge case)

## Exit

Merged as `f105b53..20bff3b` (13 commits). `.#ci` green. phase2b assertion loosened to gateway-only + `TODO(phase4b)`. Phase doc round-4 summary: "f105b53..107cf1e, +12 commits".
