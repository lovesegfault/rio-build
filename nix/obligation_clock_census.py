#!/usr/bin/env python3
"""The R32 obligation census + the R29' gate-clock census (WO-S8-14(ii)).

Argv: <src-root> [--mint-clock-grandfather] [--verify-landed].

R32 (round-12 banner): acks, trims, deletes, retransmit copies, and
disclosure duties are LINEAR RESOURCES — typed move semantics,
consume-exactly-once, droppable only through explicit typed discharge;
advisory-data obligations reject. R29' (the R29 generalization): every
production envelope/gate names its clock AND the clock is the
EVIDENCE'S OWN — arrival not read, consult not mint — production tiers
included.

This census is the STANDING REGISTRY for both:

  (a) OBLIGATION_ROWS — every linear-obligation type registers its
      discharge surface and its Drop-enforcement audit state (the
      bug_168 mode: destructor-hosted enforcement carries the
      thread::panicking() gate). The round-12 types land with their
      slots (S1/S3/S4/S6/S7); a row is `pending:<slot>` until its
      landing, then flips to anchored (`--verify-landed`, the
      wave-close duty) — an anchored row whose type or discharge
      anchor stops resolving is rot (red), and a row still pending at
      the wave-close verify is a red (the close flips it or fails).
      The `state.take()`-then-await-in-abortable-tasks lint candidate
      is RECORDED with its corpus row (S7's banner WO is the corpus;
      promotion trigger: the first post-wave take-then-await escape).
  (b) CLOCK_ROWS — the enrolled R29' instances, same pending/anchored
      lifecycle. Plus TWO standing code grammars enforced from birth:
      the LOSSY-WITNESS-ARITHMETIC flag (`as_secs()`-then-multiply —
      witness arithmetic is lossless in the finest unit any input
      const can carry, bug_151's class; sites carry an
      `r29-lossless:` witness comment or ride the shrink-only
      grandfather), and the UN-NAMED-GATE finder (production
      `.elapsed() >`-class comparisons must be census-rowed or
      grandfathered — the standing debt visible, never silent).

Self-test arms run first (the house pattern), restructured at
WO-S9-1 into the R31' riders-(a)-(d) form:

  - self_battery(src_root) — ALL plant arms, failure-collecting
    (never early-return, so every seeded mutation reliably produces
    at least one battery failure regardless of arm order): the
    rider-(a) walk floor driven through the SAME production walk;
    the per-alternation-arm grammar plants (one per GATE_RE arm, one
    per LOSSY_RE arm — rider (c)); the IN-POPULATION planted
    DUPLICATES for BOTH lanes (rels are real grandfathered files, so
    the plants sit exactly where the bug_047 masking occurred); the
    excess arm (an uncensused hit against an empty grandfather must
    red); the W13-AX fix-one-of-a-pair stale-sweep arm.
  - mutation_battery(src_root) — the rider-(d) K-SEEDED-MUTATION
    harness (K=5, the standing template WO-S9-8 generalizes): each
    mutation is a committed fixture (name, old, new) over THIS
    file's own source; the harness asserts the target text exists,
    applies it to a COPY, execs the mutant, runs the MUTANT'S OWN
    self_battery, and requires it to FAIL — a mutant whose battery
    stays green means the planted red survived its artifact's
    degeneration: the born-broken bug_047 criterion, enforced at
    every run. Recursion is grounded at the fixture tier (W13-BE):
    self_battery never invokes mutation_battery, so the mutant runs
    plants only.
"""

import pathlib
import re
import sys
from collections import Counter

import census_corpora
import rust_strip

