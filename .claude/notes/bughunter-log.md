# Bughunter cadence log — null results and below-threshold observations

Absence of finding IS signal. Future bughunter runs can diff against the audit
counts (unwrap count should stay ~constant; a jump = review).

- mc=77 (2026-03-19): 7-merge scan, null result. proto field-collision=none, emit_progress=single-def, path_tenants PK=point-lookup (≤1 row, JOIN can't multiply), config.styx changes=additive-only. unwrap/expect audit: 55 total = 52 cfg(test) + 3 invariant-cite. swallow=0, orphan-TODO=0.
- mc=84 (2026-03-19): 8-merge scan (d2041017..4986ab50). Found :275 ?→bail! leak → P0342. Smell-audit null: unwrap/expect=38 (non-test=3, all invariant-cite), silent-swallow=0, orphan-TODO=0 (upload.rs:164 TODO(P0263) closed this window), #[allow]=0, lock-cluster=0. grpc/mod.rs collision scan: P0266 fence speculative (ProgressUpdate handler doesn't exist at fence path); no trait-impl drift.

## mc=168 (eea3fb67..6b152a26, 2026-03-20)

Null result above threshold. 7 merges (P0356/docs-890214/P0236/P0291/P0234/P0371/P0372).

T1-T4 targets: all SAFE or already-tracked (rv substring-collision SAFE;
P0374 phantom-amend rebase clean; P0371 no catch-all 11/11 explicit;
P0291 is_cutoff refs = comments not dead code).

T5 actor-alive drift: 3-string variance (grpc/admin/worker_service)
PRE-DATES window — dd530256/47d5ce40. Folded into P0383
(admin-mod-split) T1 shared-fn extraction.

Smell counts: unwrap/expect=5 (2 test, 1 test, 2 moved-not-new);
silent-swallow=0; orphan-TODO=0; allow=0; lock-cluster=1 (moved);
panic/unsafe=0/0.

## mc=182 (2026-03-20)

Null result above threshold. 7 merges reviewed (mc175-182).

Cadence-negative: 7-merge window stayed clean; no accumulation above threshold.

## mc=189 (2026-03-20)

Null result above threshold. 7 merges reviewed (mc182-189).

Third consecutive null cadence entry (mc175 → mc182 → mc189). The 7-merge
window stayed clean through the P0376/P0381/P0384 refactor run — no fresh
accumulation despite high structural-churn merges.

## mc=196 (2026-03-20)

FINDING: ContentLookup self-match → P0397.
Otherwise null smell accumulation. Fourth entry in the cadence series.

## mc=203 (e5feb62c..8155e004, 2026-03-20)

7 merges. Smell counts: unwrap/expect=17 all-test-code, non-test=0.
silent-swallow=0 (one `let _=parse(text)?` has ?-propagation, discards value not error).
orphan-TODO=0 (8 TODOs all tagged P0254). allow=1 (unused_mut tagged P0254). lock-cluster=0.

Coord targets verdicts:
(1) P0253 ATerm dup — COVERED by P0304-T158 + P0398 soft-conflict. Escape-tables byte-identical (arm-order cosmetic).
(2) P0347 deadline mid-build — NOT A BUG: terminationGracePeriodSeconds=7200 inherited; worker run_drain waits in-flight; ephemeral.rs:286-292 documents heartbeat-timeout handling.
(3) P0373 const{} consistency — CONSISTENT: 3 uses all `const{assert!()}` form; no mix; no contract.rs (coord mention inaccurate).
(4) P0244×P0295 conflict — CLEAN: git merge-tree --write-tree sprint-1 p295 → c66f4b29 no-markers; p295 fork 91ccad72=mc~198 post-P0244.
(5) T-collision 2nd — NONE: docs-875101 writes T158+/T62+ correctly; phantom-commit e9b4b46b byte-identical auto-drops.

ERROR-PATH: 0 new production Result fns. RESOURCE: none suspicious.

## mc=224 (dd7f370d..3d2f7f20, 2026-03-20)

Null result above threshold. 7 merges reviewed (mc217-224: P0307/docs-988499/P0403/P0394/P0401/P0400/P0408).

Smell counts: unwrap/expect=9 (5× build.rs OUT_DIR idiomatic + 4× test code, ZERO in production src paths); silent-swallow=0; orphan-TODO=0; #[allow]=4 (2× dead_code metrics_grep.rs defensive + 2× result_large_err figment API-fixed 208B — all documented, test/support only); #[ignore]=1 (EXPLAIN test, dev-only documented); lock-cluster=0. Error-path: 2 new Result fns, both test-only.

Target-specific verdicts (all CLEAN):
- P0394 multi-line describe_counter — NON-ISSUE: detected via runtime recorder hook (with_local_recorder at rio-test-support/src/metrics.rs:240), not grep; emitted-grep handles multi-line via \s* (metrics_grep.rs:28 documents \s matches \n).
- P0403 NOT VALID/VALIDATE race — NON-ISSUE: premise wrong, migration 022 = CREATE INDEX CONCURRENTLY IF NOT EXISTS (single stmt, idempotent, INVALID-index recovery documented :21-23); no FK NOT VALID/VALIDATE path.
- P0408 1MiB .drv cap — NOT silent truncation: collect_nar_stream (client/mod.rs:202) → NarCollectError::SizeExceeded → Err→None→warn-level fallback; worker fetches with 4GiB MAX_NAR_SIZE (inputs.rs:235), scheduler-side optimization degrades gracefully.
- P0400 STATUS_CLASS vs P0406 progress — NON-ISSUE: statusClass ??gray fallback (graphLayout.ts:58, tested :120); P0406 progress() is build-level not per-derivation; P0410 closes cross-language gap.
- P0307 Jail vs P0409 validate_config — NOT overlap: Jail tests WIRING (roundtrip proves [poison].threshold=7 reaches cfg.poison.threshold); validate_config tests BOUNDS (config_rejects_*). Complementary.

P0307 f64-unvalidated gap already covered by rev-p409 followup (2026-03-20T16:05:49.867924) — NOT re-reported.

## mc=235-238 (2026-03-20)

**consol-mc235 P0412 proc-macro verdict:** NOT WORTH. 5 figment::Jail
standing-guard test copies = FULL universe (all 5 binary crates). ~50L
shared-wrapper vs ~300L proc-macro crate for per-crate TOML-literal
derivation. Net negative. [tls] assertion block (~5L×4 = 20L identical)
also not worth a helper. Leave as-is; each pair cross-refs scheduler
all_subconfigs_roundtrip_toml for rationale (T196 makes this grep-stable).

**consol-mc236 5-target verdict:** T1 P0412+P0416 combine = answered above.
T2 merge.py split = P0421.
T3 P0311 batch-sweep-shape = not duplication (14 test commits clean-merge,
workflow-correct). T4 T-collision = P0401 works at spec 9-digit; writer-side
11-digit format-divergence + 3 gaps covered by P0418.
T5 db/mod.rs residual = T206 (LOW-priority row-struct move).

**bughunt-mc238 finding:** SizeClassConfig.cpu_limit_cores Option<f64>
no validate_config check (NaN → bump-disabled, neg → always-bump).
Same P0415 failure class but missed by the wave. → P0424.

**coord P0414-FIXED archive-note:** merger-amend-branch-ref (step 7.5
detached-HEAD window) fixed by P0414 dag_flip compound. T191's
update-ref loud-fallback adds belt-and-suspenders. P0417 closes the
already-done double-bump; P0420 closes the count_bump write-order
TOCTOU. Three-plan chain (P0414→P0417→P0420) covers the full
merger state-machine failure surface.

**rev-p411 plan-doc-accuracy note (non-actionable):** plan-0411 Files
fence called for db/tests/recovery.rs (T9, not created — zero db-level
recovery tests existed; tests live at actor/tests/recovery.rs
integration-tier) and omitted db/tests/assignments.rs from tree-diagram
(impl correctly created it — 2 tests from pre-split :1856,:1880).
Plan doc over/under-specified. Only matters if plan doc becomes an
audit reference. Archived here, not fixed.
