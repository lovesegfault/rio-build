#!/usr/bin/env python3
"""The R31' predicate-derivation registry + the K-mutation standing
check (round-13 WO-S9-8(i) — the bug_047 born-broken lesson made
STANDING).

Argv: <src-root>.

R31' (the round-13 banner): a census generator's PREDICATE is derived
from type/symbol semantics or carries an adversarial plant battery
spanning the predicate's full shape space; every enforcement
artifact's self-test is MUTATION-TESTED at birth — the planted red
must die under K seeded mutations of the artifact. bug_047 shipped a
flagship enforcement artifact whose grandfather key was degenerate
from birth while its planted self-test passed anyway: enforcement is
no longer evaded by ignoring it but by BUILDING it on unverified
predicates. This registry makes the discharge standing:

  - POPULATION (machine-derived, never re-enumerated here): the
    enrolled generator fleet = census_corpora.REGISTRY names. Every
    name MUST carry a PROVENANCE row below and every PROVENANCE row
    MUST name an enrolled generator (totality both ways — a census
    with neither provenance nor debt is the gate red).
  - PROVENANCE kinds (closed alphabet, zero wildcard):
      ("derived", anchor_file, anchor_regex, how) — the predicate/key
        is computed from type/symbol/API-family semantics; the anchor
        names the derivation source IN the artifact and must resolve.
      ("planted", anchor_file, battery_regex, battery_id) — the
        artifact carries a full-shape plant battery AND a K-mutation
        harness; the anchor proves the battery exists in the artifact
        and must resolve (a battery that rots is a red, not a memory).
      ("debt", "YYYY-MM-DD", why) — the dated retrofit queue,
        SHRINK-ONLY: the committed debt set is pinned below; removing
        a row requires landing its battery/derivation, adding one
        requires editing this reviewed file (the visible-debt form;
        never silent).
  - The K-MUTATION STANDING CHECK: every "planted" artifact's battery
    anchor is verified live, and the batteries themselves run inside
    their owning artifacts' own CI invocations (the WO-S9-1 harness
    form — obligation_clock_census runs its MUTATIONS on every
    invocation; this registry pins that the harness EXISTS and has
    not been deleted or renamed out from under the claim).

Self-application (W13-BE; riders (a)-(d) of the R31' census riders):
the registry's own plants run first and its own K-mutation battery
(MUTATIONS below, the shared census_corpora.run_mutation_battery
harness) mutates a COPY of THIS file and requires the mutant's
self_battery to FAIL. Recursion is grounded at the fixture tier:
self_battery never invokes the mutation harness.
"""

import pathlib
import sys

import census_corpora

# The closed provenance alphabet (rider (c) — zero wildcard arms).
KINDS = ("derived", "planted", "debt")

# R31'-d (round-14 WO-S9-1, the merged_bug_004 perimeter repair — the
# DENOMINATOR RIDER): every PROVENANCE row carries a mandatory closed
# `axis` sub-field. Absence reds via the SAME two-way totality check as
# PROVENANCE (below) — opt-out is structurally impossible: a generator
# without an AXIS row is the gate red, exactly as a generator without a
# PROVENANCE row is. The `coverage` axis names every coverage/ratio/
# totality predicate (a generator that asserts "fraction of population
# satisfies X" or "all of N are covered"); each such row carries a
# `denominator-source` naming the AUTHORITATIVE population class —
# spec'd-replica-count / registry / owning-resource / OTHER+rationale —
# never a self-censored view (merged_bug_004's `resolved = addrs.len()`
# read total because the failure censored the denominator too). Non-
# coverage rows carry NO denominator-source: forcing one onto a syntax
# or cadence generator is a category error (the round-14 PD-2 ruling —
# a production-topology witness is the R31'-d(iii) face, not a ratio).
AXES = ("coverage", "cadence", "syntax", "other")
SOURCE_CLASSES = ("spec'd-replica-count", "registry", "owning-resource", "OTHER")

