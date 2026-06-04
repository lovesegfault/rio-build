//! The wanted-output predicate family backing the demand-driven
//! cache-hit criterion.
//!
//! A derivation node carries three parallel/related arrays:
//! `output_names` ↔ `expected_output_paths` (zip-indexed) and
//! `wanted_output_names` (the subset of declared outputs some consumer
//! or root selector actually demands). Two pieces of algebra are shared
//! by every component that touches them:
//!
//! - **The empty sentinel**: an empty `wanted_output_names` means "all
//!   declared outputs wanted" — the backward-compatible default for
//!   pre-migration rows, the `BasicDerivation` fallback, and `^*` roots.
//! - **The saturating union**: because empty = all, the union of two
//!   wanted sets saturates — `all ∪ X = all` — so combining sets from
//!   two consumers must yield empty if either side is empty.
//!
//! The scheduler's merge/dispatch/recovery classification and the
//! gateway's will-dispatch prediction + DAG dedup all call this single
//! implementation; do not re-derive the algebra at a call site. The
//! gateway predicts from its own submission's wanted set, while the
//! scheduler classifies against its effective wanted set scoped to
//! live interested builds — which may be narrower or wider than any
//! one submission's. The prediction is an inlining optimization (a
//! wrong guess costs bytes or a worker→store round-trip), not a
//! correctness contract; what must not drift between the two crates
//! is the sentinel/saturation algebra itself.

/// The wanted subset of `expected_output_paths`, resolved by zipping the
/// (`output_names` ↔ `expected_output_paths`) parallel arrays and keeping
/// only the entries whose name is in `wanted`. Empty `wanted` ⇒ all
/// declared outputs (yields every expected path) — the backward-compatible
/// sentinel for pre-migration rows, the `BasicDerivation` fallback, and
/// `^*` roots. A wanted name with no matching declared output is ignored
/// (defensive — the gateway only unions declared names).
///
/// Free function (not a method on any one node type) because the
/// merge-time cache-hit classification operates on the proto-mirror
/// `DerivationNode` before a `DerivationState` exists, and the gateway's
/// will-dispatch prediction operates on the proto type itself; every
/// call site MUST share one implementation or the hit criterion drifts
/// between prediction time, merge time, and dispatch time.
// r[impl sched.merge.wanted-outputs+3]
pub fn wanted_subset<'a>(
    output_names: &'a [String],
    expected_output_paths: &'a [String],
    wanted: &'a [String],
) -> impl Iterator<Item = &'a String> {
    let all = wanted.is_empty();
    output_names
        .iter()
        .zip(expected_output_paths.iter())
        .filter(move |(name, _)| all || wanted.contains(*name))
        .map(|(_, path)| path)
}

