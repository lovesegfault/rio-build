Every new `plan-NNNN-<slug>.md` follows this exact structure:

````markdown
# Plan NNN: <title>

<Opening prose — what/why/where-it-came-from. Link the file:line from
the follow-ups table. Link the originating plan if Deps is non-`-`.
Two to four paragraphs for standalone; one paragraph for batch preamble.>

## Entry criteria                         (optional — only if Deps ≠ `-`)

- [P<dep>](plan-<dep>-<slug>.md) merged (<what it provides>)

## Tasks

### T1 — `<type>(<scope>):` <task title>

<What to do. Code snippets in fenced blocks.>

The backtick in the T-header is the commit prefix the implementer copies
verbatim — `\`fix(scheduler):\`` becomes `fix(scheduler): <desc>` in their
commit. Use the severity→type mapping above.

## Exit criteria

- <Concrete, testable criterion. The implementer's `r[impl ...]` marker
  goes on the code that satisfies this; `r[verify ...]` goes on the test.>
- <Next criterion>

## Tracey

<Domain markers this plan implements or verifies. rio-build tracey is
DOMAIN-INDEXED — markers live in `docs/src/components/*.md` as standalone
`r[domain.area.detail]` paragraphs. This section REFERENCES them; it does
NOT define new `r[plan.*]` markers.>

References existing markers:
- `r[gw.opcode.wopFoo]` — T1 implements this
- `r[sched.actor.bar]` — T2 verifies this

Adds new markers to component specs:
- `r[store.manifest.baz]` → `docs/src/components/store.md` (see ## Spec additions below)

## Spec additions (only if new behavior)

<If this plan introduces genuinely NEW behavior with no existing spec marker,
write the new marker text here. You will ALSO add it to the appropriate
`docs/src/components/*.md` file — standalone paragraph, blank line before,
col 0. The marker text goes in the component spec; this section is the
staging area so the reviewer can see what's being added.>

## Files

```json files
[
  {"path": "rio-<crate>/src/<file>.rs", "action": "MODIFY", "note": "T1: <what changes>"},
  {"path": "rio-<crate>/src/<new>.rs", "action": "NEW", "note": "T2: <what it adds>"}
]
```

```
rio-<crate>/src/
└── <file>.rs            # T1: <what changes>
```

The fenced `json files` block feeds `state.PlanFile` and `collisions-regen` —
`_lib.plan_doc_files()` reads it. Each entry: `{"path": str, "action":
"NEW"|"MODIFY"|"DELETE"|"RENAME", "note": str}`. `path` MUST start with
`rio-*/`, `nix/`, `docs/`, `infra/`, `migrations/`, `scripts/`, `flake.nix`,
`.claude/`, `Cargo`, `justfile`, `.config/`, or `codecov.yml` (validator
enforces). The box-drawing tree below is human-readable duplication; the
fenced block is the machine contract. Keep BOTH.

## Dependencies

```json deps
{"deps": [<dep1>, <dep2>], "soft_deps": [], "note": "<ship-order nuance>"}
```

**Depends on:** [P<n>](plan-<n>-<slug>.md) — <what it provides>. <Or "none" with why.>
**Conflicts with:** <which hot-file rows / serialization chains apply.
Grep collisions.jsonl for each .rs path in your Files tree.>

The fenced `json deps` block feeds `PlanRow.deps` machine-readably. The prose
below renders for humans. Keep BOTH.

**Include `discovered_from` in `deps`:** each followup row carries
`discovered_from: int | None` — the plan number where the finding was made.
If non-null and not already in your `deps` list, add it.
````

**Batch append** differs: add `### T<n> — <title>` under `## Tasks`, add a bullet under `## Exit criteria`, add a line in the `## Files` fence, extend `## Entry criteria` if the finding's `Deps` isn't already there.
