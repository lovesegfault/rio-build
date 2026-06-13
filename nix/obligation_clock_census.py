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

Self-test arms run first (the house pattern).
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

    # --- self-test arms --------------------------------------------------
    plant = scan_clock_code(
        [("planted/lossy.rs", "fn f(d: Duration) -> u64 { d.as_secs() * 1000 }\n")]
    )
    if len(plant) != 1 or "lossless" not in plant[0][1]:
        print(f"FAIL: the lossy-arithmetic plant did not red: {plant}", file=sys.stderr)
        return 1
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
        print(f"FAIL: the witnessed lossy site still flagged: {plant}", file=sys.stderr)
        return 1
    plant = scan_clock_code(
        [("planted/gate.rs", "fn g(t: Instant) -> bool { t.elapsed() > LIMIT }\n")]
    )
    if len(plant) != 1 or "clock census row" not in plant[0][1]:
        print(f"FAIL: the un-named-gate plant did not red: {plant}", file=sys.stderr)
        return 1
    f_l = check_landed(src_root, {"straw": ("pending", "S9")}, "obligation")
    if len(f_l) != 1:
        print(f"FAIL: the pending-at-verify plant did not red: {f_l}", file=sys.stderr)
        return 1
    # W13-AX (bug_047's defeat, reproduced then killed): two
    # same-operator gates in ONE file — the charged fan-out shape
    # (log_stream.rs :179/:261, the rel below is that exact
    # grandfathered file, so the plant sits IN-POPULATION) — are
    # grandfathered, then one gate is FIXED: the stale sweep MUST
    # trip for the fixed site's row. Under the retired file×operator
    # projection the surviving gate held the shared key live and the
    # sweep stayed silent (the pre-fix red, verbatim in the landing
    # commit body).
    pair_rel = "rio-builder/src/log_stream.rs"
    pair = (
        "fn a(t: Instant) -> bool { t.elapsed() >= WINDOW }\n"
        "fn b(u: Instant) -> bool { u.elapsed() >= BATCH_TIMEOUT }\n"
    )
    fixed = "fn b(u: Instant) -> bool { u.elapsed() >= BATCH_TIMEOUT }\n"
    gf_ax = Counter(k for k, _m in scan_clock_code([(pair_rel, pair)]))
    if len(gf_ax) != 2:
        print(
            f"FAIL: W13-AX — the same-operator in-population pair minted "
            f"{len(gf_ax)} distinct key(s), want 2 (site-content keying)",
            file=sys.stderr,
        )
        return 1
    ax_excess, ax_stale = grandfather_diff(scan_clock_code([(pair_rel, fixed)]), gf_ax)
    if ax_excess or len(ax_stale) != 1:
        print(
            f"FAIL: W13-AX — fixing one gate of the grandfathered pair must "
            f"trip the stale sweep exactly once (excess={ax_excess}, "
            f"stale={ax_stale})",
            file=sys.stderr,
        )
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