# Provenance declarations — REVIEWED claims over the machine-derived
# population (the values are declarations by design: each is the
# review surface R31' demands; the totality and the anchors are  # quantifier: census(check_registry)
# machine-checked).
PROVENANCE = {
    # --- planted (battery + K-mutation harness in-artifact) -----------
    "obligation-clock-census": (
        "planted",
        "nix/obligation_clock_census.py",
        r"MUTATIONS = \[",
        "W13-AX3 (WO-S9-1: content-key projection swaps both lanes, "
        "superset-accept, emptied walk, deleted grammar arm — K=5, "
        "each killed by its named oracle)",
    ),
    # --- derived (predicate computed from type/API-family semantics) --
    "jitter-saturation-seams": (
        "derived",
        "nix/census_corpora.py",
        r"DURATION_MUL_FAMILY = \[",
        "the predicate is std Duration's own panicking-multiply API "
        "family (mul_f64/mul_f32), never an observed-spelling list; "
        "the rider-(d) narrowed fixture is asserted in the battery",
    ),
    "wire-secs-pacing-seams": (
        "derived",
        "nix/census_corpora.py",
        r"WIRE_SECS_IDIOMS = \[",
        "the grammar is the GENERATED conversion-idiom x consumption-"
        "site product (WO-S8-3) with the completeness meta-pin",
    ),
    "duration-census": (
        "derived",
        "nix/census_corpora.py",
        r"DURATION_IDIOM_CELLS = \[",
        "the finder population is the duration-idiom product with one "
        "finder-vector plant per cell; enrolled seeds are mandatory "
        "finder vectors",
    ),
    # --- derived (round-14 D-18 retrofit: each row's derivation
    # --- source is the in-generator production table / refusal
    # --- predicate that census_corpora.REGISTRY's derived_from column
    # --- already records as the reviewed claim; the retrofit promotes
    # --- that claim from REGISTRY column to PROVENANCE row and pins
    # --- the anchor here so rot of the derivation source reds) -------
    "census-enrollment": (
        "derived",
        "nix/census_enrollment.py",
        r"CLAIM_SHAPES",
        "the predicate is the closed CLAIM_SHAPES grammar (the regex "
        "alphabet is enumerated from the shapes table, not observed "
        "spelling); selftest arm in-file",
    ),
    "metric-reason-help-sync": (
        "derived",
        "nix/metric_reason_help_sync.py",
        r"LABEL_KEYS",
        "the predicate is the LABEL_KEYS production set (the metric/"
        "reason/help key universe enumerated from the registration "
        "table, not author-observed)",
    ),
    "rule-citation-versions": (
        "derived",
        "nix/rule_citation_versions.py",
        r"productions = \[",
        "the predicate is the productions table (the citation grammar "
        "enumerated from the rule families)",
    ),
    "exposure-producer-census": (
        "derived",
        "nix/exposure_producer_census.py",
        r"DISPOSITIONS = \{",
        "the predicate is the closed DISPOSITIONS table (each "
        "producer disposition is type-derived from the exposure enum)",
    ),
    "reason-alert-sync": (
        "derived",
        "nix/tests/helm/42-reason-alert-sync.sh",
        r"INTENT_DROP_REASONS",
        "the predicate is the INTENT_DROP_REASONS set (derived from "
        "the reason enum's variant list, not observed labels)",
    ),
    "quint-policy": (
        "derived",
        "nix/quint_policy.py",
        r"def conj_expansion",
        "the predicate is the P8 binding-grammar conj_expansion "
        "(derived from the closed Mirrors/Environment alphabet); "
        "rule arms planted in-file",
    ),
    "quantifier-lexicon": (
        "planted",
        "nix/quantifier_lexicon.py",
        r"MUTATIONS = \[",
        "WO-S4-4 (W14-D3): the rider line-shape rule's K=4 battery "
        "(delimiter-check deleted / rider-grammar widened / splice-"
        "detection inverted / shape-walk unwired) via the shared "
        "harness; the LEXICON x TIERS product remains the derived face",
    ),
    "fixture-provenance": (
        "derived",
        "nix/fixture_provenance.py",
        r"LANES = \{",
        "the predicate is the closed LANES table (the r13-allow lane "
        "alphabet enumerated from the typed grammar)",
    ),
    "timeout-census": (
        "derived",
        "rio-controller/tests/timeout_census.rs",
        r"USE_GRAMMAR",
        "the predicate is the USE_GRAMMAR cell product (in-crate; the "
        "(wwwww) snapshot regenerates via its ritual)",
    ),
    "cap-reader-census": (
        "derived",
        "rio-controller/src/reconcilers/nodeclaim_pool/cover.rs",
        r"CAP_ALIASES",
        "the predicate is the CAP_ALIASES const table (in-crate; the "
        "reader set derived from the alias type's variant list)",
    ),
    "await-genset": (
        "derived",
        "rio-store/src/substitute.rs",
        r"acquire-site census",
        "the predicate is the acquire-site GEN-SET (in-crate; "
        "type-derived from the await form's signature family)",
    ),
    "cleanup-posture-fold": (
        "derived",
        "rio-store/src/substitute.rs",
        r"enum CleanupPosture",
        "the predicate is the CleanupPosture enum's own variant set "
        "(in-crate; type-derived, zero wildcard arms at the fold)",
    ),
    "subst-dep-eta-disposition": (
        "derived",
        "rio-scheduler/src/actor/tests/misc.rs",
        r"SubstDepEta",
        "the predicate is the SubstDepEta type's variant alphabet "
        "(in-crate; type-derived)",
    ),
    "refusal-agreement-census": (
        "derived",
        "rio-builder/src/runtime/pull.rs",
        r"judge_refusal",
        "the predicate is the judge_refusal fn's signature (in-crate; "
        "the authority is the fn body, the census proves agreement)",
    ),
    "destructive-lane-census": (
        "derived",
        "rio-store/src/gc/lane.rs",
        r"reaches-delete-sink",
        "the predicate is the reaches-delete-sink reachability (in-"
        "crate; derived from the lane graph's sink set)",
    ),
    "witnessed-disposition-product": (
        "derived",
        "rio-scheduler/src/actor/floor.rs",
        r"WITNESSED_LETTERS",
        "the predicate is the WITNESSED_LETTERS alphabet (in-crate; "
        "type-derived from the disposition enum)",
    ),
    "cell-emission-wire-injectivity": (
        "derived",
        "rio-scheduler/src/actor/tests/sla_contract.rs",
        r"classify_cell_emission",
        "the predicate is the classify_cell_emission fn's domain "
        "(in-crate; the injectivity check is type-derived from the "
        "emission cell product)",
    ),
    "pool-demand-view-consumers": (
        "derived",
        "rio-controller/src/reconcilers/pool/jobs.rs",
        r"iter_page",
        "the predicate is the iter_page consumer family (in-crate; "
        "derived from the view-consumer signature, W10-AH census)",
    ),
    "leader-edges-census": (
        "derived",
        "rio-scheduler/src/observability.rs",
        r"LEADER_EDGES",
        "the predicate is the LEADER_EDGES const table (in-crate; "
        "every row named and total per the in-file claim)",
    ),
    "exit-edge-census": (
        "derived",
        "nix/census_corpora.py",
        r"EXIT_EDGE_GIVEUP = re\.compile",
        "the predicate is the EXIT_EDGE_GIVEUP grammar (the latch-"
        "idiom alphabet enumerated from the typed needle family)",
    ),
    "reader-census-registry": (
        "derived",
        "nix/reader_census_registry.py",
        r"UNION_ROWS = \{",
        "the predicate is the UNION_ROWS (file x kind) key product "
        "(the round-12 R31 union; the bug_047-shaped quotient one "
        "framework over — derived from the typed key, not observed)",
    ),
    "duplicate-derivation-lint": (
        "derived",
        "nix/duplicate_derivation_lint.py",
        r"R33_ROWS = \{",
        "the predicate is the R33_ROWS table (symbol-existence keying "
        "derived from the producer-symbol family, the bug_026 "
        "lesson's neighbour)",
    ),
    # --- the dated retrofit queue (shrink-only debt; round-15) -------
    # --- D-18 ranking record: the 7 rows below have NO derived_from
    # --- anchor in census_corpora.REGISTRY (column 6 is None — they
    # --- predate the discipline); each needs either a derivation
    # --- source recorded in REGISTRY first or a full plant battery +
    # --- K-mutation harness landed in-artifact. Ranked by the
    # --- governing law's blast radius: cilium-labels-filter (network
    # --- policy — highest) > registration-writer-census/-store (db
    # --- writers) > cell-emission-arm-product (sla wire) > vanish-
    # --- census (health) > string-interior-spaces / streaming-open-ban
    # --- (lint-tier — lowest). Round-15 trigger: the named row's
    # --- derived_from anchor lands in REGISTRY or its plane is
    # --- touched.
    "cilium-labels-filter": ("debt", "2026-07-13", "author share-pin (no REGISTRY derived_from anchor); queue: round-15"),
    "registration-writer-census": ("debt", "2026-07-13", "in-crate (no REGISTRY derived_from anchor); queue: round-15"),
    "registration-writer-census-store": ("debt", "2026-07-13", "in-crate (no REGISTRY derived_from anchor); queue: round-15"),
    "cell-emission-arm-product": ("debt", "2026-07-13", "in-crate (no REGISTRY derived_from anchor); queue: round-15"),
    "vanish-census": ("debt", "2026-07-13", "in-crate (no REGISTRY derived_from anchor); queue: round-15"),
    "string-interior-spaces": ("debt", "2026-07-13", "author grammar (no REGISTRY derived_from anchor); queue: round-15"),
    "streaming-open-ban": ("debt", "2026-07-13", "author descriptor list (no REGISTRY derived_from anchor); queue: round-15"),
    "predicate-derivation-registry": (
        "planted",
        "nix/predicate_derivation_registry.py",
        r"MUTATIONS = \[",
        "self-application (W13-BE): the registry's own K battery",
    ),
    "cadence-polarity-registries": (
        "planted",
        "nix/cadence_polarity_registries.py",
        r"MUTATIONS = \[",
        "WO-S9-8(ii): per-arm stamp plants (un-witnessed/witnessed/"
        "cfg-test/pending-lane) + lifecycle plants + K=4 via the "
        "shared harness",
    ),

    "model-letter-reachability": (
        "planted",
        "nix/model_letter_reachability.py",
        r"MUTATIONS = \[",
        "WO-S9-8(iii): dead/constructed/exempt/unconsumed plants + "
        "K=4 via the shared harness; v1 jurisdiction disclosed",
    ),
    "doc-link-adjacency": (
        "planted",
        "nix/doc_link_adjacency.py",
        r"MUTATIONS = \[",
        "WO-S4-5 (W14-D5): the merged_bug_002 dup plant + the "
        "single-target/later-paren/non-doc/string-literal greens; "
        "K=4 via the shared harness (needle widened / population "
        "emptied / adjacency-window broken / doc-narrowing dropped)",
    ),
}

