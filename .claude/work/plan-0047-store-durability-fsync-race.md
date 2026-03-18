# Plan 0047: Store durability — fsync + concurrent-PutPath race + placeholder cleanup

## Context

Three durability/concurrency bugs in the store's write path:

**No fsync.** `FilesystemBackend::put` did `tokio::fs::write` + `rename` with no `fsync`. After `put()` returns, `complete_upload()` flips `nar_blobs.status='complete'` in PostgreSQL (which *does* fsync its commit). Power loss in the window between rename-return and kernel page-cache flush: PG says `status='complete'`, filesystem has a zero-length or missing `.nar`. Subsequent `GetPath` serves an empty NAR → `DATA_LOSS`. But the path is permanently stuck: `PutPath` retry returns `created=false` (idempotency short-circuit on `check_complete`), and there's no repair path.

**Concurrent PutPath race.** `insert_uploading` uses `ON CONFLICT DO NOTHING` and returns `bool`. The caller discarded it. Race: A inserts placeholder, B sees `check_complete=None` (A not done), B's `insert_uploading` → `false` (no-op), B proceeds anyway, B fails validation, B `delete_uploading` → deletes *A's* placeholder. A's `complete_upload` fails "placeholder missing" → A's valid upload lost.

**Placeholder leaks on error.** Four post-insert error paths (`try_from` capacity, `stream.message()` bare `?`, chunk-size-exceeded, duplicate-metadata) returned without `delete_uploading`. Retry blocked indefinitely.

## Commits

- `11cfb0e` — fix(rio-store): fsync blob and parent directory before reporting put success
- `3f3b293` — fix(rio-store): check insert_uploading return to prevent concurrent PutPath race
- `56c3d63` — fix(rio-store): clean up uploading placeholder on all PutPath error paths
- `4b54c3b` — refactor(rio-store): capture backend.put return value for blob_key consistency
- `da10510` — fix(rio-store): propagate filesystem exists() I/O errors
- `4726a39` — test(rio-store): add GetPath DATA_LOSS integrity test and NotFound test

(Discontinuous — commits 100–102, 115, 118, 125.)

## Files

```json files
[
  {"path": "rio-store/src/backend/filesystem.rs", "action": "MODIFY", "note": "File::create+write_all+sync_all, rename, sync_all on PARENT dir (fsync the dirent); try_exists().await? (was: unwrap_or(false) hid permission errors)"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "insert_uploading return checked: false → drain stream, re-check complete, Aborted if still uploading; abort_upload() helper (delete_uploading + log + error-metric); all 7 post-insert error paths use it; capture actual_blob_key from backend.put() → complete_upload"},
  {"path": "rio-store/src/backend/memory.rs", "action": "MODIFY", "note": "corrupt_for_test() for DATA_LOSS test"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "test_concurrent_putpath_same_path_one_wins (join two futures, exactly one created=true); test_put_path_oversized_then_retry_succeeds (proves cleanup); test_put_path_rejects_duplicate_metadata; test_get_path_corrupted_blob_returns_data_loss; test_get_path_nonexistent_returns_not_found"}
]
```

## Design

**fsync sequence:** `File::create` + `write_all` + `sync_all` (fsync data + metadata), `rename`, then `sync_all` on the *parent directory* — fsyncs the dirent so the rename itself is durable. This is the POSIX-correct sequence for crash-consistent file creation.

**Concurrent handling:** if `insert_uploading` returns `false` (another uploader's placeholder exists), drain the stream, re-check `complete`. If it flipped (other uploader finished) → `created=false`. Else → `Aborted`, client retries. The drain is important — can't just error immediately, the client's stream is half-sent.

**`abort_upload()` helper:** `delete_uploading` + `error!`-on-cleanup-failure + `put_path_total{result="error"}++`. Used at all 7 post-insert error paths. The error metric is now actually computable for SLI dashboards.

**blob_key capture (`4b54c3b`):** `backend.put()` returns the key. Code discarded it and used a pre-computed `"{sha}.nar"`. Both produce the same thing today, but if a backend adds sharding (`"ab/abcd...nar"`) the metadata row's `blob_key` would silently drift from the actual key → `GetPath` 404.

**DATA_LOSS test (`4726a39`):** the `HashingReader` integrity check at `GetPath` was the *only* bitrot defense for the entire distributed cache and had zero test coverage. `PutPath` a valid NAR, `MemoryBackend::corrupt_for_test`, `GetPath` → assert stream delivers chunks then `Status::data_loss` mentioning "integrity".

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[store.fs.fsync-sequence]`, `r[store.putpath.concurrent-race]`, `r[store.getpath.integrity-verify]`.

## Outcome

Merged as `11cfb0e..56c3d63`, `4b54c3b`, `da10510`, `4726a39` (6 commits, discontinuous). `test_concurrent_putpath_same_path_one_wins` joins two concurrent futures. `test_put_path_oversized_then_retry_succeeds` proves placeholder cleanup. `test_get_path_corrupted_blob_returns_data_loss` proves the bitrot defense works.