/// The *verifiable* wanted subset of `expected_output_paths`: the
/// non-empty concrete paths resolved by [`wanted_subset`], or `None`
/// when the resolution yields zero non-empty paths — every wanted name
/// unmatched against the declared outputs (a client sent `drv^bogus`
/// and the gateway does not validate the root OutputsSpec against the
/// declared outputs), or only floating-CA `""` placeholders.
///
/// THE single guard for every wanted-output completeness predicate.
/// The empty case is unrepresentable in the `Some` branch by
/// construction so that a `wanted.iter().all(present)` predicate can
/// never be vacuously true: a vacuous "all wanted outputs present"
/// verdict completes a derivation with zero outputs verified —
/// dispatching its dependents against missing inputs, adopting an
/// unfinished orphan as completed, or (in the top-down prune) dropping
/// the dependency closure from the submission entirely. On `None` the
/// caller MUST take its conservative branch: skip the classification,
/// fall back to all declared paths, or treat the node as unavailable.
/// Falling through to a from-source build / the full merge is always
/// safe; a false "complete" is not.
// r[impl sched.merge.wanted-outputs+3]
pub fn verifiable_wanted_paths<'a>(
    output_names: &'a [String],
    expected_output_paths: &'a [String],
    wanted_output_names: &'a [String],
) -> Option<Vec<&'a str>> {
    let paths: Vec<&str> = wanted_subset(output_names, expected_output_paths, wanted_output_names)
        .map(String::as_str)
        .filter(|p| !p.is_empty())
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// Union `src` into `dst`, saturating on the empty (= "all declared
/// outputs wanted") sentinel.
///
/// The STORED node-level union only ever grows — never shrink it, or
/// build B's `{out}` could un-want build A's still-needed `dev` in the
/// persisted fallback. Live-build scoping happens elsewhere: the
/// scheduler folds per-build contributions with this same helper into
/// an effective wanted set over LIVE interested builds, so a terminal
/// build's wants stop counting without the stored union ever
/// shrinking. Empty is the "all declared outputs wanted" sentinel, so
/// the union saturates: `all ∪ X = all`. If either side is empty, the
/// result is empty (= all). Otherwise the result is the sorted,
/// deduplicated set union.
///
/// Shared by the scheduler's merge-time `DerivationState::union_wanted`,
/// the gateway's multi-root DAG dedup, and (in SQL) the PG upsert's
/// union-on-conflict — all three must agree or one root's demand is
/// silently dropped.
pub fn union_wanted_saturating(dst: &mut Vec<String>, src: &[String]) {
    if dst.is_empty() || src.is_empty() {
        dst.clear(); // all ∪ anything = all
        return;
    }
    for n in src {
        if !dst.contains(n) {
            dst.push(n.clone());
        }
    }
    dst.sort_unstable();
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify sched.merge.wanted-outputs+3]
    /// The saturation algebra of [`union_wanted_saturating`]: empty is
    /// the "all outputs wanted" sentinel, so the union of "all" with
    /// anything saturates to "all" (stays/becomes empty) regardless of
    /// which operand carries it. Non-empty ∪ non-empty is a sorted,
    /// deduplicated set union, and re-unioning an already-present name
    /// is idempotent.
    #[test]
    fn union_wanted_saturating_algebra() {
        let s = |xs: &[&str]| -> Vec<String> { xs.iter().map(|x| x.to_string()).collect() };

        // {} ∪ {x} = {} — "all" absorbs any subset.
        let mut dst = s(&[]);
        union_wanted_saturating(&mut dst, &s(&["x"]));
        assert_eq!(dst, Vec::<String>::new());

        // {x} ∪ {} = {} — a consumer wanting all saturates the union.
        let mut dst = s(&["x"]);
        union_wanted_saturating(&mut dst, &s(&[]));
        assert_eq!(dst, Vec::<String>::new());

        // {b} ∪ {a} = {a, b} — sorted, deduplicated set union.
        let mut dst = s(&["b"]);
        union_wanted_saturating(&mut dst, &s(&["a"]));
        assert_eq!(dst, s(&["a", "b"]));

        // Idempotence: re-unioning an already-present name is a no-op.
        union_wanted_saturating(&mut dst, &s(&["a"]));
        assert_eq!(dst, s(&["a", "b"]));
    }

    proptest::proptest! {
        /// bughunt wave A4: the None-on-empty contract — the
        /// establishment kernel treats `None` as "nothing verifiable"
        /// and `Some(paths)` as "check these"; `Some(vec![])` would
        /// make the all-present conjunction vacuously TRUE and adopt a
        /// completion with zero verified paths. For ANY inputs, the
        /// result is never `Some` of an empty (or empty-string) set.
        #[test]
        fn prop_verifiable_wanted_never_some_empty(
            names in proptest::collection::vec("[a-z]{1,4}", 0..6),
            paths in proptest::collection::vec(
                proptest::option::weighted(0.7, "/nix/store/[a-z]{4}"), 0..6),
            wanted in proptest::collection::vec("[a-z]{1,4}", 0..6),
        ) {
            let paths: Vec<String> =
                paths.into_iter().map(Option::unwrap_or_default).collect();
            let got = verifiable_wanted_paths(&names, &paths, &wanted);
            if let Some(v) = got {
                proptest::prop_assert!(!v.is_empty(), "Some(empty) is forbidden");
                proptest::prop_assert!(
                    v.iter().all(|p| !p.is_empty()),
                    "empty-string paths must be filtered"
                );
            }
        }
    }
}
