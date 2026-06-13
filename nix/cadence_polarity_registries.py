#!/usr/bin/env python3
"""The R34 (periodic-event, bound) census + the R33' polarity/units
rider registry (round-13 WO-S9-8(ii)).

Argv: <src-root> [--verify-landed].

R34 (the bug_018 repair, standing): every conjunctive gate whose
disarming clock is refreshed by a periodic event ships a compile-time
assert (refresh period > bound + phase margin — the liveness.rs
idiom), and refresh-on-no-op REJECTS: activity stamps require
occupancy evidence. The census below pairs each enrolled gate
conjunct with the clock that refreshes it and the bound it disarms;
pairs lacking the compile assert are census rows with the residual
named. R33' (the merged_bug_002 repair, standing): every R33 producer
mint carries polarity+units in the type or a checked rider; the
registry below annotates each enrolled cross-seam quantity with its
per-reader direction and units — re-conflation is a lint red.

LIFECYCLE (the obligation-census house form): rows land with their
slots; a row is `pending:<slot>` until its landing, then flips to
anchored at the wave-close `--verify-landed` pass — an anchored row
whose anchor stops resolving is rot (red), and a row still pending at
the close is a red (the close flips it or fails).

THE NO-OP STAMP ARM (R34(ii), enforced from birth): production writes
to `last_self_activity`-family stamps must carry an `r34-occupancy:`
witness comment within 3 lines above (naming the outcome evidence
that did work) or ride a census row — `CutStep::Empty` stamping
activity is the named defeat (bug_018: the cadence-coincidence gate).

Self-test arms run first (riders (a)-(d)); the K-mutation battery
rides the shared census_corpora.run_mutation_battery harness.
"""

import pathlib
import re
import sys

import census_corpora
import rust_strip

# --- R34: the (periodic-event, bound) census ---------------------------
# name -> (state, slot, file, anchor_regex, pair, assert_status)
#   state: "landed" (anchor must resolve) | "pending" (flips at close)
#   pair: "(periodic-event, bound)" prose — the two constants named
#   assert_status: "compile-assert" | "residual: <priced>"
R34_PAIRS = {
    "uploader-keepalive-vs-idle-abort": (
        "landed",
        "pre-wave",
        "rio-common/src/liveness.rs",
        r"UPLOADER_KEEPALIVE_PERIOD",
        "(UPLOADER_KEEPALIVE_PERIOD x margin, INBOUND_IDLE_ABORT)",
        "compile-assert (keepalive_conforms + the planted negative control — THE in-tree idiom R34 generalizes)",
    ),
    "log-cut-interval-vs-idle-bound": (
        "pending",
        "S5",
        None,
        None,
        "(log cut interval, INBOUND_IDLE_BOUND) — the bug_018 banner pair",
        "pending: WO-S5-1's assert (PD-4 expanded form: worst case ~ bound + 3x interval + housekeeping) flips here at the close",
    ),
    "udev-settle-vs-condition-clock": (
        "pending",
        "S2",
        None,
        None,
        "(udev coldplug settle, systemd job-start Condition evaluation) — merged_bug_045's wrong-clock gate",
        "pending: WO-S2-2's settle-then-branch dispatcher flips here (the R34(iii) systemd tier)",
    ),
    "scaler-funding-vs-sensor-cadence": (
        "pending",
        "S6",
        None,
        None,
        "(scaler decide tick, sensor-reading presence) — merged_bug_009's sensor-absent funding clock",
        "pending: WO-S6-2's hoist flips here (the R34(iii) sensor tier)",
    ),
    "session-drain-stamp-vs-stale-after": (
        "pending",
        "S5",
        None,
        None,
        "(drain heartbeat stamp cadence, SESSION_STALE_AFTER) — the F10/WO-S5-7 pair",
        "pending: the H5-pack assert-or-residual verdict enrolls verbatim at the close (CE-3: the watchdog-slow-cut residual is priced if arm A)",
    ),
}

# --- R33': the polarity/units rider registry ----------------------------
# name -> (state, slot, file, anchor_regex, readers, units, tier)
#   readers: "<reader>: <direction>" semicolon list — the per-reader
#   direction table merged_bug_002's prose census lacked.
R33_RIDERS = {
    "disk-p90-raw-vs-floored": (
        "pending",
        "S4",
        None,
        None,
        "reject/explain/classify_ceiling: raw; sizing: floored",
        "bytes (p90 of per-build peak)",
        "pending: H4-pack posts the TYPE names + the WO-S4-1 tier verdict (type-blocked vs census-tier) — recorded here at the close",
    ),
    "disk-evidence-peak-vs-status": (
        "pending",
        "S2",
        None,
        None,
        "sizing: monitor peak (max-fold); classification: one-shot status",
        "bytes vs typed status (two products, two consumers)",
        "pending: WO-S2-4's split lands; flips at the close",
    ),
    "weight-ring-vs-full-slice": (
        "pending",
        "S4",
        None,
        None,
        "fit ordinals: ring weights; anchor floor: full-slice recency",
        "ordinal weights (one decay law, one domain post-fix)",
        "pending: WO-S4-2's one-ordinal-domain producer flips here",
    ),
    "pod-ephemeral-vs-solve-disk": (
        "pending",
        "S2",
        None,
        None,
        "pod request: pod-ephemeral units; corroboration band: solve-disk units",
        "ONE shared minting fn (pod_ephemeral_request) post-fix",
        "pending: the H2-pack OQ-3 home verdict (rio-common arm expected-fire) enrolls the producer anchor at the close",
    ),
}

