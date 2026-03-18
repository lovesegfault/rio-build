# Plan 0170: Gateway wopAddMultipleToStore streaming — bounded mpsc pipeline

## Design

Replace whole-closure buffering with a `FramedStreamReader` → `PutPath` pipeline. Observed live on EKS: nixpkgs source NAR (~400MB) through SSM tunnel showed `store: 0 bytes` for 8 minutes — gateway was buffering the entire NAR before starting the gRPC call. A closure >~800MB would OOM the 1Gi gateway pod.

**`grpc_put_path_streaming`:** bounded mpsc (4×256KiB) bridges the chunk reader to a spawned `PutPath` RPC. Awaits the RPC before returning (pump error wins). `stream_one_entry` (formerly `parse_add_multiple_entry`) reads from `AsyncRead` directly; `.drv` paths are still buffered (≤16MiB, needed for `try_cache_drv`), rest streamed. Both handlers drain-to-sentinel after entries to detect trailing data.

`handle_add_to_store_nar`: validation moved before framed read; same `.drv` branch; `max_total=nar_size` (tighter than `MAX_FRAMED_TOTAL`). `wopAddToStore`/`wopAddTextToStore` unchanged — they need the content hash before `PutPath` metadata, can't pure-stream without disk spooling.

`tokio-stream` promoted from dev-dep to dep (`ReceiverStream`).

## Files

```json files
[
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "stream_one_entry reads from AsyncRead directly; .drv buffer, rest stream"},
  {"path": "rio-gateway/src/grpc.rs", "action": "MODIFY", "note": "grpc_put_path_streaming: bounded mpsc → spawned PutPath"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "tokio-stream dev→dep"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "DAG reconstruction section updated for streaming"}
]
```

## Tracey

No new markers. Covered by existing `r[gw.opcode.mandatory-set]`.

## Entry

- Depends on P0148: phase 3b complete (extends phase 2c `wopAddMultipleToStore` implementation)

## Exit

Merged as `eaa2c1d` (1 commit). `.#ci` green. 400MB NAR through SSM tunnel: store sees bytes immediately, gateway RSS stays flat. Tests: mixed .drv+streaming batch; trailing-data detection; truncated-NAR error string updated.
