# Design: `cargo xtask k8s replay`

Status: accepted 2026-05-24. Implementation tracked on the `xtask-replay` branch.

## Purpose

Replay a recorded build-load archive against a running rio deployment, at the
recorded request cadence, and compare what rio builds against what the
recording says was built originally. This gives us:

- a realistic load generator (real derivation graphs, real arrival times, real
  upload patterns) instead of synthetic stress loops;
- a regression detector: derivations that built successfully in the recording
  but fail on rio are surfaced as regressions, with per-output NAR-hash
  comparison for the ones that succeed;
- a harness for exercising hard-to-hit paths (client disconnects mid-build,
  upload bursts, deep closures) on demand.

Recording is **out of scope** for v1: rio will gain its own recorder later.
This command only consumes archives in the format described below.

## Non-goals (v1)

- Recording build load (future work; the archive format below is the
  compatibility contract a future recorder must produce).
- A full-screen TUI. Progress is step/heartbeat logging, consistent with the
  rest of xtask.
- Forwarding impure environment variables to builds (rio has no env-forwarding
  path; see "Impure-env demotion").
- Archive management helpers (`list`, `prune`), qa-stage integration, and a
  NixOS VM test — follow-ups.

## CLI surface

```
cargo xtask k8s replay [-p k3s|eks] --archive <PATH> [flags]
```

| Flag | Default | Meaning |
|---|---|---|
| `--archive <PATH>` | required | `.dwarfs` image or unpacked archive directory |
| `--speedup <F>` | `1.0` | time-compression factor (must be > 0) |
| `--max-sessions <N>` | `32` | concurrent in-flight requests (one SSH channel + daemon session each) |
| `--connections <N>` | `ceil(max_sessions/4)` | SSH connections to spread channels over (gateway caps 4 channels/connection) |
| `--target-substituter <URL>` | `https://cache.nixos.org` | repeatable; paths covered here are left for the target to substitute |
| `--confirm-regressions <N>` | `3` | consecutive failed rebuilds required before a failure is reported as a regression |
| `--no-prewarm` | off | skip the bulk pre-supply phase; supply per-request instead |
| `--no-disconnect-replay` | off | do not replay recorded client disconnects |
| `--dry-run` | off | resolve everything and run the timeline without connecting |
| `--limit <N>` | all | replay only the first N requests by offset (smoke runs) |
| `--watch` | off | periodically scrape and print scheduler metrics during the run |
| `--store <ssh-ng://host:port>` | unset | bypass the provider tunnel and target an explicit endpoint |
| `--ssh-key <PATH>` | deploy key | private key for `--store` targets |
| `--ssh-host-key <fp-or-path>` | unset | pinned host key for non-loopback `--store` targets |
| `--fail-on <none\|regression\|divergence>` | `none` | exit-code policy |
| `--report-dir <PATH>` | `.stress-test/replay/<ts>/` | where run artifacts are written |

## Connection model

By default the command behaves like `rsb`/`qa`: it opens a provider
port-forward to `svc/rio-gateway` and authenticates with the operator SSH key
installed by deploy (`RIO_SSH_PUBKEY`; the key comment selects the tenant).
The transport is an in-process SSH client (russh, already a workspace
dependency), not a shelled-out `ssh`/`nix`:

- keepalives enabled and a deadline on every daemon operation, so a wedged
  channel or dead connection fails its requests instead of hanging the run;
- host-key policy: accept-any **only** for loopback endpoints (the
  port-forward case). A non-loopback `--store` target must verify against
  `~/.ssh/known_hosts` or an explicit `--ssh-host-key`; otherwise the command
  fails closed.
- no SSH channel `env` requests are sent (the gateway does not consume them);
- each in-flight request gets its own channel running `nix-daemon --stdio`,
  spread over a connection pool sized for the gateway's per-connection channel
  cap (4).

## Archive format (v0 compatibility contract)

An archive is either a DwarFS image (read in-process via the `dwarfs` crate;
no external tools required) or a plain directory with the same layout:

```
manifest.json        run metadata (window, sources, counts)
requests.jsonl       one line per recorded client build request
builds.jsonl         one line per recorded build outcome (optional)
impure-env.json      drv path -> impureEnvVars names (optional)
narinfo/<hash>.narinfo   metadata sidecar for each embedded store path
nix/store/<hash>-<name>.drv      derivation ATerm bytes
nix/store/<hash>-<name>/...      embedded store paths, unpacked
```

Parsing rules (all serde structs ignore unknown fields; unknown enum values
are tolerated where noted):

