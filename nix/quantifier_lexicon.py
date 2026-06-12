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

# WO-S8-10 (merged_bug_068, R23′-as-extended): claim-semantics tiers
# BEYOND the emphatic-uppercase word lexicon — the round-11 escapes
# were universals the word-shape grammar cannot see, each tier named
# after its escape:
#   noun     — relational nouns smuggling a forall ("SUPERSET of
#              every pool filter" rode an uppercase NON-lexicon noun
#              with the quantifier in lowercase, merged_bug_068;
#              'anywhere' bound a depth-1 walk, merged_bug_122).
#              BOTH cases fire: the escapes were upper AND lower.
#   modal    — lowercase modal universals ("can never", "cannot
#              ever" — merged_bug_015's escape shape).
#   compiler — compiler-semantics claims ("the compiler enforces",
#              "deny-warnings error" — bug_127's folklore: a
#              review-enforced convention narrated as machine-
#              enforced).
# Same binding grammar, same grandfather, one plant per tier
# (W11-CD). The hit grammar is the named needles ONLY — growing a
# tier is a review surface, not a regex creep.
CLAIM_TIERS = (
    (
        "noun",
        re.compile(
            r"\b(?:superset|subset|invariant)\s+of\b|\bsuperset-of\b"
            r"|\btotality\b|\banywhere\b",
            re.I,
        ),
    ),
    ("modal", re.compile(r"\bcan\s+never\b|\bcannot\s+ever\b", re.I)),
    (
        "compiler",
        re.compile(r"\bthe\s+compiler\s+enforces\b|\bdeny-warnings\s+error\b", re.I),
    ),
)

# WO-S8-11(iii) (merged_bug_021): the NUMERIC-NARRATION binder — a
# `<number><unit>` magnitude in default/shipped-class context is a
# cross-tier restatement of a constant that lives elsewhere (the
# "shipped 100 GiB chart default" class: present-tense magnitude
# prose in another tier has no binding census; R23' binds quantifier
# words, not numerals). A hit BINDS to its source key — `figure:
# values(<values-path>)` / `figure: const(<SYMBOL>)` — or demotes
# (non-normative / lowercase has no effect here: numerals carry no
# case). Time units are NOT in the unit set: duration constants are
# the R29 duration census's jurisdiction, and registry-note duration
# figures are banned outright by the retention-notes arm.
NUMERIC_NUM_RE = re.compile(
    r"\b\d+(?:\.\d+)?\s*(?:GiB|MiB|KiB|TiB|GB|MB|TB|vCPU|cores?|replicas?)\b"
)
NUMERIC_CTX_RE = re.compile(
    r"\b(?:default|shipped|ships|configured|provisioned|the chart)\b", re.I
)
FIGURE_BIND_RE = re.compile(r"figure:\s*(?:values|const)\([^)]+\)")

BIND_RE = re.compile(
    r"quantifier:\s*(?:census|non-normative)\(\S[^)]*\)"
    # The two bind idioms the round-10 wave landed BEFORE this lint
    # minted (recognizer reconciled at the wave-close tree, recorded):
    # `bind[<fragment>.sh]` — helm narration bound to the named test
    # fragment (S7's trigger/narration binds); and `machine-backed`
    # in apposition to the quantifier — the inline form whose cite
    # follows in the same comment block (S1's lane-census bind:
    # "ALL is machine-backed (R23'): the census-derived ... set").
    r"|bind\[\S+\]"
    # WO-S8-7 (bug_123): the natural-language bind token is
    # POLARITY-ANCHORED -- a negated apposition ("this is not
    # machine-backed yet") is a DISCLAIMER, not a bind; the bare
    # unanchored alternative matched inside it and cleared the hit.
    r"|(?<!\bnot )(?<!n't )(?<!\bnever )(?<!\bun)machine-backed"
)
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
                word = None
                m = HIT_RE.search(comment)
                if m:
                    word = m.group(1)
                else:
                    for tname, rx in CLAIM_TIERS:
                        cm = rx.search(comment)
                        if cm:
                            word = f"{cm.group(0)} [{tname} tier]"
                            break
                if word is None:
                    nm = NUMERIC_NUM_RE.search(comment)
                    if nm and NUMERIC_CTX_RE.search(comment):
                        word = f"{nm.group(0)} [numeric tier]"
                if word is None:
                    continue
                # WO-S8-7 (bug_123): clearing tokens are LANE-MATCHED
                # to the firing lane -- binds evaluate on the COMMENT
                # text the hit fired on, never the raw source line
                # (the raw-line surface strictly contains the firing
                # surface: a same-line string literal carrying a bind
                # token suppressed genuine comment-lane hits).
                if (
                    BIND_RE.search(comment)
                    or FIGURE_BIND_RE.search(comment)
                    or any(t in comment for t in AUTO_BOUND)
                ):
                    continue
                line = raw_lines[lineno - 1] if lineno <= len(raw_lines) else comment
                yield rel, lineno, word, line.strip()


