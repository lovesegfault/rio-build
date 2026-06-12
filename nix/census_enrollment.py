#!/usr/bin/env python3
"""Census-enrollment lint (round-8 WO-S6-5): load-bearing prose
membership claims are UNSHIPPABLE unless bound to a machine artifact.

THE DEFECT CLASS (author-census; 4 round-8 exemplars — bug_060,
bug_094, merged_bug_059, merged_bug_063): a hand-enumerated membership
claim living as prose outside any machine-derived or enrolled
artifact, rotting silently when a later commit changes the membership
elsewhere (bug_060's type specimen: "the store's client never sends
confirm_only (both call sites pass false)" — inverted two crates away
by a later wave, nothing invalidated the comment, and the stale prose
licensed two edits that would break every production probe). This is
an evasion OF R15: the rule protects censuses that exist as
artifacts; enrollment remained manual. The kill: a lint cannot judge
whether prose is TRUE, but it can force prose that STATES a
membership to name an artifact that the membership change breaks.

TIER-1 GRAMMAR (line-local, comment-lane — the call-graph family):
a scan HIT is a physical line whose COMMENT TEXT (the shared exact
lexer's comment lane: string literals can never fire) matches any of:

    both call ?sites?              (the )?only call ?sites?
    (the )?only callers?           sole call ?sites?
    all (call ?sites?|callers?)    no other (callers?|call ?sites?)
    exactly (one|two|...|N) (callers?|call ?sites?)
    never sends?

Each alternative is anchored at word boundaries (derivation
refinement over the book's bare alternation, recorded: unanchored
`never sends?` fires inside "never sender" — a live false positive at
the round-8 base tree; the boundary keeps the tier-1 FP mass at zero)
and matched case-insensitively (prose capitalizes sentence-initially).

ENROLLMENT GRAMMAR (SAME-LINE — adjacent-line qualifiers read as live
text when quoted; the merged_bug_140/081 lesson):

    census[test: <fn_name>]   the named test IS the census pin: the
                              lint asserts `fn <fn_name>` resolves
                              somewhere in the scanned trees
                              (resolution is tree-wide, so
                              cross-crate binding works);
    census[gen: <path>]       the committed [GEN-SET] pattern: the
                              lint asserts the repo-relative file
                              exists.

A hit line must carry >= 1 census[...] tag (or be grandfathered).
EVERY census[...] tag anywhere in scanned comments must RESOLVE —
enrollment binds, never decorates: a dangling tag fails even on a
non-hit line (rot of a renamed test/file is caught wherever the tag
sits).

FROZEN GRANDFATHER, BURN-DOWN (the P7/R13 lineage):
nix/census-grandfather.txt, entries `path<TAB>trimmed-line-content`,
minted ONLY by this scanner's own --mint-grandfather mode at the
round-8 final slot tree (machine-derived per R15 — the generator is
the scanner). Semantics:
  (i)   every scan hit must be enrolled OR grandfathered — else FAIL
        naming the line and the two enrollment forms;
  (ii)  every grandfather entry must match >= 1 live hit — else FAIL
        "remove the stale entry": the file monotonically shrinks.
        Entries are CONTENT-KEYED, so editing a grandfathered census
        line evicts it (touching load-bearing prose forces enrollment
        or deletion of the claim) while line-number drift from
        unrelated edits evicts nothing;
  (iii) additions to the file fail review by construction — this
        lint never suggests grandfathering; its failure text names
        the two enrollment forms only.

SELF-TEST ARMS (planted at runtime in a temp dir; a lint that cannot
fail its planted fixtures does not gate — the fixture_provenance /
concept-tier pattern):
  (A) planted unenrolled claim line          -> MUST fail;
  (B) planted enrolled line, resolvable name -> MUST pass;
  (C) planted enrolled line, unresolvable    -> MUST fail;
  (D) planted stale grandfather entry        -> MUST fail.

RECORDED NON-GOALS (burn-down ledger — tier-2 candidates, census
first):
  - widening the noun family to {consumers, producers, writers,
    readers, publishers} quantifications: FP mass unsized; the tier-2
    sizing generator is
      rg -in "\\b(all|only|both|sole|exactly [a-z0-9]+|no other) \\
        (consumers?|producers?|writers?|readers?|publishers?)\\b" \\
        --type rust rio-*/src
    — run it BEFORE widening;
  - comment-block-spanning claims (bug_060's own "never / sends" line
    break): caught only when a shape lands on one line. The round-8
    grandfather census shows every live hit is line-local; the
    multi-line tier-2 generator is the same alternation applied to
    whole extracted comment BLOCKS (lexer comment spans joined);
  - the merged_bug_064 angle ("state that must be invalidated when a
    key prop changes") is a svelte/component census outside this
    rust comment lane — S5's keyed-record close is the fix vehicle
    (round-8 §1.6.1 dup resolution; no corpus id here);
  - macro-generated test fns: `census[test: ...]` resolution reads
    lexed source text, so a fn minted by a macro does not resolve —
    enroll those via `census[gen: ...]` against committed output.

Self-allowlist: this scanner, nix/census-grandfather.txt, and
nix/misc-checks.nix are excluded by FILENAME (defense in depth: the
walk only visits rio-*/src *.rs files, which excludes all three
structurally — the deny-tier precedent).

Exit: 0 clean; 1 violations (every violation printed); 2 usage/setup.
"""