# The no-op stamp grammar (R34(ii)): occupancy-stamp writes need an
# occupancy witness comment naming the outcome evidence.
STAMP_RE = re.compile(r"last_self_activity\s*=")
STAMP_ALLOW = re.compile(r"r34-occupancy:")
# Stamp FILES pending their owning slot's landing (the bug_018 repair
# plane: service.rs:1609's cut-arm stamp is the charged defeat and
# :1810 its sibling — S5 repairs them this wave). Pre-close the
# pending file's hits are suspended; the wave-close --verify-landed
# REDS any survivor (the close aligns the landed shape with occupancy
# witnesses per the H5 pack, or the row stays red and the close
# fails). Shrink-only: removing an entry requires the witnesses;
# adding one requires editing this reviewed table.
STAMP_PENDING = {
    "rio-store/src/logs/service.rs": "S5 (WO-S5-1: the occupancy-stamp repair)",
}


def scan_noop_stamps(files, pending=None, verify_landed=False):
    """[(rel:line, msg)] for un-witnessed occupancy-stamp writes.
    `pending` files are suspended pre-close and RED at the
    --verify-landed pass (the lifecycle's stamp lane)."""
    pending = STAMP_PENDING if pending is None else pending
    hits = []
    for rel, raw in files:
        try:
            pruned = rust_strip.strip_cfg_test(raw, source=rel)
        except rust_strip.StripError:
            pruned = raw
        lexed, _ = rust_strip.lex(pruned, blank_string_bodies=True)
        lines = raw.splitlines()
        for m in STAMP_RE.finditer(lexed):
            lineno = lexed.count("\n", 0, m.start()) + 1
            window = "\n".join(lines[max(0, lineno - 4) : lineno])
            if STAMP_ALLOW.search(window):
                continue
            if rel in pending:
                if verify_landed:
                    hits.append(
                        f"{rel}:{lineno}: occupancy stamp still un-witnessed at "
                        f"the landed verify — the pending lane ({pending[rel]}) "
                        f"must be emptied at the wave close"
                    )
                continue
            hits.append(
                f"{rel}:{lineno}: occupancy stamp written without an "
                f"`r34-occupancy:` witness (R34(ii): a stamp a no-op can "
                f"write is not activity evidence — name the outcome that "
                f"did work, or enroll the site in R34_PAIRS)"
            )
    return hits


def check_rows(src_root, rows, kind, verify_landed=False):
    """Failure list over a row table (failure-collecting)."""
    fails = []
    if not rows:
        fails.append(f"{kind}: population floor — zero enrolled rows ((vvvvv))")
    for name, row in sorted(rows.items()):
        state, slot, rel, anchor = row[0], row[1], row[2], row[3]
        if state == "pending":
            if verify_landed:
                fails.append(
                    f"{kind} row `{name}` still pending:{slot} at the landed "
                    f"verify — the wave-close flips it with its anchors or "
                    f"the close fails"
                )
            continue
        if state != "landed":
            fails.append(f"{kind} row `{name}`: state `{state}` outside {{landed, pending}}")
            continue
        f = src_root / rel
        text = f.read_text(encoding="utf-8") if f.is_file() else ""
        if not re.search(anchor, text):
            fails.append(
                f"{kind} row `{name}`: anchor /{anchor}/ does not resolve "
                f"in {rel} — the landed construction moved or rotted"
            )
    return fails


