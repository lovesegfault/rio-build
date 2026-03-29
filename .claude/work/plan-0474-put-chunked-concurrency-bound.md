# Plan 474: Bound put_chunked internal chunk-upload concurrency

**THE LAST rsb BLOCKER.** Manual rsb testing on kind with a real closure (python3: 374 MB NAR → ~1900 FastCDC chunks) found that [`put_chunked`](../../rio-store/src/cas.rs) at step 3 (parallel upload, extracted to a helper at `:189`) fires all chunk uploads concurrently with no bound. The aws-sdk-s3 connection pool saturates, requests queue in the hyper client, and eventually `DispatchFailure` errors cascade. The substitution flow then rolls back the manifest and the rsb user sees `nix build` fail on a path that *exists* upstream.

[P0473](../../rio-scheduler/src/main.rs) (commit `ceaebfd6`) bounded the scheduler→store eager-fetch fan-out to 16 concurrent `SubstitutePath` RPCs. That fixed the scheduler-side thundering herd. But each of those 16 RPCs internally calls `put_chunked`, and each `put_chunked` fires 1000+ concurrent S3 `PutObject` calls. The bound needs to move one layer down.

The fix mirrors P0473's shape exactly: wrap the chunk-upload stream in `futures::stream::iter(chunks).buffer_unordered(N)`, expose `N` as `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` (default 32 — tuned so 16 scheduler-side × 32 store-side = 512 in-flight, comfortably under aws-sdk's default 1024-connection pool with headroom for other store traffic).

## Entry criteria

- P0473 merged (`ceaebfd6` — scheduler-side `RIO_SUBSTITUTE_MAX_CONCURRENT` bound). Already on `sprint-1`.

## Tasks

### T1 — `fix(store):` bound chunk-upload concurrency with buffer_unordered

MODIFY [`rio-store/src/cas.rs`](../../rio-store/src/cas.rs). The parallel-upload helper at `:189` currently collects all chunk uploads and `try_join_all`s them. Replace with a bounded stream:

```rust
use futures::stream::{self, StreamExt, TryStreamExt};

stream::iter(chunk_hashes)
    .map(|h| upload_one(bucket, h))
    .buffer_unordered(cfg.chunk_upload_max_concurrent)
    .try_collect::<Vec<_>>()
    .await?;
```

Where `cfg.chunk_upload_max_concurrent: usize` comes from `StoreConfig`.

### T2 — `feat(store):` RIO_CHUNK_UPLOAD_MAX_CONCURRENT config knob

MODIFY [`rio-store/src/config.rs`](../../rio-store/src/config.rs) (or wherever `StoreConfig` lives — grep for the `RIO_` env-var pattern). Add:

```rust
/// Max concurrent S3 chunk uploads per put_chunked call.
/// Default 32 — with RIO_SUBSTITUTE_MAX_CONCURRENT=16 (scheduler side)
/// this caps total in-flight S3 puts at ~512.
#[arg(long, env = "RIO_CHUNK_UPLOAD_MAX_CONCURRENT", default_value_t = 32)]
pub chunk_upload_max_concurrent: usize,
```

Thread the value from `main.rs` → `CasStore::new()` → `put_chunked()`. If `put_chunked` is a free function, either add a param or stash the limit on whatever struct already carries the `aws_sdk_s3::Client`.

### T3 — `test(store):` concurrency-bound unit test

NEW test in [`rio-store/src/cas.rs`](../../rio-store/src/cas.rs) `#[cfg(test)]` mod (or `rio-store/tests/cas.rs`). Use a mock S3 client that records max-observed-concurrent-calls via an `Arc<AtomicUsize>` (increment on entry, decrement on exit, track high-water-mark). Feed `put_chunked` a synthetic 200-chunk NAR with `chunk_upload_max_concurrent = 8`, assert `high_water <= 8`.

Grep `rio-test-support/src/` for `AtomicUsize` before writing a new recorder — the mock-S3 may already have one.

### T4 — `docs(obs):` document the knob in observability.md + values.yaml

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` to the store env-var table alongside `RIO_SUBSTITUTE_MAX_CONCURRENT`.

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml) — add the knob under `store:` with a comment explaining the 16×32=512 interaction.

## Exit criteria

- `cargo nextest run -p rio-store put_chunked_concurrency_bounded` → pass; high-water-mark assert `<= N`
- `grep 'buffer_unordered' rio-store/src/cas.rs` → ≥1 hit in the upload path
- `grep 'RIO_CHUNK_UPLOAD_MAX_CONCURRENT' rio-store/src/ docs/src/observability.md infra/helm/rio-build/values.yaml` → ≥3 hits (config def + doc + helm)
- Manual rsb smoke: `cargo xtask k8s deploy kind && nix build --store ssh-ng://... nixpkgs#python3` completes without `DispatchFailure` (confirmatory — not CI-gated since kind+S3 isn't in `.#ci`)
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[store.cas.fastcdc]` — T1 constrains the upload half of the chunked-put flow
- `r[store.substitute.upstream]` — T1 makes the substitution ingest path reliable under load

Adds new marker to component specs:
- `r[store.cas.upload-bounded]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) (see Spec additions below)

## Spec additions

Add to [`docs/src/components/store.md`](../../docs/src/components/store.md) after `r[store.cas.fastcdc]`:

```
r[store.cas.upload-bounded]
Chunk uploads within a single `put_chunked` call MUST be bounded to `RIO_CHUNK_UPLOAD_MAX_CONCURRENT` (default 32) concurrent S3 operations. Unbounded fan-out on large NARs (>1000 chunks) saturates the aws-sdk connection pool and produces `DispatchFailure` errors.
```

## Files

```json files
[
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T1: buffer_unordered wrap at :189 upload helper; T3: concurrency test"},
  {"path": "rio-store/src/config.rs", "action": "MODIFY", "note": "T2: RIO_CHUNK_UPLOAD_MAX_CONCURRENT knob"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T2: thread knob to CasStore"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: env-var table row"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "spec addition: r[store.cas.upload-bounded]"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T4: store.chunkUploadMaxConcurrent"}
]
```

```
rio-store/src/
├── cas.rs              # T1: buffer_unordered; T3: test
├── config.rs           # T2: knob
└── main.rs             # T2: thread knob
docs/src/
├── observability.md    # T4: env-var row
└── components/store.md # spec: r[store.cas.upload-bounded]
infra/helm/rio-build/
└── values.yaml         # T4: knob comment
```

## Dependencies

```json deps
{"deps": [473], "soft_deps": [], "note": "P0473 bounded scheduler-side; this bounds store-side. Both needed for rsb reliability."}
```

**Depends on:** P0473 (scheduler-side `RIO_SUBSTITUTE_MAX_CONCURRENT`, `ceaebfd6`) — already merged to `sprint-1`. This plan's default-32 is tuned assuming P0473's default-16.

**Conflicts with:** [P0475](plan-0475-pg-deadlock-chunk-rollback.md) also touches `rio-store/src/cas.rs` (rollback path `:252`). Serialize: P0474 first (blocker), P0475 second (defensive). The edits are in different functions so a rebase should be clean, but landing P0474 first keeps rsb unblocked.