import re
import sys
from pathlib import Path

import rust_strip

# The tier-1 claim-shape alternation. Word-boundary anchored per the
# header's derivation note; case-insensitive.
CLAIM_SHAPES = [
    r"both call ?sites?",
    r"(?:the )?only call ?sites?",
    r"(?:the )?only callers?",
    r"sole call ?sites?",
    r"all (?:call ?sites?|callers?)",
    r"no other (?:callers?|call ?sites?)",
    # Spelled counts cover the full series writers actually use
    # (merged_bug_149: the alternation stopped at "three", so
    # "exactly four callers" silently escaped the claim census).
    r"exactly (?:one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve|[0-9]+) (?:callers?|call ?sites?)",
    r"never sends?",
]
CLAIM_RE = re.compile(
    r"\b(?:" + "|".join(CLAIM_SHAPES) + r")\b",
    re.IGNORECASE,
)

# Enrollment tags, same-line.
TAG_RE = re.compile(r"census\[(test|gen):\s*([^\]\s][^\]]*?)\s*\]")

# Test-name resolution: a real `fn <name>` token in LEXED text
# (comments and string bodies blanked — a name quoted in prose or a
# string cannot satisfy resolution).
FN_DEF_TMPL = r"\bfn\s+{name}\b"

SELF_ALLOWLIST = {"census_enrollment.py", "census-grandfather.txt", "misc-checks.nix"}


def scan_files(root: Path):
    """Yield every scanned .rs file under rio-*/src."""
    for crate_src in sorted(root.glob("rio-*/src")):
        for f in sorted(crate_src.rglob("*.rs")):
            if f.name in SELF_ALLOWLIST:
                continue
            yield f


def comment_lines(text: str):
    """Map line number (1-based) -> concatenated comment text on that
    line, via the shared lexer's comment lane."""
    _, _, comment_spans = rust_strip.lex_full(text, blank_string_bodies=True)
    # Precompute line start offsets.
    starts = [0]
    for i, ch in enumerate(text):
        if ch == "\n":
            starts.append(i + 1)
    out: dict[int, str] = {}
    li = 0
    for a, b in comment_spans:
        seg = text[a:b]
        # Advance to the line containing offset a.
        while li + 1 < len(starts) and starts[li + 1] <= a:
            li += 1
        # Split the comment span across its lines.
        line_no = li + 1
        for piece in seg.split("\n"):
            out[line_no] = out.get(line_no, "") + piece
            line_no += 1
    return out


def collect(root: Path):
    """One walk: hits, tags, and the lexed corpus for fn resolution."""
    hits = []  # (relpath, line_no, raw_line, trimmed, tags_on_line)
    all_tags = []  # (relpath, line_no, kind, value)
    lexed_corpus = []  # lexed text per file, for fn resolution
    for f in scan_files(root):
        text = f.read_text(encoding="utf-8", errors="replace")
        lexed, _, _ = rust_strip.lex_full(text, blank_string_bodies=True)
        lexed_corpus.append(lexed)
        rel = str(f.relative_to(root))
        lines = text.split("\n")
        for line_no, ctext in sorted(comment_lines(text).items()):
            tags = [(k, v) for k, v in TAG_RE.findall(ctext)]
            for k, v in tags:
                all_tags.append((rel, line_no, k, v))
            if CLAIM_RE.search(ctext):
                raw = lines[line_no - 1] if line_no - 1 < len(lines) else ""
                hits.append((rel, line_no, raw, raw.strip(), tags))
    return hits, all_tags, lexed_corpus


