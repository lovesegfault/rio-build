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
    # --- the dated retrofit queue (shrink-only debt; round-14 unless
    # --- a WO lands the battery earlier) -------------------------------
    "census-enrollment": ("debt", "2026-06-12", "author regex predicate; queue: round-14"),
    "metric-reason-help-sync": ("debt", "2026-06-12", "author label-key predicate; queue: round-14"),
    "rule-citation-versions": ("debt", "2026-06-12", "author message-tail keying; queue: round-14"),
    "exposure-producer-census": ("debt", "2026-06-12", "author dispositions table; queue: round-14"),
    "reason-alert-sync": ("debt", "2026-06-12", "author reason list; queue: round-14"),
    "cilium-labels-filter": ("debt", "2026-06-12", "author share-pin; queue: round-14"),
    "string-interior-spaces": ("debt", "2026-06-12", "author grammar; queue: round-14"),
    "streaming-open-ban": ("debt", "2026-06-12", "author descriptor list; queue: round-14"),
    "quint-policy": ("debt", "2026-06-12", "rule arms planted but no K-mutation harness; queue: round-14"),
    "quantifier-lexicon": ("debt", "2026-06-12", "author lexicon; queue: round-14"),
    "fixture-provenance": ("debt", "2026-06-12", "author lanes table; queue: round-14"),
    "timeout-census": ("debt", "2026-06-12", "in-crate; author use-grammar; queue: round-14"),
    "cap-reader-census": ("debt", "2026-06-12", "in-crate alias table; queue: round-14"),
    "vanish-census": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "await-genset": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "cleanup-posture-fold": ("debt", "2026-06-12", "in-crate enum fold; queue: round-14"),
    "registration-writer-census": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "registration-writer-census-store": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "cell-emission-arm-product": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "subst-dep-eta-disposition": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "refusal-agreement-census": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "destructive-lane-census": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "witnessed-disposition-product": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "cell-emission-wire-injectivity": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "pool-demand-view-consumers": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "leader-edges-census": ("debt", "2026-06-12", "in-crate; queue: round-14"),
    "exit-edge-census": ("debt", "2026-06-12", "authored latch-idiom needles (the grep grammar half); queue: round-14"),
    "reader-census-registry": ("debt", "2026-06-12", "the round-12 R31 union rows: (file x kind) keys are the bug_047-shaped quotient one framework over; queue: round-14 (named in the WO-S9-8 retrofit list)"),
    "duplicate-derivation-lint": ("debt", "2026-06-12", "symbol-existence keying (the bug_026 lesson's neighbour); queue: round-14"),
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
    ),}

# The shrink-only debt pin: the committed debt set may only SHRINK
# (landing a battery/derivation flips the row's kind); growth is an
# edit to this reviewed file AND a bump here — both visible.
DEBT_CEILING = 29


def check_registry(src_root, provenance=None, registry_names=None):
    """All failure strings (rider-(b)-style collecting, no early
    return — the mutation harness depends on every arm surfacing)."""
    fails = []
    provenance = PROVENANCE if provenance is None else provenance
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
    )
    if not any("NO predicate-provenance row" in x for x in got):
        fails.append(f"the missing-provenance plant did not red: {got}")
    # (b) reverse plant: a provenance row naming no generator reds.
    got = check_registry(
        src_root,
        provenance={"ghost": ("debt", "2026-06-12", "x")},
        registry_names={"real"},
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
    )
    if not any("outside the closed" in x for x in got):
        fails.append(f"the unknown-kind plant did not red: {got}")
    # (b) rot plant: a planted row whose battery anchor is dead reds.
    got = check_registry(
        src_root,
        provenance={"x": ("planted", "nix/census_corpora.py", r"NO_SUCH_" + r"BATTERY", "b")},
        registry_names={"x"},
    )
    if not any("does not resolve" in x for x in got):
        fails.append(f"the dead-battery-anchor plant did not red: {got}")
    # The undated-debt plant.
    got = check_registry(
        src_root,
        provenance={"x": ("debt", "someday", "no date")},
        registry_names={"x"},
    )
    if not any("not dated" in x for x in got):
        fails.append(f"the undated-debt plant did not red: {got}")
    # The ceiling plant: a widened debt set reds.
    wide = {f"d{i}": ("debt", "2026-06-12", "x") for i in range(DEBT_CEILING + 1)}
    got = check_registry(src_root, provenance=wide, registry_names=set(wide))
    if not any("shrink-only ceiling" in x for x in got):
        fails.append(f"the debt-ceiling plant did not red: {got}")
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
    print(
        f"predicate-derivation registry: {len(PROVENANCE)} generators — "
        f"{n['planted']} planted (battery + K-mutations), {n['derived']} derived, "
        f"{n['debt']} dated debt rows (shrink-only, ceiling {DEBT_CEILING})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
