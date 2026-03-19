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

### T11 — `test(vm):` scheduling.nix per-build-timeout chain — BLOCKED on P0329

**DO NOT DISPATCH until [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) resolves.** That plan answers whether `build_timeout` is CLI-reachable via ssh-ng. If the answer is "no" (ssh-ng never sends wopSetOptions — Claim B in 0329), this test as-written would submit a build with `--option build-timeout 10`, the option would be silently dropped, the `sleep 60` would run to completion, and the test would flake-fail at the 15s wall-clock assert. That's a false-red — the feature works, the CLI path to it doesn't.

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

## Tracey

References existing markers:
- `r[sched.admin.list-workers]` — T2 verifies (status_filter behavior at [`scheduler.md:129`](../../docs/src/components/scheduler.md))
- `r[sched.poison.ttl-persist]` — T4 verifies (recovery restores poison state including failure_count at [`scheduler.md:108`](../../docs/src/components/scheduler.md))
- `r[dash.graph.degrade-threshold]` — T5 annotation guidance (P0276 implements server-half)

- `r[worker.seccomp.localhost-profile]` — T6 verifies (CEL rules guard the Localhost-coupling spec'd at [`security.md:55`](../../docs/src/security.md)), T7 verifies (Unconfined arm coverage)

- `r[sched.timeout.per-build]` — T11 verifies (the first `r[verify]` for this marker; currently [`worker.rs:570`](../../rio-scheduler/src/actor/worker.rs) has `r[impl]` only)

No new markers. T1/T3 test cli output formatting and stream-handling — no corresponding spec markers exist (cli output format is not spec'd). T8 tests wire-format parse-roundtrip — the corpus bytes are Nix's golden fixtures, not a rio spec contract; no `r[gw.*]` marker for Realisation wire-format specifically (only the per-opcode markers, which cover behavior not byte-shape). T9 tests harness CLI + pydantic pattern — not spec'd.

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
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T11: per-build-timeout subtest (TAIL append) — BLOCKED on P0329; Route-1 ssh-ng --option build-timeout OR Route-2 grpcurl SubmitBuild; r[verify sched.timeout.per-build]"}
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
```

## Dependencies

```json deps
{"deps": [216, 219, 223, 247, 317, 308, 214, 0329], "soft_deps": [276, 304, 322, 323, 215], "note": "Fresh test-gap batch (no open one existed). T1-T3 from P0216 review (cli code exists, tests assert empty-case only). T4 from P0219 (failure_count recovery documented @ derivation.rs:364 but untested). T5 is P0276 annotation guidance (marker bundles client+server; P0276 = server-half). T6/T7 from P0223 review (seccomp CEL rules + Unconfined arm untested — T6 is strongest: test exists SPECIFICALLY to catch silent-drop, blind to 2 new rules). T8 from P0247 (DONE — corpus .bin files staged, zero .rs consumers; bitrot if upstream Nix bumps fixtures). T9 from P0317 (DONE — Mitigation model + flake-mitigation CLI verb landed, zero tests; landed_sha pattern ^[0-9a-f]{8,40}$ unverified). T10 from P0308 (reminder task — run vm-fod-proxy-k3s when KVM-capable builder available; mknod-whiteout fix host-kernel-verified but VM integration unproven, 2x allocation landed on ec2-builder8 KVM-denied). Soft-dep P0304: its T11 adds .message() to CEL attrs — if schema output changes, T6's verbatim json.contains() strings need re-check. Soft-conflict P0322 + P0323: both also add tests to test_scripts.py — all additive, different test-fn names (P0322 = dup-key negative-path; P0323 = MergeSha; T9 here = Mitigation happy-path + pattern). T11 from P0214 T3 skip — HARD-BLOCKED on P0329 (build_timeout reachability investigation): don't write a VM test for a possibly-dead feature. 0329 resolves whether ssh-ng sends wopSetOptions (claim A at handler/mod.rs:82-90 vs claim B from P0215 verification contradict). Route-1 (ssh-ng reachable) or Route-2 (gRPC-only) or Route-3 (OBE). Soft-dep P0215: the opcodes_read.rs:226 info-log that 0329 probes. discovered_from: T8=247, T9=317, T10=308, T11=214. Line refs are from plan worktrees — re-grep at dispatch."}
```

**Depends on:** [P0216](plan-0216-rio-cli-subcommands.md) — `print_build`/`BuildJson`/`--status`/dirty-close exist. [P0219](plan-0219-per-worker-failure-budget.md) merged (DONE).
**Soft-dep:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — T5 edits its plan doc, so land before P0276 dispatches (so the implementer sees the Tracey guidance). If P0276 already dispatched, send the guidance via coordinator message instead.
**Conflicts with:** [`derivation.rs`](../../rio-scheduler/src/state/derivation.rs) also touched by [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) T1 (derive) — different sections. `cli.nix` low-traffic. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T11 adds `.message()` to the derive attrs (struct top); T6 here adds asserts to `cel_rules_in_schema` (test fn bottom). Same file, non-overlapping sections. [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) count=18 — T7 adds a test fn alongside existing seccomp tests, additive. [`golden/mod.rs`](../../rio-gateway/tests/golden/mod.rs) — T8 adds one `mod ca_corpus;` line, additive. [`test_scripts.py`](../../.claude/lib/test_scripts.py) — T9, [P0322](plan-0322-flake-mitigation-dup-key-guard.md) T3, and [P0323](plan-0323-mergesha-pydantic-model.md) T4 all add test functions; all additive, zero name collisions, each ~30-50 lines in a 1800+ line file. [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — T11 here, [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) T1 (PROBE subtest), P0214-T3 (never landed), P0215-T3 (never landed) all TAIL-append. T11 BLOCKED-ON 0329 means they serialize naturally: 0329's probe lands first (and may stay as a real test), T11 appends after.