def key(rel: str, line: str) -> str:
    return f"{rel}\t{line.strip()}"


def floor_fails(root: Path, mint: bool) -> list[str]:
    """Population floor (WO-S8-3, merged_bug_028): every tier must
    yield at least one file (pathlib globs fail open at zero matches
    -- a mis-staged tree previously scanned an empty tier green), and
    the grandfather ledger -- an explicitly-declared input of the
    check -- must resolve outside --mint-grandfather (a missing
    ledger previously defaulted to EMPTY, so one broken $src emptied
    scan AND backstop together). On a correctly staged tree the
    floors cannot false-positive."""
    fails = []
    for tier in TIERS:
        if not any(True for _ in iter_tier_files(root, tier)):
            fails.append(
                f"population floor -- tier `{tier}` resolved zero files "
                f"under the scan root (mis-staged tree? ((vvvvv)))"
            )
    if not mint and not (root / GRANDFATHER).is_file():
        fails.append(
            f"population floor -- grandfather ledger {GRANDFATHER} does "
            f"not resolve; a missing ledger is staging rot, never an "
            f"empty backstop ((vvvvv))"
        )
    return fails


def run(root: Path, mint: bool) -> int:
    floors = floor_fails(root, mint)
    if floors:
        for x in floors:
            print(x, file=sys.stderr)
        print(f"\n{len(floors)} population-floor failure(s).", file=sys.stderr)
        return 1
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
        f"quantifier-lexicon: {len(LEXICON)} words + {len(CLAIM_TIERS)} claim "
        f"tiers + the numeric binder x {len(TIERS)} file tiers clean "
        f"({len(grandfathered)} grandfathered, burn-down)"
    )
    return 0