# R31'-d AXIS declarations — REVIEWED claims keyed by the SAME
# machine-derived population (totality both ways via the same  # quantifier: census(check_registry)
# :176-188 form below; a generator with PROVENANCE but no AXIS row
# reds; an AXIS row naming no generator reds). Each value is
# (axis, denominator_source) where denominator_source is
# (class, rationale) for the `coverage` axis and None otherwise (the
# PD-2 category-error guard enforces the None). At round-14 base zero
# of the 36 enrolled generators are coverage/ratio predicates in the
# R31'-d sense (they enumerate sites and check conditions; none
# computes "fraction of population satisfies X"); the founding
# `coverage` enrollments arrive with their slots and are reconciled at
# the wave-close re-mint.
AXIS = {
    # --- cadence (clock/periodic-event/timeout generators) ------------
    "obligation-clock-census": ("cadence", None),
    "timeout-census": ("cadence", None),
    "cadence-polarity-registries": ("cadence", None),
    # --- syntax (line-shape / grammar / idiom-family generators) ------
    "jitter-saturation-seams": ("syntax", None),
    "wire-secs-pacing-seams": ("syntax", None),
    "duration-census": ("syntax", None),
    "string-interior-spaces": ("syntax", None),
    "streaming-open-ban": ("syntax", None),
    "quantifier-lexicon": ("syntax", None),
    "exit-edge-census": ("syntax", None),
    "rule-citation-versions": ("syntax", None),
    "duplicate-derivation-lint": ("syntax", None),
    "quint-policy": ("syntax", None),
    "doc-link-adjacency": ("syntax", None),
    # --- other (enumeration/agreement/reachability generators; not
    # --- coverage-ratio predicates and not cadence/syntax) ------------
    "census-enrollment": ("other", None),
    "metric-reason-help-sync": ("other", None),
    "exposure-producer-census": ("other", None),
    "reason-alert-sync": ("other", None),
    "cilium-labels-filter": ("other", None),
    "fixture-provenance": ("other", None),
    "cap-reader-census": ("other", None),
    "vanish-census": ("other", None),
    "await-genset": ("other", None),
    "cleanup-posture-fold": ("other", None),
    "registration-writer-census": ("other", None),
    "registration-writer-census-store": ("other", None),
    "cell-emission-arm-product": ("other", None),
    "subst-dep-eta-disposition": ("other", None),
    "refusal-agreement-census": ("other", None),
    "destructive-lane-census": ("other", None),
    "witnessed-disposition-product": ("other", None),
    "cell-emission-wire-injectivity": ("other", None),
    "pool-demand-view-consumers": ("other", None),
    "leader-edges-census": ("other", None),
    "reader-census-registry": ("other", None),
    "model-letter-reachability": ("other", None),
    "predicate-derivation-registry": ("other", None),
}

