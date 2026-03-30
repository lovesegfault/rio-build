# Plan 983457301: Orphan scanner STALE_THRESHOLD can reap live PutPath

[P0483](plan-0483-substitute-stale-uploading-cleanup.md) dropped the orphan
scanner's `STALE_THRESHOLD` from 2h to 15min at
[`rio-store/src/gc/orphan.rs:40`](../../rio-store/src/gc/orphan.rs). The
rationale (doc-comment at `:36-38`) claims "a legitimate upload taking >15min
is pathological (multi-GB over <10Mbps)" — but the math doesn't hold at
realistic throughputs: 6GB over 50Mbps = 48Gbit / 50Mbps = 960s ≈ **16
minutes**. A large NAR over a mid-tier link gets reaped mid-flight.

The `'uploading'` placeholder currently has no heartbeat — `updated_at` is set
once at insert and never bumped during a long upload. So the scanner can't
distinguish "crashed at 12min" from "still streaming at 12min". The uploader's
error-path `delete_manifest_uploading` hitting 0 rows is *not* harmless if the
scanner already decremented chunk refcounts and enqueued them to
`pending_s3_deletes` — the uploader's subsequent `complete_manifest` will
point at chunks that may have been S3-deleted by the drain task.

Three fix routes (implementer picks ONE, document the choice in a
closing-note commit message):

- **Route A (heartbeat):** bump `updated_at` from the uploader loop every
  N chunks (e.g., every 64 chunks or every 30s, whichever first). Scanner
  semantics unchanged. Cost: one extra `UPDATE manifests SET updated_at =
  now() WHERE store_path_hash = $1 AND status = 'uploading'` per heartbeat
  interval. Cheap.
- **Route B (per-source threshold):** split `STALE_THRESHOLD` into
  `STALE_THRESHOLD_SUBSTITUTE` (5min — what P0483 cared about) and
  `STALE_THRESHOLD_PUTPATH` (keep 2h). Requires tagging the placeholder
  with its origin (`manifests.upload_source enum('putpath','substitute')`
  — new migration). More invasive.
- **Route C (revert + hot-path only):** revert orphan scanner to 2h;
  `Substituter::ingest`'s 5-minute hot-path reclaim (`:33` reference in
  the doc-comment) already covers the P0483 concern. The orphan scanner
  goes back to rare-crash-recovery safety-net. Simplest; loses the
  "placeholders nobody re-requests" sweep speedup.

**Route A is preferred** — it's the only one that makes `updated_at`
actually mean "last progress" instead of "insert time", which is what the
scanner's semantics imply. Route C is the fallback if heartbeat turns out
to interact badly with the PutPath transaction structure.

## Entry criteria

- [P0483](plan-0483-substitute-stale-uploading-cleanup.md) merged (lowered threshold + hot-path reclaim exist)

## Tasks

### T1 — `fix(store):` uploader heartbeat — bump manifests.updated_at during long PutPath