- **manifest.json** — `from`/`to`/`created_at` (RFC 3339 timestamps),
  `src_substituters` (list of cache URLs the recording host could reach;
  used as live relay sources), `target_substituters` (list; non-empty only
  for "fat" archives), `fat` (bool, default false), `requests`, `drvs`,
  `embedded_srcs` (counts). Other fields are ignored. A missing manifest is
  an error for `replay`. Plain-HTTP `src_substituters` entries are ignored
  at replay time (only `https://` and `s3://` relay sources are honored),
  and the relay hosts that will be used are announced in the log before any
  probe or fetch traffic is issued to them.
- **requests.jsonl** — `ssh_session_id` (i64 client-session id from the
  recording), `offset_s` (f64 seconds from `from`; clamped at 0), `paths`
  (list of `[drvPath, [outputName...]]` pairs; `["*"]` and `[]` both mean
  all outputs). Lines are not guaranteed globally sorted; the loader sorts by
  offset. Empty lines are skipped.
- **builds.jsonl** — `ssh_session_id`, `drv_path`, `status` (integer code;
  0 = built, 6 = cancelled, 10 = builder error, 13 = client disconnect,
  16 = resource exhaustion, other non-zero = deterministic failure; unknown
  codes treated as deterministic failures), `status_msg`, `duration_s`,
  `stop_offset_s` (optional; used to time disconnect replay), `outputs`
  (map of output name → `{nar_hash_hex, nar_size}` where `nar_hash_hex` is
  the lowercase hex SHA-256 of the uncompressed NAR). Duplicate
  `(session, drv)` keys: last line wins. The whole file is optional; without
  it the run only reports build success/failure, not divergences.
- **impure-env.json** — map of drv path → list of env var *names* declared in
  that drv's `impureEnvVars`. Optional.
