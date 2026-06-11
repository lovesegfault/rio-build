#!/usr/bin/env python3
"""Quantifier-lexicon lint (R23' — the round-10 banner; WO-S8-7).

THE DEFECT CLASS (9 of 29 round-10 banner-tagged evasions, including
one HIGH, rode unbound universal quantifiers): "Suspend ALL
collection" while three destructive lanes kept deleting; "valid under
ANY page" on a windowed view; "capped backoff on ANY listener error"
latched one error class early. R23 versioned rule-ID citations only,
so universally quantified behavioral prose was invisible to it — the
round's primary rule-design finding. R23' grows the discipline to a
greppable QUANTIFIER LEXICON: every emphatic universal claim binds to
a machine census, or demotes to non-normative wording.

HIT GRAMMAR (the recorded derivation decision): a hit is an
EMPHATIC-UPPERCASE lexicon member — one of LEXICON below — inside
COMMENT/PROSE text of a scanned tier. This repo's own convention
carries normative emphasis in caps (the round-10 exemplars: "Suspend
ALL collection", "the ONLY debit paths", "EVERY returned chunk");
lowercase is ordinary prose flow, which IS the rule's demoted form —
so "demote" is mechanical (lowercase the emphasis) and the hit
grammar is closed. Divergence from the signed rule text recorded: the
rule names the words, this lint binds their EMPHATIC form; a
lowercase universal claim is out of scope by design (the demote arm),
attackable at review.

TIERS (the tier table — plants derive from LEXICON x TIERS, R22'):
    rust    rio-*/src + xtask/src *.rs — the shared lexer's comment
            lane (string literals can never fire);
    helm    infra/helm/rio-build values.yaml + templates/*.yaml —
            '#'-comment narration (generated/ excluded);
    typst   docs/spec **/*.typ — prose lines (the spec tier);
    script  nix/*.py + nix/tests/helm/*.sh — '#'-comment lines.

BINDING GRAMMAR (same-line; enrollment binds, never decorates):
    quantifier: census(<ref>)          the claim names the artifact /
                                       test / census that enforces it;
    quantifier: non-normative(<why>)   the explicit demote-with-tag
                                       (for prose that must keep the
                                       emphasis without claiming
                                       enforcement);
plus AUTO-BOUND lines already owned by an enforcement grammar:
census[test:/gen:] tags, [GEN-SET] cites, r[impl/verify] markers,
#r(" rule mints, producer-census:/timeout-census:/refusal-census:
rows, r13-allow( sanctions, MODEL-DIVERGENCE( headers — those
grammars carry their own validation, so a lexicon word inside them
is already bound to machinery.

FROZEN GRANDFATHER, BURN-DOWN (the census_enrollment lineage):
nix/quantifier-grandfather.txt, entries `path<TAB>trimmed-line`,
minted ONLY by --mint-grandfather at the lint's landing tree and
re-minted shrink-only at integration trees. Content-keyed: editing a
grandfathered claim line evicts it (touching quantified prose forces
bind-or-demote), line drift evicts nothing; a stale entry fails
(monotone shrink); the failure text never suggests grandfathering.

SELF-TEST (R22' — the plant set DERIVES from the two tables): for
every (word, tier) cell of LEXICON x TIERS, a planted unbound claim
MUST hit, the same line with the census( bind MUST pass, and the
non-normative( demote MUST pass; plus auto-bound and grandfather
arms. The census-corpora registry row derives from LEXICON (the
refusal predicate's complement — coverage is computed, not declared).

Exit: 0 clean; 1 violations; 2 usage/setup.
"""

import re
import sys
from pathlib import Path

import rust_strip

LEXICON = ("ALL", "EVERY", "ANY", "NEVER", "ALWAYS", "ONLY", "SAME")
HIT_RE = re.compile(r"\b(" + "|".join(LEXICON) + r")\b")

BIND_RE = re.compile(r"quantifier:\s*(?:census|non-normative)\(\S[^)]*\)")
AUTO_BOUND = (
    "census[",
    "[GEN-SET]",
    "r[impl",
    "r[verify",
    '#r("',
    "producer-census:",
    "timeout-census:",
    "refusal-census:",
    "r13-allow(",
    "MODEL-DIVERGENCE(",
)

GRANDFATHER = "nix/quantifier-grandfather.txt"

# The tier table (name, description) — scan_tier dispatches on it and
# the self-test iterates it; adding a tier without a plant cell is a
# self-test red by construction.
TIERS = ("rust", "helm", "typst", "script")