# The shrink-only debt pin: the committed debt set may only SHRINK
# (landing a battery/derivation flips the row's kind); growth is an
# edit to this reviewed file AND a bump here — both visible.
DEBT_CEILING = 7


def check_registry(src_root, provenance=None, registry_names=None, axis=None):
    """All failure strings (rider-(b)-style collecting, no early
    return — the mutation harness depends on every arm surfacing)."""
    fails = []
    provenance = PROVENANCE if provenance is None else provenance
    axis = AXIS if axis is None else axis
    if registry_names is None:
        # The fleet, machine-derived (this registry is itself an
        # enrolled census_corpora.REGISTRY row — self-application).
        registry_names = {row[0] for row in census_corpora.REGISTRY}
    # Rider (a): population floor — an emptied fleet must red, never
    # vacuously green.
    if not registry_names:
        fails.append(
            "population floor — the enrolled generator fleet is empty "
            "((vvvvv): census_corpora.REGISTRY unreadable or emptied)"
        )
    # totality, both directions — quantifier: census(check_registry)
    # (R31': a census with neither
    # provenance nor debt is the gate red).
    for name in sorted(registry_names):
        if name not in provenance:
            fails.append(
                f"{name}: enrolled generator with NO predicate-provenance row — "
                f"declare derived(anchor)/planted(battery)/debt(dated) in "
                f"PROVENANCE (R31'; the bug_047 class ships exactly here)"
            )
    for name in sorted(provenance):
        if name not in registry_names:
            fails.append(
                f"{name}: provenance row names no enrolled generator — "
                f"registry rot or an unrecorded retirement"
            )
    # R31'-d totality, both directions — the SAME structural form as
    # the PROVENANCE check above (a generator with no AXIS row reds;
    # an AXIS row naming no generator reds; opt-out is structurally
    # impossible by reusing the same totality machinery).
    for name in sorted(registry_names):
        if name not in axis:
            fails.append(
                f"{name}: enrolled generator with NO axis row — declare "
                f"coverage(denominator-source)/cadence/syntax/other in AXIS "
                f"(R31'-d; the merged_bug_004 class hides exactly here)"
            )
    for name in sorted(axis):
        if name not in registry_names:
            fails.append(
                f"{name}: axis row names no enrolled generator — registry rot"
            )
    # R31'-d per-row: closed axis alphabet; coverage rows carry a
    # denominator-source in the closed class set; non-coverage rows
    # carry None (the PD-2 category-error guard).
    for name, (ax, source) in sorted(axis.items()):
        if ax not in AXES:
            fails.append(
                f"{name}: axis `{ax}` outside the closed alphabet "
                f"{AXES} (zero wildcard arms)"
            )
            continue
        if ax == "coverage":
            cls, rationale = source if source is not None else (None, None)
            if cls is None:
                fails.append(
                    f"{name}: coverage-axis row with NO denominator-source — "
                    f"name the AUTHORITATIVE population class "
                    f"{SOURCE_CLASSES} (R31'-d(iv): a coverage predicate "
                    f"over a self-censored universe is the merged_bug_004 "
                    f"defeat — 'the answers we got back' is the rejected "
                    f"answer)"
                )
            elif cls not in SOURCE_CLASSES:
                fails.append(
                    f"{name}: denominator-source class `{cls}` outside the "
                    f"closed alphabet {SOURCE_CLASSES} — a self-censored "
                    f"view (DNS answers, readiness-filtered lists, the "
                    f"survivors of the thing being measured) is NOT an "
                    f"authoritative population (R31'-d(i))"
                )
            if cls == "OTHER" and not rationale:
                fails.append(
                    f"{name}: denominator-source OTHER without a recorded "
                    f"rationale (R31'-d(ii))"
                )
        elif source is not None:
            fails.append(
                f"{name}: non-coverage axis `{ax}` carries a "
                f"denominator-source — category error (the round-14 PD-2 "
                f"ruling: a production-topology witness or syntax/cadence "
                f"generator is NOT a coverage/ratio predicate; forcing a "
                f"denominator-source onto it conflates the R31'-d(iii) face "
                f"with the R31'-d(iv) registry axis)"
            )
    # Per-row: closed kind alphabet; anchors resolve; debt dated.
    debt_count = 0
    for name, row in sorted(provenance.items()):
        kind = row[0]
        if kind not in KINDS:
            fails.append(
                f"{name}: provenance kind `{kind}` outside the closed "
                f"alphabet {KINDS} (zero wildcard arms)"
            )
            continue
        if kind in ("derived", "planted"):
            _k, rel, anchor, _how = row
            f = src_root / rel
            text = f.read_text(encoding="utf-8") if f.is_file() else ""
            import re as _re

            if not _re.search(anchor, text):
                fails.append(
                    f"{name}: {kind} anchor /{anchor}/ does not resolve in "
                    f"{rel} — the derivation source or the battery rotted "
                    f"(a battery that rots is a red, not a memory)"
                )
        else:
            debt_count += 1
            date = row[1]
            if len(date) != 10 or date[4] != "-" or date[7] != "-":
                fails.append(f"{name}: debt row's retrofit date `{date}` is not dated YYYY-MM-DD")
    if debt_count > DEBT_CEILING:
        fails.append(
            f"debt rows grew past the shrink-only ceiling "
            f"({debt_count} > {DEBT_CEILING}) — land batteries or record "
            f"the growth by bumping DEBT_CEILING in review"
        )
    return fails


