# Plan 0311: Test-gap batch — cli non-empty assertions, failure_count recovery, dash annotation

Five test gaps from the reviewer sink. No open test-gap batch existed. T1-T3 are [P0216](plan-0216-rio-cli-subcommands.md) review findings — the cli ships pretty-print and filter code that the VM test never exercises because `MockAdmin` returns empty collections and `cli.nix` only asserts empty-case output. T4 is a [P0219](plan-0219-per-worker-failure-budget.md) coverage gap (`failure_count` recovery-init is documented but untested). T5 is a one-line annotation guidance note for [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) (marker bundles client+server; P0276 implements server-half but has zero `r[dash.*]` refs).

**Line numbers are from plan worktrees**, not sprint-1 — `rio-cli/src/main.rs` references are from p216 (`fe1a79a6`); re-grep at dispatch.

## Entry criteria

- [P0216](plan-0216-rio-cli-subcommands.md) merged (`print_build`, `BuildJson::from`, `workers --status`, GC dirty-close warning all exist)
- [P0219](plan-0219-per-worker-failure-budget.md) merged (`failure_count` recovery-init at [`derivation.rs:366`](../../rio-scheduler/src/state/derivation.rs)) — **DONE**
- [P0223](plan-0223-seccomp-localhost-profile.md) merged (seccomp CEL rules + `build_seccomp_profile` exist for T6/T7)
- [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) merged (CA corpus `.bin` files + README offset table exist for T8) — **DONE**
- [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) merged (`Mitigation` model + `flake mitigation` CLI verb exist for T9) — **DONE**
- [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) merged (mknod-whiteout fix landed, `fod-proxy.nix` T3 assertion written but unverified — T10 runs it)
- [P0267](plan-0267-atomic-multi-output-tx.md) merged (`PutPathBatch` handler + `upload_outputs_batch` + `gt13_batch_rpc_atomic` exist for T17-T19) — **DONE**

## Tasks

### T1 — `test(cli):` print_build + BuildJson::from populated path

MODIFY [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix) OR `rio-cli/tests/smoke.rs` (check which is the right home at dispatch — `cli.nix:167` comment in p216 claims "populated-state assertion lives in lifecycle.nix" but grep confirms nothing there):

[`main.rs:694`](../../rio-cli/src/main.rs) (p216) `print_build` and [`main.rs:592`](../../rio-cli/src/main.rs) `BuildJson::from` never execute in tests — `MockAdmin` returns `ListBuildsResponse { builds: vec![] }` and the test asserts `"(no builds)"` only. Add a populated case:

```python
# cli.nix — after the empty-case assertion, submit a real build via
# nix-build or grpcurl SubmitBuild, then:
with subtest("cli builds --json: populated"):
    out = client.succeed("rio-cli --json builds")
    data = json.loads(out)
    assert len(data["builds"]) >= 1, f"expected populated, got {out}"
    b = data["builds"][0]
    # Exercises BuildJson::from field mapping.
    assert "build_id" in b and "status" in b

with subtest("cli builds: human-readable populated"):
    out = client.succeed("rio-cli builds")
    # Exercises print_build formatting. Don't be too specific about
    # format — just prove the codepath ran (build_id appears somewhere).
    assert data["builds"][0]["build_id"] in out
```

If `cli.nix` uses `MockAdmin`, this needs a real scheduler — move to `lifecycle.nix` or extend `MockAdmin` to return a non-empty fixture. **Check at dispatch which is easier.**

### T2 — `test(cli):` workers --status filter non-empty

MODIFY same test file as T1. [`main.rs:320`](../../rio-cli/src/main.rs) (p216) `status_filter: status.unwrap_or_default()` is never exercised with a non-empty filter. The `--status` flag was added beyond the plan spec (reviewer note: "added beyond plan spec, never exercised non-empty").

```python
with subtest("cli workers --status alive: filter works"):
    # Assumes at least one alive worker exists (VM fixture has one).
    out = client.succeed("rio-cli --json workers --status alive")
    data = json.loads(out)
    assert all(w["status"] == "alive" for w in data["workers"])

with subtest("cli workers --status draining: empty when none draining"):
    out = client.succeed("rio-cli --json workers --status draining")
    data = json.loads(out)
    assert data["workers"] == []
```

This covers `r[sched.admin.list-workers]` at [`scheduler.md:129`](../../docs/src/components/scheduler.md): "The optional `status_filter` matches 'alive' (registered + not draining), 'draining', or empty/unknown (show all)."

### T3 — `test(cli):` GC dirty-close warning presence

MODIFY same test file. [`main.rs:445`](../../rio-cli/src/main.rs) (p216) emits `"warning: GC stream closed without is_complete"` when the stream ends without a terminal `is_complete` frame. Only the **absence** of this warning is tested (happy path). Never the presence.

Triggering dirty-close requires the scheduler/store to disconnect mid-sweep. Options:
- **Unit test** in `rio-cli/tests/` with a mock stream that yields one progress frame then `Ok(None)` (EOF) without `is_complete=true`
- **VM test** that kills the scheduler mid-`rio-cli gc` — harder to orchestrate reliably

Prefer the unit test:
```rust
// r[verify sched.admin.gc-stream-complete]  -- IF this marker exists; check at dispatch
#[tokio::test]
async fn gc_stream_dirty_close_warns() {
    // Mock GetGcProgress stream: yield one GcProgress{is_complete:false},
    // then Ok(None). Capture stderr. Assert contains "closed without
    // is_complete".
}
```

### T4 — `test(scheduler):` failure_count recovery-init

MODIFY `rio-scheduler/src/state/derivation.rs` tests or `rio-scheduler/tests/`. [`derivation.rs:366`](../../rio-scheduler/src/state/derivation.rs) `failure_count: row.failed_workers.len() as u32` in `from_recovery_row` is documented (comment at `:364`: "failure_count: initialize from failed_workers.len()") but no test asserts the recovered value matches `failed_workers.len()`:

```rust
// r[verify sched.poison.ttl-persist]
// Recovery loads failure_count from PG failed_workers array length.
// If this breaks, a derivation that failed twice before restart gets
// failure_count=0 after recovery → poison threshold effectively resets.
#[tokio::test]
async fn recovery_row_initializes_failure_count_from_failed_workers() {
    let row = /* recovery row fixture with failed_workers = vec![w1, w2] */;
    let state = DerivationState::from_recovery_row(row, /* ... */);
    assert_eq!(state.failure_count, 2);
}
```

Also cover `from_poisoned_row` at [`derivation.rs:434`](../../rio-scheduler/src/state/derivation.rs) — same `failed_workers.len() as u32` pattern.

### T5 — `docs:` P0276 r[dash.*] annotation guidance

MODIFY [`.claude/work/plan-0276-getbuildgraph-rpc-pg-backed.md`](plan-0276-getbuildgraph-rpc-pg-backed.md) — add a line to its `## Tracey` section:

The marker `r[dash.graph.degrade-threshold]` at [`dashboard.md:34`](../../docs/src/components/dashboard.md) bundles client AND server: "Graph rendering MUST degrade... The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`)." P0276 implements the **server half** (the 5000-node cap). It should carry `// r[impl dash.graph.degrade-threshold]` on the `LIMIT 5000` query. Currently P0276's `## Tracey` section has zero `r[dash.*]` refs.

One-line append to P0276's Tracey section:
```markdown
- `r[dash.graph.degrade-threshold]` — server-half (5000-node cap + `truncated` flag). Client-half (2000-node table fallback, 500-node Worker) is P0280.
```

### T6 — `test(controller):` cel_rules_in_schema +2 seccomp asserts

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — P0223 line refs (p223 worktree `:443-459`; re-grep post-P0223-merge).

`cel_rules_in_schema` at p223:`:443` asserts three CEL rules land in the generated schema. Comment at `:446` says "The three `#[x_kube(validation)]` rules". **There are now five** — P0223 added two seccomp rules at p223:`:291-292` (`type in [...]` and the Localhost-coupling ternary). The test is the **exact silent-drop guard** its docstring describes: "a `#[x_kube(validation)]` attribute silently dropped from the schema means the apiserver accepts invalid specs." The two new rules are unguarded.

The sibling `camel_case_renames` test at p223:`:469` **was** updated (+2 asserts for `seccompProfile` / `localhostProfile` at `:480+`). This test was missed.

```rust
// At p223 :446, fix the comment:
// The five #[x_kube(validation)] rules, verbatim.

// After the existing 3 asserts (p223 :447-458), add:
// r[verify worker.seccomp.localhost-profile]
// P0223 seccomp: type-in-set + localhost-coupling rules.
// CEL rule text must appear verbatim in the generated schema JSON —
// if kube-derive changes its x_kube processing and drops these,
// the apiserver accepts {type: "bogus"} or {type: "Localhost"}
// with no profile path.
assert!(
    json.contains("self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']"),
    "seccomp type-in-set CEL rule missing from schema"
);
assert!(
    json.contains("self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)"),
    "seccomp localhost-coupling CEL rule missing from schema"
);
```

**Strongest test-gap in this batch** — this test exists *specifically* to catch exactly this failure mode, and it's blind to the two rules it was meant to guard.

**Interaction with [P0304](plan-0304-trivial-batch-p0222-harness.md) T11:** If T11 (CEL `.message()`) changes the rule text (e.g., wraps in `Rule::new(...)`), the verbatim `json.contains(...)` asserts here may need to match whatever kube-derive emits. Check at dispatch — T11 changes the attr SYNTAX, not necessarily the schema OUTPUT. If the schema output is unchanged (message goes in a separate JSON field), these asserts are stable. If P0304 lands first, read the regenerated CRD YAML to confirm the rule strings.

### T7 — `test(controller):` Unconfined arm in build_seccomp_profile

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) — P0223 line refs (p223 worktree `:702-722`; re-grep post-P0223-merge).

`build_seccomp_profile` at p223:`:702` has four match arms. P0223's tests cover `None`→`RuntimeDefault`, `Some("Localhost")`, and the privileged-drops path. `Some("Unconfined")` at `:711-714` is **untested**. The doc-comment at `:691` says "debugging-only" and at `:697-700` "defensive... fail-closed" — the code exists to AVOID falling through to Unconfined on a typo, but the arm that intentionally RETURNS Unconfined is a shipped, reachable branch with zero coverage.

```rust
// r[verify worker.seccomp.localhost-profile]
// Unconfined is debugging-only per the spec (security.md:56 "never
// production"). The arm is trivial (sets type_, nothing else) but
// it IS a shipped match arm — cover it so refactors don't silently
// merge it into the wildcard.
#[test]
fn build_seccomp_profile_unconfined() {
    let kind = SeccompProfileKind {
        type_: "Unconfined".into(),
        localhost_profile: None,
    };
    let profile = build_seccomp_profile(Some(&kind));
    assert_eq!(profile.type_, "Unconfined");
    assert_eq!(profile.localhost_profile, None);
}
```

~10 lines. Placement: alongside the existing seccomp builder tests (find them via `grep 'fn.*seccomp.*test\|build_seccomp_profile' builders.rs` at dispatch).

### T8 — `test(gateway):` CA corpus .bin parse-roundtrip — static bytes, no daemon

NEW [`rio-gateway/tests/golden/ca_corpus.rs`](../../rio-gateway/tests/golden/ca_corpus.rs). Three `.bin` files staged by [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) at [`rio-gateway/tests/golden/corpus/`](../../rio-gateway/tests/golden/corpus/README.md) have **zero `.rs` consumers**. [`golden/daemon.rs`](../../rio-gateway/tests/golden/daemon.rs) is live-daemon; these need a separate **static** parse-roundtrip test: read bytes → parse → assert fields match the README offset table → re-serialize → assert byte-identical.

Without this test, the corpus bitrots silently: if upstream Nix bumps the fixture shapes (new schema version, field reorder), nothing in rio notices until a live-daemon test fails much later with an opaque wire error.

~40 lines using [`rio_nix::protocol::wire`](../../rio-nix/src/protocol/wire/mod.rs) primitives (`read_string`, `read_u64`):

```rust
//! Static parse-roundtrip tests for the CA Realisation wire corpus.
//! These bytes are Nix's own golden fixtures (see corpus/README.md
//! provenance) — if rio's parse diverges, this catches it before
//! any live-daemon test.

use std::io::Cursor;
use rio_nix::protocol::wire;

const REGISTER_2DEEP: &[u8] = include_bytes!("corpus/ca-register-2deep.bin");
const REGISTER_HISTORICAL: &[u8] = include_bytes!("corpus/ca-register-with-deps-historical.bin");
const QUERY_2DEEP: &[u8] = include_bytes!("corpus/ca-query-2deep.bin");

#[tokio::test]
async fn ca_register_2deep_parses_and_roundtrips() {
    // Per README offset table: two frames, both dependentRealisations:{}.
    // Frame 1: len=176 @ 0x000, JSON @ 0x008..0x0b7.
    // Frame 2: len=367 @ 0x0b8, JSON @ 0x0c0..0x22e, 1 pad byte.
    let mut cur = Cursor::new(REGISTER_2DEEP);
    let json1 = wire::read_string(&mut cur).await.unwrap();
    let v1: serde_json::Value = serde_json::from_str(&json1).unwrap();
    assert_eq!(v1["dependentRealisations"], serde_json::json!({}));
    assert_eq!(v1["id"].as_str().unwrap(),
        "sha256:15e3c560894cbb27085cf65b5a2ecb18488c999497f4531b6907a7581ce6d527!baz");
    assert_eq!(v1["outPath"].as_str().unwrap(),
        "g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo");
    assert_eq!(v1["signatures"], serde_json::json!([]));

    let json2 = wire::read_string(&mut cur).await.unwrap();
    let v2: serde_json::Value = serde_json::from_str(&json2).unwrap();
    assert_eq!(v2["signatures"].as_array().unwrap().len(), 2);

    // EOF: cursor fully consumed.
    assert_eq!(cur.position() as usize, REGISTER_2DEEP.len());

    // Roundtrip: re-serialize via wire::write_string, assert byte-identical.
    let mut out = Vec::new();
    wire::write_string(&mut out, &json1).await.unwrap();
    wire::write_string(&mut out, &json2).await.unwrap();
    assert_eq!(out, REGISTER_2DEEP, "re-serialize must be byte-identical");
}

#[tokio::test]
async fn ca_register_historical_has_nonempty_deps() {
    // Per README: ONE frame, len=484, historical non-empty dependentRealisations
    // (Nix keeps this fixture for back-compat read testing only). rio-gateway
    // should accept-and-discard defensively — this test proves we CAN parse
    // the historical shape even though we never expect to see it.
    let mut cur = Cursor::new(REGISTER_HISTORICAL);
    let json = wire::read_string(&mut cur).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let deps = v["dependentRealisations"].as_object().unwrap();
    assert_eq!(deps.len(), 1, "historical fixture has exactly one dep entry");
    // Key format: sha256:<64-hex>!<name>, value format: store-path-basename
    let (k, val) = deps.iter().next().unwrap();
    assert!(k.starts_with("sha256:") && k.contains('!'));
    assert!(!val.as_str().unwrap().starts_with("/nix/store/"));  // basename only
    assert_eq!(cur.position() as usize, REGISTER_HISTORICAL.len());
}

#[tokio::test]
async fn ca_query_2deep_drvoutput_ids() {
    // Per README: two DrvOutput frames. len=75 @ 0x000, len=76 @ 0x058.
    let mut cur = Cursor::new(QUERY_2DEEP);
    let id1 = wire::read_string(&mut cur).await.unwrap();
    assert!(id1.starts_with("sha256:15e3c560") && id1.ends_with("!baz"));
    let id2 = wire::read_string(&mut cur).await.unwrap();
    assert!(id2.starts_with("sha256:6f869f9e") && id2.ends_with("!quux"));
    assert_eq!(cur.position() as usize, QUERY_2DEEP.len());
}
```

**Add to [`golden/mod.rs`](../../rio-gateway/tests/golden/mod.rs):** `mod ca_corpus;` (or `#[path]` include — match the existing pattern for `daemon.rs`).

**Check at dispatch:** `wire::read_string` signature — it may take `&mut impl AsyncRead` (requiring `tokio::io::BufReader<Cursor<_>>` wrapper) or `&mut (impl AsyncRead + Unpin)` (Cursor works directly via `tokio_util::compat` or `Cursor` is already `AsyncRead` via tokio). Grep [`wire/mod.rs:96`](../../rio-nix/src/protocol/wire/mod.rs) for the bound. If Cursor isn't directly compatible, wrap in `tokio::io::BufReader::new(Cursor::new(...))`.

### T9 — `test(harness):` Mitigation model pattern + flake-mitigation happy-path

MODIFY [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py). [P0317](plan-0317-excusable-vm-regex-knownflake-schema.md) T5 shipped the `flake mitigation` verb at [`cli.py:369-383`](../../.claude/lib/onibus/cli.py) and T6 shipped 5 tests for T1-T3 surfaces — but zero for T5. Zero test constructs [`Mitigation`](../../.claude/lib/onibus/models.py) directly. The `landed_sha: Field(pattern=r"^[0-9a-f]{8,40}$")` at [`models.py:145`](../../.claude/lib/onibus/models.py) is unverified: rejects 7-char git-short, rejects uppercase — neither tested.

The existing `flake add`/`flake remove` test at [`:1063-1074`](../../.claude/lib/test_scripts.py) provides sibling coverage but doesn't touch `mitigation`.

Add two tests near `:1063`:

```python
def test_mitigation_landed_sha_pattern():
    """Mitigation.landed_sha Field(pattern=r"^[0-9a-f]{8,40}$") — verified.
    Rejects: 7-char git-short (too ambiguous for a permanent record),
    uppercase hex (git is lowercase), non-hex. Accepts: 8-char abbrev,
    40-char full."""
    from onibus.models import Mitigation
    from pydantic import ValidationError
    import pytest

    # 8-char: accepted (minimum)
    Mitigation(plan=999, landed_sha="deadbeef", note="n")
    # 40-char: accepted (full)
    Mitigation(plan=999, landed_sha="a" * 40, note="n")
    # 7-char: rejected
    with pytest.raises(ValidationError):
        Mitigation(plan=999, landed_sha="abc1234", note="n")
    # uppercase: rejected
    with pytest.raises(ValidationError):
        Mitigation(plan=999, landed_sha="DEADBEEF", note="n")
    # 41-char: rejected (max 40)
    with pytest.raises(ValidationError):
        Mitigation(plan=999, landed_sha="a" * 41, note="n")


def test_flake_mitigation_appends_and_preserves_header(tmp_repo: Path):
    """flake-mitigation verb: appends to target's mitigations list,
    preserves header comments, atomic rewrite. The cli.py:369-383
    happy-path — not-found (rc=1) and ambiguous (rc=2) are
    P0322's negative-path tests."""
    lib = tmp_repo / ".claude" / "lib"
    _copy_harness(lib)
    flakes = tmp_repo / ".claude" / "known-flakes.jsonl"
    # Seed: header + one row with empty mitigations list.
    row = KnownFlake(
        test="vm-target", symptom="s", root_cause="rc",
        fix_owner="P0999", fix_description="d", retry="Once",
    )
    flakes.write_text("# HEADER ONE\n# HEADER TWO\n" + row.model_dump_json() + "\n")

    r = subprocess.run(
        [
            ".claude/bin/onibus", "flake", "mitigation",
            "--test", "vm-target", "--plan", "316",
            "--sha", "900ac467", "--note", "accel=kvm override",
        ],
        cwd=tmp_repo, capture_output=True, text=True, check=True,
    )
    # (Check argparse shape at dispatch — may be positional not --flags.)

    content = flakes.read_text()
    # Header preserved (both lines, original order).
    lines = content.splitlines()
    assert lines[0] == "# HEADER ONE"
    assert lines[1] == "# HEADER TWO"
    # Mitigation appended: re-read via model.
    from onibus.jsonl import read_jsonl
    rows = read_jsonl(flakes, KnownFlake)
    assert len(rows) == 1
    assert len(rows[0].mitigations) == 1
    m = rows[0].mitigations[0]
    assert m.plan == 316
    assert m.landed_sha == "900ac467"
    assert m.note == "accel=kvm override"
```

**Check at dispatch:** `flake mitigation` argparse shape — grep `cli.py` for the `mitigation` subparser. The test's `["--test", ..., "--plan", ..., "--sha", ..., "--note", ...]` invocation is a guess; positional args are also plausible.

### T10 — `test(nix):` run vm-fod-proxy-k3s on KVM-capable builder (P0308 T3 integration proof)

**Reminder task — no code change.** [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) T3's `elapsed < 45s` assertion at [`fod-proxy.nix:319`](../../nix/tests/scenarios/fod-proxy.nix) (region: the Q4 hard-assert block, re-grep at dispatch) shipped but was never verified in the VM. Both attempts allocated to `ec2-builder8` (slurm sticky allocation, KVM-denied on that host). The mechanism (mknod-whiteout to fast-fail ENOENT-spin) is host-kernel-verified and r3-validator static-proven — but the VM integration proof is pending.