def rust_comment_lines(text: str):
    """line number (1-based) -> comment text on that line, via the
    shared lexer's comment lane."""
    _, _, comment_spans = rust_strip.lex_full(text, blank_string_bodies=True)
    starts = [0]
    for i, ch in enumerate(text):
        if ch == "\n":
            starts.append(i + 1)
    out: dict[int, str] = {}
    li = 0
    for a, b in comment_spans:
        seg = text[a:b]
        while li + 1 < len(starts) and starts[li + 1] <= a:
            li += 1
        line_no = li + 1
        for piece in seg.split("\n"):
            out[line_no] = out.get(line_no, "") + piece
            line_no += 1
    return out


def hash_comment_lines(text: str):
    """line number -> comment text for '#'-comment tiers (helm,
    script). The comment is the text from the first '#' that is not
    inside quotes — conservative: a '#' inside a quoted string on the
    same line may still count as comment text; the tiers are
    narration files where that shape is absent."""
    out: dict[int, str] = {}
    for i, line in enumerate(text.splitlines(), 1):
        if "#" in line:
            out[i] = line[line.index("#"):]
    return out


def typst_lines(text: str):
    """line number -> prose text (whole line — the spec tier is all
    prose)."""
    return {i: line for i, line in enumerate(text.splitlines(), 1) if line.strip()}


def iter_tier_files(root: Path, tier: str):
    if tier == "rust":
        for crate_src in sorted(root.glob("rio-*/src")):
            yield from sorted(crate_src.rglob("*.rs"))
        x = root / "xtask" / "src"
        if x.is_dir():
            yield from sorted(x.rglob("*.rs"))
    elif tier == "helm":
        chart = root / "infra" / "helm" / "rio-build"
        v = chart / "values.yaml"
        if v.is_file():
            yield v
        t = chart / "templates"
        if t.is_dir():
            yield from sorted(t.glob("*.yaml"))
    elif tier == "typst":
        spec = root / "docs" / "spec"
        if spec.is_dir():
            yield from sorted(spec.rglob("*.typ"))
    elif tier == "script":
        yield from sorted((root / "nix").glob("*.py"))
        h = root / "nix" / "tests" / "helm"
        if h.is_dir():
            yield from sorted(h.glob("*.sh"))


def tier_lines(tier: str, text: str):
    if tier == "rust":
        return rust_comment_lines(text)
    if tier in ("helm", "script"):
        return hash_comment_lines(text)
    return typst_lines(text)


def scan(root: Path):
    """Yield (rel, lineno, word, trimmed-line) for every UNBOUND hit."""
    for tier in TIERS:
        for f in iter_tier_files(root, tier):
            rel = f.relative_to(root).as_posix()
            if rel == GRANDFATHER or rel == "nix/quantifier_lexicon.py":
                # The ledger and this lint's own source (its tables
                # carry the lexicon as CODE; its comments stay
                # lowercase by construction).
                continue
            try:
                text = f.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            raw_lines = text.splitlines()
            for lineno, comment in sorted(tier_lines(tier, text).items()):
                m = HIT_RE.search(comment)
                if not m:
                    continue
                line = raw_lines[lineno - 1] if lineno <= len(raw_lines) else comment
                if BIND_RE.search(line) or any(t in line for t in AUTO_BOUND):
                    continue
                yield rel, lineno, m.group(1), line.strip()


def key(rel: str, line: str) -> str:
    return f"{rel}\t{line.strip()}"


def run(root: Path, mint: bool) -> int:
    gf_path = root / GRANDFATHER
    hits = list(scan(root))
    if mint:
        # R15: the grandfather is THIS scanner's own output, never
        # hand-authored — minted at the landing tree, re-minted
        # shrink-only at integration trees.
        keys = sorted({key(rel, line) for rel, _ln, _w, line in hits})
        gf_path.write_text("".join(k + "\n" for k in keys), encoding="utf-8")
        print(f"minted {len(keys)} grandfather entr(ies) at {GRANDFATHER}")
        return 0
    grandfathered = set()
    if gf_path.is_file():
        grandfathered = {
            ln for ln in gf_path.read_text(encoding="utf-8").splitlines() if ln.strip()
        }
    live = {key(rel, line) for rel, _ln, _w, line in hits}
    out = []
    for rel, lineno, word, line in hits:
        if key(rel, line) in grandfathered:
            continue
        out.append(
            f"{rel}:{lineno}: unbound quantifier `{word}` — bind it "
            f"(`quantifier: census(<ref>)`), demote it (lowercase the "
            f"emphasis or `quantifier: non-normative(<why>)`), or generate "
            f"the claim from the artifact it describes"
        )
    for entry in sorted(grandfathered - live):
        out.append(
            f"{entry.split(chr(9))[0]}: stale quantifier-grandfather entry "
            f"(the claim was bound, demoted, or deleted) — remove it from "
            f"{GRANDFATHER}: {entry!r}"
        )
    if out:
        for v in out:
            print(v, file=sys.stderr)
        print(f"\n{len(out)} unbound quantified claim(s)/stale entr(ies).", file=sys.stderr)
        return 1
    print(
        f"quantifier-lexicon: {len(LEXICON)} words x {len(TIERS)} tiers clean "
        f"({len(grandfathered)} grandfathered, burn-down)"
    )
    return 0


