# Plan 997105502: Extract submit_build_grpc helper — 4× port-forward+grpcurl+JSON-parse boilerplate

Consolidator-mc120 finding. The port-forward + grpcurl-SubmitBuild + `|| true`-swallow-DeadlineExceeded + JSON-brace-find + `raw_decode` + `buildId`-extract pattern appears FOUR times:

| File:line | Subtest | Port | max-time | mTLS? |
|---|---|---|---|---|
| [`lifecycle.nix:556-585`](../../nix/tests/scenarios/lifecycle.nix) | cancel-cgroup-kill | 19099 | 5 | yes (k3s) |
| [`lifecycle.nix:707-747`](../../nix/tests/scenarios/lifecycle.nix) | build-timeout | 19098 | 3 | yes (k3s) |
| [`lifecycle.nix:858-882`](../../nix/tests/scenarios/lifecycle.nix) | build-timeout retry | 19097 | 3 | yes (k3s) |
| [`scheduling.nix:900-926`](../../nix/tests/scenarios/scheduling.nix) | cancel-timing | 9001 (direct) | 5 | no (standalone) |

Each is ~18 lines of near-identical Python (payload construction + port-forward + grpcurl invocation + JSON extraction + buildId assert). [P0360](plan-0360-device-plugin-vm-coverage.md) will add a 5th copy in its `privileged-hardening-e2e` fragment (it needs to SubmitBuild against the non-privileged fixture to prove the build completes).

**Existing [`sched_grpc` helper](../../nix/tests/scenarios/lifecycle.nix) at `:357-367`** can't absorb these: it hardcodes port 19001, `-max-time 30`, and uses `.succeed()` (no `|| true` swallow). SubmitBuild needs:
- **Unique port per call** — port-forward lacks `SO_REUSEADDR`, ~60s TIME_WAIT contention ([`lifecycle.nix:542`](../../nix/tests/scenarios/lifecycle.nix) comment)
- **Variable max-time** — build-timeout uses 3s (below buildTimeout=5), cancel-cgroup-kill uses 5s
- **`|| true`** — SubmitBuild streams BuildEvents; the build won't finish in max-time, grpcurl exits DeadlineExceeded
- **buildId extraction** — always needed; CancelBuild, cgroup-probing, timeout-polling all key on it

**This SUPERSEDES [P0304-T9](plan-0304-trivial-batch-p0222-harness.md)** which said "extract `submit_build_grpc(drv_path, priority=50)` … before the copy-paste happens" — the copy-paste HAS happened (4 copies, 5th coming). T9's scope was lifecycle.nix-only and assumed the `nix-instantiate`+`nix copy` prelude is part of the helper; the ACTUAL shared surface is narrower (grpcurl + JSON-parse) and spans two fixture styles.

## Entry criteria

- [P0289](plan-0289-port-specd-unlanded-test-trio.md) merged — **DONE** (build-timeout fragment at [`lifecycle.nix:678-893`](../../nix/tests/scenarios/lifecycle.nix) exists with copies 2-3)
- [P0294](plan-0294-build-crd-full-rip.md) merged — **DONE** (cancel-cgroup-kill retargeted to grpcurl at [`lifecycle.nix:518-676`](../../nix/tests/scenarios/lifecycle.nix), copy 1)

## Tasks

### T1 — `refactor(nix):` extract submit_build_grpc_k3s helper to lifecycle.nix testScript preamble

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) — add after `store_grpc` at `:379`:

```python
    # Port-allocation counter for submit_build_grpc. Each call gets
    # a unique local port (19100, 19101, …) to sidestep TIME_WAIT
    # contention — port-forward lacks SO_REUSEADDR, ~60s to rebind.
    # sched_grpc at :357 uses 19001 (safe — single-call lifetime);
    # store_grpc uses 19002. SubmitBuild calls stack within one
    # subtest (build-timeout does submit→timeout→retry-submit).
    _submit_port = iter(range(19100, 19200))

    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via port-forward + grpcurl. Returns buildId.

        `payload` is the SubmitBuildRequest dict (json.dumps'd internally).
        `max_time` caps the stream read — build usually won't finish,
        grpcurl exits DeadlineExceeded; `|| true` swallows. The build
        is persisted on receipt; stream is observability only.

        Raises AssertionError if no BuildEvent JSON in output (submit
        failed before streaming) or first event lacks buildId.
        """
        port = next(_submit_port)
        leader = leader_pod()
        out = k3s_server.succeed(
            f"k3s kubectl -n ${ns} port-forward {leader} {port}:9001 "
            f">/dev/null 2>&1 & pf=$!; "
            f"trap 'kill $pf 2>/dev/null' EXIT; sleep 2; "
            f"${grpcurl} ${grpcurlTls} -max-time {max_time} "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:{port} rio.scheduler.SchedulerService/SubmitBuild "
            f"2>&1 || true"
        )
        brace = out.find("{")
        assert brace >= 0, (
            f"no JSON in SubmitBuild output — submit failed? got: {out[:500]!r}"
        )
        first_ev, _ = json.JSONDecoder().raw_decode(out, brace)
        build_id = first_ev.get("buildId", "")
        assert build_id, (
            f"first BuildEvent missing buildId; got: {first_ev!r}"
        )
        return build_id
```