- **narinfo/** — one `.narinfo` per embedded store path, standard key:value
  format; `NarHash`/`NarSize`/`References` are required, hashes accepted in
  hex, base32, or base64 SRI forms; unknown keys ignored. The sidecar is the
  authority for path metadata at upload time (embedded trees are re-packed to
  NAR and must match `NarSize`).
- **nix/store/** — `.drv` files are ATerm and parsed with `rio-nix`'s
  derivation parser; embedded paths are plain unpacked trees (symlinks and
  the executable bit preserved).

## Architecture

### rio-nix: client-side protocol operations

`rio-nix/src/protocol/client.rs` already holds the client half used by
rio-builder (handshake, `SetOptions`, `BuildDerivation`). The replay client
adds the missing client-side codecs, in the same style (free async fns over
`AsyncRead`/`AsyncWrite`, no transport coupling):

- `client_query_valid_paths`
- `client_query_path_info`
- `client_add_to_store_nar` (framed NAR streaming)
- `client_add_multiple_to_store` (outer framed stream; per-entry
  `ValidPathInfo` + unframed NAR)
- `client_build_paths_with_results`
- a shared "send op → drain stderr → read reply" helper that surfaces
  `STDERR_ERROR` as a typed daemon refusal distinct from transport errors.

These are tested with duplex unit tests and with conformance tests that drive
them against rio-gateway's own protocol session handler, so the client and
server codecs verify each other.

### xtask: `xtask/src/k8s/replay/`

| Module | Responsibility |
|---|---|
| `mod.rs` | clap args, phase orchestration, summary/exit code |
| `archive.rs` | DwarFS/dir reader, manifest/requests/builds/impure-env parsing, narinfo sidecars, NAR packing of embedded trees |
| `client.rs` | russh transport: connect/auth/channel, exec `nix-daemon --stdio`, handshake, op wrappers with deadlines |
| `supply.rs` | workload set, source-resolution ladder, references-first upload planning, large-path routing, substituter probe/fetch (HTTP + S3) |
| `prewarm.rs` | union scan → classification → chunked validity probe → topologically levelled upload |
| `timeline.rs` | request pacing (due = start + offset/speedup), admission under the session cap, in-flight tracking, disconnect replay |
| `compare.rs` | verdict classification, per-output NAR-hash comparison, divergence log |
| `report.rs` | human summary, `summary.json`, exit-code policy |

New dependencies: `dwarfs` (workspace dependency, xtask-only, MIT/Apache-2.0);
xtask gains an edge on the existing `russh` workspace dependency. Substituter
access reuses `reqwest` and `aws-sdk-s3` already used by xtask.

## Replay semantics

1. **Load** the archive, sort requests by offset, apply `--limit`.
2. **Plan supply.** The *workload set* is every derivation that was actually
   built in the recorded window (it appears in `requests.jsonl` directly or as
   a transitive input derivation that had to be built). Outputs of workload
   derivations are **never** supplied to the target — the target must build
   them. Everything else a request's closure needs (sources, fixed-output
   results, pre-existing dependencies) is supplied from, in order: the target's
   own substituters (`--target-substituter` coverage check), paths embedded in
   the archive, live relay from the recording's `src_substituters`.
3. **Pre-warm** (default): walk the union closure of all requests, classify
   once, probe target validity in chunks, and upload everything supplyable in
   topological levels before the clock starts. With `--no-prewarm`, supply
   happens per-request inside the timeline instead (uploads then count against
   request latency, so timing fidelity is lower).
4. **Timeline.** Each request fires at `start + offset/speedup`, FIFO-admitted
   under `--max-sessions`. Per request: walk the closure from the archive's
   derivations, query missing paths on the target, upload any gaps (references
   first), then submit the build and stream daemon stderr until the result
   arrives.
5. **Impure-env demotion.** Derivations declaring `impureEnvVars` cannot be
   rebuilt faithfully (rio does not forward client env), so they are demoted:
   their recorded outputs are supplied like dependencies and they are reported
   as skips, never as regressions.
6. **Disconnect replay** (default on): requests whose recorded outcome was a
   client disconnect drop their channel at the recorded relative time instead
   of waiting for the build, exercising the gateway/scheduler disconnect
   paths. `--no-disconnect-replay` waits for completion instead.
7. **Comparison.** Per derivation, against `builds.jsonl`:
   - recorded success + replay success → compare per-output NAR hashes:
     all equal = **Match**, else **NonReproducible**;
   - recorded success + replay failure → retried up to
     `--confirm-regressions` times; persistent failure = **Regression**;
   - recorded deterministic failure → replay failure expected
     (**FailureReproduced** / **FailureNotReproduced** when it now succeeds);
   - recorded cancellation/disconnect/builder-error → informational;
   - replay outcomes of `AlreadyValid`/`Substituted` → **Skip** (the target
     already had the path; never counted as Match);
   - daemon refusal of an *upload* → **UploadRejected** for the affected
     requests, retried once on a fresh channel, never conflated with build
     regressions.
8. **Report.** Verdict counts, demoted/impure counts, timing stats, and a
   streamed `divergences.jsonl` with one record per non-Match verdict.
   Exit code follows `--fail-on` (default `none`: always 0 unless the run
   itself errored).

## Robustness requirements

- Every daemon operation runs under a deadline; SSH keepalives are enabled;
  a dead connection fails its in-flight requests and is not reused as-is: the
  pool skips closed connections and re-dials them lazily, because the gateway
  drops a connection whenever its last channel closes — routine, not
  exceptional.
- Validity probes and uploads are chunked; a refusal poisons only its channel.
- When a request depends on a path another in-flight request is currently
  uploading, it waits (bounded) for that upload to land before sending its
  own batch — references must exist on the target before dependents.
- Pre-warm degrades per path on substituter errors (a path that cannot be
  fetched is recorded and the affected requests are marked, not the whole run
  aborted). HTTP 403 from a substituter is logged and counted, never silently
  treated as "not covered".
- `--speedup` is validated; the report directory is per-run and never
  overwrites a previous run's divergence log.
- In-flight bookkeeping is keyed by a unique per-request id (the recorded
  session id is informational only and may collide).

## Output & observability

Per-run directory `.stress-test/replay/<ts>/`:

- `replay.log` — full tracing log (file layer), debug level;
- `divergences.jsonl` — streamed as divergences are found;
- `summary.json` — verdict counts, request/path/upload counts, wall-clock vs
  recorded-window timing, configuration echo.

Console output is `ui::step`-style phase lines plus a heartbeat (every ~5s:
in-flight count, oldest in-flight request and its stage). `--watch` adds a
periodic scheduler-metrics line (queue depth, executors, running builds), the
same scrape used by `xtask k8s qa --load`. No new rio metrics and no spec
rules: this is operator tooling, not a component of the system under test.

## Testing

- Unit tests: archive parsing against a small fixture archive (checked in as
  a plain directory under `xtask/tests/fixtures/replay/`), supply-ladder
  decisions, upload-plan ordering (references before dependents), timeline
  scheduling math, the full verdict-classification table, exit-code policy.
- rio-nix: codec unit tests for each new client op plus client⇄gateway
  conformance tests over an in-memory duplex.
- `cargo xtask k8s replay --dry-run --archive <fixture>` exercises the whole
  pipeline minus the network and is run in CI as a normal nextest test.
- Follow-up (not v1): a NixOS VM test replaying a tiny synthetic archive
  against a real in-VM gateway.

## Future work

- rio-native recording (producing this same format, or a v1 of it with a
  documented migration), at which point the archive reader moves out of xtask
  into a shared crate.
- qa-stage integration (`xtask k8s qa --replay <archive>`), TUI, archive
  `list`/`prune`, per-request SSH connections as a fidelity option, VM test.
