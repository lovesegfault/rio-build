# Plan 982804601: extract `jwt_metadata()` helper to rio-common

Three call sites now build the `x-rio-tenant-token` metadata pair by hand. [P0476](plan-0476-scheduler-store-client-reconnect.md) added the third (`connect_store_lazy` chain); the pattern is stable, extract it.

Current duplication:

| Site | Shape | Lines |
|---|---|---|
| [`rio-gateway/src/handler/grpc.rs:19`](../../rio-gateway/src/handler/grpc.rs) | `fn jwt_metadata(Option<&str>) -> Vec<(&'static str, &str)>` | ~6 |
| [`rio-scheduler/src/actor/merge.rs:700`](../../rio-scheduler/src/actor/merge.rs) | inline `MetadataValue::try_from` + `insert(TENANT_TOKEN_HEADER, v)` | ~14 (×2: also at :953) |
| [`rio-scheduler/src/actor/merge.rs:783`](../../rio-scheduler/src/actor/merge.rs) | inline `jwt_pair = [(TENANT_TOKEN_HEADER, t)]` | ~4 (×2: also at :1028) |

Two shapes exist: the **slice builder** (gateway, scheduler:783/1028 — for `rio-proto` helpers that take `&[(&str, &str)]`) and the **metadata inserter** (scheduler:700/953 — for hand-built `tonic::Request`). Extract both.

Discovered via consolidator; N=3 callers crosses the extraction threshold.

## Tasks

### T1 — `refactor(common):` add `jwt_metadata_slice` + `jwt_metadata_insert` helpers

MODIFY [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) — add near the existing `TENANT_TOKEN_HEADER` const (~line 68):

```rust
/// Build the `x-rio-tenant-token` metadata slice for rio-proto helpers.
///
/// Returns owned `Vec` so callers can pass `&jwt_metadata_slice(token)`
/// directly. Empty when `jwt_token` is `None` (dual-mode fallback —
/// store's interceptor treats absent header as pass-through per
/// `r[gw.jwt.dual-mode]`).
pub fn jwt_metadata_slice(jwt_token: Option<&str>) -> Vec<(&'static str, &str)> {
    match jwt_token {
        Some(t) => vec![(TENANT_TOKEN_HEADER, t)],
        None => vec![],
    }
}

/// Insert `x-rio-tenant-token` into a `tonic::Request`'s metadata.
///
/// No-op when `jwt_token` is `None`. Logs + skips on `MetadataValue`
/// encode failure (JWT is base64url ASCII, can't fail in practice;
/// belt-and-braces for the <8KB limit).
pub fn jwt_metadata_insert<T>(req: &mut tonic::Request<T>, jwt_token: Option<&str>) {
    if let Some(t) = jwt_token {
        match tonic::metadata::MetadataValue::try_from(t) {
            Ok(v) => { req.metadata_mut().insert(TENANT_TOKEN_HEADER, v); }
            Err(e) => tracing::warn!(error = %e, "jwt not ASCII-encodable; header skipped"),
        }
    }
}
```

### T2 — `refactor(gateway):` replace local `jwt_metadata` with common helper

MODIFY [`rio-gateway/src/handler/grpc.rs`](../../rio-gateway/src/handler/grpc.rs) — delete the local `fn jwt_metadata` (lines 13-24), replace call sites with `rio_common::jwt_interceptor::jwt_metadata_slice`.

### T3 — `refactor(scheduler):` replace inline metadata builds with common helpers

MODIFY [`rio-scheduler/src/actor/merge.rs`](../../rio-scheduler/src/actor/merge.rs) — replace the four inline sites:
- :700-712 and :953-957 → `jwt_metadata_insert(&mut req, jwt_token)`
- :783 and :1028 → `jwt_metadata_slice(jwt_token)` (adjust surrounding slice-ref as needed)

## Exit criteria

- `cargo build -p rio-gateway -p rio-scheduler -p rio-common` → clean
- `grep -c 'jwt_metadata_slice\|jwt_metadata_insert' rio-common/src/jwt_interceptor.rs` → ≥2 (both helpers defined)
- `grep 'fn jwt_metadata' rio-gateway/src/handler/grpc.rs` → 0 hits (local fn deleted)
- `grep -c 'TENANT_TOKEN_HEADER\|MetadataValue::try_from' rio-scheduler/src/actor/merge.rs` → ≤1 (inline sites gone; one import line may remain)
- `cargo nextest run -p rio-gateway -p rio-scheduler` → green (no behavior change)

## Tracey

References existing markers:
- `r[gw.jwt.issue]` — the JWT minted on auth that these helpers attach
- `r[gw.jwt.verify]` — the interceptor on the receiving side that reads the header

No new markers — pure extraction, no behavior change.

## Files

```json files
[
  {"path": "rio-common/src/jwt_interceptor.rs", "action": "MODIFY", "note": "T1: add jwt_metadata_slice + jwt_metadata_insert helpers"},
  {"path": "rio-gateway/src/handler/grpc.rs", "action": "MODIFY", "note": "T2: delete local jwt_metadata, use common helper"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "T3: replace 4 inline sites with common helpers"}
]
```

```
rio-common/src/jwt_interceptor.rs  # T1: +2 pub fns
rio-gateway/src/handler/grpc.rs    # T2: -local fn, +import
rio-scheduler/src/actor/merge.rs   # T3: 4 sites → helper calls
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [476], "note": "No hard deps — all three call sites exist on sprint-1. Soft-dep 476 (connect_store_lazy chain) added the third caller that motivated this; already merged. Pure refactor, no semantic change."}
```

**Depends on:** none — all sites exist.
**Conflicts with:** [`merge.rs`](../../rio-scheduler/src/actor/merge.rs) — moderate traffic (actor hot file). Check collisions at dispatch. [`jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) — low traffic, additive.