def self_battery(src_root) -> list:
    """Riders (a)-(c) plants, failure-collecting. NEVER early-returns;
    never invokes the mutation harness (W13-BE grounding)."""
    fails = []
    # The live registry must be green-shaped on its own tree (floor
    # arm: a broken live check would mask every plant below).
    live = check_registry(src_root)
    if live:
        fails.extend(f"live: {x}" for x in live)
    # (b) enrollment plant: an enrolled generator WITHOUT a provenance
    # row must red.
    got = check_registry(
        src_root,
        provenance={},
        registry_names={"strawman-census"},
        axis={"strawman-census": ("other", None)},
    )
    if not any("NO predicate-provenance row" in x for x in got):
        fails.append(f"the missing-provenance plant did not red: {got}")
    # (b) reverse plant: a provenance row naming no generator reds.
    got = check_registry(
        src_root,
        provenance={"ghost": ("debt", "2026-06-12", "x")},
        registry_names={"real"},
        axis={"real": ("other", None)},
    )
    if not any("names no enrolled generator" in x for x in got) or not any(
        "NO predicate-provenance row" in x for x in got
    ):
        fails.append(f"the ghost-row/uncovered pair did not both red: {got}")
    # (c) closed-alphabet plant: an unknown kind reds.
    got = check_registry(
        src_root,
        provenance={"x": ("vibes", "2026-06-12", "no")},
        registry_names={"x"},
        axis={"x": ("other", None)},
    )
    if not any("provenance kind `vibes` outside" in x for x in got):
        fails.append(f"the unknown-kind plant did not red: {got}")
    # (b) rot plant: a planted row whose battery anchor is dead reds.
    got = check_registry(
        src_root,
        provenance={"x": ("planted", "nix/census_corpora.py", r"NO_SUCH_" + r"BATTERY", "b")},
        registry_names={"x"},
        axis={"x": ("other", None)},
    )
    if not any("does not resolve" in x for x in got):
        fails.append(f"the dead-battery-anchor plant did not red: {got}")
    # The undated-debt plant.
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "someday", "no date")},
        registry_names={"x"},
        axis={"x": ("other", None)},
    )
    if not any("not dated" in x for x in got):
        fails.append(f"the undated-debt plant did not red: {got}")
    # The ceiling plant: a widened debt set reds.
    wide = {f"d{i}": ("debt", "2026-06-12", "x") for i in range(DEBT_CEILING + 1)}
    wide_ax = {k: ("other", None) for k in wide}
    got = check_registry(src_root, provenance=wide, registry_names=set(wide), axis=wide_ax)
    if not any("shrink-only ceiling" in x for x in got):
        fails.append(f"the debt-ceiling plant did not red: {got}")
    # R31'-d (b) totality plant: a generator WITHOUT an AXIS row reds
    # (the same two-way form as the missing-provenance plant above —
    # opt-out structurally impossible).
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={},
    )
    if not any("NO axis row" in x for x in got):
        fails.append(f"the missing-axis plant did not red: {got}")
    # R31'-d (c) closed-axis plant: an unknown axis reds.
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={"x": ("vibes", None)},
    )
    if not any("axis `vibes` outside" in x for x in got):
        fails.append(f"the unknown-axis plant did not red: {got}")
    # R31'-d denominator plants — the W14-I1 battery (every face):
    # (1) a coverage row with NO denominator-source reds.
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={"x": ("coverage", None)},
    )
    if not any("NO denominator-source" in x for x in got):
        fails.append(f"the missing-denominator plant did not red: {got}")
    # (2) a coverage row with a self-censored source class reds (the
    # merged_bug_004 shape: 'the answers we got back').
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={"x": ("coverage", ("the-answers-we-got", "DNS readiness"))},
    )
    if not any("outside the closed alphabet" in x and "self-censored" in x for x in got):
        fails.append(f"the self-censored-source plant did not red: {got}")
    # (3) OTHER without a rationale reds.
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={"x": ("coverage", ("OTHER", ""))},
    )
    if not any("OTHER without a recorded rationale" in x for x in got):
        fails.append(f"the OTHER-no-rationale plant did not red: {got}")
    # (4) a non-coverage row WITH a denominator-source reds (PD-2).
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={"x": ("syntax", ("registry", "wrong"))},
    )
    if not any("category error" in x for x in got):
        fails.append(f"the category-error plant did not red: {got}")
    # (5) the founding-shape green: a coverage row with a valid
    # source class is NOT flagged (the W14-I1 founding-enrollment arm).
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "2026-06-12", "y")},
        registry_names={"x"},
        axis={"x": ("coverage", ("spec'd-replica-count", "Deployment spec.replicas"))},
    )
    if any("denominator" in x or "axis" in x.lower() for x in got):
        fails.append(f"the valid-denominator founding shape FALSELY flagged: {got}")
    return fails