MODIFY [`rio-store/src/cas.rs`](../../rio-store/src/cas.rs) (or wherever
`put_chunked`'s upload loop lives). Inside the chunk-upload loop, every 64
chunks (or 30s elapsed, whichever first), fire:

```sql
UPDATE manifests SET updated_at = now()
 WHERE store_path_hash = $1 AND status = 'uploading'
```

Track `last_heartbeat: Instant` and `chunks_since_heartbeat: u32` locally.
The UPDATE is fire-and-forget — ignore errors (if the row's gone, either we
were already reaped or the upload will fail at `complete_manifest` anyway).

Add a doc-comment at the heartbeat site explaining the reap-race it guards.

### T2 — `fix(store):` orphan.rs — correct STALE_THRESHOLD rationale + optionally widen

MODIFY [`rio-store/src/gc/orphan.rs:25-38`](../../rio-store/src/gc/orphan.rs).
The "pathological" claim is wrong. Rewrite the doc-comment to:

- state the heartbeat dependency ("safe because uploaders heartbeat every
  30s/64chunks — see cas.rs:NNN")
- drop the "pathological" framing
- note that `updated_at` now means "last heartbeat", not "insert time"

**If Route A chosen:** 15min is fine with heartbeat (30s heartbeat × 15min
threshold = 30× safety margin). **If Route C chosen instead:** revert to
`2 * 60 * 60` and update store.md:130 prose to match.

### T3 — `test(store):` heartbeat bumps updated_at; scanner skips heartbeating upload

NEW test in [`rio-store/src/gc/orphan.rs`](../../rio-store/src/gc/orphan.rs)
`cfg(test)` mod (or `rio-store/tests/orphan_heartbeat.rs`). Setup: insert a
placeholder manually with `updated_at = now() - 20min`; call the heartbeat fn;
assert `updated_at > now() - 1min`. Then `scan_once()` and assert 0 reaped.
Negative case: same setup WITHOUT heartbeat → `scan_once()` reaps 1.

### T4 — `docs(store):` store.md orphan-scanner prose — heartbeat semantics

MODIFY [`docs/src/components/store.md:130`](../../docs/src/components/store.md).
The "configurable timeout (default: 15 minutes)" prose stays but add:
"Uploaders heartbeat `updated_at` every 30s/64 chunks during long transfers,
so the threshold is safe-vs-live-upload at any realistic throughput." Add
`r[store.gc.orphan-heartbeat]` marker (see Spec additions).

## Exit criteria

- `nix develop -c cargo nextest run -p rio-store orphan` → green
- Heartbeat UPDATE fires during a multi-chunk upload (T3 asserts this)
- `scan_once()` skips a freshly-heartbeated placeholder, reaps a stale one (T3)
- [`orphan.rs:25-38`](../../rio-store/src/gc/orphan.rs) doc-comment no longer claims "pathological"
- `grep 'r\[store.gc.orphan-heartbeat\]' docs/src/components/store.md` → ≥1 hit
- `nix develop -c tracey query rule store.gc.orphan-heartbeat` shows ≥1 `impl` + ≥1 `verify` site

## Tracey

References existing markers:
- `r[store.substitute.stale-reclaim]` — T2's doc-comment cross-references this (hot-path vs safety-net split)

Adds new markers to component specs:
- `r[store.gc.orphan-heartbeat]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) after `:130` (see Spec additions)

## Spec additions

```
r[store.gc.orphan-heartbeat]
Uploaders MUST heartbeat `manifests.updated_at` during long-running chunk uploads (interval: ≤30s or ≤64 chunks, whichever first) so the orphan scanner's stale-threshold check distinguishes in-progress uploads from crashed ones. Without heartbeat, `updated_at` reflects insert time — a 16-minute upload over 50Mbps would be reaped at the 15-minute mark.
```

## Files

```json files
[
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T1: heartbeat UPDATE in put_chunked upload loop"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "T2+T3: doc-comment fix + heartbeat test"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T4: heartbeat prose + r[store.gc.orphan-heartbeat] marker at :130"}
]
```

```
rio-store/src/
├── cas.rs                   # T1: heartbeat in upload loop
└── gc/orphan.rs             # T2: doc fix, T3: heartbeat test
docs/src/components/store.md # T4: marker + prose
```

## Dependencies

```json deps
{"deps": [483], "soft_deps": [], "note": "P0483 dropped the threshold from 2h→15min; this fixes the live-upload reap it introduced. Route-A heartbeat is a pure-add to cas.rs upload loop — no conflict with P0483's orphan.rs/substitute.rs changes. discovered_from=483."}
```

**Depends on:** [P0483](plan-0483-substitute-stale-uploading-cleanup.md) — lowered threshold + hot-path reclaim exist.
**Conflicts with:** [`orphan.rs`](../../rio-store/src/gc/orphan.rs) was just touched by P0483; [`cas.rs`](../../rio-store/src/cas.rs) is hot (check `onibus collisions check rio-store/src/cas.rs` at dispatch). [`store.md`](../../docs/src/components/store.md) touched by P0295-T116 (`:195` region, non-overlapping with `:130`).
