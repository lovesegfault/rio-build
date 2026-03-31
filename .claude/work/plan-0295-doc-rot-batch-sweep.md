# Plan 0295: Doc-rot batch sweep — 9 errata from retrospective

**Retro §Doc-rot batch.** 11 doc-rot items found; P0128 (landmine) and P0160 (5-site) were carved out as standalone [P0292](plan-0292-scheduler-probe-comment-landmine.md) and [P0293](plan-0293-link-parent-5-site-doc-fix.md). These are the remaining 9. All are comment/doc updates, no behavior change. Skip-CI candidate except clippy (comment-only).

Full detail at [`done-plan-retrospective.md §Doc rot`](../notes/done-plan-retrospective.md).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `docs:` README.md dead attributes (P0068)

MODIFY [`README.md`](../../README.md) at `:98-102` — `nix build .#ci-local-fast` / `.#ci-fast` neither exists (retired [`89ba88fd`](https://github.com/search?q=89ba88fd&type=commits)). New contributor copying from README gets `attribute not found`. Also "30s fuzz" → "2min fuzz."

Collapse to single `nix build .#ci` block per retrospective.

### T2 — `docs:` flake.nix branch-coverage DO-NOT warning (P0143)

MODIFY [`flake.nix`](../../flake.nix) at `:250` and/or [`nix/coverage.nix`](../../nix/coverage.nix) header — `RUSTFLAGS = "-C instrument-coverage"` has NO warning about branch coverage. `-Z coverage-options=branch` was tried ([`8126dcf`](https://github.com/search?q=8126dcf&type=commits)), segfaulted `llvm-cov export` at ~15GB RSS with 20+ object files, reverted ([`4c8365d`](https://github.com/search?q=4c8365d&type=commits)). Diagnostic info lives ONLY in commit [`395c0493`](https://github.com/search?q=395c0493&type=commits) — which was itself reverted.

Add: `# Do NOT add -Z coverage-options=branch — llvm-cov export segfaults at ~15GB RSS with 20+ object files (tried 8126dcf, reverted 4c8365d, diagnostic in 395c049).`

### T3 — `docs:` remediation-index stale OPEN counts (P0200)

MODIFY [`docs/src/remediations/phase4a.md`](../../docs/src/remediations/phase4a.md) at `:1457-1588` — index table says 129 OPEN, 1 FIXED — but ≥6 spot-checked entries are fixed (wkr-cancel-flag-stale, common-redact-password-at-sign, common-otel-empty-endpoint, cfg-zero-interval-tokio-panic, wkr-fod-flag-trust, gw-temp-roots-unbounded-insert-only). Index was INPUT to 21-plan sweep; nobody closed the loop.

Sweep table against 21 per-remediation docs + `plan-0200 §Remainder`: flip OPEN→FIXED(rem-NN). ~5-10 genuinely-open items stay OPEN with `TODO(phase4b)`.

### T4 — `docs:` synth_db.rs "deferred" → "by design" (P0051)

MODIFY [`rio-worker/src/synth_db.rs`](../../rio-worker/src/synth_db.rs) at `:13`, `:93`, `:158` — "Realisations: currently empty (CA support deferred)" implies temporary + globally-deferred. Both wrong. Table empty PRE-build is **permanent by design** (nix-daemon INSERTs post-CA-build; rio never populates — scheduler resolves CA inputs before dispatch). CA landed at store/gateway in 2c.

Rewrite to: "empty pre-build by design — nix-daemon INSERTs here post-CA-build; rio never populates (scheduler resolves CA inputs before dispatch per phase5.md)."

**Also:** add note at appropriate spot (check at impl time) that `drv_content` must never be empty for CA-resolved drvs (per retrospective — `executor/mod.rs:312` + `derivation.rs:352` fallbacks fetch unresolved .drv).

### T5 — `docs:` auth.rs TODO retag + security.md bearer-token strikethrough (P0154)

MODIFY [`rio-store/src/cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs):
- `:36` retag `TODO(phase4b)` → `TODO(phase5)` (phase4b.md deliberately omits; `security.md:82` + `phase5.md:19` assign to Phase 5)
- `:7` "in 4b" → "in Phase 5"

MODIFY [`docs/src/security.md`](../../docs/src/security.md) at `:69` — lists "Bearer token authentication" as not-yet-implemented. It shipped (`r[impl store.cache.auth-bearer]`). Strike from the list.

### T6 — `docs:` plan-0113 fabricated claims → actual (P0113)

MODIFY [`.claude/work/plan-0113-closure-size-estimator-fallback.md`](plan-0113-closure-size-estimator-fallback.md) at `:9`, `:19` — claims `MEMORY_MULTIPLIER` constant + `HistoryEntry.input_closure_bytes` field. **Neither ever existed in code** (`git log -S` returns only the backfill doc commit itself). Fabricated by `rio-backfill-clusterer`. Actual commit [`f343eb9`](https://github.com/search?q=f343eb9&type=commits) is DURATION proxy only.

Rewrite to describe DURATION proxy (`CLOSURE_BYTES_PER_SEC_PROXY` 10MB/s, `CLOSURE_PROXY_MIN_SECS` 5s floor). Delete fabricated claims.

### T7 — `docs:` plan-0005 "intentional" → "WRONG — bug #11" (P0005)

MODIFY [`.claude/work/plan-0005-live-daemon-golden-conformance.md`](plan-0005-live-daemon-golden-conformance.md) at `:33`, `:51`, `:63` — "Intentional divergence: wopNarFromPath sends STDERR_WRITE — both work, more robust." **All false.** Bug #11, fixed [`132de90b`](https://github.com/search?q=132de90b&type=commits) (140 commits after phase-1b). Causes `error: no sink`. `gateway.md:76` already has Historical note.

Append after `:51`: `[WRONG — bug #11, fixed 132de90b in phase 2a: Nix client's processStderr() passes no sink → error: no sink. See gateway.md:76.]`. Strike "intentional" on `:33`, `:63`.

### T8 — `docs:` plan-0076 test-rename forward-pointer (P0076)

MODIFY [`.claude/work/plan-0076-figment-config.md`](plan-0076-figment-config.md) at `:25` — describes `config_defaults_match_phase2a()` test. [`2ab2d22e`](https://github.com/search?q=2ab2d22e&type=commits) (P0097) renamed to `config_defaults_are_stable` and FIXED three dangerous defaults.

Append: `(2ab2d22e/P0097 later renamed test → config_defaults_are_stable, fixed 3 dangerous defaults. fuse_passthrough guard remains.)`

### T9 — `docs:` plan-0025 HashingReader cfg(test)-gated note (P0025)

MODIFY [`.claude/work/plan-0025-nar-streaming-refactor.md`](plan-0025-nar-streaming-refactor.md) at `:58` — "HashingReader — phase 2 uses verbatim." `HashingReader` was `#[cfg(test)]`-gated at [`68571efa`](https://github.com/search?q=68571efa&type=commits) (wrapping already-accumulated Vec doubled peak memory, ~8GiB for 4GiB NAR). `FramedStreamReader` survived.

Append: `(Later: HashingReader cfg(test)-gated at 68571efa — gRPC chunk accumulation already buffers into Vec; NarDigest::from_bytes on slice is production path. FramedStreamReader survived.)`

### T11 — `docs:` integration.md Build CRD section — DELETED CRD (USER-FACING)

**USER-FACING PRIORITY.** MODIFY [`docs/src/integration.md`](../../docs/src/integration.md) at `:84-100` — the "Kubernetes: Build CRD" section with a full `kubectl apply` YAML example for a CRD that [P0294](plan-0294-build-crd-full-rip.md) deletes. Anyone following the doc post-P0294 gets `error: the server doesn't have a resource type "builds"`.

Replace the section with the gRPC-direct submission pattern (which is what replaces Build CRD). Check P0294's plan doc for what the replacement flow is — likely `grpcurl SubmitBuild` or `rio-cli submit`. The `nix-eval-jobs` guidance at `:82` is still valid; just the CRD apply example is dead.

### T12 — `docs:` failure-modes.md Build CRD behavior refs

MODIFY [`docs/src/failure-modes.md`](../../docs/src/failure-modes.md) at `:10` and `:15`:
- `:10` "Build CRD controllers reconnect via `WatchBuild(build_id, since_sequence=last_seen)`" — delete "Build CRD controllers"; gateways reconnect the same way.
- `:15` "no Build CRD status updates" — delete this clause from the controller-down impact list.

### T13 — `docs:` phase4a.md 10 OPEN rows → MOOT-P0294

MODIFY [`docs/src/remediations/phase4a.md`](../../docs/src/remediations/phase4a.md) at `:1474` (overlaps T3's territory — coordinate) — 10 OPEN rows reference `rio-controller/src/reconcilers/build.rs` which P0294 deletes. Flip `OPEN` → `MOOT-P0294`. Also add a tombstone note to [`03-controller-stuck-build.md`](../../docs/src/remediations/phase4a/03-controller-stuck-build.md) header: "MOOT as of P0294 — Build CRD removed. Kept for provenance."

Also sweep for scattered `build.rs:NNN` line refs across `docs/` — `grep -rn 'reconcilers/build.rs' docs/` and tag each with `(file deleted in P0294)`.

### T14 — `docs:` 09-build-timeout-cgroup-orphan.md Build CR refs

MODIFY [`docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md`](../../docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md) at `:443`, `:618`, `:695` — references "Build CR status" and "Build CR conditions" in test-script prose. [P0289](plan-0289-port-specd-unlanded-test-trio.md) owns the test-port; these doc refs should say "build status (via `rio-cli builds` or `QueryBuildStatus` gRPC)" instead. Coordinate: P0289 may want to fix these alongside its test port — leave a `TODO(P0289)` if this lands first.

### T15 — `docs:` P0294 stale-comment sweep (5 files)

MODIFY per the P0294 review — stale comments referencing Build CRD behavior:
- [`rio-controller/Cargo.toml:9`](../../rio-controller/Cargo.toml) — likely a feature or dep comment
- [`rio-controller/src/main.rs:27`](../../rio-controller/src/main.rs) — module import comment
- [`nix/tests/default.nix:102`](../../nix/tests/default.nix) — test-registration comment
- [`flake.nix:534`](../../flake.nix) — VM-test or check-list comment
- [`nix/tests/scenarios/lifecycle.nix:650`](../../nix/tests/scenarios/lifecycle.nix) — subtest comment (P0294 deletes the subtest at `:518` onward; `:650` is inside that block so may disappear with P0294's rip — verify at dispatch)

Re-grep at dispatch — these are p294-worktree line numbers and P0294 itself deletes ~2030 LoC so post-merge line numbers shift dramatically.

### T16 — `docs:` gateway.md + data-flows.md + proto.md wopSetOptions 3-doc cluster

**P0215 proved ssh-ng never sends `wopSetOptions`.** Three docs still claim it does in present tense:

MODIFY [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) at `:45` — "`wopSetOptions` is **mandatory** as the first opcode after handshake --- Nix always sends it before any other operation." There's already a correction at `:515` for the mandatory-first-opcode claim. Add an **ssh-ng caveat** at `:45`: "The **daemon-protocol** client (`ssh://`) sends `wopSetOptions`. The **ssh-ng** client does NOT (empirically verified P0215) — client-side `--max-silent-time`/`--timeout` are silently non-functional over ssh-ng until [P0310](plan-0310-gateway-client-option-propagation.md) lands a gateway-side path." Run `tracey bump` on `r[gw.opcode.set-options.field-order]`.

MODIFY [`docs/src/data-flows.md`](../../docs/src/data-flows.md) at `:10` and `:71` — sequence diagram shows `Client→GW: wopSetOptions` unconditionally. Add `(ssh:// only; ssh-ng skips)` annotation.

MODIFY [`docs/src/components/proto.md`](../../docs/src/components/proto.md) at `:214` — "`// Client build options propagated from wopSetOptions`" comment in the proto. Amend to: "Client build options. For ssh:// these propagate from wopSetOptions; for ssh-ng they're populated gateway-side (P0310) or fall back to worker config defaults (P0215)."

### T17 — `docs:` fod-proxy.nix "three layers" → two (ssh-ng caveat)

**CROSS-WORKTREE — fix before P0243 merges, or fold into P0243's fix-impl round.** The file doesn't exist on sprint-1 yet; it's in the p243 worktree at [`fod-proxy.nix:199-201`](../../nix/tests/scenarios/fod-proxy.nix).

The comment says: "Three layers: wopSetOptions bounds (sent to the remote store via wopSetOptions). wget -T 15 inside the FOD bounds the network layer. `timeout 90`: shell-level."

P0215 proves the wopSetOptions layer doesn't exist for ssh-ng. Correct to: "Two layers: `wget -T 15` bounds the network leg inside the FOD builder. `timeout 90`: shell-level backstop. (wopSetOptions bounds would be a third layer, but ssh-ng doesn't send it — P0215.)"

**If P0243 has already merged when this dispatches:** apply directly to `nix/tests/scenarios/fod-proxy.nix:199-201` on sprint-1.
**If P0243 hasn't merged:** coordinator should send this fix as a message to the P0243 fix-impl round.

### T18 — `docs:` default.nix denied.invalid → k3s-agent

**P0243 worktree file.** [`nix/tests/default.nix:314`](../../nix/tests/default.nix) (p243 worktree) comment says "the denied case (`denied.invalid`) is a clean" — but [`3cb2c442`](https://github.com/search?q=3cb2c442&type=commits) changed the denied-case hostname to `k3s-agent` (the worker node's hostname, which squid's allowlist doesn't include). One-word fix. Same cross-worktree caveat as T17.

### T19 — `docs:` StoreServer → StoreServiceImpl (comment + plan doc)

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs) at `:632` — test doc comment says "The 'value reaches StoreServer' half of this roundtrip" — the struct is `StoreServiceImpl` (confirmed: 7 other refs in the same file use the correct name). [P0218](plan-0218-nar-buffer-config.md)'s plan doc also uses the wrong name — find and correct.

### T20 — `docs:` dashboard.md SchedulerService.GetBuildLogs → AdminService

MODIFY [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) at `:22` — table row says `SchedulerService.GetBuildLogs` but the RPC is in [`admin.proto:21`](../../rio-proto/proto/admin.proto) under `AdminService`. One-word fix.

### T21 — `docs:` plan-0206 digest() → sha256(convert_to()) spelling

MODIFY [`.claude/work/plan-0206-path-tenants-migration-upsert.md`](plan-0206-path-tenants-migration-upsert.md) at `:111` (T4) and `:118` (T5).

Both tasks spell the hash query as `digest($out, 'sha256')` — that's the **pgcrypto** extension function. `CREATE EXTENSION pgcrypto` appears nowhere in `migrations/`. The P0206 implementer correctly wrote `sha256(convert_to(...))` — the PG11+ builtin — at [`lifecycle.nix:1291`](../../nix/tests/scenarios/lifecycle.nix). The comment at `:1166` says "PG builtin" but doesn't say "NOT digest()". **Risk:** [P0207](plan-0207-mark-cte-tenant-retention.md) implementer reading plan-0206 as reference may copy the `digest()` spelling and hit `ERROR: function digest(text, unknown) does not exist`.

Change both occurrences:
```sql
-- Before (pgcrypto, NOT enabled):
SELECT count(*) FROM path_tenants WHERE store_path_hash = digest($out, 'sha256')
-- After (PG11+ builtin, what actually shipped):
SELECT count(*) FROM path_tenants WHERE store_path_hash = sha256(convert_to($out, 'UTF8'))
```

Also add an inline note at `:111`: `(NOT digest() — that's pgcrypto, which is not enabled. sha256() is the PG11+ builtin; convert_to() gets the bytea it wants.)`

### T22 — `docs:` onibus build excusable → onibus flake excusable

MODIFY [`.claude/work/plan-0313-kvm-fast-fail-preamble.md`](plan-0313-kvm-fast-fail-preamble.md) at `:22` and `:117`, and [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md) at `:151` (T10 header area).

All three say `onibus build excusable` — the correct subcommand group is `onibus flake excusable`. Verified: `flake` subcmd has `excusable`; `build` does not. The P0313 implementer caught this and wrote the correct `onibus flake excusable` at [`common.nix:117`](../../nix/tests/common.nix). P0304 T10 header will misdirect its implementer to the wrong subcmd group.

Three-site sed. P0313 is DONE so its doc is archaeology; P0304 is UNIMPL so its doc is live instruction — prioritize the P0304 fix.

### T23 — `docs:` docs-916455 QA nits — leading-zero, count slip, wrong P0207 filename

Four cosmetic fixes surfaced by the docs-916455 QA pass. All are prose/JSON errata in plan docs; zero behavior change.

**T23a — P0309:100 leading-zero soft_deps.** MODIFY [`.claude/work/plan-0309-helm-template-fodproxyurl-workerpool.md`](plan-0309-helm-template-fodproxyurl-workerpool.md) at `:100`:

```json
{"deps": [243], "soft_deps": [0308], ...}
```

`0308` with a leading zero is **invalid JSON** (RFC 8259 §6: "leading zeros are not allowed"). Python's `json.loads` actually rejects it; the docs-916455 batch missed this one because it wasn't in its sweep scope. Fix: `[0308]` → `[308]`.

**T23b — P0306:3 count slip.** MODIFY [`.claude/work/plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md`](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) at `:3` — opening says "**Five** coordinator/bughunter-surfaced harness bugs" then "All **four** caused real workflow failures" in the same sentence. The dag.jsonl note says "3 coordinator-surfaced bugs"; the title lists three ("merge 3-dot, lock lease, planner dag-append"). Append artifact from successive docs batch runs. Reconcile with the actual T-count at dispatch (`grep -c '^### T' plan-0306-*.md`).

**T23c+d — wrong P0207 filename (2 sites).** Both [`.claude/work/plan-0295-doc-rot-batch-sweep.md:141`](plan-0295-doc-rot-batch-sweep.md) (this file, T21's Risk prose) and [`.claude/work/plan-0304-trivial-batch-p0222-harness.md:326`](plan-0304-trivial-batch-p0222-harness.md) reference `plan-0207-tenant-key-build-gc-mark.md`. Actual filename: `plan-0207-mark-cte-tenant-retention.md` (verified: `ls .claude/work/plan-0207-*`). Both are prose Risk/context text, not Files-fence entries — the `json files` blocks are unaffected. Two-site filename fix.

### T24 — `docs:` P0295 T23c+d self-defeating exit criterion

**QA WARN accepted at docs-916455; implementer will spot at dispatch.** The exit criterion at `:215` greps for `plan-0207-tenant-key-build-gc-mark` across this file + P0304, expecting zero hits. But T23's own prose at `:175` contains the string (to describe what's being corrected), AND the criterion itself at `:215` contains it. The grep cannot reach 0 for `plan-0295-*.md`.

MODIFY this file at `:215` — narrow the grep scope:

```
- `grep 'plan-0207-tenant-key-build-gc-mark' .claude/work/plan-0295-*.md .claude/work/plan-0304-*.md | grep -vE ':(175|215):|T23' | wc -l` → 0 (T23c+d: wrong P0207 filename corrected at :141; T23 prose + criterion text intentionally match)
```

Alternative (cleaner but more churn): scope to exactly `:141` — `sed -n '141p' .claude/work/plan-0295-*.md | grep -c tenant-key-build-gc-mark` → 0. Implementer's call; the pipe-to-`grep -v T23` form is self-documenting.

### T25 — `docs:` known-flakes `-accel` pre-pivot text + retract false grep-recognition claim

**Partially superseded by [P0317](plan-317-excusable-vm-regex-knownflake-schema.md) T4** — the bracket-changelog migration to `mitigations: list[Mitigation]` rewrites the P0316 bracket and correctly uses `-machine accel=kvm`. If P0317 lands first, this T25 is a no-op for the `-accel` text; verify with grep at dispatch.

The followup row that introduced this T-item stated: *"New symptom string `failed to initialize kvm: Permission denied` is accurate and is what matters for grep-recognition."* **This statement is false** — bughunter mc21 caught it. [`known-flakes.jsonl:4`](../../.claude/known-flakes.jsonl) header says `symptom` is "for human cross-check"; match key is `test`. [`build.py:135-156`](../../.claude/lib/onibus/build.py) `excusable()` does not grep symptom strings. The `symptom` field is human-facing documentation only. The correction here is twofold:

MODIFY [`.claude/known-flakes.jsonl:11`](../../.claude/known-flakes.jsonl) (the `<tcg-builder-allocation>` row post-P0317-T3; or `vm-lifecycle-recovery-k3s` line 11 if P0317 hasn't landed) — **IF** the `[P0316 LANDED 900ac467: -accel kvm forces...]` bracket text still exists in `fix_description` (i.e., P0317 T4 hasn't migrated it), change `-accel kvm` → `-machine accel=kvm` to match [`common.nix:203-204`](../../nix/tests/common.nix). The actual shipped option is `[ "-machine" "accel=kvm" ]`; P0316 pivoted in [`9f7fee88`](https://github.com/search?q=9f7fee88&type=commits) after QEMU rejected the standalone `-accel` flag ([`common.nix:195-198`](../../nix/tests/common.nix) explains the pivot).

The `symptom` string `failed to initialize kvm: Permission denied` **is** accurate (that's what QEMU prints); it just doesn't participate in `excusable()` matching. No change needed to `symptom`.

### T26 — `docs:` plan-0316 frozen at iter-1 — sync prose with shipped `-machine accel=kvm`

MODIFY [`.claude/work/plan-0316-qemu-force-accel-kvm.md`](plan-0316-qemu-force-accel-kvm.md). P0316 is DONE; this is archaeology correction. The plan doc is frozen at iteration-1 (pre-pivot), shipping `-accel` prose while the code at [`common.nix:195-204`](../../nix/tests/common.nix) correctly uses `-machine accel=kvm` with a comment explaining why `-accel` was rejected.

| Line | Pre-pivot text | Correct |
|---|---|---|
| `:7` | `force virtualisation.qemu.options = [ "-accel" "kvm" ]` | `[ "-machine" "accel=kvm" ]` |
| `:42` | `later -accel wins` | `later -machine accel= property wins (QEMU merges -machine property sets)` — **and delete the claim that -accel can coexist**, it cannot (QEMU rejects mixing per `common.nix:195-197`) |
| `:44` | `virtualisation.qemu.options = [ "-accel" "kvm" ];` | `virtualisation.qemu.options = [ "-machine" "accel=kvm" ];` |
| `:73` | `qemu.options = [ "-accel" "kvm" ];` | `qemu.options = [ "-machine" "accel=kvm" ];` |
| `:87` | `[P0316 LANDED <sha>: -accel kvm forces...]` | `-machine accel=kvm forces` (this is the known-flakes bracket text — upstream of T25's fix) |
| `:105` | exit-criterion `grep -o '\-accel[^"]*'` | `grep -o -- '-machine accel=[^"]*'` — the current criterion returns empty against shipped code |

Lines `:1`, `:34`, `:102`, `:145` also say `-accel kvm` in prose — sweep all.

Add an erratum header after line `:1`:

> **[ITER-1 DOC — PIVOTED IN IMPL]:** This plan doc describes the iteration-1 approach (`-accel kvm` standalone flag). QEMU rejected it: `The -accel and "-machine accel=" options are incompatible` — nixpkgs `qemu-common.nix` bakes `-machine accel=kvm:tcg` into the exec prefix, which cannot coexist with standalone `-accel`. Shipped code uses `virtualisation.qemu.options = [ "-machine" "accel=kvm" ]` (a SECOND `-machine accel=` — QEMU merges property sets, later wins). See [`common.nix:186-204`](../../nix/tests/common.nix) for the authoritative explanation. Corrections applied inline below.

MODIFY [`.claude/dag.jsonl`](../../.claude/dag.jsonl) P0316 row `title` field: `"Force -accel kvm — ..."` → `"Force -machine accel=kvm — ..."` (grep `'"plan":316'`, edit `title`).

### T27 — `docs:` P0209 merger misinterpretation — exit-1 vs exit-77 = two gate levels, not a gap

**Process-note, not a file edit — goes in this plan's prose as record for future mergers reading fast-path history.** The P0209 merger (iter-2) reported "protocol-warm preamble did not fire — gap in preamble coverage." **Incorrect interpretation.** [`protocol.nix:258`](../../nix/tests/scenarios/protocol.nix) HAS `${common.kvmCheck}` (verified). What actually happened:

- **iter-1:** P0313's O_RDWR preamble caught a broken builder → **exit 77** + `KVM-DENIED-BUILDER` marker (Python `sys.exit(77)`)
- **iter-2:** kvmCheck's single CREATE_VM probe **succeeded** (builder had KVM at T0), then `protocol-warm`'s 3 concurrent QEMU children booted seconds later; 2/3 race-lost on their own CREATE_VM; P0316's `-machine accel=kvm` caught the race-losers → **exit 1** + `failed to initialize kvm: Permission denied` (QEMU's own error)

Two DIFFERENT stack levels fired on two different iterations. **Not a preamble gap — the stack working at both layers.** Exit code distinguishes which gate fired: exit-77 = preamble (Python), exit-1 = per-VM QEMU. Both are correctly fast-failing; they catch different failure modes (builder-broken-outright vs builder-races-under-concurrent-load).

The P0205-r2 merger's observation supports this: all 8 scenarios have kvmCheck ([`scheduling.nix:807`](../../nix/tests/scenarios/scheduling.nix), [`protocol.nix:258`](../../nix/tests/scenarios/protocol.nix), etc. — verified). The 5 tests that hit TCG (observability, protocol-cold/warm, scheduling-disrupt, security) are all **multi-VM** — preamble single-shot CREATE_VM succeeds, then N concurrent QEMU children each call CREATE_VM, some race-lose. Single-VM tests don't see this mode. This is **exactly** what P0316 was designed for — retrospective validation.

No file edit. This T-item is satisfied by existing as prose here. Future mergers grep `preamble.*gap` → land here.

### T28 — `docs:` rio-planner.md:127 — "because reading not writing" is wrong WHY

MODIFY [`.claude/agents/rio-planner.md`](../../.claude/agents/rio-planner.md) at `:127`.

Current prose: "relative `.claude/bin/onibus` resolves correctly there because you're reading, not writing". **Misleading.** The resolution works because of the `cd` at `:130` — `cd /root/src/rio-build/docs-<runid>` picks up the **worktree copy** of `onibus`, whose `REPO_ROOT` resolves to the worktree. Read vs write is irrelevant: if the planner invoked **worktree** onibus to `dag append` (a write), it would also work — the worktree's `dag.jsonl` would be written.

The direct-append (python3 -c inline JSON at `:115-122`) IS the right call — belt-and-suspenders, and the session-cached-agent-doc problem ([P0306](plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md) T3 fixed the onibus path-resolution but the **agent doc** describing how to call it is session-cached). But the `:127` justification muddles WHY.

The bash snippet at `:130` is correct (the `cd` IS doing the work). Low risk of misuse — but future readers build the wrong mental model. Change `:127` to:

```
Validate from inside the worktree — the `cd` picks up the WORKTREE copy of
onibus (its REPO_ROOT resolves to the worktree, so `dag validate` reads the
worktree's dag.jsonl). The direct-append above is belt-and-suspenders: agent
docs are session-cached, so onibus path-resolution fixes (P0306 T3) don't
reach this planner instance until a fresh spawn.
```

### T29 — `docs:` P0317 QA WARN dispatch fixes (belt-and-suspenders)

**P0317 implementer agent `ae11696fb0a10086d` is CURRENTLY IN FLIGHT with these fixes in their prompt** — this T-item is a doc-trail in case the agent doesn't land them (transcript-lost, abort, etc). If P0317 merges with these already fixed, T29 is a no-op — verify with grep at dispatch.

MODIFY [`.claude/work/plan-0317-excusable-vm-regex-knownflake-schema.md`](plan-0317-excusable-vm-regex-knownflake-schema.md) — three QA WARN dispatch-time fixes from the docs-922476 pass:

**T29a — `:21`/`:243`/`:358` `impl-1` → `impl-2`.** The log reference at `:21` says `rio-p0209-impl-1.log` but the hash `lbb1v37c` + line 15780 are from `impl-2`. The exit criterion at `:358` (if it extracts from the log) would fail as written — it extracts the wrong plan's output (rio-cli). Three-site fix: `impl-1` → `impl-2`.

**T29b — T6 test defeated.** P0317 T6's test premise is "bypass-validator on load" — but `read_jsonl` validates on load (pydantic model construction). The bypass doesn't exist; the test passes vacuously. Convert to `pytest.raises(ValidationError)` (assert the invalid fixture IS rejected) or delete T6.

**T29c — T1 comment: `^error:` anchor is load-bearing.** The `_VM_FAIL_RE` pattern at [`plan-0317:38`](plan-0317-excusable-vm-regex-knownflake-schema.md) has `^error:` — this **excludes** `BuildResultV3` debug-dump lines that also contain `Cannot build` but aren't the top-level nix error. Worth a one-line comment in the T1 code block noting the anchor is deliberate (future editor might "simplify" it away).

Also noted in the QA: P0304's dep on P0223 was stale at docs-922476 merge time (P0223 merged [`c0f6ebbc`](https://github.com/search?q=c0f6ebbc&type=commits) before docs-922476 landed) — but P0304's `deps=[...223...]` is still correct for dag-ordering (P0304 is UNIMPL; dep on a DONE plan is trivially satisfied). No fix needed.

### T30 — `docs:` P0273 T1/T5 unversioned tracey template → +2

MODIFY [`.claude/work/plan-0273-envoy-sidecar-grpc-web.md`](plan-0273-envoy-sidecar-grpc-web.md) at `:64` and `:260`. The T1/T5 code templates use UNVERSIONED `r[impl dash.envoy.grpc-web-translate]` and `r[verify dash.envoy.grpc-web-translate]` — but [P0326](plan-0326-dash-envoy-grpcweb-bump.md) bumped the marker to `+2` (see [`dashboard.md:26`](../../docs/src/components/dashboard.md)). Implementer copying the template verbatim → `tracey-validate` stale-annotation fail on day 1.

Fix: `s/dash.envoy.grpc-web-translate]/dash.envoy.grpc-web-translate+2]/` at both template sites. Codebase convention IS versioned (e.g., `gw.stderr.error-before-return+2`).

### T31 — `docs:` dashboard.md blank-line-after-marker — 3 siblings

MODIFY [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) at `:29-31`, `:33-35`, `:37-39`. Three markers have a **blank line between the marker and its paragraph** — tracey sees NO text → `tracey bump` silent no-op. Same defect-class as [`scheduler.md`](../../docs/src/components/scheduler.md) 4-marker gap from `f190e479` (P0304-T27 fixes the scheduler ones).

| Line | Marker | Fix |
|---|---|---|
| `:29-31` | `r[dash.journey.build-to-logs]` | delete blank line `:30` |
| `:33-35` | `r[dash.graph.degrade-threshold]` | delete blank line `:34` |
| `:37-39` | `r[dash.stream.log-tail]` | delete blank line `:38` |

P0326 fixed this for `r[dash.envoy.grpc-web-translate+2]` at `:26-27` but left the 3 siblings broken in the same file. **Work bottom-up** (`:38` → `:34` → `:30`) so line refs stay valid.

**Verification:** `nix develop -c tracey query rule dash.journey.build-to-logs` → text non-empty.

### T32 — `docs:` scheduler.md — add r[sched.classify.proactive-ema] marker

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) after `r[sched.classify.penalty-overwrite]` at [`:202-211`](../../docs/src/components/scheduler.md). [P0266](plan-0266-proactive-ema-from-worker-progress.md) adds a proactive-ema path that contradicts `r[sched.classify.penalty-overwrite]`'s "Detection happens post-completion, not mid-run" text at [`:209`](../../docs/src/components/scheduler.md). Either amend the existing marker (bump to `+2`) or add a new sibling marker.

**Recommend new marker** (proactive-ema is a distinct mechanism from penalty-overwrite — both update EMA but on different triggers):

```markdown
r[sched.classify.proactive-ema]
When a worker reports `memory_used_bytes > 0` in a `Progress` update, the scheduler proactively updates `ema_duration_secs` for the running derivation's `(pname, system)` key using the partial elapsed time. This gives the classifier fresher data for subsequent submissions of the same package even before the current build completes — useful for long-running builds where waiting for completion delays class-correction by hours. The proactive update uses standard EMA blending (not penalty-overwrite); it is recorded via `rio_scheduler_ema_proactive_updates_total`.
```

Insert after [`:211`](../../docs/src/components/scheduler.md) (the `misclassification_count` reservation paragraph) with a blank line before (marker at col 0). Then **the implementer** annotates [`db.rs:1300`](../../rio-scheduler/src/db.rs) with `// r[impl sched.classify.proactive-ema]` and the test at `:1924`/`:2024` with `// r[verify sched.classify.proactive-ema]` (p266 worktree refs).

**ALSO** amend `r[sched.classify.penalty-overwrite]` text at [`:209`](../../docs/src/components/scheduler.md): "Detection happens post-completion, not mid-run" → "Penalty-overwrite detection happens post-completion; proactive EMA updates (`r[sched.classify.proactive-ema]`) may fire mid-run from worker Progress reports." Run `tracey bump` to version-bump `penalty-overwrite` → `+2`.

### T33 — `docs:` server.rs:65 stale mint_session_jwt doc

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) at `:65-66` (p260 worktree ref). Doc says "round-trip does not exist yet — see TODO(P0260) at call site". `resolve_and_mint` EXISTS at `:433`; `auth_publickey` calls it at `:606`. The doc-comment didn't track TODO closure. Delete the stale sentence; replace with a pointer to `resolve_and_mint` and the call site.

### T34 — `docs:` server.rs jwt_issuance_tests — 3 future-tense P0260 refs

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) at `:885`, `:973`, `:995` (p260 worktree refs). Three doc-comments reference P0260 in future tense:
- `:885` — "gets it from P0260 resolve step" → resolved now
- `:973` — "until P0260 wires K8s Secret" → wired (main.rs:367-397)
- `:995` — "P0260 VM test covers the full flow" → WRONG: security.nix:697-701 says VM test covers ONLY the fallback branch; JWT-issue branch is unit-only

Update all three to present tense + accurate scope. The `:995` one is a CORRECTION not just tense-fix — the VM test does NOT cover JWT-issue.

### T35 — `docs:` lifecycle.nix ctrl.probe.named-service conflation — same fix as P0292

MODIFY [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) at `:47-52` and `:414-420` (p292 worktree refs). The `r[verify ctrl.probe.named-service]` prose has the SAME conflation P0292 fixed in main.rs — says "if the K8s readinessProbe probed `\"\"` instead, standby would pass readiness". K8s readinessProbe is `tcpSocket` — it doesn't probe gRPC at all. The test actually proves the CLIENT-SIDE BALANCER constraint via `grpc-health-probe` CLI.

Also `:50` line-ref `main.rs:380-392` → now `:416-430` post-P0292. Second site at `:414-420` (in-subtest comment, same stale ref + same K8s conflation).

### T36 — `docs:` r[impl ctrl.probe.named-service] — add second annotation at balance.rs

MODIFY [`rio-proto/src/client/balance.rs`](../../rio-proto/src/client/balance.rs) at `:309` (or `:88` — the probe fn). Post-P0292 spec text says the requirement applies to the CLIENT-SIDE balancer. Current `r[impl ctrl.probe.named-service]` sits at [`main.rs:416`](../../rio-controller/src/main.rs) (server-side `set_not_serving` on named svc). The `SCHEDULER_HEALTH_SERVICE` const at [`balance.rs:309`](../../rio-proto/src/client/balance.rs) + probe fn at `:88` is arguably the primary impl site.

Add a second `// r[impl ctrl.probe.named-service]` at `balance.rs:309`. Both sites are legitimate halves (server sets, client probes). Low-value but closes the spec-location gap.

### T37 — `docs:` verification.md — golden-matrix cold-cache cost

MODIFY [`docs/src/verification.md`](../../docs/src/verification.md) at `:27` (p300 worktree ref). 60-90min cold-cache cost for `.#golden-matrix` is documented in [`flake.nix:724`](../../flake.nix) comment and [`weekly.yml:5`](../../.github/workflows/weekly.yml) but NOT in the design doc's Multi-Nix matrix section. One-line addition after the `ls result/` line:

```markdown
Cold-cache build time ~60-90min (three full Nix source-tree builds); subsequent warm runs are minutes.
```

### T38 — `docs:` flake.nix nix-stable — branch-deletion note

MODIFY [`flake.nix`](../../flake.nix) at `:26` (p300 worktree ref). `nix-stable` locked at 2024-11-01 rev (`2.20-maintenance`). If upstream deletes the branch, `nix flake update nix-stable` fails — but the locked rev is safe (nixpkgs caches tarballs). One-line comment noting branch-deletion is survivable (lockfile pins the rev; only explicit update breaks). Resolves P0300 seed question 2: no graceful handling needed, lock pin IS the handling.

### T39 — `docs:` controller.md — note on r[ctrl.pool.ephemeral] marker granularity

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) near P0296's `r[ctrl.pool.ephemeral]` marker (location arrives with P0296 merge; re-grep at dispatch). The marker has 5× `r[impl]` annotations across 3 files (ephemeral.rs:101,274 + mod.rs:117 + main.rs:106,618 — p296 refs). Feature IS cross-cutting (controller branch / Job builder / worker single-shot gate), so each site is legitimate. But `tracey query rule ctrl.pool.ephemeral` output is unnavigable with 5 impl sites.

Add a one-line note in the marker's paragraph pointing at [P0347](plan-0347-ephemeral-pool-match-exit-semantics.md)'s sub-markers (`ctrl.pool.ephemeral-deadline`, `ctrl.pool.ephemeral-single-build`) as the pattern for future splits. Don't split the existing 5× here — P0347 adds the sub-markers organically.

### T10 — `docs:` plan-0222 metric-name corrections (P0222)

MODIFY [`.claude/work/plan-0222-grafana-dashboards.md`](plan-0222-grafana-dashboards.md) at `:18`, `:20`, `:30`, `:33`, `:44-47`, `:55`, `:56` — the plan's T1-T4 tables reference **9 nonexistent metric names**. Plan line 5 says "DO NOT invent metric names"; line 22 says grep-verify. None of the listed names appear in `rio-*/src/`, [`observability.md`](../../docs/src/observability.md), or any other plan doc. The implementer correctly substituted per exit-criterion 3 and shipped what exists ([`6b723def`](https://github.com/search?q=6b723def&type=commits)). The plan doc should record what shipped, not what was guessed at planning time.

| Task | Plan said | Shipped (per [`observability.md`](../../docs/src/observability.md)) |
|---|---|---|
| T1 :18 | `rio_scheduler_derivations_completed_total` | `rio_scheduler_builds_total` (labeled `outcome`) |
| T1 :20 | `rio_scheduler_builds_by_status` | `rio_scheduler_builds_total` by `outcome` |
| T2 :30 | `rio_controller_workerpool_replicas{class}` | `rio_controller_workerpool_replicas{pool}` (label is `pool` not `class`) |
| T2 :33 | `rio_scheduler_ready_queue_depth` | `rio_scheduler_class_queue_depth` |
| T3 :44 | `rio_store_cache_hits_total` / `_misses_total` | check shipped [`store-health.json`](../../infra/helm/grafana/store-health.json) — 4 names flagged by validator |
| T4 :55 | `rio_scheduler_dispatch_latency` | `rio_scheduler_assignment_latency` |
| T4 :56 | `rio_scheduler_ready_queue_depth` | `rio_scheduler_derivations_queued` + `_derivations_running` (split gauge) |

Rewrite the T1-T4 PromQL tables to match shipped JSONs. Keep the prose ("verify each metric exists") — it was correct instruction; the table rows were wrong.

### ~~T40 — `docs:` migration-018 same-txn comment contradicts impl~~ — OBE (P0353 landed)

> [P0353](plan-0353-sqlx-migration-checksum-freeze.md) (DONE at [`a503e7d4`](https://github.com/search?q=a503e7d4&type=commits)) stripped `.sql` commentary to `rio-store/src/migrations.rs::M_018`. But the landed `M_018` at `:52-59` has a DIFFERENT wrong claim: it says "PutChunk wraps both inserts in one BEGIN..COMMIT" — contradicted by [`chunk.rs:262-263`](../../rio-store/src/grpc/chunk.rs) ("Not in a single transaction ... autocommit"). T40's correction intent survives as [P0304-T85](plan-0304-trivial-batch-p0222-harness.md) (prod-code doc fix, not plan-doc erratum anymore).

**Original T40 content preserved below for archaeology.**

MODIFY [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql) at `:15-17`. Current:

```sql
-- AFTER the chunks row exists (PutChunk does INSERT chunks -> then
-- INSERT chunk_tenants, same txn). No race; FK enforces referential
```

But [`rio-store/src/grpc/chunk.rs:262-269`](../../rio-store/src/grpc/chunk.rs) explicitly documents the OPPOSITE: "Not in a single transaction with the chunks insert: the chunks row has already committed (autocommit). If THIS insert fails, we're left with a chunk attributed to nobody — same state as a pre-migration-018 chunk." The impl is correct (two autocommit statements at `:252` + `:276`, no begin/commit wrapping). The migration comment is wrong.

Replace `:15-16` with:

```sql
-- AFTER the chunks row exists (PutChunk does INSERT chunks, autocommit,
-- then INSERT chunk_tenants, autocommit — two separate statements, NOT
-- one txn). No race: chunks commits BEFORE junction → FK satisfied
-- sequentially. If junction insert fails, chunk row stands unattributed;
-- tenant's next FindMissingChunks says "missing" → retry self-heals.
```

The `:17` "No race" claim is TRUE but for a different reason (sequential commit, not txn atomicity). Preserved with corrected rationale. **Coordinate with [P0350](plan-0350-chunk-tenants-junction-cleanup-on-gc.md)-T2** which rewrites `:17-18` (CASCADE dead-code honest comment) — adjacent lines, apply both in one pass or sequence P0295-T40 first. discovered_from=bughunter(mc91).

### T41 — `docs:` rio-impl-validator.md phantom_amend contradictory prose

MODIFY [`.claude/agents/rio-impl-validator.md`](../../.claude/agents/rio-impl-validator.md) at `:48`. Current: "No validation-relaunch needed; `behind-check` post-rebase shows `behind=0`." But `:36` says "If `behind > 0`, return immediately — **do not verify**" and `:8` says validator is read-only (cannot rebase itself). Contradictory.

What the prose MEANS: no SendMessage-to-impl roundtrip needed; coordinator can mechanically `git rebase $TGT` in the worktree, then relaunch the validator. Reword:

```markdown
**When `phantom_amend: true`:** the merger's step-7.5 amend orphaned
this worktree's base — it rebased onto the pre-amend SHA during the
ff→amend window. Mechanical fix: **coordinator** runs `git rebase $TGT`
in the worktree (git auto-drops the "patch contents already upstream"
commit), then **relaunches** this validator. No SendMessage-to-impl
roundtrip needed — the rebase is mechanical and doesn't change feature
code. Include `phantom_amend: true` in your BEHIND report so the
coordinator skips the ~30s hand-diagnosis.
```

The "`behind-check` post-rebase shows `behind=0`" is implicit (that's what makes the relaunch succeed). The contradiction was "no validation-relaunch needed" — the validator IS relaunched; what's not needed is the impl-agent roundtrip. discovered_from=346.

### T42 — `docs:` multi-tenancy.md stale P0260 SIGHUP ref

MODIFY [`docs/src/multi-tenancy.md`](../../docs/src/multi-tenancy.md) at `:19` — blockquote still says "SIGHUP hot-swap remain scheduled at P0260 — until then pubkey is None". **False now:** [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) wires `spawn_pubkey_reload` in both scheduler+store main.rs. P0349's scrub commit [`a294380e`](https://github.com/search?q=a294380e&type=commits) missed this file. Post-P0349-merge. discovered_from=349.

### T43 — `docs:` grpc/tests.rs assert-message stale P0260 forward-ref

MODIFY [`rio-scheduler/src/grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) at `:1582` — assert message says "this is every pre-P0260 deploy" — stale forward-ref. P0349 landed the wiring; correct phrasing: "every key-unset deploy" or "every deploy without `jwt.key_path` configured". Scrub commit `a294380e` missed this file (`grpc/tests.rs` not in its changed-file set). Post-P0349-merge. discovered_from=349.

### T44 — `docs(test-support):` spawn_grpc_server_layered — ResBody param undocumented

MODIFY [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) at `:1019-1043`. [P0351](plan-0351-spawn-grpc-server-layered-generic.md) added `spawn_grpc_server_layered<L, ResBody>`. The docstring covers the `L` parameter ("mirrors tonic 0.14's `Router<L>::serve_with_incoming` bounds") but never explains **`ResBody`** — what it is, why it's needed, or why callers never spell it. Reviewers asked "what's ResBody for". Add after the existing `where`-clause paragraph at `:1033-1038`:

```rust
/// `ResBody` is the layer's HTTP response body type — each layer can
/// transform the body (e.g., compression, tracing wrappers), so tonic's
/// generic `Router<L>` can't assume `tonic::body::Body`. Callers never
/// spell this: inference flows from `.layer(...)` through `L::Service`'s
/// `Response = http::Response<ResBody>` associated type. It's here only
/// to satisfy tonic's `serve_with_incoming` bound chain.
```

Post-P0351-merge. discovered_from=351.

### T45 — `docs:` CLAUDE.md stale TODO(P0286) example

MODIFY [`CLAUDE.md`](../../CLAUDE.md) at `:195-197`. The "GOOD: points to the blocking plan" example cites `TODO(P0286)` with text matching [`rio-worker/src/cgroup.rs:301`](../../rio-worker/src/cgroup.rs). Once [P0286](plan-0286-privileged-hardening-device-plugin.md) merges, T6 deletes that TODO — the example then references a closed plan and a deleted comment. Replace with a generic example (no plan-number coupling) OR rotate to a currently-open plan's TODO. Generic is more durable:

```rust
// GOOD: points to the blocking plan
// TODO(P0NNN): this path is unreachable under the escape-hatch
// config. Exercise it once the production path (see ADR-NNN) lands.
```

Post-P0286-merge (the stale-ness only manifests then). discovered_from=286(review). Root-level `CLAUDE.md` — outside Files-fence regex until [P0304-T1](plan-0304-trivial-batch-p0222-harness.md) lands; use the prose escape-hatch pattern.

### T46 — `docs:` P0358 T1 vs DESIGN-NOTE prose contradiction

MODIFY [`.claude/work/plan-0358-phantom-amend-stacked-detection.md`](plan-0358-phantom-amend-stacked-detection.md) at `:31-33` and `:51`. T1's prose at `:31` says the message-match check "already works for stacked amends"; the DESIGN NOTE at `:198` says "THE CHECK AS WRITTEN DOESN'T GENERALIZE CLEANLY for behind>=2" and recommends approach 2 (orphan-base via `for-each-ref --contains`). The T2 test scaffold at `:114-118` and `:193-196` admits the same. The two sections prescribe opposite mechanisms for the same task.

Align T1 with DESIGN-NOTE's approach 2: change `:31-33` from "Change `if behind == 1:` → `if behind >= 1:`. The subsequent check ... already works for stacked amends" to:

```markdown
MODIFY [`.claude/lib/onibus/git_ops.py`](../../.claude/lib/onibus/git_ops.py) at `:208`. Change `if behind == 1:` → `if behind >= 1:` AND switch the detection signal from diff-subset+message-match to orphan-base check (`for-each-ref --contains <pre_amend>` → empty). The diff-subset check does NOT generalize to behind≥2 (pre_amend vs tip diff includes intermediate-merge feature files — see DESIGN NOTE below). The orphan-base check is the definitional signal: only an amend leaves a reflog-only SHA with no ref. Message-match stays as belt-and-suspenders against the recent-N integration-branch commits (not just tip).
```

And at `:51` ("Coordinate with P0304-T64/T65") clarify which conditions survive: T65's truthiness-guard drop applies to the OLD diff-subset check; if T1 here switches to orphan-base, the diff-subset block is DELETED not amended — re-verify at dispatch whether T65's test still applies to the post-T1 code shape. discovered_from=coordinator.

### T47 — `docs:` P0352 line-drift — :326, :270-345 refs vs post-P0345 sprint-1

MODIFY [`.claude/work/plan-0352-putpathbatch-hoist-signer-lookup.md`](plan-0352-putpathbatch-hoist-signer-lookup.md). The plan cites [`put_path_batch.rs:326`](../../rio-store/src/grpc/put_path_batch.rs) (`maybe_sign` loop call), `:67-70` (tenant_id extraction), `:313` (tx begin); and [`grpc/mod.rs:270-345`](../../rio-store/src/grpc/mod.rs) (`maybe_sign` body). [P0345](plan-0345-put-path-validate-metadata-helper.md) (DONE at [`a5253d63`](https://github.com/search?q=a5253d63&type=commits)) extracted `validate_put_metadata` + `apply_trailer` to `mod.rs` — the batch file shifted by dozens of lines. Re-verify at dispatch: `grep -n 'maybe_sign\|tenant_id\|\.begin(' rio-store/src/grpc/put_path_batch.rs rio-store/src/grpc/mod.rs`. Update line-cites OR (preferred, per CITATION-TIERING principle) rewrite as function-name anchors ("in the phase-3 `for (idx, accum)` loop", "at `tenant_id` extraction") which don't drift. discovered_from=352-review.

### T48 — `docs:` P0304-T67 targets shifted post-P0345 — :160 extracted, :363/:389 drifted to :316/:342

MODIFY [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md) at T67. T67 targets `put_path_batch.rs:160/:363/:389` comment line-cites to `put_path.rs`. After [P0345](plan-0345-put-path-validate-metadata-helper.md) (DONE) extracted the `:160` region to `mod.rs::validate_put_metadata`, that cite is GONE. The `:363/:389` cites drifted to [`:316`](../../rio-store/src/grpc/put_path_batch.rs) (says "put_path.rs:629-641" — content-index insert) and [`:342`](../../rio-store/src/grpc/put_path_batch.rs) (says "put_path.rs:645 parity" — bytes counter). Update T67: delete the `:160` bullet (OBE — extracted to helper); retarget `:363`→`:316`, `:389`→`:342`. discovered_from=345-review (P0345 merged during planning).

### T49 — `docs:` P0304-T62 drift direction — "+6" sign/magnitude re-verify post-P0345

MODIFY [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md) at T62. T62 says "chunked.rs test doc-comment line-refs stale by +6 post-P0342." Between P0342 and now, P0345 also touched `put_path_batch.rs` (extraction net-negative). At dispatch: `grep -n ':277\|:280\|:284' rio-store/tests/grpc/chunked.rs` to find the stale refs, then grep the CONCEPTS they point at in current `put_path_batch.rs`. Correct T62's drift estimate — it may be larger or smaller than "+6" now. discovered_from=345-review.

### T50 — `docs:` P0295-T40 OBE — P0353 landed, .sql commentary stripped

MODIFY THIS FILE at T40 (`:359-380`). [P0353](plan-0353-sqlx-migration-checksum-freeze.md) (DONE at [`a503e7d4`](https://github.com/search?q=a503e7d4&type=commits)) stripped ALL commentary from [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql), moving it to [`rio-store/src/migrations.rs::M_018`](../../rio-store/src/migrations.rs). T40's target lines (`:15-16` same-txn comment) no longer exist in the `.sql`. Strike T40 header with `~~` and add an OBE note:

```markdown
### ~~T40 — `docs:` migration-018 same-txn comment contradicts impl~~ — OBE (P0353 landed)

[P0353](plan-0353-sqlx-migration-checksum-freeze.md) (DONE) stripped
`.sql` commentary to `rio-store/src/migrations.rs::M_018`. But the
landed `M_018` at `:52-59` has a DIFFERENT wrong claim: it says
"PutChunk wraps both inserts in one BEGIN..COMMIT" — contradicted by
[`chunk.rs:262-263`](../../rio-store/src/grpc/chunk.rs) ("Not in a
single transaction ... autocommit"). T40's correction intent survives
as [P0304-T85](plan-0304-trivial-batch-p0222-harness.md) (prod-code
doc fix, not plan-doc erratum anymore).
```

discovered_from=353-review (P0353 merged during planning; retarget).

### T51 — `docs:` P0345 plan-doc archaeology — leading-zero + word dup (low-value, DONE-plan)

MODIFY [`.claude/work/plan-0345-put-path-validate-metadata-helper.md`](plan-0345-put-path-validate-metadata-helper.md) — TWO cosmetic plan-doc fixes for archaeology (P0345 is DONE, plan frozen):

1. `:182` `json deps` fence has `"soft_deps": [342, 304, 0344]` — `0344` leading-zero. Fix to `344` for JSON-parser cleanliness (4th instance; [P0304-T28](plan-0304-trivial-batch-p0222-harness.md) rule).
2. Doubled word — `grep -E '\b(\w+) \1\b' .claude/work/plan-0345-*.md` at dispatch; fix inline.

**Low-value.** Plan doc is DONE-archaeology now; neither affects anything operational. Apply only if doing a sweep pass on DONE docs. discovered_from=345-review. (The SUBSTANTIVE P0345 finding — `apply_trailer` vestigial `[u8; 32]` return in landed code — is [P0304-T84](plan-0304-trivial-batch-p0222-harness.md), not here.)

### T52 — `docs(obs):` Histogram Buckets table — missing build_graph_edges row

MODIFY [`docs/src/observability.md:199-206`](../../docs/src/observability.md) — [P0321](plan-0321-build-graph-edges-histogram-buckets.md) added `rio_scheduler_build_graph_edges` to [`HISTOGRAM_BUCKET_MAP`](../../rio-common/src/observability.rs) at [`observability.rs:310`](../../rio-common/src/observability.rs), and the inline buckets are documented at [`:120`](../../docs/src/observability.md) in the Scheduler Metrics table. But the dedicated Histogram Buckets table at `:199-204` is now incomplete vs the code map (6 entries vs 4 rows).

Insert after `:204` (`assignment_latency_seconds`):

```markdown
| `rio_scheduler_build_graph_edges` | `[100, 500, 1000, 5000, 10000, 20000]` (count) |
```

Also: [`observability.rs:268`](../../rio-common/src/observability.rs) doc-comment cites `observability.md:119` but the actual spec text is at `:120` (off-by-one drift). Fix to `:120` — OR drop the line number entirely and anchor by section name (`"Histogram Buckets" section`) since line cites in cross-file refs rot.

discovered_from=321-review. **[P0363](plan-0363-upload-references-count-buckets.md)-T4 adds the adjacent `upload_references_count` row** to the same table — both are pure row-insert, rebase-clean either order.

### T53 — `docs(tests):` lifecycle.nix:441 triple-stale cite — main.rs:676→jwt_interceptor.rs:203

MODIFY [`nix/tests/scenarios/lifecycle.nix:441-445`](../../nix/tests/scenarios/lifecycle.nix) — the `jwt-mount-present` subtest comment ([P0357](plan-0357-helm-jwt-pubkey-mount.md)) says:

```
# is implicit: main.rs:676 calls load_jwt_pubkey(path).await?
# with `?` propagation. A bad key = process exits non-zero =
# CrashLoopBackOff = waitReady never returns. The prelude's
# waitReady already proved scheduler+store+gateway are all
# Running, which means load_jwt_pubkey succeeded in each.
```

Three staleness layers after [P0355](plan-0355-extract-spawn-drain-load-wire-jwt.md) (extract-to-helper):
1. **Location:** `main.rs:676` → actual is [`main.rs:645`](../../rio-scheduler/src/main.rs) (scheduler) / [`main.rs:467`](../../rio-store/src/main.rs) (store)
2. **Function name:** `load_jwt_pubkey` → `load_and_wire_jwt` (wrapper that calls `load_jwt_pubkey` internally at [`jwt_interceptor.rs:203`](../../rio-common/src/jwt_interceptor.rs))
3. **Async-ness:** `.await?` → `?` (sync; `load_and_wire_jwt` is a blocking fn that spawns the reload task, not async)

The **reasoning** (fail-fast via `?` propagation → CrashLoopBackOff → waitReady proves success) still holds. Only the citation rots. Fix:

```
# is implicit: rio-scheduler/src/main.rs and rio-store/src/main.rs
# call load_and_wire_jwt(...)? with `?` propagation. A bad key =
# process exits non-zero = CrashLoopBackOff = waitReady never returns.
# The prelude's waitReady already proved scheduler+store+gateway are
# all Running, which means key-load succeeded in each. (The gateway
# side loads the SIGNING seed, not the pubkey — same fail-fast
# pattern via the same ?-propagation.)
```

Anchored by fn name, not line number — `load_and_wire_jwt` is grep-stable. discovered_from=357-review.

### T54 — `docs(flake):` maybeMissing ./.sqlx comment — rationale stale post-P0297

MODIFY [`flake.nix`](../../flake.nix) — [P0297](plan-0297-sqlx-query-macro-conversion.md)'s fileset entry at `~:210-212` (p297 branch) reads:

```nix
# crane depsOnly build. maybeMissing: a fresh clone before
# the first prepare run won't have the dir yet.
(pkgs.lib.fileset.maybeMissing ./.sqlx)
```

But P0297 also commits `.sqlx/*.json` (14 query files). Once merged, a fresh clone **will** have `.sqlx/` — the "before the first prepare run" rationale is stale on arrival.

Either (a) drop `maybeMissing` (dir is always present post-P0297, `./.sqlx` alone works), or (b) reword to "resilience against accidental `.sqlx/` deletion during dev — `cargo sqlx prepare` regenerates." Prefer (b): `maybeMissing` is harmless redundancy and keeps the build working if someone `rm -rf .sqlx` during debugging; just the comment needs truth.

**Gated on P0297 merge** — the fileset entry doesn't exist on sprint-1 yet. discovered_from=297-review.

### T55 — `docs(store):` 2× sign_for_tenant stale refs — resolve_once is the live path post-P0352

MODIFY two comments that reference `sign_for_tenant` by name post-[P0352](plan-0352-putpathbatch-hoist-signer-lookup.md) refactor (production code now uses `resolve_once` + `sign_with_resolved`):

- [`rio-store/src/grpc/mod.rs:856`](../../rio-store/src/grpc/mod.rs) — `// (the cluster-key path in sign_for_tenant skips the DB entirely).` → `// (the cluster-key path in resolve_once skips the DB entirely).` — the code at `:476` calls `sign_with_resolved` after a `maybe_sign`-mediated `resolve_once`, not `sign_for_tenant`.
- [`rio-store/src/main.rs:329`](../../rio-store/src/main.rs) — `// sign_for_tenant; no extra DB roundtrip on the None path.` → `// resolve_once (via maybe_sign); no extra DB roundtrip on the None path.` — same: the main.rs wiring comment describes the tenant-signer path, which is now `resolve_once`-shaped.

If [P0304-T91](plan-0304-trivial-batch-p0222-harness.md) dispatches first and deletes `sign_for_tenant` entirely, these comments become MUST-fix (dangling ref to a deleted fn); otherwise they're stale-but-correct (the fn exists, just isn't on this code path). Either way, reword to the live fn name. discovered_from=352-review.

### T56 — `docs(work):` P0352 plan-doc T3 — "used by admin.rs" claim false

MODIFY [`.claude/work/plan-0352-putpathbatch-hoist-signer-lookup.md:139`](plan-0352-putpathbatch-hoist-signer-lookup.md) — the T3 prose says:

> `sign_for_tenant` at [`signing.rs:219`](../../rio-store/src/signing.rs) stays (used by [`admin.rs`](../../rio-store/src/grpc/admin.rs)).

But `admin.rs` doesn't call `sign_for_tenant` — it uses [`cluster()`](../../rio-store/src/signing.rs) at `:205` for ResignPaths backfill (historical paths have no tenant attribution). `sign_for_tenant` has zero production callers post-P0352; only the `:561/:605/:638` unit tests call it.

DONE-plan archaeology (P0352 merged). Add erratum: `> **ERRATUM (post-merge):** sign_for_tenant has no admin.rs caller — admin.rs uses cluster() for ResignPaths. Dead-code collapse tracked at [P0304-T91](plan-0304-trivial-batch-p0222-harness.md).` discovered_from=352-review.

### T57 — `docs(obs):` Worker Metrics table — 2 missing rows (upload_skipped_idempotent, fuse_circuit_open)

MODIFY [`docs/src/observability.md:156-173`](../../docs/src/observability.md) Worker Metrics table — both metrics are described ([`lib.rs:101,129`](../../rio-worker/src/lib.rs)) and emitted ([`upload.rs:675`](../../rio-worker/src/upload.rs), [`circuit.rs:154,182,205`](../../rio-worker/src/fuse/circuit.rs)) but NOT in the spec table. `fuse_circuit_open` IS in the [`WORKER_METRICS` test const](../../rio-worker/tests/metrics_registered.rs) at `:24` (const>doc drift); `upload_skipped_idempotent_total` is in NEITHER.

Insert after `:168` (`upload_bytes_total`):

```markdown
| `rio_worker_upload_skipped_idempotent_total` | Counter | Outputs skipped before upload because `FindMissingPaths` reports them already-present in the store. Idempotency short-circuit — nonzero is healthy (repeat builds of cached paths). See [`r[worker.upload.idempotent-precheck]`](components/worker.md). |
```

Insert after `:164` (`fuse_fallback_reads_total`) or near other FUSE gauges:

```markdown
| `rio_worker_fuse_circuit_open` | Gauge | FUSE circuit-breaker open state (1 = open/tripped, 0 = closed/healthy). Set to 1 when store fetch error rate exceeds threshold; FUSE ops return EIO instead of blocking. Reset to 0 on successful probe. Alert if sustained 1. |
```

**Also** MODIFY [`rio-worker/tests/metrics_registered.rs:8-28`](../../rio-worker/tests/metrics_registered.rs) — add `"rio_worker_upload_skipped_idempotent_total",` to the `WORKER_METRICS` const (after `:20` `upload_bytes_total` for doc-table ordering parity). The const's doc-comment at `:7` says "Metric names from observability.md's Worker Metrics table" — T57 makes that true again.

discovered_from=363-review. **Adjacent to T52** (same table, both pure row-insert, rebase-clean either order). **Adjacent to [P0363](plan-0363-upload-references-count-buckets.md)-T4** (Histogram Buckets table, different section, same file — P0363 at `:206`, T57 at `:156-173`).

### T58 — `docs(scheduler):` rebalancer "config-driven" claim — spawn_task hardcodes `::default()`

MODIFY [`docs/src/components/scheduler.md:218`](../../docs/src/components/scheduler.md). The `r[sched.rebalancer.sita-e]` spec text says "All three parameters are config-driven via `scheduler.toml [rebalancer]` — workload-dependent, operator tunes." But [`rebalancer.rs:326`](../../rio-scheduler/src/rebalancer.rs) inside `spawn_task` hardcodes `let cfg = RebalancerConfig::default();` — the struct has no `Deserialize` derive ([`:44`](../../rio-scheduler/src/rebalancer.rs)), `main.rs` never loads a `[rebalancer]` TOML section, and `spawn_task` takes no config param. The spec describes an API that doesn't exist.

**Coordinate with [P0304-T95](plan-0304-trivial-batch-p0222-harness.md)** (the CODE-side fix — adds `Deserialize` + threads config from `main.rs`). Two paths:

- **If P0304-T95 lands first:** T58 becomes a no-op — the spec becomes accurate. Skip at dispatch (grep `Deserialize` on `RebalancerConfig`).
- **If T58 lands first:** Replace "config-driven via `scheduler.toml [rebalancer]`" with "defaults are `min_samples=500, ema_alpha=0.3, lookback_days=7` — not currently config-exposed." When P0304-T95 lands, it reverts T58 to the original spec text.

Prefer landing P0304-T95 first (it makes the spec true instead of making the spec honest-about-being-incomplete). If dispatch order forces T58 first, use a `> **Scheduled:** [P0304-T95](plan-0304-trivial-batch-p0222-harness.md) — config-load wiring` blockquote instead of rewriting the paragraph. discovered_from=230-review.

### T59 — `docs(infra):` rbac.yaml:93 "List only" — watcher also gets DisruptionTarget conditions via watch

MODIFY [`infra/helm/rio-build/templates/rbac.yaml:93-94`](../../infra/helm/rio-build/templates/rbac.yaml). The comment says "List only — cleanup enumerates pods by label to DrainWorker each. No mutation — pod lifecycle is the StatefulSet controller's job." But `:97` grants `get, list, watch` on pods — the `watch` verb exists specifically for [P0285](plan-0285-drainworker-disruptiontarget-watcher.md)'s DisruptionTarget watcher ([`disruption.rs:75`](../../rio-controller/src/reconcilers/workerpool/disruption.rs) — `watcher(pods, cfg)`). The comment predates P0285 and only mentions the finalizer-cleanup list use-case.

Replace with: "List/watch: the DisruptionTarget watcher ([`disruption.rs`](../../rio-controller/src/reconcilers/workerpool/disruption.rs)) watches for `status.conditions[DisruptionTarget=True]` to fire preemptive DrainWorker; the finalizer enumerates pods by label for graceful scale-to-0. No mutation — pod lifecycle is the StatefulSet controller's job." discovered_from=285-review.

### T60 — `docs(work):` plan-0364:33 regex description — over-eager, doesn't match commit body

MODIFY [`.claude/work/plan-0364-netpol-f541-fstring-fix.md:33-46`](../../.claude/work/plan-0364-netpol-f541-fstring-fix.md). The dispatch-scan regex at `:42` is `re.finditer(r'f\"([^\"]*)\"', line)` — this matches every `f"…"` per **segment**, not per **implicitly-concatenated group**. A line like `f"a {x} " f"b"` yields two matches (correct for F541 — the second segment lacks `{…}` and trips pyflakes). But the `:33` explanation says "Python implicit concat is per-literal" then gives a recipe that per-line-scans — it's correct, just phrased as if F541 operates on the concatenated result when it actually operates per-literal-segment. The fixed commit ([`49e3d34b`](https://github.com/search?q=49e3d34b&type=commits)) body is correct; only the plan doc explanation drifted.

Clarify `:33` to: "pyflakes F541 fires per **literal segment**, not per concatenated result — `f\"a {x} \" f\"b\"` → the second `f`-prefix is redundant (no `{…}` in `\"b\"`). Strip the `f` from placeholder-free segments; the concat result is byte-identical." Plan-doc archaeology (P0364 is UNIMPL so the dispatch scan is still live guidance). discovered_from=364-review.

### T61 — `docs(tests):` k3s-full.nix:466-471 "not wired here" — stale post-P0360

MODIFY [`nix/tests/fixtures/k3s-full.nix:466-471`](../../nix/tests/fixtures/k3s-full.nix). The comment at `:470-471` says "Production uses the device-plugin path (ADR-012); that's not wired here (smarter-device-manager image not in airgap set)." [P0360](plan-0360-device-plugin-vm-coverage.md) T1 adds the image to the airgap set and T2/T3 wire the nonpriv path. Once P0360 lands, this comment is a lie — the device-plugin path IS wired via `vmtest-full-nonpriv.yaml` + the `valuesFile` parameter.

Replace with: "The default `valuesFile` (`vmtest-full.yaml`) uses `privileged: true` (fast path — skips the device-plugin DS bring-up ~30-60s). `vmtest-full-nonpriv.yaml` exercises the production ADR-012 path via smarter-device-manager (see `security.nix:privileged-hardening-e2e`)." **Conditional on P0360 merge** — check at dispatch: if P0360 still UNIMPL, this fix is premature (the comment is accurate today). discovered_from=360-review.

### T62 — `docs(infra):` dashboard-gateway.yaml r[impl] version mismatch — add +2

MODIFY [`infra/helm/rio-build/templates/dashboard-gateway.yaml:3`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml). The documentary marker `# r[impl dash.envoy.grpc-web-translate]` lacks the `+2` suffix — [`dashboard.md:26`](../../docs/src/components/dashboard.md) has `r[dash.envoy.grpc-web-translate+2]` (bumped by P0326). The `:4` comment says "marker is documentary — tracey does not scan yaml" which is true, but human readers grep for the marker text and find a stale version. **Same fix-class as T30** (which fixed the P0273 plan doc references). One-char fix. discovered_from=273-review.

### T63 — `docs(obs):` observability.nix parenting-vs-link — commit observed result to spec

[`nix/tests/scenarios/observability.nix:313-355`](../../nix/tests/scenarios/observability.nix) has an observe-only block that `print()`s `"CONFIRMED: span_from_traceparent → PARENTING"` or `"→ LINK only"` based on whether worker span's `parentSpanId` matches a scheduler `spanId`. The block's own comment at `:323` says "The doc text at `r[sched.trace.assignment-traceparent]` gets tightened based on this observation." But [`observability.md:268`](../../docs/src/observability.md) STILL says *"Whether this produces parent-child (same trace_id) or a link depends on the tracing layer's enter-time resolution; the VM observability test observes which."* — P0287 T7 ran, the result was observed, but the spec text was never updated.

**At dispatch:** grep recent VM logs (or `/nixbuild .#checks.x86_64-linux.vm-observability-k3s` if KVM available) for `CONFIRMED: span_from_traceparent` to get the observed answer. Then rewrite `observability.md:268` to state the resolved behavior. Delete the "VM observability test observes which" hedge — it's no longer an open question. Leave the observability.nix block as a regression guard (change `if overlap:` / `else:` print to `assert overlap` or `assert not overlap` matching the observed behavior). discovered_from=287-review.

### T64 — `docs(code):` prose `r[...]` refs in doc-comments — rephrase to avoid tracey misparse

[`rio-proto/src/interceptor.rs:145`](../../rio-proto/src/interceptor.rs) has `/// r[obs.trace.scheduler-id-in-metadata].` — `r[...]` at column-0 of the doc-comment content (after `/// `), which tracey's comment-scanner may parse as a malformed annotation (not `r[impl ...]` or `r[verify ...]`, just bare `r[...]`). Same class as P0229/P0289's "doc-comment prose `r[verify]` parsed as malformed annotation" gotcha (see [`tracey-adoption.md`](../../.claude/memory/tracey-adoption.md)).

Five siblings in the grep: [`interceptor.rs:145`](../../rio-proto/src/interceptor.rs), [`rio-scheduler/src/actor/worker.rs:621`](../../rio-scheduler/src/actor/worker.rs), [`rio-scheduler/src/actor/build.rs:32`](../../rio-scheduler/src/actor/build.rs), [`rio-scheduler/src/actor/command.rs:312`](../../rio-scheduler/src/actor/command.rs), [`rio-gateway/src/handler/mod.rs:106`](../../rio-gateway/src/handler/mod.rs). Each has `r[...]` as prose text, not as an annotation. Rephrase each: `r[obs.trace.scheduler-id-in-metadata]` → `` `obs.trace.scheduler-id-in-metadata` spec marker `` (backtick-fenced, no leading `r[`). The backtick-code form is NOT parsed as an annotation (tracey looks for `r[` at col-0-of-content, not inside inline-code).

**Check at dispatch:** `nix develop -c tracey query validate` may or may not flag these — if it currently shows 0 errors, tracey's parser tolerates them but they're brittle (a future strictness bump would break). Fix anyway; the rephrase is clearer for human readers. discovered_from=287-review.

### T65 — `docs(tests):` default.nix 0x80 trailer grep — tighten to frame prefix

[`nix/tests/default.nix:485`](../../nix/tests/default.nix) comment mentions "The 0x80 grep proves the grpc_web filter doesn't…" — same fix-class as [P0304](plan-0304-trivial-batch-p0222-harness.md)-T43 (which fixed the P0273 plan doc's `0x80` grep). The `default.nix` grep has the same false-positive risk (any `0x80` byte in the response body matches). Tighten to `80 00 00 00` frame-prefix or `xxd -p | tail -c 50 | grep -q '^80'`.

**Check at dispatch:** the actual grep line may be in the `cli.nix` subtest (P0273-T5 put it there per [`plan-0273:306`](plan-0273-envoy-sidecar-grpc-web.md)). If `default.nix:485` is comment-only prose, fix the comment to describe the tightened grep. If there's executable grep in cli.nix, fix that too (may already be done by P0304-T43 if it covered both sites). discovered_from=273-review.

### T66 — `docs(scheduler):` r[sched.admin.sizeclass-status] — new marker for GetSizeClassStatus RPC

[P0231](plan-0231-get-sizeclass-status-rpc-hub.md) adds `AdminService.GetSizeClassStatus` RPC ([`admin.proto:48`](../../rio-proto/proto/admin.proto)). The other admin RPCs have markers: `r[sched.admin.list-workers]` at [`scheduler.md:127`](../../docs/src/components/scheduler.md), `r[sched.admin.list-builds]` at `:131`, `r[sched.admin.clear-poison]` at `:135`, `r[sched.admin.list-tenants]` at `:139`, `r[sched.admin.create-tenant]` at `:143`. GetSizeClassStatus has no marker — inconsistent with the sibling pattern (the review flagged "spec-marker inconsistent").

Add after `:143` (after `create-tenant`):

```
r[sched.admin.sizeclass-status]
`AdminService.GetSizeClassStatus` returns per-class status: configured vs effective cutoffs (the rebalancer may have recomputed), queued/running counts, sample counts in the rebalancer's lookback window. HUB for WPS autoscaler, CLI cutoffs table, CLI WPS describe. Empty classes list when size-class routing is disabled.
```

P0231's handler at [`rio-scheduler/src/admin/sizeclass.rs`](../../rio-scheduler/src/admin/sizeclass.rs) (NEW file per P0231 Files fence) then gets `// r[impl sched.admin.sizeclass-status]`. discovered_from=231-review.

### T67 — `docs(tracey):` config.styx — add dashboard Svelte/TS globs for dash.* markers

MODIFY [`.config/tracey/config.styx:38-62`](../../.config/tracey/config.styx). [`Cluster.svelte:2`](../../rio-dashboard/src/pages/Cluster.svelte) has `r[impl dash.journey.build-to-logs]` and [`Cluster.test.ts:1`](../../rio-dashboard/src/pages/__tests__/Cluster.test.ts) has `r[verify dash.journey.build-to-logs]` — both **INVISIBLE** to tracey. The `impls` include block at `:42-50` covers only `rio-*/src/**/*.rs`; the `test_include` at `:56-61` has `.rs` + `default.nix`. No Svelte or TS.

Add to the rust impls block (or a new `dashboard` impls block — tracey may support multiple `impls` sections, check config schema):

```
include (
  ...existing...
  rio-dashboard/src/**/*.svelte
  rio-dashboard/src/**/*.ts
)
test_include (
  ...existing...
  rio-dashboard/src/pages/__tests__/**/*.ts
  rio-dashboard/vitest/**/*.ts
)
```

**Check at dispatch:** tracey's comment-syntax detection for `.svelte` (`<!-- r[impl ...] -->` vs `// r[impl ...]` inside `<script>`) may need a config hint. If tracey can't parse Svelte comments, the marker stays; document in a `.claude/notes/tracey-svelte-limitation.md` instead. discovered_from=277-review.

### T68 — `docs(work):` plan-0277:121 — files-fence note stale vs body

MODIFY [`.claude/work/plan-0277-dashboard-app-shell-clusterstatus.md:121`](../../.claude/work/plan-0277-dashboard-app-shell-clusterstatus.md). The files-fence entry says `"note": "T1: createConnectTransport"` but the body at [`:18`](../../.claude/work/plan-0277-dashboard-app-shell-clusterstatus.md) says `createGrpcWebTransport, NOT createConnectTransport` (Audit B2 #20). The fence lags the body correction. Align to `"T1: createGrpcWebTransport (Audit B2 #20)"`. Plan-doc archaeology — P0277 is DONE so this is for future-grep accuracy. discovered_from=277-review.

### T69 — `docs(infra):` envoy-gateway-system namespace hardcoded — 2 sites, extract to values

[`nix/docker.nix:196`](../../nix/docker.nix) hardcodes `rio-dashboard-envoy.envoy-gateway-system.svc.cluster.local:8080` in nginx upstream config. [`infra/helm/rio-build/templates/networkpolicy.yaml:216`](../../infra/helm/rio-build/templates/networkpolicy.yaml) hardcodes `kubernetes.io/metadata.name: envoy-gateway-system` in namespace selector. Both assume Envoy Gateway runs in the default namespace — but `ControllerNamespaceMode` is configurable.

**Option A (preferred, helm-side):** extract to `values.yaml` under `envoyGateway.namespace: envoy-gateway-system` and template both: the NetworkPolicy at `:216` becomes `{{ .Values.envoyGateway.namespace }}`; the nginx upstream (built at image-bake time, not helm-time) gets an **env var** `RIO_DASHBOARD_UPSTREAM_HOST` that the nginx config reads, defaulting to the envoy-gateway-system hostname.

**Option B (doc-only):** add a comment at both sites pointing at the other ("also hardcoded at networkpolicy.yaml:216 — change together") — minimal fix, captures the coupling. Prefer A if the templating is low-friction; B if nginx config templating is hairy (it's baked into the image, not a runtime ConfigMap). discovered_from=282-review.

### T70 — `docs(work):` plan-0289 table row 8 erratum — lifecycle.nix → scheduling.nix

MODIFY [`.claude/work/plan-0289-port-specd-unlanded-test-trio.md:8`](../../.claude/work/plan-0289-port-specd-unlanded-test-trio.md). Row 8 of the spec-trio table says `worker.shutdown.sigint` Target = `nix/tests/scenarios/lifecycle.nix`. The erratum blockquote at [`:17-23`](../../.claude/work/plan-0289-port-specd-unlanded-test-trio.md) CORRECTS this to scheduling.nix ("the 15-doc's rationale was correct and load-bearing — lifecycle.nix's k3s fixture has no systemctl") but the table row itself was never updated. Table reader not reading to `:17` gets the wrong target. Fix the table cell to `nix/tests/scenarios/scheduling.nix` and keep the erratum blockquote as provenance. discovered_from=361-review.

### T71 — `docs(work):` P0250/P0245 DerivationInfo → DerivationNode

[`plan-0250-ca-detect-plumb-is-ca.md:11,19,41`](../../.claude/work/plan-0250-ca-detect-plumb-is-ca.md) and [`plan-0245-prologue-phase5-markers-gt-verify.md:35`](../../.claude/work/plan-0245-prologue-phase5-markers-gt-verify.md) say `DerivationInfo` — the proto message at [`types.proto:12`](../../rio-proto/proto/types.proto) is `DerivationNode` (no `DerivationInfo` exists; `grep '^message DerivationInfo' rio-proto/proto/*.proto` → 0 hits). Plan-doc archaeology: both plans may be DONE/dispatched, so this is grep-accuracy. Check at dispatch — if there's a DIFFERENT message (maybe `Derivation` in another file), use that name. discovered_from=248-review.

### T72 — `docs(obs):` observability.md Alerting section — add bloom_fill_ratio entry

MODIFY [`docs/src/observability.md:303+`](../../docs/src/observability.md) (`## Alerting` section). [P0288](plan-0288-bloom-fill-ratio-gauge.md) added `rio_worker_bloom_fill_ratio` gauge + noted "Alert ≥ 0.5" in the metric table at `:174` — but the dedicated Alerting section at `:303+` doesn't list it. Add an entry:

```markdown
| `rio_worker_bloom_fill_ratio >= 0.5` | Worker FUSE cache bloom filter saturated — FPR climbing past 1%. Bump `spec.bloomExpectedItems` on the WorkerPool and restart. | [P0375](../.claude/work/plan-0375-bloom-expected-items-crd-knob.md) |
```

Remediation link points at the CRD-knob plan. discovered_from=288-review.

### T73 — `docs(work):` plan-0291 — P0276 reserved-field clarity note

MODIFY [`.claude/work/plan-0291-is-cutoff-migration-drop.md`](../../.claude/work/plan-0291-is-cutoff-migration-drop.md). rev-p291 trivial: the plan references P0276's "field 3 RESERVED" at [`:5`](../../.claude/work/plan-0291-is-cutoff-migration-drop.md). Check if the `reserved` keyword in `types.proto` needs a clarifying comment (proto3 `reserved 3;` — future field-number allocation won't collide) OR if the plan doc needs a one-line note explaining why RESERVED (not just removed). discovered_from=291-review.

### T74 — `docs(dashboard):` GC.svelte ratio-comment misleading — collected/scanned is garbage-rate not progress

rev-p281 doc-bug. [`GC.svelte:75-84`](../../rio-dashboard/src/pages/GC.svelte) (p281 worktree) computes `ratio = collected / scanned` and the comment at `:75-77` says "progress bar ratio: collected/scanned when scanned>0 ... mark sweep may collect fewer than it scanned (most paths are live)". This is semantically misleading: `collected/scanned` is the **garbage rate** (fraction of scanned paths that were dead), not sweep **progress** (fraction of total paths scanned so far). A 10%-garbage store shows a bar stuck at 10% throughout the sweep, not advancing toward 100% — operators will think it's hung. The `isComplete → 1` clamp at `:80` papers over the terminal state but the mid-sweep bar is useless as progress. **Two fixes:** (a) if `GCProgress` has a `totalPaths` or `estimatedTotal` field, use `scanned / total` as the actual progress ratio; (b) if not, relabel the bar — change the comment + UI copy to "garbage rate" not "progress" and show `scanned` as a plain counter. Check `types.proto` `GCProgress` message at dispatch. **This is P0281 code.** discovered_from=281-review.

### T75 — `docs(code):` TODO(P0311) orphan-masquerading — gateway handler/mod.rs:110,500 + translate.rs:547 + config.rs:122 point at wrong plan

rev-p310 IMPORTANT doc-bug. Four `TODO(P0311)` tags at [`handler/mod.rs:110`](../../rio-gateway/src/handler/mod.rs), [`handler/mod.rs:500`](../../rio-gateway/src/handler/mod.rs), [`translate.rs:547`](../../rio-gateway/src/translate.rs), [`config.rs:122`](../../rio-worker/src/config.rs). [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) is the **test-gap batch** — it doesn't own "bump past `32827b9fb` AND advertise `set-options-map-only`". The real owner of that work is none — P0310's T0-OUTCOME at [`:42`](../../.claude/work/plan-0310-gateway-client-option-propagation.md) said "keep for future-proofing per TODO(P0311) — if rio-gateway ever advertises `set-options-map-only` the path goes live" but P0311 doesn't have a task for that. P0311-T11 is about per-build-timeout via gRPC (`SubmitBuildRequest.build_timeout`), NOT about the ssh-ng `set-options-map-only` feature. These TODOs are orphan-masquerading as tagged. **Fix:** either (a) retag to `TODO(P0XXX)` with a NEW plan that owns "advertise `set-options-map-only` + make ClientOptions accessors live" OR (b) retag to `WONTFIX(P0310)` since P0310's T0-OUTCOME explicitly said the path is dead until rio advertises the feature, which is out-of-scope. Prefer (b) — `config.rs:111` already has `WONTFIX(P0310)` for the same finding; align all four sites. discovered_from=310-review.

### T76 — `docs:` CLAUDE.md — document WONTFIX(P0NNN) convention alongside TODO(P0NNN)

rev-p310 + consol-mc160 (two cadence agents flagged the same gap). [`config.rs:111`](../../rio-worker/src/config.rs) uses `WONTFIX(P0310):` — a code-comment convention not documented in CLAUDE.md. The Deferred-work-and-TODOs section at CLAUDE.md `:206-239` covers `TODO(P0NNN)` format but not WONTFIX. WONTFIX is distinct: "we investigated, this won't be fixed, P0NNN documents WHY". Add to CLAUDE.md after the TODO format block (~`:211`):

```markdown
Format for explicitly-not-doing: `WONTFIX(P0NNN): <why>` where `P0NNN` is the plan whose investigation concluded this won't be fixed. Unlike TODO, WONTFIX does NOT schedule future work — it's a grep-able marker that someone looked at this and decided against it. The plan doc contains the rationale.

```rust
// WONTFIX(P0310): ssh-ng client options are dropped client-side — Nix
// overrides setOptions() with an empty body (088ef8175, intentional).
// The accessor stays for future-proofing if rio ever advertises
// `set-options-map-only`; until then this path is unreachable.
```

Audit: `grep -rn 'WONTFIX[^(]' rio-*/src/` finds untagged WONTFIX (should be zero).
```

discovered_from=310-review+consol-mc160.

### T77 — `docs(scheduler):` r[impl proto.stream.bidi] stranded in mod.rs:12 post-P0356 split — move to worker_service.rs

rev-p356 doc-bug. [`grpc/mod.rs:12`](../../rio-scheduler/src/grpc/mod.rs) has `// r[impl proto.stream.bidi]` annotating the module docstring. After [P0356](plan-0356-split-scheduler-grpc-service-impls.md)'s split, the actual bidirectional-stream impl (`build_execution` RPC with `spawn_monitored("build-exec-bridge")` at [`worker_service.rs:75`](../../rio-scheduler/src/grpc/worker_service.rs) and `spawn_monitored("worker-stream-reader")` at [`worker_service.rs:92`](../../rio-scheduler/src/grpc/worker_service.rs)) lives in `worker_service.rs`, not `mod.rs`. The marker at `mod.rs:12` now annotates only the **module declaration** + shared helpers — misleading. Move `// r[impl proto.stream.bidi]` to [`worker_service.rs`](../../rio-scheduler/src/grpc/worker_service.rs) near the `build_execution` impl (above the `async fn build_execution` signature or above the `:75` spawn_monitored call). `tracey query rule proto.stream.bidi` will then show the actual implementing file. discovered_from=356-review.

### T78 — `docs(dashboard):` Builds.svelte:36 — "until P0271 adds a GetBuild RPC" WRONG cross-reference

rev-p278 doc-bug. [`Builds.svelte:36`](../../rio-dashboard/src/pages/Builds.svelte) (p278 worktree) says "acceptable until P0271 adds a GetBuild RPC, at which point this whole fallback collapses to a single id lookup". But [P0271](plan-0271-cursor-pagination-admin-builds.md) is **cursor pagination** (`ListBuilds` keyset cursor), NOT a `GetBuild(id)` lookup RPC. No plan owns a single-build lookup RPC. This is an ORPHANED forward-reference. Either: (a) file a new plan for `AdminService.GetBuild(build_id)` and retag the comment, OR (b) reword to "acceptable until a `GetBuild(id)` RPC lands (no owner plan — current workaround: broad listBuilds+find)". Prefer (b) — honest about the gap without committing to a plan that doesn't exist. If a `GetBuild` RPC becomes worth its own plan (dashboard deep-link UX pain is the driver), file it separately. **This is P0278 code.** discovered_from=278-review.

### T79 — `docs:` decisions.md — add ADR-018 row (018-ca-resolution.md exists, table lags)

consol-mc160 doc-bug. [`docs/src/decisions/018-ca-resolution.md`](../../docs/src/decisions/018-ca-resolution.md) exists but the index table at [`docs/src/decisions.md:6-21`](../../docs/src/decisions.md) stops at ADR-015. ADR-016 and ADR-017 are also missing from the table (gap between 015 and 018 — either they were never written OR the table lagged). Add the missing row(s) at dispatch — check `ls docs/src/decisions/` for 016/017 presence; if only 018 exists, add just that row:

```markdown
| [ADR-018](./decisions/018-ca-resolution.md) | CA derivation resolution (<one-line summary from the ADR>) | Accepted |
```

Read `018-ca-resolution.md` for the one-line summary (likely about Realisation schema v2 or the scheduler-resolve flow). discovered_from=consol-mc160.

**NOTE on T67 re-flag (rev-p278):** rev-p278 independently flagged the tracey-dashboard-glob gap that T67 already covers — P0278's BuildDrawer.svelte and Builds.svelte add MORE `r[impl dash.*]` prose references (at `:102,:106` — `r[dash.stream.log-tail]` and `r[dash.graph.degrade-threshold]` in TODO-comments). T67's scope (add `rio-dashboard/src/**/*.svelte` + `rio-dashboard/src/**/*.ts` to config.styx `include`) already covers these. The rev-p278 finding confirms T67 is still needed AND adds scope detail: the TODO-comment markers in BuildDrawer.svelte are INSIDE `<!-- -->` HTML comments, not `<script>` `//` comments — T67's dispatch-time check for tracey's Svelte comment-syntax parsing is doubly relevant.

**NOTE on T31 re-flag (rev-p283):** rev-p283 independently flagged the dashboard.md blank-line-after-marker gap that T31 already covers. The rev-p283 finding references line numbers `:32-42` (shifted from T31's `:29-31/:33-35/:37-39` — controller.md grew between writes). Confirm at dispatch via `grep -n 'r\[dash\.' docs/src/components/dashboard.md` — the three affected markers are `dash.journey.build-to-logs`, `dash.graph.degrade-threshold`, `dash.stream.log-tail`. rev-p283 noted P0283 adds the FIRST `r[verify]` for `dash.journey.build-to-logs` — without T31's blank-line fix, `tracey bump` silently no-ops when the spec text changes. T31 scope unchanged; discovered_from secondary source = 283-review.

**NOTE on T67 re-flag-2 (rev-p283):** rev-p283 also independently flagged the config.styx dashboard-glob gap (`rio-dashboard/**` not in `impl/test_include`). Same gap T67 covers for Svelte/TS. rev-p283 added specificity: `tracey query uncovered` lists 4/5 dash.* rules as uncovered despite `.svelte`/`.ts` annotations existing at `Cluster.svelte:2` + `Cluster.test.ts:1`. THIRD re-flag of the same structural gap (rev-p277 original → rev-p278 → rev-p283). T67 scope unchanged; tracey Svelte/TS parser-support question is the blocking unknown.

### T80 — `docs(controller):` DUPLICATE r[ctrl.pool.bloom-knob] — delete controller.md:142-143

rev-p375 + rev-p374 + rev-p376 doc-bug (THREE agents independently found same). [`docs/src/components/controller.md:119`](../../docs/src/components/controller.md) AND `:142` both have `r[ctrl.pool.bloom-knob]` with IDENTICAL spec text. [P0375](plan-0375-bloom-expected-items-crd-knob.md) added the `:119` copy (correctly placed in WorkerPool section, before the `### WorkerPoolSet` header at `:126`); the `:142` copy is PRE-existing (added by an earlier batch-append from [`25499380`](https://github.com/search?q=25499380&type=commits) into the WorkerPoolSet section — wrong section for a WorkerPool-spec field). Tracey resolves first-occurrence so `:119` wins and `:142-143` is dead/shadowed.

Risk: if `:119` is ever removed, `:142` silently becomes active with possibly-stale text. Readers see duplicate paragraphs.

Fix: delete `controller.md:142-143` (the marker line + its 2-line paragraph). Verify via `grep -c 'r\[ctrl\.pool\.bloom-knob\]' docs/src/components/controller.md` → 1. **Three-agent consensus** = high-confidence dedup, no discretion needed. discovered_from=375-review (primary) + 374-review (noted sprint-1 had the dup) + 376-review (surfaced during phantom-behind analysis).

### T81 — `docs(controller):` lib.rs describe_counter + error.rs — add |conflict to error_kind enumeration

rev-p372 doc-bug. [P0372](plan-0372-migrate-finalizer-resourceversion-lock.md) added `Error::Conflict` variant at [`error.rs:44`](../../rio-controller/src/error.rs) and wired it into `error_kind()` at `:64` (`Error::Conflict(_) => "conflict"`). But TWO doc-strings lag:

1. [`lib.rs:78`](../../rio-controller/src/lib.rs) `describe_counter!("rio_controller_reconcile_errors_total", "...error_kind=kube|finalizer|invalid_spec|scheduler_unavailable...")` — operator-visible Prometheus `# HELP`. Add `|conflict`.
2. [`error.rs:52`](../../rio-controller/src/error.rs) `/// Discriminator string for metric labels. Stable across error-message changes; low cardinality (4 values).` — says "4 values", now 5.

Both are stale doc-comments; the first is operator-visible. discovered_from=372-review.

### T82 — `docs(proto):` lib.rs:86 dag-or-dag typo — types::DerivationNode vs dag::DerivationNode

rev-p376 doc-bug. [`rio-proto/src/lib.rs:86`](../../rio-proto/src/lib.rs) (p376 worktree ref — P0376 UNIMPL) doc-comment says `` `rio_proto::dag::DerivationNode` or `rio_proto::dag::DerivationNode` `` — same path twice. Intended to illustrate the dual-path re-export: `types::DerivationNode` or `dag::DerivationNode`. Copy-paste typo. One-word fix: `s/dag::DerivationNode/types::DerivationNode/` on the FIRST occurrence in the line (or second — as long as both paths appear once). discovered_from=376-review.

### T83 — `docs(infra):` rbac.yaml:102-106 — /finalizers subresource comment misexplains purpose

rev-p235 doc-bug. [`rbac.yaml:102-106`](../../infra/helm/rio-build/templates/rbac.yaml) (p235 worktree ref — P0235 UNIMPL) comment says the `/finalizers` subresource permission is needed because "finalizer() patches metadata.finalizers via the /finalizers subresource" — WRONG. `kube-rs`'s `finalizer()` uses JSON-patch on the MAIN resource (not the `/finalizers` subresource). The ACTUAL purpose: the `OwnerReferencesPermissionEnforcement` admission plugin checks `update` on `<owner>/finalizers` when creating a child WP with `blockOwnerDeletion:true` (which `controller_owner_ref` sets per [`workerpool/mod.rs:282`](../../rio-controller/src/reconcilers/workerpool/mod.rs)).

Rule is CORRECT; comment is misleading. Rewrite to:

```yaml
# /finalizers subresource: OwnerReferencesPermissionEnforcement
# admission plugin checks `update` on `<owner>/finalizers` when
# creating a child with `blockOwnerDeletion: true` (which
# controller_owner_ref sets). Without this verb, child-WP create
# fails with "cannot set blockOwnerDeletion ... not permitted to
# update the owner resource" — NOT a finalizer() helper concern
# (that uses JSON-patch on the main resource).
```

discovered_from=235-review.

### T84 — `docs(tracey):` config.styx infra/helm YAML include — structural gap for Gateway-API impl markers

rev-p371 doc-bug (structural). [`dashboard-gateway.yaml:3-4`](../../infra/helm/rio-build/templates/dashboard-gateway.yaml) has `r[impl dash.auth.method-gate]` + `r[impl dash.envoy.grpc-web-translate]` as YAML `#` comments. [`config.styx:42-50`](../../.config/tracey/config.styx) `impls.include` only covers `rio-*/src/**/*.rs` — YAML not scanned. `tracey query rule dash.auth.method-gate` confirms: spec+verify present, NO impl site. `tracey query uncovered` lists both as unimplemented despite passing VM tests. The YAML comment at `:4` admits "markers are documentary."

Options (followup listed three):
- **(a)** add `infra/helm/**/*.yaml` to config.styx `impls.include` — check at dispatch whether tracey parses `#`-comment YAML annotations (same check-at-dispatch as T67 for Svelte)
- **(b)** convention: infra-only rules are verify-only; suppress from uncovered query via a `#[skip-uncovered]` pragma or sentinel (tracey may not support this — check `tracey --help`)
- **(c)** create a Rust-side anchor file — e.g., `rio-test-support/src/dashboard_gateway_anchor.rs` with `// r[impl dash.auth.method-gate]` comment-only placeholder. Hacky but works today.

**Prefer (a)** — same shape as T67's Svelte/TS include, extends `include` list. If tracey doesn't parse YAML `#`-comments, fall back to **(c)** (hacky but deterministic — a `const _: () = ();` with the annotation above it). **(b)** changes tracey query semantics for all infra rules, which is a bigger decision.

Same structural category as [P0341](plan-0341-tracey-nix-subtests-marker-convention.md)'s convention-enforced-by-tooling work (nix subtests markers). discovered_from=371-review.

### T85 — `docs(infra):` values.yaml envoyImage — forward cross-ref k3s-full.nix airgap set

rev-p379 doc-rot. [P0379-T1](plan-0379-envoy-image-digest-pin.md) digest-pins `envoyImage` in [`values.yaml`](../../infra/helm/rio-build/values.yaml) at `:404-407`. The airgap preload set at [`k3s-full.nix:40,66-74`](../../nix/tests/fixtures/k3s-full.nix) (`envoyGatewayRender` import + `envoyproxy/envoy:distroless` preload) hardcodes the same image via [`docker-pulled.nix:94`](../../nix/docker-pulled.nix). Neither site cross-references the other. A values.yaml bump without a matching docker-pulled.nix bump silently splits production image from VM-test image.

Add a forward-cross-ref comment at `values.yaml` adjacent to the `envoyImage` key:

```yaml
# envoyImage: digest-pinned by P0379. LOCKSTEP with docker-pulled.nix:94
# envoy-distroless entry (k3s-full.nix airgap preload sources it).
# Bump BOTH or P0304-T136 helm-lint assert fails.
envoyImage:
  repository: docker.io/envoyproxy/envoy
  tag: ...
```

And reciprocally at `docker-pulled.nix:94`:

```nix
# envoy-distroless: LOCKSTEP with infra/helm/rio-build/values.yaml envoyImage.
# Bump BOTH — helm-lint assert at flake.nix checks tag match (P0304-T136).
```

Partner to T69 (envoyGateway.namespace extraction) — same values.yaml region, non-overlapping keys. discovered_from=379-review.

### T86 — `docs(controller):` main.rs:415-421 tokio::join! — clarify panic-vs-Ok-exit semantics

rev-p235 doc-bug. [`main.rs:415-421`](../../rio-controller/src/main.rs) comment says "`tokio::join!` polls both concurrently on THIS task — no separate spawn, so a panic in either reconciler propagates here (and main() exits) rather than being swallowed by a JoinHandle." Accurate but incomplete — the review flagged ambiguity on the `Ok(())`-early-exit case: if `wp_controller` returns `Ok(())` (graceful shutdown token fired) but `wps_controller` is still draining, `join!` continues polling `wps_controller`. If instead `wp_controller` PANICS (reconciler bug), the panic unwinds through `join!` immediately — `wps_controller` is NOT awaited-to-completion.

Extend the comment to cover both paths:

```rust
// tokio::join! polls both concurrently on THIS task — no separate
// spawn. Semantics:
//   - Ok(()) from ONE: join! continues polling the OTHER until it
//     also completes (graceful-shutdown waits for both drains).
//   - Panic in ONE: unwinds through join! immediately — the OTHER
//     is NOT polled to completion (process exits via unwind).
// This is the intended behavior: panics propagate (no JoinHandle
// silent-swallow), Ok-exits wait for sibling (no half-drained
// state on shutdown).
tokio::join!(wp_controller, wps_controller);
```

No behavior change — comment extension only. If the panic-immediate-unwind semantics are NOT intended (i.e., we want `wps_controller` to finish draining even if `wp_controller` panics), that would require `catch_unwind` + `join!` — out of scope for a doc-rot batch; file a separate correctness followup if rev-p235 escalates. discovered_from=235-review.

### T87 — `docs(remediations):` rem-11 admin/mod.rs:562-587 line-refs stale post-P0383 split

rev-p383 doc-bug. [`docs/src/remediations/phase4a/11-tonic-health-drain.md`](../../docs/src/remediations/phase4a/11-tonic-health-drain.md) references `rio-scheduler/src/admin/mod.rs:562-587` at three sites (`:55`, `:389`, `:612`) for the TriggerGC forward-task shutdown fix. [P0383](plan-0383-admin-mod-split-plus-actor-alive-dedup.md) extracted TriggerGC + `spawn_store_size_refresh` to [`admin/gc.rs`](../../rio-scheduler/src/admin/gc.rs) — the `:562-587` range no longer exists in `mod.rs` (now 397L). The `:690` `[ ] Unit: trigger_gc_forward_exits_on_shutdown` reference to `scheduler/admin/mod.rs tests` is also pre-split — test is at [`admin/tests.rs:1349`](../../rio-scheduler/src/admin/tests.rs) (or `admin/tests/gc_tests.rs` post-[P0386](plan-0386-admin-tests-rs-split-submodule-seams.md)).

Update the three `admin/mod.rs:562-587` refs → `admin/gc.rs` + re-grep the forward-task range (`trigger_gc` is at [`gc.rs:~44-100`](../../rio-scheduler/src/admin/gc.rs) post-split — check exact lines at dispatch). Update the `:690` test reference. Also:

- `:686` `[ ] rio-scheduler/src/admin/mod.rs: thread shutdown: Token into AdminServiceImpl` — STILL correct (`AdminServiceImpl` struct stays in `mod.rs` per P0383-T5); the `select! in TriggerGC forward task` clause is the part that moved to `gc.rs`
- `:394-395` diff header `--- a/rio-scheduler/src/admin/mod.rs` → `--- a/rio-scheduler/src/admin/gc.rs`

The followup cited stale refs at `:38/:55/:41/:367/:373` — `:55` is confirmed (the main `:562-587` ref). The other line numbers may refer to different file refs in rem-11 OR cross-refs in OTHER remediation docs that cite admin/mod.rs. At dispatch: `grep -n 'admin/mod.rs' docs/src/remediations/phase4a/*.md` and re-verify each ref against the post-P0383 397L `mod.rs`. discovered_from=383-review.

### T88 — `docs(crate-structure):` crate-structure.md — 9-crate drift sweep (28 refactor-commits, zero doc updates)

consol-mc190 doc-bug. [`docs/src/crate-structure.md`](../../docs/src/crate-structure.md) has drifted from the workspace since [`9c14a5f7`](https://github.com/search?q=9c14a5f7&type=commits) — 28 `refactor(...)` commits and zero corresponding doc updates. [P0381](plan-0381-scaling-rs-fault-line-split.md) widened the gap further (its own module move is not reflected).

**Drift inventory** (line refs to crate-structure.md @ sprint-1):

| Doc claim | Reality | Line |
|---|---|---|
| "Workspace Layout (9 crates)" | 12 Rust workspace members: `rio-common`, `rio-nix`, `rio-proto`, `rio-test-support`, `rio-gateway`, `rio-scheduler`, `rio-store`, `rio-worker`, `rio-controller`, `rio-cli`, `rio-crds`, `rio-bench` | `:3` |
| `rio-cli` under "Not yet built" | BUILT — in Cargo.toml workspace members | `:21` |
| `rio-common` 8 modules | 15 modules: bloom, config, grpc, hmac, jwt, jwt_interceptor, lib, limits, newtype, observability, server, signal, task, tenant, tls | `:84-94` |
| `rio-proto` 5 `.proto` files | 8: admin, admin_types, build_types, dag, scheduler, store, types, worker (post-[P0376](plan-0376-types-proto-domain-split.md) split) | `:126-132` |
| `rio-proto/src/client.rs` single file | `client/` directory with `mod.rs` + `balance.rs` | `:134-136` |
| `rio-crds` not mentioned | Separate workspace member since CRD extraction — carries `WorkerPool`/`WorkerPoolSet` CRDs | — |
| `rio-bench` not mentioned | Criterion benches workspace member | — |

**Sweep recipe** (re-run at dispatch — ground truth from Cargo.toml + filesystem):

```bash
# Workspace members (doc :3 count + :6-17 tree)
grep -A20 '^\[workspace\]' Cargo.toml | grep -E '^\s+"rio-'

# Per-crate module listing (doc :84+ module blocks)
for c in rio-common rio-nix rio-proto; do
  echo "--- $c/src/ ---"
  find $c/src/ -name '*.rs' -not -path '*/tests/*' | sed 's|.*/src/||' | sort
done

# Proto files (doc :126-132)
ls rio-proto/proto/*.proto
```

MODIFY [`crate-structure.md`](../../docs/src/crate-structure.md):

1. `:3` "9 crates" → actual count from Cargo.toml members
2. `:6-17` workspace-layout tree → add `rio-cli`, `rio-crds`, `rio-bench`; remove from "Not yet built" at `:19-22`
3. `:26-68` dependency-graph mermaid → add `rio-crds` edges (`rio-controller → rio-crds`, `rio-cli → rio-crds`), `rio-cli → rio-common+rio-proto`
4. `:84-94` `rio-common` module list → 15-module reality (add hmac/jwt/jwt_interceptor/server/signal/tenant/tls)
5. `:126-139` `rio-proto` proto/ + src/ → 8-proto + client/-dir reality (post-P0376)
6. Add `### rio-crds` section after `rio-controller` (WorkerPool/WorkerPoolSet CRD structs + derive macros)
7. Add `### rio-cli` section (subcommands, MockAdmin tests reference)

**OPTIONAL (EC-template fix)**: consol-mc190 noted "doc-sync not in split-plan EC". Add a forward-pointer to [`plan-doc-skeleton.md`](../lib/plan-doc-skeleton.md) or the split-plan template: "when adding/removing a workspace member OR splitting a module-directory, update crate-structure.md in the same commit." This is a ONE-LINE prose add to the skeleton, not a new T-task. Consider folding into [P0304](plan-0304-trivial-batch-p0222-harness.md)-T1's skeleton sync if that hasn't dispatched. discovered_from=consol-mc190.

### T89 — `docs(tracey):` config.styx — dashboard pages/*.svelte include (EXTENDS T67)

rev-p279 correctness + coord filing (DUPLICATE rows 8+15+16 consolidated). [P0279](plan-0279-dashboard-streaming-log-viewer.md) added only `rio-dashboard/src/lib/logStream.svelte.ts` to [`config.styx:58`](../../.config/tracey/config.styx) — the `r[impl dash.journey.build-to-logs]` markers at [`Builds.svelte:2`](../../rio-dashboard/src/pages/Builds.svelte) and [`Cluster.svelte:2`](../../rio-dashboard/src/pages/Cluster.svelte) are **invisible to tracey**. `tracey query uncovered` still lists `dash.journey.build-to-logs` despite P0279 `:113` claiming to close it.

**T67 already proposes** "`+rio-dashboard Svelte/TS globs to include + test_include`" but with a "check at dispatch: tracey may not parse Svelte `<!-- -->` comments" caveat. The P0279 markers are inside `<script lang="ts"> // r[impl ...]` blocks — that's `//` TS comment syntax inside a `<script>` tag, NOT `<!-- -->`. Whether tracey's Svelte parser handles this depends on the tree-sitter grammar (arborium-nix handles `.nix`; Svelte would need [`tree-sitter-svelte`](https://github.com/Himujjal/tree-sitter-svelte) or plain-TS parsing of the script block).

**T89 extends T67 with specifics:**

1. Add explicit filenames `rio-dashboard/src/pages/Builds.svelte` + `rio-dashboard/src/pages/Cluster.svelte` to `include` at `:58` (alongside P0279's `logStream.svelte.ts` line). Matches the `:23-28` comment's "explicit filename" approach.
2. OR (preferred if tracey parses Svelte): replace explicit filenames with `rio-dashboard/src/**/*.svelte` + `rio-dashboard/src/**/*.ts` in `include`, plus `**/__tests__/**` in `exclude`. The `:23-28` comment says tracey's `*` crosses `/` so single-glob would descend into `__tests__` — an explicit exclude fixes that.

**Fragility of option (1)** (row 16 finding): rename/move silently loses coverage with NO `tracey-validate` error (marker becomes invisible, not broken). Option (2) scales and doesn't regress on file moves.

At dispatch: `nix develop -c tracey query rule dash.journey.build-to-logs` before/after — if option (2) shows 2 impl sites post-fix, take it; else fall back to (1). Validator PARTIAL (journey-marker-invisible) flag IS this. discovered_from=279 (reviewer) + coord. **Post-P0279-merge** (config.styx `:58` line arrives with it).

### T90 — `docs(plan):` plan-0299 EC :161 not-met annotation

rev-p299 doc-bug. MODIFY [`.claude/work/plan-0299-staggered-scheduling-cold-start.md`](plan-0299-staggered-scheduling-cold-start.md) at `:161`. EC says "VM test asserts ordering on the stream (Assignment arrives AFTER PrefetchComplete)". Implemented VM scenario at [`scheduling.nix:1234-1288`](../../nix/tests/scenarios/scheduling.nix) is **passive observability check only** — asserts `fallback_total==0` and `warm_prefetch_paths_count>=1`. Weaker proof: proves messages-arrived, not ordering. The unit-test-level ordering proof also doesn't exist (tracked at [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T58).

Add `> **Not met as written:** implemented scenario is passive metrics-check only; ordering-proof tracked at [P0311-T58](plan-0311-test-gap-batch-cli-recovery-dash.md).` blockquote after the EC bullet. Plan-doc archaeology (P0299 DONE); the blockquote prevents later readers from assuming the ordering was proven. discovered_from=299.

### T91 — `docs(obs):` observability.md — two counter rows missing

rev-p251 + rev-p367 doc-bug (coord-flagged). MODIFY [`docs/src/observability.md`](../../docs/src/observability.md):

1. **Scheduler Metrics table (`:81-123`):** add `rio_scheduler_ca_hash_compares_total` row. Emitted at [`completion.rs:328-332`](../../rio-scheduler/src/actor/completion.rs), described (check `rio-scheduler/src/lib.rs` describe_metrics at dispatch). Labels: `outcome=match|miss` today; `+error` via [P0304](plan-0304-trivial-batch-p0222-harness.md)-T148; `+malformed` via P0304-T157; `+skipped_after_miss` via [P0393](plan-0393-ca-contentlookup-serial-timeout.md). discovered_from=251.

2. **Gateway Metrics table (`:63-75`):** add `rio_gateway_jwt_mint_degraded_total` row. Described at [`lib.rs:66`](../../rio-gateway/src/lib.rs) but NEVER in the obs.md table. rev-p367 found it alongside the GATEWAY_METRICS const omissions (P0304-T149 stopgap). This is the doc-side fix. discovered_from=367.

Both additive row-inserts. T91 soft-coordinates with [P0394](plan-0394-spec-metrics-derive-from-obs-md.md) — once T91 lands the rows, P0394's derived SPEC_METRICS automatically includes them (that's the point of derivation).

### T92 — `docs(plan):` RE-FLAG T90/T91 — implementer missed 3 mechanical additions

coord doc-bug (rev-p295 post-merge check). P0295's prior implementer MISSED T90+T91a+T91b — 3 mechanical docs additions not landed: the [`plan-0299:161`](plan-0299-staggered-scheduling-cold-start.md) not-met-as-written blockquote (T90) + the two `observability.md` metric table rows (T91a `rio_scheduler_ca_hash_compares_total`, T91b `rio_gateway_jwt_mint_degraded_total`). All three are described above at T90/T91; this T-item is a dispatch-time re-flag so the NEXT P0295 batch-dispatch doesn't skip them again.

Verification that T90/T91 are unlanded at dispatch:
```bash
grep 'Not met as written' .claude/work/plan-0299-*.md  # → 0 = T90 missed
grep 'rio_scheduler_ca_hash_compares_total' docs/src/observability.md  # → 0 = T91a missed
grep 'rio_gateway_jwt_mint_degraded_total' docs/src/observability.md  # → 0 = T91b missed
```

If any return ≥1 hit, that sub-item landed in a different batch pass; skip it. discovered_from=coordinator (rev-p295).

### T93 — `docs(plan):` plan-0271 files-fence — types.proto → admin_types.proto

rev-p271 doc-bug (class-(E) target-moved-by-prior-merge). MODIFY [`.claude/work/plan-0271-cursor-pagination-admin-builds.md`](plan-0271-cursor-pagination-admin-builds.md). T1 at `:16` says "MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) at `ListBuildsRequest` (`:693`)". Implementation touched [`admin_types.proto`](../../rio-proto/proto/admin_types.proto) — [P0376](plan-0376-proto-split-admin-types-out.md) split admin messages out of types.proto. Files-fence `:94` also stale (same `types.proto` reference).

Class-(E) scrutiny applied — plan was CORRECT-AT-WRITE (predates P0376), codebase moved. P0271 is DONE (or merging) so this is plan-doc archaeology only; no action on the implementation. Add an erratum blockquote after `:16`:

> **Erratum (post-P0376):** `ListBuildsRequest` moved to [`admin_types.proto`](../../rio-proto/proto/admin_types.proto) (~`:54`) with the P0376 admin-types split. Implementation correctly targeted the new location.

Update files-fence `:94` path to `rio-proto/proto/admin_types.proto` + note "P0376 split — line-ref stale, see impl commit." discovered_from=271.

### T94 — `docs(notes):` tracey skip-uncovered pragma — upstream FR tracking

rev-p295 feature (routed to batch — upstream tracey FR, not rio-build code). MODIFY [`.claude/notes/tracey-svelte-yaml-limitation.md`](../notes/tracey-svelte-yaml-limitation.md) (arrives with this plan's T67/T84 dispatch — the note documents why option-(a) was rejected). Extend the "Options considered" section with a tracked upstream request for option-(b):

```markdown
**(b) Suppress from uncovered:** tracey lacks a `#[skip-uncovered]` pragma.
  FILED UPSTREAM: tracey issue #<TBD> — requesting either a per-rule
  `skip-uncovered-reason: "impl in unparseable .yaml"` config knob, OR
  basic YAML `#`-comment + Svelte `<!-- -->`-comment parsers. Until
  resolved, the four markers below show in `tracey query uncovered`
  despite being implemented:

  | Rule | Impl location | Verify location |
  |---|---|---|
  | `dash.envoy.grpc-web-translate+2` | `dashboard-gateway.yaml:3` | `default.nix:536` |
  | `dash.auth.method-gate` | `dashboard-gateway.yaml:4` | (none) |
  | `dash.journey.build-to-logs` | `Cluster.svelte:2`, `Builds.svelte:2` | `Cluster.test.ts` |
```

Also update the "Current disposition" section — if option-(c) (Rust anchor file) gets implemented later to silence the `uncovered` noise, document the anchor file location. This is an FR-tracking note, not a code change. discovered_from=295.

### T95 — `docs:` grpc/tests.rs → grpc/tests/{submit,stream,bridge,guards}_tests.rs retarget (3+ stale refs)

rev-p395 doc-bug. [P0395](plan-0395-grpc-tests-rs-split-submodule-seams.md) splits `grpc/tests.rs` (1682L monolith) into `tests/{mod,submit_tests,stream_tests,bridge_tests,guards_tests}.rs`. Three classes of stale reference post-split:

**(a) Plan docs referencing the monolith:** [`plan-0287`](plan-0287-trace-linkage-submitbuild-metadata.md)`:117+:215` (T5 unit test location), **T43 in THIS plan** at `:409-411+:1042+:1198` (`:1582` assert-msg target moves to `submit_tests.rs` — T43 already soft-deps P0356/P0356, extend to soft-dep P0395), and P0311-T22 (`grpc/tests.rs` test-add — retargets to `submit_tests.rs` or `stream_tests.rs` per P0395's soft_deps note `:140`).

**(b) DONE-plan archaeology:** [`plan-00{66,71,73,74,77,78,92,93,94}`](plan-0066-module-splits-remaining.md) + [`plan-0145`](plan-0145-coverage-test-backfill-tracey-verify.md) + [`plan-0189`](plan-0189-rem10-proto-plumbing-dead-wrong.md) + [`plan-0190`](plan-0190-rem11-tonic-health-drain.md) all have files-fence entries pointing at the deleted file. Low-value (DONE = archaeology); add erratum blockquotes ONLY if someone greps for them — otherwise leave.

**(c) crate-structure.md:** if it lists `grpc/tests.rs` as a module — check at dispatch; T88's sweep may already cover this.

Prefer (a)-only: re-target T43 (this plan, `:409`), [`plan-0287:117,:215`](plan-0287-trace-linkage-submitbuild-metadata.md), and [`plan-0311`](plan-0311-test-gap-batch-cli-recovery-dash.md) (T22's add-location). Same class-(E) target-moved-by-split as T87 (admin/mod.rs→admin/{gc,tenants,logs}.rs) and T93 (types.proto→admin_types.proto). discovered_from=395. Post-P0395-merge.

### T96 — `docs(plan):` plan-0387 T1 stale reference — k3s-full.nix line-refs drift

rev-p387 doc-bug. MODIFY [`.claude/work/plan-0387-airgap-bare-tag-derive-from-destnametag.md`](plan-0387-airgap-bare-tag-derive-from-destnametag.md) at `:29` (T1 header) or `:7` (table). The reviewer flagged "plan-T1 fictional file reference" — candidates:

- `:7` table `k3s-full.nix:105` + `docker-pulled.nix:96-97` — line refs may have drifted (check `grep -n 'envoy-distroless' nix/docker-pulled.nix` and `grep -n 'dashboard.envoyImage' nix/tests/fixtures/k3s-full.nix` at dispatch)
- `:29` T1 `k3s-full.nix:99-106` — the `extraSet` / `optionalAttrs envoyGatewayEnabled` block may have shifted post-P0379
- `:104` comment "`same pattern as vmtest-full-nonpriv.yaml:33 devicePlugin.image`" at `k3s-full.nix:104` — if P0387-T3 deleted `:33` from the YAML, this cross-ref dangles

Most likely the third: P0387-T3 removes the `:33` image hardcode, so the `:104` comment in `k3s-full.nix` ("same pattern as vmtest-full-nonpriv.yaml:33") points at a deleted line. Plan-doc erratum blockquote + `k3s-full.nix:104` comment update ("same pattern as the `devicePlugin.image` extraValues at default.nix:349"). Class-(E). discovered_from=387. Post-P0387-merge.

### T97 — `docs(dashboard):` LogViewer.test.ts "three tests" comment — stale count after P0392

rev-p392 doc-bug at [`rio-dashboard/src/components/__tests__/LogViewer.test.ts:7`](../../rio-dashboard/src/components/__tests__/LogViewer.test.ts) (p392 ref). The header comment says "These three tests cover the conditional rendering branches at LogViewer.svelte:63-75" — but P0392-T4 adds ≥2 more (windowed-subset, truncated-banner). If [P426](plan-426-logviewer-line-h-cap-spread-limits.md)-T4 lands, that's another. Update to generic "These tests cover…" or cite the block range instead of a count. Also the `:63-75` line-ref drifts once P0392-T2's virtualization inserts before the `{#each}` block. Rephrase as: "cover the conditional rendering branches (`{#if stream.err}`, `{#each}` windowed slice, `{#if !stream.done}` spinner/empty-state) at LogViewer.svelte" — code-shape not count-or-line. discovered_from=392. Post-P0392-merge.

### T98 — `docs(scheduler):` warm_gate_initial_hint test — stale "paths_each=5" comment

rev-p391 doc-bug at [`rio-scheduler/src/actor/tests/worker.rs:1157`](../../rio-scheduler/src/actor/tests/worker.rs) (p391 ref). The test comment says "Use paths_each=5 and also attach per-parent UNIQUE paths below" — then `:1160-1161` says "Actually simpler: … bump paths_each to 70" and `:1169` assigns `let paths_each = 70`. The `:1157` "paths_each=5" is mid-thought leftover prose. Delete `:1157-1158` (the "Use paths_each=5…" sentence and its "and also attach" continuation). The `:1160` "Actually simpler" reads oddly without its antecedent — tighten to "40 parents × 70 children in a sliding window → 109 unique paths (>100 cap)." discovered_from=391. Post-P0391-merge.

### T99 — `docs(gateway):` translate.rs populate_* — restore design-rationale prose

rev-p413 doc-bug at [`rio-gateway/src/translate.rs:196-201`](../../rio-gateway/src/translate.rs). The `populate_*` docstrings lost ~30L design-rationale prose during [P0413](plan-0413-translate-populate-walker-dedup.md)'s walker-dedup refactor. Lost content: "why sequential not concurrent (30ms negligible next to build)", "why NotFound contributing 0 is safe (client-has-not-uploaded-yet; build fails at exec time anyway)", "InputNotFound should not fire — BFS fails hard". The BFS-inconsistency rationale MOVED to `iter_cached_drvs` docstring (good); the rest is gone. Restore to the `populate_input_srcs_sizes` / `populate_ca_modular_hashes` docstrings so future maintainers asking "why is this not parallelized" have the answer inline. Grep pre-P0413 git history for the original text: `git log -p --all -S'sequential not concurrent' -- rio-gateway/src/translate.rs`. discovered_from=413.

### T100 — `docs(plan):` plan-0413 — "third consumer" claim is scheduler-side, not gateway

rev-p413 doc-bug at [`.claude/work/plan-0413-translate-populate-walker-dedup.md:10`](plan-0413-translate-populate-walker-dedup.md). Plan claims [P0408](plan-0408-ca-recovery-resolve-fetch-aterm.md) is the "third consumer" of `iter_cached_drvs` justifying the dedup. P0408 is scheduler-side (`dispatch.rs:maybe_resolve_ca`), not gateway-side `translate.rs` — and `iter_cached_drvs` is crate-private (`fn` not `pub(crate)`), so scheduler CANNOT use it. Actual consumer count is 2 (`populate_input_srcs_sizes` ×2-pass + `populate_ca_modular_hashes`). Dedup still valid on 2; the 3rd-consumer justification is archaeology drift. Add erratum blockquote at `:10`. discovered_from=413.

### T101 — `docs(plan):` plan-0370 — admin/mod.rs+scaling.rs refs stale post-split

rev-p370 doc-bug at [`.claude/work/plan-0370-extract-spawn-periodic-helper.md:12`](plan-0370-extract-spawn-periodic-helper.md). Plan cites `admin/mod.rs` (6×) and `scaling.rs` (4×) — both refactored into subdirs (`admin/gc.rs`, `scaling/standalone.rs`) between plan-authoring and impl. Implementer found correct files; plan doc now has 10 stale path refs. Same class-(E) target-moved-by-split as T87 (P0383 admin split) and T95 (P0395 grpc/tests split). Retarget all 10 refs. Archaeology-tier (P0370 is DONE), but useful for future grep-the-plan-for-the-feature. discovered_from=370.

### T102 — `docs(proto):` ChunkServiceClient re-export — forward-scaffolding or dead code?

sprint-1 cleanup finding at [`rio-proto/src/lib.rs:157`](../../rio-proto/src/lib.rs). `pub use store::chunk_service_client::ChunkServiceClient` has ZERO callers across the workspace. The gRPC service itself (`ChunkService` server-side in `rio-store`) IS live — `PutChunk`/`FindMissingChunks` are used by workers. But the tonic-generated CLIENT stub is re-exported and never imported.

**Decision needed — check at dispatch:**

- **Route-(a) forward-scaffolding:** If there's a scheduled plan where workers will call `ChunkService` directly (bypassing the store-proxy path), add a `// TODO(P0NNN)` tag pointing at that plan and a doc-comment explaining the intended consumer. Grep `.claude/work/plan-*.md` for `ChunkServiceClient` or `chunk.*client` to find the owner.
- **Route-(b) dead export:** If no plan consumes it, delete the `pub use` line. The tonic-generated client stays in the `store` module (crate-private) — if a future consumer needs it, re-add the export then. Dead public API surface invites accidental depends-on.

Prefer route-(b) unless a grep of plan-docs turns up a concrete consumer. The re-export list at `lib.rs:150-160` should be the set of clients something ACTUALLY imports — `StoreServiceClient`, `SchedulerServiceClient`, etc. all have callers. discovered_from=sprint-1-cleanup.


### T103 — `docs(plan):` plan-0316 — add closure header (7bd70aba disabled, 23519879 infra-fixed)

[`.claude/work/plan-0316-qemu-force-accel-kvm.md:1`](../../.claude/work/plan-0316-qemu-force-accel-kvm.md): needs a closure header. The approach was disabled at `7bd70aba` (dual `-machine accel` breaks qemu 10.2.1 multi-VM), root cause fixed at infra level `23519879`. P0295 already depends on P0316 — natural owner. discovered_from=coordinator.

### T104 — `docs(plan):` plan-0306 T6 — correct per-RUN vs per-DOC invariant premise

[`.claude/work/plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md:255`](../../.claude/work/plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md): plan says writer invariant is `T9<runid><NN>` unique PER RUN, but impl discovered it's per-DOC (both docs start at `<runid>01`). The bare-intersection assert from the plan sketch would fire on EVERY multi-batch-doc run as false positive. Impl correctly narrowed to disagreeing-assignments-only ([`merge.py:618`](../../.claude/lib/onibus/merge.py)). Correct the plan premise so future readers understand why impl deviated. discovered_from=306.

### T105 — `docs(gateway):` gateway.md MAX_FRAMED_TOTAL — "MUST equal" → "MUST be ≥"

[`docs/src/components/gateway.md:387`](../../docs/src/components/gateway.md): spec says `MAX_FRAMED_TOTAL` MUST equal `MAX_NAR_SIZE` but the `const_assert` enforces `>=` (the functionally correct invariant per rationale). Relax spec to "MUST be ≥" — `>=` is what prevents the mid-stream-error bug; strict equality is over-specified. discovered_from=440.

### T106 — `docs(plan):` plan-0434 — update soft-dep note (P0430 route-a refutes hypothesis)

[`.claude/work/plan-0434-manifest-mode-upload-bandwidth-opt.md:58`](../../.claude/work/plan-0434-manifest-mode-upload-bandwidth-opt.md): carries stale soft-dep hypothesis ("manifest-mode may BE the first production caller of ChunkServiceClient"). [P0430](plan-0430-chunkserviceclient-zero-callers-decision.md) ADR explicitly refutes this (route-a removed re-export; P0434 uses `StoreService.PutPathManifest`, never ChunkService). Update soft_deps note to reflect decision. discovered_from=430.

### T107 — `docs(scheduler):` lifecycle_sweep.rs r[verify sched.ca.cutoff-propagate] — marker mismatch

[`rio-scheduler/src/actor/tests/lifecycle_sweep.rs:242,326`](../../rio-scheduler/src/actor/tests/lifecycle_sweep.rs): `r[verify sched.ca.cutoff-propagate]` annotates tests that exercise DependencyFailed cascade-on-poison, NOT CA-cutoff Queued→Skipped propagation. The tracey rule text is about Skipped transitions on hash match. These tests verify union-of-interested on failure cascade — may want its own rule or `sched.build.keep-going`. Retarget the markers or add a new `r[sched.cascade.union-interested]` rule. discovered_from=442.

### T108 — `docs(store):` manifest.rs Manifest::total_size — fix stale doc-comment

[`rio-store/src/manifest.rs:147-152`](../../rio-store/src/manifest.rs): doc-comment claims "Used by GetPath" but GetPath calls `ManifestKind::total_size` ([`metadata/mod.rs:195`](../../rio-store/src/metadata/mod.rs)), not this. Only caller is the unit test at `:352`. Commit `accb6642` dropped `cfg(test)` but `f541abc2` added a separate `ManifestKind::total_size` instead. Either restore `#[cfg(test)]` OR fix the doc-comment to say "test-only; production uses ManifestKind::total_size". discovered_from=429.

### T109 — `docs(builder):` lib.rs:47 "Worker Metrics" → "Builder Metrics" section name

[`rio-builder/src/lib.rs:47`](../../rio-builder/src/lib.rs): doc-comment says "sourced from docs/src/observability.md (the Worker Metrics table)" but [P0455](plan-0455-tracey-markers-docs-rename.md) renamed the section to "Builder Metrics" at [`observability.md:164`](../../docs/src/observability.md). Adjacent `r[impl obs.metric.builder]` marker was renamed; prose was not. One-word fix. discovered_from=455.

### T110 — `docs(adr):` ADR-005 body — 3× stale rio-worker/ crate paths

[`docs/src/decisions/005-builder-store-model.md:10,16`](../../docs/src/decisions/005-builder-store-model.md): file was renamed and heading edited by [P0455](plan-0455-tracey-markers-docs-rename.md) but body paths still reference `rio-worker/` (fuse module, config.rs, overlay.rs). Crate is `rio-builder/` now. Three-site `s/rio-worker/rio-builder/` sweep. discovered_from=455.

### T111 — `docs(adr):` ADR-012 body — rio-worker/src/cgroup.rs → rio-builder/src/cgroup.rs

[`docs/src/decisions/012-privileged-builder-pods.md:42`](../../docs/src/decisions/012-privileged-builder-pods.md): references `rio-worker/src/cgroup.rs`. Same P0455 rename-miss as T110. One-site fix. discovered_from=455.

### T112 — `docs(remediations):` 3× half-renamed [worker.md] link text → [builder.md]

Three remediation docs half-renamed: link href points to `../../components/builder.md` but display text still says `[worker.md]`. [P0455](plan-0455-tracey-markers-docs-rename.md) updated the markers in these lines but not the link text. Sites:
- [`docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md:792`](../../docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md)
- [`docs/src/remediations/phase4a/15-shutdown-signal-cluster.md:527`](../../docs/src/remediations/phase4a/15-shutdown-signal-cluster.md)
- [`docs/src/remediations/phase4a/02-empty-references-nar-scanner.md:1238`](../../docs/src/remediations/phase4a/02-empty-references-nar-scanner.md)

Cosmetic but inconsistent. discovered_from=455.

### T113 — `docs(obs):` observability.md:166 — add fod_queue_depth + fetcher_utilization table rows

[`docs/src/observability.md:166`](../../docs/src/observability.md): blockquote says "rows added when the emitters land" — emitters landed at [`f3d1eba0`](https://github.com/search?q=f3d1eba0&type=commits) ([P0452](plan-0452-scheduler-fod-hard-split-routing.md)) but the table rows for `rio_scheduler_fod_queue_depth` and `rio_scheduler_fetcher_utilization` were never added to the Scheduler Metrics table (`r[obs.metric.scheduler]` at `:82`). Add two rows + rewrite the blockquote at `:166` to past tense. Per [`scheduler.md:460`](../../docs/src/components/scheduler.md), fod_queue_depth tracks queued FODs; fetcher_utilization is busy/total with a `total==0 → 0.0` guard. discovered_from=452.

### T114 — `docs(obs):` observability.md Store Metrics — add substitute metrics rows

[`docs/src/observability.md:137`](../../docs/src/observability.md): `rio_store_substitute_total`, `rio_store_substitute_bytes_total`, `rio_store_substitute_duration_seconds` are not documented in the Store Metrics table. Emitters landed with [P0462](plan-0462-upstream-substitution-core.md). Add three rows under `r[obs.metric.store]`:

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rio_store_substitute_total` | counter | `upstream_host`, `outcome` | Substitution attempts (hit/miss/error) |
| `rio_store_substitute_bytes_total` | counter | `upstream_host` | Bytes fetched from upstream |
| `rio_store_substitute_duration_seconds` | histogram | `upstream_host` | Time to fetch+verify from upstream |

Label `upstream_host` (not full URL) per [P0304](plan-0304-trivial-batch-p0222-harness.md) T257's cardinality fix. discovered_from=462.

### T115 — `docs(helm):` values.yaml Fastly CIDR example — show multi-CIDR + blast-radius note

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml) at `:206`. Current example shows a single Fastly `/16` CIDR for the upstream egress allowlist. This is misleading two ways:

1. **Fails intermittently** — DNS resolves `cache.nixos.org` to different Fastly POPs; one `/16` won't cover them all
2. **Opens egress to ALL Fastly-backed sites** — that `/16` hosts thousands of Fastly customers, not just `cache.nixos.org`

Rewrite the example to show the multi-CIDR reality + a blast-radius note:

```yaml
# Fastly anycast — cache.nixos.org resolves to multiple POPs.
# CAUTION: these CIDRs open egress to ALL Fastly-backed origins,
# not just cache.nixos.org. L3 NetworkPolicy cannot distinguish by
# Host header. For tighter scoping, use an L7 egress proxy (Squid/
# Envoy with SNI allowlist) instead of CIDR-based NetPol.
upstreamEgressCIDRs:
  - 151.101.0.0/16
  - 199.232.0.0/16
  # ... (check Fastly's published IP ranges)
```

discovered_from=463.

### T116 — `docs(store):` add r[store.tenant.filter-scope] — outputs vs inputs boundary

MODIFY [`docs/src/components/store.md`](../../docs/src/components/store.md) near `r[store.tenant.narinfo-filter]` (`:195`). Two separate findings hit the same design boundary: [P0465](plan-0465-gateway-jwt-propagation-read-opcodes.md) initially over-threaded tenant filtering through *inputs* (later corrected), and bughunter found the sig_visibility_gate PutPath timing bug ([P0478](plan-0478-sig-visibility-gate-cluster-key-union.md)) — both are questions about *where the tenant boundary sits*. The spec needs an explicit marker:

```
r[store.tenant.filter-scope]
Tenant filtering applies to OUTPUTS (paths a tenant receives via QueryPathInfo/GetPath/narinfo), NOT to INPUTS (paths a tenant uploads via PutPath or that the scheduler feeds to builds). A tenant's builds may consume any path the cluster has; a tenant's *queries* see only paths the tenant built, substituted-via-trusted-key, or that verify against the tenant's trusted keys.
```

This codifies the boundary so future work has a grep-able reference. discovered_from=465 (+ bughunter P0463 context).


### T487 — `docs(tooling):` _extract_new_test_names — add "Rust-#[test]-only" qualifier

MODIFY [`.claude/lib/onibus/merge.py:556`](../lib/onibus/merge.py). The docstring says "Test fn names ADDED (not modified) by base..head" — but the function only matches Rust `#[test]`/`#[tokio::test]`/`#[rstest]` attrs via `_TEST_ATTR_RE` at `:548-552`. Python `def test_*`, vitest `it(...)`, nix VM subtests are invisible. Amend docstring:

```python
def _extract_new_test_names(base: str, head: str = "HEAD") -> list[str]:
    """Rust #[test]/#[tokio::test]/#[rstest] fn names ADDED (not modified)
    by base..head. Parses the unified diff for +-prefixed attrs. Cheap —
    just regex over diff output. Does NOT cover Python/vitest/nix VM tests
    — those run through the full .#ci gate regardless."""
```

discovered_from=479.

### T488 — `docs(builder):` main.rs audit comment — READ_ONLY_ROOT_MOUNTS table, not "if blocks"

MODIFY [`rio-builder/src/main.rs:172-174`](../../rio-builder/src/main.rs). Post-[P0467](plan-0467-readonly-root-mounts-const-table.md), the mounts live in a `READ_ONLY_ROOT_MOUNTS` const table at [`sts.rs:428`](../../rio-controller/src/reconcilers/common/sts.rs), not scattered `if p.read_only_root_fs` blocks. The audit comment still says "extend ... the `if p.read_only_root_fs` blocks". Correct to:

```rust
// Adding a new startup write? Extend BOTH this table AND the
// READ_ONLY_ROOT_MOUNTS const table in common/sts.rs (one row =
// one emptyDir volume+mount pair). vm-fetcher-split-k3s catches
// misses.
```

discovered_from=467.

### T489 — `docs(store):` SUBSTITUTE_STALE_THRESHOLD — drop phantom store.toml config claim

MODIFY [`rio-store/src/substitute.rs:43-44`](../../rio-store/src/substitute.rs). The doc-comment claims "main.rs threads `[substitute] stale_threshold_secs` from store.toml here" — but `grep stale_threshold_secs rio-store/src/main.rs` returns zero hits. The config doesn't exist. Either:

- **Option A (remove claim):** rewrite to "Overridable via [`Substituter::with_stale_threshold`] for tests; no production override knob currently." — honest.
- **Option B (add config):** actually plumb `[substitute] stale_threshold_secs` through `store.toml` → `main.rs` → `with_stale_threshold()`. Makes the doc true. More invasive; only if there's an operator use-case.

**Option A preferred** — the builder method exists for test-override, not production config. If a real config need surfaces, it's a separate feature. Apply the same fix to `with_stale_threshold()`'s docstring at `:177-178` ("main.rs threads ... here" → "Test-override knob; no production config path currently"). discovered_from=483.

### T490 — `docs(plan):` plan-0484 T2 — "cadence mechanism" → per-merge

MODIFY [`.claude/work/plan-0484-merger-cov-smoke-ci.md:29`](plan-0484-merger-cov-smoke-ci.md). T2 says "Invoke via the existing cadence mechanism (merge-count modulo, same pattern as consolidator/bughunter — every Nth merge)." — but the implementation at [`build.py:111-128`](../lib/onibus/build.py) runs `.#coverage-full` on EVERY merge (backgrounded by merger step 6 unconditionally). The per-merge design is better (faster detection of infrastructure-red); the plan text is stale. Rewrite to:

> Invoke on every merge (backgrounded by merger step 6 — existing behavior). The halt-check is cheap: `coverage_full_red()` scans the log post-red, no extra build.

discovered_from=484.

### T491 — `docs(tooling):` cli.py clause4-check comment — merger uses jq, not `|| exit`

MODIFY [`.claude/lib/onibus/cli.py:345-346`](../lib/onibus/cli.py). The comment says:

```python
# Nonzero on HALT so the merger's `|| exit` catches it without
# jq-parsing. halt_queue() has already written the sentinel.
```

But [`rio-impl-merger.md:87`](../agents/rio-impl-merger.md) shows the merger DOES jq-parse: `decision=$(jq -r .decision <<<"$verdict")`. There is no `|| exit` in the merger's step 4.5. The comment describes a contract that either never existed or was refactored away by [P0488](plan-0488-merger-clause4-wiring.md). Rewrite to match reality:

```python
# Distinct nonzero on HALT (3, not 1 — see P0304-T492) so callers
# can distinguish HALT-verdict from uncaught-exception. The merger
# jq-parses .decision; this exit code is belt-and-suspenders for
# shell callers that check $? directly.
```

Coordinates with [P0304](plan-0304-trivial-batch-p0222-harness.md) T492 (exit 1→3) and [P0496](plan-0496-merger-clause4-crash-handling.md) T1 (try/except wrap) — all three touch the same `:340-347` block. Whichever lands last sees the others' changes. discovered_from=488.

### T492 — `docs(test):` test_scripts.py:1506 — "realistic" → "plausible — P0430 log not retained"

MODIFY [`.claude/lib/test_scripts.py:1506`](../lib/test_scripts.py). Comment says "realistic nix-level error lines" — but none of the 4 positive-case lines at `:1508-1511` exist in any of 269 `/tmp/rio-dev` logs (the P0430 log that would have contained them is gone). Contrast: the false-positive guards at `:1537`/`:1549` ARE log-verified (`rio-p0454-impl-2.log:34269` checks out byte-for-byte). "Realistic" implies corpus-verified; these are plan-prescribed.

Fix:

```python
# Plausible nix-level error lines — plan-prescribed (P0447 doc),
# NOT corpus-verified (P0430 log not retained). Contrast false-
# positive guards at :1537/:1549 which ARE log-verified byte-for-
# byte. P0508 adds two corpus-derived positives below.
```

Cross-refs [P0508](plan-0508-infra-error-re-ssh-killed.md) T2 which adds ACTUALLY-corpus-derived positives to the same block. discovered_from=447.

### T493 — `docs(plan):` plan-0508 — strike Killed\s*$ prescription, add exit-137 deviation note

[`.claude/work/plan-0508-infra-error-re-ssh-killed.md`](../../.claude/work/plan-0508-infra-error-re-ssh-killed.md) prescribes `Killed\s*$` across 8 refs (title, T1 body, regex-snippet `:60`, rationale `:65-67`, test-fixture `:80`, negative-case `:92-97`, validation-cmds `:103-104`, file-manifest `:120-121`). Implementation correctly deviated to exit-code-137 after corpus evidence: zero `Killed`-at-EOL hits in 289 `/tmp/rio-dev` logs. Deviation IS documented at [`build.py:184-187`](../../.claude/lib/onibus/build.py) and the P0508 commit body — plan doc was never amended.

Archaeology from the plan hits a mismatch. Add a deviation header block at `:1`:

```markdown
> **Deviation note:** T1 prescribed `Killed\s*$`; implementation used
> exit-code-137 instead after corpus evidence (0 `Killed`-at-EOL hits
> in 289 logs, `/tmp/rio-dev` scan 2026-03). SSH-killed processes
> surface as exit 137 (128+SIGKILL) in the captured log, not the
> shell-echoed `Killed` string — that only appears on interactive
> TTYs. See `build.py:184-187` + commit body.
```

Optionally strike `:60` regex-snippet + `:80` fixture that still say `Killed\s*$` — or leave them as-is under the deviation header (archaeology value: shows what was tried before the corpus corrected it). discovered_from=508.

### T494 — `docs(controller):` select_deletable_jobs — drop false deterministic-order claim

[`rio-controller/src/reconcilers/builderpool/manifest.rs:724-726`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) doc-comment claims "BTreeMap iteration order → deterministic (stable Job delete ordering across ticks)". False: the function iterates `active_jobs: &[&Job]` (slice from `jobs_api.list()` — K8s List API has no contractual ordering), NOT the BTreeMap. The BTreeMap (`reapable`) is only consulted for the per-bucket budget cap.

Test `surplus_budget_caps_deletions_per_bucket` correctly does NOT assert WHICH Job dies — it knows the claim is wrong.

Two routes. **Route-(a), preferred:** make the claim true — sort `active_jobs` by `.metadata.creation_timestamp` before the loop (oldest-first deletion is sensible policy: the Job that's been surplus longest dies first). Minor behavior change; operators see consistent `kubectl get events` ordering.

**Route-(b), minimal:** drop the claim:

```rust
/// Per-bucket cap: at most `reapable[bucket]` Jobs from each bucket.
/// Iteration follows `active_jobs` order (K8s List API — not
/// contractually ordered across ticks). If deterministic delete
/// order matters operationally, sort by creation_timestamp first.
```

Route-(a) if this bundles with a behavior change anyway (e.g., adjacent to [P0511](plan-0511-manifest-failed-job-sweep.md)'s sweep); route-(b) if pure doc-fix. discovered_from=505.

### T495 — `docs(controller):` r[ctrl.pool.manifest-scaledown] — add controller-restart-resets-clocks sentence

[`docs/src/components/controller.md:154`](../../docs/src/components/controller.md) `r[ctrl.pool.manifest-scaledown]` says "Demand returning before the window elapses resets the clock." True but incomplete: `manifest_idle` is in-memory `Instant` ([`reconcilers/mod.rs:88`](../../rio-controller/src/reconcilers/mod.rs)) — meaningless across process boundaries. Controller restart resets ALL clocks.

Operationally relevant: if restarts are more frequent than `SCALE_DOWN_WINDOW` (600s default), scale-down NEVER fires. A cluster with a 5-minute controller restart cadence (aggressive image-pull, leader-election churn) will never delete idle manifest Jobs.

Design is correct (conservative — restart-resets means never-delete-prematurely). Just needs one sentence. Append to `:154`:

```markdown
Manifest-mode scale-down is per-bucket: when `supply > demand` for a `(memory-class, cpu-class)` bucket for `SCALE_DOWN_WINDOW` (600s default), the controller deletes `surplus` Jobs from that bucket. Deletion skips Jobs whose pods are mid-build (`running_builds > 0` from `ListExecutors`). Demand returning before the window elapses resets the clock. Controller restart resets ALL clocks (idle state is in-memory) — restarts more frequent than the window mean scale-down never fires.
```

No `tracey bump` — spec behavior didn't change, an operational caveat was added. Existing `r[impl]` annotations remain valid. discovered_from=505.

### T991618106 — `docs(store):` `r[store.substitute.tenant-sig-visibility]` — add cluster-key to trusted-set union + tracey bump

[`store.md:231`](../../docs/src/components/store.md) `r[store.substitute.tenant-sig-visibility]` says the trusted set is "union of B's upstream `trusted_keys` arrays". [P0478](plan-0478-sig-visibility-gate-cluster-key-union.md) added the cluster key to the code-side trusted set at [`grpc/mod.rs:534`](../../rio-store/src/grpc/mod.rs) `trusted.push(ts.cluster().trusted_key_entry())` — without it, a freshly-built path (rio-signed, `path_tenants` not yet populated) fails the gate. The code-side comment at `:525-527` explains it; the spec lagged.

Amend `:231`:

```markdown
r[store.substitute.tenant-sig-visibility]
A substituted path is cross-tenant visible only by signature: tenant B's `QueryPathInfo` for a path substituted by tenant A returns the path IFF at least one of the stored `narinfo.signatures` verifies against a key in tenant B's trusted set — the union of B's upstream `trusted_keys` arrays AND the rio cluster key. The cluster key is always trusted: a freshly-built path (rio-signed, `path_tenants` not yet populated by the scheduler) must verify against the cluster key, not only upstream keys, or it would fail the gate for the building tenant itself. This prevents tenant A from poisoning tenant B's store by substituting from a cache B doesn't trust.
```

**Run `tracey bump`** before committing — this is a meaningful behavior change (the trusted-set composition changed, not just phrasing). Marker becomes `r[store.substitute.tenant-sig-visibility+2]`. Existing annotations:
- `r[impl ...]` at whatever `grpc/mod.rs` line carries it → review + bump to `+2` (the P0478 code IS the implementation of the amended spec — bump is confirmation, not new work)
- `r[verify ...]` at [`signing.rs:769`](../../rio-store/src/signing.rs) → same review + bump

`tracey query rule store.substitute.tenant-sig-visibility` post-bump shows the stale annotations until they're bumped. discovered_from=478.

### T991618107 — `docs(test):` strip `r[...]` marker tokens from scenario-file prose section headers

CLAUDE.md `§ VM-test r[verify] placement` MUST-NOT: scenario-file header blocks may keep prose descriptions, but MUST NOT carry the marker token itself. `config.styx`'s `test_include` narrows to `nix/tests/default.nix` only — a marker in a scenario file is invisible to tracey, so no functional break today, but: (a) grep noise (`grep 'r\[ctrl' nix/` finds unwired markers), (b) copy-propagation risk (next subtest copies the pattern), (c) breaks if `test_include` ever widens.

**3 new sites** from P0512 in [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix):
- [`:2454`](../../nix/tests/scenarios/lifecycle.nix) — `# ── r[ctrl.pool.manifest-labels]: class labels present ────────`
- [`:2481`](../../nix/tests/scenarios/lifecycle.nix) — `# ── r[ctrl.pool.manifest-long-lived]: NO RIO_EPHEMERAL env ────`
- [`:2498`](../../nix/tests/scenarios/lifecycle.nix) — `# ── r[ctrl.pool.manifest-reconcile]: status_patch ran ─────────`

**1 pre-existing site** in [`dashboard-gateway.nix:310`](../../nix/tests/scenarios/dashboard-gateway.nix) — `# ── r[dash.auth.method-gate] end-to-end: ClearPoison unrouted ───────`

Fix: strip the `r[...]` wrapper, keep the rule-id as plain prose. E.g. `:2454`:

```python
# ── ctrl.pool.manifest-labels: class labels present ───────────────
```

The rule-id stays as a grep-able cross-reference; the `r[...]` token that tracey would try to parse is gone. Verify the REAL `# r[verify ...]` markers for these rules exist in [`default.nix`](../../nix/tests/default.nix) at the subtests wiring — if any are missing, add them there (structural coupling: no wiring → no marker → tracey catches unwired fragment). discovered_from=512.

## Exit criteria

- `/nbr .#ci` green (clippy-only gate; no behavior change)
- `grep 'ci-fast\|ci-local-fast' README.md` → 0 hits
- `grep 'coverage-options=branch' flake.nix nix/coverage.nix` → ≥1 hit (the DO-NOT comment)
- `grep -c 'OPEN' docs/src/remediations/phase4a.md` — significantly reduced from 129
- `grep 'CA support deferred' rio-worker/src/synth_db.rs` → 0 hits
- `grep 'TODO(phase4b)' rio-store/src/cache_server/auth.rs` → 0 hits
- `grep 'Bearer token authentication' docs/src/security.md | grep -v 'r\['` → 0 hits in not-yet-implemented context
- `grep 'MEMORY_MULTIPLIER\|input_closure_bytes' .claude/work/plan-0113*.md` → 0 hits
- `grep 'Intentional divergence' .claude/work/plan-0005*.md` → 0 hits (or fenced in `[WRONG]` erratum)
- `grep 'derivations_completed_total\|builds_by_status\|ready_queue_depth\|dispatch_latency' .claude/work/plan-0222*.md` → 0 hits (T10: nonexistent names removed)
- `grep 'kind: Build' docs/src/integration.md` → 0 hits (T11: CRD YAML removed)
- `grep 'Build CRD' docs/src/failure-modes.md` → 0 hits (T12)
- `grep 'MOOT-P0294' docs/src/remediations/phase4a.md` → ≥10 hits (T13: rows flipped)
- `grep 'ssh-ng' docs/src/components/gateway.md | grep -i 'wopSetOptions\|does NOT'` → ≥1 hit (T16: caveat present)
- `grep 'StoreServer' rio-store/src/main.rs` → 0 hits (T19)
- `grep 'SchedulerService.GetBuildLogs' docs/src/components/dashboard.md` → 0 hits (T20)
- `grep "digest(\$out\|digest('\$out" .claude/work/plan-0206-*.md` → 0 hits (T21: pgcrypto spelling removed)
- `grep 'onibus build excusable' .claude/work/plan-0313-*.md .claude/work/plan-0304-*.md` → 0 hits (T22: wrong subcmd group corrected)
- `python3 -c 'import json; json.loads(open(".claude/work/plan-0309-helm-template-fodproxyurl-workerpool.md").read().split("json deps")[1].split("\n")[1])'` → no ValueError (T23a: leading-zero fixed)
- `grep -E 'Five.*bugs|All four' .claude/work/plan-0306-*.md` → 0 hits (T23b: count reconciled)
- T23c+d criterion: see T24 — original form was self-defeating (the criterion-text itself matches the grep)
- `grep -- '-accel' .claude/work/plan-0316-*.md | grep -v 'ITER-1\|rejected\|incompatible'` → 0 (T26: pre-pivot text corrected or fenced in ITER-1 erratum)
- `grep -- '-machine accel=kvm' .claude/work/plan-0316-*.md` → ≥3 hits (T26: corrected option in prose + code blocks)
- `python3 -c 'from onibus.jsonl import read_jsonl; from onibus.models import PlanRow; r = [x for x in read_jsonl(".claude/dag.jsonl", PlanRow) if x.plan == 316][0]; assert "-machine" in r.title or "accel=kvm" in r.title'` (T26: dag row title corrected)
- T25 conditional: `grep -- '-accel kvm forces' .claude/known-flakes.jsonl` → 0 (T25: pre-pivot bracket text corrected — OR superseded by P0317 T4 mitigation-migration)
- T27: no file criterion — prose record for grep `preamble.*gap`
- `grep 'because.*reading.*not.*writing' .claude/agents/rio-planner.md` → 0 hits (T28: muddled WHY removed)
- `grep 'session-cached\|WORKTREE copy' .claude/agents/rio-planner.md` → ≥1 hit (T28: corrected WHY present)
- T29 conditional: `grep 'impl-1.log' .claude/work/plan-0317-*.md | grep -E ':21:|:243:|:358:'` → 0 hits IF P0317 hasn't already merged with the fix; skip if P0317 DONE with correction
- T30: `grep 'dash.envoy.grpc-web-translate+2' .claude/work/plan-0273-*.md` → ≥2 hits (T1 template + T5 template both versioned)
- T30: `grep 'grpc-web-translate]' .claude/work/plan-0273-*.md | grep -v '+2'` → 0 hits (no unversioned refs remain)
- T31: `nix develop -c tracey query rule dash.journey.build-to-logs` → text non-empty (blank-line fix makes tracey see it)
- T31: same check for `dash.graph.degrade-threshold` and `dash.stream.log-tail`
- T32: `grep 'r\[sched.classify.proactive-ema\]' docs/src/components/scheduler.md` → 1 hit (new marker added)
- T32: `nix develop -c tracey query rule sched.classify.proactive-ema` → non-empty rule text (marker parsed)
- T32: `grep 'penalty-overwrite+2\|penalty-overwrite\]' docs/src/components/scheduler.md` — if tracey-bumped, `+2` appears; check the `r[impl]` annotations in code are updated too (or left as stale-to-be-reviewed per tracey bump semantics)
- T33: `grep 'round-trip does not exist\|TODO(P0260).*call site' rio-gateway/src/server.rs` → 0 hits (stale doc removed; post-P0260-merge)
- T34: `grep 'until P0260 wires\|P0260 VM test covers the full flow' rio-gateway/src/server.rs` → 0 hits (future-tense + wrong-claim removed)
- T35: `grep 'readinessProbe probed ""' nix/tests/scenarios/lifecycle.nix` → 0 hits (conflation fixed)
- T35: `grep 'main.rs:380-392\|main.rs:380' nix/tests/scenarios/lifecycle.nix` → 0 hits (stale line ref updated)
- T36: `grep 'r\[impl ctrl.probe.named-service\]' rio-proto/src/client/balance.rs` → ≥1 hit (second annotation added)
- T37: `grep 'Cold-cache.*60-90min\|three full Nix' docs/src/verification.md` → ≥1 hit (cost note added; post-P0300-merge)
- T38: `grep 'branch-deletion\|lockfile pins the rev' flake.nix | head -1` → ≥1 hit in the nix-stable input comment (post-P0300-merge)
- T39: `grep 'sub-markers\|ephemeral-deadline.*ephemeral-single-build' docs/src/components/controller.md` → ≥1 hit in the r[ctrl.pool.ephemeral] paragraph (post-P0296-merge)
- T40: `grep 'same txn' migrations/018_chunk_tenants.sql` → 0 hits (T40: false same-txn claim removed)
- T40: `grep 'autocommit.*autocommit\|two separate statements' migrations/018_chunk_tenants.sql` → ≥1 hit (T40: honest autocommit note present)
- T41: `grep 'No validation-relaunch needed' .claude/agents/rio-impl-validator.md` → 0 hits (T41: contradictory claim removed)
- T41: `grep 'coordinator.*relaunches\|No SendMessage-to-impl' .claude/agents/rio-impl-validator.md` → ≥1 hit (T41: clarified prose)
- T42: `grep 'scheduled at P0260\|until then pubkey is None' docs/src/multi-tenancy.md` → 0 hits (T42: stale ref removed; post-P0349-merge)
- T43: `grep 'pre-P0260 deploy' rio-scheduler/src/grpc/tests.rs` → 0 hits (T43: stale forward-ref corrected; post-P0349-merge)
- T44: `grep 'ResBody.*HTTP response body\|inference flows from' rio-test-support/src/grpc.rs` → ≥1 hit (T44: ResBody param docstring added; post-P0351-merge)
- T45: `grep 'TODO(P0286)' CLAUDE.md` → 0 hits (T45: stale plan-coupled example replaced; post-P0286-merge)
- T46: `grep 'already works for stacked amends' .claude/work/plan-0358-*.md` → 0 hits (T46: T1 prose aligned with DESIGN-NOTE approach-2)
- T46: `grep 'for-each-ref --contains\|orphan-base' .claude/work/plan-0358-*.md | grep -c T1` → ≥1 (T46: T1 references the approach-2 mechanism)
- T47: `grep ':326\|:67-70\|:313\|:270-345' .claude/work/plan-0352-*.md` → 0 hits OR all verified-current at dispatch (line-cites anchored or updated)
- T48: `grep ':160\|:363\|:389' .claude/work/plan-0304-*.md | grep T67` → 0 hits (T67 retargeted to :316/:342, :160 bullet deleted)
- T49: `grep '+6.*post-P0342' .claude/work/plan-0304-*.md` → 0 hits OR updated with post-P0345 re-verified delta
- T50: `grep '~~T40\|OBE.*P0353' .claude/work/plan-0295-*.md` → ≥1 hit (T40 struck + OBE note)
- T51 conditional: `python3 -c 'import json; json.loads(open(".claude/work/plan-0345-put-path-validate-metadata-helper.md").read().split("json deps")[1].split("\n")[1])'` → no ValueError (leading-zero fixed) — LOW VALUE, DONE-archaeology
- T52: `grep 'rio_scheduler_build_graph_edges' docs/src/observability.md | grep -c 'Histogram Buckets\|^|'` → ≥2 (inline :120 + table row added)
- T52: `grep 'observability.md:119' rio-common/src/observability.rs` → 0 hits (off-by-one cite fixed or de-lineified)
- T62: `grep 'grpc-web-translate\]' infra/helm/rio-build/templates/dashboard-gateway.yaml | grep -v '+2'` → 0 hits (unversioned marker fixed)
- T63: `grep 'observes which\|depends on.*enter-time resolution' docs/src/observability.md` → 0 hits (hedge removed, observation committed)
- T63: `grep 'CONFIRMED: span_from_traceparent\|assert overlap\|assert not overlap' nix/tests/scenarios/observability.nix` → ≥1 hit (regression guard, not just observe-only)
- T64: `grep '^/// r\[' rio-proto/src/interceptor.rs rio-scheduler/src/actor/worker.rs rio-scheduler/src/actor/build.rs rio-scheduler/src/actor/command.rs rio-gateway/src/handler/mod.rs | grep -v 'r\[impl\|r\[verify'` → 0 hits (prose-refs rephrased; may need adjusted grep for exact col)
- T65: `grep "80 00 00 00\|grep -q '\\^80'" nix/tests/default.nix nix/tests/scenarios/cli.nix` → ≥1 hit (tightened grep pattern, partner to P0304-T43)
- T66: `nix develop -c tracey query rule sched.admin.sizeclass-status` → non-empty rule text (marker added + parsed)
- T66: `grep 'r\[sched.admin.sizeclass-status\]' docs/src/components/scheduler.md` → 1 hit (standalone paragraph, blank line before)
- T53: `grep 'main.rs:676\|load_jwt_pubkey(path).await' nix/tests/scenarios/lifecycle.nix` → 0 hits (stale cite removed)
- T53: `grep 'load_and_wire_jwt' nix/tests/scenarios/lifecycle.nix` → ≥1 hit (corrected fn name)
- T54: `grep 'before the first prepare run' flake.nix` → 0 hits (stale rationale reworded; post-P0297-merge)
- T54: `grep 'maybeMissing.*.sqlx\|.sqlx.*regenerates' flake.nix` → ≥1 hit (wrapper kept with honest comment OR wrapper dropped)
- T55: `grep 'sign_for_tenant' rio-store/src/grpc/mod.rs rio-store/src/main.rs` → 0 hits (both stale refs reworded to resolve_once/maybe_sign)
- T56: `grep 'ERRATUM.*admin.rs\|Dead-code collapse.*T91' .claude/work/plan-0352-*.md` → ≥1 hit (erratum block added)
- T57: `grep -c 'upload_skipped_idempotent_total\|fuse_circuit_open' docs/src/observability.md` → ≥2 (both table rows present)
- T57: `grep 'upload_skipped_idempotent_total' rio-worker/tests/metrics_registered.rs` → 1 hit (WORKER_METRICS const synced)
- T58: `grep 'config-driven via.*scheduler.toml.*rebalancer' docs/src/components/scheduler.md` → 0 hits IF P0304-T95 not yet dispatched (spec honest-about-defaults-only); OR ≥1 hit IF T95 dispatched first (spec accurate, T58 skipped)
- T58 (alternate, if P0304-T95 not first): `grep 'Scheduled.*P0304-T95\|config-load wiring' docs/src/components/scheduler.md` → ≥1 hit (forward-pointer blockquote)
- T59: `grep 'List only' infra/helm/rio-build/templates/rbac.yaml` → 0 hits (replaced with DisruptionTarget-watcher mention)
- T59: `grep 'DisruptionTarget watcher\|disruption.rs' infra/helm/rio-build/templates/rbac.yaml` → ≥1 hit
- T60: `grep 'per.*literal segment' .claude/work/plan-0364-*.md` → ≥1 hit (:33 clarified)
- T61 conditional (post-P0360-merge): `grep 'not wired here\|not in airgap set' nix/tests/fixtures/k3s-full.nix` → 0 hits at :470-471 region
- T61 conditional: `grep 'vmtest-full-nonpriv\|privileged-hardening-e2e' nix/tests/fixtures/k3s-full.nix` → ≥1 hit in the waitReady comment block
- T67: `grep 'rio-dashboard.*svelte\|rio-dashboard.*\.ts' .config/tracey/config.styx` → ≥1 hit (dashboard globs added) — OR `.claude/notes/tracey-svelte-limitation.md` exists if tracey can't parse
- T67: `nix develop -c tracey query rule dash.journey.build-to-logs` → shows ≥1 impl + ≥1 verify site (Cluster.svelte + Cluster.test.ts now scanned) — conditional on tracey-svelte parse
- T68: `grep 'createConnectTransport' .claude/work/plan-0277-*.md | grep files-fence\|:121` → 0 hits in files-fence (corrected to GrpcWeb)
- T69-A: `grep '{{ .Values.envoyGateway.namespace }}\|.Values.envoyGateway' infra/helm/rio-build/templates/networkpolicy.yaml` → ≥1 hit (templated)
- T69-B (if A not taken): `grep 'also.*hardcoded.*docker.nix\|also.*hardcoded.*networkpolicy' nix/docker.nix infra/helm/rio-build/templates/networkpolicy.yaml` → ≥2 hits (cross-ref comments)
- T70: `sed -n '8p' .claude/work/plan-0289-*.md | grep scheduling.nix` → match (table cell corrected)
- T71: `grep 'DerivationInfo' .claude/work/plan-0250-*.md .claude/work/plan-0245-*.md` → 0 hits (corrected to DerivationNode or actual message name)
- T72: `grep 'bloom_fill_ratio.*0.5\|bloomExpectedItems' docs/src/observability.md | grep -c Alerting\|:30[3-9]\|:31` → ≥1 (Alerting section entry added)
- T73: `grep 'reserved 3\|RESERVED.*future.*field' .claude/work/plan-0291-*.md rio-proto/proto/types.proto` → check clarity (may be no-op if already clear)
- T74: `grep 'garbage rate\|garbage-rate' rio-dashboard/src/pages/GC.svelte` → ≥1 hit (comment corrected) OR `grep 'scanned.*total\|totalPaths' rio-dashboard/src/pages/GC.svelte` → ≥1 hit if GCProgress has a total field (actual progress ratio used)
- T75: `grep -c 'TODO(P0311)' rio-gateway/src/handler/mod.rs rio-gateway/src/translate.rs rio-worker/src/config.rs` → 0 (retagged to WONTFIX(P0310) or new owner plan)
- T75: `grep -c 'WONTFIX(P0310)' rio-gateway/src/handler/mod.rs rio-gateway/src/translate.rs rio-worker/src/config.rs` → ≥4 (all 4 sites aligned — config.rs:111 already had it, handler:110+500 + translate:547 added)
- T76: `grep 'WONTFIX(P0NNN)' CLAUDE.md` → ≥1 hit (convention documented)
- T77: `grep 'r\[impl proto.stream.bidi\]' rio-scheduler/src/grpc/mod.rs` → 0 hits (moved out)
- T77: `grep 'r\[impl proto.stream.bidi\]' rio-scheduler/src/grpc/worker_service.rs` → ≥1 hit (moved to actual impl site)
- T78: `grep 'P0271 adds a GetBuild RPC' rio-dashboard/src/pages/Builds.svelte` → 0 hits (wrong cross-ref removed)
- T79: `grep 'ADR-018\|018-ca-resolution' docs/src/decisions.md` → ≥1 hit (table row added)
- T80: `grep -c 'r\[ctrl\.pool\.bloom-knob\]' docs/src/components/controller.md` → 1 (duplicate at :142-143 deleted; :119 remains)
- T81: `grep 'kube|finalizer|invalid_spec|scheduler_unavailable|conflict' rio-controller/src/lib.rs` → ≥1 hit (describe_counter enumeration updated)
- T92: `grep 'Not met as written' .claude/work/plan-0299-*.md` → ≥1 hit (T90 landed); `grep 'rio_scheduler_ca_hash_compares_total\|rio_gateway_jwt_mint_degraded_total' docs/src/observability.md` → ≥2 hits (T91a+T91b both landed)
- T93: `grep 'admin_types.proto' .claude/work/plan-0271-*.md` → ≥2 hits (T1 erratum + files-fence updated); `grep 'Erratum.*P0376\|P0376 split' .claude/work/plan-0271-*.md` → ≥1 hit
- T94: `grep 'FILED UPSTREAM\|skip-uncovered' .claude/notes/tracey-svelte-yaml-limitation.md` → ≥1 hit (FR tracking note added)
- T95: `grep 'grpc/tests.rs' .claude/work/plan-0287-*.md .claude/work/plan-0311-*.md` → 0 hits in active-plan files-fences (retargeted to grpc/tests/{submit,stream}_tests.rs; post-P0395-merge); T43 in THIS plan's :409-411 also re-targeted
- T96: `grep 'vmtest-full-nonpriv.yaml:33' nix/tests/fixtures/k3s-full.nix` → 0 hits (dangling cross-ref updated; post-P0387-merge); `grep 'extraValues.*default.nix' nix/tests/fixtures/k3s-full.nix` → ≥1 hit (updated comment pattern-ref)
- T81: `grep '5 values\|five values' rio-controller/src/error.rs` → ≥1 hit (comment count corrected)
- T82: `grep 'types::DerivationNode.*or.*dag::DerivationNode\|dag::DerivationNode.*or.*types::DerivationNode' rio-proto/src/lib.rs` → ≥1 hit (both paths present once each; post-P0376)
- T82: `grep -o 'dag::DerivationNode' rio-proto/src/lib.rs | wc -l` → check ≤2 (once in doc-comment, once in pub use — not the same-path-twice typo)
- T83: `grep 'OwnerReferencesPermissionEnforcement' infra/helm/rio-build/templates/rbac.yaml` → ≥1 hit (corrected explanation; post-P0235)
- T83: `grep 'finalizer() patches metadata.finalizers via' infra/helm/rio-build/templates/rbac.yaml` → 0 hits (misleading text removed)
- T84: `grep 'infra/helm.*yaml' .config/tracey/config.styx` → ≥1 hit IF option-(a) taken (yaml glob added); OR `grep 'r\[impl dash.auth.method-gate\]' rio-*/src/` → ≥1 hit IF option-(c) taken (Rust anchor file)
- T84: `nix develop -c tracey query rule dash.auth.method-gate` → shows ≥1 impl site (previously-invisible YAML annotation OR Rust anchor — either way the rule is no longer uncovered)
- T85: `grep 'LOCKSTEP.*docker-pulled' infra/helm/rio-build/values.yaml` → ≥1 hit (forward cross-ref to airgap set; post-P0379)
- T85: `grep 'LOCKSTEP.*values.yaml' nix/docker-pulled.nix` → ≥1 hit (reciprocal cross-ref at :94)
- T86: `grep 'Panic in ONE.*unwinds.*immediately' rio-controller/src/main.rs` → ≥1 hit (tokio::join! semantics clarified — both Ok-exit-waits and panic-unwinds covered)
- T87: `grep 'admin/mod.rs:562' docs/src/remediations/phase4a/11-tonic-health-drain.md` → 0 hits (stale range gone); `grep 'admin/gc.rs' docs/src/remediations/phase4a/11-tonic-health-drain.md` → ≥3 hits (refs retargeted to post-P0383 file)
- T88: `grep '9 crates' docs/src/crate-structure.md` → 0 hits (count corrected); `grep 'rio-cli\|rio-crds\|rio-bench' docs/src/crate-structure.md` → ≥3 hits in Workspace Layout tree (all three present, NOT under "Not yet built")
- T88: `grep -c 'hmac\|jwt_interceptor\|server\.rs\|signal\.rs\|tenant\.rs\|tls\.rs' docs/src/crate-structure.md` → ≥6 (rio-common 15-module list present)
- T88: `grep -c 'admin_types\.proto\|build_types\.proto\|dag\.proto' docs/src/crate-structure.md` → 3 (post-P0376 proto split reflected)
- T88: `bash -c 'doc=$(grep "## Workspace Layout" -A3 docs/src/crate-structure.md | grep -oE "[0-9]+ crates" | grep -oE "[0-9]+"); actual=$(grep -cE "^\s+\"rio-" Cargo.toml); [ "$doc" = "$actual" ]'` → exit 0 (doc count matches Cargo.toml members)
- T89: `grep 'Builds.svelte\|Cluster.svelte\|pages/\*\*/\*.svelte' .config/tracey/config.styx` → ≥1 hit (dashboard pages included; post-P0279-merge)
- T89: `nix develop -c tracey query rule dash.journey.build-to-logs` → shows ≥2 `impl` sites at `Builds.svelte:2` + `Cluster.svelte:2` (no longer uncovered; kill tracey daemon first)
- T90: `grep 'Not met as written\|P0311-T58' .claude/work/plan-0299-staggered-scheduling-cold-start.md` → ≥1 hit (blockquote added at `:161`)
- T91: `grep 'rio_scheduler_ca_hash_compares_total' docs/src/observability.md` → ≥1 hit (Scheduler Metrics table row added)
- T91: `grep 'rio_gateway_jwt_mint_degraded_total' docs/src/observability.md` → ≥1 hit (Gateway Metrics table row added)
- T97: `grep 'These three tests\|:63-75' rio-dashboard/src/components/__tests__/LogViewer.test.ts` → 0 hits (stale count+line-ref removed); `grep 'conditional rendering branches' rio-dashboard/src/components/__tests__/LogViewer.test.ts` → ≥1 hit (generic phrasing)
- T98: `grep 'paths_each=5\|Actually simpler' rio-scheduler/src/actor/tests/worker.rs` → 0 hits (mid-thought-leftover deleted, "Actually" rephrased); `sed -n '1169p' rio-scheduler/src/actor/tests/worker.rs | grep 'paths_each = 70'` → match (actual assignment unchanged)
- T99: `grep 'sequential not concurrent\|NotFound contributing 0\|BFS fails hard' rio-gateway/src/translate.rs` → ≥2 hits (rationale restored)
- T100: `grep 'erratum\|scheduler-side.*not gateway\|crate-private' .claude/work/plan-0413-*.md` → ≥1 hit (erratum blockquote added)
- T101: `grep 'admin/mod.rs\|scaling\.rs[^/]' .claude/work/plan-0370-*.md` → 0 hits pointing at deleted paths (retargeted to admin/gc.rs, scaling/standalone.rs)
- T102: `grep 'ChunkServiceClient' rio-proto/src/lib.rs` → route-(a): line has `TODO(P0NNN)` tag; OR route-(b): 0 hits (dead re-export deleted)
- T103: `head -5 .claude/work/plan-0316*.md | grep -i 'disabled\|closure\|superseded'` → ≥1 hit
- T104: `grep 'per-DOC\|per-doc' .claude/work/plan-0306*.md` → ≥1 hit at :255 region
- T105: `grep 'MUST be ≥\|MUST be >=' docs/src/components/gateway.md` → ≥1 hit at :387
- T106: `grep 'route-a\|PutPathManifest' .claude/work/plan-0434*.md` → ≥1 hit at :58 soft-dep note
- T107: `grep 'sched.ca.cutoff-propagate' rio-scheduler/src/actor/tests/lifecycle_sweep.rs` → 0 hits at :242,:326 (retargeted)
- T108: `grep 'test-only\|ManifestKind::total_size' rio-store/src/manifest.rs` → ≥1 hit in :147-152 doc-comment
- T109: `grep 'Worker Metrics' rio-builder/src/lib.rs` → 0 hits
- T110: `grep 'rio-worker/' docs/src/decisions/005-builder-store-model.md` → 0 hits
- T111: `grep 'rio-worker/' docs/src/decisions/012-privileged-builder-pods.md` → 0 hits
- T112: `grep '\[worker.md\]' docs/src/remediations/phase4a/{02,09,15}-*.md` → 0 hits
- T113: `grep 'rio_scheduler_fod_queue_depth\|rio_scheduler_fetcher_utilization' docs/src/observability.md | grep '^|'` → ≥2 hits (table rows added)
- T113: `grep 'rows added when the emitters land' docs/src/observability.md` → 0 hits (blockquote rewritten past-tense)

- T114: `grep 'rio_store_substitute_total\|rio_store_substitute_bytes\|rio_store_substitute_duration' docs/src/observability.md | grep '^|'` → ≥3 hits (table rows added)
- T115: `grep 'Fastly\|blast-radius\|SNI allowlist' infra/helm/rio-build/values.yaml` → ≥2 hits at :206 region
- T116: `grep 'r\[store.tenant.filter-scope\]' docs/src/components/store.md` → ≥1 hit; `nix develop -c tracey query rule store.tenant.filter-scope` shows the marker
- T487: `grep 'Rust.*#\[test\]' .claude/lib/onibus/merge.py` → ≥1 hit in _extract_new_test_names docstring
- T488: `grep 'READ_ONLY_ROOT_MOUNTS' rio-builder/src/main.rs` → ≥1 hit in audit comment at :172
- T489: `grep 'stale_threshold_secs' rio-store/src/substitute.rs` → 0 hits in doc-comments (phantom config removed)
- T490: `grep 'cadence mechanism\|every Nth merge' .claude/work/plan-0484-merger-cov-smoke-ci.md` → 0 hits; `grep 'every merge' .claude/work/plan-0484-merger-cov-smoke-ci.md` → ≥1 hit
- T491: `grep '|| exit' .claude/lib/onibus/cli.py` near :345 → 0 hits; comment mentions jq-parses + exit-3
- T492: `grep 'realistic nix-level' .claude/lib/test_scripts.py` → 0 hits; `grep 'Plausible.*plan-prescribed\|NOT corpus-verified' .claude/lib/test_scripts.py` → ≥1 hit
- T493: `grep 'Deviation note\|exit-code-137\|exit 137' .claude/work/plan-0508-infra-error-re-ssh-killed.md` → ≥1 hit in a header block near `:1`
- T494: `grep 'BTreeMap iteration order.*deterministic' rio-controller/src/reconcilers/builderpool/manifest.rs` → 0 hits; comment either replaced with "K8s List API — not contractually ordered" (route-b) OR `active_jobs` is sorted by creation_timestamp before the loop (route-a)
- T495: `grep 'Controller restart resets\|in-memory' docs/src/components/controller.md` near `:154` → ≥1 hit; no `tracey bump` (caveat addition, not behavior change)
- T991618106: `grep 'cluster key' docs/src/components/store.md | grep -i 'trusted'` → ≥1 hit in `r[store.substitute.tenant-sig-visibility]` text; `nix develop -c tracey query rule store.substitute.tenant-sig-visibility` → shows `+2` version suffix (bump ran); `grep 'tenant-sig-visibility+2' rio-store/src/grpc/mod.rs rio-store/src/signing.rs` → ≥2 hits (impl+verify bumped to match)
- T991618107: `grep -E '^ *# .*r\[' nix/tests/scenarios/lifecycle.nix nix/tests/scenarios/dashboard-gateway.nix` → 0 hits (all prose `r[...]` tokens stripped — box-drawing section headers now carry bare rule-id); `grep 'r\[verify ctrl.pool.manifest-' nix/tests/default.nix` → ≥3 hits (REAL verify markers at subtests wiring — add if missing)

## Tracey

**T32 adds one new marker:** `r[sched.classify.proactive-ema]` → [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) after `:211`. Proactive EMA updates from worker Progress reports are a distinct mechanism from penalty-overwrite (both update `ema_duration_secs`, different triggers/blending). P0266's db.rs gets the `r[impl]` annotation; its tests get `r[verify]`. See `## Spec additions` below for the marker text staging.

**T32 bumps one marker:** `r[sched.classify.penalty-overwrite]` text at `:209` amended to acknowledge mid-run proactive updates — `tracey bump` → `+2`. Existing `r[impl]` annotations become stale-to-review (the penalty-overwrite LOGIC didn't change; the "post-completion-only" claim did).

**T31 is tracey-mechanical:** three `r[dash.*]` markers already exist at [`dashboard.md:29/33/37`](../../docs/src/components/dashboard.md); the blank-line deletions make tracey parse their text. No bump — text didn't change, tracey's view of it did. Same fix-class as P0304-T27 for `r[sched.ca.*]`.

Remaining items are errata in plan docs, README, code comments. `r[sched.trace.assignment-traceparent]` is NOT touched here (that was P0160's item, carved out to P0293).

**T991618106 bumps one marker:** `r[store.substitute.tenant-sig-visibility]` at [`store.md:230`](../../docs/src/components/store.md) amended to include the cluster key in the trusted-set union — `tracey bump` → `+2`. Existing `r[impl]` at grpc/mod.rs + `r[verify]` at signing.rs become stale-to-review; both should bump to `+2` (P0478's code IS the amended behavior — bump is confirmation not rework).

**T991618107 is tracey-hygiene:** strips `r[...]` marker tokens from scenario-file prose section headers (4 sites). `test_include` doesn't scan scenarios/ so tracey never saw these — no functional change, just policy enforcement + grep-noise reduction.

## Spec additions

**T32 — new `r[sched.classify.proactive-ema]`** (goes to [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) after `:211`, standalone paragraph, blank line before, col 0):

```
r[sched.classify.proactive-ema]
When a worker reports `memory_used_bytes > 0` in a `Progress` update, the scheduler proactively updates `ema_duration_secs` for the running derivation's `(pname, system)` key using the partial elapsed time. This gives the classifier fresher data for subsequent submissions of the same package even before the current build completes — useful for long-running builds where waiting for completion delays class-correction by hours. The proactive update uses standard EMA blending (not penalty-overwrite); it is recorded via `rio_scheduler_ema_proactive_updates_total`.
```

**T66 — new `r[sched.admin.sizeclass-status]`** (goes to [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) after `:145` create-tenant paragraph, blank line before, col 0):

```
r[sched.admin.sizeclass-status]
`AdminService.GetSizeClassStatus` returns per-class status: configured vs effective cutoffs (the rebalancer may have recomputed), queued/running counts, sample counts in the rebalancer's lookback window. HUB for the WPS autoscaler, CLI cutoffs table, and CLI WPS describe. Returns empty `classes` list when size-class routing is disabled (no `[[size_classes]]` entries in scheduler.toml).
```

**Status:** this planner run has ADDED the `r[sched.admin.sizeclass-status]` marker to [`scheduler.md:147`](../../docs/src/components/scheduler.md) — T66 is DONE at plan-doc-authoring time (marker-first discipline). When P0295 dispatches, T66 verifies the marker is still present and `tracey query rule sched.admin.sizeclass-status` returns non-empty text.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: DO-NOT branch-coverage comment at :250 (P0143)"},
  {"path": "nix/coverage.nix", "action": "MODIFY", "note": "T2: DO-NOT branch-coverage comment in header (P0143)"},
  {"path": "docs/src/remediations/phase4a.md", "action": "MODIFY", "note": "T3: sweep index OPEN→FIXED(rem-NN) at :1457-1588 (P0200)"},
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "T4: 'deferred'→'by design' at :13,:93,:158 + drv_content-nonempty note (P0051)"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T5: TODO(phase4b)→TODO(phase5) at :36, :7 (P0154)"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T5: strike Bearer token from not-yet-implemented at :69 (P0154)"},
  {"path": ".claude/work/plan-0113-closure-size-estimator-fallback.md", "action": "MODIFY", "note": "T6: delete fabricated MEMORY_MULTIPLIER/input_closure_bytes, describe DURATION proxy (P0113)"},
  {"path": ".claude/work/plan-0005-live-daemon-golden-conformance.md", "action": "MODIFY", "note": "T7: [WRONG — bug #11] erratum at :51, strike 'intentional' :33,:63 (P0005)"},
  {"path": ".claude/work/plan-0076-figment-config.md", "action": "MODIFY", "note": "T8: forward-pointer to 2ab2d22e rename (P0076)"},
  {"path": ".claude/work/plan-0025-nar-streaming-refactor.md", "action": "MODIFY", "note": "T9: HashingReader cfg(test)-gated note (P0025)"},
  {"path": ".claude/work/plan-0222-grafana-dashboards.md", "action": "MODIFY", "note": "T10: rewrite T1-T4 PromQL tables to match shipped metric names (P0222)"},
  {"path": "docs/src/integration.md", "action": "MODIFY", "note": "T11: delete Build CRD kubectl-apply section :84-100 (USER-FACING — CRD deleted by P0294)"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "T12: remove Build CRD refs at :10 :15"},
  {"path": "docs/src/remediations/phase4a/03-controller-stuck-build.md", "action": "MODIFY", "note": "T13: MOOT-P0294 tombstone header"},
  {"path": "docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md", "action": "MODIFY", "note": "T14: Build CR → rio-cli/gRPC refs at :443 :618 :695; leave TODO(P0289)"},
  {"path": "rio-controller/Cargo.toml", "action": "MODIFY", "note": "T15: stale Build CRD comment at :9"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T15: stale Build CRD comment at :27"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T15: stale comment :102; T18: denied.invalid → k3s-agent :314 (p243-worktree line — CROSS-WORKTREE caveat)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T16: ssh-ng wopSetOptions caveat at :45 + tracey bump r[gw.opcode.set-options.field-order]"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "T16: (ssh:// only) annotation at :10 :71"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "T16: wopSetOptions comment at :214 — ssh-ng caveat"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T17: three layers → two at :199-201 (CROSS-WORKTREE — p243; fix before merge or via P0243 fix-impl)"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T19: StoreServer → StoreServiceImpl at :632"},
  {"path": ".claude/work/plan-0218-nar-buffer-config.md", "action": "MODIFY", "note": "T19: same StoreServer typo in plan doc"},
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T20: SchedulerService.GetBuildLogs → AdminService at :22"},
  {"path": ".claude/work/plan-0206-path-tenants-migration-upsert.md", "action": "MODIFY", "note": "T21: digest() → sha256(convert_to()) at :111 :118 — pgcrypto not enabled; PG11+ builtin is what shipped"},
  {"path": ".claude/work/plan-0313-kvm-fast-fail-preamble.md", "action": "MODIFY", "note": "T22: onibus build excusable → onibus flake excusable at :22 :117"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T22: onibus build excusable → onibus flake excusable at :151 (T10 header — LIVE instruction, P0304 UNIMPL); T23d: wrong P0207 filename at :326"},
  {"path": ".claude/work/plan-0309-helm-template-fodproxyurl-workerpool.md", "action": "MODIFY", "note": "T23a: soft_deps [0308] → [308] at :100 — leading zero is invalid JSON"},
  {"path": ".claude/work/plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md", "action": "MODIFY", "note": "T23b: Five/four count slip at :3 — reconcile with actual T-count"},
  {"path": ".claude/work/plan-0316-qemu-force-accel-kvm.md", "action": "MODIFY", "note": "T26: -accel → -machine accel=kvm sweep at :1,:7,:34,:42,:44,:73,:87,:102,:105,:145 + ITER-1 erratum header"},
  {"path": ".claude/dag.jsonl", "action": "MODIFY", "note": "T26: P0316 row title -accel → -machine accel=kvm"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T25: CONDITIONAL — -accel → -machine accel in P0316 bracket IF P0317 T4 hasn't already migrated it to mitigations list. Check at dispatch."},
  {"path": ".claude/agents/rio-planner.md", "action": "MODIFY", "note": "T28: :127 muddled 'because reading not writing' → correct 'because cd picks up worktree copy' (bash :130 is correct; prose WHY is wrong)"},
  {"path": ".claude/work/plan-0317-excusable-vm-regex-knownflake-schema.md", "action": "MODIFY", "note": "T29: CONDITIONAL — QA WARN fixes (impl-1→impl-2 at :21/:243/:358; T6 defeated-test; T1 ^error: anchor comment). P0317 implementer has these in prompt — skip if already fixed at merge."},
  {"path": ".claude/work/plan-0273-envoy-sidecar-grpc-web.md", "action": "MODIFY", "note": "T30: :64 + :260 template r[impl/verify] dash.envoy.grpc-web-translate → +2 (P0326 bumped marker)"},
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T31: delete blank lines :30 :34 :38 (tracey parse fix, work bottom-up). P0326 fixed :26-27; siblings remain broken."},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T32: +r[sched.classify.proactive-ema] marker after :211; amend :209 penalty-overwrite text + tracey bump to +2"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T33: :65-66 stale mint_session_jwt doc; T34: :885/:973/:995 future-tense P0260 refs → present + :995 correction (VM test fallback-only) — all p260 worktree refs"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T35: :47-52 + :414-420 ctrl.probe.named-service conflation + main.rs line-ref update (p292 refs)"},
  {"path": "rio-proto/src/client/balance.rs", "action": "MODIFY", "note": "T36: +r[impl ctrl.probe.named-service] at :309 (SCHEDULER_HEALTH_SERVICE const) or :88 (probe fn)"},
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "T37: +cold-cache cost note at :27 (p300 ref)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T38: +branch-deletion-survivable comment at nix-stable input :26 (p300 ref)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T39: +sub-markers note in r[ctrl.pool.ephemeral] paragraph (p296 ref — location arrives with P0296)"},
  {"path": "migrations/018_chunk_tenants.sql", "action": "MODIFY", "note": "T40: :15-16 same-txn claim → autocommit honest note (coordinate with P0350-T2 which edits :17-18 adjacent)"},
  {"path": ".claude/agents/rio-impl-validator.md", "action": "MODIFY", "note": "T41: :48 phantom_amend 'no validation-relaunch needed' → 'coordinator relaunches' clarification"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T42: :19 blockquote — 'SIGHUP scheduled at P0260'→'SIGHUP-reloadable wired (P0349)' (post-P0349-merge)"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T43: :1582 assert msg 'pre-P0260 deploy'→'key-unset deploy' (post-P0349-merge)"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T44: :1033-1043 spawn_grpc_server_layered docstring +ResBody param explanation (post-P0351-merge)"},
  {"path": ".claude/work/plan-0358-phantom-amend-stacked-detection.md", "action": "MODIFY", "note": "T46: :31-33 T1 prose align with DESIGN-NOTE approach-2 (orphan-base not diff-subset); :51 P0304-T64/T65 coordination note"},
  {"path": ".claude/work/plan-0352-putpathbatch-hoist-signer-lookup.md", "action": "MODIFY", "note": "T47: line-cite drift check :326/:67-70/:313/:270-345 post-P0345 — anchor by fn-name or update"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T48: T67 retarget :160(OBE)/:363→:316/:389→:342 post-P0345; T49: T62 +6 drift re-verify post-P0345"},
  {"path": ".claude/work/plan-0295-doc-rot-batch-sweep.md", "action": "MODIFY", "note": "T50: strike T40 OBE (P0353 landed, .sql commentary stripped) — intent survives as P0304-T85"},
  {"path": ".claude/work/plan-0345-put-path-validate-metadata-helper.md", "action": "MODIFY", "note": "T51 (low-value, DONE-plan): soft_deps 0344→344 leading-zero; doubled-word grep+fix"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T52: Histogram Buckets table :204 +build_graph_edges row (P0321 added to HISTOGRAM_BUCKET_MAP, spec table lagged)"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "T52: :268 doc-comment observability.md:119→:120 OR drop line# (off-by-one cite drift)"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T53: :441-445 main.rs:676 load_jwt_pubkey.await? → load_and_wire_jwt? (fn-name-anchor, post-P0355 triple-stale)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T54: maybeMissing ./.sqlx comment :210-212 reword 'fresh clone before prepare' → 'resilience against accidental deletion' (post-P0297)"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T55: :856 sign_for_tenant → resolve_once in comment (post-P0352)"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T55: :329 sign_for_tenant → resolve_once in comment (post-P0352)"},
  {"path": ".claude/work/plan-0352-putpathbatch-hoist-signer-lookup.md", "action": "MODIFY", "note": "T56: :139 erratum — admin.rs doesn't call sign_for_tenant (uses cluster() for ResignPaths); dead-code tracked at P0304-T91"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T57: Worker Metrics table +2 rows (upload_skipped_idempotent_total, fuse_circuit_open) — described+emitted but table lagged"},
  {"path": "rio-worker/tests/metrics_registered.rs", "action": "MODIFY", "note": "T57: WORKER_METRICS const +upload_skipped_idempotent_total (fuse_circuit_open already present at :24)"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T58: :218 r[sched.rebalancer.sita-e] — 'config-driven' honest-about-defaults OR forward-pointer to P0304-T95 (CONDITIONAL on dispatch order)"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "T59: :93-94 'List only' → DisruptionTarget-watcher + finalizer both use pods perm (post-P0285)"},
  {"path": ".claude/work/plan-0364-netpol-f541-fstring-fix.md", "action": "MODIFY", "note": "T60: :33 F541 per-segment clarification (plan-doc archaeology, P0364 UNIMPL — live dispatch guidance)"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T61: :466-471 'not wired here' → vmtest-full-nonpriv reference (CONDITIONAL on P0360 merge)"},
  {"path": "infra/helm/rio-build/templates/dashboard-gateway.yaml", "action": "MODIFY", "note": "T62: :3 r[impl dash.envoy.grpc-web-translate] → +2 (version bump parity with dashboard.md:26)"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T63: :268 r[sched.trace.assignment-traceparent] — commit parenting-vs-link observation, remove 'observes which' hedge"},
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T63: :340-354 observe-only print() → assert (regression guard matching observed result)"},
  {"path": "rio-proto/src/interceptor.rs", "action": "MODIFY", "note": "T64: :145 prose r[obs.trace.scheduler-id-in-metadata] → backtick-fenced (avoid tracey misparse)"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "T64: :621 prose r[sched.backstop.timeout] → backtick-fenced"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "T64: :32 prose r[sched.timeout.per-build] → backtick-fenced"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "T64: :312 prose r[sched.timeout.per-build] → backtick-fenced"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T64: :106 prose r[sched.timeout.per-build] → backtick-fenced (+ sibling refs in same file if present)"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T65: :485 0x80 grep comment — tighten to frame-prefix (partner to P0304-T43)"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T66: +r[sched.admin.sizeclass-status] marker after :143 create-tenant (parity with sibling admin markers)"},
  {"path": ".config/tracey/config.styx", "action": "MODIFY", "note": "T67: +rio-dashboard Svelte/TS globs to include + test_include (:42-61 region)"},
  {"path": ".claude/work/plan-0277-dashboard-app-shell-clusterstatus.md", "action": "MODIFY", "note": "T68: :121 files-fence note createConnectTransport → createGrpcWebTransport (match :18 body)"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T69: :196 envoy-gateway-system upstream — templated env var OR cross-ref comment to networkpolicy.yaml:216"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T69-A: :216 → {{ .Values.envoyGateway.namespace }}"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T69-A: +envoyGateway.namespace: envoy-gateway-system"},
  {"path": ".claude/work/plan-0289-port-specd-unlanded-test-trio.md", "action": "MODIFY", "note": "T70: :8 table cell lifecycle.nix → scheduling.nix (erratum at :17-23 corrected body but not table)"},
  {"path": ".claude/work/plan-0250-ca-detect-plumb-is-ca.md", "action": "MODIFY", "note": "T71: :11,:19,:41 DerivationInfo → DerivationNode (no such proto msg as DerivationInfo)"},
  {"path": ".claude/work/plan-0245-prologue-phase5-markers-gt-verify.md", "action": "MODIFY", "note": "T71: :35 DerivationInfo → DerivationNode"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T72: :303+ Alerting section +bloom_fill_ratio >= 0.5 entry with CRD-knob remediation link"},
  {"path": ".claude/work/plan-0291-is-cutoff-migration-drop.md", "action": "MODIFY", "note": "T73: reserved-field clarity note (may be no-op if already clear)"},
  {"path": "rio-dashboard/src/pages/GC.svelte", "action": "MODIFY", "note": "T74: :75-84 ratio comment 'progress'→'garbage rate' OR use scanned/total if GCProgress has total (post-P0281)"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T75: :110,:500 TODO(P0311)→WONTFIX(P0310) — P0311 is test-gap batch, doesn't own set-options-map-only"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T75: :547 TODO(P0311)→WONTFIX(P0310)"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "T75: :122 TODO(P0311)→WONTFIX(P0310) — align with :111 existing WONTFIX(P0310)"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T77: :12 r[impl proto.stream.bidi] — delete (stranded post-P0356 split)"},
  {"path": "rio-scheduler/src/grpc/worker_service.rs", "action": "MODIFY", "note": "T77: +r[impl proto.stream.bidi] near build_execution impl (~:75 or fn signature)"},
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "MODIFY", "note": "T78: :36 'until P0271 adds a GetBuild RPC' → 'until a GetBuild(id) RPC lands (no owner plan)' — P0271 is cursor pagination (post-P0278)"},
  {"path": "docs/src/decisions.md", "action": "MODIFY", "note": "T79: +ADR-018 row (018-ca-resolution.md exists, table stops at ADR-015; check 016/017 at dispatch)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T80: delete :142-143 duplicate r[ctrl.pool.bloom-knob] (keep :119; 3-agent consensus)"},
  {"path": "rio-controller/src/lib.rs", "action": "MODIFY", "note": "T81: :78 describe_counter error_kind enumeration +|conflict"},
  {"path": "rio-controller/src/error.rs", "action": "MODIFY", "note": "T81: :52 '4 values' → '5 values' (Conflict variant added by P0372)"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T82: :86 dag::DerivationNode-or-dag::DerivationNode typo → types:: vs dag:: dual-path (post-P0376)"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "T83: :102-106 /finalizers subresource comment → OwnerReferencesPermissionEnforcement explanation (post-P0235)"},
  {"path": ".config/tracey/config.styx", "action": "MODIFY", "note": "T84: impls.include +infra/helm/**/*.yaml IF tracey parses yaml #-comments (same check-at-dispatch as T67 Svelte); OR create rio-test-support anchor file"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T85: envoyImage key — forward cross-ref comment to docker-pulled.nix:94 + P0304-T136 lockstep-assert (post-P0379)"},
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T85: :94 envoy-distroless entry — reciprocal cross-ref to values.yaml envoyImage"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T86: :415-421 tokio::join! comment — extend with Ok-vs-panic exit semantics (comment-only, no behavior)"},
  {"path": "docs/src/remediations/phase4a/11-tonic-health-drain.md", "action": "MODIFY", "note": "T87: :55,:389,:394-395,:612,:686,:690 admin/mod.rs:562-587→admin/gc.rs retarget (post-P0383 split; also sweep other remediation docs for admin/mod.rs refs)"},
  {"path": "docs/src/crate-structure.md", "action": "MODIFY", "note": "T88: :3 count 9→12; :6-17 tree +rio-cli/rio-crds/rio-bench; :21 drop rio-cli from Not-yet; :26-68 mermaid +edges; :84-94 rio-common 8→15 modules; :126-139 rio-proto 5→8 proto + client/-dir; +rio-crds +rio-cli sections (consol-mc190 28-refactor-commits-zero-doc-updates drift)"},
  {"path": ".config/tracey/config.styx", "action": "MODIFY", "note": "T89: +Builds.svelte+Cluster.svelte explicit filenames OR **/*.svelte+**/*.ts glob + __tests__ exclude (extends T67 with rev-p279 specifics; post-P0279-merge)"},
  {"path": ".claude/work/plan-0299-staggered-scheduling-cold-start.md", "action": "MODIFY", "note": "T90: :161 EC +not-met-as-written blockquote → P0311-T58 forward-ref"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T91: Scheduler table +rio_scheduler_ca_hash_compares_total row (labels match|miss|error|malformed|skipped_after_miss); Gateway table +rio_gateway_jwt_mint_degraded_total row"},
  {"path": ".claude/work/plan-0271-cursor-pagination-admin-builds.md", "action": "MODIFY", "note": "T93: :16 T1 erratum blockquote (types.proto→admin_types.proto post-P0376); :94 files-fence path correction. Class-(E) archaeology. discovered_from=271"},
  {"path": ".claude/notes/tracey-svelte-yaml-limitation.md", "action": "MODIFY", "note": "T94: extend Options-(b) with FILED-UPSTREAM FR tracking + 4-rule uncovered-noise table (T67/T84 follow-on; upstream feature request, not rio-build code). discovered_from=295"},
  {"path": ".claude/work/plan-0287-trace-linkage-submitbuild-metadata.md", "action": "MODIFY", "note": "T95: :117,:215 grpc/tests.rs→grpc/tests/submit_tests.rs retarget (class-E post-P0395). discovered_from=395"},
  {"path": ".claude/work/plan-0311-test-gap-batch-cli-recovery-dash.md", "action": "MODIFY", "note": "T95: T22 grpc/tests.rs test-add-location → submit_tests.rs or stream_tests.rs (post-P0395 soft_deps note :140 already flags). discovered_from=395"},
  {"path": ".claude/work/plan-0387-airgap-bare-tag-derive-from-destnametag.md", "action": "MODIFY", "note": "T96: plan-T1 stale ref — :7 line-refs drift OR :104 cross-ref dangles post-T3 (check at dispatch). discovered_from=387"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T96: :104 'same pattern as vmtest-full-nonpriv.yaml:33' — points at deleted line post-P0387-T3; update to 'extraValues at default.nix:349'. discovered_from=387. Post-P0387-merge"},
  {"path": "rio-dashboard/src/components/__tests__/LogViewer.test.ts", "action": "MODIFY", "note": "T97: :7 'These three tests' → code-shape phrasing (no count, no :63-75 line-ref). discovered_from=392. Post-P0392-merge"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "MODIFY", "note": "T98: :1157-1161 delete stale paths_each=5 mid-thought + tighten 'Actually simpler' prose. discovered_from=391. Post-P0391-merge"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T99: :196-201 populate_* docstrings — restore ~30L design-rationale (sequential-not-concurrent, NotFound-contributes-0-safe, InputNotFound-BFS-hard). discovered_from=413"},
  {"path": ".claude/work/plan-0413-translate-populate-walker-dedup.md", "action": "MODIFY", "note": "T100: :10 erratum — P0408 is scheduler-side not gateway; iter_cached_drvs crate-private; actual consumers=2. discovered_from=413"},
  {"path": ".claude/work/plan-0370-extract-spawn-periodic-helper.md", "action": "MODIFY", "note": "T101: 10× stale admin/mod.rs+scaling.rs refs → admin/gc.rs+scaling/standalone.rs (class-E post-split). discovered_from=370"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T102: :157 ChunkServiceClient re-export — route-(a) add TODO(P0NNN) tag OR route-(b) delete dead pub-use. discovered_from=sprint-1-cleanup"},
  {"path": ".claude/work/plan-0316-qemu-force-accel-kvm.md", "action": "MODIFY", "note": "T103: :1 closure header (7bd70aba disabled, 23519879 infra-fixed). discovered_from=coordinator"},
  {"path": ".claude/work/plan-0306-onibus-merge-3dot-lock-lease-planner-isolation.md", "action": "MODIFY", "note": "T104: :255 per-RUN→per-DOC invariant correction. discovered_from=306"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T105: :387 MAX_FRAMED_TOTAL MUST-equal → MUST-be-≥. discovered_from=440"},
  {"path": ".claude/work/plan-0434-manifest-mode-upload-bandwidth-opt.md", "action": "MODIFY", "note": "T106: :58 soft-dep note update (P0430 route-a refutes). discovered_from=430"},
  {"path": "rio-scheduler/src/actor/tests/lifecycle_sweep.rs", "action": "MODIFY", "note": "T107: :242,:326 r[verify sched.ca.cutoff-propagate] → correct marker or new rule. discovered_from=442"},
  {"path": "rio-store/src/manifest.rs", "action": "MODIFY", "note": "T108: :147-152 Manifest::total_size doc-comment — test-only or restore cfg(test). discovered_from=429"},
  {"path": "rio-builder/src/lib.rs", "action": "MODIFY", "note": "T109: :47 Worker Metrics → Builder Metrics section name. discovered_from=455"},
  {"path": "docs/src/decisions/005-builder-store-model.md", "action": "MODIFY", "note": "T110: :10,16 rio-worker/ → rio-builder/ (3 sites). discovered_from=455"},
  {"path": "docs/src/decisions/012-privileged-builder-pods.md", "action": "MODIFY", "note": "T111: :42 rio-worker/src/cgroup.rs → rio-builder/. discovered_from=455"},
  {"path": "docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md", "action": "MODIFY", "note": "T112: :792 [worker.md] link text → [builder.md]. discovered_from=455"},
  {"path": "docs/src/remediations/phase4a/15-shutdown-signal-cluster.md", "action": "MODIFY", "note": "T112: :527 [worker.md] link text → [builder.md]. discovered_from=455"},
  {"path": "docs/src/remediations/phase4a/02-empty-references-nar-scanner.md", "action": "MODIFY", "note": "T112: :1238 [worker.md] link text → [builder.md]. discovered_from=455"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T113: :82 Scheduler Metrics table +2 rows (fod_queue_depth, fetcher_utilization); :166 blockquote → past tense. discovered_from=452"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T114: :137 Store Metrics table +3 rows (substitute_total/_bytes_total/_duration_seconds). discovered_from=462"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T115: :206 Fastly CIDR example — multi-CIDR + blast-radius note. discovered_from=463"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T116: add r[store.tenant.filter-scope] marker near :195. discovered_from=465"},
  {"path": ".claude/lib/onibus/merge.py", "action": "MODIFY", "note": "T487: _extract_new_test_names docstring Rust-only qualifier :556. discovered_from=479"},
  {"path": "rio-builder/src/main.rs", "action": "MODIFY", "note": "T488: audit comment READ_ONLY_ROOT_MOUNTS table ref :172-174. discovered_from=467"},
  {"path": "rio-store/src/substitute.rs", "action": "MODIFY", "note": "T489: drop phantom store.toml config claim :43-44 + :177-178. discovered_from=483"},
  {"path": ".claude/work/plan-0484-merger-cov-smoke-ci.md", "action": "MODIFY", "note": "T490: T2 cadence→per-merge wording fix :29. discovered_from=484"},
  {"path": ".claude/lib/onibus/cli.py", "action": "MODIFY", "note": "T491: clause4-check comment || exit → jq-parses at :345-346. discovered_from=488"},
  {"path": ".claude/lib/test_scripts.py", "action": "MODIFY", "note": "T492: :1506 realistic→plausible — distinguishes plan-prescribed from corpus-verified. Adjacent to P0508 T2's insertions at :1508. discovered_from=447"},
  {"path": ".claude/work/plan-0508-infra-error-re-ssh-killed.md", "action": "MODIFY", "note": "T493: :1 deviation header — Killed\\s*$ prescribed, exit-137 implemented on corpus evidence. discovered_from=508"},
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T494: :724-726 doc-comment false deterministic claim — iterates active_jobs Vec not BTreeMap. Route-a sort by creation_timestamp OR route-b drop claim. discovered_from=505"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T495: :154 r[ctrl.pool.manifest-scaledown] append controller-restart-resets-clocks sentence. No tracey bump. discovered_from=505"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T991618106: :231 r[store.substitute.tenant-sig-visibility] add cluster-key to trusted-set union. tracey bump → +2. discovered_from=478"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T991618106: bump r[impl store.substitute.tenant-sig-visibility] → +2 (wherever the annotation lives near :534). discovered_from=478"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T991618106: bump r[verify store.substitute.tenant-sig-visibility] :769 → +2. discovered_from=478"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T991618107: strip r[...] marker tokens from prose section headers :2454,:2481,:2498 — keep bare rule-id. discovered_from=512"},
  {"path": "nix/tests/scenarios/dashboard-gateway.nix", "action": "MODIFY", "note": "T991618107: strip r[...] marker token from prose section header :310. Pre-existing site. discovered_from=512"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T991618107 CONDITIONAL: add r[verify ctrl.pool.manifest-labels/-long-lived/-reconcile] at subtests wiring IF missing (structural coupling). discovered_from=512"}
]
```

**Root-level file (outside Files-fence validator pattern):** `README.md` MODIFY at repo root — T1 dead `.#ci-fast` attrs → single `.#ci` block. T45: `CLAUDE.md` MODIFY at repo root — `:195-197` TODO(P0286) example → generic (post-P0286-merge). T76: `CLAUDE.md` MODIFY at repo root — add WONTFIX(P0NNN) convention after `:211` TODO-format block. Zero collision risk on all.

```
README.md                          # T1 (root-level)
flake.nix                          # T2
nix/coverage.nix                   # T2
docs/src/remediations/phase4a.md   # T3
rio-worker/src/synth_db.rs         # T4
rio-store/src/cache_server/auth.rs # T5
docs/src/security.md               # T5
.claude/work/plan-00{05,25,76,113}*.md  # T6-T9
.claude/work/plan-0222*.md         # T10
```

## Dependencies

```json deps
{"deps": [204, 222, 294, 316, 306, 326, 266, 260, 292, 296, 300, 264, 346, 349, 351, 286, 321, 357, 355, 297, 352, 363, 230, 285, 287, 273, 277, 282, 288, 361, 248, 281, 310, 356, 278, 375, 372, 376, 235, 371, 379, 383, 381, 279, 299, 251, 367, 395, 387, 392, 391, 440, 430, 442, 429, 455, 452, 488, 478, 512], "soft_deps": [215, 218, 243, 289, 206, 313, 317, 304, 347, 348, 350, 356, 358, 345, 363, 360, 364, 231, 291, 375, 271, 374, 283, 386, 394, 393, 311, 426], "note": "retro \u00a7Doc-rot (T1-T10) + sprint-1 sink (T11-T29) + sprint-1 sink-2 (T30-T32) + sprint-1 sink-3 (T33-T39) + plan-doc-errata sink (T47-T51). T11-T15 depend on P0294 (Build CRD rip \u2014 landmarks must be gone before we reference their absence). T16-T18 depend on P0215 finding (ssh-ng wopSetOptions). T17/T18 CROSS-WORKTREE with p243 \u2014 fix before P0243 merges or fold into P0243 fix-impl. T14 coordinates with P0289 (same file, leave TODO). T21 discovered_from=206 (digest() pgcrypto spelling). T22 discovered_from=313 (wrong onibus subcmd). T23 docs-916455 QA nits. T24 fixes T23's self-defeating criterion. T25-T26 depend on P0316 (DONE \u2014 pre-pivot -accel text; discovered_from=316). T25 soft-dep P0317 (T4 mitigation-migration supersedes the -accel fix; T25 becomes conditional). T27 discovered_from=209 (merger misinterpretation \u2014 exit-1 vs exit-77). T28 depends on P0306 (DONE \u2014 rio-planner.md :127 prose references P0306 T3's fix; discovered_from=306). T29 soft-dep P0317 (CONDITIONAL \u2014 in-flight implementer has fixes in prompt; belt-and-suspenders doc trail). T30+T31 depend on P0326 (DONE \u2014 marker bump to +2 + dashboard.md :26-27 fix happened there; discovered_from=326). T32 depends on P0266 (UNIMPL \u2014 proactive-ema code at db.rs:1300 arrives with it; the marker ADD happens here, the r[impl] annotation lands with P0266; discovered_from=266). T31+T32 soft-conflict P0304-T27 (both do tracey blank-line fixes in component specs \u2014 T31=dashboard.md, T27=scheduler.md; non-overlapping files). T32 soft-conflict P0304-T25 (both edit scheduler.md :200-211 region \u2014 T25 edits :449-451, T32 edits :209+post-:211; non-overlapping hunks). T33+T34 depend on P0260 (UNIMPL \u2014 server.rs mint_session_jwt doc + jwt_issuance_tests comments arrive; discovered_from=260). T35 depends on P0292 (DONE \u2014 main.rs fix landed, lifecycle.nix prose still has same conflation; discovered_from=292). T36 depends on P0292 (same \u2014 spec text shifted to client-side; discovered_from=292). T37+T38 depend on P0300 (UNIMPL \u2014 verification.md section + flake.nix nix-stable input arrive; discovered_from=300). T37 soft-conflicts P0348 (both edit verification.md \u2014 T37 at :27 cost note, P0348 at :18 bump note; non-overlapping lines). T39 depends on P0296 (UNIMPL \u2014 r[ctrl.pool.ephemeral] marker arrives; discovered_from=296); soft-dep P0347 (the sub-markers T39 points at). T40 depends on P0264 (DONE \u2014 migration 018 exists; discovered_from=bughunter-mc91). T40 adjacent-lines with P0350-T2 (both edit 018_chunk_tenants.sql comments: T40=:15-16 same-txn, P0350-T2=:17-18 CASCADE) \u2014 coordinate at dispatch or apply both in one commit. T41 depends on P0346 (DONE \u2014 phantom_amend prose at :48 exists; discovered_from=346). T42+T43 depend on P0349 (UNIMPL \u2014 spawn_pubkey_reload wiring lands; scrub commit a294380e missed multi-tenancy.md + grpc/tests.rs:1582; discovered_from=349). T43 soft-conflict P0356 (grpc/mod.rs split \u2014 tests.rs may see import churn; T43 is a :1582 assert-message edit, low collision risk). T44 depends on P0351 (DONE \u2014 spawn_grpc_server_layered<L, ResBody> exists; discovered_from=351). T44 is pure docstring add at :1033-1043, no behavior change. T45 depends on P0286 (UNIMPL \u2014 the CLAUDE.md TODO(P0286) example becomes stale once P0286 T6 deletes the real cgroup.rs TODO it mirrors; discovered_from=286-review). T46 no hard-dep (plan doc errata \u2014 P0358 was assigned from placeholder P996659202; contradiction between :31 T1 'already works' and :198 DESIGN-NOTE 'doesn't generalize cleanly'; soft_dep 358 \u2014 coordinate with its implementer). T47-T51 soft-dep 352/304/345 (plan-doc errata \u2014 P0345+P0353 merged DURING planning; items retargeted from plan-doc-errata to DONE-plan-archaeology or prod-code followups). T47 discovered_from=352-review (line-cite drift post-P0345 extraction). T48+T49 discovered_from=345-review (P0304-T67 targets shifted :160-extracted :363\u2192:316 :389\u2192:342; T62 drift re-verify). T50 discovered_from=353-review (P0353 landed at a503e7d4 \u2014 T40 target .sql lines gone; strike OBE; substantive correction moved to P0304-T85 M_018 'BEGIN..COMMIT' claim wrong vs chunk.rs:262). T51 discovered_from=345-review (low-value DONE-archaeology \u2014 leading-zero + doubled-word; apply only on sweep pass). THREE prior items (P0353 ::-path, CLAUDE.md snippet, apply_trailer vestigial) OBE or moved to P0304-T84/T85 after impl landed cleaner than plan draft. T47-T51 are plan-doc-only; zero code touch; clause-4(a) `.claude/work/`-only candidate. Mostly no behavior change \u2014 docs/comments/plan-doc errata. T32 is the exception: adds a spec marker + bumps another. T36 adds a tracey annotation (second r[impl] site). T52 depends on P0321 (DONE \u2014 HISTOGRAM_BUCKET_MAP :310 entry exists, Histogram Buckets spec table lagged; discovered_from=321). T52 soft-dep P0363 (adds adjacent upload_references_count row to same table \u2014 both pure row-insert, rebase-clean either order). T53 depends on P0357 (DONE \u2014 jwt-mount-present subtest comment exists at lifecycle.nix:441) + P0355 (DONE \u2014 load_and_wire_jwt extraction shifted main.rs line refs; discovered_from=357). T54 depends on P0297 (UNIMPL \u2014 maybeMissing ./.sqlx fileset entry + comment arrive with it; discovered_from=297). T55+T56 depend on P0352 (DONE \u2014 resolve_once+sign_with_resolved exist; sign_for_tenant has zero prod callers; plan-doc T3 admin.rs claim false \u2014 admin.rs uses cluster() not sign_for_tenant; discovered_from=352-review). T55 soft-dep P0304-T91 (if T91 deletes sign_for_tenant entirely, these refs become dangling-to-deleted vs stale-but-exists; either way reword). T57 depends on P0363 (DONE or merging \u2014 upload_skipped_idempotent_total + fuse_circuit_open both in describe_metrics + emit sites; obs.md table lagged; discovered_from=363-review). T57 same file as T52 (observability.md) \u2014 T52 inserts at Histogram Buckets table :204, T57 inserts at Worker Metrics table :156-173; different sections, rebase-clean. T58 depends on P0230 (DONE \u2014 RebalancerConfig + spawn_task exist; discovered_from=230-review). T58 CONDITIONAL on dispatch order with P0304-T95 (code-side fix adds Deserialize + threads config): if T95 lands first, spec becomes accurate \u2192 T58 no-op; if T58 lands first, use Scheduled-blockquote forward-pointer. scheduler.md :218 is r[sched.rebalancer.sita-e] spec text \u2014 no tracey bump (config-surface detail, not behavior change). T59 depends on P0285 (DONE \u2014 disruption.rs watcher exists; rbac.yaml:93 comment predates it; discovered_from=285-review). T60 soft-dep P0364 (UNIMPL \u2014 plan-0364 dispatch scan at :42 is live guidance; T60 is archaeology-tier but useful since P0364 hasn't dispatched; discovered_from=364-review). T61 soft-dep P0360 (UNIMPL \u2014 comment is ACCURATE today, becomes a lie once P0360 lands; CONDITIONAL fix: skip if P0360 still UNIMPL at dispatch; discovered_from=360-review). T61 same-file-non-overlapping with P0304-T100 (T100=:475 timeout bump, T61=:466-471 comment \u2014 adjacent but distinct hunks, rebase-clean). T62 depends on P0273 (DONE \u2014 dashboard-gateway.yaml exists; discovered_from=273-review). T62 same fix-class as T30 (plan-doc marker-version fix); T62 is the yaml-side partner (tracey doesn't scan yaml so it's documentary, but humans grep). T62 soft-conflict P0371-T1 (rewrites dashboard-gateway.yaml GRPCRoute \u2014 T62 edits :3 marker-comment, P0371-T1 edits :63-86; non-overlapping). T63 depends on P0287 (DONE \u2014 observability.nix T7 observe-only block landed; discovered_from=287-review). T63 is outcome-driven doc: read the VM log's CONFIRMED print, commit the answer to :268, replace print with assert. If vm-observability never KVM-ran (check kvm-pending.md), T63 is conditional on getting one KVM run first. T64 depends on P0287 (DONE \u2014 interceptor.rs:145 prose-ref arrived with it; discovered_from=287-review). T64 is same gotcha-class as P0229/P0289 doc-comment r[verify] misparse \u2014 rephrase r[...] prose to backtick-fenced. 5 sites across 5 files, all 1-line comment edits. T65 depends on P0273 (DONE \u2014 default.nix:485 comment arrived with P0273 T5; discovered_from=273-review). T65 is partner to P0304-T43 (tightens the grep pattern; T43 fixed the plan-doc, T65 fixes the default.nix prose). T66 soft-dep P0231 (UNIMPL \u2014 GetSizeClassStatus RPC arrives with it; discovered_from=231-review). T66 adds the spec marker so P0231's handler has an r[impl] target. Marker goes to scheduler.md:144+ alongside the sibling r[sched.admin.*] markers (list-workers/list-builds/clear-poison/list-tenants/create-tenant \u2014 all present, sizeclass-status missing). T66 can land BEFORE P0231 (marker-first discipline per tracey adoption). T67 depends on P0277 (DONE \u2014 Cluster.svelte + Cluster.test.ts with dash.* markers exist; discovered_from=277-review). T67 is tracey config \u2014 low-conflict, config.styx append to a glob list. T67 CARE: tracey may not parse Svelte <!-- --> comments or inline <script> // comments \u2014 check at dispatch; if unparseable, document in .claude/notes/ instead of silent-fail. T68 depends on P0277 (DONE \u2014 plan-0277 exists; discovered_from=277-review). T68 is pure plan-doc-archaeology (P0277 DONE, fence lags body). T69 depends on P0282 (DONE \u2014 docker.nix nginx upstream + networkpolicy.yaml exist; discovered_from=282-review). T69 is a 2-site hardcode \u2014 Option-A (values.yaml extraction) preferred but Option-B (cross-ref comment) is the minimal doc-rot fix. T70 depends on P0361 (DONE \u2014 P0361's dispatch surfaced the plan-0289 table/erratum mismatch; discovered_from=361-review). T70 is 1-cell table fix. T71 depends on P0248 (UNIMPL \u2014 P0248 is 'types.proto: is_ca field' \u2014 the review of its Entry criteria found the DerivationInfo typo in P0250/P0245; discovered_from=248-review). T71 is pure plan-doc s/DerivationInfo/DerivationNode/ (proto message name \u2014 grep confirmed no DerivationInfo exists). T72 depends on P0288 (DONE \u2014 bloom_fill_ratio gauge + :174 metric-table entry exist; Alerting section :303+ lagged; discovered_from=288-review). T72 soft-dep P0375 (CRD knob \u2014 T72's remediation text points at it; T72 can land before P0375 with a 'Scheduled' forward-pointer blockquote). T73 soft-dep P0291 (UNIMPL \u2014 the reserved-field note; discovered_from=291-review). T73 may be no-op \u2014 check if existing 'field 3 RESERVED' is already clear enough. T74 depends on P0281 (UNIMPL \u2014 GC.svelte arrives with it; discovered_from=281-review). T75 depends on P0310 (DONE \u2014 the WONTFIX(P0310) target + TODO(P0311) sites arrived with it; discovered_from=310-review). T76 no hard-dep (CLAUDE.md Deferred-work section exists since sprint-1; discovered_from=310-review+consol-mc160 \u2014 two cadence agents same finding). T77 depends on P0356 (DONE \u2014 the grpc/mod.rs \u2192 worker_service.rs split stranded the marker at :12; discovered_from=356-review). T78 depends on P0278 (UNIMPL \u2014 Builds.svelte :36 comment arrives with it; discovered_from=278-review); soft-dep P0271 (the WRONG cross-ref target \u2014 T78 corrects the reference, doesn't need P0271 to land). T79 no hard-dep (decisions.md + 018-ca-resolution.md both exist since phase-5 kickoff; discovered_from=consol-mc160). T77 touches grpc/mod.rs (count=34 post-split) \u2014 1-line annotation delete; worker_service.rs \u2014 1-line add near :75. Both annotation-moves, zero behavior change. T74 is a comment-fix OR a ratio-formula change \u2014 check types.proto GCProgress fields at dispatch; if totalPaths exists, the ratio fix is load-bearing not just a comment. T80 depends on P0375 (DONE \u2014 :119 bloom-knob marker arrived with it; :142 pre-existed from 25499380 batch-append; discovered_from=375-review primary + 374-review + 376-review \u2014 THREE-AGENT CONSENSUS). T80 is pure 2-line delete at controller.md:142-143; zero behavior change, tracey silently resolves first-occurrence so removal changes nothing functionally. T81 depends on P0372 (DONE \u2014 Error::Conflict variant + error_kind() wiring exist; discovered_from=372-review). T81 is 2-site doc-comment sync (lib.rs:78 describe_counter + error.rs:52 '4 values' comment). Operator-visible (Prometheus #HELP). T82 depends on P0376 (UNIMPL \u2014 lib.rs:86 dual-path doc-comment arrives with p376's domain re-export; discovered_from=376-review). T82 is 1-word typo fix. T83 depends on P0235 (UNIMPL \u2014 rbac.yaml:102-106 comment arrives with WPS RBAC regen; discovered_from=235-review). T83 is a comment rewrite \u2014 rule itself CORRECT, explanation wrong. T84 depends on P0371 (DONE \u2014 dashboard-gateway.yaml:3-4 r[impl] markers exist; discovered_from=371-review). T84 same check-at-dispatch as T67 (does tracey parse this comment syntax) \u2014 if YES, add glob (option-a); if NO, Rust anchor file (option-c). T84 config.styx is low-conflict; soft-conflicts P0304-T31 (adds 4 crate test-dirs to same file; non-overlapping globs). T31 re-flagged by rev-p283 (dashboard.md blank-line-after \u2014 line refs shifted :29/33/37 \u2192 ~:32-42 as docs grew; re-grep at dispatch). T67 re-flagged AGAIN by rev-p283 (3rd independent re-flag; Svelte/TS parser-support remains the blocking unknown). T85 depends on P0379 (UNIMPL \u2014 envoyImage digest-pin at values.yaml:404-407 arrives with it; discovered_from=379-review). T85 is T69's envoy-specific partner (T69 = namespace extraction; T85 = image-bump cross-ref comment). T85 references P0304-T136's lockstep-assert \u2014 soft-dep 304 already in list. Same values.yaml region as T69, non-overlapping keys. docker-pulled.nix low-traffic. T86 depends on P0235 (DONE \u2014 main.rs tokio::join! at :421 for wp+wps controllers exists post-WPS-wire; discovered_from=235-review). T86 is comment-extension only at :415-421; rio-controller/src/main.rs count=24, T86 is 5-line comment expansion inside existing comment block, zero behavior change. T86 same file as T15 (Build CRD stale comment at :27) \u2014 different sections (:27 vs :415-421), non-overlapping. rev-p235 RBAC-finalizers-subresource finding ALREADY COVERED by T83 (same finding, no new T). T87 depends on P0383 (DONE \u2014 admin/mod.rs\u2192{logs,gc,tenants}.rs split landed; rem-11's :562-587 TriggerGC refs now point at deleted range; discovered_from=383-review). T87 is 5-site line-ref retarget in remediation docs (archaeology-tier, no tracey impact). T87 soft-dep P0386 (admin/tests.rs split \u2014 rem-11:690 test ref becomes admin/tests/gc_tests.rs post-split; if P0386 dispatches first, T87 targets the new path). T88 depends on P0376+P0381 (DONE \u2014 proto domain split + scaling.rs fault-line split are the two biggest drift sources; both must be landed before T88's sweep has stable ground-truth; discovered_from=consol-mc190). T88 is the ONLY T in this batch that touches crate-structure.md \u2014 low-traffic doc (no collision-count entry). T88 OPTIONAL skeleton-template forward-pointer may fold into P0304-T1's skeleton sync (same-file, non-overlapping prose paragraph). T89 depends on P0279 (UNIMPL \u2014 config.styx :58 line arrives with it; the Builds.svelte+Cluster.svelte r[impl dash.journey.build-to-logs] markers already exist on sprint-1 but are invisible without config.styx include; discovered_from=279-review+coord). T89 EXTENDS T67 (same config.styx file, same add-dashboard-globs fix; T89 has specific filenames + the exclude-based alternative from row 16's fragility note). T89 CORRECTNESS-TIER: validator PARTIAL journey-marker-invisible is this. T90 depends on P0299 (DONE \u2014 plan-0299 exists with :161 EC; discovered_from=299). T90 is pure plan-doc blockquote-add (archaeology tier, P0299 DONE). T91 depends on P0251 (DONE \u2014 completion.rs CA compare hook emits rio_scheduler_ca_hash_compares_total; discovered_from=251-review) + P0367 (DONE \u2014 lib.rs:66 describes jwt_mint_degraded_total; discovered_from=367-review). T91 soft-coordinates P0394 (derived SPEC_METRICS picks these rows up automatically once landed \u2014 sequence T91 BEFORE P0394 preferred) + P0304-T148/T157 + P0393 (all add labels to ca_hash_compares_total \u2014 T91's row lists all labels). T95+T96 from rev-p395/rev-p387. T95 depends on P0395 (grpc/tests.rs \u2192 tests/{submit,stream,bridge,guards}_tests.rs split; discovered_from=395); class-(E) target-moved-by-split, same fix-shape as T87 (admin split) and T93 (proto split); 3 active-plan refs (plan-0287:117+215, T43 here, P0311-T22) retarget; DONE-plan archaeology (plan-00{66,71,73,74,77,78,92,93,94}+0145+0189+0190) left alone unless grep-needed. T95 self-edits THIS plan's T43 prose + files-fence :1198 to say grpc/tests/submit_tests.rs. T96 depends on P0387 (destNameTag derivation; discovered_from=387); most likely k3s-full.nix:104 comment 'same pattern as vmtest-full-nonpriv.yaml:33' dangles after P0387-T3 deletes :33; plan-doc erratum + comment-update. T97 depends on P0392 (UNIMPL \u2014 LogViewer.test.ts header comment arrives with it; discovered_from=392). T97 soft-dep P426 (adds more tests to same file \u2014 if P426 dispatches first, it may already clean the count). T98 depends on P0391 (UNIMPL \u2014 warm_gate_initial_hint_is_deterministic test + :1157 comment arrive with it; discovered_from=391). T109-T112 depend on P0455 (DONE \u2014 worker\u2192builder rename; these sweep the doc-prose misses). T113 depends on P0452 (DONE \u2014 FOD split emitters landed at f3d1eba0; table rows never added). T113 same file as T52+T57+T91 (observability.md) \u2014 different sections (T52=:204, T57=:156, T91=scheduler+gateway tables, T113=:82+:166), all additive row-inserts."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root. [P0294](plan-0294-build-crd-full-rip.md) — T11-T15 reference the CRD's absence; must land after the rip.
**Soft-deps:** [P0215](plan-0215-max-silent-time.md) (T16-T18 cite its finding), [P0218](plan-0218-nar-buffer-config.md) (T19 fixes its plan doc), [P0243](plan-0243-vm-fod-proxy-scenario.md) (T17/T18 cross-worktree), [P0289](plan-0289-port-specd-unlanded-test-trio.md) (T14 same file).
**Conflicts with:** [`security.md`](../../docs/src/security.md) also touched by [P0286](plan-0286-privileged-hardening-device-plugin.md) (spec additions — different section). [`gateway.md`](../../docs/src/components/gateway.md) count=21 — T16 edits `:45`; [P0310](plan-0310-gateway-client-option-propagation.md) T4 edits `:62` — non-overlapping. [`phase4a.md`](../../docs/src/remediations/phase4a.md) T3+T13 overlap — same file, coordinate. [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) T17 + [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) + [P0309](plan-0309-helm-template-fodproxyurl-workerpool.md) — three plans, non-overlapping sections (`:199`, `:285`, `:103`).