Migrate 3 lifecycle.nix call sites:
- `:556-585` (cancel-cgroup-kill) → `build_id = submit_build_grpc({"nodes": [...], "edges": []})`
- `:707-747` (build-timeout) → `build_id = submit_build_grpc({"nodes": [...], "edges": [], "buildTimeout": 5}, max_time=3)`
- `:858-882` (build-timeout retry) → `submit_build_grpc({"nodes": [...], "edges": []}, max_time=3)` (retry only needs brace≥0 assert, not buildId — but extracting it harmlessly satisfies the helper contract)

Each migration saves ~15-18 lines. Net ~-45 lines across lifecycle.nix.

### T2 — `refactor(nix):` extract submit_build_grpc_standalone helper to scheduling.nix

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — different fixture (standalone, withPki=false, no port-forward, direct `localhost:9001`). Add in the testScript preamble (near `sched_grpc` / `scrape_metrics` helpers if they exist, else near the top of the Python block):

```python
    def submit_build_grpc(payload: dict, max_time: int = 5) -> str:
        """SubmitBuild via plaintext gRPC direct to :9001. Returns buildId.

        Standalone fixture variant — no port-forward, no mTLS.
        Same JSON-parse + buildId-extract contract as lifecycle.nix's
        k3s variant; same `|| true` swallow-DeadlineExceeded.
        """
        out = ${gatewayHost}.succeed(
            f"grpcurl -plaintext -max-time {max_time} "
            f"-protoset ${protoset}/rio.protoset "
            f"-d '{json.dumps(payload)}' "
            f"localhost:9001 rio.scheduler.SchedulerService/SubmitBuild "
            f"2>&1 || true"
        )
        brace = out.find("{")
        assert brace >= 0, (
            f"no JSON in SubmitBuild output — submit failed?\n{out[:500]!r}"
        )
        first_ev, _ = json.JSONDecoder().raw_decode(out, brace)
        build_id = first_ev.get("buildId", "")
        assert build_id, (
            f"first BuildEvent missing buildId; got: {first_ev!r}"
        )
        return build_id
```

Migrate 1 scheduling.nix call site:
- `:900-926` (cancel-timing) → `build_id = submit_build_grpc({"nodes": [...], "edges": []})`

**Intentionally NOT unified across files** — the two fixtures differ (k3s mTLS + port-forward vs standalone plaintext). A common.nix-level helper would need fixture-shape conditionals; keeping them per-file is simpler. The SHARED surface is the Python body (JSON-parse + buildId); the fixture-specific part is the invocation prefix. If a third fixture adds another variant, revisit.

### T3 — `docs:` P0360 forward-pointer + P0304-T9 OBE mark

MODIFY [`.claude/work/plan-0360-device-plugin-vm-coverage.md`](plan-0360-device-plugin-vm-coverage.md) — add to its Tasks section (whichever T adds the privileged-hardening-e2e fragment) a forward-pointer:

```markdown
**Use `submit_build_grpc` helper** (landed in [P997105502](plan-997105502-extract-submit-build-grpc-helper.md)
— lifecycle.nix testScript preamble) instead of copy-pasting the
port-forward+grpcurl+JSON-parse block a 5th time. If P997105502 hasn't
merged yet, this fragment is the 5th copy — note it for later cleanup.
```

MODIFY [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md) at T9 header and body — mark OBE:

```markdown
### T9 — ~~`refactor(nix):` lifecycle.nix extract submit_build_grpc~~ — OBE, see [P997105502](plan-997105502-extract-submit-build-grpc-helper.md)

**SUPERSEDED** by [P997105502](plan-997105502-extract-submit-build-grpc-helper.md): the copy-paste HAS
happened (4 copies at lifecycle.nix:556/707/858 + scheduling.nix:900;
P0360 will add a 5th). T9's scope was lifecycle.nix-only and bundled
nix-instantiate+nix-copy into the helper; the actual shared surface is
narrower (grpcurl+JSON-parse) and spans two fixture styles. Skip T9;
P997105502 is the live plan.
```

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'def submit_build_grpc' nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/scheduling.nix` → 2 (T1+T2: both helpers defined)
- `grep -c 'submit_build_grpc(' nix/tests/scenarios/lifecycle.nix` → ≥4 (T1: 1 def + 3 call sites)
- `grep 'port-forward.*19099\|port-forward.*19098\|port-forward.*19097' nix/tests/scenarios/lifecycle.nix` → 0 hits (T1: hardcoded ports migrated to iterator)
- `grep 'brace = submit_out.find\|brace = out.find\|brace = retry_out.find' nix/tests/scenarios/lifecycle.nix` → 0 hits outside the helper body (T1: inline JSON-parse sites migrated)
- `grep 'brace = submit_out.find' nix/tests/scenarios/scheduling.nix` → 0 hits outside helper body (T2)
- `wc -l nix/tests/scenarios/lifecycle.nix` → reduced by ~40-50 vs pre-T1 (net save from 3× ~18L deduped → ~25L helper + 3× ~3L calls)
- T3: `grep 'OBE.*P997105502\|SUPERSEDED.*P997105502' .claude/work/plan-0304-*.md` → ≥1 hit (T9 marked)
- T3: `grep 'submit_build_grpc helper' .claude/work/plan-0360-*.md` → ≥1 hit (forward-pointer present)
- **Behavior preservation:** `cargo nextest run` test-count unchanged (no Rust touched); `/nixbuild .#checks.x86_64-linux.vm-lifecycle-recovery-k3s` still passes `cancel-cgroup-kill` + `build-timeout` subtests (confirmatory — same risk profile as clause-4c, VM test allocation permitting)

## Tracey

No spec markers. Test-helper extraction — the fragments under test already carry their `r[verify ...]` annotations at col-0 ([`lifecycle.nix` headers](../../nix/tests/scenarios/lifecycle.nix), [`scheduling.nix:23-52`](../../nix/tests/scenarios/scheduling.nix)). Consolidating the invocation mechanics doesn't change what's verified.

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1: +submit_build_grpc helper after store_grpc :379; migrate :556-585/:707-747/:858-882 call sites. ~-45L net"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T2: +submit_build_grpc standalone variant in testScript preamble; migrate :900-926 cancel-timing call site"},
  {"path": ".claude/work/plan-0360-device-plugin-vm-coverage.md", "action": "MODIFY", "note": "T3: forward-pointer to use submit_build_grpc helper in privileged-hardening-e2e fragment"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T3: mark T9 OBE — superseded by this plan"}
]
```

```
nix/tests/scenarios/
├── lifecycle.nix                 # T1: +helper, 3 call sites migrated
└── scheduling.nix                # T2: +helper (standalone variant), 1 call site
.claude/work/
├── plan-0360-*.md                # T3: forward-pointer
└── plan-0304-*.md                # T3: T9 OBE mark
```

## Dependencies

```json deps
{"deps": [289, 294], "soft_deps": [360, 304, 311, 997105501], "note": "consolidator-mc120 refactor. P0289 (DONE) landed build-timeout fragment (copies 2-3); P0294 (DONE) retargeted cancel-cgroup-kill to grpcurl (copy 1). P0240 (DONE) landed scheduling.nix cancel-timing (copy 4). discovered_from=consolidator(mc120). Soft-dep P0360 (UNIMPL — will be the 5th copy; T3 adds forward-pointer so P0360 uses the helper). Soft-dep P0304-T9 (T3 here marks it OBE — if P0304 dispatches before this, skip T9 there). Soft-dep P0311-T11 (per-build-timeout subtest tail-appends to scheduling.nix — may also need submit_build_grpc if it uses the grpcurl path; check at dispatch). Soft-dep P997105501 (sigint-graceful ordering — touches scheduling.nix subtests region; T2 here is preamble-helper, non-overlapping). lifecycle.nix count=17 (warm); T1 is a preamble-helper-add + 3 mid-file rewrites — medium conflict surface. scheduling.nix count=13; T2 is preamble-helper-add + 1 mid-file rewrite. Both files are HOT for subtests but the helper lands in the STABLE preamble region (~:357-400 range). Migration sites conflict with nothing currently UNIMPL (P0311-T11 is tail-append, P997105501-T2 is tail-of-sigint-fragment)."}
```

**Depends on:** [P0289](plan-0289-port-specd-unlanded-test-trio.md) — build-timeout fragment at `:678-893` exists (copies 2-3). [P0294](plan-0294-build-crd-full-rip.md) — cancel-cgroup-kill grpcurl retarget at `:518-676` exists (copy 1).
**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=17 — T1's 3 migration sites (`:556-585`, `:707-747`, `:858-882`) are mid-file. No UNIMPL plan touches those exact ranges; [P0304-T56](plan-0304-trivial-batch-p0222-harness.md) reorders ephemeral precondition assert ~`:1780` (well below). [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) count=13 — [P0311-T11](plan-0311-test-gap-batch-cli-recovery-dash.md) tail-appends per-build-timeout; [P997105501](plan-997105501-sigint-graceful-load50drv-ordering.md)-T2 tail-appends to sigint-graceful. T2 here edits `:900-926` (cancel-timing mid-file) + preamble — non-overlapping.