def floor_fails(root: Path, grandfather_path, mint: bool) -> list[str]:
    """Population floor (WO-S8-3, merged_bug_028): the rio-*/src walk
    must resolve at least one crate root and one file (pathlib globs
    fail open at zero matches), and an EXPLICITLY-passed grandfather
    ledger must resolve outside mint mode -- a missing ledger
    previously defaulted to EMPTY, so one broken $src emptied scan
    AND backstop together. On a correctly staged tree the floors
    cannot false-positive."""
    fails = []
    crate_roots = sorted(root.glob("rio-*/src"))
    if not crate_roots:
        fails.append(
            "population floor -- zero rio-*/src roots resolved under the "
            "scan root (mis-staged tree? ((vvvvv)))"
        )
    elif not any(True for _ in scan_files(root)):
        fails.append("population floor -- the rio-*/src walk yielded zero files")
    if not mint and grandfather_path is not None and not grandfather_path.is_file():
        fails.append(
            f"population floor -- explicitly-passed grandfather ledger "
            f"{grandfather_path} does not resolve; staging rot, never an "
            f"empty backstop ((vvvvv))"
        )
    return fails


def run(root: Path, grandfather_path: Path | None, mint: bool) -> int:
    floors = floor_fails(root, grandfather_path, mint)
    if floors:
        for x in floors:
            print(f"FAIL: {x}", file=sys.stderr)
        return 1
    hits, all_tags, lexed_corpus = collect(root)
    corpus_blob = "\n".join(lexed_corpus)
    failures: list[str] = []

    # Tag resolution — every tag binds (arm C).
    for rel, line_no, kind, value in all_tags:
        if kind == "test":
            if not re.search(FN_DEF_TMPL.format(name=re.escape(value)), corpus_blob):
                failures.append(
                    f"{rel}:{line_no}: census[test: {value}] does not resolve — "
                    f"no `fn {value}` exists in the scanned trees "
                    f"(enrollment must bind, not decorate)"
                )
        else:  # gen
            if not (root / value).is_file():
                failures.append(
                    f"{rel}:{line_no}: census[gen: {value}] does not resolve — "
                    f"no such committed file (enrollment must bind, not decorate)"
                )

    unenrolled = [(rel, ln, raw, trimmed) for rel, ln, raw, trimmed, tags in hits if not tags]

    if mint:
        # Machine-mint the grandfather: every currently-unenrolled hit,
        # content-keyed and deduplicated. NEVER hand-edited upward
        # (semantics (iii)).
        for rel, trimmed in sorted(set((r, t) for r, _ln, _raw, t in unenrolled)):
            print(f"{rel}\t{trimmed}")
        return 0

    grandfather: list[tuple[str, str]] = []
    if grandfather_path is not None and grandfather_path.is_file():
        for raw in grandfather_path.read_text(encoding="utf-8").splitlines():
            if not raw.strip() or raw.startswith("#"):
                continue
            path, _, content = raw.partition("\t")
            grandfather.append((path, content))

    gset = set(grandfather)

    # (i) every hit enrolled or grandfathered.
    for rel, ln, _raw, trimmed in unenrolled:
        if (rel, trimmed) in gset:
            continue
        failures.append(
            f"{rel}:{ln}: load-bearing census prose without a bound artifact:\n"
            f"    {trimmed}\n"
            f"  enroll it on the SAME line: census[test: <fn_name>] (a test whose "
            f"failure tracks the membership) or census[gen: <repo-relative path>] "
            f"(a committed generator output) — or delete the claim"
        )

    # (ii) every grandfather entry matches >= 1 live hit (content-keyed).
    live = set((rel, trimmed) for rel, _ln, _raw, trimmed in unenrolled)
    for path, content in grandfather:
        if (path, content) not in live:
            failures.append(
                f"nix/census-grandfather.txt: stale entry — no live unenrolled hit "
                f"matches ({path!r}, {content!r}); remove the stale entry "
                f"(the file only ever shrinks)"
            )

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(
            f"\ncensus-enrollment: {len(failures)} violation(s); "
            f"{len(hits)} hit(s) scanned",
            file=sys.stderr,
        )
        return 1
    print(
        f"OK: census-enrollment — {len(hits)} claim line(s): "
        f"{sum(1 for *_x, t in hits if t)} enrolled, "
        f"{len(unenrolled)} grandfathered; {len(all_tags)} tag(s) resolved"
    )
    return 0


