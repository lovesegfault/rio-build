# Plan 0192: Rem-13 — Test fidelity: MockScheduler since_sequence + seq=1 + hash-verify + inproc transport

## Design

**P1 (HIGH).** Close four test-fidelity gaps that let reconnect tests prove "doesn't crash" instead of "works".

**`MockScheduler.watch_build`:** record `(build_id, since_sequence)` into `watch_calls`; HONOR `since_sequence` (skip events ≤ since, after auto-fill). Previously ignored the parameter entirely — tests couldn't assert the client sent the right cursor.

**Sequence starts at 1 not 0 (5 sites):** the real scheduler never emits `sequence=0` — it's the `WatchBuildRequest`-side "from start" sentinel. Mock emitting 0 meant tests were exercising a code path the real scheduler never does.

**`MockStore.put_path`:** reject non-empty `metadata.nar_hash` (mirror real `put_path.rs:206`). Hash-upfront was removed pre-phase3a; all clients send empty. Mock accepting it means a client regression (hash-upfront coming back) would pass tests. Also: verify trailer hash == sha256(nar); missing trailer is now a protocol error (truncated stream), not silent `Ok(created=true)`.

**`spawn_mock_store_inproc`:** `tokio::io::duplex` + `connect_with_connector`. `start_paused` tests over real TCP auto-advance timeouts while kernel accept/handshake pend; in-mem duplex halves are tokio tasks so auto-advance sees them as not-idle. Migrated 3 `upload.rs` tests.

Remediation doc: `docs/src/remediations/phase4a/13-test-fidelity-mockscheduler.md` (800 lines). The §2.7 path in the index is stale — mocks were extracted to `rio-test-support/src/grpc.rs`.

## Files

```json files
[
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockScheduler: record (build_id,since); honor since; seq starts at 1. MockStore: reject non-empty nar_hash; verify trailer hash. spawn_mock_store_inproc"},
  {"path": "rio-test-support/src/kube_mock.rs", "action": "MODIFY", "note": "inproc helper"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "tokio io-util feature"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "test fixture adjustment"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "seq=1 adjustment"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "test fixture"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "since_sequence assertion"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "hash verify assertion"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "3 tests migrated to inproc"},
  {"path": "rio-scheduler/src/lease/election.rs", "action": "MODIFY", "note": "kube-mock adjustment"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "r[gw.reconnect.since-seq] spec marker"}
]
```

## Tracey

- `r[impl gw.reconnect.since-seq]` — `1e5d1f2`
- `r[verify gw.reconnect.since-seq]` — `1e5d1f2`

2 marker annotations.

## Entry

- Depends on P0177: tonic reconnect class (the reconnect tests this hardens)

## Exit

Merged as `1e5d1f2` (fix; plan at index §2.7). `.#ci` green. Reconnect tests now fail on wrong `since_sequence`.
