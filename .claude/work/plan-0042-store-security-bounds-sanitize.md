# Plan 0042: Store security hardening — bounds, path validation, error sanitization, MAX_NAR_SIZE

## Context

The store service accepts untrusted input (via gateway, which forwards untrusted Nix client data). Five classes of vulnerability fixed:

1. **OOM via malicious `nar_size`:** client declares `nar_size=u64::MAX`, server does `Vec::with_capacity(nar_size)` → immediate OOM before reading a single byte.
2. **DoS via unbounded batch:** `FindMissingPaths` with a million paths → runs a million DB queries.
3. **Path traversal:** `FilesystemBackend::put(key)` writes to `{base}/{key}`. Key = `"../../etc/passwd"` → writes outside the store directory.
4. **Error disclosure:** `Status::internal(format!("{e}"))` leaks S3 bucket names, filesystem paths, SDK internals to clients.
5. **Unvalidated metadata:** `PathInfo.references`/`signatures` persisted to DB without count bounds or format checks. ~150k short strings fit in the 32MiB gRPC frame.

Plus: `MAX_NAR_SIZE` constant centralized in `rio-common` (was duplicated gateway+store; worker had no bound at all).

## Commits

- `c0b3af6` — fix(rio-store): security hardening - bound sizes, validate paths, sanitize errors
- `e6f0fc9` — refactor(rio-common): share MAX_NAR_SIZE constant across crates
- `36adc78` — fix(rio-worker): bound NAR accumulation to MAX_NAR_SIZE in FUSE and executor
- `a729182` — fix(rio-store): validate references and signatures in PutPath
- `4a69652` — fix(rio-store): use try_from for nar_size capacity to avoid 32-bit truncation

(Discontinuous — commits 38, 43, 44, 81, 82.)

## Files

```json files
[
  {"path": "rio-common/src/limits.rs", "action": "NEW", "note": "MAX_NAR_SIZE=4GiB, MAX_BATCH_PATHS=10k, MAX_REFERENCES=10k, MAX_SIGNATURES=100, MAX_DAG_NODES=100k (for scheduler P0049)"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "bound nar_size before alloc; bound FindMissingPaths batch; validate_store_path at RPC boundaries; route all internal errors through internal_error() helper (log full, return generic); bound references+signatures counts, parse each reference; usize::try_from capacity"},
  {"path": "rio-store/src/backend/filesystem.rs", "action": "MODIFY", "note": "validate_key rejects /, \\, .., null; async tokio::fs::try_exists (was sync)"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "check rows_affected in complete_upload (detect concurrent delete_uploading); cleanup placeholder+blob on complete_upload fail"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "saturating_add check before buffer.extend in fetch_and_extract → EFBIG"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "saturating_add check in fetch_drv_from_store → BuildFailed"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "use rio_common::limits::MAX_NAR_SIZE (was: local const)"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "test_put_path_rejects_absurd_nar_size, rejects_malformed_store_paths, rejects_oversized_batch, rejects_path_traversal, rejects_excessive_references, rejects_malformed_reference"}
]
```

## Design

**Pre-allocation bound:** `MAX_NAR_SIZE = 4 * 1024^3` checked *before* `Vec::with_capacity`. The `usize::try_from` guard (`4a69652`) handles the edge: on 32-bit, `4_294_967_296` is one greater than `u32::MAX` → truncates to 0.

**`validate_store_path`:** `StorePath::parse` from `rio-nix`. Rejects anything that isn't exactly `/nix/store/{32-char-hash}-{name}`. Applied at every RPC boundary that takes a store path.

**`internal_error()`:** `error!(err=?e, "storage operation failed"); Status::internal("storage operation failed")`. Server log has full detail; client sees nothing.

**`validate_key` traversal defense:** backend keys are SHA-256 hex + `.nar` — they should never contain path separators. Rejecting `/`, `\`, `..`, null is defense-in-depth.

**Worker-side bounds:** the gateway and store enforce `MAX_NAR_SIZE` on their ends. But the worker's `fetch_and_extract` (FUSE) and `fetch_drv_from_store` (executor) accumulated chunks from the store with no bound. A buggy/compromised store could OOM the worker. `saturating_add` before `buffer.extend`.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[store.sec.nar-size-bound]`, `r[store.sec.path-validate]`, `r[store.sec.error-sanitize]`, `r[store.sec.traversal-guard]`, `r[common.limits.*]`.

## Outcome

Merged as `c0b3af6`, `e6f0fc9`, `36adc78`, `a729182`, `4a69652` (5 commits, discontinuous). 6 new tests covering each boundary. `MAX_DAG_NODES` defined here but consumed in P0049 (scheduler gRPC boundary validation).
