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
        "landed",
        "S5",
        "rio-store/src/logs/service.rs",
        r"idle_trip_worst_case",
        "(log cut interval, INBOUND_IDLE_BOUND) — the bug_018 banner pair",
        "compile-assert (the PD-4 expanded form: idle_trip_worst_case(interval) + PHASE_MARGIN <= IDLE_TRIP_DISCLOSED_CEILING, plus the MAX_LOG_CUT_INTERVAL round-trip asserts; config validate consumes the same fn — H5 item 4)",
    ),
    "udev-settle-vs-condition-clock": (
        "landed",
        "S2",
        "nix/nixos-node/eks-node.nix",
        r"rio-kubelet-mount",
        "(udev coldplug settle, systemd job-start Condition evaluation) — merged_bug_045's wrong-clock gate",
        "structural (the R34(iii) systemd tier): ONE condition-free rio-kubelet-mount oneshot settles FIRST then classifies on the settled view; vm-nixos-node asserts the rendered unit is condition-free + settle-precedes-glob (H2: the assert is the unit's shape, not a const pair)",
    ),
    "scaler-funding-vs-sensor-cadence": (
        "landed",
        "S6",
        "rio-controller/src/reconcilers/componentscaler/decide.rs",
        r"RESET regardless of sensor availability",
        "(scaler decide tick, sensor-reading presence) — merged_bug_009's sensor-absent funding clock",
        "structural (the R34(iii) sensor tier): the streak/bank predicates evaluate on sensor-absent ticks (the hoist — banked evidence resets regardless of sensor availability; ratio-growth funding demands total coverage)",
    ),
    "session-drain-stamp-vs-stale-after": (
        "landed",
        "S5",
        "rio-store/src/logs/sessions.rs",
        r"worst_one_miss_committed_age",
        "(drain heartbeat stamp cadence, SESSION_STALE_AFTER) — the F10/WO-S5-7 pair",
        "compile-assert SATISFIED via the existing sessions.rs margin certificate (worst committed-stamp age 2I+F+R < SESSION_STALE_AFTER; the pair is REUSED at the drain's second spawn site, no new coupling minted — H5 item 2; F10 closed ARM A, the per-completed-chunk cadence rejected per CE-3)",
    ),
}

# --- R34-w: the recovery-edge axis (round-14 WO-S9-2) ------------------
# The R34 census above records what REFRESHES each gate's clock; this
# axis records what CLEARS each degradation/recovery/disarm clock and
# asserts the clearing event is in the witnessed-work class — never
# open/connect/re-mint/retried-attempt (the merged_bug_003 perimeter
# leak: an episode-ending event that can occur while the failure
# persists is NOT recovery evidence; a peer that delivers refusals
# in-band reset the clock every second so the 30s notice never fired).
#
# name -> (state, slot, file, anchor_regex, clearing_event, witness_class)
#   witness_class: (class, rationale) where class is in WITNESSED_WORK
#   for the recovery-edge axis to GREEN. The REJECTED set below names
#   every member of R34-w's enumerated no-progress class (per R16, the
#   plant battery covers each one — WS-I2).
WITNESSED_WORK = ("relayed-line", "committed-row", "completed-unit", "OTHER")
REJECTED_CLEARS = ("open", "connect", "re-mint", "retried-attempt")
# Founding enrollments arrive with their slots (S1's occupancy-keyed
# live-tail clear; S2's in-process fence-arming record) and are
# reconciled at the wave-close re-mint with anchors at the landed tree.
R34_CLEARS = {
}


