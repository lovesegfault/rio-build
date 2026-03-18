# Plan 0131: HMAC-SHA256 assignment tokens

## Design

mTLS (P0128) authenticates WHO is connecting; it doesn't authorize WHAT they upload. A compromised worker with a valid TLS cert could `PutPath` arbitrary paths into the store — poisoning every downstream build that references them. This plan added per-assignment HMAC tokens: the scheduler signs a scoped capability at dispatch, the store verifies it on `PutPath`.

The token is `base64url(serde_json(Claims)).base64url(hmac_sha256)`. `Claims { worker_id, drv_hash, expected_outputs: Vec<String>, expiry_unix }`. JSON (not a custom binary format) because it's self-delimiting, already a workspace dep, and human-debuggable — when a `PutPath` fails with `PERMISSION_DENIED`, you can `base64 -d` the token header and read the JSON. The `hmac` crate's `verify_slice` provides constant-time comparison.

`HmacSigner::load(Option<&Path>)` follows the `Signer::load` pattern from `rio-store/src/signing.rs`: None path → None (HMAC disabled for dev), bad file → Err, valid → Some. The key is 32 raw bytes.

On the scheduler side: `DagActor` gets `hmac_signer: Option<Arc<HmacSigner>>` via `with_hmac_signer()` builder. At dispatch, if the signer is present: build `Claims { worker_id, drv_hash, expected_output_paths: state.expected_output_paths.clone(), expiry: now + 2*build_timeout }` and sign. The token goes into `Assignment.assignment_token` (which previously held a meaningless format-string placeholder).

On the worker side: `upload_output` wraps the tonic `Request` and inserts `x-rio-assignment-token` into gRPC metadata.

On the store side: `PutPath` handler extracts the metadata header BEFORE `request.into_inner()`. If verifier present + no token → `PERMISSION_DENIED`. If token invalid/expired → `PERMISSION_DENIED`. After `ValidatedPathInfo::try_from` computes the store path, check it's in `claims.expected_outputs` — else `PERMISSION_DENIED "path not in assignment"`.

The gateway bypass: gateway does `PutPath` for `wopAddToStoreNar` but has no assignment (it's not a worker). Initial implementation: if `request.peer_certs()` is present (any mTLS client), skip HMAC. This was later found to be a critical vulnerability (X1 in P0139 — compromised worker omits token → bypass) and tightened to require `CN=rio-gateway` specifically via x509-parser.

## Files

```json files
[
  {"path": "rio-common/src/hmac.rs", "action": "NEW", "note": "HmacSigner/HmacVerifier + Claims + sign/verify"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "pub mod hmac"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "hmac = 0.12"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "build Claims + sign at assignment creation"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "hmac_signer field"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "with_hmac_signer builder"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "hmac_key_path config + HmacSigner::load"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "extract x-rio-assignment-token + verify + check path in expected_outputs"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "hmac_verifier field + with_hmac_verifier builder + peer_certs bypass"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "hmac_key_path config + HmacVerifier::load"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "req.metadata_mut().insert(x-rio-assignment-token)"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "plumb assignment_token through upload_all_outputs"}
]
```

## Tracey

Markers implemented:
- `r[impl sec.boundary.grpc-hmac]` — store `PutPath` HMAC verification (`87dfc92`). The verify annotation lands with P0128's TLS integration test and is later reinforced by P0145's dedicated `tests/grpc/hmac.rs`.

## Entry

- Depends on P0127: phase 3a complete (`PutPath` handler, `Assignment.assignment_token` field, dispatch path).
- Depends on P0128: mTLS peer_certs available for gateway bypass.

## Exit

Merged as `87dfc92` (1 commit). `.#ci` green at merge. Tests: roundtrip sign→verify; tamper signature byte → fail; tamper claim JSON → fail (signature mismatch); expired → fail; wrong key → fail; malformed base64 → fail. Store tests: valid token + matching path → success; valid token + wrong path → `PERMISSION_DENIED`; no token + verifier set → `PERMISSION_DENIED`; no verifier → no-op.