def self_battery(src_root) -> list:
    """Riders (a)-(c), failure-collecting; never invokes the mutation
    harness (W13-BE grounding)."""
    fails = []
    # Rider (a): the production walk floor, driven through the same
    # jurisdiction derivation the live scan uses.
    walked = [
        rel for rel, _raw in _production_files(src_root)
    ]
    if not walked:
        fails.append(
            "rider (a): population floor — the production walk yielded "
            "zero files ((vvvvv): mis-staged tree or emptied walk)"
        )
    # Rider (c) plants — the stamp arm, both polarities.
    plant = scan_noop_stamps(
        [("planted/stamp.rs", "fn f(c: &mut C) { c.last_self_activity = now(); }\n")]
    )
    if len(plant) != 1 or "occupancy stamp" not in plant[0]:
        fails.append(f"the un-witnessed stamp plant did not red: {plant}")
    plant = scan_noop_stamps(
        [
            (
                "planted/allowed.rs",
                "// r34-occupancy: lines were appended this cut (n_appended > 0)\n"
                "fn f(c: &mut C) { c.last_self_activity = now(); }\n",
            )
        ]
    )
    if plant:
        fails.append(f"the witnessed stamp still flagged: {plant}")
    # cfg(test) stamps stay out of the population.
    plant = scan_noop_stamps(
        [
            (
                "planted/gated.rs",
                "#[cfg(test)]\nmod t {\n    fn f(c: &mut C) { c.last_self_activity = now(); }\n}\n",
            )
        ]
    )
    if plant:
        fails.append(f"a cfg(test) stamp entered the census: {plant}")
    # The pending-stamp lane plants: suspended pre-close, RED at the
    # landed verify.
    pf = {"planted/pending.rs": "S9 (plant)"}
    body = [("planted/pending.rs", "fn f(c: &mut C) { c.last_self_activity = now(); }\n")]
    if scan_noop_stamps(body, pending=pf):
        fails.append("a pending-lane stamp red pre-close (must be suspended)")
    got = scan_noop_stamps(body, pending=pf, verify_landed=True)
    if len(got) != 1 or "must be emptied at the wave close" not in got[0]:
        fails.append(f"the pending-lane stamp did not red at the landed verify: {got}")
    # The pending-at-verify plant (the lifecycle's red).
    got = check_rows(
        src_root, {"straw": ("pending", "S9", None, None)}, "straw", verify_landed=True
    )
    if len(got) != 1:
        fails.append(f"the pending-at-verify plant did not red: {got}")
    # The rot plant (a landed row with a dead anchor).
    got = check_rows(
        src_root,
        {"x": ("landed", "S9", "nix/census_corpora.py", r"NO_SUCH_" + r"ANCHOR")},
        "straw",
    )
    if not any("does not resolve" in x for x in got):
        fails.append(f"the dead-anchor plant did not red: {got}")
    # The closed state alphabet.
    got = check_rows(src_root, {"x": ("vibes", "S9", None, None)}, "straw")
    if not any("outside {landed, pending}" in x for x in got):
        fails.append(f"the unknown-state plant did not red: {got}")
    return fails


def _production_files(src_root):
    for crate in census_corpora.jurisdiction_crates(src_root):
        croot = src_root / crate / "src"
        test_files = rust_strip.cfg_test_reachable_files(croot)
        for f in sorted(croot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if f.relative_to(croot).as_posix() in test_files:
                continue
            yield rel, f.read_text(encoding="utf-8")


# Rider (d): K=4 (needles concatenation-split; the shared harness).
MUTATIONS = [
    (
        "stamp-arm-deleted",
        "STAMP_RE narrowed to never match — killed by the"
        " un-witnessed stamp plant",
        "STAMP_RE = re." + r'compile(r"last_self_activity\s*=")',
        "STAMP_RE = re." + r'compile(r"NEVER_MATCHES_ANYTHING\s*=")',
    ),
    (
        "allow-widened",
        "the occupancy-witness window accepts everything — killed by"
        " the un-witnessed stamp plant (it would stop redding)",
        "STAMP_ALLOW = re." + r'compile(r"r34-occupancy:")',
        "STAMP_ALLOW = re." + r'compile(r"")',
    ),
    (
        "rot-check-disabled",
        "landed anchors no longer verified — killed by the dead-anchor"
        " plant",
        "        if not re." + "search(anchor, text):",
        "        if False and not re." + "search(anchor, text):",
    ),
    (
        "population-emptied",
        "the production walk emptied — killed by the rider-(a) floor",
        '        for f in sorted(croot.rglob("*' + '.rs")):',
        "        for f in [" + "]:",
    ),
]


def main() -> int:
    args = sys.argv[1:]
    verify_landed = "--verify-landed" in args
    args = [a for a in args if not a.startswith("--")]
    src_root = pathlib.Path(args[0])

    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1
    battery = self_battery(src_root)
    if battery:
        print("FAIL: cadence/polarity registries self-battery —", file=sys.stderr)
        for x in battery:
            print(f"  {x}", file=sys.stderr)
        return 1
    killed = census_corpora.run_mutation_battery(
        pathlib.Path(__file__), MUTATIONS, "self_battery", (src_root,)
    )
    if killed:
        print("FAIL: cadence/polarity registries K-mutation battery —", file=sys.stderr)
        for x in killed:
            print(f"  {x}", file=sys.stderr)
        return 1

    fails = []
    files = list(_production_files(src_root))
    if not files:
        fails.append("population floor — zero production files ((vvvvv))")
    fails += scan_noop_stamps(files, verify_landed=verify_landed)
    fails += check_rows(src_root, R34_PAIRS, "R34 pair", verify_landed)
    fails += check_rows(src_root, R33_RIDERS, "R33' rider", verify_landed)
    n34p = sum(1 for r in R34_PAIRS.values() if r[0] == "pending")
    n33p = sum(1 for r in R33_RIDERS.values() if r[0] == "pending")
    print(
        f"cadence/polarity registries: {len(R34_PAIRS)} R34 (periodic-event, "
        f"bound) pairs ({n34p} pending slot landings), {len(R33_RIDERS)} R33' "
        f"polarity/units riders ({n33p} pending; the wave-close "
        f"--verify-landed flips them), the R34(ii) no-op stamp grammar live"
    )
    if fails:
        print("FAIL: cadence/polarity registry violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
