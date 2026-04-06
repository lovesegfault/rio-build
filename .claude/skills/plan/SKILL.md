---
name: plan
description: Promote follow-ups into plan docs. Reads followups-pending.jsonl (the reviewer sink), generates a runid, launches rio-planner in a docs-<runid> worktree. After writer commits — QA gate (rio-plan-reviewer) before merge; FAIL bounces to writer. Writer uses 9-digit placeholder IDs; /merge-impl assigns real numbers. Usage — /plan OR /plan --inline '<json-array>'
---

## 1. Load follow-ups from the sink

**Default mode** (no `$ARGUMENTS`): read `followups-pending.jsonl`. The sink IS canonical — reviewers and cadence agents write to it via `onibus state followup`; if a reviewer crashed before writing, coordinator re-runs `/review-impl`.

```bash
.claude/bin/onibus state followups-render
```

**`--inline` mode**: `$ARGUMENTS` is a JSON array of Followup-shaped objects (coordinator ad-hoc — e.g., tooling fixes mid-run). Validated against `state.Followup`:

```bash
.claude/bin/onibus state followups-render --inline '<json-array>'
```

If empty: report "no follow-ups" and stop.

Schema (`state.Followup` — `.claude/bin/onibus schema Followup` for JSON Schema):

| Field | Meaning |
|---|---|
| severity | `trivial` · `correctness` · `feature` · `test-gap` · `doc-bug` · `perf` |
| description | One-line what-and-why |
| file_line | Where the finding lives (may be stale — writer re-greps) |
| proposed_plan | `P-batch-<kind>` (writer appends to existing batch) or `P-new` (writer creates standalone) |
| deps | Which plan introduced the flagged code, or null for pre-existing |
| discovered_from | int plan# where found (None for tooling/inline) — writer adds to the new plan's `deps` to close the loop |

## 2. Generate a run ID

```bash
printf '%06d' $(( $(date +%s) % 1000000 ))
```

Six digits, unique per `/plan` invocation (collides only if two invocations land in the same second within an ~11-day window — coordinator is serial, so they don't). The writer computes its own placeholder numbers from this: `9<runid><NN>` where `NN` is the writer's local sequence (01, 02, …). Nine digits, starts with 9, so it never collides with a real plan number (≤4 digits). Same token goes in filename, dag.jsonl rows, links — `/merge-impl` rewrites them all with one string-replace at merge time. No counter, no TOCTOU: the writer can mint as many IDs as it needs.

## 3. Check for existing batch targets

```bash
# Is the relevant batch doc still UNIMPL/PARTIAL? dag.jsonl is the source of truth.
.claude/bin/onibus state open-batches
```

If a batch doc has `"status":"DONE"`, the writer creates a fresh batch doc instead of appending. Note this in the prompt.

## 4. Compose the prompt

**Only the table + allocation context.** Worktree protocol, plan-doc skeleton, tracey discipline, dag.jsonl integration, link discipline, commit format — all baked into `rio-planner`'s system prompt. Do NOT repeat them.

```
Follow-ups from sink (followups-pending.jsonl):

<rendered table from step 1>

Run ID: <runid>   (6 digits — compute placeholders as 9<runid>01, 9<runid>02, …)
  P-new rows: <count>
  P-batch rows: <count> → append to open batches (see step 3)

<Batch note if any target batch is DONE:
 "Batch P<NNNN> is DONE — mint a fresh trivial-hardening batch with the next placeholder">
```

## 5. Launch

```
Agent(
  subagent_type="rio-planner",
  name="docs-<runid>",
  cwd="/root/src/rio-build/main",
  run_in_background=true,
  prompt=<composed>
)
```

## 6. Record launch

Append a `role=writer` row via `onibus state agent-row`:

```bash
.claude/bin/onibus state agent-row \
  '{"plan":"docs-<runid>","role":"writer","agent_id":"<id>","worktree":"/root/src/rio-build/docs-<runid>","status":"running","note":"runid <runid>"}'
```

**Do NOT truncate the sink yet.** The prompt says "read the full sink" but if we truncate here the sink is already empty by the time the planner reads it (race: writer was launched with a rendered table, but the jsonl itself is the durable record). Truncation moves to step 7c — after the rows are durably captured in a commit. If QA bounces, the followups are still available for the next run.

Note in your report: **agent types need a session restart to pick up.** If `rio-planner` is freshly added, the launch will fail with "unknown subagent_type" — restart the session first. Skills (this file) live-reload; agent defs don't.

## 7. Plan-doc gate (after planner returns — before merge)

The planner commits plan docs to `docs-<runid>`. **Before** ff-merging to main and entering `dag.jsonl`, two-stage gate:

### 7a. Mechanical precondition (skill-layer — don't spawn to abort)

Same pattern as `onibus merge atomicity-check` in `/merge-impl` step 0b. Cheap pydantic validation — no agent spawn.

```bash
.claude/bin/onibus plan qa-check /root/src/rio-build/docs-<runid>
```

`qa_mechanical_check` checks: `json files` fence → `list[PlanFile]`; `json deps` ints exist in dag.jsonl (9-digit placeholders skipped); **NO `r[plan.*]` markers (pollution — rio-build tracey is domain-indexed)**; ≥1 `r[domain.*]` marker ref (WARN only). **FAIL → don't spawn reviewer.** `SendMessage` the planner with the pydantic error; planner fixes in-place, re-commit, re-run 7a.

### 7b. Judgment review (spawn `rio-plan-reviewer`)

Precondition passed — docs are mechanically valid. Now: are they *good*?

```
Agent(
  subagent_type="rio-plan-reviewer",
  cwd="/root/src/rio-build/main",
  prompt=f"Worktree: /root/src/rio-build/docs-<runid>. Mechanical precondition passed. Judge: exit-criteria testability, prose sufficiency, anti-patterns (TODO-in-criteria, hedged language)."
)
```

Append `role=qa` row via `onibus state agent-row`.

**Reviewer verdict → action:**

| Verdict | Action |
|---|---|
| PASS | `/merge-impl docs-<runid>` → docs land on main, `dag-append` rows enter `dag.jsonl` |
| FAIL | **Don't merge.** `SendMessage` the planner with the issue table (TODO-in-criteria, hedged language); planner fixes, re-commit, re-run from 7a. |
| WARN | Coordinator judgment: merge-anyway (terse batch task) or bounce-to-planner (complex feature, needs more prose). |

A malformed doc that enters `dag.jsonl` wastes an implementer's time. The precondition catches bad `PlanFile.path`, missing deps, `r[plan.*]` pollution; the reviewer catches vague criteria and thin prose.

### 7c. Truncate sink (post-merge — default mode only)

Now that the rows are durably captured in a merged commit, truncate the sink:

```bash
: > .claude/state/followups-pending.jsonl  # default mode only — skip for --inline
```

If QA FAILed and you abandoned the run: **leave the sink alone** — the next `/plan` invocation re-renders the same followups. Nothing lost.