# Rider (d): the registry's own K-mutation fixtures (the WO-S9-1
# template self-applied through the SHARED harness,
# census_corpora.run_mutation_battery; needles concatenation-split so
# this table never matches itself). K=5.
MUTATIONS = [
    (
        "totality-inverted",
        "the missing-provenance arm disabled — killed by the"
        " missing-provenance plant",
        "        if name not in " + "provenance:",
        "        if False and name not in " + "provenance:",
    ),
    (
        "kind-alphabet-widened",
        "the closed-kind check disabled (any kind accepted) — killed"
        " by the unknown-kind plant",
        "        if kind not in " + "KINDS:",
        "        if False and kind not in " + "KINDS:",
    ),
    (
        "anchor-check-disabled",
        "derivation/battery anchors no longer verified — killed by the"
        " dead-battery-anchor plant",
        "            if not _re." + "search(anchor, text):",
        "            if False and not _re." + "search(anchor, text):",
    ),
    (
        "population-emptied",
        "the fleet derivation emptied — killed by the live arm (every"
        " provenance row reds as naming no enrolled generator)",
        "        registry_names = {row[0] for row in " + "census_corpora.REGISTRY}",
        "        registry_names = " + "set()",
    ),
    (
        "debt-ceiling-unbounded",
        "the shrink-only ceiling disabled — killed by the ceiling"
        " plant",
        "    if debt_count > " + "DEBT_CEILING:",
        "    if False and debt_count > " + "DEBT_CEILING:",
    ),
    (
        "axis-totality-disabled",
        "the R31'-d missing-axis arm disabled — killed by the"
        " missing-axis plant (opt-out becomes silently possible)",
        "        if name not in " + "axis:",
        "        if False and name not in " + "axis:",
    ),
    (
        "denominator-check-deleted",
        "the coverage denominator-source presence check disabled —"
        " killed by the missing-denominator plant (the merged_bug_004"
        " class re-hides)",
        "            if cls is " + "None:",
        "            if False and cls is " + "None:",
    ),
    (
        "source-class-widened",
        "the closed source-class alphabet accepts anything — killed"
        " by the self-censored-source plant",
        "            elif cls not in " + "SOURCE_CLASSES:",
        "            elif False and cls not in " + "SOURCE_CLASSES:",
    ),
]