# name -> (kind, slot, type token, discharge anchor regex, drop-audit)
# pending rows carry None anchors; --verify-landed (the wave-close
# pass) requires every row anchored and resolving.
# Landed rows carry (state, slot, file, anchor-regex, drop-audit);
# check_landed resolves every anchor in its file — rot reds. Flipped
# at the wave-close --verify-landed pass (bw12, dfd3afb2b+19): each
# anchor grep-verified at the composed tree before the flip; the
# RetransmitBuffer NAME from the relay pack resolved by CONTENT per
# (xxxxx) — the landed discharge is `discharge_through(DurableFrontier)`
# on the upload buffer (log_upload.rs:1059), the witness TYPE is
# DurableFrontier (ack-borne mints only).
OBLIGATION_ROWS = {
    "retransmit-buffer": ("landed", "S1", "rio-builder/src/log_upload.rs", r"fn discharge_through\(&mut self, frontier: DurableFrontier\)", "n/a"),
    "lease-release-guard": ("landed", "S1", "rio-store/src/logs/service.rs", r"LeaseReleaseGuard", "panicking-gated"),
    "batch-authority": ("landed", "S3", "rio-store/src/gc/lane.rs", r"BatchAuthority|authorize_batch", "n/a"),
    "routing-verdict": ("landed", "S4", "rio-lease/src/lib.rs", r"RoutingVerdict", "n/a"),
    "tombstone-disposition": ("landed", "S6", "rio-controller/src/reconcilers/nodeclaim_pool/health.rs", r"TombstoneDisposition", "n/a"),
    "disclosure-guard": ("landed", "S7", "rio-gateway/src/handler/log_tail.rs", r"PendingGapCell", "panicking-gated"),
    "exit-discharge": ("landed", "S7", "rio-log-kernel/src/lib.rs", r"Exit \{", "n/a"),
}
TAKE_THEN_AWAIT_CANDIDATE = (
    "lint candidate: `state.take()` followed by `.await` inside an "
    "abortable task — corpus: WO-S7-1's take-then-await windows; "
    "promotion trigger: the first post-wave escape of this shape"
)

# Landed clock rows: (state, slot, file, anchor-regex) — the clock
# description lives in the anchor's own doc at the landed site.
CLOCK_ROWS = {
    "inbound-idle-arrival": ("landed", "S1", "rio-store/src/logs/service.rs", r"last_inbound|INBOUND_IDLE"),
    "session-margin-schedule": ("landed", "S1", "rio-store/src/logs/sessions.rs", r"FAST_RETRY_BUDGET|TICK_BODY_BOUND"),
    "infra-poison-consecutive": ("landed", "S2", "rio-scheduler/src/retry_policy.rs", r"consecutive|infra"),
    "gc-clearance-consult-age": ("landed", "S3", "rio-store/src/gc/lane.rs", r"regate|HoldClearance"),
    "lease-futility-next-eval": ("landed", "S4", "rio-lease/src/lib.rs", r"BlindClock|futility|next_eval"),
    "scaler-fund-eq-spend": ("landed", "S6", "rio-controller/src/reconcilers/componentscaler/decide.rs", r"fund|streak|sustain"),
    "gap-witness-lossless": ("landed", "S7", "rio-common/src/liveness.rs", r"admin_verify_worst_emission_gap"),
    # live_062 WO-S10-6 (bw13-S10): the tail degraded-notice gate —
    # deadline-shaped on the episode's own arming stamp (armed +
    # TAIL_DEGRADED_NOTICE_AFTER vs Instant::now(), the grace_deadline
    # idiom; same Instant domain). The clock description lives in the
    # anchor's doc per the row convention.
    "tail-degraded-notice": ("landed", "S10", "rio-gateway/src/handler/log_tail.rs", r"TAIL_DEGRADED_NOTICE_AFTER"),
    # live_064 WO-S6-4 (bw14-S6): the rejection-warn burst gate —
    # deadline-shaped on the interceptor's own monotonic Instant (now
    # vs the warn_not_before deadline minted as now + WINDOW at warn
    # time; same Instant domain, no elapsed-comparison). Log-cardinality
    # bound only — the metric is the durable evidence; NOT a
    # degradation/recovery clock (R34-w does not apply — the warn never
    # CLEARS anything).
    "auth-rejection-warn-burst": ("landed", "S6", "rio-auth/src/jwt_interceptor.rs", r"REJECTION_WARN_BURST_WINDOW"),
    # sh-037 S11 (LIVE_STRIKES): the live-observation wall-floor gate —
    # deadline-shaped on the ledger row's own monotonic Instant (now −
    # stamp vs STRIKE_WALL_FLOOR; same Instant domain as
    # note_live_strike, the StrikeEntry::expired idiom). The clock
    # description lives in the anchor's doc per the row convention.
    "live-strike-wall-floor": ("landed", "S11", "rio-controller/src/reconcilers/pool/jobs.rs", r"STRIKE_WALL_FLOOR"),
}

