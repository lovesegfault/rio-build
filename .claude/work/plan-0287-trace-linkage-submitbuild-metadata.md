# Plan 0287: Gateway→scheduler trace linkage via SubmitBuild initial metadata

**Retro P0151 finding.** P0160 round-4 **proved** gateway→scheduler linkage NEVER worked. The pattern at [`rio-scheduler/src/grpc/mod.rs:350-359`](../../rio-scheduler/src/grpc/mod.rs) is:

```rust
#[instrument(skip(self, request), fields(rpc = "SubmitBuild"))]
async fn submit_build(...) {
    // ...
    rio_proto::interceptor::link_parent(&request);  // set_parent() on already-created span
```

`#[instrument]` creates the span at function entry. `link_parent()` then calls `set_parent()` on `tracing::Span::current()` — but the span was already opened with its own trace_id. Result: orphan with its own trace_id, **LINKED** to the gateway trace (OTel span link) but NOT a child. Jaeger shows two traces, not one.

[`nix/tests/scenarios/observability.nix:268-283`](../../nix/tests/scenarios/observability.nix) has the `TODO(phase4b)` — **36 commits past phase-4a tag, still unlanded.** Assertion stays GATEWAY-ONLY.

Only scheduler→worker works (round-3 data-carry via `WorkAssignment.traceparent`, [`observability.md:255`](../../docs/src/observability.md)). rem-14 (P0193, span-propagation) does NOT cover this — it's about `spawn_monitored` component-field propagation, zero overlap with `interceptor.rs`/`grpc/mod.rs`.

**User decision: option (b)** from [`observability.nix:279-280`](../../nix/tests/scenarios/observability.nix) — scheduler returns its trace_id in `SubmitBuild` initial gRPC metadata, gateway emits THAT in `STDERR_NEXT`. Same pattern as P0199's `x-rio-build-id` header ([`rio-proto/src/lib.rs:25`](../../rio-proto/src/lib.rs), set at [`grpc/mod.rs:503`](../../rio-scheduler/src/grpc/mod.rs), read at [`handler/build.rs:257`](../../rio-gateway/src/handler/build.rs)). **Metadata-only** — no proto body field, no `.proto` recompile.

Why (b) over (a): option (a) ("create handler spans manually AFTER extracting traceparent") ripples through every `#[instrument]` site (7 in `grpc/mod.rs` alone per grep). Option (b) is 3 touched files + un-TODOs the VM assertion.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `feat(proto):` TRACE_ID_HEADER constant

MODIFY [`rio-proto/src/lib.rs`](../../rio-proto/src/lib.rs) — alongside `BUILD_ID_HEADER` at `:25`:

```rust
/// gRPC metadata key: scheduler's trace_id for the SubmitBuild span.
/// Set by the scheduler AFTER link_parent() so it reflects the actual
/// trace the handler is in (which, due to the #[instrument] + set_parent
/// ordering, is a NEW trace LINKED to the gateway's — not a child).
/// Gateway emits this in STDERR_NEXT so operators grep the RIGHT trace.
pub const TRACE_ID_HEADER: &str = "x-rio-trace-id";
```

### T2 — `feat(scheduler):` set x-rio-trace-id in SubmitBuild response metadata

MODIFY [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) — at the same spot `BUILD_ID_HEADER` is set (`:503`, after `MergeDag` commits, before streaming starts):

```rust
// r[impl obs.trace.scheduler-id-in-metadata]
// Set x-rio-trace-id alongside x-rio-build-id. The scheduler's
// #[instrument] span was created BEFORE link_parent() ran, so it
// has its OWN trace_id (LINKED to gateway's, not parented). Gateway
// emits THIS id in STDERR_NEXT — operators grep the scheduler trace,
// which IS the one that spans scheduler→worker (via the data-carry
// at observability.md:255). The gateway's trace_id gets them to a
// trace with only gateway spans; this one gets them to the full chain.
let trace_id = rio_proto::interceptor::current_trace_id_hex();
if !trace_id.is_empty() {
    metadata.insert(
        rio_proto::TRACE_ID_HEADER,
        trace_id.parse().unwrap_or_else(|_| "invalid".parse().unwrap()),
    );
}
```

### T3 — `feat(gateway):` emit scheduler's trace_id in STDERR_NEXT, not gateway's

MODIFY [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) at [`~:321-330`](../../rio-gateway/src/handler/build.rs):

```rust
// Emit trace_id to the client via STDERR_NEXT. PRIORITIZE the
// scheduler's trace_id (x-rio-trace-id header) over our own —
// the scheduler span is the one that actually spans the full
// scheduler→worker chain (data-carry per observability.md:255).
// Our own trace only has gateway spans (link_parent on the
// scheduler side creates a LINK, not a parent — see P0287).
// Fallback to our own for legacy schedulers.
let trace_id = resp.metadata()
    .get(rio_proto::TRACE_ID_HEADER)
    .and_then(|v| v.to_str().ok())
    .map(str::to_owned)
    .filter(|s| !s.is_empty())
    .unwrap_or_else(rio_proto::interceptor::current_trace_id_hex);
if !trace_id.is_empty() {
    let _ = stderr.log(&format!("rio trace_id: {trace_id}\n")).await;
}
```

**Moved AFTER `resp` is available** — the current emission at `:328` fires before the `SubmitBuild` call completes. Move it to after `:270` (where `resp.metadata()` is readable, same spot `x-rio-build-id` is read).

### T4 — `test:` un-TODO observability.nix — assert scheduler+worker spans in the emitted trace

MODIFY [`nix/tests/scenarios/observability.nix`](../../nix/tests/scenarios/observability.nix):