**What to do:** when a KVM-capable builder is available (i.e., not `ec2-builder8`, or after the fleet's KVM-denied population is fixed):

```bash
/nixbuild .#checks.x86_64-linux.vm-fod-proxy-k3s
```

If green: P0308 T3's exit criterion is satisfied, close the test-gap. If red (elapsed ≥ 45s or the assertion fails): the mknod-whiteout fix doesn't propagate through the full k3s-in-VM stack — re-open P0308 with the VM log.

**Risk profile (why this is test-gap not correctness):** The fix cannot REGRESS anything — FOD failures already hang without it. Non-FOD safety is proven (the whiteout only affects paths the FOD builder never created). So a red VM test means "the fix doesn't HELP in the k3s stack", not "the fix broke something". Worst case is status-quo.

**If the builder allocation keeps landing on KVM-denied hosts:** the coordinator can fast-path this via nextest-standalone as evidence of the rust-tier, then manually verify the VM test once locally or via a targeted single-VM run. The P0308 mechanism is solid enough that integration proof is confirmatory, not gating.

### T11 — `test(vm):` scheduling.nix per-build-timeout chain — ROUTE 2 (gRPC-only)

**P0329 RESOLVED → outcome (b), Claim B holds.** Nix `SSHStore::setOptions()` is an **empty override** (ssh-store.cc, unchanged since 088ef8175, 2018-03-05) — `wopSetOptions` never reaches rio-gateway via `ssh-ng://`. Source-verified against pinned flake input; regression-guarded by the `setoptions-unreachable` fragment in scheduling.nix. `--option build-timeout N --store ssh-ng://` is a silent no-op. **Proceed via Route 2 below.** Route 1 is dead; Route 3 does not apply (the feature is live via gRPC `SubmitBuildRequest.build_timeout` field 6).

**Prior blocker note (now resolved):** If the answer had been "no" (ssh-ng never sends wopSetOptions — Claim B in 0329), this test as-written would submit a build with `--option build-timeout 10`, the option would be silently dropped, the `sleep 60` would run to completion, and the test would flake-fail at the 15s wall-clock assert. That's exactly what would happen — the feature works, the CLI path to it doesn't.

[P0214](plan-0214-per-build-timeout.md) T3 was skipped — the original VM integration test for the timeout→CancelSignal→worker-SIGKILL chain. The unit test at [`actor/tests/worker.rs`](../../rio-scheduler/src/actor/tests/worker.rs) (grep for `DebugBackdateSubmitted`) has no worker attached, so `to_cancel` is empty — it proves `transition_build_to_failed` fires but NOT that `CancelSignal` reaches a worker. The chain is:

```
handle_tick :582-589 (collect timed-out builds)
  → cancel_build_derivations :605
    → sends CancelSignal to worker's stream_tx
      → worker receives → cgroup.kill()
  → transition_build_to_failed :606
```

The unit test covers the last arrow. VM test covers the full chain.

**After P0329 resolves — three routes:**

**Route 1 — ssh-ng reachable (0329 T2-path-A):** use the test as P0214 T3 originally sketched, but with the CORRECT option name. P0214's plan doc says `--option timeout` at [`:61`](plan-0214-per-build-timeout.md) but the gateway keys on `"build-timeout"` at [`handler/mod.rs:77`](../../rio-gateway/src/handler/mod.rs). Use `--option build-timeout`.

```python
with subtest("per-build timeout: ssh-ng --option build-timeout → CancelSignal → Failed"):
    # 10s timeout, 60s build. Expect Failed within ~15s wall-clock.
    start = time.monotonic()
    # r[verify sched.timeout.per-build]
    client.fail(  # nix-build should exit non-zero (build failed)
        "nix-build --option build-timeout 10 "
        "-E 'derivation { name=\"sleeper\"; system=\"x86_64-linux\"; "
        "builder=\"/bin/sh\"; args=[\"-c\" \"sleep 60\"]; }' "
        "--store ssh-ng://rio-gateway"
    )
    elapsed = time.monotonic() - start
    assert elapsed < 20, f"timeout should fire ~10s, got {elapsed}s (did --option propagate?)"
    assert elapsed > 8, f"timeout fired too early: {elapsed}s (different failure?)"

    # Verify the metric incremented (proves :593 fired, not some other path).
    metrics = scheduler.succeed("curl -s localhost:9090/metrics | grep build_timeouts_total")
    assert "rio_scheduler_build_timeouts_total 1" in metrics
```

**Route 2 — gRPC-only (0329 T2-path-B):** submit via `grpcurl SubmitBuild` with `BuildOptions.build_timeout = 10` explicitly in the request proto. Same assertions; different submit path. Proves the feature works for API consumers even though CLI is dead.

**Route 3 — OBE:** if 0329 finds the feature is truly dead (no path sets `build_timeout > 0`), this task is obsolete. Record and close.

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — TAIL append (same as P0214/P0215 convention; both their T3s also targeted tail-append here, neither landed). If P0329 T2-path-A keeps its PROBE subtest as a real test, place T11's subtest AFTER it.

### T12 — `test(scheduler):` class_drift_total behavioral assertion — drift-without-penalty

Bughunter finding. [`completion.rs:390-395`](../../rio-scheduler/src/actor/completion.rs) emits `rio_scheduler_class_drift_total` when `classify(actual) != assigned_class`. The metric is **spec'd** at [`observability.md:103`](../../docs/src/observability.md) ("Cutoff-drift signal — decoupled from penalty logic. A build can trigger drift without penalty (actual barely over cutoff, under 2×)"). But the ONLY test coverage is the **name-check** at [`metrics_registered.rs:49`](../../rio-scheduler/tests/metrics_registered.rs) — string-in-a-list. Easy to:
- swap `classify()` argument order at `:385-387` → silently wrong bucket
- invert `!=` to `==` at `:388` → fires on NON-drift
- delete the `if let Some(actual_class) =` guard → unwrap-panic or silent no-op

All would pass `all_spec_metrics_have_describe_call`.

Sibling metric `rio_scheduler_misclassifications_total` HAS a behavioral test: [`tests/completion.rs:724`](../../rio-scheduler/src/actor/tests/completion.rs) `test_misclass_detection_on_slow_completion` — asserts via `logs_contain("misclassification")` at `:811` + EMA-write side-effect at `:823-831`. Mirror that pattern for drift, but use `CountingRecorder` for the metric directly (drift has no warn-log, only the counter).

**The threshold difference is the test design:**
- `misclassifications_total` fires at `duration > 2× cutoff` (`:400`) — a 100s build at 30s small-cutoff = 3.3× → **both** drift AND penalty fire (the sibling test proves penalty, drift fires too but isn't checked)
- `class_drift_total` fires at `classify(actual) != assigned` — a 40s build at 30s small-cutoff = 1.3× → drift fires (40s classifies as medium ≠ small), penalty does NOT (40 < 60)

The **drift-without-penalty** window is the unique test case. MODIFY [`tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) — add after `test_misclass_detection_on_slow_completion` at `:834`:

```rust
/// class_drift_total fires on ANY actual≠assigned mismatch, decoupled
/// from the 2×-cutoff penalty threshold. This tests the drift-WITHOUT-
/// penalty window: duration past cutoff but under 2× — drift counter
/// increments, penalty-overwrite and misclass warn-log do NOT fire.
///
/// Sibling test_misclass_detection_on_slow_completion covers the
/// >2× case where BOTH fire. This test isolates drift alone.
// r[verify obs.metric.scheduler]  — behavioral, not just name-in-list
#[tokio::test]
#[traced_test]
async fn test_class_drift_fires_without_penalty() -> TestResult {
    // Same setup as sibling: small-class worker, no EMA pre-seed
    // → default classify() routes to "small" (30s cutoff).
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_with(/* ... same as :730-742 */);

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    // ... worker + merge + dispatch (mirror :746-774, distinct pname
    //     to avoid cross-test build_history pollution)

    // Complete with duration=40s. 40 > 30 (cutoff) → classify()
    // picks "medium" → drift fires. 40 < 60 (2×30) → penalty does NOT.
    handle.send_unchecked(ActorCommand::ProcessCompletion {
        // ... start_time: seconds:1000, stop_time: seconds:1040 ...
    }).await?;
    barrier(&handle).await;

    // Precondition self-check: the recorder saw SOME counter. A broken
    // set_default_local_recorder install (wrong thread, wrong guard scope)
    // would make the asserts below pass vacuously on zero-keys.
    assert!(
        !recorder.all_keys().is_empty(),
        "recorder captured zero counters — set_default_local_recorder scope broken?"
    );

    // THE ASSERTION: drift fired with correct labels.
    assert_eq!(
        recorder.get("rio_scheduler_class_drift_total{actual_class=medium,assigned_class=small}"),
        1,
        "class_drift should fire: 40s at small(30s) cutoff classifies as medium. \
         All counter keys: {:?}", recorder.all_keys()
    );

    // THE NEGATIVE ASSERTION: penalty did NOT fire. Proves decoupling.
    assert_eq!(
        recorder.get("rio_scheduler_misclassifications_total{}"),
        0,
        "misclass should NOT fire: 40s < 2×30s=60s penalty threshold"
    );
    assert!(
        !logs_contain("misclassification"),
        "penalty warn-log should NOT fire in drift-only window"
    );

    Ok(())
}
```

**Label-key ordering:** `CountingRecorder::counter_key()` sorts labels alphabetically — `actual_class` before `assigned_class`. Verify at dispatch; if the key format is `{assigned_class=small,actual_class=medium}` instead, the assertion needs flipping. The `all_keys()` dump in the failure message shows the actual format on first run.

**If [P0330](plan-0330-test-recorder-extraction-test-support.md) has landed:** `CountingRecorder` is imported from `rio_test_support::metrics::CountingRecorder` (or via the `helpers.rs` re-export — same thing). If not yet landed: use `helpers::CountingRecorder` as `test_misclass` does at `:724`.

### T13 — `test(nix):` run vm-scheduling-disrupt-standalone on KVM — cancel-timing + load-50drv never executed

**Reminder task — no code change.** Bughunter finding (mc=51-56 range): four VM test fragments shipped via clause-4 drv-identity fast-path and have **never executed under KVM**. T13 covers two of them bundled in the same VM test attr.

[P0240](plan-0240-vm-section-fj-scheduling.md) (DONE) added `cancel-timing` at [`scheduling.nix:841-997`](../../nix/tests/scenarios/scheduling.nix) and `load-50drv` at [`:1020-1089`](../../nix/tests/scenarios/scheduling.nix). Both are subtests under [`vm-scheduling-disrupt-standalone`](../../nix/tests/default.nix) at `:210-235`. The merge was clause-4(a) fast-pathed — nix/tests-only delta, rust-tier untouched. Correct at the time; but the test bodies themselves were never executed.

**Load-bearing assertions never proven:**
- `cancel-timing` at [`:866`](../../nix/tests/scenarios/scheduling.nix): cgroup gone within 5s of `CancelBuild`. The `elapsed` check at `:996` is the assertion. Sibling test `lifecycle.nix:cancel-cgroup-kill` covers the same chain but on a different fixture — this subtest is the **scheduling-fixture** coverage.
- `load-50drv` at [`:1036`](../../nix/tests/scenarios/scheduling.nix): 50-leaf fanout completes, `assignments ≥ 50`. The [`:230-232`](../../nix/tests/default.nix) comment explicitly notes TCG could stretch this to 150s — the `globalTimeout = 900` bump was for THIS subtest. Timeout reasoning never validated.

**What to do:** when a KVM-capable builder is available:

```bash
/nixbuild .#checks.x86_64-linux.vm-scheduling-disrupt-standalone
```

If green: both subtests' exit criteria satisfied. If red on `cancel-timing`: the cgroup-gone-in-5s timing doesn't hold under VM — re-tune or investigate why VM adds latency. If red on `load-50drv`: either the 50-leaf fanout tickles a scheduler bug at scale, or the 900s timeout is too tight under TCG (the `:230` comment's 150s estimate was a guess).

**Risk profile (test-gap not correctness):** Neither subtest introduces new production code — they exercise existing scheduler paths at higher cardinality / tighter timing. A red test means "the assertion was too optimistic" or "scale tickles a latent bug", not "we shipped something broken". Worst case: re-tune timing bounds. Best case: finds a real scale bug that existing lower-cardinality tests missed.

### T14 — `test(nix):` run vm-scheduling-disrupt-standalone — setoptions-unreachable fragment

**Same VM attr as T13; same reminder.** [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) (DONE) added `setoptions-unreachable` at [`scheduling.nix:777-838`](../../nix/tests/scenarios/scheduling.nix). Fast-path merged alongside P0329's other changes. The subtest carries `r[verify gw.opcode.set-options.propagation+2]` at [`:777`](../../nix/tests/scenarios/scheduling.nix) — the ONLY `r[verify]` for the `+2` bump of this marker.

**What it proves:** [`:811`](../../nix/tests/scenarios/scheduling.nix) subtest greps ALL gateway journal history for evidence that `wopSetOptions` hit the wire. The `:220` comment in [`default.nix`](../../nix/tests/default.nix) notes it's placed after `sizeclass` + `max-silent-time` to cover THEIR ssh-ng sessions too. A green result closes the `r[verify gw.opcode.set-options.propagation+2]` gap that `tracey query untested` currently shows (once T31 of [P0304](plan-0304-trivial-batch-p0222-harness.md) makes tracey scan it — no wait, this is `nix/tests/*.nix` which IS already scanned; the `+2` marker may show as verified-but-never-run, which tracey can't distinguish).

**Bundled with T13** — same `/nixbuild` invocation covers all three subtests (`setoptions-unreachable`, `cancel-timing`, `load-50drv`). One KVM run, three proofs.

### T15 — `test(nix):` run vm-netpol-k3s on KVM — P0241's FIRST-build fragment

**Reminder task — no code change.** [P0241](plan-0241-vm-section-g-netpol.md) (DONE) added the entire [`vm-netpol-k3s`](../../nix/tests/default.nix) attr at `:364-371`. Bughunter notes the merge log said "**FIRST build**" — the drv had never been built at all, not even cached-green. Fast-path clause-4(a) applied (nix/tests-only delta after [P0220](plan-0220-netpol-preverify-decision.md) proved kube-router enforces).

**Load-bearing assertion — proves-nothing positive-control:** [`netpol.nix:125`](../../nix/tests/scenarios/netpol.nix) (`netpol-positive: allowed egress (scheduler:9001) connects`) is the [P0305-pattern](plan-0305-nar-roundtrip-chunk-count-precondition.md) self-validation: it proves the test CAN succeed (egress-in-general works from inside the worker pod) BEFORE the negative assertions (`netpol-kubeapi`, `netpol-imds`, `netpol-internet`) at `:153`, `:180`, `:196`. Without the positive-control, a blocked-everything test passes vacuously when the pod has NO networking at all. This self-check has never run.

```bash
/nixbuild .#checks.x86_64-linux.vm-netpol-k3s
```

If green: P0241's three exit criteria (IMDS blocked, public blocked, k8s-API blocked) hold AND the positive-control (allowed egress works) validates the test isn't vacuous. If red on `netpol-positive`: the pod has no egress at all — fixture problem, not NetworkPolicy problem (investigate k3s-full's `networkPolicy.enabled=true` rendering). If red on any `netpol-*` block: kube-router enforcement doesn't match [P0220](plan-0220-netpol-preverify-decision.md)'s standalone verify — k3s-full fixture differs somehow.

**netpol.nix has NO `r[verify]` markers** — the finding's "All have r[verify] markers" was imprecise for this one. The subtests reference `r[sec.*]` concepts (`sec.boundary.*`, IMDS blocking from [`security.md:80`](../../docs/src/security.md)) but no formal marker for "NetworkPolicy egress enforcement". Optional: add `r[sec.netpol.egress-deny]` to [`security.md`](../../docs/src/security.md) and annotate — but that's a spec addition, out of scope for a reminder task. Note it for a followup if the VM test proves valuable.

### T16 — `test(nix):` KVM-pending tracking — consolidate T10/T13/T14/T15 into one run-list

**Meta-task.** T10, T13, T14, T15 are all the same shape: "run `.#checks.x86_64-linux.<X>` when KVM is available". Four reminder-tasks is fragmentation. Consolidate into a single "pending KVM execution" tracking note — either:

**Option A — inline comment at [`default.nix`](../../nix/tests/default.nix) top:** a block comment listing which attrs have never KVM-executed, updated as they're run. Cheap, visible, but manual.

**Option B — `.claude/notes/kvm-pending.md`:** structured list with plan-of-origin, merge-commit, assertion-summary. Coordinator consults it when a KVM-capable slot opens. The `/nixbuild` invocation for the whole pending set:

```bash
# All four pending-KVM attrs in one go (each is a separate drv, builds in parallel):
/nixbuild .#checks.x86_64-linux.vm-fod-proxy-k3s \
          .#checks.x86_64-linux.vm-scheduling-disrupt-standalone \
          .#checks.x86_64-linux.vm-netpol-k3s
```

Three attrs cover four T-items (T13+T14 share `vm-scheduling-disrupt-standalone`).

**Prefer Option B** — `.claude/notes/` is the design-provenance home per coordinator convention. The note is NOT a plan doc (no `plan-NNNN-` prefix); it's a manifest the coordinator reads at KVM-slot-open time. When a run goes green, delete the line. When all lines gone, delete the file.

### T17 — `test(store):` PutPathBatch FailedPrecondition (≥INLINE_THRESHOLD)

NEW test in [`rio-store/tests/grpc/chunked.rs`](../../rio-store/tests/grpc/chunked.rs) after `gt13_batch_rpc_atomic` at [`:377`](../../rio-store/tests/grpc/chunked.rs). [`put_path_batch.rs:245-252`](../../rio-store/src/grpc/put_path_batch.rs) rejects any output ≥ `INLINE_THRESHOLD` (256 KiB at [`cas.rs:32`](../../rio-store/src/cas.rs)) with `Status::failed_precondition` — the v1 inline-only bound. No test sends an oversize NAR and asserts the code + message + clean placeholder state.

```rust
/// PutPathBatch v1 is inline-only: any output >= INLINE_THRESHOLD (256 KiB)
/// is rejected with FailedPrecondition — the client-side signal to fall
/// back to independent PutPath. Assert:
///   - code == FailedPrecondition (not InvalidArgument — the request
///     shape is valid, it's a "use the other RPC" signal)
///   - message names the output index
///   - placeholders cleaned up (abort_batch ran — oversize check is
///     at :245 AFTER phase-2 placeholder insert for PRIOR outputs)
// r[verify store.atomic.multi-output]
#[tokio::test]
async fn gt13_batch_oversize_failed_precondition() -> TestResult {
    let s = StoreSession::new().await?;

    // Output 0: small, valid. Output 1: 256 KiB + 1 — over the threshold.
    let out0_path = test_store_path("oversize-out0");
    let (out0_nar, _) = make_nar(b"tiny");
    let out0_info = make_path_info_for_nar(&out0_path, &out0_nar);

    let out1_path = test_store_path("oversize-out1");
    // NAR-wrap 256 KiB of content → NAR size = 256 KiB + NAR overhead
    // (~100 bytes header+trailer). Well over INLINE_THRESHOLD.
    let big_content = vec![0x42u8; rio_store::cas::INLINE_THRESHOLD];
    let (out1_nar, _) = make_nar(&big_content);
    assert!(
        out1_nar.len() >= rio_store::cas::INLINE_THRESHOLD,
        "precondition: NAR must be >= INLINE_THRESHOLD for this test"
    );
    let out1_info = make_path_info_for_nar(&out1_path, &out1_nar);

    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, out0_info.into(), out0_nar).await;
    send_batch_output(&tx, 1, out1_info.into(), out1_nar).await;
    drop(tx);

    let mut client = s.client.clone();
    let r = client.put_path_batch(ReceiverStream::new(rx)).await;
    let status = r.expect_err("oversize output-1 must be rejected");
    assert_eq!(
        status.code(), tonic::Code::FailedPrecondition,
        "FailedPrecondition is the fall-back-to-PutPath signal — \
         NOT InvalidArgument (request shape is fine, it's a \
         use-the-other-RPC hint)"
    );
    assert!(status.message().contains("output 1"));
    assert!(status.message().contains("INLINE_THRESHOLD"));

    // Cleanup: output-0's placeholder was inserted at phase-2 iteration 0
    // BEFORE phase-2 iteration 1 hit the :245 oversize check. bail! at
    // :246 must have called abort_batch → zero 'uploading' rows.
    //
    // WAIT — check the code order: :245 oversize check is BEFORE :268
    // insert_manifest_uploading in the SAME iteration. So for output-1,
    // no placeholder was inserted before the oversize reject. But for
    // output-0 (iteration 0), the oversize check passed AND the
    // placeholder WAS inserted at :284 → owned_placeholders=[out0_hash].
    // Then iteration 1 bails at :246 → abort_batch cleans out0.
    let uploading: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests WHERE status = 'uploading'"
    ).fetch_one(&s.db.pool).await?;
    assert_eq!(uploading, 0, "output-0's placeholder cleaned up by abort_batch");

    Ok(())
}
```

**Check at dispatch:** `rio_store::cas::INLINE_THRESHOLD` may not be `pub` at the crate level — grep `pub const INLINE_THRESHOLD` in [`cas.rs:32`](../../rio-store/src/cas.rs). If it's `pub(crate)`, either re-export or hardcode `256 * 1024` with a comment pointing at the constant.

### T18 — `test(store):` PutPathBatch already_complete idempotency

NEW test in [`rio-store/tests/grpc/chunked.rs`](../../rio-store/tests/grpc/chunked.rs). [`put_path_batch.rs:256-259`/`:304-307`](../../rio-store/src/grpc/put_path_batch.rs) — if an output's `store_path_hash` already has a `'complete'` manifest, skip placeholder + commit for that output, `created[idx]=false`. No test pre-seeds a complete path and asserts the per-output `created` flag.

```rust
/// Idempotency: output-0 pre-completed, output-1 fresh → batch returns
/// created=[false, true]. Output-0 skipped entirely (no placeholder, no
/// commit — :258 continue before :268 insert). Output-1 committed normally.
// r[verify store.put.idempotent]
// r[verify store.atomic.multi-output]
#[tokio::test]
async fn gt13_batch_already_complete_per_output() -> TestResult {
    let s = StoreSession::new().await?;

    let out0_path = test_store_path("idem-out0");
    let (out0_nar, _) = make_nar(b"already here");
    let out0_info = make_path_info_for_nar(&out0_path, &out0_nar);

    let out1_path = test_store_path("idem-out1");
    let (out1_nar, _) = make_nar(b"fresh");
    let out1_info = make_path_info_for_nar(&out1_path, &out1_nar);

    // Pre-seed: output-0 already complete. Use a single-output PutPath
    // (the normal RPC, not batch) to get a 'complete' row in place.
    put_path_raw(&mut s.client.clone(), out0_info.clone().into(), out0_nar.clone()).await?;
    let complete_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests WHERE status = 'complete'"
    ).fetch_one(&s.db.pool).await?;
    assert_eq!(complete_before, 1, "precondition: out0 pre-completed");

    // Batch: send BOTH. Server should skip out0 (:258), commit out1.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, out0_info.into(), out0_nar).await;
    send_batch_output(&tx, 1, out1_info.into(), out1_nar).await;
    drop(tx);

    let mut client = s.client.clone();
    let resp = client.put_path_batch(ReceiverStream::new(rx)).await?.into_inner();
    assert_eq!(
        resp.created, vec![false, true],
        "out0 already_complete → created=false; out1 fresh → created=true"
    );

    // Both complete. Out0 unchanged (idempotent no-op), out1 new.
    let complete_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests WHERE status = 'complete'"
    ).fetch_one(&s.db.pool).await?;
    assert_eq!(complete_after, 2, "out1 committed; out0 unchanged");

    // No 'uploading' placeholders linger (out0's :258 continue happens
    // BEFORE :268 insert — no placeholder was ever created for it).
    let uploading: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM manifests WHERE status = 'uploading'"
    ).fetch_one(&s.db.pool).await?;
    assert_eq!(uploading, 0);

    Ok(())
}
```

**Check at dispatch:** `put_path_raw` helper shape — it's the single-output sender, should already be in `chunked.rs` or `core.rs`. If its signature differs (e.g. takes `ValidatedPathInfo` not `PathInfo`), adjust.

### T19 — `test(worker):` FailedPrecondition fallthrough to independent PutPath

NEW test in [`rio-worker/src/upload.rs`](../../rio-worker/src/upload.rs) tests module (near [`:1004`](../../rio-worker/src/upload.rs) `test_upload_all_outputs_multiple`). [`upload.rs:558-570`](../../rio-worker/src/upload.rs) — when `upload_outputs_batch` returns `UploadError::UploadExhausted { source }` with `source.code() == FailedPrecondition`, the match arm logs a warning and falls through to independent `PutPath` at [`:575+`](../../rio-worker/src/upload.rs). Zero test coverage — `MockStore` never emits `FailedPrecondition`.

Two approaches:

**Option A — extend MockStore with a `fail_batch_precondition` knob.** Add `pub fail_batch_precondition: Arc<AtomicBool>` at [`grpc.rs:48`](../../rio-test-support/src/grpc.rs) alongside `fail_next_puts`. In `put_path_batch` at [`:209`](../../rio-test-support/src/grpc.rs), before the existing `fail_next_puts` injection, check the flag and return `Status::failed_precondition("mock: oversize")`. Test:

```rust
/// upload_all_outputs fallthrough: batch returns FailedPrecondition
/// (oversize output for v1 inline-only handler) → falls through to
/// independent PutPath with a warning. Atomicity is LOST — that's the
/// documented pre-P0267 status quo per :562 comment. This test proves
/// the fallthrough is reachable and the outputs still land.
// r[verify worker.upload.multi-output]
#[tokio::test]
#[traced_test]
async fn test_upload_all_outputs_batch_fallthrough_on_precondition() -> anyhow::Result<()> {
    let (store, client, _h) = spawn_mock_store_with_client().await?;
    store.fail_batch_precondition.store(true, Ordering::SeqCst);

    let tmp = tempfile::tempdir()?;
    let store_dir = tmp.path().join("nix/store");
    fs::create_dir_all(&store_dir)?;
    let (b1, b2) = (test_store_basename("fall1"), test_store_basename("fall2"));
    fs::write(store_dir.join(&b1), b"one")?;
    fs::write(store_dir.join(&b2), b"two")?;

    let results = upload_all_outputs(&client, tmp.path(), "", "", &[]).await?;

    // Both outputs uploaded — via independent PutPath, not batch.
    assert_eq!(results.len(), 2);
    // MockStore's put_calls records BOTH paths (batch was never committed,
    // independent PutPath was). The mock's fail_batch_precondition fires
    // at the TOP of put_path_batch — zero outputs recorded via batch.
    assert_eq!(store.put_calls.read().unwrap().len(), 2);

    // The fallthrough warning fired. :565-569 warn! with "falling back
    // to independent PutPath".
    assert!(logs_contain("falling back to independent PutPath"));

    Ok(())
}
```

**Option B — real oversize output.** Write a >256 KiB file, let the REAL `put_path_batch` handler (if `StoreSession` exists in worker tests, which it doesn't — worker tests use `MockStore`) reject it. Doesn't apply — worker's `upload.rs` tests go through `MockStore`, which doesn't enforce INLINE_THRESHOLD.

**Recommend Option A.** Add `fail_batch_precondition: Arc<AtomicBool>` to [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) `MockStore` struct + `Default` + a check at the top of `put_path_batch` handler. One knob, one test.

**Subtlety — ensure fallthrough path PASSES.** If `fail_batch_precondition` is a simple bool, the fallthrough's independent `PutPath` calls will succeed (the knob only affects `put_path_batch`). If the knob were implemented via `fail_next_puts` (which it shouldn't — different code path), it would burn through the counter on the independent calls too. Keep them separate.

### T20 — `test(harness):` flake-mitigation happy-path (rc=0, single match)

NEW test in [`.claude/lib/test_scripts.py`](../../.claude/lib/test_scripts.py) near the existing flake tests (T9 adds two at ~`:1063`). [P0322](plan-0322-flake-mitigation-dup-key-guard.md) refactored `next() → matches[0]` at [`cli.py:399-406`](../../.claude/lib/onibus/cli.py) (behaviorally equivalent for `len==1`) and tested only the new `rc=2` ambiguous path. The pre-existing `rc=0` happy-path (single match → mitigation appended, header preserved) has no test — a 10-line regression guard:

```python
def test_flake_mitigation_happy_path_single_match(tmp_path):
    """rc=0: single match → mitigation appended, header preserved,
    exactly one row modified. Regression guard for the next()→matches[0]
    refactor — equivalent for len==1, this proves it."""
    kf = tmp_path / "known-flakes.jsonl"
    kf.write_text(
        "# header line — preserved\n"
        '{"test":"vm-foo","symptom":"bar","drv_name":"rio-foo","mitigations":[],"retry":"Once"}\n'
    )
    rc = _run_cli("flake", "mitigation", "vm-foo", "m1-desc", "--sha", "abc12345", kf=kf)
    assert rc == 0
    lines = kf.read_text().splitlines()
    assert lines[0].startswith("# header")  # header preserved
    row = json.loads(lines[1])
    assert len(row["mitigations"]) == 1
    assert row["mitigations"][0]["description"] == "m1-desc"
```

**Check at dispatch:** exact CLI invocation signature (`flake mitigation` vs `flake-mitigation`, arg order) — match T9's test shape.

### T21 — `test(store):` PutChunk fail-closed no-tenant

NEW test in [`rio-store/tests/grpc/chunk_service.rs`](../../rio-store/tests/grpc/chunk_service.rs). [P0264](plan-0264-chunk-tenant-isolation.md) adds `require_tenant` at [`grpc/chunk.rs:131`](../../rio-store/src/grpc/chunk.rs) (PutChunk) AND [`:392`](../../rio-store/src/grpc/chunk.rs) (FindMissingChunks) — only the latter has `test_find_missing_chunks_no_tenant_fail_closed`. Asymmetric. PutChunk without `x-test-tenant-id` should UNAUTHENTICATED before any stream consumption. Copy the fail-closed test shape:

```rust
/// PutChunk without x-test-tenant-id → Unauthenticated before stream
/// consumption. Same fail-closed contract as FindMissingChunks; both
/// call require_tenant at handler entry. r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn test_put_chunk_no_tenant_fail_closed() -> TestResult {
    let s = StoreSession::new().await?;  // no tenant header
    let (tx, rx) = mpsc::channel(4);
    // Don't even need to send anything — the check fires before
    // stream.message(). But send one chunk to prove it wasn't read.
    tx.send(PutChunkRequest { data: vec![0u8; 100] }).await?;
    drop(tx);
    let r = s.client.clone().put_chunk(ReceiverStream::new(rx)).await;
    let status = r.expect_err("must fail-closed without tenant");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert!(status.message().contains("PutChunk is tenant-scoped"));
    Ok(())
}
```

**p264 worktree ref — re-grep at dispatch.** The `require_tenant` calls arrive with P0264. Test goes near the existing `test_find_missing_chunks_no_tenant_fail_closed` (p264 ref `:751`).

### T22 — `test(scheduler):` Progress arm grpc wiring — counter fires

NEW test in [`rio-scheduler/src/grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs). [P0266](plan-0266-proactive-ema-from-worker-progress.md) adds a Progress arm at [`grpc/mod.rs:846-878`](../../rio-scheduler/src/grpc/mod.rs) — `if let Some(pool)+resources` guard, `memory_used_bytes > 0` filter, `tokio::spawn`, counter increment. The db function IS well-tested; the grpc glue layer has zero coverage. Intentional per the code comment (no pool in `new_for_tests`), but the counter-fire path is measurable without a real pool:

```rust
/// Progress arm: rio_scheduler_ema_proactive_updates_total fires when
/// a worker reports memory_used_bytes > 0. The db.rs proactive_update
/// IS tested; this proves the grpc wiring (guard, filter, spawn) fires.
/// Uses CountingRecorder not a real PG — the spawn'd task's db call
/// errors in the test harness (no pool), but the counter increments
/// BEFORE the db call. r[verify obs.metric.scheduler]
#[tokio::test]
async fn test_progress_arm_ema_counter_fires() -> TestResult {
    // Set up CountingRecorder + scheduler grpc server with a dangling
    // pool (the db call inside the spawn will error; the counter fires
    // before it). memory_used_bytes > 0 is the filter.
    // …assert recorder.get("rio_scheduler_ema_proactive_updates_total") == 1
    // AFTER the tokio::spawn'd task has had a chance to run (yield_now or
    // sleep(1ms)).
    unimplemented!("p266 worktree — handler shape determines test scaffolding")
}
```

**p266 worktree ref — the `:846-878` handler shape drives the test.** May need a `with_pool_for_tests` builder if the `if let Some(pool)` guard makes the arm unreachable without a real pool. If so, document as "counter fires only with a pool; test validates the guard-then-increment ordering with a poisoned pool that errors on connect but lets the counter emit first."

### T23 — `test(store):` PutPathBatch tenant-id extraction → maybe_sign

NEW test in [`rio-store/tests/grpc/signing.rs`](../../rio-store/tests/grpc/signing.rs) (or new `chunked_signing.rs`). [P0338](plan-0338-tenant-signer-wiring-putpath.md) adds tenant-id extraction at [`put_path_batch.rs:67-70`](../../rio-store/src/grpc/put_path_batch.rs) → `maybe_sign` at [`:320`](../../rio-store/src/grpc/put_path_batch.rs). `signing.rs` tests cover PutPath single-path only; `chunked.rs` batch tests have zero tenant/JWT/signing asserts. The extraction block is DUPLICATED (not shared) — a regression in the batch version ships silently:

```rust
/// PutPathBatch tenant extraction → tenant-key signing. 2 outputs,
/// both signed under the tenant key, neither under cluster. Proves
/// the :67-70 extraction (duplicated from PutPath, not shared) works.
// r[verify store.tenant.sign-key]
#[tokio::test]
async fn batch_outputs_signed_with_tenant_key() -> TestResult {
    let s = StoreSessionBuilder::new()
        .with_fake_jwt_tenant("tenant-bat")
        .with_tenant_key("tenant-bat", "bat-key-1", &TENANT_SEED)
        .build()
        .await?;

    // 2-output batch.
    let (tx, rx) = mpsc::channel(16);
    send_batch_output(&tx, 0, make_info("bat-0"), make_nar(b"zero").0).await;
    send_batch_output(&tx, 1, make_info("bat-1"), make_nar(b"one").0).await;
    drop(tx);
    s.client.clone().put_path_batch(ReceiverStream::new(rx)).await?;

    // Both sigs verify under tenant key, neither under cluster.
    for path in &["bat-0", "bat-1"] {
        let info = query_path_info(&s, path).await?;
        assert!(verify_sig(&info, "bat-key-1", &TENANT_SEED),
            "{path}: expected tenant-key sig");
        assert!(!verify_sig(&info, &s.cluster_key_name, &s.cluster_seed),
            "{path}: should NOT have cluster-key sig");
    }
    Ok(())
}
```

**p338 worktree ref — `:67-70` extraction + `:320` maybe_sign arrive with P0338.** Test shape mirrors the existing single-path `tests/grpc/signing.rs` PutPath tenant-sign test.

### T24 — `test(gateway):` resolve_and_mint graceful-degrade path — use MockScheduler.resolve_tenant

[P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) added `MockScheduler.resolve_tenant_uuid` + `resolve_tenant_calls` fields at [`grpc.rs:574,580`](../../rio-test-support/src/grpc.rs) (p260 refs) + `resolve_tenant` impl at `:808` — doc says "for testing gateway graceful-degrade path". But ZERO rio-gateway tests use them. `resolve_and_mint` at [`server.rs:433`](../../rio-gateway/src/server.rs), `with_jwt_signing_key` at `:210`, and `auth_publickey` JWT branches at `:603-642` (success, required-reject, degrade) have NO gateway-side integration test. VM test (security.nix) covers ONLY `signing_key=None` fallback.

NEW tests in [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) `#[cfg(test)]` (or `rio-gateway/tests/` integration) — one per branch:

```rust
/// auth_publickey with signing_key=Some + scheduler resolve succeeds →
/// JWT minted, session carries Claims. Uses MockScheduler.resolve_tenant_uuid.
// r[verify gw.jwt.issue]
#[tokio::test]
async fn auth_publickey_mints_jwt_on_resolve_success() { /* ... */ }

/// signing_key=Some + resolve returns UNAUTHENTICATED → reject per r[gw.jwt.dual-mode].
/// Proves the "required" mode rejects unknown pubkeys.
// r[verify gw.jwt.dual-mode]
#[tokio::test]
async fn auth_publickey_rejects_on_resolve_unauthenticated() { /* ... */ }

/// signing_key=Some + resolve returns UNAVAILABLE (scheduler down) →
/// degrade to fallback (session proceeds, no Claims). Proves graceful-degrade.
// r[verify gw.jwt.dual-mode]
#[tokio::test]
async fn auth_publickey_degrades_on_resolve_unavailable() { /* ... */ }
```

Wire `MockScheduler.resolve_tenant_uuid` to return the test-controlled response. Assert `resolve_tenant_calls` incremented.

**p260 worktree refs — re-grep at dispatch.** The MockScheduler fields + server.rs JWT branches arrive with P0260.

### T25 — `test(controller):` reconcile_ephemeral spawn_count arithmetic

[P0296](plan-0296-ephemeral-builders-opt-in.md) reconciler spawn-decision at [`ephemeral.rs:176`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) (p296 ref): `headroom = ceiling.saturating_sub(active).max(0); to_spawn = queued.min(headroom)`. No unit test — only VM-tested. The i32/u32 cast dance + `saturating_sub` + `try_into().unwrap_or(i32::MAX)` has edge cases (active>ceiling, negative ceiling from pre-CEL CRD). Autoscaler `compute_desired` has 5 unit tests for the same math class.

Extract `spawn_count(queued: u32, active: u32, ceiling: u32) -> u32` as a free fn in [`ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs), add table-driven tests:

```rust
// r[verify ctrl.pool.ephemeral]
#[test]
fn spawn_count_table() {
    // (queued, active, ceiling, expected)
    let cases = [
        (10, 0, 5, 5),    // queue > ceiling → cap at headroom
        (2, 0, 5, 2),     // queue < headroom → spawn queued
        (10, 5, 5, 0),    // active == ceiling → no headroom
        (10, 7, 5, 0),    // active > ceiling → saturating_sub = 0
        (0, 0, 5, 0),     // empty queue → 0
        (u32::MAX, 0, 5, 5),  // overflow guard
    ];
    for (q, a, c, exp) in cases {
        assert_eq!(spawn_count(q, a, c), exp, "queued={q} active={a} ceiling={c}");
    }
}
```

**p296 worktree ref — re-grep at dispatch.** The `:176` arithmetic arrives with P0296.

### T26 — `test(store):` maybe_sign fallback-on-TenantKeyLookup arm

[P0338](plan-0338-tenant-signer-wiring-putpath.md) added the fallback at [`grpc/mod.rs:323-339`](../../rio-store/src/grpc/mod.rs): when `sign_for_tenant(Some(tid), fp)` returns `Err` (`TenantKeyLookup` from PG failure or corrupt seed via `MetadataError::InvariantViolation`), handler warns + falls back to `sign_for_tenant(None, fp).expect("infallible")`. `sign_for_tenant` has 3 happy-path tests at [`signing.rs:510-607`](../../rio-store/src/signing.rs) (with-key, no-key-fallback, `None`-no-DB-hit); the `Err` arm at `:231` is covered downstream by `get_active_signer` `InvariantViolation` test at [`tenant_keys.rs:222`](../../rio-store/src/metadata/tenant_keys.rs). But the **fallback wiring** in `maybe_sign` — does the warn fire, does the cluster-key sig land, does the upload proceed — is untested.

If `get_active_signer` starts returning new error variants, or if the `.expect()` line's assumption rots, nothing catches it.

NEW test in [`rio-store/tests/grpc/signing.rs`](../../rio-store/tests/grpc/signing.rs) after `put_path_with_tenant_jwt_signs_with_tenant_key` (~`:350`):

```rust
// r[verify store.tenant.sign-key]
/// maybe_sign fallback: tenant has a CORRUPT seed (16 bytes, not 32)
/// → get_active_signer returns InvariantViolation → sign_for_tenant
/// returns Err(TenantKeyLookup) → maybe_sign warns + falls back to
/// cluster key → upload still succeeds.
///
/// This tests the FALLBACK WIRING, not the error detection (that's
/// tenant_keys.rs:222 bad_seed_length_is_invariant_violation).
/// Mutation check: if maybe_sign's Err arm fails the upload instead
/// of falling back, this test catches it (put_path returns Err).
#[tokio::test]
async fn put_path_corrupt_tenant_seed_falls_back_to_cluster() -> TestResult {
    use base64::Engine;
    use ed25519_dalek::{Signature, SigningKey, Verifier};

    let db = TestDb::new(rio_store::MIGRATOR).await;
    let cluster = cluster_signer();  // existing test fixture
    let cluster_pk = cluster.verifying_key();

    // Seed tenant with 16-byte CORRUPT seed (reuse tenant_keys.rs:227
    // pattern). BYTEA has no length constraint → insert succeeds,
    // ed25519 parse fails → InvariantViolation on lookup.
    let tid = seed_tenant(&db.pool, "fallback-corrupt").await;
    sqlx::query(
        "INSERT INTO tenant_keys (tenant_id, key_name, seed) VALUES ($1, $2, $3)",
    )
    .bind(tid)
    .bind("tenant-corrupt-fallback-1")
    .bind(&[0x55u8; 16][..])  // 16 bytes, not 32
    .execute(&db.pool)
    .await?;

    let ts = TenantSigner::new(cluster.clone(), db.pool.clone());
    let service = StoreServiceImpl::new(db.pool.clone()).with_signer(ts);
    let (mut client, _server) = spawn_store_with_fake_jwt(service, tid).await?;

    // PutPath with JWT Claims.sub = tid (corrupt-seed tenant).
    let (path, nar, info) = make_test_path_with_nar();
    let resp = put_path(&mut client, &path, &nar, &info).await?;
    assert!(resp.created, "upload MUST proceed despite tenant-key lookup failure");

    // Signature verifies under CLUSTER key (fallback), NOT tenant key.
    let narinfo = query_path_info(&mut client, &path).await?;
    let sig_str = &narinfo.signatures[0];
    let (_name, sig_b64) = sig_str.split_once(':').expect("sig format");
    let sig = Signature::from_slice(
        &base64::engine::general_purpose::STANDARD.decode(sig_b64)?,
    )?;
    let fp = fingerprint_for(&narinfo);  // existing test helper
    assert!(
        cluster_pk.verify(fp.as_bytes(), &sig).is_ok(),
        "signature must verify under cluster pubkey (fallback fired)"
    );

    Ok(())
}
```

Optionally: capture the `warn!` line via `tracing_subscriber::fmt::test_writer` if the test harness supports it; asserting the log proves the "warn loud" intent held. Not gating — the cluster-sig verify is the load-bearing assert.

**Interaction with [P0352](plan-0352-putpathbatch-hoist-signer-lookup.md):** T26 tests the Err arm of `maybe_sign`'s `sign_for_tenant` call. P0352-T3 rewrites `maybe_sign` via `resolve_once` — the Err arm moves but the fallback semantics are preserved (warn + cluster). T26's assertions (upload succeeds + cluster sig verifies) hold before and after. If P0352 lands first, T26's "corrupt seed → warn + cluster fallback → upload OK" still tests the same chain, just via `resolve_once` instead of `sign_for_tenant`. Sequence-independent. discovered_from=bughunter(mc98).

### T27 — `test(vm):` conn_cap VM subtest — 3rd connection gets disconnect

`r[gw.conn.cap]` at [`gateway.md:697`](../../docs/src/components/gateway.md) has unit tests for the tokio Semaphore primitive ONLY. The actual `new_client → conn_permit:None → ensure_permit → auth-Err → russh-disconnect` path is not integration-tested. Rate-limit got a VM subtest in security.nix; conn_cap did not.

NEW subtest in [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) (alongside the rate-limit subtest):

```python
# r[verify gw.conn.cap]
with subtest("conn_cap: 3rd connection at cap=2 gets disconnect"):
    # Set gateway max_connections=2 via drop-in. The gateway.toml loader
    # picks up RIO_MAX_CONNECTIONS or a config drop-in — check which at
    # dispatch (P0213's cfg mechanism).
    gateway.succeed(
        "mkdir -p /etc/rio-gateway.d && "
        "echo 'max_connections = 2' > /etc/rio-gateway.d/99-test-cap.toml"
    )
    gateway.systemctl("restart rio-gateway")
    gateway.wait_for_open_port(2022)

    # Open 2 long-lived SSH sessions (sleep 30 in nix-shell to hold).
    for i in range(2):
        client.succeed(
            f"ssh -o StrictHostKeyChecking=no -p 2022 builder@gateway "
            f"'sleep 30 &' &"
        )

    # 3rd connection attempt — must get TooManyConnections disconnect
    # BEFORE the session spawns (permit acquired in accept loop).
    out = client.fail(
        "ssh -o StrictHostKeyChecking=no -p 2022 builder@gateway 'echo hi'"
    )
    assert "too many" in out.lower() or "connection cap" in out.lower(), \
        f"expected cap-disconnect message, got: {out}"

    # Clean up: kill the sleepers, restore config.
    client.succeed("pkill -f 'ssh.*gateway.*sleep' || true")
    gateway.succeed("rm /etc/rio-gateway.d/99-test-cap.toml")
    gateway.systemctl("restart rio-gateway")
```

**Impl note:** the exact mechanism to set `max_connections=2` depends on P0213's config layer — drop-in toml, env var `RIO_MAX_CONNECTIONS`, or helm values override. Check at dispatch. Post-P0213-merge. discovered_from=213.

### T28 — `test(gateway):` resolve_and_mint error paths — extends T24 with timeout + unparseable + counters

T24 covers resolve-success (mint), resolve-UNAUTHENTICATED (reject), resolve-UNAVAILABLE (degrade). Bughunter (mc98-105) found gaps at [`rio-gateway/src/server.rs:433-481,606-642`](../../rio-gateway/src/server.rs):

- **Timeout path** at `:452` (`tokio::time::timeout` on the ResolveTenant RPC) — distinct from UNAVAILABLE Status; scheduler slow, not down
- **Unparseable tenant_id** at `:472-477` — ResolveTenant returns a garbage string, not a valid UUID
- **Counter coverage:** `rejected_jwt` at `:625` and `mint_degraded_total` at `:640` have zero test assertions

NEW tests in [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) `#[cfg(test)]` (alongside T24's 3 tests):

```rust
/// ResolveTenant RPC times out → same treatment as UNAVAILABLE
/// (degrade when required=false; reject when required=true).
/// Uses a MockScheduler that sleeps past the timeout.
// r[verify gw.jwt.dual-mode]
#[tokio::test(start_paused = true)]
async fn auth_publickey_resolve_timeout_degrades() { /* ... */ }

/// ResolveTenant returns garbage (not a UUID) → parse fails at
/// :472-477 → treated as resolve-failure (reject/degrade per mode).
// r[verify gw.jwt.dual-mode]
#[tokio::test]
async fn auth_publickey_unparseable_tenant_id_degrades() { /* ... */ }

/// rejected_jwt counter fires on required=true + resolve failure.
/// Uses CountingRecorder to capture metric emissions.
// r[verify obs.metric.gateway]
#[tokio::test]
async fn auth_publickey_rejected_jwt_counter_fires() { /* ... */ }

/// mint_degraded_total counter fires on required=false + resolve
/// failure. Proves the degrade path is observable.
// r[verify obs.metric.gateway]
#[tokio::test]
async fn auth_publickey_mint_degraded_counter_fires() { /* ... */ }
```

**security.nix comment correction:** the comment at `:11` says "ISSUE-side of dual-mode is proven by the rust tests" — after T24+T28 that's accurate; before, it overclaimed. If T24 lands without T28, amend the comment to "proven by rust tests (happy path + 3 error paths)" for precision. discovered_from=bughunter(mc98-105). Post-P0260-merge (same as T24).

### T29 — `test(common):` load_and_wire_jwt — no test for extracted helper

[P0355](plan-0355-extract-drain-jwt-load-helpers.md) (DONE) extracted `load_and_wire_jwt` to [`rio-common/src/jwt_interceptor.rs:190`](../../rio-common/src/jwt_interceptor.rs) from two main.rs sites but only tested `spawn_drain_task` (P0355-T3). The jwt helper has no unit test. Both production callers ([`rio-scheduler/src/main.rs:645`](../../rio-scheduler/src/main.rs), [`rio-store/src/main.rs:467`](../../rio-store/src/main.rs)) are main.rs-wiring only tested via VM integration — no fast-feedback regression guard.

NEW test in [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) `#[cfg(test)]` mod (near the existing `load_jwt_pubkey_from_file` test at ~`:542`):

```rust
/// load_and_wire_jwt: key_path=Some → load + Arc<RwLock>(Some(key)) +
/// spawn_pubkey_reload task active. Key-refresh on SIGHUP is covered
/// by sighup_swaps_pubkey; THIS test proves the one-shot boot path.
// r[verify gw.jwt.dual-mode]
#[tokio::test]
async fn load_and_wire_jwt_some_path_loads_and_spawns() {
    let (path, pk) = encode_pubkey_file(&ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng).verifying_key());
    let shutdown = CancellationToken::new();
    let slot = load_and_wire_jwt(Some(&path), shutdown.clone()).unwrap();
    // Slot populated with the key.
    assert_eq!(*slot.read().await, Some(pk));
    // Reload task spawned — prove it via shutdown + join cleanliness
    // (no panic on drop). The task's SIGHUP-handling is tested by
    // sighup_swaps_pubkey; here we only prove it RUNS.
    shutdown.cancel();
    tokio::task::yield_now().await;
}

/// load_and_wire_jwt: key_path=None → Arc<RwLock>(None), no reload
/// spawned. The "JWT pubkey not configured; auth disabled" log path.
// r[verify gw.jwt.dual-mode]
#[tokio::test]
async fn load_and_wire_jwt_none_path_returns_inert() {
    let shutdown = CancellationToken::new();
    let slot = load_and_wire_jwt(None, shutdown).unwrap();
    assert_eq!(*slot.read().await, None);
}
```

**CHECK AT DISPATCH:** the exact `encode_pubkey_file` signature (P0349 landed it at `:532` per P0304-T69; may return `(PathBuf, VerifyingKey)` or `(NamedTempFile, Vec<u8>)` — adapt the assert accordingly). Marker is `r[gw.jwt.dual-mode]` — `load_and_wire_jwt` already carries `// r[impl gw.jwt.dual-mode]` at [`jwt_interceptor.rs:189`](../../rio-common/src/jwt_interceptor.rs) (the None→inert/Some→active split IS the dual-mode mechanism). discovered_from=355-review.

### T30 — `test(vm):` passthrough marker-that-lies — #[ignore] stub doesn't verify passthrough works

`r[worker.fuse.passthrough]` at [`worker.md:366`](../../docs/src/components/worker.md) has `r[impl]` at [`fuse/mod.rs:8`](../../rio-worker/src/fuse/mod.rs) and `r[verify]` at `:286` — but the verify is an `#[ignore]`'d stub: "The stub verifies ... `passthrough_failures` initializes to 0. Trivial, but it anchors the tracey verify annotation." The stub's own docstring admits "Full verify (mount + open cycle + assert counter stays 0) deferred to VM test."

[`scheduling.nix:220-224`](../../nix/tests/scenarios/scheduling.nix) tests the NEGATIVE (passthrough OFF → `fallback_reads_total ≥ 1` on wsmall2) in `sizeclass` subtest. No test proves the POSITIVE (passthrough ON → reads bypass userspace → `fallback_reads_total` stays 0 on wsmall1). Bughunter-mc119 found this marker-that-lies pattern: tracey says "tested" because the annotation EXISTS; the test never runs (#[ignore]) and only checks a struct-field default.

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — extend the existing `sizeclass` subtest's passthrough check at `:220-225` with the POSITIVE assert:

```python
          # wsmall1 has passthrough ON (default). fallback_reads_total
          # should be ZERO or near-zero after sizeclass builds —
          # open_backing() succeeded, kernel handles reads directly.
          # Nonzero under passthrough ON = open_backing() silently
          # failing for some files (investigate). Tolerance: ≤2 (boot-
          # time before mount settles may fall back; the steady-state
          # post-build count is what matters).
          # r[verify worker.fuse.passthrough]  — MOVE to col-0 header
          assert_metric_le(wsmall1, 9093,
              "rio_worker_fuse_fallback_reads_total", 2,
              msg="passthrough ON should bypass userspace reads")
```

**Add `# r[verify worker.fuse.passthrough]`** to the col-0 header block at `:23-52` (where other scenario-level markers live, per [P0341](plan-0341-tracey-verify-reachability-convention.md) convention). MOVE the marker from the `#[ignore]`'d Rust stub to default.nix's `sizeclass` subtest entry per the marker-at-subtests-entry convention. Delete or keep the stub — if kept, strip its `r[verify]` annotation (the VM test IS the verify now; the stub is boot-check-only, no spec claim).

**CHECK AT DISPATCH:** `assert_metric_le` may not exist in the common helpers — only `assert_metric_ge` at `:225` is visible. If absent, add it to [`nix/tests/lib/common.nix`](../../nix/tests/lib/common.nix) or use inline `scrape_metrics` + `metric_value` + `assert val <= 2`. discovered_from=bughunter(mc119).

### T31 — `test(store):` tenant_quota_by_name unit test — new fn, no test

[P0255](plan-0255-quota-reject-submitbuild.md) (DONE) added [`tenant_quota_by_name`](../../rio-store/src/gc/tenant.rs) at `:51-67` — wraps name→tenant_id resolution + `tenant_store_bytes` into one call for the `TenantQuota` RPC. The function has zero unit tests; the only coverage is indirect via the gateway's VM security.nix quota-exceeded subtest ([`b9e90d9a`](https://github.com/search?q=b9e90d9a&type=commits)).

NEW test in [`rio-store/src/gc/tenant.rs`](../../rio-store/src/gc/tenant.rs) `#[cfg(test)]` mod (or alongside `tenant_store_bytes` test wherever it lives — grep `fn test.*tenant_store_bytes` at dispatch):

```rust
/// tenant_quota_by_name: known tenant with limit → Some((used, Some(limit))).
/// Three cases: (1) unknown name → None; (2) known, NULL limit → Some((used, None));
/// (3) known with limit → Some((used, Some(limit))). Prove the name→id→usage
/// chain works end-to-end.
// r[verify store.gc.tenant-quota-enforce]
#[tokio::test]
async fn tenant_quota_by_name_cases() {
    let db = rio_test_support::TestDb::new().await;
    // Case 1: unknown.
    assert_eq!(tenant_quota_by_name(&db.pool, "ghost-tenant").await.unwrap(), None);

    // Seed a tenant with limit.
    let tid: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO tenants (tenant_name, gc_max_store_bytes) VALUES ($1, $2) RETURNING tenant_id"
    ).bind("quota-test-seeded").bind(Some(4096i64)).fetch_one(&db.pool).await.unwrap();

    // Case 3: known + limit, zero usage (no path_tenants rows yet).
    let q = tenant_quota_by_name(&db.pool, "quota-test-seeded").await.unwrap();
    assert_eq!(q, Some((0, Some(4096))));

    // Seed 2 paths attributed to this tenant (128 bytes each say).
    // ... (reuse seed_path helper from mark.rs if visible; else inline)
    // Re-query → used reflects attribution.
    let q = tenant_quota_by_name(&db.pool, "quota-test-seeded").await.unwrap();
    assert!(matches!(q, Some((used, Some(4096))) if used > 0));

    // Case 2: NULL limit → Some((used, None)).
    sqlx::query("UPDATE tenants SET gc_max_store_bytes = NULL WHERE tenant_id = $1")
        .bind(tid).execute(&db.pool).await.unwrap();
    let q = tenant_quota_by_name(&db.pool, "quota-test-seeded").await.unwrap();
    assert!(matches!(q, Some((_, None))));
}
```

**CHECK AT DISPATCH:** the existing `tenant_store_bytes` test at [`gc/mark.rs:572`](../../rio-store/src/gc/mark.rs) uses `seed_path` helper — reuse if `pub(crate)`. The `r[verify store.gc.tenant-quota-enforce]` marker already has one verify (gateway quota gate unit tests at [`b701c13a`](https://github.com/search?q=b701c13a&type=commits)); this adds a second at the store-side backing function. discovered_from=255.

### T32 — `test(vm):` jwt-mount-present subtest — KVM execution deferred

The [P0357](plan-0357-helm-jwt-pubkey-mount.md) `jwt-mount-present` VM subtest at [`lifecycle.nix:416-532`](../../nix/tests/scenarios/lifecycle.nix) never built (drv `wm2wmwssi1`, KVM-DENIED builder roulette). The helm-lint yq assertions at [`flake.nix:498-575`](../../flake.nix) prove the mount **renders** in the Helm template; the VM subtest proves it **works at runtime** (pod Running → ConfigMap mounted → `load_and_wire_jwt` succeeded via `?` fail-fast).

**REMINDER TASK** — same shape as T10/T13-T15. Add to [`kvm-pending.md`](../notes/kvm-pending.md) (T16 creates it):

```markdown
- vm-lifecycle-jwt-mount-present (P0357) — jwt-mount-present subtest
  :416-532. helm-lint proves template-renders; VM proves
  pod-Running→load_and_wire_jwt-succeeded. drv wm2wmwssi1 never built
  (KVM-DENIED roulette). Gated on r[sec.jwt.pubkey-mount].
```

When a KVM-capable builder slot opens, `/nixbuild .#checks.x86_64-linux.vm-lifecycle-k3s` (or whichever attr exposes the jwt-mount subtest) and check the subtest passes. The `r[verify sec.jwt.pubkey-mount]` marker at [`default.nix`](../../nix/tests/default.nix) subtests entry (per P0341 convention) is valid but unexecuted.

**Stale-cite note:** [P0295](plan-0295-doc-rot-batch-sweep.md)-T53 fixes the `main.rs:676 load_jwt_pubkey.await?` triple-stale reference in the subtest comment at `:441` — land that first so the VM-run doesn't cite wrong fn/line. discovered_from=coordinator (P0357 merger log).

### T33 — `test(worker,controller,store,gateway):` assert_histograms_have_buckets — 4-crate sweep

[P0321](plan-0321-build-graph-edges-histogram-buckets.md) landed [`assert_histograms_have_buckets`](../../rio-test-support/src/metrics.rs) at [`metrics.rs:328`](../../rio-test-support/src/metrics.rs) but only wired it in [`rio-scheduler/tests/metrics_registered.rs:101`](../../rio-scheduler/tests/metrics_registered.rs). The plan doc at `:133` made the sweep optional ("Implementer's call"). Post-hoc: **worker has a live bug** (`rio_worker_upload_references_count` — count-type, no bucket entry, every >10-ref sample in `+Inf`). That's [P0363](plan-0363-upload-references-count-buckets.md), which wires the worker test specifically.

This task sweeps the remaining three crates (controller/store/gateway) + worker if P0363 hasn't dispatched. Add `all_histograms_have_bucket_config` to each crate's `tests/metrics_registered.rs`:

```rust
// r[verify obs.metric.<crate>]
#[test]
fn all_histograms_have_bucket_config() {
    use rio_common::observability::HISTOGRAM_BUCKET_MAP;
    const DEFAULT_BUCKETS_OK: &[&str] = &[/* per-crate exemptions */];
    assert_histograms_have_buckets(
        rio_<crate>::describe_metrics,
        HISTOGRAM_BUCKET_MAP,
        DEFAULT_BUCKETS_OK,
        "rio-<crate>",
    );
}
```

Per-crate `DEFAULT_BUCKETS_OK` at dispatch:
- **controller**: `rio_controller_reconcile_duration_seconds` is in the map → empty exempt list
- **store**: `rio_store_put_path_duration_seconds` — sub-second PutPath latency, default fits → exempt
- **gateway**: `rio_gateway_opcode_duration_seconds` — sub-second opcode handler, default fits → exempt (spec at [`:206`](../../docs/src/observability.md) already documents this)
- **worker**: `rio_worker_fuse_fetch_duration_seconds` — sub-second fetch, default fits → exempt (P0363-T2 handles this if it dispatched first)

The test should fail CI if a future `describe_histogram!` ships without a `HISTOGRAM_BUCKET_MAP` entry and isn't exempted. Structural guard value: P0321 proved the describe-only test doesn't catch this; P0363 proved it happens twice.

**Dispatch-time status (rev-p363 confirm):** scheduler + worker are wired (2/5 — [`rio-scheduler/tests/metrics_registered.rs:108`](../../rio-scheduler/tests/metrics_registered.rs), [`rio-worker/tests/metrics_registered.rs:63`](../../rio-worker/tests/metrics_registered.rs)). T33 sweeps the remaining 3: controller/store/gateway. Worker's wiring is [P0363](plan-0363-upload-references-count-buckets.md)-T2 (merging now) — skip the worker branch if [`rio-worker/tests/metrics_registered.rs`](../../rio-worker/tests/metrics_registered.rs) already has `all_histograms_have_bucket_config` at dispatch.

discovered_from=321-review.

### T34 — `test(store):` batch_outputs_signed_with_tenant_key — PutPathBatch post-P0352 hoist

MODIFY [`rio-store/tests/grpc/signing.rs`](../../rio-store/tests/grpc/signing.rs) — [P0352](plan-0352-putpathbatch-hoist-signer-lookup.md) hoisted the tenant-signer lookup out of the per-output loop (`resolve_once` called once before the tx at [`put_path_batch.rs:327`](../../rio-store/src/grpc/put_path_batch.rs), `sign_with_resolved` per-output). T23 covers the pre-hoist shape; this extends with a direct-integration assertion that **all** outputs in a batch carry the tenant key signature when the tenant has an active key.

```rust
// r[verify store.tenant.sign-key]
#[tokio::test]
async fn batch_outputs_signed_with_tenant_key_post_hoist() {
    // Arrange: tenant WITH active key, 3-output batch
    let (pool, tid) = seed_tenant_with_key().await;
    let store = spawn_store_with_pg(pool).await;
    let paths = three_output_batch_fixture();

    // Act: PutPathBatch
    let resp = store.put_path_batch(batch_request(tid, &paths)).await.unwrap();

    // Assert: ALL three narinfo signatures verify with the TENANT key
    // (not cluster — was_tenant=true via resolve_once's single lookup)
    for narinfo in &resp.narinfos {
        assert!(verify_with_tenant_key(&narinfo.sig, tid),
            "output {} signed with cluster key — resolve_once hoist didn't propagate", narinfo.path);
    }
    // Regression: the resolve_once is called exactly once (not N times inside tx)
    // — prove via PG query-log hook OR mock-signer call-count. Skip if impractical;
    // the sig-verify is the observable contract.
}
```

Re-use T23's `seed_tenant_with_key` + `verify_with_tenant_key` helpers if they exist; otherwise inline. T23 and T34 are adjacent tests in the same file — T34 specifically targets the post-P0352 `resolve_once`-once-not-N-times shape. discovered_from=352-review.

### T35 — `test(store,gateway):` opcode/put_path duration histograms may exceed 10s — exempt-list false

**Refines T33.** T33's `DEFAULT_BUCKETS_OK` exemption list at [`:1194-1195`](#t33--testworkercontrollerstoreassert_histograms_have_buckets--4-crate-sweep) says `rio_store_put_path_duration_seconds` and `rio_gateway_opcode_duration_seconds` fit the `[0.005..10.0]` default buckets ("sub-second PutPath latency", "sub-second opcode handler"). Consolidator (mc135) disagrees: a `wopAddToStoreNar` with a multi-GB NAR takes well over 10 seconds (network transfer + hash + PG write); `PutPath` for the same payload likewise. Any sample >10s lands in `+Inf` — the same silent-useless-histogram bug P0321 found for `build_graph_edges`.

**Check at dispatch:** grep `describe_histogram` in [`rio-store/src/lib.rs`](../../rio-store/src/lib.rs) and [`rio-gateway/src/lib.rs`](../../rio-gateway/src/lib.rs) for the exact metric names, then check if [`HISTOGRAM_BUCKET_MAP`](../../rio-common/src/observability.rs) at `:302` has entries. Currently (per consol-mc135 finding) it does NOT — both are exempt-by-default, silently truncated at 10s.

MODIFY [`rio-common/src/observability.rs`](../../rio-common/src/observability.rs) at `HISTOGRAM_BUCKET_MAP` — add two entries:

```rust
// Store PutPath and gateway opcode latencies span sub-second (cache
// hits, small NARs) to minutes (multi-GB NAR uploads over wopAddTo-
// StoreNar). Default [0.005..10.0] buckets truncate at 10s — every
// large-NAR sample lands in +Inf. Use the same BUILD_DURATION_BUCKETS
// ladder but without the hour-scale tail (NAR uploads don't run hours).
const NAR_LATENCY_BUCKETS: &[f64] = &[0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0];

pub const HISTOGRAM_BUCKET_MAP: &[(&str, &[f64])] = &[
    // ... existing 7 entries ...
    ("rio_store_put_path_duration_seconds", NAR_LATENCY_BUCKETS),
    ("rio_gateway_opcode_duration_seconds", NAR_LATENCY_BUCKETS),
];
```

Then REMOVE the two metrics from T33's `DEFAULT_BUCKETS_OK` exempt lists in [`rio-store/tests/metrics_registered.rs`](../../rio-store/tests/metrics_registered.rs) and [`rio-gateway/tests/metrics_registered.rs`](../../rio-gateway/tests/metrics_registered.rs). The test now asserts they're in the map (which they are, after this edit).

Update [`docs/src/observability.md:208`](../../docs/src/observability.md) — the "Histograms not listed here (e.g., `rio_gateway_opcode_duration_seconds`, `rio_store_put_path_duration_seconds`, ...) use the default buckets" sentence becomes stale. Either drop the two names from the example list, or add them to the Histogram Buckets table at `:200+`. discovered_from=consol-mc135.

### T36 — `test(scheduler):` spawn_task loop — no direct test coverage

The rebalancer has `compute_cutoffs` tests ([`rebalancer/tests.rs:18-200`](../../rio-scheduler/src/rebalancer/tests.rs)) and one `apply_pass` integration test (`apply_pass_writes_cutoffs_through_rwlock` at `:227`). But `spawn_task` at [`rebalancer.rs:310-363`](../../rio-scheduler/src/rebalancer.rs) — the actual background loop with `tokio::select!`, `biased;` shutdown-first ordering, and the `interval.tick()` skip-first — has zero test coverage. A regression in the shutdown handling (e.g., dropping `biased;` per the P0335 gotcha in lang-gotchas.md — `tokio::select!` defaults to RANDOM branch choice) would go unnoticed.

NEW test in [`rio-scheduler/src/rebalancer/tests.rs`](../../rio-scheduler/src/rebalancer/tests.rs):

```rust
/// spawn_task respects the shutdown token and exits promptly. The
/// biased; at :342 guarantees shutdown wins over interval.tick() even
/// when both are ready — without it, random branch choice could
/// let the loop run one more pass after shutdown fires (harmless for
/// the rebalancer, but the pattern matters — see P0335 for the
/// cancel-on-disconnect case where it WASN'T harmless).
// r[verify sched.rebalancer.sita-e]
#[tokio::test(start_paused = true)]
async fn spawn_task_exits_on_shutdown() {
    let (_pg, db) = test_db_empty().await;
    let size_classes = Arc::new(RwLock::new(vec![
        SizeClassConfig { name: "a".into(), cutoff_secs: 10.0, ..Default::default() },
    ]));
    let token = rio_common::signal::Token::new();

    spawn_task(db, Arc::clone(&size_classes), token.clone());

    // Let the first interval.tick() consume (the skip-first at :331),
    // then advance past REBALANCE_INTERVAL so the loop is armed.
    tokio::time::advance(REBALANCE_INTERVAL + Duration::from_millis(10)).await;
    tokio::task::yield_now().await;

    // Shutdown — biased; at :342 means the next select! iteration
    // picks shutdown.cancelled() over interval.tick().
    token.cancel();

    // The spawn_monitored wrapper should exit within a few ticks.
    // tokio::time::timeout proves it doesn't hang.
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::yield_now(),
    ).await.expect("spawn_task should exit promptly after shutdown");
}
```

**Gotcha at dispatch:** `spawn_task` uses `rio_common::task::spawn_monitored` which may need a join-handle or completion signal to assert on. Check the `spawn_monitored` API — if it returns no handle, add a `#[cfg(test)]` completion channel OR use `tokio::task::JoinSet` in a test-only variant. discovered_from=230-review.

### T37 — `test(vm):` vm-security-nonpriv-k3s — KVM execution reminder

[`nix/tests/default.nix:321`](../../nix/tests/default.nix) registers `vm-security-nonpriv-k3s` with `r[verify sec.pod.fuse-device-plugin]` at `:310`. The scenario at [`security.nix:1034-1060`](../../nix/tests/scenarios/security.nix) exercises the device-plugin + hostUsers:false production path. bughunt-mc147 reports this VM test was never KVM-executed (broken-builder roulette, same as T10/T13/T15 sibling entries).

**REMINDER TASK** — same shape as T10/T13/T15/T32. Add to [`kvm-pending.md`](../notes/kvm-pending.md) (T16 creates it):

```markdown
- vm-security-nonpriv-k3s (P0360) — privileged-hardening-e2e scenario.
  Device-plugin + cgroup rw-remount + hostUsers:false production path.
  All other k3s fixtures use privileged:true fast-path; this is the
  first nonpriv exercise. r[verify sec.pod.fuse-device-plugin] +
  r[verify sec.pod.host-users-false] + cgroup remount. Never
  KVM-built (bughunt-mc147). Requires P0304-T100's 300s timeout bump
  (DS bring-up adds 30-60s under TCG).
```

discovered_from=bughunt-mc147. Gated on `r[sec.pod.fuse-device-plugin]` + `r[sec.pod.host-users-false]`.

### T38 — `test(scheduler):` GetSizeClassStatus svc-level grpcurl test

[P0231](plan-0231-get-sizeclass-status-rpc-hub.md) adds the `GetSizeClassStatus` RPC with a unit test (`GetSizeClassStatusResponse::decode` roundtrip) but no service-level integration. The plan's exit criterion at `:149` says "grpcurl on a fresh scheduler returns all configured classes with `sample_count: 0`" — that's the svc-level test, but it's not a wired subtest.

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — append a subtest after the existing `sched_grpc` block (grep `sched_grpc` or `port-forward.*9001`):

```python
# r[verify sched.admin.sizeclass-status]
with subtest("GetSizeClassStatus returns configured classes"):
    out = sched_grpc(
        "rio.admin.AdminService/GetSizeClassStatus",
        payload="{}",
    )
    resp = json.loads(out)
    classes = resp.get("classes", [])
    # The standalone fixture configures 2 size-classes via scheduler.toml
    # (grep [[size_classes]] in the fixture). Fresh scheduler →
    # sample_count=0, cutoff values match config.
    assert len(classes) >= 1, f"no classes returned: {resp}"
    for c in classes:
        assert c.get("sampleCount", 0) == 0, \
            f"fresh scheduler should have sample_count=0: {c}"
    # Name-match: class names from scheduler.toml appear.
    names = {c["name"] for c in classes}
    assert "small" in names or len(names) >= 1, f"names={names}"
```

Uses [P0362](plan-0362-extract-submit-build-grpc-helper.md)'s helper pattern (or the `sched_grpc` local if P0362 didn't extract to scheduling.nix's fixture style). Check at dispatch: the standalone fixture's `scheduler.toml` must have `[[size_classes]]` entries — if it has none, GetSizeClassStatus returns empty `classes` and the test needs a precondition seed. discovered_from=231-review.

### T39 — `test(common):` hmac.rs:218 expiry boundary — now==expiry is VALID

cargo-mutants (post-[P0368](plan-0368-cargo-mutants-baseline-failure.md) baseline fix) flags [`rio-common/src/hmac.rs:218`](../../rio-common/src/hmac.rs) as MISSED: `if now_unix > claims.expiry_unix` mutated to `>=` survives. **CORRECTS earlier coordinator speculation** — a prior followup said the mutation was "constant-time comparison" (WRONG). Actual: `>` vs `>=` at the boundary where `now_unix == claims.expiry_unix`. Current code: at-the-second is VALID (`>` rejects only strictly-expired). A `>=` mutation would reject at-the-second tokens.

NEW test in [`rio-common/src/hmac.rs`](../../rio-common/src/hmac.rs) `#[cfg(test)]` mod tests (after `:261` `sign_verify_roundtrip`):

```rust
/// Expiry boundary: token with expiry_unix == now() is VALID.
/// Codifies the `>` (not `>=`) at :218 — at-the-second is the last
/// instant of validity, not the first instant of expiry.
///
/// Catches cargo-mutants' `> → >=` mutation (bughunt-mc147 flagged
/// as MISSED). Without this test, the mutation survives — no test
/// exercises now==expiry exactly.
// r[verify sec.boundary.grpc-hmac]
#[test]
fn expiry_boundary_at_the_second_is_valid() {
    let signer = HmacSigner::from_key(TEST_KEY.to_vec());
    let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

    // Claims with expiry_unix = now (at the boundary). Using
    // SystemTime::now directly — the verify() call reads SystemTime
    // again, so there's a sub-second race. Loop a few times; at
    // least one should land exactly on the boundary OR within the
    // 1s tick. If flakes: add a 1-second sleep in a retry loop.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = Claims {
        worker_id: "boundary-test".into(),
        drv_hash: "abc".into(),
        expected_outputs: vec!["/nix/store/aaa-out".into()],
        expiry_unix: now,  // EXACTLY at boundary
    };
    let token = signer.sign(&claims);

    // verify() reads now() again — may tick to now+1 between sign
    // and verify. If it does, the token is expired and this becomes
    // a NEGATIVE test (Err(Expired)). We want the POSITIVE case.
    // Mitigate: bump expiry_unix to now+1 but check against now+1
    // equality — BUT that's not what we want to test either.
    //
    // Correct approach: mock the clock OR construct claims at now+N
    // and sleep until now+N-epsilon. Simpler: use a cfg(test)-only
    // verify_at(now_unix) that takes the clock as a parameter.
    // Add that if it doesn't exist; makes the boundary test
    // deterministic.

    // Assuming verify_at exists (add it in this task):
    let verified = verifier.verify_at(&token, now)
        .expect("at-the-second (now==expiry) MUST be valid per :218 `>`");
    assert_eq!(verified.expiry_unix, now);

    // Sanity: one second PAST expiry is invalid.
    assert!(
        matches!(verifier.verify_at(&token, now + 1), Err(HmacError::Expired { .. })),
        "one second past expiry MUST fail"
    );
}
```

Add `#[cfg(test)]` `pub(crate) fn verify_at(&self, token: &str, now_unix: u64) -> Result<Claims, HmacError>` as a test-only helper that takes `now_unix` as a parameter instead of `SystemTime::now()`. Refactor `verify()` to delegate: `verify_at(token, SystemTime::now()...)`. Zero behavior change; makes the boundary test deterministic. discovered_from=bughunt-mc147. Forward-referenced by [P0373](plan-0373-mutants-missed-hotspots-aterm-wire.md)-T3.

### T40 — `test(controller):` build_class_statuses — WPS status aggregation unit test

[P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md) adds `build_class_statuses` at [`workerpoolset/mod.rs:202`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) (p234 worktree ref) — reads GetSizeClassStatusResponse, builds per-class `ClassStatus` entries for the WPS status SSA patch. **No unit test** — the P0234-T3 test covers `scale_wps_class` / managedFields, not the status aggregation.

NEW test in [`rio-controller/src/reconcilers/workerpoolset/mod.rs`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) `#[cfg(test)]` mod tests (or a separate `tests.rs` sibling if P0234 established that pattern):

```rust
// r[verify ctrl.wps.cutoff-status]
#[tokio::test]
async fn build_class_statuses_maps_rpc_to_status() {
    let wps = test_wps_with_classes(&["small", "large"]);
    let resp = GetSizeClassStatusResponse {
        classes: vec![
            mock_class("small", /*queued=*/ 5, /*eff_cutoff=*/ 60.0),
            mock_class("large", /*queued=*/ 2, /*eff_cutoff=*/ 3600.0),
            // Class NOT in wps.spec — should be ignored.
            mock_class("nonexistent", 99, 0.0),
        ],
    };
    let statuses = build_class_statuses(&wps, &resp, &mock_wp_api()).await;

    assert_eq!(statuses.len(), 2, "only spec.classes entries, not nonexistent");
    let small = statuses.iter().find(|s| s.name == "small").expect("small present");
    assert_eq!(small.queued, 5);
    assert_eq!(small.effective_cutoff_secs, 60.0);
    // Load-bearing: nonexistent class NOT in output (iterates spec, not RPC resp).
    assert!(
        statuses.iter().all(|s| s.name != "nonexistent"),
        "build_class_statuses must iterate spec.classes not RPC classes"
    );
}
```

discovered_from=234-review. Depends on P0234 merge.

### T41 — `test(controller):` scale_wps_class — happy-path replica patch test

P0234-T3 tests `wps_autoscaler_writes_via_ssa_field_manager` (managedFields assertion). [P0374](plan-0374-wps-asymmetric-key-scaling-flap.md)-T3 tests the name-collision skip. Neither tests the **happy path**: WPS-owned child, class in RPC response, desired computed correctly, STS patched with right replica count. Add:

```rust
// r[verify ctrl.wps.autoscale]
#[tokio::test]
async fn scale_wps_class_patches_child_sts_replicas() {
    let wps = test_wps_with_classes(&["small"]);
    let child = wps_child_with_ownerref(&wps, "small", /*min=*/1, /*max=*/10, /*target=*/5);

    // queued=20, target=5 → desired=ceil(20/5)=4, clamped to [1,10]=4
    let sc_resp = GetSizeClassStatusResponse {
        classes: vec![mock_class_status("small", /*queued=*/ 20)],
    };

    let mut scaler = test_scaler_with_captured_patches();
    scaler.scale_wps_class(&wps, &wps.spec.classes[0], &sc_resp, &[child]).await;

    let patches = scaler.captured_patches();
    assert_eq!(patches.len(), 1, "one replica patch for one class");
    assert_eq!(
        patches[0].pointer("/spec/replicas").and_then(|v| v.as_i64()),
        Some(4),
        "desired replicas = ceil(queued=20 / target=5) = 4"
    );
    // Field manager is the per-class one (P0234-T3 covers this, but
    // re-assert here so T41 standalone proves the full happy path).
    assert_eq!(
        scaler.captured_field_manager(),
        "rio-controller-wps-autoscaler"
    );
}
```

Adjust mock-capture to match P0234-T3's pattern. discovered_from=234-review.

### T42 — `test(cli):` cutoffs subcommand — non-empty table output

[P0236](plan-0236-rio-cli-cutoffs.md) adds `rio-cli cutoffs` which calls GetSizeClassStatus and prints a table. **Same test-gap class as T1-T2** (cli ships pretty-print code that VM test never exercises because MockAdmin returns empty). Add a unit test in [`rio-cli/tests/smoke.rs`](../../rio-cli/tests/smoke.rs) (alongside T1-T3's) OR in `rio-cli/src/cutoffs.rs` `#[cfg(test)]`:

```rust
// r[verify sched.admin.sizeclass-status]
#[test]
fn cutoffs_table_renders_nonempty() {
    let resp = GetSizeClassStatusResponse {
        classes: vec![
            mock_class("small", /*configured=*/60.0, /*effective=*/62.5, /*queued=*/3, /*running=*/1, /*samples=*/150),
            mock_class("large", 3600.0, 3400.0, 0, 2, 80),
        ],
    };
    let output = render_cutoffs_table(&resp);
    // Load-bearing: NOT empty, NOT just headers.
    assert!(output.lines().count() >= 3, "header + 2 data rows");
    assert!(output.contains("small"), "class names in output");
    assert!(output.contains("62.5"), "effective cutoff in output");
}
```

Extract `render_cutoffs_table` as a pure fn from whatever P0236-T1 builds (if it inlines the print, refactor to return String for testability). discovered_from=236-review.

### T43 — `test(vm):` cli.nix cutoffs subtest — end-to-end against running scheduler

MODIFY [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix) (extends T1-T2's populated-state assertions). After the scheduler has size-class config loaded (may need a pre-step ConfigMap or `scheduler.toml [[size_classes]]` entries), run:

```python
with subtest("cli cutoffs — non-empty table"):
    # r[verify sched.admin.sizeclass-status]
    out = cli_run("cutoffs")
    assert "small" in out or "large" in out, \
        f"cutoffs table should list configured classes, got: {out}"
    json_out = cli_run("cutoffs --json")
    parsed = json.loads(json_out)
    assert len(parsed.get("classes", [])) >= 1, \
        "cutoffs --json should have ≥1 class entry"
```

**Precondition:** scheduler.toml has `[[size_classes]]` entries — if the cli.nix fixture doesn't, T43 needs a setup step. Check at dispatch. discovered_from=236-review.

### T44 — `test(dashboard):` BuildDrawer — tab-switch + backdrop-close + onclose-optional

rev-p278 test-gap. [`BuildDrawer.svelte`](../../rio-dashboard/src/components/BuildDrawer.svelte) (p278 worktree) has `activeTab` state at `:15`, backdrop button at `:38-44` that calls `onclose`, and conditional close-✕ at `:58-60` when `onclose` is provided. P0278-T4 exists but scope-check at dispatch — rev-p278 flagged gaps: (a) tab-switch doesn't have a test asserting `activeTab` flips and the correct placeholder renders; (b) clicking backdrop calls the `onclose` callback; (c) when `onclose` is `undefined` (deep-link standalone mount per `:7-9` comment), the ✕ button does NOT render AND clicking backdrop is a no-op (onclose optional chaining). Three test fns in a new `__tests__/BuildDrawer.test.ts` (P0278 may have created it — extend if so, create if not). discovered_from=278-review.

### T45 — `test(dashboard):` GC.svelte — error-toast fires on non-abort stream failure

rev-p281 test-gap. [`GC.svelte:45-49`](../../rio-dashboard/src/pages/GC.svelte) (p281 worktree) has a `catch (e)` that calls `toast.error` ONLY when `!ctrl?.signal.aborted` — user-initiated cancel is swallowed, real stream errors surface. P0281-T7's "Mock each RPC. Assert optimistic state set → success keeps it / error reverts. GC: mock stream → 3 progress chunks → bar updates" covers the happy path. The error-path (`triggerGC` stream throws mid-iteration with NO abort) needs its own test: mock `admin.triggerGC` to return an async-generator that yields one progress then throws; assert `toast.error` called with the error message AND `running` state flipped back to `false`. The abort-is-swallowed path: trigger → click Cancel → abort → assert `toast.error` NOT called. discovered_from=281-review.

### T46 — `test(dashboard):` GC.svelte — $effect teardown aborts in-flight RPC on unmount

rev-p281 test-gap. [`GC.svelte:62`](../../rio-dashboard/src/pages/GC.svelte) (p281 worktree) `$effect(() => () => ctrl?.abort())` — Svelte 5's effect-teardown idiom. If the component unmounts mid-stream, the returned cleanup fn aborts the controller. Test: mount GC, trigger (mock stream hangs on a never-resolving Promise), unmount → assert `AbortController.abort()` was called (spy on the controller or check `ctrl.signal.aborted`). Vitest + `@testing-library/svelte`'s `unmount()` triggers the teardown. discovered_from=281-review.

### T47 — `test(dashboard):` Workers.svelte — poll-interval cleanup + error state renders

rev-p281 test-gap. [`Workers.svelte:35-39`](../../rio-dashboard/src/pages/Workers.svelte) (p281 worktree) `$effect` with `setInterval(refresh, 5000)` and `return () => clearInterval(id)`. Same teardown-idiom as T46. Plus [`Workers.svelte:30-32`](../../rio-dashboard/src/pages/Workers.svelte) sets `error = String(e)` on fetch failure and [`Workers.svelte:64-65`](../../rio-dashboard/src/pages/Workers.svelte) renders `<div role="alert">listWorkers failed: {error}</div>`. Two tests: (a) mount Workers, advance fake timers past 5s, assert `admin.listWorkers` called ≥2× (initial + one tick), unmount, advance past another 5s, assert NOT called again (interval cleared); (b) mock `listWorkers` to reject, assert `role="alert"` element renders with the error text. discovered_from=281-review.

### T48 — `test(infra):` flake.nix helm-lint — proto↔GRPCRoute cardinality assert

rev-p371 test-gap. [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto) `AdminService` has 11 RPCs (12 `rpc ` lines total; `CreateEphemeralWorker` is `ControllerService` — subtract 1). [`dashboard-gateway.yaml`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml) enumerates exactly 11 `method:` matches (7 readonly + 4 mutating). If a future RPC is added to `admin.proto` without a `method:` entry in the GRPCRoute, no CI check fires — silent 404 from dashboard (fail-closed for mutating, broken-UX for read-only).

Add a yq assert to the `helm-lint` check in [`flake.nix:~594`](../../flake.nix) (after the existing `enableMutatingMethods` assert):

```bash
# Cross-file sync: AdminService RPC count == GRPCRoute method matches
PROTO_RPCS=$(grep -c '^  rpc ' ${./rio-proto/proto/admin.proto})
# CreateEphemeralWorker is ControllerService not AdminService — subtract
# the count of rpcs OUTSIDE the AdminService {} block. Simpler: grep
# between 'service AdminService {' and the next '}'.
ADMIN_RPCS=$(awk '/^service AdminService/,/^}/' ${./rio-proto/proto/admin.proto} | grep -c '^  rpc ')
ROUTE_METHODS=$(yq 'select(.kind=="GRPCRoute") | .spec.rules[].matches[].method.method' \
  /tmp/dash-full.yaml /tmp/dash-mut.yaml | sort -u | wc -l)
[ "$ADMIN_RPCS" -eq "$ROUTE_METHODS" ] || {
  echo "FAIL: AdminService has $ADMIN_RPCS RPCs, GRPCRoute has $ROUTE_METHODS method matches" >&2
  echo "  Add new RPC to dashboard-gateway.yaml readonly or mutating route" >&2
  exit 1
}
```

The existing helm-lint at `:585` already templates `/tmp/dash-mut.yaml` with `enableMutatingMethods=true`; need to also template a readonly variant (`enableMutatingMethods=false`) to `/tmp/dash-readonly.yaml` — OR use one template with both flags and yq-select both GRPCRoute names. discovered_from=371-review.

### T49 — `test(vm):` cli.nix wps subtest — end-to-end against real apiserver

rev-p237 test-gap. `rio wps` subcommand has ZERO coverage — neither [`smoke.rs`](../../rio-cli/tests/smoke.rs) nor `cli.nix` invoke it. `smoke.rs` covers every OTHER subcommand (status/list-tenants/workers/builds/cutoffs/logs/gc/poison-clear). wps is kube-only (no gRPC), so MockAdmin can't cover it.

Add subtest to `nix/tests/scenarios/cli.nix` (or the scenario file that owns cli subtests — check at dispatch):

```python
with subtest("rio-cli wps list+describe"):
    # k3s-full fixture has WorkerPoolSet CRD + live apiserver
    cluster.succeed("kubectl apply -f ${test_wps_yaml}")
    cluster.wait_until_succeeds("kubectl get wps test-wps -o jsonpath='{.metadata.name}'")
    out = cluster.succeed("rio wps list --namespace rio-test")
    assert "test-wps" in out, f"expected test-wps in list: {out!r}"
    desc = cluster.succeed("rio wps describe test-wps --namespace rio-test")
    # Children may not be reconciled yet (controller lag) — accept either
    # "-/-" (child not found) or "N/M" (replicas). The RBAC-denied case
    # (from P0382) isn't exercised here; this is happy-path.
    assert "small" in desc or "CLASSES" in desc
```

Partner to T42/T43 (cutoffs unit+VM) — same cli.nix insertion pattern. The `test_wps_yaml` fixture is a minimal WorkerPoolSet with 1-2 classes. discovered_from=237-review.

### T50 — `test(controller):` prune_stale_children — direct ApiServerVerifier unit test

rev-p374 test-gap. [`workerpoolset/mod.rs:226`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) `prune_stale_children(wps, wp_api)` has no direct unit test. Existing `r[verify ctrl.wps.prune-stale]` at [`scaling.rs:1633`](../../rio-controller/src/scaling.rs) (`is_wps_owned_by_matches_uid_not_just_kind`) tests the UID filter — NOT the prune flow itself: `active_classes` membership check (`:257`), `deletion_timestamp` skip (`:250`), delete 404-tolerance (`:269`).

The fn takes `Api<WorkerPool>` — needs `ApiServerVerifier` fixture (same pattern as `wps_autoscaler_writes_via_ssa_field_manager` at [`scaling.rs:~1692`](../../rio-controller/src/scaling.rs) if that test exists post-P0234, or the pattern from [`rio-controller/src/fixtures.rs`](../../rio-controller/src/fixtures.rs) `apply_ok_scenarios`). Add to `rio-controller/src/reconcilers/workerpoolset/tests.rs` (new file if absent):

```rust
#[tokio::test]
async fn prune_stale_children_deletes_removed_class_skips_active_and_terminating() {
    // WPS with classes=[small, large]; 3 children: small (active),
    // large (active), medium (stale — class removed from spec).
    // medium has NO deletionTimestamp; a 4th child "huge" has
    // deletionTimestamp set (already being deleted).
    // Expected: prune issues DELETE for "test-wps-medium" ONLY.
    let wps = test_wps_with_classes(&["small", "large"]);
    let scenarios = vec![
        Scenario { verb: "GET", path_suffix: "workerpools", /* list returns 4 children */ },
        Scenario { verb: "DELETE", path_suffix: "workerpools/test-wps-medium", response_code: 200 },
    ];
    // Run prune; ApiServerVerifier asserts exactly those 2 calls in order.
}

#[tokio::test]
async fn prune_stale_children_tolerates_404_on_delete() {
    // stale child → DELETE returns 404 (GC won the race); prune
    // continues without propagating error.
    let scenarios = vec![
        Scenario { verb: "GET", path_suffix: "workerpools", /* list returns 1 stale child */ },
        Scenario { verb: "DELETE", path_suffix: "workerpools/test-wps-medium", response_code: 404 },
    ];
    // prune returns without panic; test PASSES == no-error-propagated.
}
```

The UID-filter behavior (2nd WPS in same ns) is already covered by `is_wps_owned_by_matches_uid_not_just_kind` — T50 doesn't re-test that, it tests the prune FLOW around it. discovered_from=374-review.

### T51 — `test(infra):` helm-lint yq thirdparty-image loop — null-filter silent false-green guard

rev-p379 CORRECTNESS (batched — helm-lint check-definition, not code behavior). [P0379-T2](plan-0379-envoy-image-digest-pin.md) at `:55-72` adds a generalized `yq eval-all` loop over all rendered container images to assert third-party ones are digest-pinned. The yq path `.spec.template.spec.containers[].image` returns `null` for manifests without that path (ConfigMaps, Secrets, ServiceAccounts in the render). Piped through `grep -v ':test$'` (rio-image exclusion), a stream of `null\nnull\n...` + one third-party image that IS digest-pinned produces zero failure output → check passes. But if a manifest mis-specs `containers` (e.g., typo `container:` singular, or nested under wrong key), that image drops to `null` too — silently excluded, never asserted.

Add a `select(. != null)` filter AND a minimum-count guard:

```bash
thirdparty=$(yq eval-all '
  select(.kind == "Deployment" or .kind == "DaemonSet" or .kind == "StatefulSet")
  | .spec.template.spec.containers[].image
  | select(. != null)
' "$RENDERED" | grep -v '^rio-' | grep -v ':test$')

# Floor-check: the render MUST have at least N third-party images.
# If yq path breaks (schema change, typo), $thirdparty drops to 0
# and the digest-pin check below silently passes. This guard catches
# that class.
count=$(echo "$thirdparty" | grep -c .)
[ "$count" -ge 2 ] || {
  echo "helm-lint: expected >=2 third-party images, got $count — yq path broken?"
  exit 1
}
```

The `>=2` floor is conservative (envoy + device-plugin at minimum). Adjust at dispatch based on the actual render output from `helm template ... | yq '.spec.template.spec.containers[].image' | sort -u`. **Same proves-nothing axis as P0241 (airgapped-VM environment) and P0328 (regex-false-green)**: a check that passes when its path is broken proves nothing. discovered_from=379-review.

### T52 — `test(vm):` plan-0239 dispatch-note — use sched_grpc/submit_build_grpc, don't inline 5th grpcurl helper

rev-p239 pre-dispatch guard (seed: "wps-lifecycle grpcurl-5th-copy"). [P0239](plan-0239-vm-pdb-section-h-wps.md) adds `pdb-ownerref` + `wps-lifecycle` fragments to [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix). Both fragments as-drafted use `kubectl` only (no grpcurl). If the implementer extends `wps-lifecycle` to verify scheduler-side class status (e.g., `GetSizeClassStatus` after child WorkerPools appear), the natural move is another inline `grpcurl ${grpcurlTls} ... port-forward ... trap` block — the FIFTH copy of that pattern.

[`lifecycle.nix:375`](../../nix/tests/scenarios/lifecycle.nix) `sched_grpc(payload, method)` and [`:407`](../../nix/tests/scenarios/lifecycle.nix) `submit_build_grpc(payload, max_time)` are the extracted helpers from [P0362](plan-0362-extract-submit-build-grpc-helper.md). T52 is a **dispatch-note append** to [`plan-0239-vm-pdb-section-h-wps.md`](plan-0239-vm-pdb-section-h-wps.md):

```markdown
> **DISPATCH NOTE (rev-p239):** if wps-lifecycle needs scheduler gRPC
> verification (e.g., GetSizeClassStatus after children appear), use
> the existing `sched_grpc` helper at lifecycle.nix:375 — do NOT inline
> a 5th port-forward+grpcurlTls+trap block. [P0362] extracted this
> exactly to stop the copy-paste. The wps-lifecycle fragment runs in
> the SAME testScript scope, so `sched_grpc` is in scope.
```

Not a test-gap per se — a **pre-emptive consolidation guard** before P0239 dispatches. Low cost (plan-doc prose), high value (prevents the consolidator from re-flagging lifecycle.nix grpcurl duplication at mc+7). discovered_from=239-review.

### T53 — `test(nix):` plan-0239 + kvm-pending.md — NEVER-BUILT fragment tracking entry

rev-p239 reminder-task (seed: "NEVER-BUILT tracking"). Same shape as T10/T13/T15/T32/T37. [P0239](plan-0239-vm-pdb-section-h-wps.md)'s `pdb-ownerref` + `wps-lifecycle` fragments carry `r[verify ctrl.wps.reconcile]` + `r[verify ctrl.wps.autoscale]` at col-0. If P0239 merges via clause-4 fast-path (likely — lifecycle.nix is `nix/tests/`-only), the fragments NEVER build until a KVM-capable slot opens. Add to [`kvm-pending.md`](../notes/kvm-pending.md) (T16 creates it):

```
- vm-lifecycle-k3s / pdb-ownerref + wps-lifecycle fragments (P0239):
  r[verify ctrl.wps.reconcile] + r[verify ctrl.wps.autoscale] at col-0.
  PDB ownerRef cascade + WPS 3-child create→GC'd lifecycle. Never
  KVM-built if P0239 fast-pathed. Run: /nixbuild .#checks.x86_64-linux.vm-lifecycle-k3s
  → check `pdb-ownerref` + `wps-lifecycle` subtest output.
```

Also: MODIFY [`plan-0239-vm-pdb-section-h-wps.md`](plan-0239-vm-pdb-section-h-wps.md) `## Exit criteria` to add the OR-documented-in-kvm-pending clause (same shape as T32/T37/T49's confirmatory-not-gating phrasing). discovered_from=239-review.

### T54 — `test(scheduler):` completion.rs CA cutoff-compare — slow-store timeout doesn't block completion

rev-p251 test-gap (seed: "timeout"). [P0251](plan-0251-ca-cutoff-compare.md)'s merged code at [`completion.rs:309-326`](../../rio-scheduler/src/actor/completion.rs) wraps `content_lookup` in `tokio::time::timeout(DEFAULT_GRPC_TIMEOUT, ...)`. Good — the timeout exists. But no test proves it: if a refactor removes the wrapper, a slow/hung store blocks the scheduler's completion path indefinitely (every CA build's `handle_completed` awaits the store).

Add to [`actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs):

```rust
// r[verify sched.ca.cutoff-compare]
// Slow-store regression guard: content_lookup that never responds must
// NOT block completion past DEFAULT_GRPC_TIMEOUT. The timeout wrapper at
// completion.rs:309 is what keeps the scheduler responsive — if it's
// removed, this test hangs (use tokio's timeout-on-test-fn as the
// outer guard to catch that).
#[tokio::test(start_paused = true)]
async fn ca_cutoff_compare_slow_store_doesnt_block_completion() {
    // MockStore.content_lookup → futures::future::pending() (never resolves)
    // Submit CA drv, complete it.
    // tokio::time::advance past DEFAULT_GRPC_TIMEOUT.
    // Assert: handle_completed returned; state.ca_output_unchanged = false;
    //         counter outcome="error" (or "miss" if P0304-T148 hasn't landed).
    // Assert: state transitioned to Completed (completion PROCEEDED despite lookup failure).
}
```

**CARE — `start_paused` + MockStore**: `tokio::time::pause()` mocks `tokio::time::*` but not sqlx/PG pool timeouts. If the test uses a real PG pool, `start_paused` breaks pool acquisition. Use MockStore + in-memory actor state (no PG). The P0251 test at `4ebd385d` (`test(scheduler): CA cutoff-compare — match/miss/non-CA scenarios`) likely has the scaffold — extend it with the pending-future case. discovered_from=251-review. Soft-dep [P0304](plan-0304-trivial-batch-p0222-harness.md)-T148 (adds `outcome="error"` third label — T54's counter-assert checks whichever landed).

### T55 — `test(scheduler):` DerivationState.ca_output_unchanged — restart-recovery semantics

rev-p251 test-gap (seed: "persist"). [`ca_output_unchanged`](../../rio-scheduler/src/state/derivation.rs) is in-memory `DerivationState`-only — NOT persisted to PG. If the scheduler restarts between `handle_completed` setting it and [P0252](plan-0252-ca-cutoff-propagate-skipped.md)'s propagation consuming it, the flag is lost. Cutoff silently doesn't happen for that derivation (downstream rebuilds instead of skipping — correctness-safe but perf-wasteful).

**Two resolutions** — pick one at dispatch:

**Option A (document-and-accept)**: cutoff is an optimization. Restart during the compare→propagate window is rare; the cost is one redundant build, not incorrectness. Add a doc-comment at the `ca_output_unchanged` field declaration explaining the restart-loss semantics + a `// r[verify]` annotation on a test that asserts recovery sets it to `false` (default) and proceeds correctly:

```rust
/// Set by completion.rs hash-check ([P0251]). Consumed by
/// dag/mod.rs find_cutoff_eligible() ([P0252]).
///
/// NOT persisted — if scheduler restarts between compare and propagate,
/// flag resets to false. Downstream builds proceed (no cutoff). This is
/// correctness-safe (rebuild > stale-skip) at the cost of one wasted
/// build per affected drv. P0252's propagation runs in the same actor
/// tick as completion.rs's set — the window is a single message-loop
/// iteration, not a multi-second gap.
pub ca_output_unchanged: bool,
```

**Option B (persist)**: add `ca_output_unchanged` to the DB `derivations` row (migration N+1), persist in `persist_status` alongside `Completed` status, restore in recovery. Heavier (migration + persist-path touch), but closes the gap. Only pursue if [P0252](plan-0252-ca-cutoff-propagate-skipped.md)'s propagation runs in a LATER actor tick (making the window observable).

At dispatch: read [P0252](plan-0252-ca-cutoff-propagate-skipped.md)'s design. If propagate happens in the same `handle_completed` call, Option A is correct (window is zero). If it's a separate actor message, Option B may be warranted. **Prefer Option A** unless P0252 explicitly defers propagation. Add the test either way:

```rust
// r[verify sched.ca.cutoff-compare]
#[tokio::test]
async fn ca_output_unchanged_resets_on_recovery() {
    // Seed PG with a CA drv in Completed state.
    // Restart actor (recovery path).
    // Assert: state.ca_output_unchanged == false (default).
    // This is the DOCUMENTED behavior, not a bug — proves recovery
    // doesn't LEAK a stale true from some other path.
}
```

discovered_from=251-review.

### T56 — `test(vm):` kvm-pending.md — vm-lifecycle-wps-k3s entry (partners with T53)

rev-p239 test-gap. **PARTNERS WITH T53** (docs-267443 added the same kvm-pending entry; T56 cross-checks format parity). T53's entry uses `vm-lifecycle-k3s / pdb-ownerref + wps-lifecycle fragments (P0239)` format; ensure the entry lists BOTH fragments and the two `r[verify]` markers they carry (`ctrl.wps.reconcile` + `ctrl.wps.autoscale`). If T53 already covers this fully at dispatch, T56 is a no-op/review. discovered_from=239 (independent re-flag — confirms the finding).

### T57 — `test(scheduler):` warm_gate_fallback — counter increment assert

rev-p299 test-gap. MODIFY [`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs) at `:596-617`. `warm_gate_fallback_when_no_warm_workers` asserts the PICK (`Some("b")`) but NOT the counter increment. [P0299](plan-0299-staggered-scheduling-cold-start.md) EC `:160` explicitly says "increments `rio_scheduler_warm_gate_fallback_total`".

`CountingRecorder` exists in [`rio-test-support`](../../rio-test-support/src/metrics.rs), used by 3 sibling tests (`actor/tests/worker.rs:449`, `misc.rs:84`, `completion.rs:31`). Add recorder setup + `assert_counter(rio_scheduler_warm_gate_fallback_total{}, 1)` after the pick assert:

```rust
// r[verify sched.assign.warm-gate]
// + r[verify obs.metric.scheduler] — fallback_total increments
#[test]
fn warm_gate_fallback_when_no_warm_workers() {
    let recorder = CountingRecorder::install();
    // ... existing test body ...
    assert_eq!(result.as_deref(), Some("b"), /* ... */);
    assert_eq!(
        recorder.get("rio_scheduler_warm_gate_fallback_total", &[]),
        1,
        "fallback path must increment fallback_total exactly once"
    );
}
```

`assignment.rs` is `#[cfg(test)]` mod tests — `r[verify]` annotation works there (not in `test_include`, in `include`). discovered_from=299.

### T58 — `test(scheduler):` on_worker_registered — initial-hint dedicated unit tests

rev-p299 test-gap. MODIFY [`rio-scheduler/src/actor/tests/worker.rs`](../../rio-scheduler/src/actor/tests/worker.rs). [P0299](plan-0299-staggered-scheduling-cold-start.md) EC `:159` promised "Fresh worker receives PrefetchHint within one tick of registration (unit test with `start_paused`)" — **never written**. Test helper at `helpers.rs:126` papers over it by unconditionally sending `PrefetchComplete`.

Missing coverage (three cases from the finding):

1. **merge-then-connect** → initial hint arrives before any `Assignment` on the stream
2. **connect-then-empty-queue** → `warm` flipped `true` immediately (short-circuit at [`worker.rs:126-136`](../../rio-scheduler/src/actor/worker.rs))
3. **hint-send-fails** → `warm` flipped anyway (defensive path at `:158-163` — "gate is optimization, not correctness")

```rust
// r[verify sched.assign.warm-gate]
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn on_worker_registered_sends_initial_hint_before_assignment() {
    // Merge a DAG with ≥1 Ready node, THEN register a worker.
    // Assert: first msg on stream_rx is PrefetchHint (not Assignment).
    // Send PrefetchComplete, advance clock, assert Assignment arrives.
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn on_worker_registered_empty_queue_flips_warm_immediately() {
    // Register worker BEFORE any DAG merge. Assert: worker.warm == true
    // immediately, no PrefetchHint on stream (nothing to prefetch for).
    // Then merge DAG, assert Assignment arrives without waiting for ACK.
}

#[tokio::test]
async fn on_worker_registered_send_fail_flips_warm_anyway() {
    // Register worker with stream_tx full or closed. Assert: warn! fires
    // + worker.warm == true (defensive flip, :158-163).
}
```

Add near existing warm-gate tests (grep `:449` region). These partner with [P0391](plan-0391-warm-gate-deterministic-prefetch.md)'s determinism test (T3 there — different axis: ordering-proof vs reproducibility). discovered_from=299.

### T59 — `test(worker):` handle_prefetch_hint joiner — fetched/cached/sink-closed coverage

rev-p299 test-gap. MODIFY [`rio-worker/src/runtime.rs`](../../rio-worker/src/runtime.rs) in `#[cfg(test)] mod` near `:844`. The joiner at `:791-841` (+60 new lines from P0299: handle collection, fetched/cached counting, `PrefetchComplete` send) has zero unit-test coverage. Only verified via VM metrics post-hoc at [`scheduling.nix:1276`](../../nix/tests/scenarios/scheduling.nix) (`hist_count >= 1`).

No test for: (a) all-cached paths → `paths_fetched=0 paths_cached=N`; (b) mixed labels; (c) sink-closed → debug-log-not-error path at `:833-838`. The `spawn_blocking + Cache` pattern from `test_heartbeat_includes_bloom_from_cache:1091` shows the test harness exists.

```rust
// r[verify sched.assign.warm-gate]
#[tokio::test]
async fn prefetch_hint_joiner_reports_accurate_counts() {
    // Seed cache with 3 paths. Send PrefetchHint with 5 paths
    // (3 cached + 2 not). Await joiner. Assert PrefetchComplete
    // on stream_rx has paths_fetched=2, paths_cached=3.
}

#[tokio::test]
async fn prefetch_hint_joiner_sink_closed_is_debug_not_error() {
    // Close stream_tx BEFORE joiner sends ACK. Assert:
    // logs_contain("sink closed; shutting down") at DEBUG
    // and NOT at ERROR/WARN.
}
```

discovered_from=299.

### T60 — `test(dashboard):` LogViewer.svelte — error/empty/spinner jsdom-testable paths

rev-p279 test-gap. NEW (or MODIFY if P0279-T4 creates it) [`rio-dashboard/src/components/__tests__/LogViewer.test.ts`](../../rio-dashboard/src/components/__tests__/LogViewer.test.ts). Other components have test files (`BuildStatePill`, `ClearPoisonButton`, `DrainButton`, `Toast`); LogViewer doesn't. Comment at [`LogViewer.svelte:47-48`](../../rio-dashboard/src/components/LogViewer.svelte) says follow-tail is "covered manually" — jsdom layout limitation acknowledged. But error-alert render (`:63-65`), empty-state (`:73-74`), and spinner-visible-until-done (`:69-72`) are all jsdom-testable.

```ts
// Three jsdom-testable paths (follow-tail scroll is NOT — layout zeros).
it('renders error alert when stream.err is set', () => {
  // mount with mock stream where err = new Error('boom')
  // assert role=alert exists with 'log stream failed: boom'
});
it('renders empty-state when done && lines empty && no err', () => {
  // mount with done=true, lines=[], err=null
  // assert 'no log output' text present, spinner absent
});
it('shows spinner until stream.done', () => {
  // mount with done=false
  // assert data-testid='log-tail' present, 'streaming…' text
  // flip done=true, assert spinner gone
});
```

`logStream.test.ts` tests the store; this tests the component rendering. Partners with [P0392](plan-0392-logstream-quadratic-virtualize.md)-T4 (windowed-render-count assert — same file, additive). discovered_from=279. **Post-P0279-merge.**

### T61 — `test(scheduler):` CA-compare — zero-outputs/malformed/Err/Elapsed edge paths

rev-p251 test-gap (coord-flagged). MODIFY [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs). The CA-compare hook at [`completion.rs:289-337`](../../rio-scheduler/src/actor/completion.rs) has edge paths with no dedicated coverage.

**Partners with T54** (T54 = slow-store timeout doesn't-block-completion; T61 = explicit edge-input assertions). T54 covers the timeout ARM; T61 extends to the other edges:

1. **zero outputs** (`built_outputs.is_empty()`) → `all_matched` starts `false` at `:293` (`!result.built_outputs.is_empty()`). Never sets `ca_output_unchanged = true` on empty result. Correct, but untested.
2. **malformed hash** (`output_hash.len() != 32` at `:295`) → debug + `all_matched=false` + `continue`. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T157 adds the counter; this T adds the test.
3. **RPC `Err(Status)`** at `:316-320` → debug + `matched=false`. T54 exercises timeout implicitly; this explicitly asserts the `Err` arm's counter label ([P0304](plan-0304-trivial-batch-p0222-harness.md)-T148's `outcome=error`).

```rust
// r[verify sched.ca.cutoff-compare]
#[tokio::test]
async fn ca_compare_zero_outputs_is_not_unchanged() {
    // CA completion with built_outputs=[]. Assert ca_output_unchanged
    // stays false (the empty-start at :293). Defensive — worker bug
    // emitting zero outputs shouldn't accidentally enable cutoff.
}

#[tokio::test]
async fn ca_compare_malformed_hash_counts_as_miss() {
    // 1 output with output_hash=16 bytes (not 32). Assert:
    // counter{outcome=malformed}==1 (via P0304-T157),
    // ca_output_unchanged==false, ContentLookup NOT called.
}

#[tokio::test]
async fn ca_compare_rpc_err_counts_as_error() {
    // MockStore: content_lookup → Err(Unavailable).
    // 2 outputs. Assert: counter{outcome=error}==2 (or ==1 if
    // P0393 short-circuit landed — via P0304-T148's label),
    // ca_output_unchanged==false.
}
```

Partners with [P0393](plan-0393-ca-contentlookup-serial-timeout.md)-T4 (breaker+short-circuit tests — different axis: edge-inputs vs latency-optimization). Both add to `tests/completion.rs`; additive, non-overlapping test-fn names. discovered_from=251.

### T62 — `test(scheduler):` downstream_placeholder — Nix known-answer golden value

rev-p253 test-gap. MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) `#[cfg(test)] mod tests` near the existing `placeholder_shape` test (`~:594`). Current tests are shape-only (length/alphabet/strip-suffix) — no golden-value against upstream Nix. [`nix/src/libstore-tests/downstream-placeholder.cc:16-20`](https://github.com/NixOS/nix/blob/master/src/libstore-tests/downstream-placeholder.cc) has:

```cpp
StorePath{"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo.drv"}, "out"
→ "/0c6rn30q4frawknapgwq386zq358m8r6msvywcvc89n6m5p2dgbz"
```

Add:

```rust
// r[verify sched.ca.resolve]
#[test]
fn placeholder_golden_matches_nix_upstream() {
    // Golden value from nix/src/libstore-tests/downstream-placeholder.cc:16-20.
    // Catches nixbase32 bit-order / Sha256 input-encoding divergence
    // that shape tests (length/alphabet) miss.
    let drv_path = StorePath::parse(
        "/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo.drv"
    ).unwrap();
    let p = downstream_placeholder(&drv_path, "out");
    assert_eq!(p, "/0c6rn30q4frawknapgwq386zq358m8r6msvywcvc89n6m5p2dgbz");
}
```

If this FAILS at dispatch: the divergence is in `nixbase32::encode` (byte-order) OR the `drv_path.hash_part()` call (whether it includes the name or just the 32-char prefix) OR the cleartext format string. The test IS the debugging probe. Soft-dep [P0304-T159](plan-0304-trivial-batch-p0222-harness.md) (Result→String signature — this test's `downstream_placeholder(...)` call drops the `.unwrap()` if T159 landed first). discovered_from=253.

### T63 — `test(scheduler):` maybe_resolve_ca + collect_ca_inputs gate-path coverage

rev-p253 test-gap. MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) `#[cfg(test)]` (or a dedicated `rio-scheduler/src/actor/tests/dispatch.rs` if the file has a tests submodule). `maybe_resolve_ca` (`:645-717`) + `collect_ca_inputs` (`:726-752`) added by P0253 with zero coverage. The gate logic is LIVE on every dispatch even with the empty-vec stub:

```rust
// r[verify sched.ca.resolve]
#[tokio::test]
async fn maybe_resolve_ca_ia_derivation_passthrough() {
    // state.is_ca=false → :654 gate fails → returns drv_content unchanged.
    // Assert: no DB query fired, returned bytes == input drv_content.
}

#[tokio::test]
async fn maybe_resolve_ca_fixed_output_passthrough() {
    // state.is_ca=true BUT state.is_fixed_output=true → same gate fails.
    // Assert: passthrough. FOD is CA but output path is pre-known,
    // no resolve needed (ADR-018 shouldResolve table).
}

#[tokio::test]
async fn maybe_resolve_ca_empty_drv_content_passthrough() {
    // state.is_ca=true, is_fixed_output=false, drv_content=[].
    // :663 gate → passthrough (recovered derivation, can't resolve).
}

#[tokio::test]
async fn maybe_resolve_ca_no_ca_inputs_passthrough() {
    // Floating CA with only IA children → collect_ca_inputs=[] →
    // :688 gate → passthrough. The "CA mkDerivation on IA stdenv"
    // common case per :685 comment.
}

#[tokio::test]
async fn maybe_resolve_ca_resolve_error_swallows_to_warn() {
    // Force resolve_ca_inputs → Err (mock PG failure).
    // :708-715 swallow → warn + returns original drv_content.
    // Assert: no panic, returned == input, warn! fired
    // (via tracing-test or MockRecorder).
}
```

**After [P0254-T6](plan-0254-ca-metrics-vm-demo.md) lands** `collect_ca_inputs` returns non-empty and these tests extend to cover the live path (`ca_modular_hash` plumbed → actual CaResolveInput pushed → resolve fires with real inputs). Mark the last test `#[ignore = "P0254-T6"]` if it depends on the plumbing, or use a test-only backdoor to populate `ca_modular_hash`. discovered_from=253.

### T64 — `test(scheduler):` cargo-mutants MISSED — assignment.rs best_worker + derivation.rs transitions

rev-p373 test-gap. NEW tests targeting cargo-mutants scattered MISSED in arithmetic-heavy regions. Beyond P0373-T5 budget; dedicated mutation coverage needed.

**[`rio-scheduler/src/assignment.rs`](../../rio-scheduler/src/assignment.rs) `best_worker` scoring (48 candidate mutations):** the scoring formula mixes `%`, `/`, `>`, `>=`, `+`, `-` — mutations that flip operator sense or swap `>` for `>=` produce subtly-wrong-but-still-compiles scoring. Add a table-driven test that asserts EXACT scores for known worker/load/queue configurations:

```rust
// r[verify sched.assign.warm-gate]
#[test]
fn best_worker_scoring_table() {
    // 10 rows: (worker_state, expected_score). Each row exercises a
    // distinct arithmetic path (empty queue vs full, >= vs > boundary
    // on the concurrency cap, % on the load-balance slot, etc.).
    // Any mutant that changes ONE operator should flip ≥1 row's score.
    for (state, expected) in cases {
        assert_eq!(score(&state), expected, "case {state:?}");
    }
}
```

**[`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs) `DerivationStatus` transitions (30 candidate mutations):** `validate_transition` has N match arms; a mutant that deletes one arm or swaps `Ok(())` for `Err` silently breaks one transition. Add exhaustive cartesian-product test:

```rust
#[test]
fn validate_transition_exhaustive() {
    // Every (from, to) pair. Assert exact Ok/Err result per the
    // state-machine diagram at scheduler.md r[sched.state.machine].
    // ~8 variants × 8 = 64 cases. A mutant that flips ANY arm's
    // outcome breaks exactly 1 case.
}
```

Run `just mutants-scheduler` (or equivalent) after to confirm MISSED→CAUGHT. Soft-dep [P0373](plan-0373-cargo-mutants-triage-wire-aterm-hmac.md) (the mutants infrastructure). discovered_from=373.

### T65 — `test(store):` cargo-mutants MISSED — manifest.rs chunk-length arithmetic boundary

rev-p373 test-gap. MODIFY [`rio-store/src/manifest.rs`](../../rio-store/src/manifest.rs) tests near `:106-115`. Serialize/deserialize arithmetic has 13 candidate mutations: `%` and `/` in chunk-length math, `>` vs `>=` at boundary. Pattern from [P0373-T2](plan-0373-cargo-mutants-triage-wire-aterm-hmac.md)'s wire `MAX_FRAME_SIZE` boundary test — apply same shape here:

```rust
#[test]
fn manifest_chunk_boundary_arithmetic() {
    // 5 inputs: chunk-size exactly on boundary, ±1, far below, far above.
    // Assert serialize→deserialize round-trips EXACT values. A mutant
    // that swaps % for / or > for >= breaks the boundary cases.
    for (input_len, expected_chunks) in [
        (CHUNK_SIZE - 1, 1),       // below → 1 chunk
        (CHUNK_SIZE,     1),       // exact → 1 chunk (>= vs > boundary)
        (CHUNK_SIZE + 1, 2),       // +1 → 2 chunks
        (CHUNK_SIZE * 3, 3),       // multiple exact
        (CHUNK_SIZE * 3 + 1, 4),   // multiple +1
    ] {
        let m = serialize_manifest(&bytes(input_len));
        assert_eq!(m.chunk_count(), expected_chunks);
        // Round-trip: deserialize(serialize(x)).len() == x.len()
    }
}
```

Grep for the exact const name (`CHUNK_SIZE` vs `MANIFEST_CHUNK_LEN` etc.) at dispatch. Soft-dep P0373 (mutants infra). discovered_from=373.

### T66 — `test(controller):` ephemeral_deadline_seconds Some(N) → activeDeadlineSeconds propagation

rev-p347 test-gap. MODIFY [`rio-controller/src/reconcilers/workerpool/ephemeral.rs`](../../rio-controller/src/reconcilers/workerpool/ephemeral.rs) near `:394` (or its test file). `job_spec_load_bearing_fields` covers `None→3600` default only; no unit test sets `wp.spec.ephemeral_deadline_seconds = Some(7200)` and asserts `spec.active_deadline_seconds == Some(7200)`. VM negative-apply proves CEL rejects the field on non-ephemeral pools, but no positive exercises the non-default `Some` branch.

```rust
#[test]
fn ephemeral_deadline_some_propagates_to_job_spec() {
    let wp = test_workerpool_ephemeral(|spec| {
        spec.ephemeral_deadline_seconds = Some(7200);
    });
    let job_spec = build_ephemeral_job_spec(&wp, /* ... */);
    assert_eq!(job_spec.active_deadline_seconds, Some(7200));
}
```

Uses [P0380](plan-0380-workerpoolspec-test-fixture-extraction.md)'s `test_workerpool_ephemeral` builder if available; otherwise inline the spec literal. discovered_from=347.

### T67 — `test(scheduler):` verify_cutoff_candidates wiring — |h| verified.contains(h) mutation kill

rev-p252 test-gap (validator-noted). MODIFY [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs). The wiring `|h| verified.contains(h)` at [`completion.rs:467`](../../rio-scheduler/src/actor/completion.rs) is NOT integration-tested — mutation `|_| true` (skip every cutoff candidate unconditionally) undetected by unit tests.

Simple test using [`spawn_mock_store_with_client`](../../rio-test-support/src/grpc.rs) at `:1126` + `MockStore.find_missing_paths` at `:409` (works off `store.paths` HashMap):

```rust
// r[verify sched.ca.cutoff-propagate]
#[tokio::test]
async fn cascade_only_skips_verified_candidates() {
    // A (is_ca) → B → C chain. Seed MockStore.paths with B's output
    // ONLY (not C's). Submit with expected_output_paths so verify
    // checks store presence. Complete A with ca_unchanged=true.
    //
    // Expected: B goes Skipped (output present in store — verified).
    //           C does NOT go Skipped (output absent — verify-rejected).
    //           [P0399 makes C go Ready instead of stuck-Queued.]
    //
    // Mutation check: replace `|h| verified.contains(h)` with `|_| true`
    // → C goes Skipped too (test FAILs on assert_ne).
    let (mock, client) = spawn_mock_store_with_client();
    mock.paths.insert(b_output_path);  // B's output present, C's absent
    // ... submit + complete A ...
    assert_eq!(dag.node(&b).unwrap().status(), DerivationStatus::Skipped);
    assert_ne!(dag.node(&c).unwrap().status(), DerivationStatus::Skipped);
}
```

~30 lines. Partners with [P0399](plan-0399-all-deps-completed-skipped-hang.md)-T3 (that test asserts C goes Ready; this test asserts C does NOT go Skipped — complementary assertions on the same scenario). discovered_from=252. **Post-P0252-merge** (DONE).

### T68 — `test(scheduler):` find_cutoff_eligible_speculative — non-empty provisional_skipped set

rev-p252 test-gap. MODIFY [`rio-scheduler/src/dag/tests.rs`](../../rio-scheduler/src/dag/tests.rs). `find_cutoff_eligible_speculative` with NON-EMPTY `provisional_skipped` is not directly unit-tested — only the empty-set path (via `find_cutoff_eligible` wrapper) is covered by `cascade_cutoff` tests. The non-empty path (`provisional_skipped.contains(d)` OR-branch at [`dag/mod.rs:534`](../../rio-scheduler/src/dag/mod.rs)) is only reachable from the private `verify_cutoff_candidates` speculative walk at [`completion.rs:77-99`](../../rio-scheduler/src/actor/completion.rs).

```rust
#[test]
fn speculative_provisional_skipped_makes_parent_eligible() {
    // Chain A → B → C. All Queued. A just completed.
    // Call find_cutoff_eligible_speculative(A, {}) → returns [B]
    //   (B's only dep A is terminal → B eligible).
    // Call find_cutoff_eligible_speculative(B, {B}) → returns [C]
    //   (C's only dep B is in provisional_skipped → C eligible).
    // Inverted-contains bug (negated provisional check) would return []
    // in the second call — this test catches it.
    let dag = chain_abc_a_completed();
    assert_eq!(dag.find_cutoff_eligible_speculative(&a, &hashset![]), vec![b.clone()]);
    assert_eq!(dag.find_cutoff_eligible_speculative(&b, &hashset![b.clone()]), vec![c.clone()]);
}
```

discovered_from=252. **Post-P0252-merge** (DONE).

### T69 — `test(dashboard):` Worker protocol (postMessage envelope + error-fallback-to-sync)

rev-p280 test-gap (informed, low severity). NEW (or MODIFY) `rio-dashboard/src/lib/__tests__/graphLayout.worker.test.ts`. The Worker protocol (`WorkerRequest`/`WorkerResponse` envelope at [`graphLayout.worker.ts:32-41`](../../rio-dashboard/src/lib/graphLayout.worker.ts) + error-fallback-to-sync at [`Graph.svelte:97-101`](../../rio-dashboard/src/pages/Graph.svelte)) is untested — acknowledged at `test:3-5` (jsdom Worker stub doesn't support module-type). `runLayout` body IS tested directly; the gap is postMessage wiring + the error path.

Two routes:
- **(a) Mock Worker:** vitest has `vi.stubGlobal('Worker', MockWorker)` where `MockWorker` synchronously invokes `onmessage` after `postMessage`. Tests the envelope shape (request → response round-trip) + error path (throw in onmessage → `catch` fires sync fallback).
- **(b) Playwright E2E:** real Worker in a real browser. Heavier; defer unless (a) proves insufficient.

Prefer **(a)**. The MockWorker can be `test-support/mock-worker.ts`:

```typescript
export class MockWorker {
  onmessage: ((ev: MessageEvent) => void) | null = null;
  postMessage(data: unknown) {
    // Echo the worker's computation synchronously — the real
    // graphLayout.worker.ts calls runLayout(data.nodes, data.edges)
    // then postMessage({positions}). Mock does the same inline.
    queueMicrotask(() => this.onmessage?.({ data: mockLayout(data) } as MessageEvent));
  }
  // ... terminate, addEventListener, etc.
}
```

**Post-P0280-merge.** discovered_from=280. Soft-dep [P0400](plan-0400-graph-page-skipped-worker-race.md) (that plan's inflight-gate fix changes the `layoutInWorker` calling code; T69's tests should reflect the post-fix shape).

### T70 — `test(vm):` observability PARENTING — observe-before-commit + kvm-pending entry

rev-p295 test-gap (proves-nothing axis-1 risk — committed specific outcome without observing it). [P0295](plan-0295-doc-rot-batch-sweep.md)-T63 committed a PARENTING result to [`observability.md:281`](../../docs/src/observability.md) spec text + [`observability.nix:362`](../../nix/tests/scenarios/observability.nix) assert, but zero `CONFIRMED: span_from_traceparent → PARENTING/LINK` hits across all 1278 rix-dev logs — the VM test never KVM-ran. Plan `:629` said conditional-on-one-KVM-run-first; impl reasoned from mechanism (`set_parent` before enter → lazy OTel span alloc sees stored parent context).

If mechanism-analysis is wrong, `vm-observability-standalone` fires RED at first KVM-capable run AND the spec text at `r[sched.trace.assignment-traceparent]` is wrong. This is T10/T13-T15-class: never-KVM-executed VM scenario with load-bearing assertion.

Two sub-tasks:

1. **kvm-pending.md entry:** MODIFY [`.claude/notes/kvm-pending.md`](../notes/kvm-pending.md) (from T16). Add `vm-observability-standalone` (or whichever attr runs `observability.nix`) with entry:
   ```
   - vm-observability-standalone — observability.nix:328-368 PARENTING observe-block.
     P0295-T63 committed spec text + assert based on mechanism-analysis (set_parent
     before enter → lazy OTel span alloc). Zero KVM observations. If wrong, assert
     at :362 fires wrong-direction AND spec text at r[sched.trace.assignment-traceparent]
     is wrong. r[verify sched.trace.assignment-traceparent].
   ```

2. **Conditional spec-text guard (if T63 dispatched before KVM run):** check whether the observe-only `print()` at [`observability.nix:357,363`](../../nix/tests/scenarios/observability.nix) was already converted to `assert overlap` (T63's change). If YES and KVM hasn't run, consider reverting the assert to `print()` + adding a `# TODO(P0311-T70): convert to assert after first KVM-observed CONFIRMED print` until the observation exists. Prevents a confidence-false assert from gating CI on first KVM allocation. If NO (T63 not yet dispatched), extend [P0295-T63's](plan-0295-doc-rot-batch-sweep.md) dispatch note with "gate on ≥1 KVM `CONFIRMED:` log before committing assert direction."

**Confirmatory-not-gating** same as T10/T13-T15. If vm-observability runs green with the committed assert direction, T70 closes as no-op (mechanism-analysis validated). discovered_from=295.

### T71 — `test(scheduler):` config_rejects_inf_backoff_multiplier — INFINITY explicit proof

rev-p415 test-gap at [`rio-scheduler/src/main.rs:1067`](../../rio-scheduler/src/main.rs). [P0415](plan-0415-retrypolicy-backoff-f64-bounds-complete.md)'s `config_rejects_*` tests cover `NaN` but not `f64::INFINITY`. The `is_finite()` check rejects both (empirically: `(1e308*2.0).is_finite() == false`) so the code IS sound — but no test proves inf-rejection.

Add explicit proof after the existing NaN tests (grep `config_rejects_nan_backoff` for anchor):

```rust
#[test]
fn config_rejects_inf_backoff_multiplier() {
    // is_finite() rejects both NaN AND ±INFINITY. P0415 tested NaN only;
    // this test proves the INFINITY arm explicitly (bughunt would otherwise
    // flag "inf never tested" despite same is_finite() branch covering it).
    let mut cfg = test_valid_config();
    cfg.retry_policy.backoff_multiplier = f64::INFINITY;
    let err = validate_config(&cfg).unwrap_err().to_string();
    assert!(
        err.contains("backoff_multiplier") && err.contains("finite"),
        "INFINITY must be rejected by is_finite(), got: {err}"
    );
}
```

One test covers all 3 `ensure`s (same `is_finite()` branch for `backoff_base`, `backoff_multiplier`, `backoff_max`) — add for `backoff_multiplier` as the representative. If implementer prefers exhaustive, mirror for all 3. discovered_from=415.

### T72 — `test(dashboard):` logStream push → LogViewer render — integration proves reactivity wired end-to-end

rev-p392 test-gap. [P0392](plan-0392-logstream-quadratic-virtualize.md)-T1 replaces spread-reassign with `lines.push()` (runes proxy tracks mutation) and T2 makes LogViewer's viewport `$derived` read `stream.lines.length`. These are tested INDEPENDENTLY (perf test drives push, render-count test stubs a static array) — but the WIRE between them (push → `.length` bump → `$derived` rerun → DOM slice update) has no integration test. A regression that breaks reactivity (e.g., accidentally wrapping `lines` in a non-proxied object, or the `$derived` reading a snapshot instead of `.length`) would pass both unit tests but break live-streaming render.

NEW/MODIFY [`rio-dashboard/src/components/__tests__/LogViewer.integration.test.ts`](../../rio-dashboard/src/components/__tests__/LogViewer.integration.test.ts) (or add to existing `LogViewer.test.ts`):

```ts
// r[verify dash.stream.log-tail]
// Integration: push into the REAL stream object (not a static mock)
// and assert the rendered slice updates. jsdom layout is zero, so
// the viewport $derived produces [0, OVERSCAN] — we don't test
// scroll arithmetic, only that push→length→derived→DOM is wired.
it('renders new lines as stream pushes (reactivity end-to-end)', async () => {
  // Use the REAL createLogStream but with a controllable mock for
  // admin.getBuildLogs that yields chunks on demand.
  const { pushChunk } = mockControllableLogStream();
  const { container } = render(LogViewer, { props: { buildId: 'b-live' } });

  expect(container.querySelectorAll('pre.line').length).toBe(0);

  pushChunk(['a', 'b', 'c']);
  await tick();  // Svelte flush
  expect(container.querySelectorAll('pre.line').length).toBe(3);

  pushChunk(['d', 'e']);
  await tick();
  expect(container.querySelectorAll('pre.line').length).toBe(5);
});
```

The `mockControllableLogStream` helper mocks `admin.getBuildLogs` to return an async-generator fed by `pushChunk()` — the REAL `createLogStream` drives it, so the push → reactivity chain is live. This catches the class of bug where `$state` proxy-tracking silently breaks under refactor. discovered_from=392. Post-P0392-merge. Soft-dep [P426](plan-426-logviewer-line-h-cap-spread-limits.md) (T2 changes push to loop-push — integration test still holds, only the per-line push timing differs; `await tick()` still batches to one render).

### T73 — `test(harness):` canonical_plan_id — direct unit tests for all branches

rev-p418 finding at [`.claude/lib/onibus/models.py:33-55`](../../.claude/lib/onibus/models.py). `canonical_plan_id` has 7 call sites + 2 `field_validator`s (`AgentRow`/`MergeQueueRow`) depending on it, but no direct unit test. Indirectly tested via `dag_flip_consumes_queue_row` + `agent_start` assertions, but the `field_validator` docs-branch passthrough (`_DOCS_BRANCH_RE`) has zero callers. Add to [`test_scripts.py`](../../.claude/lib/test_scripts.py):

```python
def test_canonical_plan_id_branches():
    # docs-XXXXXX passthrough
    assert canonical_plan_id("docs-330260") == "docs-330260"
    # int input path
    assert canonical_plan_id(414) == "P0414"
    # idempotency
    assert canonical_plan_id("P0414") == "P0414"
    # ValueError on bad input
    for bad in ("", "P-", "P123x"):
        with pytest.raises(ValueError):
            canonical_plan_id(bad)
```

discovered_from=418.

### T74 — `test(scheduler):` config_rejects_zero_cpu_limit_cores

rev-p424 finding at [`rio-scheduler/src/main.rs:1215`](../../rio-scheduler/src/main.rs). Commit `e11c4742` message says `{nan,neg,zero}`, [P0424](plan-0424-sizeclassconfig-cpu-limit-validation.md) plan table row 9 covers zero, code `> 0.0` rejects it, but only nan/neg tests exist. Add `config_rejects_zero_cpu_limit_cores` mirroring the existing neg test:

```rust
#[test]
fn config_rejects_zero_cpu_limit_cores() {
    let cfg = SizeClassConfig { cpu_limit_cores: Some(0.0), ..default() };
    assert!(validate_config(&cfg).is_err());
}
```

discovered_from=424. main.rs HOT count=41 — additive `cfg(test)` fn, non-overlapping.

### T75 — `test(test-support):` with_ed25519_key — migrate signing.rs tests OR drop dead API

rev-p304 finding at [`rio-test-support/src/pg.rs:465,473`](../../rio-test-support/src/pg.rs). `with_ed25519_key()` + `with_key_name()` builder methods added with zero callers. Doc-comment claims "most signing.rs tests" need them but `signing.rs` does not import `TenantSeed`. The `ed25519_seed.is_some()` branch at `pg.rs:501-513` never executes. **Route-(a):** migrate `signing.rs` tests to the builder (preferred — the API was built for them). **Route-(b):** drop the dead builder methods + the `:501-513` branch if migration doesn't fit. Check at dispatch which `signing.rs` tests currently hand-roll the key setup the builder was meant to replace. discovered_from=304.

### T76 — `test(controller):` RFC-1123 63-char name guard — exercise InvalidSpec branch

rev-p304 finding at [`rio-controller/src/reconcilers/workerpoolset/builders.rs:166-177`](../../rio-controller/src/reconcilers/workerpoolset/builders.rs). RFC-1123 63-char name guard added with no test. Existing 4 tests in `mod tests` cover happy-path + empty-image error only. Add a test with 40-char WPS name + 30-char class name (= 70+ chars combined) to exercise the `InvalidSpec` branch:

```rust
#[test]
fn rejects_name_exceeding_rfc1123_limit() {
    let wps_name = "a".repeat(40);
    let class_name = "b".repeat(30);
    let result = build_worker_pool_name(&wps_name, &class_name);
    assert!(matches!(result, Err(Error::InvalidSpec(_))));
}
```

discovered_from=304.

### T77 — `test(scheduler):` sched.merge.shared-priority-max — priority bump on shared-node merge

sprint-1 cleanup finding. `r[sched.merge.shared-priority-max]` at [`scheduler.md:166`](../../docs/src/components/scheduler.md) is `r[impl]`-annotated at [`merge.rs:3`](../../rio-scheduler/src/actor/merge.rs) but has NO `r[verify]` — `tracey query untested` surfaces it. The rule: when a higher-priority build merges a DAG that shares a node with an existing lower-priority build, the shared node's effective priority bumps to `max(old, new)`. No test asserts this.

NEW test in [`rio-scheduler/src/actor/tests/merge.rs`](../../rio-scheduler/src/actor/tests/merge.rs):

```rust
// r[verify sched.merge.shared-priority-max]
#[tokio::test]
async fn shared_node_priority_bumps_to_max() {
    let mut actor = test_actor().await;
    // Build A (priority=Low) submits drv X
    merge(&mut actor, build_a, PriorityClass::Low, &[drv_x()]).await;
    assert_eq!(actor.dag.node(drv_x_hash).priority, PriorityClass::Low);
    // Build B (priority=High) submits SAME drv X (shared node)
    merge(&mut actor, build_b, PriorityClass::High, &[drv_x()]).await;
    // Shared node bumped to max(Low, High) = High
    assert_eq!(actor.dag.node(drv_x_hash).priority, PriorityClass::High);
    // Inverse: Build C (priority=Low) does NOT bump High back down
    merge(&mut actor, build_c, PriorityClass::Low, &[drv_x()]).await;
    assert_eq!(actor.dag.node(drv_x_hash).priority, PriorityClass::High);
}
```

discovered_from=sprint-1-cleanup.

### T78 — `test(worker):` worker.executor.resolve-input-drvs — inputDrv→output-path resolution

sprint-1 cleanup finding. `r[worker.executor.resolve-input-drvs]` at [`worker.md:263`](../../docs/src/components/worker.md) is `r[impl]`-annotated at [`executor/mod.rs:740`](../../rio-worker/src/executor/mod.rs) but only the `fetch_drv_from_store` helper is unit-tested — the full `resolve_inputs` path (inputDrv spec → fetch .drv → filter outputs by name → collect into `resolved_input_srcs`) has no dedicated test. `tracey query untested` surfaces it.

NEW test in [`rio-worker/src/executor/tests.rs`](../../rio-worker/src/executor/tests.rs) (or inline `mod tests`):

```rust
// r[verify worker.executor.resolve-input-drvs]
#[tokio::test]
async fn resolve_inputs_maps_inputdrvs_to_output_paths() {
    // Mock store serving two .drv files:
    //   dep-a.drv → outputs: {out: /nix/store/aaa-dep-a}
    //   dep-b.drv → outputs: {out: /nix/store/bbb-dep-b, dev: /nix/store/ccc-dep-b-dev}
    let mock_store = MockStore::with_drvs(&[
        ("dep-a.drv", &[("out", "/nix/store/aaa-dep-a")]),
        ("dep-b.drv", &[("out", "/nix/store/bbb-dep-b"), ("dev", "/nix/store/ccc-dep-b-dev")]),
    ]);
    // Target drv with inputDrvs: {dep-a.drv: [out], dep-b.drv: [dev]}
    let drv = make_drv_with_input_drvs(&[
        ("dep-a.drv", &["out"]),
        ("dep-b.drv", &["dev"]),  // only dev, NOT out
    ]);
    let resolved = resolve_inputs(&mock_store.client(), &drv, "target.drv").await.unwrap();
    // Named outputs only: aaa (dep-a.out) + ccc (dep-b.dev), NOT bbb (dep-b.out unrequested)
    assert!(resolved.basic_derivation.input_srcs().contains("/nix/store/aaa-dep-a"));
    assert!(resolved.basic_derivation.input_srcs().contains("/nix/store/ccc-dep-b-dev"));
    assert!(!resolved.basic_derivation.input_srcs().contains("/nix/store/bbb-dep-b"));
}
```

The `names.contains(out.name())` filter at [`executor/mod.rs:777`](../../rio-worker/src/executor/mod.rs) is the load-bearing logic — test must assert both the positive (requested output included) AND negative (unrequested output excluded) cases. discovered_from=sprint-1-cleanup.

### T79 — `test(worker):` VM — per-build cgroup memory.max/cpu.max kernel enforcement

sprint-1 cleanup finding. [P0436](plan-0436-per-build-cgroup-limits.md) landed per-build cgroup `memory.max`/`cpu.max` limits at commit `2e5d8553`. Unit tests cover the `set_memory_max`/`set_cpu_max` API surface at [`rio-worker/src/cgroup.rs`](../../rio-worker/src/cgroup.rs), but there's no VM-level verification that the **kernel actually enforces** the limits — OOM-kills on `memory.max` breach, throttles on `cpu.max`.

MODIFY [`nix/tests/scenarios/`](../../nix/tests/) — add a cgroup-enforcement subtest (new scenario file or extend existing worker scenario):

```python
with subtest("cgroup memory.max: kernel OOM-kills on breach"):
    # Submit a build with memory.max=64MiB that allocates 128MiB.
    # Assert: build fails with OOM (exit 137 or dmesg shows oom-kill).
    worker.succeed("grep -q 'Memory cgroup out of memory' <(dmesg | tail -50)")

with subtest("cgroup cpu.max: kernel throttles on limit"):
    # Submit a CPU-bound build with cpu.max=100000 100000 (1 core).
    # Run for 2s wall-clock, assert cpu.stat shows throttled_usec > 0.
    worker.succeed("test $(cat /sys/fs/cgroup/rio-build-*/cpu.stat | grep throttled_usec | awk '{print $2}') -gt 0")
```

Add `# r[verify worker.cgroup.memory-max]` and `# r[verify worker.cgroup.cpu-max]` at the `subtests = [...]` entry in [`nix/tests/default.nix`](../../nix/tests/default.nix) per VM-test marker placement convention. discovered_from=P0436.


### T80 — `test(harness):` merge.py T6 collision assert — agreeing+disagreeing cases

[`merge.py:619`](../../.claude/lib/onibus/merge.py): the T6 collision assert has zero test coverage. No test exercises `_rewrite_t_placeholders` with two batch docs sharing a placeholder key. Add: (a) agreeing case — same key same value, assert skipped; (b) disagreeing case — same key different value, assert fires. The assert is defensive but shipped untested. discovered_from=306.

### T81 — `test(store):` sweep cross-batch-boundary cycle — >100 paths, cycle spans SWEEP_BATCH_SIZE

[`rio-store/src/gc/sweep.rs:635`](../../rio-store/src/gc/sweep.rs): `sweep_reclaims_two_cycle` tests 3 paths in one batch. Code comment at `:180-182` says cycles may span `SWEEP_BATCH_SIZE=100` boundaries — the `.bind(&unreachable)` (not `&batch`) is specifically for this. But no test has >100 paths with a cycle across the batch boundary. If someone changes `.bind(&unreachable)` to `.bind(batch)`, test still green. Add: 110-path sweep, cycle members at positions 50 and 105, assert both reclaimed. discovered_from=441. **Coordinate with [P0449](plan-0449-gc-sweep-temp-table-anti-join.md)** — that plan replaces the array-bind with a temp-table anti-join; this test should land against whichever impl is current.

### T82 — `test(store):` sweep external-referrer negative case — cycle survives when live path references member

[`rio-store/src/gc/sweep.rs:635`](../../rio-store/src/gc/sweep.rs): `sweep_reclaims_two_cycle` covers the positive case. Missing negative: A↔B cycle where live path D (NOT in unreachable) also references A — the `<> ALL($2)` filter must NOT mask D, and A+B must survive. More dangerous regression direction: over-exclusion = data loss. discovered_from=441.

### T83 — `test(scheduler):` CA-cutoff Skipped + recovery orphan upsert_path_tenants_for

[`rio-scheduler/src/actor/completion.rs:837`](../../rio-scheduler/src/actor/completion.rs): `upsert_path_tenants_for` (m039) — CA-cutoff Skipped path and recovery orphan-completion path ([`recovery.rs:661`](../../rio-scheduler/src/actor/recovery.rs)) have no regression tests. Only merge-cache-hit and merge-preexisting-Completed are tested. The two untested paths are precisely the ones the bug report flagged as missing upsert. Needs CA-cutoff harness + recovery harness setup. discovered_from=442.

### T84 — `test(scheduler):` CA-cutoff skipped_interested union — merged build with only cascade-Skipped node

[`rio-scheduler/src/actor/completion.rs:1156`](../../rio-scheduler/src/actor/completion.rs): `skipped_interested` HashSet fix in `handle_success_completion` (`:764-836`, consumed at `:1156-1169`) is untested. Both m052 tests ([`lifecycle_sweep.rs:248,331`](../../rio-scheduler/src/actor/tests/lifecycle_sweep.rs)) exercise FAILURE-cascade union, not CA-cutoff Skipped-union. A merged build whose only node is cascade-Skipped would hang Active under old code — no test proves it now completes. discovered_from=442.

### T85 — `test(store):` ManifestKind::total_size Chunked arm

[`rio-store/src/metadata/mod.rs:198`](../../rio-store/src/metadata/mod.rs): `ManifestKind::total_size` Chunked arm has no test. Integration test `test_get_path_size_mismatch_returns_data_loss` only exercises Inline. Chunked sum logic is trivial but untested. Add a unit test alongside, OR a chunked-store variant of the size-mismatch integration test. discovered_from=429.

### T86 — `test(coverage):` p425 coverage regression — investigate and backfill

Coverage regression post-p425 merge — see `/tmp/rio-dev/rio-sprint-1-merge-3.log`. Origin: coverage-pending sink. Check the log for which lines/branches regressed, add targeted tests. [P0425](plan-0425-ensure-required-helper-ten-site.md) was a 10-site helper extraction — likely the extracted helper itself is untested.

### T87 — `test(coverage):` p0441 coverage regression — investigate and backfill

Coverage regression post-p0441 merge — see `/tmp/rio-dev/rio-sprint-1-merge-24.log`. Origin: coverage-pending sink. [P0441](plan-0441-store-gc-drain-toctou-sweep-gaps.md) added sweep-gap fixes — check which branches are uncovered (likely the new TOCTOU guard paths or the `path_tenants` CASCADE).

### T88 — `test(scheduler):` FOD completion skips insert_build_sample — negative test

[`rio-scheduler/src/actor/completion.rs:996`](../../rio-scheduler/src/actor/completion.rs): FOD skip on `insert_build_sample` has no negative test. Positive path `test_completion_writes_build_sample` exists; the negative (FOD success completion → `COUNT(*) FROM build_samples == 0`) does not. FODs are fetchers-only per [ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md); they don't contribute to the build-duration distribution that `build_samples` feeds. Add `test_fod_completion_skips_build_sample` alongside the positive test, asserting the `is_fixed_output` guard holds. discovered_from=452.

### T89 — `test(scheduler):` fod_queue_depth + fetcher_utilization metric emission + NaN guard

[`rio-scheduler/src/actor/dispatch.rs:185`](../../rio-scheduler/src/actor/dispatch.rs): `rio_scheduler_fod_queue_depth` + `rio_scheduler_fetcher_utilization` emission has zero test assertions. Per observability checklist, metric emission needs a capture test. Critically: `fetcher_utilization = busy/total` must guard `total==0 → 0.0` (not NaN). Add a metric-capture test using `CountingRecorder` (from [`rio-test-support`](../../rio-test-support/src/metrics.rs)) that covers: (a) queue-depth gauge updates on FOD enqueue, (b) utilization gauge with `total>0`, (c) utilization = 0.0 when no fetchers registered (the NaN-guard path). discovered_from=452.

### T90 — `test(coverage):` investigate backgrounded .#coverage-full VM-test flakes (4 logs, 2 signatures)

Four `coverage-pending.jsonl` entries with `exit_code≠0` post-p452/p453/p455/p456 merges. Logs: `/tmp/rio-dev/rio-sprint-1-merge-{7,14,18,26}.log`. Two distinct failure signatures:

1. **protocol-* VM mid-test** (merge-7: `cov-vm-protocol-cold`, merge-14: `cov-vm-protocol-warm`) — VM test failed during run
2. **lifecycle-wps test-driver build** (merge-18 + merge-26: `nixos-test-driver-rio-lifecycle-wps`) — the test-driver derivation itself failed to build, both times

Signature 2 is likely a REAL bug (same derivation failing twice post-p453 is not host-IO). Signature 1 may be host-IO contention (same pattern as baseline CI investigation — many VM tests ran concurrently in both logs). Investigate:
- Re-run `.#cov-vm-lifecycle-wps-k3s` in isolation — if it builds clean, it's contention; if not, it's a real p453-introduced break
- Check if merge-7/14 protocol failures correlate with concurrent VM-test load
- If all are environmental: write a `.claude/known-flakes.jsonl` entry for the coverage-full VM-test parallelism ceiling
- If lifecycle-wps is real: open a targeted fix plan

This is T86/T87-class (investigate-then-route, not write-a-test-blind). discovered_from=coverage-sink.

### T91 — `test(vm):` netpol.nix — store-ingress cross-ns rules

[`networkpolicy.yaml:227`](../../infra/helm/rio-build/templates/networkpolicy.yaml): `store-ingress` allows from `rio-system` + `rio-builders` + `rio-fetchers` via three `namespaceSelector` blocks. [`netpol.nix`](../../nix/tests/scenarios/netpol.nix) currently probes only builder-egress (the nsenter pattern at `:11-17`). Add a `store-ingress` probe subtest: nsenter into a builder pod netns, `nc -z rio-store.rio-store.svc.cluster.local 9002` → MUST succeed (cross-ns allowed). Negative: nsenter into a pod in a fourth namespace (or host netns), same probe → MUST fail (policy denies non-listed ns). Add `# r[verify store.netpol.ingress-cross-ns]` at the `netpol` subtests wiring in [`nix/tests/default.nix`](../../nix/tests/default.nix) if a marker exists, otherwise this is behavioral coverage without a tracey ref. discovered_from=454.

### T92 — `test(coverage):` p454 coverage regression — investigate and backfill

`merge-34.log` `coverage-pending.jsonl` entry: `.#coverage-full` exit_code≠0 post-P0454 merge. Same T86/T87/T90 investigate-then-route shape. P0454 touched `xtask/src/k8s/` (status.rs, mod.rs, three provider deploys) + `infra/helm/` — helm changes don't affect coverage, xtask changes might if the VM-test fixture imports xtask helpers. Check: `grep 'xtask\|k8s::status' nix/tests/` — if zero hits, the regression is likely VM-test environmental (same as T90's signature-1). If non-zero, the xtask multi-ns refactor broke a fixture import. Re-run `.#coverage-full` in isolation; if green → `known-flakes.jsonl` entry; if red → targeted fix plan. discovered_from=coverage-sink.

### T93 — `test(vm):` lifecycle.nix — fetcher pod startup e2e

[`lifecycle.nix:2706`](../../nix/tests/scenarios/lifecycle.nix): no e2e coverage for FetcherPool pod startup. The fixture has builder-role nodes only. Extend [`fixtures/k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) with a second node carrying `rio.build/node-role=fetcher` label + `rio.build/fetcher=true:NoSchedule` taint (mirror the builder-node setup). Add a `lifecycle.nix` subtest: apply a `FetcherPool` CR, wait for `.status.readyReplicas >= 1`, assert the pod landed on the fetcher node (`kubectl get pod -o jsonpath='{.spec.nodeName}'` matches). This also exercises [P0459](plan-0459-adr019-netpol-scheduling-hardening.md)-T1's DS-scheduling fix (fetcher pod Pending without it). `lifecycle.nix` is count=23 HOT — append as a new subtest at tail, non-overlapping. discovered_from=bughunter.

### T94 — `test(helm):` devicePlugin+seccompInstaller nodeAffinity golden test

[`infra/helm/rio-build/templates/device-plugin.yaml:70`](../../infra/helm/rio-build/templates/device-plugin.yaml): the `devicePlugin.nodeAffinity` and `seccompInstaller.nodeAffinity` template paths render only in production — VM fixtures null them out via `vmtest-full-nonpriv.yaml`. No test asserts the rendered shape. Add a helm-template golden test to [`flake.nix`](../../flake.nix) `helm-lint` block:

```bash
rendered=$(helm template . --set devicePlugin.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution.nodeSelectorTerms[0].matchExpressions[0].key=rio.build/node-role)
echo "$rendered" | yq 'select(.kind=="DaemonSet" and .metadata.name=="rio-device-plugin").spec.template.spec.affinity.nodeAffinity' | grep -q 'rio.build/node-role'   || { echo "FAIL: devicePlugin nodeAffinity did not render"; exit 1; }
```

Same for `seccompInstaller`. discovered_from=459.

### T95 — `test(nix):` verify_sig cross-validation — sign via fingerprint(), verify via verify_sig()

[`rio-nix/src/narinfo.rs:1067`](../../rio-nix/src/narinfo.rs): existing `verify_sig` tests are self-referential — the test fixture builds the fingerprint string with the SAME format string the function-under-test uses. A format bug would pass both. Add a cross-validation test: sign via the free `fingerprint()` fn, verify via `verify_sig()`. If the two disagree on format, the cross-test fails. Add `// r[verify store.tenant.sign-key]` if the marker covers sig verification (check at dispatch). discovered_from=461.

### T96 — `test(store):` r[verify store.singleflight] — moka get_with coalescing

[`rio-store/src/substitute.rs:86`](../../rio-store/src/substitute.rs): `r[impl store.singleflight]` has no `r[verify]`. The `moka` cache's `get_with` coalescing (concurrent requests for same key → single upstream fetch) is not directly exercised. Add a test: spawn N concurrent tasks requesting the same path, assert the upstream mock was hit exactly once. Use `Arc<AtomicUsize>` counter on the mock. Add `// r[verify store.singleflight]` on the test. discovered_from=462.

### T97 — `test(vm):` rio-cli upstream subcommand — RPC dispatch, JSON output, e2e substitution

[`rio-cli/src/upstream.rs:86`](../../rio-cli/src/upstream.rs): the `upstream` subcommand has parse-tests only — no VM test exercises RPC dispatch, `--json` output formatting, table formatting, or the end-to-end substitution flow. Extend [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix) (or `substitute.nix` if the fixture setup overlaps) with:

```python
with subtest("rio-cli upstream add+list+remove roundtrip"):
    client.succeed("rio-cli upstream add --tenant t1 --url https://cache.nixos.org --trusted-key 'cache.nixos.org-1:...'")
    out = client.succeed("rio-cli --json upstream list --tenant t1")
    data = json.loads(out)
    assert any(u["url"] == "https://cache.nixos.org" for u in data["upstreams"])
    client.succeed("rio-cli upstream remove --tenant t1 --url https://cache.nixos.org")
```

Partners with [P0465](plan-0465-gateway-jwt-propagation-read-opcodes.md) T2's ssh-ng substitution test — that covers the protocol path, this covers the CLI surface. discovered_from=463.

### T98 — `test(coverage):` triage coverage-regression logs from merges 36/38/40/43/45/47

Six `coverage-pending.jsonl` entries from the sprint-1 merge stream: post-p459 (`merge-36`), post-p457 (`merge-38`), post-docs-692561 (`merge-40`, docs-only — likely baseline `test_concurrent_waiters` flake), post-p461 (`merge-43`), post-p462 (`merge-45`), post-p463 (`merge-47`). Logs at `/tmp/rio-dev/rio-sprint-1-merge-{36,38,40,43,45,47}.log`.

Same investigate-then-route shape as T86/T87/T90/T92. Most are likely the known `test_concurrent_waiters` baseline flake. For each log:
1. `grep 'FAIL\|panic\|error' <log>` — identify the failing test
2. If `test_concurrent_waiters` → known-flake, no action (already in `known-flakes.jsonl`)
3. If novel → re-run `.#coverage-full` in isolation; green → `known-flakes.jsonl` entry; red → file followup via `onibus state followup`

discovered_from=coverage-sink (6 entries collapsed).

### T99 — `test(store):` Substituter Err arms — test each error-to-None conversion

MODIFY [`rio-store/src/substitute.rs`](../../rio-store/src/substitute.rs). `try_substitute` at `:661-962` converts every `Err` → `Ok(None)` inside the moka `get_with` closure, so the outer `Result` is effectively dead — zero tests assert any Err arm. Add targeted tests for each swallowed error: HTTP non-2xx response, xz decompression failure, zstd decompression failure, unsupported compression variant, narinfo parse error, `HashMismatch`, `cas::put_chunked` failure + cleanup. Use a mock upstream HTTP server (wiremock or `rio-test-support`) to inject each failure mode; assert the Err→None conversion happens AND a `warn!` fires so the operator sees it. discovered_from=462 (bughunter).

### T100 — `test(helm):` builderpool hostUsers rio.optBool — helm-lint assert

MODIFY [`flake.nix`](../../flake.nix) helm-lint check. [P0468](plan-0468-helm-optbool-helper.md) added `rio.optBool` and applied it at [`builderpool.yaml:38`](../../infra/helm/rio-build/templates/builderpool.yaml), commit `16e7ab8c` added an assert for `hostUsers:false` rendering — but only for *one* template path. Add a parallel assert that covers `builderpool.yaml` specifically: `helm template --set builderPools[0].hostUsers=false` → rendered YAML contains `hostUsers: false` under the builderpool spec. discovered_from=468.

### T101 — `test(coverage):` triage coverage regression post-p464 merge

Coverage-pending entry post-p464 merge. Likely the vm-fetcher-split baseline flake that [P0466](plan-0466-fetcherpool-readonly-rootfs-emptydir.md) fixed. Re-verify: re-run `.#coverage-full` at current `sprint-1` HEAD; if green, close as resolved-by-P0466; if still red, investigate the specific test and file a followup. Same investigate-then-route shape as T86/T87/T90/T92/T98. discovered_from=coverage-sink.

### T102 — `test(coverage):` triage coverage regression post-docs-736477 merge

Coverage-pending entry post-docs-736477 merge (docs-only change). A docs-only merge shouldn't affect coverage — this is almost certainly the baseline `test_concurrent_waiters` flake (see P0304 T101). Re-verify: check the log for `test_concurrent_waiters`; if that's the failure, close as known-flake; if novel, investigate. discovered_from=coverage-sink.


### T487 — `test(ci):` grafana configmap.yaml drift check vs source JSONs

MODIFY [`flake.nix`](../../flake.nix) helm-lint check (at `:660`+). The `infra/helm/grafana/configmap.yaml` is generated from `infra/helm/grafana/*.json` dashboards, but nothing in CI catches drift if someone edits a JSON and forgets to regen. Add a drift assert: regen configmap.yaml in `$TMPDIR`, diff against committed. Pattern mirrors the CRD drift check at `:961+`:

```bash
# ── Grafana dashboard configmap drift ──────────────────────────
# configmap.yaml is generated from *.json. Editing a JSON without
# regen → deployed dashboards diverge from source.
(cd ${./infra/helm/grafana} && <regen-command>) > $TMPDIR/configmap-regen.yaml
diff ${./infra/helm/grafana/configmap.yaml} $TMPDIR/configmap-regen.yaml \
  || { echo "FAIL: configmap.yaml drift — run 'just grafana-configmap'" >&2; exit 1; }
```

Check `justfile` for the regen target name at dispatch (P0304-T3 adds `grafana-configmap`). discovered_from=458.

### T488 — `test(tooling):` record_green_ci_hash + ci_hash=None fallback — first test callers

MODIFY [`.claude/lib/test_scripts.py`](../lib/test_scripts.py). `record_green_ci_hash()` at [`merge.py:537`](../lib/onibus/merge.py) and the `ci_hash=None` pure-docs fallback at `:602` have zero test callers. Add:

- `test_record_green_writes_hash` — mock `_ci_drv_hash` to return a fixed string; call `record_green_ci_hash()`; assert `_LAST_GREEN_HASH` file content matches.
- `test_record_green_eval_failure` — mock `_ci_drv_hash` to return `None`; assert returns `None` and file NOT written.
- `test_clause4_pure_docs_fallback` — mock `_ci_drv_hash → None`, `_diff_files → ["docs/foo.md", ".claude/bar"]`; assert `decision == "SKIP"` with `reason` containing "pure-docs fallback".
- `test_clause4_pure_docs_negative` — mock `_diff_files → ["rio-store/src/foo.rs"]`; assert `decision == "RUN_FULL"`.

discovered_from=479.

### T489 — `test(controller):` build_executor_pod_spec read_only_root_fs volumes/mounts

MODIFY [`rio-controller/src/reconcilers/common/sts.rs`](../../rio-controller/src/reconcilers/common/sts.rs) `cfg(test)` mod (or `rio-controller/tests/`). `build_executor_pod_spec(.., read_only_root_fs: true)` at `:427-437` iterates `READ_ONLY_ROOT_MOUNTS` to add emptyDir volumes — but no unit test asserts the resulting volumes/mounts. Add:

- `test_readonly_root_adds_emptydir_volumes` — call with `read_only_root_fs: true`; assert `spec.volumes` contains one entry per `READ_ONLY_ROOT_MOUNTS` row with correct `name`, `medium`, `size_limit`.
- `test_readonly_root_false_skips_mounts` — call with `false`; assert none of the `READ_ONLY_ROOT_MOUNTS` names appear.

Paired with `r[verify sec.pod.readonly-root]` if the marker exists (grep at dispatch). discovered_from=467.

### T490 — `test(store):` Substituter::with_stale_threshold — first test caller

MODIFY [`rio-store/src/substitute.rs`](../../rio-store/src/substitute.rs) `cfg(test)` mod. `with_stale_threshold()` at `:179` is a builder method with zero test callers. Add:

- `test_with_stale_threshold_overrides_default` — construct `Substituter::new(...).with_stale_threshold(Duration::from_secs(1))`; assert `self.stale_threshold == 1s` (may need a `#[cfg(test)]` getter or make the field `pub(crate)`).
- Optional integration: in the existing `stale-reclaim` test, use `with_stale_threshold(Duration::from_millis(100))` instead of whatever cfg(test) const override exists — makes the builder the test-knob, not a cfg gate.

discovered_from=483.

### T491 — `test(tooling):` build.coverage rc≠0 → coverage_maybe_halt integration path

MODIFY [`.claude/lib/test_scripts.py`](../lib/test_scripts.py). [`build.py:124-128`](../lib/onibus/build.py) wires `coverage()` → `coverage_maybe_halt()` on `r.rc != 0`. `test_coverage_full_red_heuristic` (`:894`) tests `coverage_maybe_halt` in isolation; nothing tests the WIRING. A refactor that dropped the call (or the late-import — see [P0304](plan-0304-trivial-batch-p0222-harness.md) T492) would pass CI but silently break queue-halt on infrastructure-red. Add:

- `test_coverage_rc_nonzero_invokes_halt` — monkeypatch `build.run()` to return `rc=1` with a log containing ≥3 scenario-fail lines; call `build.coverage(...)`; assert `queue-halted` file written AND `coverage-pending.jsonl` has the row.
- `test_coverage_rc_zero_skips_halt` — `rc=0`; assert NO `queue-halted`, but `coverage-pending.jsonl` still written.

discovered_from=484.

### T492 — `test(vm):` psa-restricted subtest — add rio-dashboard to depl loop

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) at the PSA-restricted subtest P0460 T4 adds. The `for depl, ns_name in [...]` loop covers scheduler/gateway/controller/store but omits `rio-dashboard` ([`dashboard.yaml:35-36`](../../infra/helm/rio-build/templates/dashboard.yaml) sets `runAsNonRoot: true` inline — it runs in `rio-system` and is subject to PSA `restricted`). Dashboard uses UID 65534 (nginx-unprivileged nobody), not 65532 (distroless nonroot) — the loop needs per-deployment expected-UID:

```python
for depl, ns_name, expected_uid in [
    ("rio-scheduler", ns_system, 65532),
    ("rio-gateway", ns_system, 65532),
    ("rio-controller", ns_system, 65532),
    ("rio-store", ns_store, 65532),
    ("rio-dashboard", ns_system, 65534),
]:
    ...
    assert sc["runAsUser"] == expected_uid, ...
```

Depends on P0460 merging first (subtest doesn't exist yet). discovered_from=460.

### T493 — `test(helm):` rio.podSecurityContext coverage-mode self-guard — positive assert

MODIFY [`flake.nix:660`](../../flake.nix) `helm-lint` check. The `rio.podSecurityContext` helper (P0460 adds to [`_helpers.tpl`](../../infra/helm/rio-build/templates/_helpers.tpl)) self-guards on `.Values.coverage.enabled` — when coverage mode is on, it skips `runAsNonRoot` (coverage profraws need hostPath writes that fail as nonroot). That self-guard is NOT positively tested: nothing renders with `coverage.enabled=true` and asserts the securityContext is ABSENT. Add:

```bash
helm template rio . -f values/vmtest-full.yaml --set coverage.enabled=true \
  | yq 'select(.kind == "Deployment" and (.metadata.name | test("^rio-")))' \
  | yq '.spec.template.spec.securityContext.runAsNonRoot' \
  | { ! grep -q true; }  # zero runAsNonRoot=true when coverage on
```

Depends on P0460 merging first (helper doesn't exist yet). discovered_from=460.

### T984472801 — `test(helm):` helm-lint max-coverage render — all default-off features enabled together

MODIFY [`flake.nix:681`](../../flake.nix) `helm-lint` check. Current renders cover default, dev, kind, vmtest-full, and dash-on individually. MISSING: a "max-coverage" render with ALL default-off features enabled simultaneously. [P0460](plan-0460-psa-restricted-control-plane.md)'s bootstrap-job template slipped because it's only rendered when `bootstrap.enabled=true` AND no lint pass toggles that. Next default-off template slips the same way.

Add after the dash-on render at `:689-693`:

```bash
# Max-coverage: every default-off toggle ON. Catches templates that
# only render under optional flags (bootstrap-job slipped P0460 this way).
helm template rio . \
  --set global.image.tag=test \
  --set dashboard.enabled=true \
  --set bootstrap.enabled=true \
  --set coverage.enabled=true \
  --set postgresql.enabled=true \
  --set rustfs.enabled=true \
  > /tmp/max-coverage.yaml
# Basic sanity: renders without error, has at least one of each expected kind.
for kind in Deployment Job ConfigMap Service; do
  grep -q "^kind: $kind$" /tmp/max-coverage.yaml || { echo "missing $kind"; exit 1; }
done
```

The toggle list is best-effort — grep `values.yaml` for `enabled: false` defaults at dispatch to catch new ones added since this was written. Depends on [P0493](plan-0493-bootstrap-job-psa-restricted.md) merging first (bootstrap-job exists). discovered_from=493.

### T984472802 — `test(tooling):` clause4_check crash-path test

MODIFY [`.claude/lib/test_scripts.py`](../lib/test_scripts.py) after the `_clause4_fixture` helper at `:962`. All four existing clause4 tests at `:959-1076` are happy-path (SKIP/RUN_FULL/HALT verdicts from valid inputs). Add a crash-path test: monkeypatch `_ci_drv_hash` (or `merge.clause4_check` directly) to raise, assert the CLI dispatch degrades to `RUN_FULL` not a traceback.

**CLOSED BY** [P984472802](plan-984472802-merger-clause4-crash-handling.md) T3 — that plan ships the try/except wrap AND this test together. If P984472802 dispatches, this T-item is a no-op (mark done-via-P984472802). If this dispatches first (unlikely — P984472802 has the fix, this is just the test-gap tracker), write the test against CURRENT behavior (crash propagates) with `@pytest.mark.xfail(reason="P984472802")`. discovered_from=488.

## Exit criteria

- `/nbr .#ci` green
- T1: `cli builds --json` on populated state → JSON with ≥1 build, `build_id` + `status` fields present
- T2: `cli workers --status alive` → all returned workers have `status=="alive"`; `--status draining` → empty when none draining
- T3: mock-stream dirty-close unit test → stderr contains "closed without is_complete"
- T4: `recovery_row_initializes_failure_count_from_failed_workers` passes; asserts `failure_count == failed_workers.len()`
- T5: `grep 'dash.graph.degrade-threshold' .claude/work/plan-0276*.md` → ≥1 hit
- `nix develop -c tracey query rule sched.poison.ttl-persist` shows a `verify` site (T4)
- `nix develop -c tracey query rule sched.admin.list-workers` — check if T2 should add a `verify` annotation (the status_filter is spec'd)
- `grep -c 'self.type in\|has(self.localhostProfile)' rio-controller/src/crds/workerpool.rs` ≥ 4 (T6: 2 attrs + 2 asserts; post-P0223-merge)
- `grep 'five.*x_kube\|5.*x_kube' rio-controller/src/crds/workerpool.rs` → ≥1 hit (T6: comment updated from "three" to "five")
- `cargo nextest run -p rio-controller build_seccomp_profile_unconfined` → passes (T7)
- `nix develop -c tracey query rule worker.seccomp.localhost-profile` shows ≥2 verify sites (T6+T7)
- `cargo nextest run -p rio-gateway ca_register_2deep_parses_and_roundtrips ca_register_historical_has_nonempty_deps ca_query_2deep_drvoutput_ids` → 3 passed (T8)
- `grep -c 'include_bytes.*corpus' rio-gateway/tests/golden/ca_corpus.rs` → 3 (T8: all three .bin files consumed)
- **T8 roundtrip self-check:** the `assert_eq!(out, REGISTER_2DEEP)` is the load-bearing assertion — proves rio's wire serializer produces byte-identical output to Nix's golden fixtures. If padding or key-ordering differs, this catches it.
- `nix develop -c pytest .claude/lib/test_scripts.py -k 'mitigation_landed_sha or mitigation_appends'` → 2 passed (T9)
- **T9 precondition self-check:** `test_flake_mitigation_appends_and_preserves_header` asserts `len(rows) == 1` BEFORE indexing `rows[0].mitigations` — a broken rewrite that drops the row fails on the length check, not an IndexError
- T10: `/nixbuild .#checks.x86_64-linux.vm-fod-proxy-k3s` → green on a KVM-capable builder (P0308 T3 `elapsed < 45s` assertion verified end-to-end). OR: documented as "still pending KVM fleet" if allocation keeps landing on denied hosts (confirmatory, not gating — see T10 risk profile)
- **T11 precondition: [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) resolved** — one of Route-1/2/3 determined. Exit criteria below are for Route-1/2; Route-3 = task OBE.
- T11 Route-1/2: scheduling.nix subtest `per-build timeout` passes with `8 < elapsed < 20` (timeout fires ~10s, well before 60s sleep)
- T11: `grep 'rio_scheduler_build_timeouts_total 1' <metrics-scrape>` → match (proves worker.rs:593 fired — not a different failure path)
- T11: `nix develop -c tracey query rule sched.timeout.per-build` shows ≥1 `verify` site (T11 adds the first — currently impl-only)
- T12: `cargo nextest run -p rio-scheduler test_class_drift_fires_without_penalty` → pass
- T12 precondition self-check: the `!recorder.all_keys().is_empty()` assert is load-bearing — it proves the `set_default_local_recorder` guard scoped correctly BEFORE the main drift assertion. A guard-scope bug would make both `get()` calls return 0 and the negative-assert pass vacuously.
- T12 mutation check: flip `!=` to `==` at [`completion.rs:388`](../../rio-scheduler/src/actor/completion.rs) → test fails (drift fires on NON-drift → `get(...)` returns 0 for the real-drift key). Proves the test catches the inversion.
- T12 decoupling proof: BOTH the drift-fires AND penalty-does-not asserts pass — proves the window between cutoff and 2×cutoff is real, not collapsed
- T13/T14: `/nixbuild .#checks.x86_64-linux.vm-scheduling-disrupt-standalone` → green on KVM-capable builder. OR: documented in `.claude/notes/kvm-pending.md` as "still pending KVM fleet". If green: `cancel-timing` cgroup-gone-in-5s holds, `load-50drv` 50-leaf fanout completes within 900s, `setoptions-unreachable` journal grep confirms no wopSetOptions on wire
- T15: `/nixbuild .#checks.x86_64-linux.vm-netpol-k3s` → green on KVM-capable builder. `netpol-positive` subtest passing = self-validation precondition holds (allowed egress works — test not vacuous). OR: documented in `.claude/notes/kvm-pending.md`
- T16: `.claude/notes/kvm-pending.md` exists with ≥3 entries (vm-fod-proxy-k3s, vm-scheduling-disrupt-standalone, vm-netpol-k3s) — OR is empty/deleted if all three ran green during this plan's dispatch
- T17: `cargo nextest run -p rio-store gt13_batch_oversize_failed_precondition` → pass
- T17 precondition self-check: the `out1_nar.len() >= INLINE_THRESHOLD` assert is load-bearing — if `make_nar`'s overhead changes and the NAR becomes <256 KiB, the test would pass for the wrong reason (no FailedPrecondition, batch just succeeds). The precondition assert catches that.
- T18: `cargo nextest run -p rio-store gt13_batch_already_complete_per_output` → pass
- T18: the `resp.created == vec![false, true]` assert is the load-bearing assertion — proves per-output idempotency flag (not just "batch succeeded")
- T19: `cargo nextest run -p rio-worker test_upload_all_outputs_batch_fallthrough_on_precondition` → pass
- T19: `grep 'fail_batch_precondition' rio-test-support/src/grpc.rs` → ≥2 hits (field decl + check in put_path_batch handler)
- T19: `logs_contain("falling back to independent PutPath")` → match (proves the :565 warning fired — the fallthrough ran, not some other success path)
- `nix develop -c tracey query rule store.atomic.multi-output` shows ≥3 `verify` sites after T17+T18 (original `gt13_batch_rpc_atomic` + T17 oversize + T18 idempotency)
- `nix develop -c tracey query rule worker.upload.multi-output` shows ≥1 `verify` site (T19)
- T20: `nix develop -c pytest .claude/lib/test_scripts.py -k 'happy_path_single_match'` → 1 passed
- T20: `grep -c 'rc == 0\|header preserved' .claude/lib/test_scripts.py` → ≥2 (T20's assertions present)
- T21: `cargo nextest run -p rio-store test_put_chunk_no_tenant_fail_closed` → pass (post-P0264-merge)
- T21: `grep -c 'fail_closed\|fail-closed' rio-store/tests/grpc/chunk_service.rs` → ≥2 (PutChunk + FindMissingChunks symmetric)
- T22: `cargo nextest run -p rio-scheduler test_progress_arm_ema_counter_fires` → pass (post-P0266-merge)
- T22: `grep 'ema_proactive_updates_total' rio-scheduler/src/grpc/tests.rs` → ≥1 (counter asserted)
- T23: `cargo nextest run -p rio-store batch_outputs_signed_with_tenant_key` → pass (post-P0338-merge)
- `nix develop -c tracey query rule store.tenant.sign-key` shows ≥2 `verify` sites (existing single-path + T23's batch)
- T24: `cargo nextest run -p rio-gateway auth_publickey_mints_jwt auth_publickey_rejects auth_publickey_degrades` → 3 passed (post-P0260-merge)
- T24: `grep 'resolve_tenant_calls\|MockScheduler.*resolve_tenant' rio-gateway/src/server.rs rio-gateway/tests/` → ≥1 hit (MockScheduler fields used)
- `nix develop -c tracey query rule gw.jwt.issue` shows ≥1 `verify` site (T24's mints_jwt test)
- `nix develop -c tracey query rule gw.jwt.dual-mode` shows ≥2 `verify` sites (T24's reject + degrade tests; plus P0260's VM fallback)
- T25: `cargo nextest run -p rio-controller spawn_count_table` → pass (post-P0296-merge)
- T25: `grep 'fn spawn_count\|pub fn spawn_count' rio-controller/src/reconcilers/workerpool/ephemeral.rs` → ≥1 hit (extracted as free fn)
- T26: `cargo nextest run -p rio-store put_path_corrupt_tenant_seed_falls_back_to_cluster` → pass (post-P0338-merge)
- T26 load-bearing assert: `cluster_pk.verify(fp.as_bytes(), &sig).is_ok()` — proves fallback fired (cluster key signed), not tenant-key-partial-success
- `nix develop -c tracey query rule store.tenant.sign-key` shows ≥3 `verify` sites (existing single-path + T23 batch + T26 fallback)
- T27: security.nix `conn_cap` subtest passes — 3rd SSH at cap=2 gets disconnect with "too many"/"connection cap" in output (post-P0213-merge)
- T27: `nix develop -c tracey query rule gw.conn.cap` shows ≥1 `verify` site (T27 is the first VM-level verify)
- T28: `cargo nextest run -p rio-gateway auth_publickey_resolve_timeout_degrades auth_publickey_unparseable_tenant_id_degrades auth_publickey_rejected_jwt_counter_fires auth_publickey_mint_degraded_counter_fires` → 4 passed (post-P0260-merge; extends T24's 3)
- T28: `grep -c 'rejected_jwt\|mint_degraded_total' rio-gateway/src/server.rs` → ≥4 (emit-sites + test asserts)
- `nix develop -c tracey query rule gw.jwt.dual-mode` shows ≥4 `verify` sites (T24's 2 + T28's timeout+unparseable)
- T29: `cargo nextest run -p rio-common load_and_wire_jwt_some_path_loads_and_spawns load_and_wire_jwt_none_path_returns_inert` → 2 passed (post-P0355-merge — DONE)
- T29: `grep 'load_and_wire_jwt' rio-common/src/jwt_interceptor.rs | grep -c 'fn\|#\[tokio::test\]'` → ≥3 (1 def + 2 tests)
- T30: `grep 'passthrough ON should bypass\|fallback_reads_total.*<= 2\|fallback_reads_total.*≤ 2' nix/tests/scenarios/scheduling.nix` → ≥1 hit (positive assert added)
- T30: `grep '^# r\[verify worker.fuse.passthrough\]' nix/tests/scenarios/scheduling.nix nix/tests/default.nix` → ≥1 hit (col-0 marker per P0341 convention)
- T30: `grep 'r\[verify worker.fuse.passthrough\]' rio-worker/src/fuse/mod.rs` → 0 hits (stub annotation removed — VM test owns the verify now)
- T31: `cargo nextest run -p rio-store tenant_quota_by_name_cases` → pass (post-P0255-merge — DONE)
- T31: `nix develop -c tracey query rule store.gc.tenant-quota-enforce` shows ≥2 `verify` sites (gateway unit + T31's store-side test)
- T32: `grep 'jwt-mount-present\|wm2wmwssi1' .claude/notes/kvm-pending.md` → ≥1 hit (T16's manifest extended with P0357 entry)
- T32: `/nixbuild .#checks.x86_64-linux.vm-lifecycle-k3s` → `jwt-mount-present` subtest passes on KVM-capable builder. OR: documented in `kvm-pending.md` (confirmatory-not-gating, same as T10/T15)
- T33: `grep -c 'all_histograms_have_bucket_config' rio-controller/tests/metrics_registered.rs rio-store/tests/metrics_registered.rs rio-gateway/tests/metrics_registered.rs` → 3 (T33: test in each crate's metrics_registered)
- T33: `cargo nextest run all_histograms_have_bucket_config` → ≥4 passed (scheduler's existing + 3 new, or 4 new if worker included)
- T33 mutation check: add a `describe_histogram!("rio_store_fake_count", ...)` to store's describe_metrics → store's test fails with `"no HISTOGRAM_BUCKET_MAP entry"`. Remove → passes.
- T34: `cargo nextest run -p rio-store batch_outputs_signed_with_tenant_key_post_hoist` → pass (post-P0352-merge)
- T34: `grep 'r\[verify store.tenant.sign-key\]' rio-store/tests/grpc/signing.rs | wc -l` → ≥3 (T23 + T26 + T34)
- T35: `grep 'put_path_duration_seconds\|opcode_duration_seconds' rio-common/src/observability.rs | grep -c HISTOGRAM_BUCKET_MAP` — both metrics present in the map (grep shows ≥2 entry-line hits)
- T35: `grep 'NAR_LATENCY_BUCKETS\|nar_latency' rio-common/src/observability.rs` → ≥1 hit (bucket const defined)
- T35: `grep 'put_path_duration_seconds\|opcode_duration_seconds' rio-store/tests/metrics_registered.rs rio-gateway/tests/metrics_registered.rs | grep DEFAULT_BUCKETS_OK` → 0 hits (removed from exempt list)
- T35: `grep 'put_path_duration_seconds\|opcode_duration_seconds' docs/src/observability.md | grep 'default buckets'` → 0 hits (stale :208 sentence updated)
- T36: `cargo nextest run -p rio-scheduler spawn_task_exits_on_shutdown` → pass
- T36: `grep 'r\[verify sched.rebalancer.sita-e\]' rio-scheduler/src/rebalancer/tests.rs` → ≥2 hits (existing :18 + T36's test)
- T37: `grep 'vm-security-nonpriv-k3s\|bughunt-mc147' .claude/notes/kvm-pending.md` → ≥1 hit (T16's manifest extended)
- T37: `/nixbuild .#checks.x86_64-linux.vm-security-nonpriv-k3s` → green on KVM-capable builder. OR: documented in kvm-pending.md (confirmatory-not-gating)
- T38: `grep 'GetSizeClassStatus\|sched.admin.sizeclass-status' nix/tests/scenarios/scheduling.nix` → ≥1 hit (subtest present, post-P0231-merge)
- T38: `grep '^# r\[verify sched.admin.sizeclass-status\]' nix/tests/default.nix nix/tests/scenarios/scheduling.nix` → ≥1 hit (col-0 marker per P0341 convention)
- T39: `cargo nextest run -p rio-common expiry_boundary_at_the_second_is_valid` → pass
- T39: `grep 'verify_at\b' rio-common/src/hmac.rs` → ≥2 hits (helper + delegate)
- T39 mutation check: flip `>` to `>=` at [`hmac.rs:218`](../../rio-common/src/hmac.rs) → `expiry_boundary_at_the_second_is_valid` fails (proves test catches the MISSED mutant)
- T40: `cargo nextest run -p rio-controller build_class_statuses_maps_rpc_to_status` → pass (post-P0234-merge)
- T40: `grep 'r\[verify ctrl.wps.cutoff-status\]' rio-controller/src/reconcilers/workerpoolset/` → ≥1 hit (T40's verify annotation)
- T40 load-bearing: the `s.name != "nonexistent"` assert proves build_class_statuses iterates `spec.classes` not RPC classes (would leak nonexistent classes into WPS status otherwise)
- T41: `cargo nextest run -p rio-controller scale_wps_class_patches_child_sts_replicas` → pass (post-P0234-merge)
- T41: the `Some(4)` replica-count assert is load-bearing — proves compute_desired arithmetic correct for the happy path (queued=20/target=5=4)
- T42: `cargo nextest run -p rio-cli cutoffs_table_renders_nonempty` → pass (post-P0236-merge)
- T42: `grep 'render_cutoffs_table\|fn render_cutoffs' rio-cli/src/cutoffs.rs` → ≥1 hit (pure-fn extracted for testability, if not already)
- T43: cli.nix cutoffs subtest passes — `cutoffs --json | jq '.classes | length'` ≥ 1 (post-P0236-merge, conditional on scheduler.toml [[size_classes]])
- `nix develop -c tracey query rule sched.admin.sizeclass-status` shows ≥2 verify sites (T38's VM grpcurl + T42's cli unit + T43's cli.nix — T42+T43 partner with T38)
- `nix develop -c tracey query rule ctrl.wps.cutoff-status` shows ≥2 verify sites (P0234-T3's managedFields + T40's status aggregation)
- `nix develop -c tracey query rule ctrl.wps.autoscale` shows ≥3 verify sites (P0234-T3 + T41 happy-path + P0374-T3 name-collision)
- T44: `nix develop -c pnpm --filter rio-dashboard test -- BuildDrawer` → ≥3 passed (tab-switch, backdrop-close, onclose-optional)
- T45: `nix develop -c pnpm --filter rio-dashboard test -- GC` → passed including `gc stream error surfaces toast.error` + `gc cancel swallows error` (post-P0281)
- T46: `nix develop -c pnpm --filter rio-dashboard test -- GC` → passed including `gc unmount aborts in-flight stream` (post-P0281)
- T47: `nix develop -c pnpm --filter rio-dashboard test -- Workers` → passed including `workers poll interval cleared on unmount` + `workers fetch-error renders role=alert` (post-P0281)
- T47 fake-timer proof: vi.useFakeTimers, mount, advance 5001ms, assert listWorkers called 2×, unmount, advance another 5001ms, assert still 2× (interval cleared — NOT 3×)
- T48: `nix build .#checks.x86_64-linux.helm-lint` → green (cardinality assert passes today with 11==11)
- T48 mutation check: add `rpc TestFake(...)` to `admin.proto` inside `service AdminService {}` → helm-lint fails with "AdminService has 12 RPCs, GRPCRoute has 11 method matches" → remove → passes
- T49: `grep '"rio-cli wps list+describe"' nix/tests/scenarios/cli.nix` → ≥1 hit (subtest present; post-P0237)
- T49: `/nixbuild .#checks.x86_64-linux.vm-cli-k3s` (or whichever drv runs cli.nix) → green including wps subtest — OR documented in kvm-pending.md (confirmatory-not-gating, same as T43)
- T50: `cargo nextest run -p rio-controller prune_stale_children` → ≥2 passed (deletes-removed-class + tolerates-404; post-P0374)
- T50: `grep 'r\[verify ctrl.wps.prune-stale\]' rio-controller/src/reconcilers/workerpoolset/` → ≥1 hit (T50's verify annotation in the new tests.rs or appended to mod.rs tests)
- T51: `grep 'select(. != null)' flake.nix` → ≥1 hit in helm-lint yq block (null-filter guard; post-P0379)
- T51: `grep 'expected >=.*third-party images' flake.nix` → ≥1 hit (minimum-count floor-check prevents silent-false-green when yq path breaks)
- T51 mutation: temporarily change `containers` to `containerz` (typo) in a rendered manifest → helm-lint MUST fail on floor-check (count drops below 2) → revert → passes
- T52: `grep 'DISPATCH NOTE.*sched_grpc\|do NOT inline.*grpcurl' .claude/work/plan-0239-*.md` → ≥1 hit (pre-emptive consolidation guard in P0239's doc; pre-P0239-dispatch)
- T53: `grep 'pdb-ownerref\|wps-lifecycle' .claude/notes/kvm-pending.md` → ≥1 hit (T16's manifest extended with P0239 entry); `grep 'OR.*kvm-pending' .claude/work/plan-0239-*.md` → ≥1 hit (confirmatory-not-gating clause added)
- T54: `cargo nextest run -p rio-scheduler ca_cutoff_compare_slow_store` → 1 passed; mutation check: remove `tokio::time::timeout(` wrapper at completion.rs:309 → test hangs/fails (proves timeout is load-bearing) → revert → passes (post-P0251)
- T55: `grep 'NOT persisted\|restart.*reset.*false' rio-scheduler/src/state/derivation.rs` → ≥1 hit (Option-A doc-comment OR Option-B persist-path comment); `cargo nextest run -p rio-scheduler ca_output_unchanged_resets_on_recovery` → 1 passed (post-P0251)
- T56: `grep 'vm-lifecycle-wps-k3s\|pdb-ownerref.*wps-lifecycle' .claude/notes/kvm-pending.md` → ≥1 hit (PARTNERS WITH T53 — if T53 landed the entry, T56 is no-op/review)
- T57: `grep 'rio_scheduler_warm_gate_fallback_total' rio-scheduler/src/assignment.rs | grep -c assert` → ≥1 (counter asserted in fallback test)
- T57: `cargo nextest run -p rio-scheduler warm_gate_fallback_when_no_warm_workers` → pass with counter assert
- T58: `cargo nextest run -p rio-scheduler on_worker_registered_sends_initial_hint on_worker_registered_empty_queue on_worker_registered_send_fail` → 3 passed
- T58: `nix develop -c tracey query rule sched.assign.warm-gate` shows ≥4 verify sites (T57's fallback-counter + T58's three + existing)
- T59: `cargo nextest run -p rio-worker prefetch_hint_joiner_reports_accurate_counts prefetch_hint_joiner_sink_closed` → 2 passed
- T60: `pnpm --filter rio-dashboard test -- LogViewer` → ≥3 passed (error/empty/spinner; post-P0279-merge)
- T61: `cargo nextest run -p rio-scheduler ca_compare_zero_outputs ca_compare_malformed ca_compare_rpc_err` → 3 passed
- T61: `nix develop -c tracey query rule sched.ca.cutoff-compare` shows ≥3 verify sites (P0251's existing + T54 timeout + T61's edge-cases)
- T66: `cargo nextest run -p rio-controller ephemeral_deadline_some_propagates_to_job_spec` → 1 passed (Some(7200) → active_deadline_seconds=Some(7200))
- T67: `cargo nextest run -p rio-scheduler cascade_only_skips_verified_candidates` → 1 passed; mutation check: `|_| true` at completion.rs:467 → test fails (proves wiring is load-bearing)
- T68: `cargo nextest run -p rio-scheduler speculative_provisional_skipped_makes_parent_eligible` → 1 passed (non-empty provisional_skipped path covered)
- T69: `pnpm --filter rio-dashboard test -- graphLayout.worker` → ≥2 passed (postMessage envelope round-trip + error-fallback-to-sync; post-P0280-merge)
- T70: `grep 'vm-observability\|PARENTING observe-block' .claude/notes/kvm-pending.md` → ≥1 hit (kvm-pending entry added)
- T70 (conditional): `/nixbuild .#checks.x86_64-linux.vm-observability-standalone` → green on KVM-capable builder AND log contains `CONFIRMED: span_from_traceparent →` line. OR: documented in kvm-pending.md (confirmatory-not-gating, same as T10/T13-T15/T32/T37)
- T70 observed-direction check: if the `CONFIRMED:` line says `PARENTING`, the P0295-T63 spec-text commit stands; if `LINK only`, spec text at r[sched.trace.assignment-traceparent] needs correction (observability.md:281 "produces parent-child" → "produces a link"). Either way, convert observability.nix:356-368 `print()` → `assert` matching observed direction
- T71: `cargo nextest run -p rio-scheduler config_rejects_inf_backoff_multiplier` → 1 passed; `grep 'f64::INFINITY\|INFINITY must be rejected' rio-scheduler/src/main.rs` → ≥2 hits (test body + assert msg)
- T72: `pnpm --filter rio-dashboard test -- LogViewer` → passes including push-to-render integration test; `grep 'pushChunk\|reactivity end-to-end\|mockControllableLogStream' rio-dashboard/src/components/__tests__/` → ≥2 hits (helper + test body)
- T73: `pytest .claude/lib/test_scripts.py::test_canonical_plan_id_branches` → passes (docs-passthrough, int, idempotent, ValueError cases)
- T74: `cargo nextest run -p rio-scheduler config_rejects_zero_cpu_limit_cores` → 1 passed
- T75: `grep 'with_ed25519_key\|TenantSeed' rio-store/src/signing.rs` → route-(a): ≥1 hit (migrated); OR route-(b): `grep 'with_ed25519_key' rio-test-support/src/pg.rs` → 0 hits (dead API dropped)
- T76: `cargo nextest run -p rio-controller rejects_name_exceeding_rfc1123_limit` → 1 passed
- T77: `cargo nextest run -p rio-scheduler shared_node_priority_bumps_to_max` → 1 passed; `nix develop -c tracey query rule sched.merge.shared-priority-max` shows ≥1 `verify` site
- T78: `cargo nextest run -p rio-worker resolve_inputs_maps_inputdrvs_to_output_paths` → 1 passed; `nix develop -c tracey query rule worker.executor.resolve-input-drvs` shows ≥1 `verify` site
- T80: `pytest .claude/lib/test_scripts.py -k rewrite_t_placeholders` → ≥2 passed (agreeing+disagreeing)
- T81: `cargo nextest run -p rio-store sweep_cross_batch_cycle` → passed (>100 paths, cycle spans boundary)
- T82: `cargo nextest run -p rio-store sweep_cycle_survives_external_referrer` → passed
- T83: `cargo nextest run -p rio-scheduler ca_cutoff_skipped_upserts_path_tenants recovery_orphan_upserts_path_tenants` → 2 passed
- T84: `cargo nextest run -p rio-scheduler merged_build_only_skipped_completes` → passed
- T85: `cargo nextest run -p rio-store manifest_kind_total_size_chunked` → passed
- T88: `cargo nextest run -p rio-scheduler test_fod_completion_skips_build_sample` → passed
- T89: `cargo nextest run -p rio-scheduler fod_queue_depth_gauge_updates fetcher_utilization_nan_guard` → ≥2 passed
- T90: `.#cov-vm-lifecycle-wps-k3s` re-run outcome documented — if green: `grep 'lifecycle-wps\|coverage-full parallelism' .claude/known-flakes.jsonl` → ≥1 hit; if red: followup plan filed via `onibus state followup`

- T91: `grep 'store-ingress\|rio-store.rio-store.svc' nix/tests/scenarios/netpol.nix` → ≥2 hits (probe added)
- T92: `.#coverage-full` re-run outcome documented — green → `known-flakes.jsonl` entry; red → followup plan filed
- T93: `grep 'FetcherPool\|readyReplicas' nix/tests/scenarios/lifecycle.nix` → ≥2 hits; `grep 'rio.build/node-role.*fetcher' nix/tests/fixtures/k3s-full.nix` → ≥1 hit

- T94: `helm template . --set devicePlugin.nodeAffinity...` renders nodeAffinity block; helm-lint assert present in flake.nix
- T95: `cargo nextest run -p rio-nix verify_sig_cross_validation` → passed
- T96: `cargo nextest run -p rio-store singleflight_coalesces_concurrent` → passed; `nix develop -c tracey query rule store.singleflight` shows ≥1 `verify` site
- T97: `cli.nix` (or `substitute.nix`) has `upstream add+list+remove` subtest; `/nixbuild .#checks.x86_64-linux.vm-cli-k3s` green
- T98: 6 merge logs triaged; outcomes documented (known-flake / novel followup)
- T99: ≥6 new tests in `rio-store/tests/substitute_errors.rs` (or inline) — one per Err arm; each asserts `warn!` fired
- T100: `helm template --set builderPools[0].hostUsers=false | grep 'hostUsers: false'` → match; flake.nix helm-lint assert present
- T101: post-p464 coverage regression resolved (green re-run OR followup filed)
- T102: post-docs-736477 coverage regression resolved (known-flake confirmed OR novel followup filed)
- T487: `nix build .#checks.x86_64-linux.helm-lint` green with configmap drift assert; manual JSON edit without regen → FAIL
- T488: `pytest .claude/lib/test_scripts.py -k 'record_green or pure_docs'` → ≥4 tests green
- T489: `cargo nextest run -p rio-controller readonly_root` → ≥2 tests green; READ_ONLY_ROOT_MOUNTS entries asserted
- T490: `cargo nextest run -p rio-store with_stale_threshold` → ≥1 test green; builder method covered
- T491: `pytest .claude/lib/test_scripts.py -k 'coverage_rc'` → ≥2 tests green; drop the `coverage_maybe_halt` call → test fails
- T492: security.nix psa-restricted subtest loop includes `("rio-dashboard", ..., 65534)`; `/nixbuild .#checks.x86_64-linux.vm-security-k3s` green
- T493: `nix build .#checks.x86_64-linux.helm-lint` green; manually set coverage.enabled=true + add runAsNonRoot to one deployment → helm-lint FAILS
- T984472801: `nix build .#checks.x86_64-linux.helm-lint` renders max-coverage profile; comment out bootstrap-job template → helm-lint still green (proves the render REACHES it: re-add broken syntax → FAILS)
- T984472802: `pytest .claude/lib/test_scripts.py -k clause4_crash` → ≥1 test (green if P984472802 landed, xfail otherwise)

## Tracey

References existing markers:
- `r[sched.admin.list-workers]` — T2 verifies (status_filter behavior at [`scheduler.md:129`](../../docs/src/components/scheduler.md))
- `r[sched.poison.ttl-persist]` — T4 verifies (recovery restores poison state including failure_count at [`scheduler.md:108`](../../docs/src/components/scheduler.md))
- `r[dash.graph.degrade-threshold]` — T5 annotation guidance (P0276 implements server-half)

- `r[worker.seccomp.localhost-profile]` — T6 verifies (CEL rules guard the Localhost-coupling spec'd at [`security.md:55`](../../docs/src/security.md)), T7 verifies (Unconfined arm coverage)
- `r[sec.psa.control-plane-restricted]` — T492 verifies (dashboard included in PSA enforcement loop at [`security.md:79`](../../docs/src/security.md)); T493 verifies (coverage-mode self-guard)

- `r[sched.timeout.per-build]` — T11 verifies (the first `r[verify]` for this marker; currently [`worker.rs:570`](../../rio-scheduler/src/actor/worker.rs) has `r[impl]` only)
- `r[obs.metric.scheduler]` — T12 verifies (behavioral coverage for `class_drift_total` — the [`metrics_registered.rs:49`](../../rio-scheduler/tests/metrics_registered.rs) name-check under the same marker doesn't prove the emit-site fires correctly)
- `r[gw.opcode.set-options.propagation+2]` — T14's VM run is the first EXECUTION of this marker's `r[verify]` annotation at [`scheduling.nix:777`](../../nix/tests/scenarios/scheduling.nix). tracey already sees it (nix/tests/*.nix is in test_include); running it proves the journal-grep assertion actually holds

- `r[store.atomic.multi-output]` — T17 verifies (FailedPrecondition cleanup), T18 verifies (per-output idempotency flag within batch)
- `r[store.put.idempotent]` — T18 verifies (already-complete path inside batch → `created=false`)
- `r[worker.upload.multi-output]` — T19 verifies (fallthrough on FailedPrecondition → independent PutPath per [`worker.md:224`](../../docs/src/components/worker.md); atomicity lost per the spec's "register each output path" independent-upload step)
- `r[sec.boundary.grpc-hmac]` — T21 verifies (PutChunk fail-closed; the `require_tenant` check is tenant-scoped HMAC-adjacent auth — uses `x-test-tenant-id` metadata header, not the assignment token, but enforces the same fail-closed contract)
- `r[obs.metric.scheduler]` — T22 verifies (counter fires on Progress arm wiring)
- `r[store.tenant.sign-key]` — T23 verifies (batch outputs signed with tenant key not cluster)
- `r[gw.jwt.issue]` — T24 verifies (auth_publickey mints JWT on resolve success)
- `r[gw.jwt.dual-mode]` — T24 verifies (reject on UNAUTHENTICATED, degrade on UNAVAILABLE — both halves of dual-mode)
- `r[ctrl.pool.ephemeral]` — T25 verifies (spawn_count arithmetic table-driven test)
- `r[store.tenant.sign-key]` — T26 verifies (corrupt-seed fallback → cluster key → upload succeeds; third `verify` site for this marker after T23 batch + the existing single-path)
- `r[gw.conn.cap]` — T27 verifies (first VM-level verify for this marker; unit tests cover Semaphore primitive only at [`gateway.md:697`](../../docs/src/components/gateway.md))
- `r[gw.jwt.dual-mode]` — T28 verifies (timeout + unparseable paths — extends T24's reject/degrade coverage)
- `r[obs.metric.gateway]` — T28 verifies (rejected_jwt + mint_degraded_total counter emissions)
- `r[gw.jwt.dual-mode]` — T29 verifies (load_and_wire_jwt Some/None paths — the helper carries `r[impl gw.jwt.dual-mode]` at [`jwt_interceptor.rs:189`](../../rio-common/src/jwt_interceptor.rs); T29's tests are unit-level verify for the None→inert/Some→active split)
- `r[dash.auth.method-gate]` — T48 verifies (cardinality assert proves GRPCRoute stays in sync with AdminService proto)
- `r[ctrl.wps.prune-stale]` — T50 verifies (direct prune flow test: active_classes membership + deletionTimestamp skip + 404-tolerance; the existing scaling.rs:1633 `is_wps_owned_by_matches_uid_not_just_kind` tests the UID filter, T50 tests the prune FLOW around it)
- `r[worker.fuse.passthrough]` — T30 verifies (first NON-STUB verify — passthrough ON → fallback_reads stays ≤2 on wsmall1; removes marker from #[ignore]'d Rust stub at [`fuse/mod.rs:286`](../../rio-worker/src/fuse/mod.rs))
- `r[store.gc.tenant-quota-enforce]` — T31 verifies (tenant_quota_by_name 3-case unit — second `verify` site, store-side backing for the gateway's quota gate)
- `r[sec.jwt.pubkey-mount]` — T32 is the first VM-level EXECUTION of this marker's `r[verify]` annotation (helm-lint yq asserts cover template-renders; VM proves pod-Running→mount-works). tracey already sees it at the `default.nix` subtests entry (P0341 convention)
- `r[obs.metric.controller]`, `r[obs.metric.store]`, `r[obs.metric.gateway]` — T33 adds `r[verify]` annotations on the bucket-coverage tests (parity with scheduler's existing one at [`metrics_registered.rs:91`](../../rio-scheduler/tests/metrics_registered.rs))
- `r[store.tenant.sign-key]` — T34 verifies (third `verify` site — batch post-hoist resolve_once-once-not-N shape, extends T23's pre-hoist coverage)
- `r[sched.rebalancer.sita-e]` — T36 verifies (spawn_task shutdown — biased; priority regression guard)
- `r[sec.pod.fuse-device-plugin]` + `r[sec.pod.host-users-false]` — T37 is the first KVM-execution of these markers' `r[verify]` annotations at [`default.nix:310`](../../nix/tests/default.nix) / [`security.nix:1059`](../../nix/tests/scenarios/security.nix). tracey sees them (nix/tests/default.nix is scanned); running proves device-plugin + cgroup-remount paths work.
- `r[sched.admin.sizeclass-status]` — T38 verifies (first VM-level verify; [P0295](plan-0295-doc-rot-batch-sweep.md)-T66 adds the marker; T38 adds the `r[verify]` annotation)
- `r[sec.boundary.grpc-hmac]` — T39 verifies (expiry boundary — `>` vs `>=` at the second; first boundary-precision verify for this marker at [`security.md:29`](../../docs/src/security.md))
- `r[ctrl.wps.cutoff-status]` — T40 verifies (build_class_statuses maps RPC→status correctly; second verify after P0234-T3 at [`controller.md:126`](../../docs/src/components/controller.md))
- `r[ctrl.wps.autoscale]` — T41 verifies (happy-path replica patch — partners with P0234-T3 managedFields + P0374-T3 name-collision skip for full coverage triad)
- `r[sched.admin.sizeclass-status]` — T42 verifies (cli unit — table render), T43 verifies (cli.nix end-to-end). Partner to T38's grpcurl subtest; three layers: RPC direct (T38), CLI unit (T42), CLI VM (T43).

No new markers. T1/T3 test cli output formatting and stream-handling — no corresponding spec markers exist (cli output format is not spec'd). T8 tests wire-format parse-roundtrip — the corpus bytes are Nix's golden fixtures, not a rio spec contract; no `r[gw.*]` marker for Realisation wire-format specifically (only the per-opcode markers, which cover behavior not byte-shape). T9/T20 test harness CLI + pydantic pattern — not spec'd. T52/T53 are plan-doc dispatch-notes + kvm-pending manifest entries — no tracey markers (operational tracking, not spec behavior). T54/T55 both verify `r[sched.ca.cutoff-compare]` (timeout-guard + restart-recovery edges of the compare path spec'd at [`scheduler.md:260`](../../docs/src/components/scheduler.md)).

- `r[sched.assign.warm-gate]` — T57 verifies (fallback counter increment), T58 verifies (initial-hint ordering × 3 paths), T59 verifies (joiner counts + sink-closed)
- `r[obs.metric.scheduler]` — T57 verifies (fallback_total emission)
- `r[sched.ca.cutoff-compare]` — T61 verifies (zero-outputs/malformed/Err edge paths — partners with T54's timeout edge)
- `r[dash.stream.log-tail]` — T60 verifies (error-alert/empty-state/spinner — component-render coverage partnering with P0279's store test)
- `r[sched.ca.resolve]` — T62 verifies (golden-value); T63 verifies (gate-path edges)
- `r[sched.assign.best-worker]` — T64 verifies (scoring arithmetic table-driven; mutation-caught)
- `r[sched.state.machine]` — T64 verifies (validate_transition exhaustive)
- `r[ctrl.pool.ephemeral]` — T66 verifies (ephemeral_deadline_seconds Some-branch propagation — extends T25's arithmetic coverage with the non-default deadline path)
- `r[sched.ca.cutoff-propagate]` — T67 verifies (verify-closure wiring — `|h| verified.contains(h)` mutation-kill), T68 verifies (speculative provisional-skipped OR-branch)
- `r[sched.trace.assignment-traceparent]` — T70 is the first KVM-execution of this marker's observe-block at [`observability.nix:328-368`](../../nix/tests/scenarios/observability.nix). Spec text at [`observability.md:279`](../../docs/src/observability.md) hedges "produces parent-child or a link depending on enter-time resolution"; T70 resolves which.
- `r[sched.merge.shared-priority-max]` — T77 verifies (first `r[verify]` for this marker; [`merge.rs:3`](../../rio-scheduler/src/actor/merge.rs) has `r[impl]` only)
- `r[worker.executor.resolve-input-drvs]` — T78 verifies (first `r[verify]` for this marker; [`executor/mod.rs:740`](../../rio-worker/src/executor/mod.rs) has `r[impl]` only)
- `r[sched.dispatch.fod-to-fetcher]` — T88 verifies (FOD completion path — build_samples skip is the metrics-side of the hard-split)
- `r[obs.metric.scheduler]` — T89 verifies (fod_queue_depth + fetcher_utilization gauges; first `r[verify]` for these two specific metrics)

- `r[fetcher.node.dedicated]` — T93 verifies (fetcher pod lands on dedicated-taint node; first `r[verify]` for this marker)
- `r[ctrl.fetcherpool.reconcile]` — T93 verifies (FetcherPool CR → STS → readyReplicas)

## Files

```json files
[
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T1+T2: populated builds/workers assertions (OR lifecycle.nix if MockAdmin can't populate)"},
  {"path": "rio-cli/tests/smoke.rs", "action": "MODIFY", "note": "T3: gc dirty-close warning unit test (preferred over VM — reliable mock-stream)"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T4: failure_count recovery-init test (or in rio-scheduler/tests/)"},
  {"path": ".claude/work/plan-0276-getbuildgraph-rpc-pg-backed.md", "action": "MODIFY", "note": "T5: add r[dash.graph.degrade-threshold] server-half line to Tracey section"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "T6: cel_rules_in_schema +2 seccomp asserts + comment three→five (p223 :443-459) — post-P0223-merge"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T7: build_seccomp_profile_unconfined test (~10 lines, near p223 :702) — post-P0223-merge"},
  {"path": "rio-gateway/tests/golden/ca_corpus.rs", "action": "NEW", "note": "T8: static parse-roundtrip for 3 corpus .bin files (include_bytes + wire primitives + serde_json assertions)"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "MODIFY", "note": "T8: add `mod ca_corpus;` declaration"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T9: +test_mitigation_landed_sha_pattern + test_flake_mitigation_appends_and_preserves_header (near existing flake test :1063)"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T10: NO CODE CHANGE — reminder to run .#checks.x86_64-linux.vm-fod-proxy-k3s on KVM-capable builder, verify P0308 T3 elapsed<45s assertion"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T11: per-build-timeout subtest (TAIL append) — BLOCKED on P0329; Route-1 ssh-ng --option build-timeout OR Route-2 grpcurl SubmitBuild; r[verify sched.timeout.per-build]"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T12: test_class_drift_fires_without_penalty after :834 — 40s@30s-cutoff (drift-without-penalty window); CountingRecorder + precondition self-check; r[verify obs.metric.scheduler]"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T13/T14: NO CODE CHANGE — reminder to run vm-scheduling-disrupt-standalone on KVM (cancel-timing :866, load-50drv :1036, setoptions-unreachable :811 never executed)"},
  {"path": "nix/tests/scenarios/netpol.nix", "action": "MODIFY", "note": "T15: NO CODE CHANGE — reminder to run vm-netpol-k3s on KVM (P0241's FIRST-build, netpol-positive :125 proves-nothing self-check never verified)"},
  {"path": ".claude/notes/kvm-pending.md", "action": "NEW", "note": "T16: manifest of never-KVM-executed VM attrs (vm-fod-proxy-k3s, vm-scheduling-disrupt-standalone, vm-netpol-k3s) — coordinator consults at KVM-slot-open"},
  {"path": "rio-store/tests/grpc/chunked.rs", "action": "MODIFY", "note": "T17: gt13_batch_oversize_failed_precondition (256KiB+ → FailedPrecondition + placeholder cleanup); T18: gt13_batch_already_complete_per_output (pre-seeded complete → created[0]=false). Both after :377; r[verify store.atomic.multi-output] + r[verify store.put.idempotent]"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T19: test_upload_all_outputs_batch_fallthrough_on_precondition in tests mod near :1004; r[verify worker.upload.multi-output]"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T19: MockStore +fail_batch_precondition: Arc<AtomicBool> field at :48 + Default impl + check at top of put_path_batch :209"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T20: +test_flake_mitigation_happy_path_single_match near T9's tests ~:1063"},
  {"path": "rio-store/tests/grpc/chunk_service.rs", "action": "MODIFY", "note": "T21: +test_put_chunk_no_tenant_fail_closed near existing find_missing fail-closed (p264 ref :751); r[verify sec.boundary.grpc-hmac]"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T22: +test_progress_arm_ema_counter_fires (CountingRecorder + Progress msg); r[verify obs.metric.scheduler]"},
  {"path": "rio-store/tests/grpc/signing.rs", "action": "MODIFY", "note": "T23: +batch_outputs_signed_with_tenant_key (2-output batch, tenant-key sig verify); r[verify store.tenant.sign-key]"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T24: +3 auth_publickey tests (mints/rejects/degrades) using MockScheduler.resolve_tenant_uuid (p260 refs :574,:808); r[verify gw.jwt.issue] + r[verify gw.jwt.dual-mode]"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T25: extract spawn_count(queued,active,ceiling)→u32 + spawn_count_table test (6 cases); r[verify ctrl.pool.ephemeral] (p296 ref :176)"},
  {"path": "rio-store/tests/grpc/signing.rs", "action": "MODIFY", "note": "T26: +put_path_corrupt_tenant_seed_falls_back_to_cluster after ~:350 (16-byte seed → InvariantViolation → maybe_sign warn+cluster fallback → upload OK); r[verify store.tenant.sign-key]"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T27: conn_cap subtest — max_connections=2 drop-in, 3rd SSH gets disconnect; r[verify gw.conn.cap] (post-P0213-merge)"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T28: +4 tests extending T24 — resolve_timeout_degrades, unparseable_tenant_id_degrades, rejected_jwt_counter_fires, mint_degraded_counter_fires (post-P0260-merge)"},
  {"path": "rio-common/src/jwt_interceptor.rs", "action": "MODIFY", "note": "T29: +load_and_wire_jwt_some_path_loads_and_spawns + _none_path_returns_inert tests near :542; r[verify gw.jwt.dual-mode]"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T30: sizeclass subtest :220-225 +assert_metric_le(wsmall1, fallback_reads_total, 2) — passthrough ON positive; col-0 r[verify worker.fuse.passthrough]"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "T30: strip r[verify worker.fuse.passthrough] from :286 #[ignore] stub — VM test owns verify now"},
  {"path": "rio-store/src/gc/tenant.rs", "action": "MODIFY", "note": "T31: +tenant_quota_by_name_cases test (3-case: unknown/limit/null-limit); r[verify store.gc.tenant-quota-enforce]"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T32: +vm-lifecycle-jwt-mount-present entry (P0357 subtest, drv wm2wmwssi1 never built)"},
  {"path": "rio-controller/tests/metrics_registered.rs", "action": "MODIFY", "note": "T33: +all_histograms_have_bucket_config test; r[verify obs.metric.controller]"},
  {"path": "rio-store/tests/metrics_registered.rs", "action": "MODIFY", "note": "T33: +all_histograms_have_bucket_config test (T35 REFINES: do NOT exempt put_path_duration — it gets a NAR_LATENCY_BUCKETS entry instead); r[verify obs.metric.store]"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T33: +all_histograms_have_bucket_config test (T35 REFINES: do NOT exempt opcode_duration — it gets a NAR_LATENCY_BUCKETS entry instead); r[verify obs.metric.gateway]"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "MODIFY", "note": "T33: +all_histograms_have_bucket_config test IF P0363 hasn't dispatched (exempt fuse_fetch_duration_seconds); r[verify obs.metric.worker]"},
  {"path": "rio-store/tests/grpc/signing.rs", "action": "MODIFY", "note": "T34: +batch_outputs_signed_with_tenant_key_post_hoist (3-output batch, all tenant-key-signed, resolve_once-once shape); r[verify store.tenant.sign-key]"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "T35: +NAR_LATENCY_BUCKETS const + 2 HISTOGRAM_BUCKET_MAP entries (put_path_duration, opcode_duration — both exceed 10s on large NARs)"},
  {"path": "rio-store/tests/metrics_registered.rs", "action": "MODIFY", "note": "T35: DEFAULT_BUCKETS_OK — remove put_path_duration_seconds (now in map)"},
  {"path": "rio-gateway/tests/metrics_registered.rs", "action": "MODIFY", "note": "T35: DEFAULT_BUCKETS_OK — remove opcode_duration_seconds (now in map)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T35: :208 'not listed here' example list — drop put_path/opcode; Histogram Buckets table +2 rows"},
  {"path": "rio-scheduler/src/rebalancer/tests.rs", "action": "MODIFY", "note": "T36: +spawn_task_exits_on_shutdown test (biased; priority, token.cancel, timeout-bound); r[verify sched.rebalancer.sita-e]"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T37: +vm-security-nonpriv-k3s entry (bughunt-mc147 — device-plugin+hostUsers:false never KVM-ran)"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T38: +GetSizeClassStatus grpcurl subtest after sched_grpc block; r[verify sched.admin.sizeclass-status] (post-P0231-merge)"},
  {"path": "rio-common/src/hmac.rs", "action": "MODIFY", "note": "T39: +expiry_boundary_at_the_second_is_valid test + cfg(test) verify_at(token, now_unix) helper; r[verify sec.boundary.grpc-hmac]"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/mod.rs", "action": "MODIFY", "note": "T40: +build_class_statuses_maps_rpc_to_status test (iterates spec.classes not RPC); r[verify ctrl.wps.cutoff-status] (post-P0234)"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T41: +scale_wps_class_patches_child_sts_replicas happy-path test; r[verify ctrl.wps.autoscale] (post-P0234)"},
  {"path": "rio-cli/src/cutoffs.rs", "action": "MODIFY", "note": "T42: extract render_cutoffs_table pure fn + cutoffs_table_renders_nonempty test; r[verify sched.admin.sizeclass-status] (post-P0236)"},
  {"path": "rio-cli/tests/smoke.rs", "action": "MODIFY", "note": "T42 alt: cutoffs_table_renders_nonempty test alongside T3's dirty-close test"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T43: +cutoffs subtest (non-empty table + --json) — extends T1/T2's populated-state assertions; r[verify sched.admin.sizeclass-status] (post-P0236)"},
  {"path": "rio-dashboard/src/components/__tests__/BuildDrawer.test.ts", "action": "NEW", "note": "T44: tab-switch + backdrop-close + onclose-optional tests (extend if P0278-T4 already created; post-P0278)"},
  {"path": "rio-dashboard/src/pages/__tests__/GC.test.ts", "action": "MODIFY", "note": "T45: stream-error surfaces toast.error (non-abort throw); cancel swallows error (abort-initiated). T46: unmount-aborts-in-flight via $effect teardown (post-P0281)"},
  {"path": "rio-dashboard/src/pages/__tests__/Workers.test.ts", "action": "MODIFY", "note": "T47: poll-interval cleared on unmount (fake-timers); fetch-error renders role=alert (post-P0281)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T48: helm-lint +proto↔GRPCRoute cardinality assert after ~:594 (ADMIN_RPCS awk-count == ROUTE_METHODS yq-count)"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T49: +wps list+describe subtest (kube-only, ApiServerVerifier pattern or real k3s; post-P0237)"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/tests.rs", "action": "NEW", "note": "T50: +prune_stale_children_deletes_removed_class + tolerates_404 (ApiServerVerifier; new tests.rs or append to mod.rs tests; post-P0374)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T51: helm-lint yq thirdparty loop — +select(.!=null) filter + >=2 count floor-check (prevents silent-false-green on broken yq path; post-P0379)"},
  {"path": ".claude/work/plan-0239-vm-pdb-section-h-wps.md", "action": "MODIFY", "note": "T52: +DISPATCH NOTE (use sched_grpc/submit_build_grpc, no 5th inline grpcurl helper). T53: +OR-documented-in-kvm-pending exit-criterion clause (confirmatory-not-gating)"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T53: +vm-lifecycle pdb-ownerref + wps-lifecycle entry (P0239 fragments — r[verify ctrl.wps.reconcile/autoscale]; pre-P0239-merge)"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T54: +ca_cutoff_compare_slow_store_doesnt_block_completion (pending-future MockStore, tokio start_paused, advance past DEFAULT_GRPC_TIMEOUT). T55: +ca_output_unchanged_resets_on_recovery (recovery→false default; post-P0251)"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T55: ca_output_unchanged field doc-comment — NOT-persisted restart-semantics (Option-A) OR persist-path (Option-B; migration + persist_status touch if P0252 defers propagation)"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T56: PARTNERS WITH T53 — verify vm-lifecycle-wps-k3s entry lists BOTH fragments + both r[verify] markers; no-op if T53 already complete"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T57: :596-617 warm_gate_fallback +CountingRecorder +assert fallback_total==1"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "T58: +3 on_worker_registered tests (merge-then-connect, empty-queue-flip, send-fail-flip) near :449"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "T59: tests mod near :844 — +prefetch_hint_joiner_reports_accurate_counts + _sink_closed_is_debug_not_error"},
  {"path": "rio-dashboard/src/components/__tests__/LogViewer.test.ts", "action": "NEW", "note": "T60: error-alert/empty-state/spinner component tests (NEW or extend P0279-T4's file; post-P0279-merge)"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T61: +3 CA-compare edge tests (zero-outputs, malformed-hash, rpc-err); r[verify sched.ca.cutoff-compare]; partners with T54 timeout"},
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "MODIFY", "note": "T62: +placeholder_golden_matches_nix_upstream test near :594 (Nix known-answer g1w7hy3q...-foo.drv+out→/0c6rn30q...). discovered_from=253"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T63: +5 maybe_resolve_ca gate tests (ia-passthrough, fod-passthrough, empty-content, no-ca-inputs, resolve-error-swallow). discovered_from=253"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "T64: +best_worker_scoring_table test (48 candidate mutations — table-driven exact-score asserts). discovered_from=373"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T64: +validate_transition_exhaustive test (30 candidate mutations — cartesian product Ok/Err). discovered_from=373"},
  {"path": "rio-store/src/manifest.rs", "action": "MODIFY", "note": "T65: +manifest_chunk_boundary_arithmetic test (13 candidate mutations — ±1 boundary round-trip, P0373-T2 pattern). discovered_from=373"},
  {"path": "rio-controller/src/reconcilers/workerpool/ephemeral.rs", "action": "MODIFY", "note": "T66: +ephemeral_deadline_some_propagates_to_job_spec test near :394 (Some(7200)→active_deadline_seconds=Some(7200); complements job_spec_load_bearing_fields None-default). discovered_from=347"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T67: +cascade_only_skips_verified_candidates (MockStore seed B-output-only; A ca_unchanged → B Skipped, C NOT Skipped; mutation |_| true → fail). discovered_from=252"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "T68: +speculative_provisional_skipped_makes_parent_eligible (non-empty provisional_skipped set — :534 OR-branch direct coverage). discovered_from=252"},
  {"path": "rio-dashboard/src/lib/__tests__/graphLayout.worker.test.ts", "action": "NEW", "note": "T69: MockWorker (vi.stubGlobal) → postMessage envelope round-trip + error-fallback-to-sync (post-P0280-merge). discovered_from=280"},
  {"path": "rio-dashboard/src/test-support/mock-worker.ts", "action": "NEW", "note": "T69: MockWorker class for vitest Worker stubbing (post-P0280-merge). discovered_from=280"},
  {"path": ".claude/notes/kvm-pending.md", "action": "MODIFY", "note": "T70: +vm-observability-standalone entry (observability.nix PARENTING observe-block never KVM-ran; P0295-T63 committed spec+assert from mechanism-analysis). discovered_from=295"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T70 (conditional — if T63 dispatched pre-KVM): :356-368 observe-only print kept OR revert-assert+TODO until first CONFIRMED observation. No-op if KVM-run confirms mechanism-analysis. discovered_from=295"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T71: +config_rejects_inf_backoff_multiplier after existing NaN tests (post-P0415 ~:1067). discovered_from=415. HOT count=38 — additive test-fn"},
  {"path": "rio-dashboard/src/components/__tests__/LogViewer.test.ts", "action": "MODIFY", "note": "T72: +push-to-render integration test (mockControllableLogStream → pushChunk → tick → assert DOM updates). Proves $state proxy .push() → .length → $derived chain live. discovered_from=392. Post-P0392-merge; r[verify dash.stream.log-tail]"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T73: +test_canonical_plan_id_branches (docs-passthrough/int/idempotent/ValueError). discovered_from=418"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T74: +config_rejects_zero_cpu_limit_cores test. discovered_from=424. HOT count=41 — additive cfg(test)"},
  {"path": "rio-test-support/src/pg.rs", "action": "MODIFY", "note": "T75: route-(a) no-change OR route-(b) drop with_ed25519_key+with_key_name+:501-513 branch. discovered_from=304"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T75 route-(a): migrate tests to TenantSeed builder. discovered_from=304"},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "MODIFY", "note": "T76: +rejects_name_exceeding_rfc1123_limit test near :166-177 guard. discovered_from=304"},
  {"path": "rio-scheduler/src/actor/tests/merge.rs", "action": "MODIFY", "note": "T77: +shared_node_priority_bumps_to_max test; r[verify sched.merge.shared-priority-max]. discovered_from=sprint-1-cleanup"},
  {"path": "rio-worker/src/executor/tests.rs", "action": "MODIFY", "note": "T78: +resolve_inputs_maps_inputdrvs_to_output_paths test; r[verify worker.executor.resolve-input-drvs]. discovered_from=sprint-1-cleanup"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T80: +_rewrite_t_placeholders collision assert tests (agreeing+disagreeing). discovered_from=306"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T81: +cross-batch-boundary cycle test (>100 paths). T82: +external-referrer negative case. discovered_from=441. Coordinate P0449"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T83: +CA-cutoff Skipped + recovery orphan upsert tests. discovered_from=442"},
  {"path": "rio-scheduler/src/actor/tests/lifecycle_sweep.rs", "action": "MODIFY", "note": "T84: +CA-cutoff skipped_interested union test (merged build, only cascade-Skipped node). discovered_from=442"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T85: +ManifestKind::total_size Chunked arm unit test. discovered_from=429"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T88: +test_fod_completion_skips_build_sample (negative: COUNT==0 after FOD success). discovered_from=452"},
  {"path": "rio-scheduler/src/actor/tests/dispatch.rs", "action": "MODIFY", "note": "T89: +fod_queue_depth + fetcher_utilization CountingRecorder capture test incl total==0 NaN-guard. discovered_from=452"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T90: CONDITIONAL — coverage-full VM parallelism ceiling entry if investigation confirms environmental. discovered_from=coverage"},
  {"path": "nix/tests/scenarios/netpol.nix", "action": "MODIFY", "note": "T91: store-ingress cross-ns probe subtest"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T93: FetcherPool startup e2e subtest (TAIL-append, count=23 HOT)"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T93: add fetcher-role node (label+taint)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T91+T93: r[verify] wiring at subtests entries"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T94: helm-lint golden assert for devicePlugin+seccompInstaller nodeAffinity. discovered_from=459"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "T95: verify_sig cross-validation test via fingerprint() fn. discovered_from=461"},
  {"path": "rio-store/src/substitute.rs", "action": "MODIFY", "note": "T96: r[verify store.singleflight] — moka get_with coalescing test. discovered_from=462"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T97: upstream add+list+remove roundtrip subtest. discovered_from=463"},
  {"path": "rio-store/tests/substitute_errors.rs", "action": "NEW", "note": "T99: Err-arm coverage — 6+ error-injection tests. discovered_from=462"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T100: helm-lint assert builderpool hostUsers:false renders. discovered_from=468"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T487: grafana configmap drift check in helm-lint. discovered_from=458"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T488: record_green + pure-docs fallback tests. discovered_from=479"},
  {"path": "rio-controller/src/reconcilers/common/sts.rs", "action": "MODIFY", "note": "T489: read_only_root_fs volumes/mounts unit tests. discovered_from=467"},
  {"path": "rio-store/src/substitute.rs", "action": "MODIFY", "note": "T490: with_stale_threshold builder test. discovered_from=483"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T491: coverage rc≠0 → halt integration test. discovered_from=484"},
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T492: psa-restricted loop + rio-dashboard per-depl UID. discovered_from=460"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T493: helm-lint coverage-mode self-guard positive assert. discovered_from=460"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T984472801: helm-lint max-coverage render all default-off enabled. discovered_from=493"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T984472802: clause4_check crash-path test (CLOSED BY P984472802 T3). discovered_from=488"}
]
```

```
nix/tests/scenarios/cli.nix       # T1+T2: populated-state assertions
rio-cli/tests/smoke.rs            # T3: dirty-close unit test
rio-scheduler/src/state/
└── derivation.rs                 # T4: recovery-init test
.claude/work/plan-0276*.md        # T5: Tracey annotation guidance
rio-controller/src/
├── crds/workerpool.rs            # T6: cel_rules_in_schema +2 asserts
└── reconcilers/workerpool/
    └── builders.rs               # T7: Unconfined arm test
rio-gateway/tests/golden/
├── ca_corpus.rs                  # T8: NEW — 3 parse-roundtrip tests
└── mod.rs                        # T8: +mod ca_corpus
.claude/lib/test_scripts.py       # T9: Mitigation pattern + happy-path tests
nix/tests/scenarios/fod-proxy.nix # T10: REMINDER — run VM test on KVM builder
nix/tests/scenarios/scheduling.nix # T11: per-build-timeout subtest (BLOCKED on P0329)
rio-scheduler/src/actor/tests/
└── completion.rs                 # T12: class_drift behavioral test
```

## Dependencies

```json deps
{"deps": [216, 219, 223, 247, 317, 308, 214, 329, 240, 241, 267, 322, 264, 266, 338, 260, 296, 213, 355, 255, 289, 357, 321, 352, 363, 230, 360, 231, 234, 236, 278, 281, 371, 237, 374, 379, 251, 299, 279, 253, 373, 347, 252, 280, 287, 415, 306, 441, 442, 429, 425, 452, 453, 455, 456, 454, 460, 484, 493, 488], "soft_deps": [276, 304, 323, 215, 330, 342, 344, 351, 341, 361, 295, 363, 366, 362, 368, 374, 382, 381, 239, 391, 392, 393, 254, 398, 380, 399, 400, 389, 409, 424, 426], "note": "Fresh test-gap batch (no open one existed). T1-T3 from P0216 review (cli code exists, tests assert empty-case only). T4 from P0219 (failure_count recovery documented @ derivation.rs:364 but untested). T5 is P0276 annotation guidance (marker bundles client+server; P0276 = server-half). T6/T7 from P0223 review (seccomp CEL rules + Unconfined arm untested \u2014 T6 is strongest: test exists SPECIFICALLY to catch silent-drop, blind to 2 new rules). T8 from P0247 (DONE \u2014 corpus .bin files staged, zero .rs consumers; bitrot if upstream Nix bumps fixtures). T9 from P0317 (DONE \u2014 Mitigation model + flake-mitigation CLI verb landed, zero tests; landed_sha pattern ^[0-9a-f]{8,40}$ unverified). T10 from P0308 (reminder task \u2014 run vm-fod-proxy-k3s when KVM-capable builder available; mknod-whiteout fix host-kernel-verified but VM integration unproven, 2x allocation landed on ec2-builder8 KVM-denied). Soft-dep P0304: its T11 adds .message() to CEL attrs \u2014 if schema output changes, T6's verbatim json.contains() strings need re-check. Soft-conflict P0323: adds tests to test_scripts.py \u2014 all additive, different test-fn names. T11 from P0214 T3 skip \u2014 HARD-BLOCKED on P0329 (build_timeout reachability investigation): don't write a VM test for a possibly-dead feature. 329 resolves whether ssh-ng sends wopSetOptions (claim A at handler/mod.rs:82-90 vs claim B from P0215 verification contradict). Route-1 (ssh-ng reachable) or Route-2 (gRPC-only) or Route-3 (OBE). Soft-dep P0215: the opcodes_read.rs:226 info-log that 329 probes. T12 from bughunter (completion.rs:390 class_drift emit has only name-check coverage; sibling misclassifications_total HAS behavioral test at :811 \u2014 mirror the pattern for the drift-without-penalty window). Soft-dep P0330: CountingRecorder extraction to rio-test-support \u2014 T12 uses it via helpers.rs re-export or direct import; works either way, sequence-independent. T12's 329 dep in the fence above was leading-zero \u2014 fixed to 329 per P0304-T28 (also fixed 330\u2192330 soft-dep). T13/T14 from P0240 (DONE \u2014 scheduling.nix cancel-timing + load-50drv + setoptions-unreachable never KVM-executed, mc=51-56 clause-4 fast-path). T15 from P0241 (DONE \u2014 vm-netpol-k3s FIRST-build, netpol-positive proves-nothing self-check never ran). T16 meta-task consolidating T10/T13-T15 into .claude/notes/kvm-pending.md manifest. All four reminder-tasks are confirmatory-not-gating same as T10's risk profile. discovered_from: T8=247, T9=317, T10=308, T11=214, T12=bughunter, T13=240, T14=329, T15=241, T16=bughunter, T17=267, T18=267, T19=267, T20=322, T21=264, T22=266, T23=338. T17+T18+T19 from P0267 review: PutPathBatch handler at put_path_batch.rs:245 (FailedPrecondition oversize), :256 (already_complete idempotency), and worker-side upload.rs:558 (FailedPrecondition fallthrough) all untested. Soft-dep P0342: fixes :275 ?\u2192bail! in put_path_batch.rs and adds tests to chunked.rs after :377 \u2014 same file as T17+T18, all additive test-fns, non-overlapping names (gt13_batch_oversize_* vs gt13_batch_placeholder_cleanup_*). If P0342 lands first, T17+T18's 'after :377' becomes 'after ~:500' \u2014 re-grep at dispatch. T19 adds fail_batch_precondition to MockStore (rio-test-support/src/grpc.rs:48) \u2014 low-traffic, additive field. T20 depends on P0322 (DONE \u2014 next()\u2192matches[0] refactor exists at cli.py:399-406; discovered_from=322). T21 depends on P0264 (UNIMPL \u2014 require_tenant at grpc/chunk.rs:131 + test_find_missing_chunks_no_tenant_fail_closed arrive with it; discovered_from=264). T22 depends on P0266 (UNIMPL \u2014 Progress arm at grpc/mod.rs:846-878 + counter arrive with it; discovered_from=266). T23 depends on P0338 (UNIMPL \u2014 tenant-id extraction at put_path_batch.rs:67-70 + maybe_sign at :320 arrive with it; discovered_from=338). Soft-dep P0344: adds ContentLookup test to chunked.rs after :377 \u2014 same file as T17+T18, additive. T24 depends on P0260 (UNIMPL \u2014 MockScheduler.resolve_tenant_uuid/calls at grpc.rs:574,580,808 + server.rs auth_publickey JWT branches arrive; discovered_from=260). T25 depends on P0296 (UNIMPL \u2014 ephemeral.rs:176 spawn-decision arithmetic arrives; discovered_from=296). T25 soft-conflicts P0347 (both touch ephemeral.rs \u2014 T25 extracts spawn_count fn, P0347 adds activeDeadlineSeconds at Job builder; non-overlapping sections). T26 depends on P0338 (DONE \u2014 maybe_sign Err-arm fallback at grpc/mod.rs:323-339 exists; discovered_from=bughunter-mc98). T26 soft-conflicts P0351 (spawn_grpc_server_layered helper \u2014 T26 uses spawn_store_with_fake_jwt which may migrate to the layered helper; sequence-independent, test body unchanged) + P0352 (rewrites maybe_sign via resolve_once \u2014 T26's fallback-test semantics hold before and after, the Err arm moves but warn+cluster preserved). T27 depends on P0213 (UNIMPL \u2014 conn_cap Semaphore + ensure_permit + gateway.toml max_connections arrive with it; discovered_from=213). T27 adds first r[verify gw.conn.cap] at VM level. T28 depends on P0260 (DONE \u2014 resolve_and_mint + MockScheduler.resolve_tenant fields exist; discovered_from=bughunter mc98-105). T28 EXTENDS T24: adds timeout path (:452 tokio::time::timeout \u2014 distinct from UNAVAILABLE Status), unparseable tenant_id path (:472-477), and counter assertions (rejected_jwt :625, mint_degraded_total :640) \u2014 T24 covers success/reject/degrade but not these. Both T24+T28 touch rio-gateway/src/server.rs cfg(test) mod \u2014 additive test fns, zero overlap. T29 depends on P0355 (DONE \u2014 load_and_wire_jwt at jwt_interceptor.rs:190 exists; discovered_from=355-review). T29 soft-dep P0349 (encode_pubkey_file helper at :532 \u2014 used by T29's test setup). T30 depends on P0289 (DONE \u2014 sizeclass subtest at scheduling.nix:220-225 exists with passthrough-OFF negative assert; discovered_from=bughunter-mc119). T30 soft-dep P0341 (marker-at-subtests-entry convention \u2014 T30 moves r[verify worker.fuse.passthrough] from #[ignore]'d Rust stub to col-0). T30 soft-conflict P0361 (touches scheduling.nix \u2014 T30 at :220-225 sizeclass body, P0361-T2 at :1217+ sigint-graceful tail; non-overlapping). T31 depends on P0255 (DONE \u2014 tenant_quota_by_name at gc/tenant.rs:51 exists; discovered_from=255). T31 uses same TestDb + seed_path pattern as gc/mark.rs:572 existing test. Line refs are from plan worktrees \u2014 re-grep at dispatch. T32 depends on P0357 (DONE \u2014 jwt-mount-present subtest at lifecycle.nix:416-532 + drv wm2wmwssi1 exist but never KVM-built; discovered_from=coordinator). T32 soft-dep P0295-T53 (fixes the :441 main.rs:676 triple-stale cite \u2014 land first so VM-run doesn't cite wrong fn/line). T33 depends on P0321 (DONE \u2014 assert_histograms_have_buckets at metrics.rs:328 + HISTOGRAM_BUCKET_MAP at observability.rs:292 exist; discovered_from=321). T33 soft-dep P0363: that plan wires the worker test specifically (live upload_references_count bug motivated it). If P0363 dispatched first, T33 covers controller/store/gateway only. If not, T33 includes worker. Check at dispatch: grep all_histograms_have_bucket_config rio-worker/tests/. T34 depends on P0352 (DONE \u2014 resolve_once+sign_with_resolved hoist exists at put_path_batch.rs:327; discovered_from=352-review). T34 soft-dep P0363 (rev-p363 confirmed 2/5 crates wired \u2014 scheduler+worker; T33 sweeps remaining 3). T34 extends T23's signing.rs test coverage \u2014 same file, additive test-fn, reuse T23's seed_tenant_with_key helpers. T35 depends on P0321+P0363 (DONE \u2014 HISTOGRAM_BUCKET_MAP at observability.rs:302 + assert_histograms_have_buckets helper exist; discovered_from=consol-mc135). T35 REFINES T33: T33 at :1194-1195 said put_path_duration + opcode_duration 'sub-second, default fits' \u2192 exempt. Consolidator mc135: NAR uploads exceed 10s (multi-GB wopAddToStoreNar) \u2014 samples land in +Inf, silently-useless same as P0321's build_graph_edges. T35 adds NAR_LATENCY_BUCKETS (0.1..300s) + 2 map entries + drops both from exempt lists + fixes obs.md :208 sentence. T36 depends on P0230 (DONE \u2014 spawn_task at rebalancer.rs:310 exists; discovered_from=230-review). spawn_task has tokio::select! + biased; + shutdown-token \u2014 zero test coverage. P0335-gotcha (select! default=RANDOM) relevant: a biased; regression would go unnoticed. T36 soft-conflict P366 (re-emit cutoff_seconds gauge in spawn_task :350+; T36 tests the loop itself \u2014 non-overlapping, T36 can use P366's gauge emit as an observable side effect). T37 depends on P0360 (UNIMPL \u2014 vm-security-nonpriv-k3s scenario + vmtest-full-nonpriv.yaml arrive with it; discovered_from=bughunt-mc147). T37 is T16-class reminder task (kvm-pending.md entry). Soft-dep P0304-T100 (300s timeout bump \u2014 nonpriv DS bring-up needs it). T38 depends on P0231 (UNIMPL \u2014 GetSizeClassStatus RPC + admin/sizeclass.rs handler arrive with it; discovered_from=231-review). T38 soft-dep P0295-T66 (adds r[sched.admin.sizeclass-status] marker \u2014 T38's r[verify] targets it; marker-first discipline means T66 lands first, T38's r[verify] then has a target). T38 soft-dep P0362 (submit_build_grpc helper pattern \u2014 T38 may use sched_grpc local or P0362's extracted helper). T39 no hard-dep (hmac.rs:218 exists since phase-3b; discovered_from=bughunt-mc147). T39 CORRECTS earlier coordinator speculation that the mutation was 'constant-time comparison' \u2014 WRONG, actual is `>` vs `>=` at expiry-second boundary. Forward-referenced by P0373-T3 (mutants-triage plan \u2014 T39 here IS the hmac-boundary fix, P0373 covers aterm\u00d77+wire\u00d76). T39 soft-dep P0368 (if baseline fixed, just mutants re-run confirms T39 catches the mutation; not blocking \u2014 the test stands alone). T40+T41 depend on P0234 (UNIMPL \u2014 build_class_statuses at workerpoolset/mod.rs:202 + scale_wps_class at scaling.rs:501 arrive with it; discovered_from=234-review). T40 tests status-aggregation (iterates spec.classes not RPC classes \u2014 load-bearing nonexistent-class filter). T41 tests happy-path replica-compute (queued/target=desired). T40+T41 soft-dep P0374 (asymmetric-key flap fix \u2014 T41's happy-path is a SEPARATE axis from P0374-T3's name-collision skip; both touch scaling.rs cfg(test) mod, additive test fns). T42+T43 depend on P0236 (UNIMPL \u2014 cutoffs.rs + cli cutoffs subcommand arrive with it; discovered_from=236-review). T42 is unit-level (pure render fn), T43 is VM end-to-end (cli.nix subtest). Same test-gap class as T1-T2 (cli ships pretty-print, MockAdmin returns empty \u2192 untested). T42+T43 partner with T38 (grpcurl subtest for same RPC) \u2014 three-layer coverage: raw RPC (T38), CLI unit (T42), CLI VM (T43). T43 precondition: scheduler.toml [[size_classes]] entries \u2014 may need fixture setup. T44 depends on P0278 (UNIMPL \u2014 BuildDrawer.svelte arrives with it; discovered_from=278-review). T45+T46+T47 depend on P0281 (UNIMPL \u2014 GC.svelte + Workers.svelte + their test scaffolds arrive with it; discovered_from=281-review). P0281-T7 covers happy-path; T45-T47 cover error+teardown paths. T44-T47 all vitest+@testing-library/svelte \u2014 same tooling P0277 brought in. No tracey markers (write-action UI tests, no dash.* spec coverage). T47's fake-timer test is the canonical 'setInterval cleared on unmount' \u2014 if it fails, the page leaks intervals across routing, eventually stacking N timers per navigation. T48 depends on P0371 (DONE \u2014 dashboard-gateway.yaml GRPCRoute with 11 method: matches exists; discovered_from=371-review). T48 is a helm-lint bash assert in flake.nix \u2014 additive after the existing enableMutatingMethods assert at ~:594. T48 uses awk to isolate 'service AdminService {' block (CreateEphemeralWorker is ControllerService, must exclude). T49 depends on P0237 (UNIMPL \u2014 wps subcommand arrives with it; discovered_from=237-review). T49 is cli.nix subtest TAIL-append same file as T43 (cutoffs subtest); non-overlapping subtests. T49 soft-dep P0382 (the .ok()\u2192get_opt fix \u2014 T49 exercises happy-path, P0382 exercises RBAC-403; orthogonal, sequence-independent). T50 depends on P0374 (DONE or merging \u2014 prune_stale_children at workerpoolset/mod.rs:226 exists; discovered_from=374-review). T50 uses ApiServerVerifier fixture (same pattern as T40's build_class_statuses test); creates workerpoolset/tests.rs if absent. T50 soft-dep P0381 (scaling.rs split \u2014 T50's ApiServerVerifier import stays in fixtures.rs, unaffected by the scaling split; sequence-independent). T51 depends on P0379 (UNIMPL \u2014 the yq eval-all thirdparty-image loop in flake.nix helm-lint arrives with P0379-T2; discovered_from=379-review). T51 is CORRECTNESS-severity but batched (helm-lint check-definition hardening, not code behavior). Same proves-nothing axis as P0241 (axis-2 environment) and P0328 (axis-3 regex-false-green). T51 soft-conflict P0304-T86 (both touch flake.nix helm-lint block \u2014 T51 in P0379-T2's yq loop, P0304-T86 is /tmp\u2192$TMPDIR; non-overlapping bash blocks within same derivation checkPhase). T51 also soft-conflict P0304-T136 (envoy lockstep assert adjacent in helm-lint; additive, non-overlapping). T52+T53 soft-dep P0239 (UNIMPL \u2014 pdb-ownerref + wps-lifecycle fragments are the SUBJECTS of T52/T53; T52 is pre-dispatch guard prose, T53 is NEVER-BUILT manifest entry; both can land BEFORE P0239 dispatches \u2014 they're advisory for the implementer; discovered_from=239-review). T52 is plan-doc-only (.claude/work/ edit); T53 touches kvm-pending.md (same manifest as T16/T32/T37). T54+T55 depend on P0251 (DONE \u2014 ca_hash_compares counter + ca_output_unchanged field + timeout-wrapped content_lookup exist at completion.rs:309-335; discovered_from=251-review). T54 soft-dep P0304-T148 (adds outcome=error third label \u2014 T54's counter-assert checks whichever landed; sequence-independent). T55 soft-dep P0252 (UNIMPL \u2014 cutoff-propagate plan; T55's Option-A vs Option-B decision depends on whether P0252 propagates same-tick or deferred-message; read P0252's design at dispatch). completion.rs count=26 HOT \u2014 T54 adds one test fn alongside P0251's existing tests, additive. derivation.rs count=17 \u2014 T55 is doc-comment-only (Option-A) or field-add+migration (Option-B). T56 PARTNERS WITH T53 (same kvm-pending entry \u2014 independent re-flag confirms finding; T56 is format-parity review, no-op if T53 landed entry). T57 depends on P0299 (DONE \u2014 warm_gate_fallback_when_no_warm_workers exists at assignment.rs:596; discovered_from=299). T57 uses CountingRecorder from rio-test-support (exists since P0330). T58 depends on P0299 (DONE \u2014 on_worker_registered at actor/worker.rs:99 exists; discovered_from=299). T58 soft-dep P0391 (determinism test \u2014 same actor/tests/worker.rs file, additive, different axis). T59 depends on P0299 (DONE \u2014 prefetch-hint joiner at runtime.rs:791 exists; discovered_from=299). T60 depends on P0279 (UNIMPL \u2014 LogViewer.svelte arrives with it; discovered_from=279). T60 soft-dep P0392 (windowed-render test \u2014 same __tests__/LogViewer.test.ts, additive). T61 depends on P0251 (DONE \u2014 CA-compare hook at completion.rs:289-337 exists; discovered_from=251). T61 PARTNERS WITH T54 (T54=timeout edge, T61=zero-outputs/malformed/Err edges; both add to actor/tests/completion.rs additively). T61 soft-dep P0304-T148/T157 (error+malformed counter labels \u2014 T61's tests assert the labels; sequence T148/T157 FIRST or make T61 assertions conditional) + P0393 (short-circuit \u2014 T61's rpc-err test assertion count changes from 2 to 1 if short-circuit landed; document both in test comment). T62 depends on P0253 (DONE \u2014 downstream_placeholder at resolve.rs:150 exists; discovered_from=253). T62 soft-dep P0304-T159 (Result\u2192String signature \u2014 drop .unwrap() if T159 landed). T63 depends on P0253 (DONE \u2014 maybe_resolve_ca+collect_ca_inputs at dispatch.rs:645-752 exist; discovered_from=253). T63 soft-dep P0254-T6 (collect_ca_inputs returns non-empty after modular_hash plumbing \u2014 T63's last test may need #[ignore] or backdoor). T63 soft-dep P0398 (tryResolve semantics \u2014 changes resolve.rs, doesn't touch dispatch.rs gates T63 covers). T64+T65 depend on P0373 (DONE \u2014 cargo-mutants infra + baseline exists; discovered_from=373). T64 covers 48+30=78 MISSED mutations beyond P0373-T5 budget; T65 covers 13 in manifest.rs. Both use table-driven exact-value asserts (any single-operator mutant flips \u22651 table row). T64 touches assignment.rs (count~12) + derivation.rs (count=17) \u2014 both cfg(test) additive. T65 touches manifest.rs (count~5) cfg(test) additive. FIXED soft_deps leading-zero: 391\u2192391, 392\u2192392, 393\u2192393; moved 373 from soft to hard (T64/T65 discovered_from). T70 depends on P0287 (DONE \u2014 observability.nix observe-block at :328-368 arrived with P0287-T7; the PARENTING question is still unresolved because VM never KVM-ran; discovered_from=295-review). T70 soft-dep P0295-T63 (the spec-text + assert commit \u2014 T70 either confirms or refutes its mechanism-analysis). T70 is T16/T32/T37/T53-class kvm-pending reminder. T71 depends on P0415 (DONE \u2014 is_finite() checks on backoff_* exist; config_rejects_nan_* tests exist; discovered_from=415). T71 soft-dep P0409 (DONE \u2014 validate_config + config_rejects_* test pattern originated). T71 soft-dep P0424 (cpu_limit_cores validation \u2014 same main.rs cfg(test) mod, additive test-fn, different fn-name; non-overlapping). T72 depends on P0392 (UNIMPL \u2014 push-based logStream + virtualized LogViewer arrive with it; discovered_from=392). T72 soft-dep P426 (loop-push variant \u2014 integration test still holds). T88+T89 depend on P0452 (DONE \u2014 FOD hard-split routing + metrics emitters at dispatch.rs:185 + completion.rs:996 FOD-skip guard exist; discovered_from=452). T89 uses CountingRecorder from rio-test-support (exists since P0330). T90 investigates coverage-full VM flakes across p452/p453/p455/p456 merges \u2014 same T86/T87 investigate-then-route shape; two signatures (protocol-* mid-test vs lifecycle-wps driver-build). lifecycle-wps failed TWICE (merge-18+26) post-p453 \u2014 higher suspicion of real break. T88 same file as T83 (actor/tests/completion.rs) \u2014 both additive test-fns, zero name collision."}
```

**Depends on:** [P0216](plan-0216-rio-cli-subcommands.md) — `print_build`/`BuildJson`/`--status`/dirty-close exist. [P0219](plan-0219-per-worker-failure-budget.md) merged (DONE).
**Soft-dep:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — T5 edits its plan doc, so land before P0276 dispatches (so the implementer sees the Tracey guidance). If P0276 already dispatched, send the guidance via coordinator message instead.
**Conflicts with:** [`derivation.rs`](../../rio-scheduler/src/state/derivation.rs) also touched by [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) T1 (derive) — different sections. `cli.nix` low-traffic. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T11 adds `.message()` to the derive attrs (struct top); T6 here adds asserts to `cel_rules_in_schema` (test fn bottom). Same file, non-overlapping sections. [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) count=18 — T7 adds a test fn alongside existing seccomp tests, additive. [`golden/mod.rs`](../../rio-gateway/tests/golden/mod.rs) — T8 adds one `mod ca_corpus;` line, additive. [`test_scripts.py`](../../.claude/lib/test_scripts.py) — T9, [P0322](plan-0322-flake-mitigation-dup-key-guard.md) T3, and [P0323](plan-0323-mergesha-pydantic-model.md) T4 all add test functions; all additive, zero name collisions, each ~30-50 lines in a 1800+ line file. [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — T11 here, [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) T1 (PROBE subtest), P0214-T3 (never landed), P0215-T3 (never landed) all TAIL-append. T11 BLOCKED-ON 0329 means they serialize naturally: 0329's probe lands first (and may stay as a real test), T11 appends after. [`tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) — T12 here adds after `:834`. [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) itself is count=25 but T12 doesn't touch it — only reads `:388-396` for the emit-site under test. [P0330](plan-0330-test-recorder-extraction-test-support.md) T2 touches `helpers.rs` (CountingRecorder source) — T12 consumes it; works before or after the extraction (re-export is transparent).
