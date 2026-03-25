# Plan 0433: Trailer-refs protocol extension — inline ref-scan with upload tee

sprint-1 cleanup finding, deferred remainder of [P0181](plan-0181-worker-upload-references-scan.md). Worker upload at [`rio-worker/src/upload.rs:177`](../../rio-worker/src/upload.rs) does a **separate pre-scan disk pass** to compute references before streaming, because references go in `PathInfo` (the FIRST gRPC message — trailer-mode protocol requires metadata at index 0) and we can't know refs until the dump finishes. A trailer-refs protocol extension would move refs into the PutPath trailer so the scan happens inline with the upload tee, avoiding the extra disk pass.

**Gated on measuring pre-scan cost at scale** (see `docs/src/components/worker.md` § pre-scan cost). Scope-creeps into store `put_path.rs`, `ValidatedPathInfo`, and the re-sign path — hence deferred from P0181. Promoted from the orphan `TODO(P0181-followup)` comment block.

## Entry criteria

- Production metrics confirm pre-scan disk pass is a measurable bottleneck (otherwise the proto churn isn't worth it)
- [P0181](plan-0181-worker-upload-references-scan.md) merged (DONE — it added the pre-scan)

## Tasks

### T1 — `feat(proto):` PutPath trailer — optional refs field

MODIFY [`rio-proto/proto/store.proto`](../../rio-proto/proto/store.proto). Add an optional `repeated string references` field to the PutPath trailer message (or a new `PutPathTrailer` message if none exists). Backward-compatible: old clients that send refs in PathInfo still work; new clients send empty refs in PathInfo and populate the trailer.

### T2 — `feat(store):` put_path handler — accept refs from trailer

MODIFY [`rio-store/src/grpc/put_path.rs`](../../rio-store/src/grpc/put_path.rs). If PathInfo.references is empty AND trailer.references is non-empty, use trailer refs. ValidatedPathInfo construction moves to after trailer receipt. Re-sign path needs the same treatment.

### T3 — `feat(worker):` three-way tee — scan refs inline with upload stream

MODIFY [`rio-worker/src/upload.rs`](../../rio-worker/src/upload.rs) around `:177`. Replace the separate pre-scan pass with a three-way tee: dump → (upload stream, hash sink, ref-scanner sink). Send refs in the trailer. Delete the `TODO(P0433)` comment block.

### T4 — `test(store):` PutPath accepts trailer-refs

Integration test: client sends PathInfo with empty refs, streams NAR, sends trailer with refs. Assert store records the trailer refs in narinfo.

### T5 — `test(worker):` upload does single disk pass

Unit or VM test: assert the output path is read exactly once (instrument with a counting reader or check `rio_worker_upload_disk_passes` metric if one exists).

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'separate pre-scan\|pre-scan disk pass' rio-worker/src/upload.rs` → 0 hits (comment block gone, pass eliminated)
- Backward-compat: old-client test (refs in PathInfo, no trailer) still passes

## Tracey

References `r[worker.upload.references-scanned]` — the trailer-refs approach still satisfies it (refs are scanned and uploaded), just inline. May need a new `r[store.put.trailer-refs]` marker for the proto extension.

## Files

```json files
[
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "T1: +references field to PutPath trailer"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T2: accept trailer refs; ValidatedPathInfo construction after trailer"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T3: three-way tee replaces pre-scan pass at :177"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "Update pre-scan cost section to reflect trailer-refs elimination"}
]
```

## Dependencies

```json deps
{"deps": [181], "soft_deps": [434], "note": "sprint-1 cleanup finding (discovered_from=rio-worker cleanup worker, orphan TODO P0181-followup). Hard-dep P0181 (DONE — the pre-scan this replaces). Soft-dep P0434 (manifest-mode upload also touches upload.rs streaming path — coordinate to avoid rebase churn). GATED on production pre-scan cost measurement — do not dispatch until metrics confirm bottleneck."}
```

**Depends on:** [P0181](plan-0181-worker-upload-references-scan.md) — DONE.
**Conflicts with:** [`upload.rs`](../../rio-worker/src/upload.rs) — [P0434](plan-0434-manifest-mode-upload-bandwidth-opt.md) touches `:547` manifest-mode section; T3 here touches `:177` pre-scan section. Same file, different sections. [`put_path.rs`](../../rio-store/src/grpc/put_path.rs) moderate-traffic — several plans touch it.
