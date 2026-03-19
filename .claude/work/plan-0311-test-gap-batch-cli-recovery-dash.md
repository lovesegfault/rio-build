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

## Tracey

References existing markers:
- `r[sched.admin.list-workers]` — T2 verifies (status_filter behavior at [`scheduler.md:129`](../../docs/src/components/scheduler.md))
- `r[sched.poison.ttl-persist]` — T4 verifies (recovery restores poison state including failure_count at [`scheduler.md:108`](../../docs/src/components/scheduler.md))
- `r[dash.graph.degrade-threshold]` — T5 annotation guidance (P0276 implements server-half)

- `r[worker.seccomp.localhost-profile]` — T6 verifies (CEL rules guard the Localhost-coupling spec'd at [`security.md:55`](../../docs/src/security.md)), T7 verifies (Unconfined arm coverage)

- `r[sched.timeout.per-build]` — T11 verifies (the first `r[verify]` for this marker; currently [`worker.rs:570`](../../rio-scheduler/src/actor/worker.rs) has `r[impl]` only)
- `r[obs.metric.scheduler]` — T12 verifies (behavioral coverage for `class_drift_total` — the [`metrics_registered.rs:49`](../../rio-scheduler/tests/metrics_registered.rs) name-check under the same marker doesn't prove the emit-site fires correctly)
- `r[gw.opcode.set-options.propagation+2]` — T14's VM run is the first EXECUTION of this marker's `r[verify]` annotation at [`scheduling.nix:777`](../../nix/tests/scenarios/scheduling.nix). tracey already sees it (nix/tests/*.nix is in test_include); running it proves the journal-grep assertion actually holds

- `r[store.atomic.multi-output]` — T17 verifies (FailedPrecondition cleanup), T18 verifies (per-output idempotency flag within batch)
- `r[store.put.idempotent]` — T18 verifies (already-complete path inside batch → `created=false`)
- `r[worker.upload.multi-output]` — T19 verifies (fallthrough on FailedPrecondition → independent PutPath per [`worker.md:224`](../../docs/src/components/worker.md); atomicity lost per the spec's "register each output path" independent-upload step)
- `r[sec.boundary.grpc-hmac]` — T21 verifies (PutChunk fail-closed; the `require_tenant` check is tenant-scoped HMAC-adjacent auth — uses `x-test-tenant-id` metadata header, not the assignment token, but enforces the same fail-closed contract)
- `r[obs.metric.scheduler]` — T22 verifies (counter fires on Progress arm wiring)
- `r[store.tenant.sign-key]` — T23 verifies (batch outputs signed with tenant key not cluster)

No new markers. T1/T3 test cli output formatting and stream-handling — no corresponding spec markers exist (cli output format is not spec'd). T8 tests wire-format parse-roundtrip — the corpus bytes are Nix's golden fixtures, not a rio spec contract; no `r[gw.*]` marker for Realisation wire-format specifically (only the per-opcode markers, which cover behavior not byte-shape). T9/T20 test harness CLI + pydantic pattern — not spec'd.

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
  {"path": "rio-store/tests/grpc/signing.rs", "action": "MODIFY", "note": "T23: +batch_outputs_signed_with_tenant_key (2-output batch, tenant-key sig verify); r[verify store.tenant.sign-key]"}
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
{"deps": [216, 219, 223, 247, 317, 308, 214, 329, 240, 241, 267, 322, 264, 266, 338], "soft_deps": [276, 304, 323, 215, 330, 342, 344], "note": "Fresh test-gap batch (no open one existed). T1-T3 from P0216 review (cli code exists, tests assert empty-case only). T4 from P0219 (failure_count recovery documented @ derivation.rs:364 but untested). T5 is P0276 annotation guidance (marker bundles client+server; P0276 = server-half). T6/T7 from P0223 review (seccomp CEL rules + Unconfined arm untested — T6 is strongest: test exists SPECIFICALLY to catch silent-drop, blind to 2 new rules). T8 from P0247 (DONE — corpus .bin files staged, zero .rs consumers; bitrot if upstream Nix bumps fixtures). T9 from P0317 (DONE — Mitigation model + flake-mitigation CLI verb landed, zero tests; landed_sha pattern ^[0-9a-f]{8,40}$ unverified). T10 from P0308 (reminder task — run vm-fod-proxy-k3s when KVM-capable builder available; mknod-whiteout fix host-kernel-verified but VM integration unproven, 2x allocation landed on ec2-builder8 KVM-denied). Soft-dep P0304: its T11 adds .message() to CEL attrs — if schema output changes, T6's verbatim json.contains() strings need re-check. Soft-conflict P0323: adds tests to test_scripts.py — all additive, different test-fn names. T11 from P0214 T3 skip — HARD-BLOCKED on P0329 (build_timeout reachability investigation): don't write a VM test for a possibly-dead feature. 0329 resolves whether ssh-ng sends wopSetOptions (claim A at handler/mod.rs:82-90 vs claim B from P0215 verification contradict). Route-1 (ssh-ng reachable) or Route-2 (gRPC-only) or Route-3 (OBE). Soft-dep P0215: the opcodes_read.rs:226 info-log that 0329 probes. T12 from bughunter (completion.rs:390 class_drift emit has only name-check coverage; sibling misclassifications_total HAS behavioral test at :811 — mirror the pattern for the drift-without-penalty window). Soft-dep P0330: CountingRecorder extraction to rio-test-support — T12 uses it via helpers.rs re-export or direct import; works either way, sequence-independent. T12's 0329 dep in the fence above was leading-zero — fixed to 329 per P0304-T28 (also fixed 0330→330 soft-dep). T13/T14 from P0240 (DONE — scheduling.nix cancel-timing + load-50drv + setoptions-unreachable never KVM-executed, mc=51-56 clause-4 fast-path). T15 from P0241 (DONE — vm-netpol-k3s FIRST-build, netpol-positive proves-nothing self-check never ran). T16 meta-task consolidating T10/T13-T15 into .claude/notes/kvm-pending.md manifest. All four reminder-tasks are confirmatory-not-gating same as T10's risk profile. discovered_from: T8=247, T9=317, T10=308, T11=214, T12=bughunter, T13=240, T14=329, T15=241, T16=bughunter, T17=267, T18=267, T19=267, T20=322, T21=264, T22=266, T23=338. T17+T18+T19 from P0267 review: PutPathBatch handler at put_path_batch.rs:245 (FailedPrecondition oversize), :256 (already_complete idempotency), and worker-side upload.rs:558 (FailedPrecondition fallthrough) all untested. Soft-dep P0342: fixes :275 ?→bail! in put_path_batch.rs and adds tests to chunked.rs after :377 — same file as T17+T18, all additive test-fns, non-overlapping names (gt13_batch_oversize_* vs gt13_batch_placeholder_cleanup_*). If P0342 lands first, T17+T18's 'after :377' becomes 'after ~:500' — re-grep at dispatch. T19 adds fail_batch_precondition to MockStore (rio-test-support/src/grpc.rs:48) — low-traffic, additive field. T20 depends on P0322 (DONE — next()→matches[0] refactor exists at cli.py:399-406; discovered_from=322). T21 depends on P0264 (UNIMPL — require_tenant at grpc/chunk.rs:131 + test_find_missing_chunks_no_tenant_fail_closed arrive with it; discovered_from=264). T22 depends on P0266 (UNIMPL — Progress arm at grpc/mod.rs:846-878 + counter arrive with it; discovered_from=266). T23 depends on P0338 (UNIMPL — tenant-id extraction at put_path_batch.rs:67-70 + maybe_sign at :320 arrive with it; discovered_from=338). Soft-dep P0344: adds ContentLookup test to chunked.rs after :377 — same file as T17+T18, additive. Line refs are from plan worktrees — re-grep at dispatch."}
```

**Depends on:** [P0216](plan-0216-rio-cli-subcommands.md) — `print_build`/`BuildJson`/`--status`/dirty-close exist. [P0219](plan-0219-per-worker-failure-budget.md) merged (DONE).
**Soft-dep:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — T5 edits its plan doc, so land before P0276 dispatches (so the implementer sees the Tracey guidance). If P0276 already dispatched, send the guidance via coordinator message instead.
**Conflicts with:** [`derivation.rs`](../../rio-scheduler/src/state/derivation.rs) also touched by [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) T1 (derive) — different sections. `cli.nix` low-traffic. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T11 adds `.message()` to the derive attrs (struct top); T6 here adds asserts to `cel_rules_in_schema` (test fn bottom). Same file, non-overlapping sections. [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) count=18 — T7 adds a test fn alongside existing seccomp tests, additive. [`golden/mod.rs`](../../rio-gateway/tests/golden/mod.rs) — T8 adds one `mod ca_corpus;` line, additive. [`test_scripts.py`](../../.claude/lib/test_scripts.py) — T9, [P0322](plan-0322-flake-mitigation-dup-key-guard.md) T3, and [P0323](plan-0323-mergesha-pydantic-model.md) T4 all add test functions; all additive, zero name collisions, each ~30-50 lines in a 1800+ line file. [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — T11 here, [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) T1 (PROBE subtest), P0214-T3 (never landed), P0215-T3 (never landed) all TAIL-append. T11 BLOCKED-ON 0329 means they serialize naturally: 0329's probe lands first (and may stay as a real test), T11 appends after. [`tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) — T12 here adds after `:834`. [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) itself is count=25 but T12 doesn't touch it — only reads `:388-396` for the emit-site under test. [P0330](plan-0330-test-recorder-extraction-test-support.md) T2 touches `helpers.rs` (CountingRecorder source) — T12 consumes it; works before or after the extraction (re-export is transparent).
