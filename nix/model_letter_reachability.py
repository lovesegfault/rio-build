#!/usr/bin/env python3
"""P9 — the model-letter reachability lint (round-13 WO-S9-8(iii);
the audit's candidate, codified).

Argv: <src-root> [--mint-grandfather].

THE RULE: every model letter appearing in a wired invariant's guard
needs an in-model constructor reachability witness or a recorded
vacuity exemption — F2 (leaderMarks' `marks["other"]` with no
in-model constructor: a sweep branch dead in every reachable state
under a verify marker) is the founding instance of the CLASS this
lint polices.

V1 JURISDICTION (disclosed, never silently claimed): VARIANT LABELS
in top-level model files (docs/spec/models/*.qnt; calibration/ twins
are defect lanes by design and excluded). A label referenced in any
`val` body (the invariant/witness tier) must OCCUR in at least one
`action`/`run` body — the constructor side — or carry a
`p9-vacuity: <why>` exemption comment on its declaration arm.
Match-arm occurrences inside actions count as construction in v1
(the conservative, under-flagging direction — disclosed); the
recorded v2 queue: true-constructor positions only, and map-KEY
letters (F2's literal shape — its repair lands with S3's model
constructor this wave, the class exemplar). Pre-existing violations
ride the SHRINK-ONLY grandfather (nix/p9-grandfather.txt, minted at
birth, content-keyed via the shared census_corpora.content_key
projection — the WO-S9-1 keying, never a hand quotient).

Self-test arms run first; K-mutations via the shared
census_corpora.run_mutation_battery harness (recursion grounded:
self_battery never invokes it).
"""

import pathlib
import re
import sys

import census_corpora
import rust_strip

GRANDFATHER = "nix/p9-grandfather.txt"
VARIANT_ARM = re.compile(r"^\s*\|\s*([A-Z]\w*)", re.M)
P9_VACUITY = re.compile(r"p9-vacuity:")
# Top-level decl boundaries (two-space module-body indent, the house
# style).
DECL = re.compile(
    r"^\s{2}(?:(pure\s+def|def|val|action|run|type|var|const)\b\s*(\w+)?)", re.M
)


def model_files(src_root):
    d = src_root / "docs" / "spec" / "models"
    for f in sorted(d.glob("*.qnt")) if d.is_dir() else []:
        yield str(f.relative_to(src_root)), f.read_text(encoding="utf-8")


def blocks(text):
    """[(kind, name, body)] segmented at top-level decls (crude but
    stable against the house two-space style)."""
    out = []
    marks = list(DECL.finditer(text))
    for i, m in enumerate(marks):
        end = marks[i + 1].start() if i + 1 < len(marks) else len(text)
        out.append((m.group(1), m.group(2) or "", text[m.start() : end]))
    return out

def scan_letters(files):
    """[(key, msg)] for val-referenced variant labels with zero
    action/run occurrences and no vacuity exemption."""
    hits = []
    for rel, raw in files:
        # Comments blanked through the SHARED lexer (the shadow-
        # stripper ban applies to this scanner too; .qnt comment and
        # string syntax is lex-compatible); raw lines kept for the
        # vacuity window.
        lines = raw.splitlines()
        stripped, _ = rust_strip.lex(raw, blank_string_bodies=True)
        declared = {}
        for m in VARIANT_ARM.finditer(stripped):
            lineno = stripped.count("\n", 0, m.start()) + 1
            declared.setdefault(m.group(1), lineno)
        if not declared:
            continue
        bl = blocks(stripped)
        val_text = "\n".join(b for k, _n, b in bl if k in ("val", "def", "pure def"))
        act_text = "\n".join(b for k, _n, b in bl if k in ("action", "run"))
        for label, lineno in sorted(declared.items()):
            if not re.search(rf"\b{label}\b", val_text):
                continue  # not consumed by the invariant tier
            if re.search(rf"\b{label}\b", act_text):
                continue  # constructed (v1: any action/run occurrence)
            window = "\n".join(lines[max(0, lineno - 2) : min(len(lines), lineno + 1)])
            if P9_VACUITY.search(window):
                continue
            key = census_corpora.content_key(rel, "p9", lines[lineno - 1])
            hits.append(
                (
                    key,
                    f"{rel}:{lineno}: variant letter `{label}` is consumed by "
                    f"the invariant tier but occurs in NO action/run body — a "
                    f"dead model letter under a wired guard (P9; the F2 "
                    f"class): give it a constructor, a `p9-vacuity: <why>` "
                    f"exemption, or it rides the shrink-only grandfather",
                )
            )
    return hits


