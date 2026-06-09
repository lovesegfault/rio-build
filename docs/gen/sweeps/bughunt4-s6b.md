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

## Gateway log-tail floor-advance census (merged_bug_020)

The law: the relay floor (`last_relayed`) may only advance over lines
that were RELAYED or DISCLOSED — never over fetched-but-undisclosed
content. The defect class: a flush that advances the floor BEFORE
reconciling against fresh coverage hides the healing lines below it.

Command:

    grep -n "last_relayed = Some" rio-gateway/src/handler/log_tail.rs

Output (2026-06-09, classified — every site advances only over
relayed/disclosed lines):

| site | classification |
|---|---|
| `:840` Serve arm | advances to `next_line-1` AFTER the served slice was sent; `pending_gap.on_serve` reconciled the hole FIRST (shrink/heal — the heal continuation rides) |
| `:856` heal continuation | advances to the healed watermark AFTER the trimmed withheld suffix was sent |
| `:1106` `flush_pending_gap` | advances AFTER marker + withheld send; reached only when no fresh coverage exists below the advance (Divergent backfill is split out BEFORE this flush — the m020 fix) |
| `:1165` `reconcile_backfill` step 2 | advances to `fresh_next-1` AFTER the healing slice was sent |
| `:1180` `reconcile_backfill` step 3a | advances to the withheld watermark AFTER the trimmed suffix was sent |
| `:1205` `reconcile_backfill` step 3b | advances AFTER second marker + whole withheld send |

The Divergent arm's discriminant (`first < pending_until` →
reconcile-before-advance; else flush-then-revisit) is the chokepoint:
the only path that previously advanced over fetched content
(`flush_pending_gap` then re-visit of a backfilling chunk) is
unreachable for backfills by construction.
