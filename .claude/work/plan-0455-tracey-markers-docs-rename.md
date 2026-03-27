# Plan 0455: tracey markers + docs rename — worker→builder, add fetcher domain

[ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) splits the single "worker" executor into **builder** (airgapped, regular derivations) and **fetcher** (egress-open, FODs only). P0451 landed the crate/proto/CRD renames; this plan follows with the tracey marker domain rename (`r[worker.*]` → `r[builder.*]`) and the design-book rewrite. The two must land close together or `tracey query validate` stays red — P0451 moved the `r[impl worker.*]` sites into `rio-builder/`, but the spec markers in `docs/` still say `worker.*`.

This is a mechanical bulk rename with one structural addition (`docs/src/components/fetcher.md`, new) and one structural deletion (the `fod-proxy` section in `security.md` — ADR-019 deletes Squid entirely). 72 unique `r[worker.*]` marker IDs, ~390 annotation lines across `docs/` + `*.rs` + `nix/tests/default.nix`. No behavior change; `tracey query status` coverage % must be identical before/after.

## Entry criteria

- [P0451](plan-0451-crate-crd-proto-rename.md) merged (crate `rio-worker` → `rio-builder`, so `r[impl]` sites now live at `rio-builder/src/**` — the sed targets moved)

## Tasks

### T1 — `docs(tracey):` bulk sed `r[worker.*]` → `r[builder.*]` across spec + impl + verify

One-shot rename of all three annotation forms. The marker domain changes; the `.area.detail` suffix is preserved verbatim.

```bash
# spec markers (docs/**/*.md, col-0 standalone paragraphs)
git grep -lz 'r\[worker\.' -- 'docs/**/*.md' | xargs -0 sed -i 's/r\[worker\./r[builder./g'
# impl annotations (*.rs)
git grep -lz 'r\[impl worker\.' -- '*.rs' | xargs -0 sed -i 's/r\[impl worker\./r[impl builder./g'
# verify annotations (*.rs, nix/tests/default.nix)
git grep -lz 'r\[verify worker\.' -- '*.rs' 'nix/tests/*.nix' | xargs -0 sed -i 's/r\[verify worker\./r[verify builder./g'
```

Then `tracey query validate` — expect zero broken refs. If any `r[worker.*]` survives (e.g., versioned `r[worker.foo+2]`), the regex above catches it (matches the domain prefix, not the full ID).

### T2 — `docs(components):` `worker.md` → `builder.md`, new `fetcher.md`

1. `git mv docs/src/components/worker.md docs/src/components/builder.md`
2. Inside `builder.md`: `s/rio-worker/rio-builder/g`, `s/Worker/Builder/g` in headings, update the intro paragraph to reference ADR-019 and scope the doc to non-FOD builds only.
3. New `docs/src/components/fetcher.md` — minimal:
   - Intro: FOD-only executor, same binary as builder, `RIO_EXECUTOR_KIND=fetcher`
   - Pointer to [ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) for rationale
   - `r[fetcher.*]` markers already defined in ADR-019 (`fetcher.netpol.egress-open`, `fetcher.sandbox.strict-seccomp`, `fetcher.node.dedicated`) — reference them, don't duplicate. Add component-level markers for anything ADR-019 doesn't cover (e.g., `r[fetcher.upload.hash-verify-before]` if the hash-check-before-upload flow needs a spec home).
4. `docs/src/SUMMARY.md`: replace the `[rio-worker](./components/worker.md)` entry with `[rio-builder](./components/builder.md)` + new `[rio-fetcher](./components/fetcher.md)` sibling.

### T3 — `docs(adr):` rename ADR-005/011/012 titles + filenames in-place

Same ADR numbers, new names. `git mv` + heading edit + cross-ref fixup:

| From | To |
|---|---|
| `005-worker-store-model.md` | `005-builder-store-model.md` |
| `011-streaming-worker-model.md` | `011-streaming-builder-model.md` |
| `012-privileged-worker-pods.md` | `012-privileged-builder-pods.md` |

Update `# ADR-NNN:` heading in each. Grep for inbound links (`git grep '00[5|11|12]-.*worker' -- 'docs/**' '*.md'`) and retarget — `SUMMARY.md:14,20,21`, `decisions.md` index, and any ADR-to-ADR cross-refs (ADR-019 already uses the new names at `:16`).

### T4 — `docs(observability):` metric name table `rio_worker_*` → `rio_builder_*`

[`observability.md:169-173`](../../docs/src/observability.md) — the five-row worker metrics table. `s/rio_worker_/rio_builder_/g` in the `| name |` column. Add a note referencing ADR-019 §Observability for the new `rio_scheduler_fod_queue_depth` / `rio_scheduler_fetcher_utilization` metrics (those get their own rows when P0456+ lands the actual emitters; this plan just renames existing).

### T5 — `docs(security):` delete fod-proxy section, pointer to ADR-019

[`security.md:175-186`](../../docs/src/security.md) — the `### FOD Network Egress` section and the Squid/allowlist mitigation paragraph. ADR-019 deletes `fod-proxy` entirely (hash check is the integrity boundary; domain allowlist is operational friction). Replace with a short `### FOD Network Isolation` section pointing at ADR-019 §Network isolation — builders airgapped, fetchers egress-open-minus-RFC1918, hash check as backstop.

Also fix the forward-ref at `:244` (item 5 in the threat-model list) — currently links `#fod-network-egress`, retarget to the new section anchor.

### T6 — `docs(misc):` sweep remaining prose refs

