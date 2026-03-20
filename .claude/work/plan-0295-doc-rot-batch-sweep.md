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

discovered_from=321-review. **[P997443701](plan-997443701-upload-references-count-buckets.md)-T4 adds the adjacent `upload_references_count` row** to the same table — both are pure row-insert, rebase-clean either order.

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
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T66: +r[sched.admin.sizeclass-status] marker after :143 create-tenant (parity with sibling admin markers)"}
]
```

**Root-level file (outside Files-fence validator pattern):** `README.md` MODIFY at repo root — T1 dead `.#ci-fast` attrs → single `.#ci` block. T45: `CLAUDE.md` MODIFY at repo root — `:195-197` TODO(P0286) example → generic (post-P0286-merge). Zero collision risk on both.

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
{"deps": [204, 222, 294, 316, 306, 326, 266, 260, 292, 296, 300, 264, 346, 349, 351, 286, 321, 357, 355, 297, 352, 363, 230, 285, 287, 273], "soft_deps": [215, 218, 243, 289, 206, 313, 317, 304, 347, 348, 350, 996394104, 358, 345, 997443701, 360, 364, 231, 998869301], "note": "retro §Doc-rot (T1-T10) + sprint-1 sink (T11-T29) + sprint-1 sink-2 (T30-T32) + sprint-1 sink-3 (T33-T39) + plan-doc-errata sink (T47-T51). T11-T15 depend on P0294 (Build CRD rip — landmarks must be gone before we reference their absence). T16-T18 depend on P0215 finding (ssh-ng wopSetOptions). T17/T18 CROSS-WORKTREE with p243 — fix before P0243 merges or fold into P0243 fix-impl. T14 coordinates with P0289 (same file, leave TODO). T21 discovered_from=206 (digest() pgcrypto spelling). T22 discovered_from=313 (wrong onibus subcmd). T23 docs-916455 QA nits. T24 fixes T23's self-defeating criterion. T25-T26 depend on P0316 (DONE — pre-pivot -accel text; discovered_from=316). T25 soft-dep P0317 (T4 mitigation-migration supersedes the -accel fix; T25 becomes conditional). T27 discovered_from=209 (merger misinterpretation — exit-1 vs exit-77). T28 depends on P0306 (DONE — rio-planner.md :127 prose references P0306 T3's fix; discovered_from=306). T29 soft-dep P0317 (CONDITIONAL — in-flight implementer has fixes in prompt; belt-and-suspenders doc trail). T30+T31 depend on P0326 (DONE — marker bump to +2 + dashboard.md :26-27 fix happened there; discovered_from=326). T32 depends on P0266 (UNIMPL — proactive-ema code at db.rs:1300 arrives with it; the marker ADD happens here, the r[impl] annotation lands with P0266; discovered_from=266). T31+T32 soft-conflict P0304-T27 (both do tracey blank-line fixes in component specs — T31=dashboard.md, T27=scheduler.md; non-overlapping files). T32 soft-conflict P0304-T25 (both edit scheduler.md :200-211 region — T25 edits :449-451, T32 edits :209+post-:211; non-overlapping hunks). T33+T34 depend on P0260 (UNIMPL — server.rs mint_session_jwt doc + jwt_issuance_tests comments arrive; discovered_from=260). T35 depends on P0292 (DONE — main.rs fix landed, lifecycle.nix prose still has same conflation; discovered_from=292). T36 depends on P0292 (same — spec text shifted to client-side; discovered_from=292). T37+T38 depend on P0300 (UNIMPL — verification.md section + flake.nix nix-stable input arrive; discovered_from=300). T37 soft-conflicts P0348 (both edit verification.md — T37 at :27 cost note, P0348 at :18 bump note; non-overlapping lines). T39 depends on P0296 (UNIMPL — r[ctrl.pool.ephemeral] marker arrives; discovered_from=296); soft-dep P0347 (the sub-markers T39 points at). T40 depends on P0264 (DONE — migration 018 exists; discovered_from=bughunter-mc91). T40 adjacent-lines with P0350-T2 (both edit 018_chunk_tenants.sql comments: T40=:15-16 same-txn, P0350-T2=:17-18 CASCADE) — coordinate at dispatch or apply both in one commit. T41 depends on P0346 (DONE — phantom_amend prose at :48 exists; discovered_from=346). T42+T43 depend on P0349 (UNIMPL — spawn_pubkey_reload wiring lands; scrub commit a294380e missed multi-tenancy.md + grpc/tests.rs:1582; discovered_from=349). T43 soft-conflict P996394104 (grpc/mod.rs split — tests.rs may see import churn; T43 is a :1582 assert-message edit, low collision risk). T44 depends on P0351 (DONE — spawn_grpc_server_layered<L, ResBody> exists; discovered_from=351). T44 is pure docstring add at :1033-1043, no behavior change. T45 depends on P0286 (UNIMPL — the CLAUDE.md TODO(P0286) example becomes stale once P0286 T6 deletes the real cgroup.rs TODO it mirrors; discovered_from=286-review). T46 no hard-dep (plan doc errata — P0358 was assigned from placeholder P996659202; contradiction between :31 T1 'already works' and :198 DESIGN-NOTE 'doesn't generalize cleanly'; soft_dep 358 — coordinate with its implementer). T47-T51 soft-dep 352/304/345 (plan-doc errata — P0345+P0353 merged DURING planning; items retargeted from plan-doc-errata to DONE-plan-archaeology or prod-code followups). T47 discovered_from=352-review (line-cite drift post-P0345 extraction). T48+T49 discovered_from=345-review (P0304-T67 targets shifted :160-extracted :363→:316 :389→:342; T62 drift re-verify). T50 discovered_from=353-review (P0353 landed at a503e7d4 — T40 target .sql lines gone; strike OBE; substantive correction moved to P0304-T85 M_018 'BEGIN..COMMIT' claim wrong vs chunk.rs:262). T51 discovered_from=345-review (low-value DONE-archaeology — leading-zero + doubled-word; apply only on sweep pass). THREE prior items (P0353 ::-path, CLAUDE.md snippet, apply_trailer vestigial) OBE or moved to P0304-T84/T85 after impl landed cleaner than plan draft. T47-T51 are plan-doc-only; zero code touch; clause-4(a) `.claude/work/`-only candidate. Mostly no behavior change — docs/comments/plan-doc errata. T32 is the exception: adds a spec marker + bumps another. T36 adds a tracey annotation (second r[impl] site). T52 depends on P0321 (DONE — HISTOGRAM_BUCKET_MAP :310 entry exists, Histogram Buckets spec table lagged; discovered_from=321). T52 soft-dep P997443701 (adds adjacent upload_references_count row to same table — both pure row-insert, rebase-clean either order). T53 depends on P0357 (DONE — jwt-mount-present subtest comment exists at lifecycle.nix:441) + P0355 (DONE — load_and_wire_jwt extraction shifted main.rs line refs; discovered_from=357). T54 depends on P0297 (UNIMPL — maybeMissing ./.sqlx fileset entry + comment arrive with it; discovered_from=297). T55+T56 depend on P0352 (DONE — resolve_once+sign_with_resolved exist; sign_for_tenant has zero prod callers; plan-doc T3 admin.rs claim false — admin.rs uses cluster() not sign_for_tenant; discovered_from=352-review). T55 soft-dep P0304-T91 (if T91 deletes sign_for_tenant entirely, these refs become dangling-to-deleted vs stale-but-exists; either way reword). T57 depends on P0363 (DONE or merging — upload_skipped_idempotent_total + fuse_circuit_open both in describe_metrics + emit sites; obs.md table lagged; discovered_from=363-review). T57 same file as T52 (observability.md) — T52 inserts at Histogram Buckets table :204, T57 inserts at Worker Metrics table :156-173; different sections, rebase-clean. T58 depends on P0230 (DONE — RebalancerConfig + spawn_task exist; discovered_from=230-review). T58 CONDITIONAL on dispatch order with P0304-T95 (code-side fix adds Deserialize + threads config): if T95 lands first, spec becomes accurate → T58 no-op; if T58 lands first, use Scheduled-blockquote forward-pointer. scheduler.md :218 is r[sched.rebalancer.sita-e] spec text — no tracey bump (config-surface detail, not behavior change). T59 depends on P0285 (DONE — disruption.rs watcher exists; rbac.yaml:93 comment predates it; discovered_from=285-review). T60 soft-dep P0364 (UNIMPL — plan-0364 dispatch scan at :42 is live guidance; T60 is archaeology-tier but useful since P0364 hasn't dispatched; discovered_from=364-review). T61 soft-dep P0360 (UNIMPL — comment is ACCURATE today, becomes a lie once P0360 lands; CONDITIONAL fix: skip if P0360 still UNIMPL at dispatch; discovered_from=360-review). T61 same-file-non-overlapping with P0304-T100 (T100=:475 timeout bump, T61=:466-471 comment — adjacent but distinct hunks, rebase-clean). T62 depends on P0273 (DONE — dashboard-gateway.yaml exists; discovered_from=273-review). T62 same fix-class as T30 (plan-doc marker-version fix); T62 is the yaml-side partner (tracey doesn't scan yaml so it's documentary, but humans grep). T62 soft-conflict P998869301-T1 (rewrites dashboard-gateway.yaml GRPCRoute — T62 edits :3 marker-comment, P998869301-T1 edits :63-86; non-overlapping). T63 depends on P0287 (DONE — observability.nix T7 observe-only block landed; discovered_from=287-review). T63 is outcome-driven doc: read the VM log's CONFIRMED print, commit the answer to :268, replace print with assert. If vm-observability never KVM-ran (check kvm-pending.md), T63 is conditional on getting one KVM run first. T64 depends on P0287 (DONE — interceptor.rs:145 prose-ref arrived with it; discovered_from=287-review). T64 is same gotcha-class as P0229/P0289 doc-comment r[verify] misparse — rephrase r[...] prose to backtick-fenced. 5 sites across 5 files, all 1-line comment edits. T65 depends on P0273 (DONE — default.nix:485 comment arrived with P0273 T5; discovered_from=273-review). T65 is partner to P0304-T43 (tightens the grep pattern; T43 fixed the plan-doc, T65 fixes the default.nix prose). T66 soft-dep P0231 (UNIMPL — GetSizeClassStatus RPC arrives with it; discovered_from=231-review). T66 adds the spec marker so P0231's handler has an r[impl] target. Marker goes to scheduler.md:144+ alongside the sibling r[sched.admin.*] markers (list-workers/list-builds/clear-poison/list-tenants/create-tenant — all present, sizeclass-status missing). T66 can land BEFORE P0231 (marker-first discipline per tracey adoption)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root. [P0294](plan-0294-build-crd-full-rip.md) — T11-T15 reference the CRD's absence; must land after the rip.
**Soft-deps:** [P0215](plan-0215-max-silent-time.md) (T16-T18 cite its finding), [P0218](plan-0218-nar-buffer-config.md) (T19 fixes its plan doc), [P0243](plan-0243-vm-fod-proxy-scenario.md) (T17/T18 cross-worktree), [P0289](plan-0289-port-specd-unlanded-test-trio.md) (T14 same file).
**Conflicts with:** [`security.md`](../../docs/src/security.md) also touched by [P0286](plan-0286-privileged-hardening-device-plugin.md) (spec additions — different section). [`gateway.md`](../../docs/src/components/gateway.md) count=21 — T16 edits `:45`; [P0310](plan-0310-gateway-client-option-propagation.md) T4 edits `:62` — non-overlapping. [`phase4a.md`](../../docs/src/remediations/phase4a.md) T3+T13 overlap — same file, coordinate. [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) T17 + [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) + [P0309](plan-0309-helm-template-fodproxyurl-workerpool.md) — three plans, non-overlapping sections (`:199`, `:285`, `:103`).