# The lossy-witness-arithmetic grammar (live from birth): seconds
# truncation followed by scaling — `.as_secs() * K` / `K * x.as_secs()`
# — downcasts toward green before multiplying (bug_151's class). A
# lawful site carries `r29-lossless:` within 3 lines above.
LOSSY_RE = re.compile(r"\.as_secs\(\)\s*\*|\*\s*\w+\.as_secs\(\)")
LOSSY_ALLOW = re.compile(r"r29-lossless:")
# The un-named-gate finder: production elapsed/age comparisons.
GATE_RE = re.compile(r"\.elapsed\(\)\s*(?:[<>]=?|\.as_secs\(\)\s*[<>]=?)")
CLOCK_GRANDFATHER = "nix/gate-clock-grandfather.txt"


def production_files(src_root):
    for crate in census_corpora.jurisdiction_crates(src_root):
        croot = src_root / crate / "src"
        test_files = rust_strip.cfg_test_reachable_files(croot)
        for f in sorted(croot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if f.relative_to(croot).as_posix() in test_files:
                continue
            yield rel, f.read_text(encoding="utf-8")


def scan_clock_code(files):
    """[(key, message)] for lossy-arithmetic and un-named-gate hits.

    Keys are CONTENT-BEARING at the granularity of the excepted SITE
    (R31'(i), the bug_047 repair): the trimmed source line joins the
    key through the ONE shared projection
    (census_corpora.content_key) — in BOTH lanes. The retired
    projection keyed on the regex match fragment (`m.group(0)`),
    which carries zero site-identifying content for GATE_RE and for
    LOSSY_RE's first alternation arm, quotienting every same-operator
    site in a file into one grandfather class — a fixed site never
    tripped the stale sweep and a new site stayed green."""
    hits = []
    for rel, raw in files:
        try:
            pruned = rust_strip.strip_cfg_test(raw, source=rel)
        except rust_strip.StripError:
            pruned = raw
        lexed, _ = rust_strip.lex(pruned, blank_string_bodies=True)
        lines = raw.splitlines()
        for m in LOSSY_RE.finditer(lexed):
            lineno = lexed.count("\n", 0, m.start()) + 1
            window = "\n".join(lines[max(0, lineno - 4) : lineno])
            if LOSSY_ALLOW.search(window):
                continue
            # R31'(i): the site's own line, never the bare fragment —
            # the lossy lane is lane-explicit (the `.as_secs() *` arm
            # matched a contentless fragment; the `* CONST.as_secs()`
            # arm quotiented same-const multiplies).
            key = census_corpora.content_key(rel, "lossy", lines[lineno - 1])
            hits.append(
                (
                    key,
                    f"{rel}:{lineno}: seconds-truncated witness arithmetic "
                    f"(`{m.group(0).strip()}`) — witness math is lossless in the "
                    f"finest input unit (R29'/bug_151); convert in millis/nanos "
                    f"or carry an `r29-lossless:` witness comment",
                )
            )
        for m in GATE_RE.finditer(lexed):
            lineno = lexed.count("\n", 0, m.start()) + 1
            # R31'(i): site-content keying (the gate lane's mint).
            key = census_corpora.content_key(rel, "gate", lines[lineno - 1])
            hits.append(
                (
                    key,
                    f"{rel}:{lineno}: production elapsed-comparison gate without a "
                    f"clock census row — name its clock in CLOCK_ROWS (R29': the "
                    f"clock must be the evidence's own) or it rides the "
                    f"shrink-only grandfather as visible debt",
                )
            )
    return hits


def grandfather_diff(hits, gf_counts):
    """(excess_msgs, stale_rows) of live `hits` against the MULTISET
    grandfather `gf_counts` (Counter of key -> recorded count).

    Shrink-only made literal (R31'(i) count-bearing semantics layered
    over content keys): a key's live count above its recorded count
    fails the excess hits (new sites are never silently absorbed by a
    same-content row); a live count below the recorded count is a
    stale row (the fixed site's debt entry must be removed). The
    identical-line corner — two sites whose trimmed source lines are
    byte-equal — is therefore still discriminated by COUNT even
    though content alone cannot split them."""
    live_by_key = {}
    for k, m in hits:
        live_by_key.setdefault(k, []).append(m)
    excess = []
    for k in sorted(live_by_key):
        msgs = live_by_key[k]
        over = len(msgs) - gf_counts.get(k, 0)
        if over > 0:
            excess.extend(msgs[-over:])
    stale = []
    for k in sorted(gf_counts):
        deficit = gf_counts[k] - len(live_by_key.get(k, []))
        if deficit > 0:
            stale.append((k, deficit))
    return excess, stale


def check_landed(src_root, rows, kind):
    """--verify-landed: every row anchored AND its anchor resolving in
    its file (rot reds; a pending row here is a red — the wave-close
    flips it or fails)."""
    fails = []
    for name, row in sorted(rows.items()):
        state = row[0]
        if state == "pending":
            fails.append(
                f"{kind} row `{name}` still pending:{row[1]} at the landed "
                f"verify — the wave-close flips it with its anchors or the "
                f"close fails"
            )
            continue
        rel, anchor = row[2], row[3]
        f = src_root / rel
        text = f.read_text(encoding="utf-8") if f.is_file() else ""
        if not re.search(anchor, text):
            fails.append(
                f"{kind} row `{name}`: anchor /{anchor}/ does not resolve in "
                f"{rel} — the landed construction moved or rotted (re-derive)"
            )
    return fails


def self_battery(src_root) -> list:
    """ALL plant arms, failure-collecting (rider (a)+(b)+(c) of the
    R31' census riders; WO-S9-1 commit 2). Returns a list of failure
    strings — empty means every plant red where it must and stayed
    quiet where it must. NEVER early-returns: each arm appends, so a
    seeded mutation of any single control-flow site still surfaces
    through its oracle arm (mutation_battery requires a NON-empty
    return from a mutant's battery).

    Recursion grounding (W13-BE): this fn never invokes
    mutation_battery — a mutant exec'd by the harness runs plants
    only."""
    fails = []

    # Rider (a) — the walk floor, driven through the same walk path
    # as production, plus the expected-member pins (two charged
    # bug_047 population files; both stable production surfaces — if
    # one moves, its grandfather row stales in the same run, so the
    # coupling is coherent, never a stranded assert).
    walked = [rel for rel, _raw in production_files(src_root)]
    if not walked:
        fails.append(
            "rider (a): population floor — the production walk yielded "
            "zero files ((vvvvv): mis-staged tree or emptied walk)"
        )
    for expected in (
        "rio-builder/src/log_stream.rs",
        "rio-store/src/logs/sessions.rs",
    ):
        if walked and expected not in walked:
            fails.append(
                f"rider (a): expected member {expected} absent from the "
                f"production walk — the walk or the population rotted"
            )

    # Rider (c) — one plant per LOSSY_RE alternation arm.
    # Arm 1: `.as_secs() *` (the contentless-fragment shape).
    plant = scan_clock_code(
        [("planted/lossy.rs", "fn f(d: Duration) -> u64 { d.as_secs() * 1000 }\n")]
    )
    if len(plant) != 1 or "lossless" not in plant[0][1]:
        fails.append(f"the lossy-arithmetic arm-1 plant did not red: {plant}")
    # Arm 2: `* CONST.as_secs()` (the const-naming shape).
    plant = scan_clock_code(
        [("planted/lossy2.rs", "fn f() -> u64 { 2 * HEARTBEAT_INTERVAL.as_secs() }\n")]
    )
    if len(plant) != 1 or "lossless" not in plant[0][1]:
        fails.append(f"the lossy-arithmetic arm-2 plant did not red: {plant}")
    # The witnessed site stays quiet.
    plant = scan_clock_code(
        [
            (
                "planted/allowed.rs",
                "// r29-lossless: millis would overflow the wire u32; bound proven\n"
                "fn f(d: Duration) -> u64 { d.as_secs() * 1000 }\n",
            )
        ]
    )
    if plant:
        fails.append(f"the witnessed lossy site still flagged: {plant}")

    # Rider (c) — one plant per GATE_RE alternation arm.
    # Arm 1: bare `.elapsed() <op>`.
    plant = scan_clock_code(
        [("planted/gate.rs", "fn g(t: Instant) -> bool { t.elapsed() > LIMIT }\n")]
    )
    if len(plant) != 1 or "clock census row" not in plant[0][1]:
        fails.append(f"the un-named-gate arm-1 plant did not red: {plant}")
    # Arm 2: `.elapsed().as_secs() <op>`.
    plant = scan_clock_code(
        [
            (
                "planted/gate2.rs",
                "fn g(t: Instant) -> bool { t.elapsed().as_secs() >= LIMIT_SECS }\n",
            )
        ]
    )
    if len(plant) != 1 or "clock census row" not in plant[0][1]:
        fails.append(f"the un-named-gate arm-2 plant did not red: {plant}")

    # The pending-at-verify plant.
    f_l = check_landed(src_root, {"straw": ("pending", "S9")}, "obligation")
    if len(f_l) != 1:
        fails.append(f"the pending-at-verify plant did not red: {f_l}")

    # The EXCESS arm (rider (b) enrollment face): an uncensused hit
    # against an empty grandfather must fail as excess — a widened
    # (superset-accepting) enforcement set dies here.
    exc, _st = grandfather_diff(
        scan_clock_code(
            [("planted/gate.rs", "fn g(t: Instant) -> bool { t.elapsed() > LIMIT }\n")]
        ),
        Counter(),
    )
    if len(exc) != 1:
        fails.append(
            f"the excess arm did not red: an uncensused gate against an "
            f"empty grandfather yielded {len(exc)} excess fail(s), want 1"
        )

    # The IN-POPULATION planted DUPLICATES, BOTH lanes (R31'(iii):
    # the plant lives inside the grandfathered population, never only
    # in a clean file — the rels below are real grandfathered files,
    # exactly where the bug_047 masking occurred). The lossy plants
    # are that lane's only machine oracle today: each grandfathered
    # lossy file carries exactly one live site (pull.rs:118,
    # lib.rs:282, sessions.rs:125, common.rs:79), so the live-tree
    # collision census is vacuously green for the lossy lane
    # regardless of key degeneracy.
    # (gate) two same-operator gates in one grandfathered file.
    dup = [
        k
        for k, _m in scan_clock_code(
            [
                (
                    "rio-builder/src/log_stream.rs",
                    "fn a(t: Instant) -> bool { t.elapsed() >= WINDOW }\n"
                    "fn b(u: Instant) -> bool { u.elapsed() >= BATCH_TIMEOUT }\n",
                )
            ]
        )
    ]
    if len(set(dup)) != 2:
        fails.append(
            f"W13-AX2 (gate lane): the in-population same-operator pair "
            f"minted {len(set(dup))} distinct key(s), want 2"
        )
    # (lossy, arm 1) two same-fragment `.as_secs() *` multiplies in
    # one grandfathered file.
    dup = [
        k
        for k, _m in scan_clock_code(
            [
                (
                    "rio-store/src/grpc/put_path/common.rs",
                    "fn a(d: Duration) -> u64 { d.as_secs() * 1000 }\n"
                    "fn b(e: Duration) -> u64 { e.as_secs() * 1000 }\n",
                )
            ]
        )
    ]
    if len(set(dup)) != 2:
        fails.append(
            f"W13-AX2 (lossy lane, arm 1): the in-population same-fragment "
            f"pair minted {len(set(dup))} distinct key(s), want 2"
        )
    # (lossy, arm 2) two multiplies of the same const in one
    # grandfathered file — the sessions.rs-shaped face.
    dup = [
        k
        for k, _m in scan_clock_code(
            [
                (
                    "rio-store/src/logs/sessions.rs",
                    "fn a() -> u64 { 2 * HEARTBEAT_INTERVAL.as_secs() }\n"
                    "fn b() -> u64 { 3 * HEARTBEAT_INTERVAL.as_secs() }\n",
                )
            ]
        )
    ]
    if len(set(dup)) != 2:
        fails.append(
            f"W13-AX2 (lossy lane, arm 2): the same-const multiply pair "
            f"minted {len(set(dup))} distinct key(s), want 2"
        )

    # W13-AX (bug_047's defeat, reproduced then killed): grandfather
    # the gate pair, FIX one gate — the stale sweep MUST trip for the
    # fixed site's row. Under the retired file×operator projection
    # the surviving gate held the shared key live and the sweep
    # stayed silent (the pre-fix red, verbatim in the landing commit
    # body).
    pair_rel = "rio-builder/src/log_stream.rs"
    pair = (
        "fn a(t: Instant) -> bool { t.elapsed() >= WINDOW }\n"
        "fn b(u: Instant) -> bool { u.elapsed() >= BATCH_TIMEOUT }\n"
    )
    fixed = "fn b(u: Instant) -> bool { u.elapsed() >= BATCH_TIMEOUT }\n"
    gf_ax = Counter(k for k, _m in scan_clock_code([(pair_rel, pair)]))
    ax_excess, ax_stale = grandfather_diff(scan_clock_code([(pair_rel, fixed)]), gf_ax)
    if ax_excess or len(ax_stale) != 1:
        fails.append(
            f"W13-AX: fixing one gate of the grandfathered pair must trip "
            f"the stale sweep exactly once (excess={ax_excess}, "
            f"stale={ax_stale})"
        )
    # The fix-plus-add face: one grandfathered gate FIXED and one NEW
    # same-operator gate added in the same file — under content keys
    # this is 1 stale + 1 excess; under a fragment key layered on
    # count-bearing comparison the two events CANCEL (count
    # unchanged) and both stay invisible. Pins that content keying is
    # load-bearing beyond what counts alone can discriminate.
    swapped = (
        "fn b(u: Instant) -> bool { u.elapsed() >= BATCH_TIMEOUT }\n"
        "fn c(v: Instant) -> bool { v.elapsed() >= FLUSH_DEADLINE }\n"
    )
    sw_excess, sw_stale = grandfather_diff(scan_clock_code([(pair_rel, swapped)]), gf_ax)
    if len(sw_excess) != 1 or len(sw_stale) != 1:
        fails.append(
            f"W13-AX (fix-plus-add face): one fixed + one new same-operator "
            f"gate must yield exactly 1 excess + 1 stale "
            f"(excess={len(sw_excess)}, stale={len(sw_stale)})"
        )
    return fails


# The rider-(d) K-mutation fixtures (W13-AX3; K=5 — the WO-S9-1
# battery, the standing template WO-S9-8's framework registry
# enrolls): committed (name, oracle, old, new) source substitutions
# over THIS artifact. Each mutation degrades one control-flow site
# the bug_047 class rides on; the harness requires the mutant's OWN
# self_battery to FAIL (the planted red must die under the
# mutation). A mutation whose `old` no longer matches EXACTLY ONCE is
# harness rot and fails loudly — the fixtures pin the artifact's
# load-bearing lines. The `old`/`new` literals are built by
# CONCATENATION (the census_corpora SHADOW_STRIPPER precedent) so
# this table's own source never matches the needles it pins.
MUTATIONS = [
    (
        "key-degenerate-gate",
        "the gate mint swapped back to the born-broken file×operator"
        " fragment — killed by the gate-lane in-population duplicate"
        " (W13-AX2) and the fix-plus-add cancellation face",
        "key = census_corpora.content_" + 'key(rel, "gate", lines[lineno - 1])',
        'key = f"{rel}' + "\\tgate\\tL-content-{' '.join(m.group(0).split())}\"",
    ),
    (
        "lossy-mint-bypasses-helper",
        "the lossy mint degraded to the m.group(0) fragment, BYPASSING"
        " the shared helper — the per-lane miswire the gate-side"
        " projection-swap cannot see; killed by the lossy in-population"
        " duplicates (both arms)",
        "key = census_corpora.content_" + 'key(rel, "lossy", lines[lineno - 1])',
        'key = f"{rel}' + "\\tlossy\\t{' '.join(m.group(0).split())}\"",
    ),
    (
        "enforcement-superset",
        "the enforcement set widened to superset-accept (an unknown key"
        " defaults to its own live count) — killed by the excess arm",
        "over = len(msgs) - gf_counts." + "get(k, 0)",
        "over = len(msgs) - gf_counts." + "get(k, len(msgs))",
    ),
    (
        "population-walk-emptied",
        "the production walk emptied/rerooted — killed by the rider-(a)"
        " walk floor (driven through the same walk path)",
        "for f in sorted(croot." + 'rglob("*.rs")):',
        "for f in [" + "]:",
    ),
    (
        "gate-arm-deleted",
        "GATE_RE's `.as_secs()` alternation arm deleted — killed by the"
        " per-arm gate-2 plant",
        "GATE_RE = re." + r'compile(r"\.elapsed\(\)\s*(?:[<>]=?|\.as_secs\(\)\s*[<>]=?)")',
        "GATE_RE = re." + r'compile(r"\.elapsed\(\)\s*(?:[<>]=?)")',
    ),
]


def mutation_battery(src_root) -> list:
    """Rider (d): exec a COPY of this artifact under each committed
    MUTATION and require the mutant's self_battery to FAIL. A mutant
    whose battery stays green is the born-broken verdict — the
    plants cannot detect that degeneration of the artifact, which is
    exactly how bug_047 shipped."""
    fails = []
    src = pathlib.Path(__file__).read_text(encoding="utf-8")
    for name, oracle, old, new in MUTATIONS:
        n = src.count(old)
        if n != 1:
            fails.append(
                f"K-mutation `{name}`: target text matched {n} time(s), "
                f"want exactly 1 — the fixture rotted against the artifact "
                f"(re-pin the mutation to the load-bearing line)"
            )
            continue
        ns = {
            "__name__": f"obligation_clock_census_mutant_{name.replace('-', '_')}",
            "__file__": str(pathlib.Path(__file__)),
        }
        exec(compile(src.replace(old, new, 1), f"<mutant:{name}>", "exec"), ns)
        mutant_fails = ns["self_battery"](src_root)
        if not mutant_fails:
            fails.append(
                f"K-mutation `{name}` NOT killed: the mutant's self_battery "
                f"stayed green — the planted red survived its artifact's "
                f"degeneration (the bug_047 born-broken criterion; oracle: "
                f"{oracle})"
            )
    return fails


def main() -> int:
    args = sys.argv[1:]
    mint = "--mint-clock-grandfather" in args
    verify_landed = "--verify-landed" in args
    args = [a for a in args if not a.startswith("--")]
    src_root = pathlib.Path(args[0])

    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1

    # --- self-test arms (riders (a)-(c)) + the K-mutation battery
    # (rider (d)) — both run FIRST, every invocation -------------------
    battery = self_battery(src_root)
    if battery:
        print("FAIL: obligation/clock census self-battery —", file=sys.stderr)
        for x in battery:
            print(f"  {x}", file=sys.stderr)
        return 1
    killed = mutation_battery(src_root)
    if killed:
        print("FAIL: obligation/clock census K-mutation battery —", file=sys.stderr)
        for x in killed:
            print(f"  {x}", file=sys.stderr)
        return 1

    # --- the real scan ----------------------------------------------------
    fails = []
    files = list(production_files(src_root))
    if not files:
        fails.append("population floor — zero production files ((vvvvv))")
    hits = scan_clock_code(files)
    gf_path = src_root / CLOCK_GRANDFATHER
    if mint:
        if fails:
            for x in fails:
                print(f"mint refused: {x}")
            return 1
        # MULTISET mint (no dedup): one row per live hit — "count
        # preserved" through the projection is the literal
        # non-degeneracy fact (a projection that quotients distinct
        # sites would write fewer rows than hits; the W13-AX
        # sub-assert pins rows == live sites at every re-mint).
        keys = sorted(k for k, _m in hits)
        gf_path.write_text("".join(k + "\n" for k in keys))
        print(
            f"minted {len(keys)} gate-clock grandfather entries "
            f"({len(set(keys))} distinct content keys)"
        )
        return 0
    gf_counts = Counter()
    if gf_path.is_file():
        gf_counts = Counter(
            x for x in gf_path.read_text().splitlines() if x.strip()
        )
    excess, stale_rows = grandfather_diff(hits, gf_counts)
    fails += excess
    for k, deficit in stale_rows:
        fails.append(
            f"{k.split(chr(9))[0]}: stale gate-clock grandfather entry "
            f"({k!r} ×{deficit}) — remove it ({CLOCK_GRANDFATHER}, shrink-only)"
        )
    if verify_landed:
        fails += check_landed(src_root, OBLIGATION_ROWS, "obligation")
        fails += check_landed(src_root, CLOCK_ROWS, "clock")
    n_pending = sum(1 for r in list(OBLIGATION_ROWS.values()) + list(CLOCK_ROWS.values()) if r[0] == "pending")
    print(
        f"obligation+clock census: {len(OBLIGATION_ROWS)} obligation rows, "
        f"{len(CLOCK_ROWS)} clock rows ({n_pending} pending slot landings; "
        f"the wave-close --verify-landed flips them), lossy-arithmetic + "
        f"un-named-gate grammars live ({sum(gf_counts.values())} grandfathered "
        f"site rows, content-keyed, shrink-only); "
        f"recorded: {TAKE_THEN_AWAIT_CANDIDATE[:40]}..."
    )
    if fails:
        print("FAIL: obligation/clock census violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
