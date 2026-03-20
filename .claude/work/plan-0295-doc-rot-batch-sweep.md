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

Both tasks spell the hash query as `digest($out, 'sha256')` — that's the **pgcrypto** extension function. `CREATE EXTENSION pgcrypto` appears nowhere in `migrations/`. The P0206 implementer correctly wrote `sha256(convert_to(...))` — the PG11+ builtin — at [`lifecycle.nix:1291`](../../nix/tests/scenarios/lifecycle.nix). The comment at `:1166` says "PG builtin" but doesn't say "NOT digest()". **Risk:** [P0207](plan-0207-tenant-key-build-gc-mark.md) implementer reading plan-0206 as reference may copy the `digest()` spelling and hit `ERROR: function digest(text, unknown) does not exist`.

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

### T40 — `docs:` migration-018 same-txn comment contradicts impl (autocommit)

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

## Tracey

**T32 adds one new marker:** `r[sched.classify.proactive-ema]` → [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) after `:211`. Proactive EMA updates from worker Progress reports are a distinct mechanism from penalty-overwrite (both update `ema_duration_secs`, different triggers/blending). P0266's db.rs gets the `r[impl]` annotation; its tests get `r[verify]`. See `## Spec additions` below for the marker text staging.

**T32 bumps one marker:** `r[sched.classify.penalty-overwrite]` text at `:209` amended to acknowledge mid-run proactive updates — `tracey bump` → `+2`. Existing `r[impl]` annotations become stale-to-review (the penalty-overwrite LOGIC didn't change; the "post-completion-only" claim did).

**T31 is tracey-mechanical:** three `r[dash.*]` markers already exist at [`dashboard.md:29/33/37`](../../docs/src/components/dashboard.md); the blank-line deletions make tracey parse their text. No bump — text didn't change, tracey's view of it did. Same fix-class as P0304-T27 for `r[sched.ca.*]`.

Remaining items are errata in plan docs, README, code comments. `r[sched.trace.assignment-traceparent]` is NOT touched here (that was P0160's item, carved out to P0293).

## Spec additions

**T32 — new `r[sched.classify.proactive-ema]`** (goes to [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) after `:211`, standalone paragraph, blank line before, col 0):

```
r[sched.classify.proactive-ema]
When a worker reports `memory_used_bytes > 0` in a `Progress` update, the scheduler proactively updates `ema_duration_secs` for the running derivation's `(pname, system)` key using the partial elapsed time. This gives the classifier fresher data for subsequent submissions of the same package even before the current build completes — useful for long-running builds where waiting for completion delays class-correction by hours. The proactive update uses standard EMA blending (not penalty-overwrite); it is recorded via `rio_scheduler_ema_proactive_updates_total`.
```

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
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "T43: :1582 assert msg 'pre-P0260 deploy'→'key-unset deploy' (post-P0349-merge)"}
]
```

**Root-level file (outside Files-fence validator pattern):** `README.md` MODIFY at repo root — T1 dead `.#ci-fast` attrs → single `.#ci` block. Zero collision risk.

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
{"deps": [204, 222, 294, 316, 306, 326, 266, 260, 292, 296, 300, 264, 346, 349], "soft_deps": [215, 218, 243, 289, 206, 313, 317, 304, 347, 348, 350, 996394104], "note": "retro §Doc-rot (T1-T10) + sprint-1 sink (T11-T29) + sprint-1 sink-2 (T30-T32) + sprint-1 sink-3 (T33-T39). T11-T15 depend on P0294 (Build CRD rip — landmarks must be gone before we reference their absence). T16-T18 depend on P0215 finding (ssh-ng wopSetOptions). T17/T18 CROSS-WORKTREE with p243 — fix before P0243 merges or fold into P0243 fix-impl. T14 coordinates with P0289 (same file, leave TODO). T21 discovered_from=206 (digest() pgcrypto spelling). T22 discovered_from=313 (wrong onibus subcmd). T23 docs-916455 QA nits. T24 fixes T23's self-defeating criterion. T25-T26 depend on P0316 (DONE — pre-pivot -accel text; discovered_from=316). T25 soft-dep P0317 (T4 mitigation-migration supersedes the -accel fix; T25 becomes conditional). T27 discovered_from=209 (merger misinterpretation — exit-1 vs exit-77). T28 depends on P0306 (DONE — rio-planner.md :127 prose references P0306 T3's fix; discovered_from=306). T29 soft-dep P0317 (CONDITIONAL — in-flight implementer has fixes in prompt; belt-and-suspenders doc trail). T30+T31 depend on P0326 (DONE — marker bump to +2 + dashboard.md :26-27 fix happened there; discovered_from=326). T32 depends on P0266 (UNIMPL — proactive-ema code at db.rs:1300 arrives with it; the marker ADD happens here, the r[impl] annotation lands with P0266; discovered_from=266). T31+T32 soft-conflict P0304-T27 (both do tracey blank-line fixes in component specs — T31=dashboard.md, T27=scheduler.md; non-overlapping files). T32 soft-conflict P0304-T25 (both edit scheduler.md :200-211 region — T25 edits :449-451, T32 edits :209+post-:211; non-overlapping hunks). T33+T34 depend on P0260 (UNIMPL — server.rs mint_session_jwt doc + jwt_issuance_tests comments arrive; discovered_from=260). T35 depends on P0292 (DONE — main.rs fix landed, lifecycle.nix prose still has same conflation; discovered_from=292). T36 depends on P0292 (same — spec text shifted to client-side; discovered_from=292). T37+T38 depend on P0300 (UNIMPL — verification.md section + flake.nix nix-stable input arrive; discovered_from=300). T37 soft-conflicts P0348 (both edit verification.md — T37 at :27 cost note, P0348 at :18 bump note; non-overlapping lines). T39 depends on P0296 (UNIMPL — r[ctrl.pool.ephemeral] marker arrives; discovered_from=296); soft-dep P0347 (the sub-markers T39 points at). T40 depends on P0264 (DONE — migration 018 exists; discovered_from=bughunter-mc91). T40 adjacent-lines with P0350-T2 (both edit 018_chunk_tenants.sql comments: T40=:15-16 same-txn, P0350-T2=:17-18 CASCADE) — coordinate at dispatch or apply both in one commit. T41 depends on P0346 (DONE — phantom_amend prose at :48 exists; discovered_from=346). T42+T43 depend on P0349 (UNIMPL — spawn_pubkey_reload wiring lands; scrub commit a294380e missed multi-tenancy.md + grpc/tests.rs:1582; discovered_from=349). T43 soft-conflict P996394104 (grpc/mod.rs split — tests.rs may see import churn; T43 is a :1582 assert-message edit, low collision risk). Mostly no behavior change — docs/comments/plan-doc errata. T32 is the exception: adds a spec marker + bumps another. T36 adds a tracey annotation (second r[impl] site)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root. [P0294](plan-0294-build-crd-full-rip.md) — T11-T15 reference the CRD's absence; must land after the rip.
**Soft-deps:** [P0215](plan-0215-max-silent-time.md) (T16-T18 cite its finding), [P0218](plan-0218-nar-buffer-config.md) (T19 fixes its plan doc), [P0243](plan-0243-vm-fod-proxy-scenario.md) (T17/T18 cross-worktree), [P0289](plan-0289-port-specd-unlanded-test-trio.md) (T14 same file).
**Conflicts with:** [`security.md`](../../docs/src/security.md) also touched by [P0286](plan-0286-privileged-hardening-device-plugin.md) (spec additions — different section). [`gateway.md`](../../docs/src/components/gateway.md) count=21 — T16 edits `:45`; [P0310](plan-0310-gateway-client-option-propagation.md) T4 edits `:62` — non-overlapping. [`phase4a.md`](../../docs/src/remediations/phase4a.md) T3+T13 overlap — same file, coordinate. [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) T17 + [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) + [P0309](plan-0309-helm-template-fodproxyurl-workerpool.md) — three plans, non-overlapping sections (`:199`, `:285`, `:103`).
