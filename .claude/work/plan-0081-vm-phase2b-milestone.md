# Plan 0081: gRPC contract tests + container images + vm-phase2b + STDERR_RESULT bug fix

## Design

The milestone validation, and the milestone test **immediately found a real bug** — latent bug #12 in the running tally of bugs found by VM tests.

**gRPC contract tests** (`5454a08`, `rio-proto/tests/contract.rs`): tests that encode invariants the rest of the system relies on.
- traceparent survives tonic `Request` wrapping (defense: a tonic version that strips unknown headers).
- `link_parent` on untraced request doesn't panic (every grpcurl call would crash the handler if it did).
- proto3 `oneof None`: `PutPathRequest.msg` and `WorkerMessage.msg` can be `None`; handlers that forget the `None => {}` arm panic on malformed input.
- `check_bound` error message contract: field name + actual count + limit all present (user-facing — tells them which bound and what they sent).
- **Compile-time tripwires** via `const { assert! }`: `MAX_DAG_NODES >= 70k` (nixpkgs is ~60k), `NAR_CHUNK_SIZE <= 1 MiB` (P0080's streaming work was specifically to bound per-upload memory; a 64 MiB chunk would undo it), `DEFAULT_MAX_MESSAGE_SIZE >= 16 MiB` (nixpkgs DAG is ~12 MiB). A PR lowering one won't even build.

**Container images** (`df7f6ff`, `nix/docker.nix`): `dockerTools.buildLayeredImage` ×4. Common layer: cacert (TLS for S3/gRPC), tzdata, `rio-workspace`, `SSL_CERT_FILE` env, `RIO_LOG_FORMAT=json`, OCI labels. Gateway/scheduler/store: minimal. Worker: +nix (spawns `nix-daemon --stdio` per build), +fuse3 (fusermount3 for `fuser` AutoUnmount), +util-linux (mount/umount for overlay teardown), +passwd/group stubs (nix-daemon drops privs to `nixbld` — without `/etc/passwd`, "getpwnam: user nixbld does not exist"), +nix.conf baseline, PATH with all three bin dirs (executor does `Command::new("nix-daemon")` without abs path). `.#dockerImages` aggregate.

**vm-phase2b** (`df7f6ff`, `nix/tests/phase2b.nix`): 5 VMs (control + 3 workers + client). Four assertions:
1. **Chain build**: A→B→C sequential derivations, `maxBuilds=1` × 3 workers × 3 steps → forces one-per-worker dispatch.
2. **Log pipeline**: each step echoes `PHASE2B-LOG-MARKER: building {name}` to stderr. Test greps client's `nix-build` output — presence proves worker LogBatcher → scheduler ring buffer → ForwardLogBatch → BuildEvent::Log → gateway STDERR_NEXT → SSH → client.
3. **Cache hit**: second build of same chain is fast + `rio_scheduler_cache_hits_total > 0`.
4. **OTLP**: Tempo `/api/search` returns traces for `service.name=scheduler`. (Tempo not Jaeger — same OTLP contract, and Tempo is what's packaged in nixpkgs.)

**THE BUG** (`feadc8f`): vm-phase2b's assertion 2 (`rio_scheduler_log_lines_forwarded_total >= 3`) failed on first run. Root cause: worker's `read_build_stderr_loop` was silently dropping ALL build output. Modern `nix-daemon` sends builder stderr as `STDERR_RESULT` with `result_type=101` (BuildLogLine) or `107` (PostBuildLogLine) — NOT raw `STDERR_NEXT`. The match arm had `Result { .. }` in the "discard" bucket alongside activity lifecycle messages.

**This was a latent phase-2a bug.** It never mattered there because phase-2a's VM test doesn't assert on log content. vm-phase2b's log-pipeline assertion caught it. Commit body: "Exactly what milestone VM tests are for — 1 more bug found for the 'latent bugs found via VM test' tally (now 12 total)."

Fix: match `STDERR_RESULT{101|107}` with first field String, extract as a log line, feed to `LogBatcher` same as `STDERR_NEXT`. Also updated `rio-nix/src/protocol/stderr.rs` to parse `BuildLogLine`/`PostBuildLogLine` result-type payloads, and `rio-scheduler/src/{actor/mod,grpc/mod}.rs` for the metric.

## Files

```json files
[
  {"path": "rio-proto/tests/contract.rs", "action": "NEW", "note": "traceparent roundtrip, link_parent no-panic, oneof-None, check_bound message contract, const{assert!} tripwires"},
  {"path": "rio-proto/Cargo.toml", "action": "MODIFY", "note": "contract test deps"},
  {"path": "nix/docker.nix", "action": "NEW", "note": "buildLayeredImage ×4; worker: nix+fuse3+util-linux+passwd stubs+nix.conf+PATH"},
  {"path": "nix/tests/phase2b.nix", "action": "NEW", "note": "5-VM test; chain build + log marker grep + cache hit + Tempo OTLP assertions"},
  {"path": "nix/tests/phase2b-derivation.nix", "action": "NEW", "note": "A→B→C chain drvs, each echoes PHASE2B-LOG-MARKER to stderr"},
  {"path": "flake.nix", "action": "MODIFY", "note": ".#dockerImages aggregate; vm-phase2b check"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "LATENT BUG #12 FIX: match STDERR_RESULT{101|107}, extract as log line (was in discard bucket — ALL build output silently dropped)"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "parse BuildLogLine (101) / PostBuildLogLine (107) result-type payloads"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "log_lines_forwarded_total metric"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "metric wiring"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[wk.log.stderr-result]` (the bug fix), `r[test.vm.phase2b]`.

## Entry

- Depends on **P0078** (log pipeline): assertion 2 tests the ring buffer → forward chain.
- Depends on **P0079** (OTLP): assertion 4 tests Tempo trace export.
- Depends on **P0080** (streaming upload): worker image needs the streaming upload to avoid OOM in tight VM memory.
- Depends on **P0075** (cargo-deny): the `.#dockerImages` target joins the CI aggregate alongside cargo-deny.

## Exit

Merged as `5454a08..feadc8f` (3 commits). `.#ci` green at merge — `vm-phase2b` PASSES after the STDERR_RESULT fix. `nix build .#dockerImages` produces 4 images. Phase 2b complete.
