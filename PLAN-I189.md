# I-189 — FUSE EIO on `gcc-15.2.0` under `hello-deep-256x`

## Symptom

```
getting status of '/var/rio/overlays/.../nix/store/h31qj6s31sb2hvddbfx74msvf6bmqmsy-gcc-15.2.0':
Input/output error
```

during nix-daemon sandbox setup. `hello-shallow-256x` succeeds; `hello-deep-256x`
fails.

## What "deep" vs "shallow" actually means

This was the key misunderstanding in the initial triage.

`nix-bench` (`~/src/nix-bench/main/lib.nix`):

- **`hello-shallow-Nx`**: N cache-busted copies of `hello` only (top-level
  drv). Closure ≈ N × 1 build, all sharing the SAME stdenv inputs.
- **`hello-deep-Nx`**: N cache-busted copies of the **entire bootstrap chain**
  (`mkPkgsDeep` overlays `currentTime` into stdenv). At -64x this is **~9600
  derivations** (verified live: 9614 `.drv` files copied during the `-64x`
  repro). At -256x: **~38000**.

So `hello-deep-256x` doesn't mean 256 builders. It means hundreds of builders
churning continuously through a wide DAG, with stage-transition waves where many
builders simultaneously need the same bootstrap input (e.g., stage4's
`gcc-15.2.0`).

`hello-shallow-256x` is a single burst that the 7.6s retry loop absorbs.
`hello-deep-256x` is sustained herd pressure across many waves — eventually one
wave loses the retry race.

## Live observations (`hello-deep-64x` partial repro, 2026-04-05 22:14–22:24 UTC)

| Observation | Evidence |
|---|---|
| **Aurora cold-start latency** | `UPDATE narinfo` took **7.12 s**, `INSERT narinfo` **6.93 s** during the initial 9600-drv PutPath burst. Aurora Serverless v2 at min 0.5 ACU scales up under load; queries stall during scale. |
| **ComponentScaler reaction** | rio-store scaled 2→14 replicas within ~60 s of the drv-upload burst. |
| **h2 stream failure under load** | `GetPath transient failure; retrying` with `error="h2 protocol error: error reading a body from connection / BrokenPipe"` for `glibc-2.42`. At -64x: 1 occurrence, retried successfully. |
| **No store OOM** | `kube_pod_container_status_last_terminated_reason{reason="OOMKilled"}` = 0 over 3 days. |
| **Ephemeral builder metrics not scraped** | `rio_builder_fuse_jit_lookup_total{outcome="eio"}` = 0 over 3 days despite the known I-189 failure — pods die before Prometheus scrapes. |

The -64x repro was stopped before reaching the gcc-consuming bootstrap stage
(would have taken hours and significant EKS spend); the failure mechanism was
captured from earlier-stage glibc fetches.

## Root cause (most likely)

**Thundering herd on rio-store `GetPath` exhausts the builder's retry budget.**

Chain at -256x:

1. A bootstrap-stage wave dispatches: hundreds of fresh ephemeral builders, each
   calling `GetPath(/nix/store/h31qj6s31...gcc-15.2.0)` (164 MB) within the same
   few seconds. Cross-builder, this is **not** singleflighted.
2. Store-side: 14 replicas × ~18 builders each, each spawning a `get-path-stream`
   task with `mpsc::channel(16)` × 256 KiB pieces. Egress per replica ≈ 3 GB.
   Under bandwidth saturation, h2 streams stall; some connections reset
   (`BrokenPipe` — observed live).
3. Builder-side: `NarCollectError::Stream(Unknown)` → `is_transient()` → retry.
   `RETRY_BACKOFF = [100ms, 500ms, 2s, 5s]` = 5 attempts over 7.6 s.
4. Under **sustained** herd (the next attempt re-enters the same saturated
   pool), a fraction of builders exhaust all 5 attempts → `"retries exhausted"`
   (currently `warn!`) → `Errno::EIO`.
5. `circuit.record(false)` × 5 across a few inputs → `CircuitBreaker` opens
   (threshold 5) → all subsequent JIT lookups on that builder `EIO` for 30 s →
   the build's sandbox `lstat(gcc-15.2.0)` → `Input/output error`.

The classifier fix (separate sprint-1 commit) already remaps this to
`InfrastructureFailure` → scheduler retry, so the build is **not poisoned** —
but the retry storm wastes a node-spin per failure and the user sees errors.

## Secondary findings (code review)