def main() -> int:
    src_root = pathlib.Path(sys.argv[1])
    battery = self_battery(src_root)
    if battery:
        print("FAIL: predicate-derivation registry self-battery —", file=sys.stderr)
        for x in battery:
            print(f"  {x}", file=sys.stderr)
        return 1
    killed = census_corpora.run_mutation_battery(
        pathlib.Path(__file__), MUTATIONS, "self_battery", (src_root,)
    )
    if killed:
        print("FAIL: predicate-derivation registry K-mutation battery —", file=sys.stderr)
        for x in killed:
            print(f"  {x}", file=sys.stderr)
        return 1
    n = {"derived": 0, "planted": 0, "debt": 0}
    for row in PROVENANCE.values():
        n[row[0]] += 1
    nax = {a: 0 for a in AXES}
    for ax, _ in AXIS.values():
        nax[ax] += 1
    print(
        f"predicate-derivation registry: {len(PROVENANCE)} generators — "
        f"{n['planted']} planted (battery + K-mutations), {n['derived']} derived, "
        f"{n['debt']} dated debt rows (shrink-only, ceiling {DEBT_CEILING}); "
        f"R31'-d axis: {nax['coverage']} coverage (denominator-source named), "
        f"{nax['cadence']} cadence, {nax['syntax']} syntax, {nax['other']} other"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
