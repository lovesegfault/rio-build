# Plan 0151: Traceparent through WorkAssignment — SSH-boundary trace propagation

## Design

The core observability gap phase 4a was chartered to close: ssh-ng has no gRPC metadata channel, so the standard `inject_current`/`link_parent` pattern cannot carry trace context from gateway to worker. The scheduler dispatches work over a long-lived bidi stream (`BuildExecution`), and each `WorkAssignment` is a message on that stream — there's no per-message metadata.

The fix: per-assignment traceparent travels **in the payload**. `WorkAssignment` proto gains `string traceparent = 9` (W3C traceparent format). The scheduler populates it at dispatch time; the worker parses it via `TraceContextPropagator::extract` and wraps the spawned build task with `.instrument(span)`. Empty traceparent → fresh root span (same as the gRPC no-header case).

Four commits landed the full chain: proto field + `inject_current` on two orphaned client helpers (`61ecb98`); scheduler-side `current_traceparent()` helper + dispatch population + `link_parent` on three admin handlers that were missing it (`650f9d0`); worker-side `span_from_traceparent()` bundle + `#[instrument]` on `spawn_build_task`/`handle_prefetch_hint` + `inject_current` before `put_path` (`2bae701`); and store-side `link_parent` on `content_lookup`/`trigger_gc`/`pin_path`/`unpin_path` (`fb01825`). A fifth commit (`b9b7fc3`) added the gateway's `STDERR_NEXT` emission of the trace_id — gives operators a grep handle for Tempo.

**Critical follow-on (see P0159 round 3):** this implementation had a fatal bug. The scheduler's `dispatch.rs` called `current_traceparent()` which captured the current span context — but span context does NOT cross the mpsc channel to the actor task. The actor's `#[instrument]` span is a fresh root. The worker received a valid traceparent but it belonged to a disjoint trace. Round 3 fixed this by carrying traceparent as plain data through `MergeDagRequest` → `DerivationState` → `WorkAssignment` instead of reading the ambient span.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "WorkAssignment.traceparent field 9"},
  {"path": "rio-proto/src/interceptor.rs", "action": "MODIFY", "note": "current_traceparent(), extract_traceparent(), span_from_traceparent() helpers"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "inject_current in query_path_info_opt + get_path_nar (previously orphaned store spans)"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "populate WorkAssignment.traceparent via current_traceparent()"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "inject_current on find_missing_paths"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "inject_current on orphan reconcile store calls"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "link_parent on cluster_status/trigger_gc/drain_worker"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "#[instrument] on spawn_build_task + handle_prefetch_hint; span_from_traceparent wrap"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "inject_current before store_client.put_path"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "link_parent on content_lookup/trigger_gc/pin_path/unpin_path"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "emit trace_id via STDERR_NEXT at SubmitBuild time"}
]
```

## Tracey

- `r[impl sched.trace.assignment-traceparent]` — in `2bae701` (worker-side parse), later also tagged in `20e557f` (scheduler-side dispatch)
- `r[verify sched.trace.assignment-traceparent]` — added retroactively in `20e557f`; proper verify test added in round 3 (`8196337`)

2 marker annotations in this cluster's commits; VM assertion in P0157.

## Entry

- Depends on P0148: phase 3b complete (extends existing tracing infrastructure)

## Exit

Merged as `61ecb98..b9b7fc3` (5 commits). `.#ci` green. scheduler→worker→store chain parents correctly; gateway→scheduler linkage later shown broken in round 4 (deferred to `TODO(phase4b)`).
