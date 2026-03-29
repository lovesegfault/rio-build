# Plan 982804602: HOWTO doc — add-concurrency-knob plumbing chain

Two plans ([P0473](../../.claude/dag.jsonl), [P0474](plan-0474-put-chunked-concurrency-bound.md)) independently walked the same 6-touchpoint chain to plumb a concurrency knob from `config.toml` through to the call site. At N=2 the pattern isn't worth abstracting (a macro or builder adds more complexity than it saves), but it IS worth documenting so the next author doesn't re-derive the chain.

The 6 touchpoints:

1. `docs/src/observability.md` — document the knob + default
2. `rio-<component>/src/config.rs` — add struct field + `#[serde(default = "...")]`
3. `infra/helm/.../<component>.toml` tpl — helm-templated value
4. `infra/helm/values.yaml` — default value + doc-comment
5. `rio-<component>/src/<module>.rs` — thread config field to call site (often through Actor/Service constructor)
6. `rio-<component>/src/<module>.rs` test — assert non-default value honored

Discovered via consolidator; proposed re-evaluation for extraction at N=4.

## Tasks

### T1 — `docs(contributing):` write add-concurrency-knob.md HOWTO

NEW file [`docs/src/contributing/add-concurrency-knob.md`](../../docs/src/contributing/) — walk the 6-touchpoint chain with a concrete example (use P0474's `put_chunked_concurrency` as the worked example since it's the cleanest of the two). Include:

- Table of the 6 files + what changes in each
- The `#[serde(default = "default_<knob>")]` + `fn default_<knob>() -> usize { N }` idiom
- Helm `{{ .Values.<component>.<knob> | default 16 }}` templating pattern
- Testing pattern: `config::from_toml_str(r#"<knob> = 99"#)` → assert field == 99
- Cross-ref to [`docs/src/observability.md`](../../docs/src/observability.md) naming conventions

Create `docs/src/contributing/` directory (doesn't exist yet). Add to `docs/src/SUMMARY.md` navigation.

### T2 — `docs(contributing):` add re-eval-at-N=4 marker

At the bottom of the HOWTO, add a `> **Re-evaluate for extraction at N=4**` block noting that when the 4th concurrency knob lands, consider a `concurrency_knob!(name, default)` declarative macro that generates the config field + default fn + serde attr. Link back to this plan number for context.

## Exit criteria

- `test -f docs/src/contributing/add-concurrency-knob.md` — file exists
- `grep -c 'touchpoint\|serde.*default' docs/src/contributing/add-concurrency-knob.md` → ≥6 (all touchpoints + serde idiom covered)
- `grep 'add-concurrency-knob' docs/src/SUMMARY.md` → ≥1 hit (nav wired)
- `grep 'N=4\|Re-evaluate' docs/src/contributing/add-concurrency-knob.md` → ≥1 hit (T2 marker)
- `nix build .#checks.x86_64-linux.doc` → green (mdbook builds)

## Tracey

No domain markers — contributing doc, not component spec. No `docs/src/components/` behavior described.

## Files

```json files
[
  {"path": "docs/src/contributing/add-concurrency-knob.md", "action": "NEW", "note": "T1: 6-touchpoint HOWTO with P0474 worked example"},
  {"path": "docs/src/SUMMARY.md", "action": "MODIFY", "note": "T1: nav entry for contributing section"}
]
```

```
docs/src/
├── contributing/
│   └── add-concurrency-knob.md   # NEW: 6-touchpoint HOWTO
└── SUMMARY.md                     # T1: nav entry
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [474], "note": "No hard deps — doc-only. Soft-dep 474 (put_chunked_concurrency) is the worked example; already merged. P0473 is the other instance but no plan doc exists (ad-hoc rsb fix)."}
```

**Depends on:** none — doc-only.
**Conflicts with:** [`SUMMARY.md`](../../docs/src/SUMMARY.md) — low traffic, one-line nav append.
