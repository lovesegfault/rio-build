# S6b generated sweeps (bughunt4)

Committed [GEN-SET] output per the round-4 banner: "every surface" is a
generated set, never a memory. Regenerate each block with its command;
a drifted block means a new surface appeared and must be classified.

## CLI sentinel-stream drain-law census (bug_163 / merged_bug_106)

Every server-stream drain in rio-cli routes through
`stream_util::drain_with` (the one chokepoint: bounded silence,
sentinel seal, missing-sentinel posture). The census proves totality:
zero raw `Streaming::message()` drain loops outside `stream_util.rs`.

Command:

    grep -rn "\.message()" rio-cli/src/

Output (2026-06-09, classified):

| site | classification |
|---|---|
| `rio-cli/src/stream_util.rs:69` | THE chokepoint's blanket `MessageStream for tonic::Streaming` impl |
| `rio-cli/src/stream_util.rs:162` | `tonic::Status::message()` — error-text accessor, not a poll |
| `rio-cli/src/main.rs:128` | `tonic::Status::message()` in the unary `rpc()` retry printer — `rpc()` rejects `Streaming<_>` at compile time (`T: Default`), so no stream can drain through it |
| `rio-cli/src/main.rs:142` | `tonic::Status::message()` — error-text accessor |
| `rio-cli/src/stream_util.rs:37` | this census's own doc reference |

Consumers (every `tonic::Streaming`-producing call site in rio-cli),
all routed:

| consumer | RPC | policy |
|---|---|---|
| `verify_chunks.rs` | `VerifyChunks` | `DrainPolicy::audit` (Truncation: missing sentinel = nonzero PARTIAL) |
| `logs.rs::drain_log_chunks` | `TailLog` (non-follow) | DiscloseExitZero + standard bound (bug_163: sentinel seal kills the post-seal poll) |
| `gc.rs` | `TriggerGC` | DiscloseExitZero + 15 min GC bound (merged_bug_106) + nonzero on failure-bearing sentinel |