def check_clears(src_root, rows, verify_landed=False):
    """R34-w(iv) recovery-edge check (failure-collecting): every
    enrolled disarm/recovery clock names its clearing event in the
    witnessed-work class. Reuses check_rows for state/anchor; adds the
    witness-class arm. NO population floor — the axis is empty at
    birth; enrollment is per-slot."""
    fails = check_rows(src_root, rows, "R34-w clear", verify_landed) if rows else []
    for name, row in sorted(rows.items()):
        cls, rationale = row[5]
        if cls in REJECTED_CLEARS:
            fails.append(
                f"R34-w clear `{name}`: clearing event class `{cls}` is in "
                f"the REJECTED no-progress set {REJECTED_CLEARS} — an "
                f"event that can occur while the failure persists "
                f"(successful open against a refusing peer; a re-mint the "
                f"verifier rejects again; a retried call that observed "
                f"nothing) is NOT recovery evidence (R34-w(i): the "
                f"merged_bug_003 defeat)"
            )
        elif cls not in WITNESSED_WORK:
            fails.append(
                f"R34-w clear `{name}`: clearing event class `{cls}` "
                f"outside the closed witnessed-work alphabet "
                f"{WITNESSED_WORK} — name the productive outcome that "
                f"proves the recovery actually recovered (first relayed "
                f"chunk, the committed row at the success site, the "
                f"completed unit of the thing previously failing)"
            )
        if cls == "OTHER" and not rationale:
            fails.append(
                f"R34-w clear `{name}`: witness class OTHER without a "
                f"recorded rationale (R34-w(i))"
            )
    return fails


# --- R33': the polarity/units rider registry ----------------------------
# name -> (state, slot, file, anchor_regex, readers, units, tier)
#   readers: "<reader>: <direction>" semicolon list — the per-reader
#   direction table merged_bug_002's prose census lacked.
R33_RIDERS = {
    "disk-p90-raw-vs-floored": (
        "landed",
        "S4",
        "rio-scheduler/src/sla/types.rs",
        r"struct RawDiskP90",
        "reject/explain/classify_ceiling: raw (RawDiskP90); sizing: floored (DiskBytes on disk_p90)",
        "bytes (p90 of per-build peak); fork minted once as ingest::DiskP90Fork{raw, floored}",
        "TIER VERDICT: TYPE-BLOCKED (H4 — the RawDiskP90 newtype lands at consumer signatures; exceeds_ceiling re-signed; re-conflation fails to type-check; no census-tier withdrawal needed)",
    ),
    "disk-evidence-peak-vs-status": (
        "landed",
        "S2",
        "rio-builder/src/executor/mod.rs",
        r"struct DiskEvidence",
        "sizing: monitor peak (sizing_peak max-fold); classification: one-shot status (classification)",
        "bytes vs typed QuotaStatus (DiskEvidence{sizing_peak, classification} + fold_disk_evidence — total, 4 cells; absence consumes the FOLD OUTPUT)",
        "type-split product (H2): two fields, two consumers; the kubelet -1 sentinel finding rides the H2 pack as a round-14 candidate",
    ),
    "weight-ring-vs-full-slice": (
        "landed",
        "S4",
        "rio-scheduler/src/sla/ingest.rs",
        r"fn axis_samples",
        "fit ordinals: ring weights (reserved to the fit's subset universe); aggregate/anchor: full-slice ordinals derived in-body",
        "ordinal weights — ONE producer (axis_samples takes no weight parameter; callers cannot pass a mismatched domain)",
        "producer-signature enforcement (H4: the one-ordinal-domain signature; the w12_ad consult upgraded to domain-checked)",
    ),
    "pod-ephemeral-vs-solve-disk": (
        "landed",
        "S2",
        "rio-common/src/k8s.rs",
        r"pod_ephemeral_request_bytes",
        "pod request: pod-ephemeral units; corroboration band: solve-disk units — both denominate through the ONE shared producer",
        "bytes via rio_common::k8s::{overlay_size_limit_bytes, pod_ephemeral_request_bytes} (the OQ-3 rio-common arm FIRED; controller jobs.rs delegates; scheduler floor.rs band consumes the same fns + headroom codomain consts)",
        "shared-minting-fn enforcement (H2; compile-time const asserts both sides pin the fallback inside the band)",
    ),
}