| Finding | File:line | Impact |
|---|---|---|
| `Status::aborted` not in `is_transient()` | `rio-proto/src/client/mod.rs:300-305` | Store returns `Aborted` for Serialization/GcMarkBusy/Deadlock with explicit "retry" intent; builder treats it as terminal → immediate EIO. Only matters when manifest hint is absent (BatchGetManifest failed). |
| S3 backend error → `ChunkError::NotFound` | `rio-store/src/cas.rs:602-607` → `cas.rs:548` | A transient S3 error (`backend.get()` Err) is mapped to `None` then `NotFound`. `GetPath` surfaces it as `DATA_LOSS` (non-transient). Mitigated by store-side singleflight + `s3MaxAttempts:10`, but a real S3 outage poisons the path instead of retrying. |
| Terminal-EIO logged at `warn!` | `rio-builder/src/fuse/fetch.rs:909,915,994,1000,1029` | Operator grepping `level=ERROR` finds only `ops.rs` `"JIT fetch failed → EIO, errno=5"` — the underlying gRPC status is on a separate `WARN` line. **Fixed in this commit.** |
| Ephemeral builder metrics never scraped | n/a | `outcome=eio`, `circuit_open`, `materialization_failures` all read 0 in Prometheus. Need push-gateway or terminal-metric flush before exit. |

## Candidates ruled out

- **Builder disk full**: builds dispatched to `x86-64-medium` (25 Gi
  ephemeral-storage), not `tiny` (2 Gi). gcc closure ≈ 500 MB, spool peak ≈ 1
  GB. Headroom is fine.
- **Store OOM**: kube-state-metrics shows zero OOMKilled in 3 days.
- **`jit_fetch_timeout`**: 164 MB at the 15 MiB/s floor → 11 s, so the 60 s base
  applies. Per-stream throughput at 14 store replicas / hundreds of builders is
  >2.7 MB/s, well within the 60 s budget for the *fetch itself*. The EIO is from
  retry-exhaustion on stream resets, not from a single fetch timing out.

## What was implemented (this commit — diagnostic only)

`fix(builder): log underlying error at ERROR level when JIT fetch → EIO (I-189)`

- `rio-builder/src/fuse/fetch.rs`: terminal `return Err(Errno::EIO)` sites
  promoted from `tracing::warn!` to `tracing::error!`, with `→ EIO` in the
  message so the gRPC status / io::Error is on the same ERROR-level line as the
  failure (previously: ERROR line in ops.rs had only `errno=5`; the cause was a
  separate WARN line).
- `rio-builder/src/fuse/ops.rs:281`: `errno = i32::from(errno)` → `errno =
  ?errno` (symbolic, e.g., `EIO`/`ENOENT`/`EAGAIN`); message points operators
  at the preceding error! line for the cause.

No control-flow change. Next `hello-deep-256x` failure will have the gRPC
status string at ERROR level in builder logs, making the exact store-side
trigger directly greppable.

## Structural fix (NOT implemented — needs a plan)

The diagnostic change makes the next failure self-explanatory. Actually
*preventing* the EIO needs one or more of:

### Option A — jittered retry backoff (small, builder-side)

`RETRY_BACKOFF` is fixed `[100ms, 500ms, 2s, 5s]`. Under herd, all builders
retry in lockstep → the retry IS the herd. Add per-attempt jitter
(`delay × rand(0.5..1.5)`) and one more step (`10s`). Cheap, no new state, ~80%
likely to be sufficient. **Recommended first.**

  - File: `rio-builder/src/fuse/fetch.rs:242` (`RETRY_BACKOFF`) + the two
    `tokio::time::sleep(delay)` call sites.

### Option B — store-side `GetPath` admission control (medium)

`GetPath` currently spawns unbounded `get-path-stream` tasks. Add a
`Semaphore(N)` (N ≈ 64) around `stream_path`; over-limit returns
`ResourceExhausted` immediately (which the builder already retries). This
back-pressures the herd at the store instead of letting h2 collapse it.

  - File: `rio-store/src/grpc/get_path.rs:150` `stream_path`, mirroring
    `nar_bytes_budget` but for download streams.

### Option C — scheduler-side input prefetch staggering (structural)

The scheduler knows all N builders need the same closure. It could (a) dispatch
in jittered batches, or (b) send a `Prefetch` hint to one builder per AZ first,
let store warm its moka cache, then dispatch the rest. Largest payoff,
largest scope.

### Option D — `Aborted` as transient (1-line, separate concern)

Add `tonic::Code::Aborted` to `NarCollectError::is_transient()` and
`is_transient_status()`. The store explicitly returns `aborted` for retryable PG
conflicts; the builder currently EIOs immediately on it. Orthogonal to the herd
but closes a real gap in the no-hint fallback path.

  - Files: `rio-proto/src/client/mod.rs:300`,
    `rio-builder/src/fuse/fetch.rs:660`.

**Recommendation:** A + D in one small follow-up plan (low risk, no new infra);
B as a separate plan if A proves insufficient under -256x re-test.

## Unrelated: pre-existing test flake found

`rio-builder fuse::fetch::tests::test_fetch_via_chunks_unimplemented_falls_back_to_getpath`
flakes ~10% under nextest (1/10 at base `7307d0f6` with this commit's changes
stashed; 2/8 with them applied). Passes in isolation. Not in
`known-flakes.jsonl`. Likely `setup_fetch_harness` port/state collision.
→ separate followup.
