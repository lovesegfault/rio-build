# bughunt-4 S5b generated sweep sets (banner b)

Each section is the committed output of the named command at the
commit that closes the finding. Re-run the command to re-derive; a
drifted set is a failed sweep, not a stale doc. Line numbers are as of
the S5b chain tip.

## bug_178 — open-coded store-unreachable alphabets

    $ rg -n 'Unavailable \| tonic::Code::DeadlineExceeded|Code::Unavailable \| Code::DeadlineExceeded' --type rust

    (empty — zero open-coded store-lane alphabets remain)

The two pre-fix members (rio-builder/src/upload/mod.rs
is_store_unreachable; rio-builder/src/runtime/result.rs MetadataFetch
arm) both route through rio_common::classify::is_store_unreachable_code.
rio_common::grpc::is_transient is the OTHER law (retry advice, not
unreachability evidence) — intentionally distinct, divergence argued in
classify.rs.

## bug_182 — every run-state consumer

    $ rg -n 'store_degraded_run|trailing_uncharged_class_count|run_step\(' --type rust -g '!*/tests/*'

    rio-retry-kernel/src/lib.rs:1550-1588   decide() fold — ALL run maintenance
                                            routes through run_step (LaneSkip/
                                            Pace/Extend/Break), hoisted above
                                            row_to_event
    rio-retry-kernel/src/lib.rs:2097-2121   run_step + is_bounded_uncharged_row
                                            (the registry-derived classifier)
    rio-retry-kernel/src/lib.rs:2135-2147   trailing_uncharged_class_count —
                                            the admission scan, same classifier
    rio-scheduler/src/retry_policy.rs       test docs only (no second consumer)

No third run-state consumer exists; both production consumers call
run_step, so the law structurally cannot fork again
(check_run_step_registry_consistency pins the classifier to the
registry).

## merged_bug_210 — every precedence consumer

    $ rg -n 'fold_substitute_loop\(|fold_tenant_attempts\(|\.fold\(\)' --type rust -g '!*test*'

    rio-store/src/substitute.rs:1093        loop-fold verdict match (per-arm
                                            handling; no ordering assumption)
    rio-evidence-kernel/src/outcome.rs:565  TenantAttemptRecorder::fold →
                                            fold_tenant_attempts
    rio-store/src/materialize/executor.rs   recorder .fold() consumer

Both folds' orderings are pinned to disposition_tier ∘
classify_substitute_failure (K1's min-tier law sweeps the loop fold;
fold_orders_match_disposition_tiers binds the tenant fold). No third
fold and no other consumer of the precedence exists.

## merged_bug_026 — every parallel-array consumer + producer

    $ rg -n 'wanted_subset\(' --type rust

    rio-common/src/wanted_outputs.rs:92     verifiable_wanted_paths (THE single
                                            guard — None on length skew)
    rio-common/src/wanted_outputs.rs        proptest oracle (asserts None on
                                            every skewed pair)
    rio-scheduler/src/state/derivation.rs   test helper (test-only)
    rio-scheduler/src/ca/resolve.rs         test name only

    Producer: rio-gateway/src/translate.rs:597 `.unzip()` — pairs by
    construction.

## merged_bug_263 — every Unobtainable consumer

    $ rg -l 'Unobtainable' --type rust  (excl. UnobtainableRouting /
      ResolvedUnobtainable / MaterializationUnobtainable homonyms)

    rio-store/src/materialize/executor.rs   producer — trust_refused set from
                                            the existing refusal census
    rio-scheduler/src/actor/materialize.rs  consumer — field threaded into
                                            RoutingInputs
    rio-scheduler/src/actor/tests/          literals (trust_refused: false)
    rio-evidence-kernel/src/routing.rs      the typed axis + settlement law
    rio-evidence-kernel/src/outcome.rs      fold homonyms only (no proto decode)
    rio-store/src/substitute.rs             probe-cache eviction mirror
    rio-proto/src/lib.rs                    generated decode

No consumer parses `Unobtainable.cause` (the display string) for
control flow; the typed field is the only settlement input.

## merged_bug_145 — every PullPhaseOutcome variant carries MintEvidence

    $ rg -n 'PullPhaseOutcome::' rio-builder/src/runtime/pull.rs (match arms)

    Assigned(_)                    the mint itself rides the variant
    Gone                           authoritative not-wanted answer (any
                                   straggler mint reports into a Gone
                                   consumption)
    IdleExit  { maybe_minted }     confirm under ConfirmRegime::Idle
    Shutdown  { maybe_minted }     confirm under ConfirmRegime::Shutdown
    Rejected  { maybe_minted, … }  best-effort confirm, exit stays nonzero

The exhaustive `mint_evidence` table (runtime/pull.rs tests) is the
compile-time witness: a future variant cannot exist without naming its
evidence.
