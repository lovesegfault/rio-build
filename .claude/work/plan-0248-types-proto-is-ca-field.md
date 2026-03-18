# Plan 0248: types.proto — is_ca field (ONLY phase5 types.proto touch)

Per GT6+GT7+GT8 in `.claude/notes/phase5-partition.md` §1: `PutChunk` proto already exists ([`types.proto:585`](../../rio-proto/proto/types.proto)), `ResourceUsage` already exists (`:401`), JWT uses a string metadata header. **This is the only `types.proto` touch Phase 5 core needs.** [P0270](plan-0270-buildstatus-critpath-workers.md)/[P0271](plan-0271-cursor-pagination-admin-builds.md) touch `admin.proto` not `types.proto`; [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) (dashboard) is an advisory-serial EOF-append.

`types.proto` is the hottest file in the repo (collision count=27+). This plan makes the smallest possible change — 3 lines EOF-appended — and serializes hard after 4c [P0231](plan-0231-actor-drain-mailbox.md).

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (`r[sched.ca.detect]` seeded)
- [P0231](plan-0231-actor-drain-mailbox.md) merged (last 4c `types.proto` toucher — HUB plan)

## Tasks

### T1 — `feat(proto):` add is_content_addressed to DerivationInfo

EOF-append to `DerivationInfo` message in [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto):

```protobuf
  // Phase 5 CA cutoff: set by gateway translate.rs from
  // has_ca_floating_outputs() || is_fixed_output(). Scheduler uses
  // this to gate the hash-compare branch in completion.rs.
  bool is_content_addressed = <next_free>;
```

Find the current last field number in `DerivationInfo` at dispatch (`grep -A50 'message DerivationInfo' types.proto`). Don't guess.

### T2 — `test(proto):` roundtrip

Encode/decode a `DerivationInfo` with `is_content_addressed = true`, assert it survives. Catches syntax errors before any downstream plan hits them.

## Exit criteria

- `/nbr .#ci` green (includes proto regen via `build.rs`)
- `grep is_content_addressed rio-proto/proto/types.proto` → 1 match
- `cargo build -p rio-proto` clean

## Tracey

none — pure plumbing. The marker `r[sched.ca.detect]` is impl'd by [P0250](plan-0250-ca-detect-plumb-is-ca.md) which USES this field.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: EOF-append bool is_content_addressed to DerivationInfo (~3 lines)"}
]
```

```
rio-proto/proto/
└── types.proto                   # T1: +bool is_content_addressed (DerivationInfo EOF)
```

## Dependencies

```json deps
{"deps": [245, 231], "soft_deps": [], "note": "types.proto count=27 HOTTEST. Serial after P0231 (4c HUB). ONLY phase5 core types.proto touch — P0276 (dashboard) advisory-serial only."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — marker seeded. [P0231](plan-0231-actor-drain-mailbox.md) — last 4c `types.proto` touch; hard file-serial.
**Conflicts with:** `types.proto` is the hottest file. At dispatch: `git log -p --since='2 months' -- rio-proto/proto/types.proto | head -50` to confirm EOF is clean. P0276 also EOF-appends (distinct message) — textually parallel-safe.