# The no-op stamp grammar (R34(ii)): occupancy-stamp writes need an
# occupancy witness comment naming the outcome evidence.
STAMP_RE = re.compile(r"last_self_activity\s*=")
# The witness alphabet (R34(ii)): an explicit `r34-occupancy:` comment,
# OR the landed type-derived witnesses — the `CutOccupancy::Occupied`
# guard (the occupancy verdict's own type, S5's WO-S5-1 shape) and the
# `cut_while_due(` work call (the in-arm cut IS the occupancy; cut_due
# implies work per the H5 pack). A future bare stamp with none of
# these in its window reds.
STAMP_ALLOW = re.compile(r"r34-occupancy:|CutOccupancy::Occupied|cut_while_due\(")
# Stamp FILES pending their owning slot's landing. EMPTIED at the
# round-13 wave close: S5's WO-S5-1 landed the occupancy repair —
# the cut-arm stamp is now guarded by `CutOccupancy::Occupied` (the
# type's own verdict) and the in-arm batch stamp by the `cut_while_due`
# work call (cut_due implies work, H5 item 3) — both recognized by
# STAMP_ALLOW as DERIVED witnesses, so the lane has nothing left to
# suspend. The lane mechanism stays (a future slot may need it);
# adding an entry requires editing this reviewed table.
STAMP_PENDING = {}


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
            window = "\n".join(lines[max(0, lineno - 6) : lineno])
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
    # R34-w plants — the W14-I2 battery (every face of the law's
    # rejected-class enumeration, per R16/WS-I2): one planted clock row
    # for EACH named no-progress event; all four red.
    for rej in REJECTED_CLEARS:
        got = check_clears(
            src_root,
            {"x": ("pending", "S9", None, None, f"on-{rej}", (rej, ""))},
        )
        if not any("REJECTED no-progress set" in x for x in got):
            fails.append(f"the rejected-clear plant `{rej}` did not red: {got}")
    # An unknown clearing class reds (closed alphabet).
    got = check_clears(
        src_root,
        {"x": ("pending", "S9", None, None, "on-vibes", ("vibes", ""))},
    )
    if not any("outside the closed witnessed-work alphabet" in x for x in got):
        fails.append(f"the unknown-clear-class plant did not red: {got}")
    # OTHER without rationale reds.
    got = check_clears(
        src_root,
        {"x": ("pending", "S9", None, None, "on-x", ("OTHER", ""))},
    )
    if not any("OTHER without a recorded rationale" in x for x in got):
        fails.append(f"the clear-OTHER-no-rationale plant did not red: {got}")
    # The founding-shape green: a witnessed-work clearing event is NOT
    # flagged (the W14-I2 founding-enrollment arm).
    got = check_clears(
        src_root,
        {
            "x": (
                "pending",
                "S1",
                None,
                None,
                "first relayed chunk on the live tail",
                ("relayed-line", "the occupancy-keyed clear: episode ends on the first chunk DELIVERED, not on open"),
            )
        },
    )
    if got:
        fails.append(f"the witnessed-work founding shape FALSELY flagged: {got}")
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
        "STAMP_ALLOW = re." + r'compile(r"r34-occupancy:|CutOccupancy::Occupied|cut_while_due\(")',
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
    (
        "rejected-clear-set-emptied",
        "the R34-w rejected no-progress set emptied — killed by the"
        " four rejected-clear plants (open/connect/re-mint/retried-"
        "attempt would each stop redding)",
        "        if cls in " + "REJECTED_CLEARS:",
        "        if cls in (" + "):",
    ),
    (
        "witnessed-work-widened",
        "the closed witnessed-work alphabet accepts anything — killed"
        " by the unknown-clear-class plant",
        "        elif cls not in " + "WITNESSED_WORK:",
        "        elif False and cls not in " + "WITNESSED_WORK:",
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
    fails += check_clears(src_root, R34_CLEARS, verify_landed)
    fails += check_rows(src_root, R33_RIDERS, "R33' rider", verify_landed)
    n34p = sum(1 for r in R34_PAIRS.values() if r[0] == "pending")
    n33p = sum(1 for r in R33_RIDERS.values() if r[0] == "pending")
    nclr = sum(1 for r in R34_CLEARS.values() if r[0] == "pending")
    print(
        f"cadence/polarity registries: {len(R34_PAIRS)} R34 (periodic-event, "
        f"bound) pairs ({n34p} pending slot landings), {len(R34_CLEARS)} R34-w "
        f"recovery-edge clears ({nclr} pending; clearing events in the "
        f"witnessed-work class), {len(R33_RIDERS)} R33' polarity/units riders "
        f"({n33p} pending; the wave-close --verify-landed flips them), the "
        f"R34(ii) no-op stamp grammar live"
    )
    if fails:
        print("FAIL: cadence/polarity registry violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
