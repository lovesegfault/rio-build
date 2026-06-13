#!/usr/bin/env python3
"""Doc-link adjacency lint (WO-S4-5, D-4b -- the merged_bug_002
class-killer; T3).

THE DEFECT CLASS: a mechanical doc-link qualification rewrite
appended an explicit target to existing markdown links and pasted it
TWICE -- `[`X`](path)(path)` -- so CommonMark consumes the first
parenthesized group as the link destination and renders the second
verbatim as stray fully-qualified-path prose. rustdoc -D warnings
stays green (the link itself resolves) and every standing
census/predicate instrument has no signal: rendered-doc prose
quality is outside all enforcement (the merged_bug_002 process-clean
evasion).

NEEDLE (derived from the CommonMark inline-link grammar, not the
single observed instance): a link destination group `](...)` followed
IMMEDIATELY by a second parenthesized group -- the `)(` adjacency.
The second group cannot be a destination (the grammar consumes
exactly one); whatever it carries renders as prose.

POPULATION: doc-comment lines (`///`, `//!`) across the workspace
rust sources (rio-*/src + xtask/src), via the shared rust_strip
lexer's comment lane. Non-doc `//` lines are excluded (a `)(` in a
plain comment is not rendered).

R31'-compliant from birth (the bug_047 lesson): the planted-red
fixture (W14-D4) is the verbatim pre-fix capacity_term.rs:9 line
inside the governed rust population; the K-mutation self-test
(W14-D5, K=4) runs through the shared census_corpora harness; the
population floor reds an empty walk ((vvvvv)).

Exit: 0 clean; 1 violations; 2 usage.
"""

import pathlib
import re
import sys

import census_corpora
import rust_strip

# The CommonMark inline-link adjacency: `](dest)(` -- a destination
# group immediately followed by an opening paren. The dest excludes
# `)` (CommonMark's `<...>`-unwrapped destination grammar).
ADJACENCY_RE = re.compile(r"\]\([^)]*\)\(")


def doc_comment_lines(text: str):
    """line number (1-based) -> doc-comment text on that line, via the
    shared lexer's comment lane, narrowed to `///` and `//!` (the
    rendered surface)."""
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
            stripped = piece.lstrip()
            if stripped.startswith("///") or stripped.startswith("//!"):
                out[line_no] = out.get(line_no, "") + piece
            line_no += 1
    return out


def iter_rust_sources(root: pathlib.Path):
    for crate_src in sorted(root.glob("rio-*/src")):
        yield from sorted(crate_src.rglob("*.rs"))
    x = root / "xtask" / "src"
    if x.is_dir():
        yield from sorted(x.rglob("*.rs"))


def scan(root: pathlib.Path):
    """Yield (rel, lineno, trimmed-line) for every doc-comment line
    carrying the `](dest)(` adjacency."""
    for f in iter_rust_sources(root):
        rel = f.relative_to(root).as_posix()
        try:
            text = f.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        raw_lines = text.splitlines()
        for lineno, comment in sorted(doc_comment_lines(text).items()):
            if ADJACENCY_RE.search(comment):
                line = raw_lines[lineno - 1] if lineno <= len(raw_lines) else comment
                yield rel, lineno, line.strip()


def floor_fails(root: pathlib.Path) -> list:
    """Population floor (merged_bug_028, (vvvvv)): the rust-source
    walk must yield at least one file -- a mis-staged tree scans an
    empty population green otherwise."""
    if not any(True for _ in iter_rust_sources(root)):
        return [
            "population floor -- the rust-source walk resolved zero "
            "files under the scan root (mis-staged tree? ((vvvvv)))"
        ]
    return []


def run(root: pathlib.Path) -> int:
    floors = floor_fails(root)
    if floors:
        for x in floors:
            print(x, file=sys.stderr)
        return 1
    out = []
    for rel, lineno, _line in scan(root):
        out.append(
            f"{rel}:{lineno}: doc-link `](...)` immediately followed by `(` "
            f"-- a duplicated link target (the second group renders as "
            f"stray prose; merged_bug_002). Drop the duplicate or use the "
            f"shorthand `[`X`]` form."
        )
    if out:
        for v in out:
            print(v, file=sys.stderr)
        print(f"\n{len(out)} doc-link adjacency violation(s).", file=sys.stderr)
        return 1
    print("doc-link-adjacency: workspace doc-comment lines clean")
    return 0