def selftest(tmp: Path) -> str | None:
    """The four planted arms. Returns an error string on the first arm
    whose verdict is wrong (a lint that cannot fail its planted
    fixtures does not gate). Arm output is swallowed — the planted
    FAIL lines would read as real violations in a green gate log."""
    import contextlib
    import io

    src = tmp / "rio-planted/src"
    src.mkdir(parents=True)
    gf = tmp / "grandfather.txt"

    def write(body: str) -> None:
        (src / "lib.rs").write_text(body, encoding="utf-8")

    def quiet_run() -> int:
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return run(tmp, gf, mint=False)

    # (A0, WO-S8-3 / W12-BA) population floors: an empty root reds the
    # walk; a missing explicitly-passed ledger reds outside mint mode.
    import tempfile as _tf

    with _tf.TemporaryDirectory() as _etd:
        _e = Path(_etd)
        ff = floor_fails(_e, _e / "no-ledger.txt", mint=False)
        if len(ff) != 2:
            return f"floor plant: expected walk+ledger reds, got {ff}"
        ff = floor_fails(_e, _e / "no-ledger.txt", mint=True)
        if len(ff) != 1:
            return f"floor plant: mint mode must exempt only the ledger arm, got {ff}"

    # (A) unenrolled claim -> fail.
    write("// the helper has exactly one caller in the dispatch path\nfn planted() {}\n")
    gf.write_text("", encoding="utf-8")
    if quiet_run() != 1:
        return "arm A: planted unenrolled claim line did not fail"

    # (B) enrolled, resolvable -> pass.
    write(
        "// the helper has exactly one caller — census[test: planted_pin]\n"
        "fn planted_pin() {}\n"
    )
    if quiet_run() != 0:
        return "arm B: planted enrolled+resolvable line did not pass"

    # (C) enrolled, unresolvable -> fail.
    write("// the helper has exactly one caller — census[test: no_such_fn_anywhere]\n")
    if quiet_run() != 1:
        return "arm C: planted dangling enrollment did not fail"

    # (D) stale grandfather entry -> fail (clean tree, nonempty file).
    write("fn planted_clean() {}\n")
    gf.write_text("rio-planted/src/lib.rs\t// some long-deleted claim\n", encoding="utf-8")
    if quiet_run() != 1:
        return "arm D: planted stale grandfather entry did not fail"

    # (E) merged_bug_149: a spelled count PAST "three" is a claim —
    # the old alternation stopped at three, so this line escaped the
    # census silently (the red this plant pins).
    write("// the fold has exactly four call sites in the actor\nfn planted_four() {}\n")
    gf.write_text("", encoding="utf-8")
    if quiet_run() != 1:
        return "arm E: planted spelled-count-four claim did not fail"
    return None


def main() -> int:
    args = sys.argv[1:]
    mint = "--mint-grandfather" in args
    if mint:
        args.remove("--mint-grandfather")
    grandfather = None
    if "--grandfather" in args:
        i = args.index("--grandfather")
        grandfather = Path(args[i + 1])
        del args[i : i + 2]
    if len(args) != 1:
        print(
            "usage: census_enrollment.py [--mint-grandfather] "
            "[--grandfather FILE] <src-root>",
            file=sys.stderr,
        )
        return 2

    # Shared-lexer selftest gates first (fail-closed before any scan).
    err = rust_strip.selftest()
    if err is not None:
        print(f"FAIL: shared lexer selftest: {err}", file=sys.stderr)
        return 2

    if not mint:
        import tempfile

        with tempfile.TemporaryDirectory() as td:
            err = selftest(Path(td))
        if err is not None:
            print(f"FAIL: census-enrollment selftest: {err}", file=sys.stderr)
            return 2

    return run(Path(args[0]), grandfather, mint)


if __name__ == "__main__":
    sys.exit(main())