1. Delete the `TODO(phase4b)` block at `:268-283`
2. Strengthen the assertion: the emitted trace_id must appear in ≥1 scheduler span AND ≥1 worker span in the collector file (currently "stays GATEWAY-ONLY")

```python
# r[verify obs.trace.scheduler-id-in-metadata]
with subtest("trace-id-propagation: STDERR_NEXT id spans scheduler+worker"):
    m = re.search(r"rio trace_id: ([0-9a-f]{32})", output)
    assert m, f"expected 'rio trace_id: <32-hex>'; got: {output[:500]!r}"
    tid = m.group(1)
    # The emitted id is now the SCHEDULER's trace_id (x-rio-trace-id
    # header), which the data-carry chain extends through worker.
    # Collector file should show this trace_id on spans from ≥2 services.
    spans = json.loads(client.succeed(f"cat /var/log/otel/spans.json"))
    services_in_trace = {
        s['resource']['service.name']
        for s in spans if s['traceId'] == tid
    }
    assert 'rio-scheduler' in services_in_trace, \
        f"scheduler not in trace {tid}: {services_in_trace}"
    assert 'rio-worker' in services_in_trace, \
        f"worker not in trace {tid}: {services_in_trace}"
```

### T5 — `test(scheduler):` unit — header set after link_parent

NEW test in [`rio-scheduler/src/grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) — exercise `submit_build` with an injected `traceparent` in request metadata, assert response metadata has `x-rio-trace-id` set to a 32-hex string that **differs** from the injected traceparent's trace_id portion (proving the scheduler span is its own trace, LINKED not parented — this is the CURRENT behavior, documented, not a bug to fix here).

## Exit criteria

- `/nbr .#ci` green — includes the strengthened observability.nix assertion
- `grep TRACE_ID_HEADER rio-proto/src/lib.rs` → 1 hit
- `grep TODO observability.nix | grep -i 'gateway.only\|phase4b.*link_parent'` → 0 hits
- The emitted `rio trace_id:` line in VM output matches scheduler+worker spans (T4 assertion passes)
- [P0293](plan-0293-link-parent-5-site-doc-fix.md) becomes a spot-check: the 5 "same trace_id" claims stay FALSE (scheduler still has its own trace_id), but the operator-visible `STDERR_NEXT` id is now USEFUL

## Tracey

References existing markers:
- `r[obs.trace.w3c-traceparent]` — T2 sets header in the W3C format's spirit (raw hex trace_id)
- `r[sched.trace.assignment-traceparent]` — unchanged; this is the scheduler→worker data-carry. T3's emitted id is the trace THAT chain is part of.

Adds new markers to component specs:
- `r[obs.trace.scheduler-id-in-metadata]` → `docs/src/observability.md` (see ## Spec additions below)

## Spec additions

New paragraph in [`docs/src/observability.md`](../../docs/src/observability.md), inserted after `r[sched.trace.assignment-traceparent]` (after the long paragraph at `:255`):

```markdown
r[obs.trace.scheduler-id-in-metadata]

The scheduler sets `x-rio-trace-id` in `SubmitBuild` response metadata to its
handler span's trace_id (captured AFTER `link_parent()`). The gateway emits
THIS id in `STDERR_NEXT` (`rio trace_id: <32-hex>`), not its own. Rationale:
`link_parent()` + `#[instrument]` produces an orphan — the scheduler handler
span has its own trace_id, LINKED to the gateway trace but not parented.
The gateway's trace contains only gateway spans; the scheduler's trace is the
one extended through worker via the `WorkAssignment.traceparent` data-carry.
Operators grepping the emitted id land in the trace that actually spans the
full scheduler→worker chain.
```

## Files

```json files
[
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T1: TRACE_ID_HEADER constant alongside BUILD_ID_HEADER"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T2: set x-rio-trace-id at same spot as x-rio-build-id (~:503)"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T3: emit scheduler's trace_id from header, fallback to own; move emission after resp available"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T4: delete TODO(phase4b), strengthen assertion to scheduler+worker in trace"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T5: unit test — header set, differs from injected traceparent"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "Spec addition: r[obs.trace.scheduler-id-in-metadata]"}
]
```

```
rio-proto/src/lib.rs                      # T1: constant
rio-scheduler/src/grpc/
├── mod.rs                                # T2: set header
└── tests.rs                              # T5: unit
rio-gateway/src/handler/build.rs          # T3: read + emit
nix/tests/scenarios/observability.nix     # T4: un-TODO + strengthen
docs/src/observability.md                 # spec addition
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0151 — discovered_from=151. P0160 round-4 PROVED link_parent+#[instrument] = orphan (LINKED not parented). Option (b) from observability.nix:279. Same pattern as P0199 x-rio-build-id. Metadata-only, no .proto. Obsoletes the P0160 doc-rot half-fix — P0293 becomes a spot-check: the 5 'same trace_id' claims stay false but the STDERR_NEXT id is now the USEFUL one."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Soft-blocks:** [P0293](plan-0293-link-parent-5-site-doc-fix.md) — if this merges first, P0293's 5-site rewording says "emits scheduler trace_id per `r[obs.trace.scheduler-id-in-metadata]`" instead of "LINKED not parented — see TODO."
**Conflicts with:** [`rio-scheduler/src/grpc/mod.rs`](../../rio-scheduler/src/grpc/mod.rs) moderate-traffic file. [P0293](plan-0293-link-parent-5-site-doc-fix.md) touches same file (comment at `:357`) — trivial merge. [`observability.md`](../../docs/src/observability.md) also touched by P0288 (different section, metric table).