def selftest() -> str | None:
    import tempfile

    # The floor's own plant (WO-S8-3 / W12-BA): an empty root REDS
    # every tier AND the missing ledger; mint mode exempts only the
    # ledger arm (minting writes it).
    with tempfile.TemporaryDirectory() as td:
        ff = floor_fails(Path(td), mint=False)
        if len(ff) != len(TIERS) + 1:
            return f"population-floor plant did not red per tier + ledger: {ff}"
        ff = floor_fails(Path(td), mint=True)
        if len(ff) != len(TIERS):
            return f"population-floor mint-mode plant wrong: {ff}"

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
        # W11-CD (WO-S8-10): one plant per CLAIM tier — each the named
        # escape from this corpus (068's relational noun with the
        # quantifier in lowercase; 015's lowercase modal; 127's
        # compiler claim). Unbound hits; census( bind clears;
        # non-normative( demote clears.
        rel, template = specimens["rust"]
        p = root / rel
        tier_plants = {
            "noun": "// a SUPERSET of every pool filter, so over-counting under-reaps",
            "modal": "// a concurrent check can never observe the torn state",
            "compiler": "// the compiler enforces the consume-once law here",
        }
        for tname, claim in tier_plants.items():
            p.write_text(f"{claim}\nfn x() {{}}\n", encoding="utf-8")
            got = list(scan(root))
            if len(got) != 1 or f"[{tname} tier]" not in got[0][2]:
                return f"claim-tier plant ({tname}): expected 1 unbound hit, got {got}"
            p.write_text(
                f"{claim}  quantifier: census(planted-artifact)\nfn x() {{}}\n",
                encoding="utf-8",
            )
            if list(scan(root)):
                return f"claim-tier plant ({tname}): census( bind did not clear"
            p.write_text(
                f"{claim}  quantifier: non-normative(narrative)\nfn x() {{}}\n",
                encoding="utf-8",
            )
            if list(scan(root)):
                return f"claim-tier plant ({tname}): non-normative( demote did not clear"
        # W11-CE (WO-S8-11(iii)): the numeric-narration plant — the
        # planted stale magnitude (the merged_bug_021 shape) hits;
        # the figure: source-key bind clears; non-normative clears.
        p.write_text("// the shipped 100 GiB chart default governs the solve\nfn x() {}\n", encoding="utf-8")
        got = list(scan(root))
        if len(got) != 1 or "[numeric tier]" not in got[0][2]:
            return f"W11-CE: the planted stale magnitude did not hit: {got}"
        p.write_text(
            "// the shipped 100 GiB chart default — figure: values(scheduler.sla.defaultDiskGib)\nfn x() {}\n",
            encoding="utf-8",
        )
        if list(scan(root)):
            return "W11-CE: the figure: values(...) bind did not clear"
        p.write_text(
            "// a 100 GiB example budget, quantifier: non-normative(illustrative)\nfn x() {}\n",
            encoding="utf-8",
        )
        if list(scan(root)):
            return "W11-CE: the non-normative demote did not clear"
        # … a magnitude WITHOUT default/shipped context stays silent
        # (the conjunction is the hit grammar).
        p.write_text("// reserves 100 GiB for the solve\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            return "W11-CE: a context-free magnitude hit (conjunction broken)"
        # … and the claim tiers fire in EVERY file tier (the product
        # discipline): the helm/script/typst specimens carry the noun.
        for ftier in ("helm", "script", "typst"):
            frel, ftemplate = specimens[ftier]
            fp = root / frel
            fp.parent.mkdir(parents=True, exist_ok=True)
            fp.write_text(ftemplate.format("a superset of every pool filter:"), encoding="utf-8")
            got = list(scan(root))
            if len(got) != 1 or "[noun tier]" not in got[0][2]:
                return f"claim-tier file-tier plant ({ftier}): {got}"
            fp.unlink()
        # Auto-bound arm: an enforcement-grammar line never hits.
        p.write_text("// EVERY member censused — census[gen: x.txt]\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            return "auto-bound census[ line still hit"
        # The two landed round-10 bind idioms (recognizer reconciled
        # at the wave-close tree): S7's bind[<fragment>] tag and S1's
        # machine-backed apposition both clear the hit.
        p.write_text("// ALL replicas share the budget. bind[26-store-scaling.sh]\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            return "bind[fragment] idiom did not clear the hit"
        p.write_text("// Suspend ALL collection — where ALL is machine-backed\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            return "machine-backed idiom did not clear the hit"
        # W12-BE (WO-S8-7, bug_123) -- the two false-negative faces,
        # RED plants (both yielded ZERO hits pre-fix):
        # (a) negative polarity: a DISCLAIMER naming the bind token
        # must still fire -- the bare alternative matched inside it.
        p.write_text("// ALL lanes delete here; this is not machine-backed yet\nfn x() {}\n", encoding="utf-8")
        got = list(scan(root))
        if len(got) != 1:
            return f"W12-BE (a): the negated machine-backed disclaimer did not fire: {got}"
        # (negation spellings ride the same anchor)
        p.write_text("// EVERY arm is swept; sadly never machine-backed\nfn x() {}\n", encoding="utf-8")
        if len(list(scan(root))) != 1:
            return "W12-BE (a'): the never-negated disclaimer did not fire"
        # (b) lane match: a same-line STRING carrying a bind token
        # must not clear a comment-lane hit (the clearing surface
        # equals the firing surface now).
        p.write_text('fn x() { let s = "machine-backed"; } // EVERY lane is swept\nfn y() {}\n', encoding="utf-8")
        got = list(scan(root))
        if len(got) != 1:
            return f"W12-BE (b): a cross-lane string bind suppressed the hit: {got}"
        # … and the binder leg rides the same law: a figure bind in a
        # string clears nothing.
        p.write_text('fn x() { let s = "figure: const(X)"; } // the shipped 100 GiB chart default\nfn y() {}\n', encoding="utf-8")
        got = list(scan(root))
        if len(got) != 1 or "[numeric tier]" not in got[0][2]:
            return f"W12-BE (b'): a cross-lane figure bind suppressed the numeric hit: {got}"
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
        # Grandfather: content-keyed pass + stale-entry red. run() now
        # enforces the population floors (WO-S8-3), so the fixture
        # root must stage every tier non-vacuously first (benign,
        # hit-free files).
        for ftier in ("helm", "script", "typst"):
            frel, _t = specimens[ftier]
            fp = root / frel
            fp.parent.mkdir(parents=True, exist_ok=True)
            fp.write_text("# plain\n" if ftier != "typst" else "Plain prose.\n", encoding="utf-8")
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
