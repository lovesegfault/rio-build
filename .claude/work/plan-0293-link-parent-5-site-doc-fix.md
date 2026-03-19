# Plan 0293: Fix 5 sites claiming link_parent() = same trace_id (VM-proven false)

**Retro P0160 finding.** 5 sites claim `link_parent()` → "same trace_id / contiguous trace." **VM-PROVEN FALSE** (P0160 round-4, commit [`3204e4af`](https://github.com/search?q=3204e4af&type=commits)). The pattern:

```rust
#[instrument(...)]
async fn handler(&self, request: ...) {
    rio_proto::interceptor::link_parent(&request);  // set_parent() on Span::current()
```

`#[instrument]` creates the span at function entry with its own trace_id. `set_parent()` called after creates an OTel **span LINK**, not a parent-child relationship — the span keeps its own trace_id. Jaeger shows two traces connected by a link. Proof is in [`nix/tests/scenarios/observability.nix:268-283`](../../nix/tests/scenarios/observability.nix). Last touch to `interceptor.rs` was 72 minutes AFTER the proof; nobody fixed the docstrings.

The 5 lying sites:
- [`rio-proto/src/interceptor.rs:42-43`](../../rio-proto/src/interceptor.rs) — module-doc example: "Server-side is the same: `link_parent` + `Span::current().set_parent()`"
- [`rio-proto/src/interceptor.rs:136-139`](../../rio-proto/src/interceptor.rs) — `link_parent` docstring: "`set_parent` retroactively stitches that span into [the caller's trace]"
- [`rio-scheduler/src/grpc/mod.rs:355-358`](../../rio-scheduler/src/grpc/mod.rs) — comment before `link_parent` call: "stitches it to the client's trace_id. Everything [is one trace]"
- [`rio-scheduler/src/grpc/mod.rs:455-459`](../../rio-scheduler/src/grpc/mod.rs) — similar (check actual line numbers at impl time; retrospective may be approximate)
- [`docs/src/observability.md:255`](../../docs/src/observability.md) — the long `r[sched.trace.assignment-traceparent]` paragraph says "`link_parent()` stitches the `#[instrument]` span into the incoming parent"

**Soft-dep on [P0287](plan-0287-trace-linkage-submitbuild-metadata.md):** if P0287 merges first, the OPERATOR-VISIBLE behavior is fixed (STDERR_NEXT emits the scheduler's trace_id, which IS the one that spans scheduler→worker). The 5 sites' claims stay technically false (scheduler span still has its own trace_id, LINKED not parented) — but the docstrings can then say "see `r[obs.trace.scheduler-id-in-metadata]` for the operator fix" instead of "see TODO(phase4b)."

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `docs:` branch on P0287 status at impl time

**IF [P0287](plan-0287-trace-linkage-submitbuild-metadata.md) has merged:**

Rewrite the 5 sites to describe the ACTUAL mechanism + point to the fix:

> Creates an OTel **span LINK** to the incoming traceparent — NOT a parent.
> `#[instrument]` created this span at function entry with its own trace_id;
> `set_parent()` called after produces a link, not a parent-child edge.
> Jaeger shows two traces connected by the link. The scheduler's trace_id
> is returned via `x-rio-trace-id` (per `r[obs.trace.scheduler-id-in-metadata]`);
> operators grep THAT, it spans scheduler→worker via the data-carry.

**IF P0287 has NOT merged:**

Same rewrite but the tail is:

> Jaeger shows two traces connected by the link — see TODO(phase4b) at
> `observability.nix:268` for the operator-visible consequence.

Either way, the "same trace_id / contiguous trace / stitches into" phrasing dies.

### T2 — `docs:` observability.md:255 paragraph surgery

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) at `:255` — the long paragraph currently says:

> `rio_proto::interceptor::link_parent()` stitches the `#[instrument]` span into the incoming parent (server side, first line of each handler).

Change to:

> `rio_proto::interceptor::link_parent()` adds an OTel span **link** to the
> incoming traceparent (server side, first line of each handler). The
> `#[instrument]` span was already created with its own trace_id;
> `set_parent()` after-the-fact creates a link, not a parent-child edge.

The scheduler→worker data-carry prose (rest of that paragraph) stays correct — that mechanism IS actual parenting (via `span_from_traceparent()` + `.instrument(span)` wrapping, which creates the span WITH the parent, not after).

## Exit criteria

- `/nbr .#ci` green (clippy-only — no behavior change)
- `grep 'stitches.*into.*parent\|same trace_id\|contiguous trace' rio-proto/src/interceptor.rs` → 0 hits
- `grep 'stitches.*into.*parent' rio-scheduler/src/grpc/mod.rs` → 0 hits
- `grep 'stitches the.*span into the incoming parent' docs/src/observability.md` → 0 hits
- All 5 sites describe LINK-not-parent explicitly

## Tracey

References existing markers:
- `r[obs.trace.w3c-traceparent]` — T2 edits the paragraph near this marker (marker itself unchanged)
- `r[sched.trace.assignment-traceparent]` — T2 edits the prose of this marker's paragraph (the data-carry half stays correct; only the `link_parent` claim changes)

If P0287 has merged, T1 also references:
- `r[obs.trace.scheduler-id-in-metadata]` — P0287's new marker; T1 points at it

## Files

```json files
[
  {"path": "rio-proto/src/interceptor.rs", "action": "MODIFY", "note": "T1: fix 2 sites (:42-43 module-doc, :136-139 docstring) — LINK not parent"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: fix 2 sites (~:355-358, ~:455-459) — LINK not parent"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T2: paragraph surgery at :255 — link_parent creates LINK, data-carry is actual parenting"}
]
```

```
rio-proto/src/interceptor.rs     # T1: 2 sites
rio-scheduler/src/grpc/mod.rs    # T1: 2 sites
docs/src/observability.md        # T2: paragraph
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [287], "note": "retro P0160 — discovered_from=160. 5 sites claim link_parent=same-trace-id, VM-proven false (3204e4af round-4). soft_dep 287: if P0287 merges first, the 5-site rewrite points at r[obs.trace.scheduler-id-in-metadata] (operator fix exists). If not, points at TODO(phase4b). Either way 'stitches into parent' phrasing dies."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Soft-depends on:** [P0287](plan-0287-trace-linkage-submitbuild-metadata.md) — not a blocker. If P0287 is DONE at impl time, rewrite text points at its marker. If UNIMPL, points at the TODO. T1 branches on status.
**Conflicts with:** [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) also touched by P0287 (T2 sets header at `:503`, this plan edits comment at `:357` — different lines). [`observability.md`](../../docs/src/observability.md) also touched by P0287 (spec addition, different section) + P0288 (metric table, different section). [`interceptor.rs`](../../rio-proto/src/interceptor.rs) low-traffic.