def selftest() -> str | None:
    import tempfile

    specimens = {
        "rust": ("rio-planted/src/lib.rs", "// {} callers route through the gate\nfn x() {{}}\n"),
        "helm": ("infra/helm/rio-build/values.yaml", "# {} replicas share one budget\nkey: 1\n"),
        "typst": ("docs/spec/x.typ", "The gate spans {} lanes here.\n"),
        "script": ("nix/planted.py", "# {} scanners consume the lexer\nx = 1\n"),
    }
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        # The LEXICON x TIERS product (R22': the plant set derives from
        # the two tables — a new word or tier without a passing cell is
        # a red here, never a silent gap).
        for tier in TIERS:
            rel, template = specimens[tier]
            p = root / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            for word in LEXICON:
                # (a) unbound -> exactly one hit naming the word.
                p.write_text(template.format(word), encoding="utf-8")
                got = list(scan(root))
                if len(got) != 1 or got[0][2] != word:
                    return f"plant ({word},{tier}): expected 1 unbound hit, got {got}"
                # (b) census-bound -> clean.
                bound = template.format(word).replace(
                    "\n", "  quantifier: census(planted-artifact)\n", 1
                )
                p.write_text(bound, encoding="utf-8")
                if list(scan(root)):
                    return f"plant ({word},{tier}): census( bind did not clear the hit"
                # (c) non-normative -> clean.
                demoted = template.format(word).replace(
                    "\n", "  quantifier: non-normative(narrative emphasis)\n", 1
                )
                p.write_text(demoted, encoding="utf-8")
                if list(scan(root)):
                    return f"plant ({word},{tier}): non-normative( demote did not clear the hit"
            p.unlink()
        # Auto-bound arm: an enforcement-grammar line never hits.
        rel, template = specimens["rust"]
        p = root / rel
        p.write_text("// EVERY member censused — census[gen: x.txt]\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            return "auto-bound census[ line still hit"
        # Lowercase IS the demoted form: no hit.
        p.write_text("// every member is checked here\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            return "lowercase prose hit — the demote arm is broken"
        # String literals can never fire (the rust tier reads the
        # lexer's comment lane).
        p.write_text('fn x() { let s = "ALL CAPS STRING"; }\n', encoding="utf-8")
        if list(scan(root)):
            return "string-literal lexicon word fired in the rust tier"
        p.unlink()
        # Grandfather: content-keyed pass + stale-entry red.
        p.write_text("// ALL lanes consult the hold\nfn x() {}\n", encoding="utf-8")
        gf = root / GRANDFATHER
        gf.parent.mkdir(parents=True, exist_ok=True)
        gf.write_text("rio-planted/src/lib.rs\t// ALL lanes consult the hold\n", encoding="utf-8")
        import contextlib
        import io

        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            rc = run(root, mint=False)
        if rc != 0:
            return "grandfathered claim still failed"
        gf.write_text(
            "rio-planted/src/lib.rs\t// ALL lanes consult the hold\n"
            "rio-planted/src/lib.rs\t// EVERY ghost entry\n",
            encoding="utf-8",
        )
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            rc = run(root, mint=False)
        if rc != 1:
            return "stale grandfather entry did not fail"
    return None


def main() -> int:
    args = sys.argv[1:]
    mint = "--mint-grandfather" in args
    args = [a for a in args if a != "--mint-grandfather"]
    if len(args) != 1:
        print("usage: quantifier_lexicon.py [--mint-grandfather] <repo-root>", file=sys.stderr)
        return 2
    err = rust_strip.selftest()
    if err:
        print(f"FAIL: shared lexer self-test — {err}", file=sys.stderr)
        return 1
    err = selftest()
    if err:
        print(f"FAIL: quantifier-lexicon self-test — {err}", file=sys.stderr)
        return 1
    return run(Path(args[0]), mint)


if __name__ == "__main__":
    sys.exit(main())