def self_battery(src_root) -> list:
    fails = []
    files = list(model_files(src_root))
    if not files:
        fails.append("population floor — zero model files ((vvvvv))")
    dead = (
        "module m {\n"
        "  type T =\n"
        "    | Alive\n"
        "    | Dead\n"
        "  var x: T\n"
        "  action init = x' = Alive\n"
        "  action step = x' = Alive\n"
        "  val inv = x != Dead\n"
        "}\n"
    )
    got = scan_letters([("planted/dead.qnt", dead)])
    if len(got) != 1 or "Dead" not in got[0][1]:
        fails.append(f"the dead-letter plant did not red: {got}")
    alive = dead.replace("action step = x' = Alive", "action step = x' = Dead")
    if scan_letters([("planted/alive.qnt", alive)]):
        fails.append("the constructed letter still flagged")
    exempt2 = (
        "module m {\n"
        "  type T =\n"
        "    | Alive\n"
        "    // p9-vacuity: Dead is the unreachable-by-design pole\n"
        "    | Dead\n"
        "  var x: T\n"
        "  action init = x' = Alive\n"
        "  action step = x' = Alive\n"
        "  val inv = x != Dead\n"
        "}\n"
    )
    if scan_letters([("planted/exempt.qnt", exempt2)]):
        fails.append("the vacuity-exempted letter still flagged")
    unconsumed = dead.replace("val inv = x != Dead", "val inv = true")
    if scan_letters([("planted/unconsumed.qnt", unconsumed)]):
        fails.append("an invariant-unconsumed letter entered the population")
    return fails


MUTATIONS = [
    (
        "consumption-check-deleted",
        "every declared letter treated as consumed — killed by the"
        " unconsumed-letter plant (it would start flagging)",
        '            if not re.search(rf"\\b{label}\\b", ' + "val_text):",
        "            if False and not re.search(rf\"\\b{label}\\b\", " + "val_text):",
    ),
    (
        "construction-check-deleted",
        "every letter treated as constructed — killed by the"
        " dead-letter plant",
        '            if re.search(rf"\\b{label}\\b", ' + "act_text):",
        "            if True or re.search(rf\"\\b{label}\\b\", " + "act_text):",
    ),
    (
        "exemption-widened",
        "the vacuity window accepts everything — killed by the"
        " dead-letter plant (it would stop flagging)",
        "P9_VACUITY = re." + 'compile(r"p9-vacuity:")',
        "P9_VACUITY = re." + 'compile(r"")',
    ),
    (
        "population-emptied",
        "the model walk emptied — killed by the floor",
        '    for f in sorted(d.glob("*' + '.qnt")) if d.is_dir() else []:',
        "    for f in [" + "]:",
    ),
]


def main() -> int:
    args = sys.argv[1:]
    mint = "--mint-grandfather" in args
    args = [a for a in args if not a.startswith("--")]
    src_root = pathlib.Path(args[0])

    battery = self_battery(src_root)
    if battery:
        print("FAIL: P9 self-battery —", file=sys.stderr)
        for x in battery:
            print(f"  {x}", file=sys.stderr)
        return 1
    killed = census_corpora.run_mutation_battery(
        pathlib.Path(__file__), MUTATIONS, "self_battery", (src_root,)
    )
    if killed:
        print("FAIL: P9 K-mutation battery —", file=sys.stderr)
        for x in killed:
            print(f"  {x}", file=sys.stderr)
        return 1

    fails = []
    files = list(model_files(src_root))
    if not files:
        fails.append("population floor — zero model files ((vvvvv))")
    hits = scan_letters(files)
    gf_path = src_root / GRANDFATHER
    if mint:
        if fails:
            for x in fails:
                print(f"mint refused: {x}")
            return 1
        keys = sorted(k for k, _m in hits)
        gf_path.write_text("".join(k + "\n" for k in keys))
        print(f"minted {len(keys)} P9 grandfather entries ({len(set(keys))} distinct)")
        return 0
    from collections import Counter

    gf_counts = Counter()
    if gf_path.is_file():
        gf_counts = Counter(x for x in gf_path.read_text().splitlines() if x.strip())
    live_by_key = {}
    for k, m in hits:
        live_by_key.setdefault(k, []).append(m)
    for k in sorted(live_by_key):
        over = len(live_by_key[k]) - gf_counts.get(k, 0)
        if over > 0:
            fails.extend(live_by_key[k][-over:])
    for k in sorted(gf_counts):
        deficit = gf_counts[k] - len(live_by_key.get(k, []))
        if deficit > 0:
            fails.append(
                f"{k.split(chr(9))[0]}: stale P9 grandfather entry ({k!r} "
                f"×{deficit}) — remove it ({GRANDFATHER}, shrink-only)"
            )
    print(
        f"P9 model-letter reachability: {len(files)} model files, "
        f"{len(hits)} dead-letter site(s) "
        f"({sum(gf_counts.values())} grandfathered, shrink-only, "
        f"content-keyed)"
    )
    if fails:
        print("FAIL: P9 violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
