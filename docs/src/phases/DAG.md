# Phase DAG — Execution Plan

> **⚠️ STUB — REGENERATE BEFORE USE**
>
> This file is the checked-in phase dependency graph. The content below is a **structural placeholder** extracted from `> **Depends on Phase X:**` blockquotes at port time — it has NOT been validated by a full scout run. Before trusting the collision matrix for parallel-worktree planning, run the [regeneration recipe](#regeneration) below.

**What this is:** the dependency DAG of all rio-build implementation phases, extracted from `docs/src/phases/phase*.md`. Drives parallel-worktree execution: which phases can start now, which conflict on files, which must serialize.

**Last regenerated:** never (stub). Replace this line with the commit hash when regenerated.

---

## Phase table (stub — from doc headers only)

| Phase | Title | Depends on | Status | Milestone target |
|---|---|---|---|---|
| 1a | Wire Format + Read-Only Protocol | — | COMPLETE | nextest + golden conformance |
| 1b | Build Execution | 1a | COMPLETE | nextest + `nix build` roundtrip |
| 2a | Core Distribution | 1b | COMPLETE | vm-phase2a |
| 2b | Observability + Packaging | 2a | COMPLETE | vm-phase2b |
| 2c | Intelligent Storage + Scheduling | 2b | COMPLETE | vm-phase2c |
| 3a | Operator + Worker Store | 2c | COMPLETE | vm-phase3a (k3s) |
| 3b | Production Hardening | 3a | COMPLETE | vm-phase3b |
| 4a | Observability + Multi-Tenancy Foundation | 3b | COMPLETE | vm-phase4 §A |
| 4b | GC Correctness + Operational Tooling | 4a | pending | vm-phase4 §B |
| 4c | Adaptive Scheduling + Polish | 4a, 4b | pending | vm-phase4 §C |
| 5 | CA Early Cutoff + Advanced | 4c | pending | TBD |

**Structure:** mostly linear. Parallelism within a phase (sub-tasks across crates) is common; parallelism across phases is rare outside the 4b/4c window where a careful file-collision check is needed.

---

## File collision matrix

**NOT POPULATED** — requires scout run. Key files to check for frontier-phase overlap:

- `migrations/00N_*.sql` — migration 009 is chained across 4a/4b/4c as Part A/B/C (serialize!)
- `rio-scheduler/src/actor/*.rs` — multiple phases touch the actor
- `rio-store/src/gc/*.rs` — 4b GC correctness + 4c GC tuning
- `nix/tests/scenarios/*.nix` — VM test scenarios are appended across sub-phases
- `rio-proto/proto/*.proto` — field additions from multiple phases need field-number coordination

---

## Regeneration

<a name="regeneration"></a>

This doc is scout output. To regenerate after phase docs change:

1. Launch a read-only research agent with cwd `/root/src/rio-build/main` and this prompt:

> You are building a dependency DAG of all rio-build implementation phases. This DAG drives parallel worktree execution.
>
> **Task**
>
> 1. **Enumerate**: `ls docs/src/phases/phase*.md` — list every phase doc. Skip umbrella docs (`phase3.md`, `phase4.md`) which are overviews; the sub-phase docs (`phase3a.md`, `phase4b.md`, etc.) are the implementable units.
> 2. **For each sub-phase doc**, extract into a table: phase ID + title; dependencies (the `> **Depends on Phase X:**` blockquote); task count (`- [ ]` and `- [x]` under `## Tasks`); milestone (the `## Milestone` section — nextest target + VM test name); primary files touched (grep for `rio-[a-z]+/src/.*\.rs`, `nix/tests/.*\.nix`, `migrations/.*\.sql` paths in the doc); status (grep for `Status:` line — COMPLETE vs absent).
> 3. **Cross-reference implementation state**: does the `Status:` line say COMPLETE? Are all `- [ ]` boxes actually `- [x]`? Spot-check: if the doc says "add field X to proto Y", grep `rio-proto/proto/*.proto` for field X.
> 4. **Identify the frontier**: phases without `Status: COMPLETE` whose dependencies ARE complete.
> 5. **File collision matrix**: for the frontier phases, which touch overlapping files? This determines which can run in truly parallel worktrees vs which serialize. Pay special attention to `migrations/` (migration numbers collide), `rio-proto/proto/*.proto` (field numbers collide), and `nix/tests/scenarios/*.nix` (subtests append to shared scenario files).
> 6. **Flag doc bugs**: dangling file:line refs (paths mentioned in docs that don't exist), proto fields mentioned but absent, functions named that grep can't find.
>
> **Output format**
>
> Produce a markdown report with: Table 1 (all phases — ID, Title, Deps, Tasks done/total, Milestone, Status); Table 2 (file collision matrix for frontier phases); List (doc bugs); short mermaid graph (linear-ish, so keep it simple).
>
> Do NOT modify any files. This is read-only research.

2. Replace the content between the stub warning and this regeneration section with the agent's output.

3. Update the "Last regenerated" line with the commit hash.