`architecture.md`, `crate-structure.md`, `configuration.md`, `README.md`, `CLAUDE.md` — prose mentions of "worker" / `rio-worker` / `WorkerPool` that aren't tracey markers. Context-sensitive `s/worker/builder/` (NOT blind — "worker node" in k8s context stays, "Nix worker protocol" stays). `git grep -in '\bworker\b' -- docs/ README.md CLAUDE.md | grep -v 'r\['` to find candidates; hand-review each.

## Exit criteria

- `/nixbuild .#ci` green (includes `tracey-validate`)
- `nix develop -c tracey query validate` → exit 0, no broken refs
- `nix develop -c tracey query status` → coverage % unchanged vs pre-rename baseline (capture baseline before T1; a drop means an annotation got lost in the sed)
- `git grep 'r\[worker\.' -- 'docs/**' '*.rs' '*.nix'` → 0 hits
- `git grep 'r\[impl worker\.\|r\[verify worker\.' -- '*.rs' '*.nix'` → 0 hits
- `ls docs/src/components/builder.md docs/src/components/fetcher.md` → both exist
- `ls docs/src/components/worker.md` → gone
- `git grep 'fod-proxy\|Squid' -- docs/src/security.md` → 0 hits (or only in a "formerly" historical note)
- `git grep 'rio_worker_' -- docs/src/observability.md` → 0 hits
- mdbook builds: `nix develop -c mdbook build docs/` → no broken links

## Tracey

This plan IS the tracey rename. No new `r[impl]`/`r[verify]` annotations — it moves 72 existing marker IDs from the `worker.*` domain to `builder.*`. ADR-019 already introduced the `r[builder.*]`, `r[fetcher.*]`, `r[ctrl.builderpool.*]`, `r[ctrl.fetcherpool.*]`, `r[sched.dispatch.fod-*]` markers; those are net-new and will show in `tracey query uncovered` until the ADR-019 implementation plans land `r[impl]` for them.

**Baseline capture before T1:** `nix develop -c tracey query status > /tmp/tracey-baseline-p0455.txt` — diff against post-rename output in exit-criteria check.

## Files

```json files
[
  {"path": "docs/src/components/worker.md", "action": "RENAME", "note": "T2: → builder.md, s/rio-worker/rio-builder/, scope to non-FOD"},
  {"path": "docs/src/components/builder.md", "action": "CREATE", "note": "T2: renamed from worker.md"},
  {"path": "docs/src/components/fetcher.md", "action": "CREATE", "note": "T2: new, minimal, ADR-019 pointer + r[fetcher.*] refs"},
  {"path": "docs/src/SUMMARY.md", "action": "MODIFY", "note": "T2+T3: component entry rename, ADR link retarget"},
  {"path": "docs/src/decisions/005-worker-store-model.md", "action": "RENAME", "note": "T3: → 005-builder-store-model.md"},
  {"path": "docs/src/decisions/011-streaming-worker-model.md", "action": "RENAME", "note": "T3: → 011-streaming-builder-model.md"},
  {"path": "docs/src/decisions/012-privileged-worker-pods.md", "action": "RENAME", "note": "T3: → 012-privileged-builder-pods.md"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: :169-173 metric table rio_worker_* → rio_builder_*"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T5: delete :175-186 fod-proxy section, ADR-019 pointer; fix :244 xref"},
  {"path": "docs/src/architecture.md", "action": "MODIFY", "note": "T6: prose worker→builder"},
  {"path": "docs/src/crate-structure.md", "action": "MODIFY", "note": "T6: rio-worker → rio-builder crate entry"},
  {"path": "docs/src/configuration.md", "action": "MODIFY", "note": "T6: WorkerPool → BuilderPool refs"},
  {"path": "README.md", "action": "MODIFY", "note": "T6: component list rename"},
  {"path": "CLAUDE.md", "action": "MODIFY", "note": "T6: any rio-worker refs"},
  {"path": "rio-builder/src/**/*.rs", "action": "MODIFY", "note": "T1: r[impl worker.*] → r[impl builder.*] (bulk sed, ~dozens of files)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T1: r[verify worker.*] → r[verify builder.*] at subtests entries"}
]
```

```
docs/src/
├── SUMMARY.md                              # T2+T3: TOC retarget
├── architecture.md                         # T6
├── crate-structure.md                      # T6
├── configuration.md                        # T6
├── observability.md                        # T4: metric table
├── security.md                             # T5: fod-proxy section deleted
├── components/
│   ├── worker.md → builder.md              # T2: git mv
│   └── fetcher.md                          # T2: NEW
└── decisions/
    ├── 005-worker-store-model.md → 005-builder-store-model.md      # T3
    ├── 011-streaming-worker-model.md → 011-streaming-builder-model.md
    └── 012-privileged-worker-pods.md → 012-privileged-builder-pods.md
rio-builder/src/**/*.rs                     # T1: r[impl] bulk sed
nix/tests/default.nix                       # T1: r[verify] bulk sed
README.md, CLAUDE.md                        # T6
```

## Dependencies

```json deps
{"deps": [451], "soft_deps": [], "note": "P0451 renamed rio-worker → rio-builder crate; r[impl] annotation sites moved with the files. This plan's sed targets rio-builder/src/**, not rio-worker/src/** — would no-op if run before P0451."}
```

**Depends on:** [P0451](plan-0451-crate-crd-proto-rename.md) — the crate rename moved every `// r[impl worker.*]` line to a new path. Running T1's sed before P0451 would hit stale paths.
**Conflicts with:** any in-flight plan touching `docs/src/components/worker.md` or the three ADR files. `onibus collisions check docs/src/components/worker.md` before starting. The `rio-builder/src/**` sed is comment-only (no code change) — merge conflicts are trivial if they occur.