def self_battery(_src_root) -> list:
    """W14-D4 plant battery (failure-collecting; the W13-BE
    grounding -- never invokes the K-mutation harness)."""
    import tempfile

    fails = []
    # Population floor plant: an empty root reds.
    with tempfile.TemporaryDirectory() as td:
        if not floor_fails(pathlib.Path(td)):
            fails.append("the population-floor plant did not red on an empty root")
    with tempfile.TemporaryDirectory() as td:
        root = pathlib.Path(td)
        p = root / "rio-planted" / "src" / "lib.rs"
        p.parent.mkdir(parents=True, exist_ok=True)
        # W14-D4 RED: the verbatim pre-fix capacity_term.rs:9 text
        # (the merged_bug_002 exhibit; the repair and the lint share
        # one fixture so neither can drift).
        merged_bug_002_dup = (
            "//! [`WireCapacity`]" + "(crate::cell_wire::WireCapacity)"
            "(crate::cell_wire::WireCapacity) alphabet. This decoder "
            "is a TOTAL match over the\n"
        )
        p.write_text(merged_bug_002_dup + "fn x() {}\n", encoding="utf-8")
        got = list(scan(root))
        if len(got) != 1:
            fails.append(f"W14-D4: the merged_bug_002 pre-fix dup did not red: {got}")
        # The single-target repair (the post-fix text) passes.
        merged_bug_002_repaired = (
            "//! [`WireCapacity`]" + "(crate::cell_wire::WireCapacity) "
            "alphabet. This decoder is a TOTAL match over the\n"
        )
        p.write_text(merged_bug_002_repaired + "fn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            fails.append("W14-D4: the single-target repair did not pass")
        # A single-target link followed by an UNRELATED parenthetical
        # later on the same line passes (the adjacency is `)(`, not
        # `)...(` -- the K-mutation oracle for adjacency-window-broken).
        p.write_text(
            "//! see [`X`]" + "(crate::x::X) and the (other) law\nfn x() {}\n",
            encoding="utf-8",
        )
        if list(scan(root)):
            fails.append("W14-D4: a single-target link with a later `(` fired (adjacency window broken)")
        # A `///` outer doc carrying the adjacency reds (the //!///
        # narrowing covers both rendered forms).
        p.write_text("/// [`X`]" + "(a::b)(a::b) is the law\nfn x() {}\n", encoding="utf-8")
        if len(list(scan(root))) != 1:
            fails.append("W14-D4: a `///` outer-doc adjacency did not red")
        # A plain `//` non-doc comment with the adjacency does NOT
        # fire (not rendered).
        p.write_text("// see [`X`]" + "(a::b)(a::b) for the law\nfn x() {}\n", encoding="utf-8")
        if list(scan(root)):
            fails.append("W14-D4: a plain `//` non-doc adjacency fired (not rendered)")
        # A string literal carrying the shape never fires (the lexer's
        # comment lane).
        p.write_text('fn x() { let s = "[`X`]' + '(a)(a)"; }\n', encoding="utf-8")
        if list(scan(root)):
            fails.append("W14-D4: a string-literal adjacency fired")
    return fails


# W14-D5 (R31'(iii)): the K-mutation self-test -- K=4 seeded
# degenerations of the lint, each REQUIRED to kill the W14-D4 plant
# battery via the shared harness. Needles concatenation-split per the
# (ooooo) probe-needle note.
MUTATIONS = [
    (
        "needle-widened-to-never-match",
        "ADJACENCY_RE widened to never-match -- killed by the W14-D4"
        " merged_bug_002 dup plant",
        "ADJACENCY_RE = re.compile(r\"\\]\\([^)]*\\)" + "\\(\")",
        "ADJACENCY_RE = re.compile(r\"NEVER_MATCH" + "ES_ZZZ\")",
    ),
    (
        "population-emptied",
        "iter_rust_sources emptied -- killed by the population-floor"
        " plant",
        "    for crate_src in sorted(root.glob(\"rio-*" + "/src\")):",
        "    for crate_src in " + "[]:",
    ),
    (
        "adjacency-window-broken",
        "the `)(` adjacency relaxed to `)...(` (any gap admitted) --"
        " killed by the W14-D4 single-target repair plant (a"
        " single-target link followed by prose `(` would now"
        " false-positive)",
        "ADJACENCY_RE = re.compile(r\"\\]\\([^)]*\\)" + "\\(\")",
        "ADJACENCY_RE = re.compile(r\"\\]\\([^)]*\\)" + ".*\\(\")",
    ),
    (
        "doc-narrowing-dropped",
        "the `///`+`//!` narrowing dropped (plain `//` comments enter"
        " the population) -- killed by the W14-D4 plain-`//` plant",
        "            if stripped.startswith(\"//" + "/\") or stripped.startswith(\"//" + "!\"):",
        "            if " + "True:",
    ),
]


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: doc_link_adjacency.py <repo-root>", file=sys.stderr)
        return 2
    err = rust_strip.selftest()
    if err:
        print(f"FAIL: shared lexer self-test -- {err}", file=sys.stderr)
        return 1
    battery = self_battery(pathlib.Path(sys.argv[1]))
    if battery:
        print("FAIL: doc-link-adjacency self-battery --", file=sys.stderr)
        for x in battery:
            print(f"  {x}", file=sys.stderr)
        return 1
    killed = census_corpora.run_mutation_battery(
        pathlib.Path(__file__), MUTATIONS, "self_battery", (pathlib.Path(sys.argv[1]),)
    )
    if killed:
        print("FAIL: doc-link-adjacency K-mutation battery --", file=sys.stderr)
        for x in killed:
            print(f"  {x}", file=sys.stderr)
        return 1
    return run(pathlib.Path(sys.argv[1]))


if __name__ == "__main__":
    sys.exit(main())
