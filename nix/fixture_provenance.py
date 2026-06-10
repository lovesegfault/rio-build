#!/usr/bin/env python3
"""fixture-provenance lint (R13, bughunt-5 WO-S8-11, scope+sanction
closed round-6 WO-S8-1; see nix/misc-checks.nix).

Argv: [--census] <src-root>

Machine witnesses are minted through PRODUCTION CONSTRUCTORS only: test
fixtures may NOT hand-roll wire/identity shapes the producing crate
cannot emit (the round-5 banner, RC-1). A fix certified by a
fabricated-world test is treated as NO test — the bug_071/bug_077
lesson. This lint enforces the discipline mechanically over the named
corpus shapes; review carries it everywhere else.

Arms are deliberately NARROW — named patterns from the round-5 corpus,
not heuristics; false-positive pressure routes to the allow-comment
lane, never to weakening an arm:

  A executor-id-override   `.executor_id =` in controller pool TEST
                           code (bug_071: pod-shaped executor_ids the
                           production mint — which derives them from
                           the intent — cannot emit).
  B exposure-uid-input     `event_uid: Some("exposure:…` constructions
                           in TEST code: the uid FORMAT is owned by the
                           shared typed constructor (the producer's own
                           golden output-asserts do not match this
                           input-minting shape).
  C objectmeta-uid-literal `uid: Some("uid-…` in controller TEST code
                           (bug_089: ObjectMeta uids are UUID-shaped;
                           a reusable literal fabricates an identity
                           the apiserver cannot mint).
  D direct-handle-call     `.handle_completion(` in scheduler TEST
                           code (bug_077: bypasses the typed admission
                           the production dispatch enforces).

Sanction lane (TI-3, the CLOSED alphabet): an inline comment on the
fixture line or the line above of the form

    // r13-allow(<lane>): <reason>

with <lane> ∈ {refusal-probe, frozen-legacy, opaque-consumer} mapping
to R13's exceptions (i)/(ii)/(iii) and a mandatory non-empty reason.
Any other lane token is a hit (the alphabet is closed — the planted
UNSANCTIONED-lane selftest red pins this); a banned shape with no
comment is a hit. Sanction tokens are read ONLY from lexer-identified
COMMENT SPANS (merged_bug_073 hole 2 closed): a token inside a
string/char literal is structurally invisible — the grammar's own lane
is the comment, so only a comment can speak it.

Test scope is resolved from the MODULE GRAPH, fail-closed
(merged_bug_073 hole 1 closed): per crate root present in the scan
roots (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`) the scanner walks
`mod NAME;` declarations on comment-blanked text, resolving NAME →
`NAME.rs` / `NAME/mod.rs`. A file is TEST-scoped when its declaration
carries `#[cfg(test)]`, when the file head carries `#![cfg(test)]`,
when its path matches the test-file conventions, or transitively when
its declaring module is test-scoped. Inline `#[cfg(test)] mod … { … }`
spans inside production files stay scanned as before. FAIL-CLOSED
RESIDUE RULE: a scanned `.rs` file (crate roots excepted) that NO
declaration reaches is a lint ERROR — unrecognized never again means
exempt. Item-level `#[cfg(test)]` on non-mod items remains span
residue: the conversion census (commit body, WO-S8-1) proves it empty
of arm shapes; extend the recognizer before relying on it.

Token-accurate scanning via the shared exact lexer (nix/rust_strip.py):
comments are blanked before matching (a commented-out shape cannot
fire), string contents stay visible (arms B/C match into literals);
allow-comments are read from the lexer's comment spans of the ORIGINAL
text. Inline `#[cfg(test)] mod … { … }` spans are located with the
same brace-matching shape the streaming-open ban uses (on blanked
text, so braces are real).

Per the signed fail-closed rule the selftest corpus (one planted red +
one green per arm, the UNSANCTIONED-lane red, and the round-6 scope
and sanction-lane reds: out-of-line declaration, inner attribute,
string-literal token, unresolvable module) runs first; any selftest
miss exits 1 before the real scan may gate.

`--census` prints the file → scope-verdict table with the provenance
edge per file (`declaration <file>:<line>`, `inner-attr :<line>`,
`filename`, `parent <file>`, `crate-root`) — the conversion census is
generator output (banner rule 1): the committed member list is this
flag's stdout, never an author-typed table.
"""

import pathlib
import re
import sys

import rust_strip

LANES = {"refusal-probe", "frozen-legacy", "opaque-consumer"}
ALLOW = re.compile(r"r13-allow\(([a-z-]+)\):\s*\S")

CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
CFG_TEST_INNER = re.compile(r"#\s*!\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
MOD_AFTER = re.compile(
    r"\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+(?:r#)?\w+\s*([;{])"
)
# Out-of-line module declaration with its leading attribute run, at
# line start (the in-tree shape; an exotic mid-line declaration leaves
# its target file unreached → the fail-closed residue rule errors).
# `r#` raw identifiers resolve to the bare name (`mod r#override;` →
# override.rs — found by the residue rule's first real-tree run).
DECL = re.compile(
    r"^[ \t]*((?:#\s*\[[^\]]*\]\s*)*)(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+(?:r#)?(\w+)\s*;",
    re.MULTILINE,
)

# (name, scope-predicate over repo-relative path, pattern over
#  comment-blanked text, production-alternative message)
ARMS = [
    (
        "executor-id-override",
        lambda rel: rel.startswith("rio-controller/src/reconcilers/pool/"),
        re.compile(r"\.executor_id\s*=[^=]"),
        "hand-rolled executor_id override (bug_071) — the production mint "
        "derives executor ids from the intent; drive the production "
        "constructor instead",
    ),
    (
        "exposure-uid-input",
        lambda rel: rel.startswith(("rio-scheduler/src/", "rio-controller/src/")),
        re.compile(r'event_uid:\s*Some\(\s*"exposure:'),
        "hand-rolled exposure uid input — the uid FORMAT is owned by the "
        "shared typed constructor; opaque-key contract tests tag "
        "r13-allow(opaque-consumer)",
    ),
    (
        "objectmeta-uid-literal",
        lambda rel: rel.startswith("rio-controller/src/"),
        re.compile(r'uid:\s*Some\(\s*"uid-'),
        "literal 'uid-…' ObjectMeta uid (bug_089) — apiserver uids are "
        "UUID-shaped; mint via the production object constructor",
    ),
    (
        "direct-handle-call",
        lambda rel: rel.startswith("rio-scheduler/src/"),
        re.compile(r"\.handle_completion\("),
        "direct handle_completion call (bug_077) — bypasses the typed "
        "admission; drive the production dispatch path",
    ),
]

SCAN_ROOTS = ["rio-controller/src", "rio-scheduler/src"]


def is_test_file(rel: str) -> bool:
    name = rel.rsplit("/", 1)[-1]
    return (
        "/tests/" in rel
        or name == "tests.rs"
        or name.endswith("_tests.rs")
        or name == "test_helpers.rs"
    )


def is_crate_root(rel: str) -> bool:
    parts = rel.split("/")
    if len(parts) >= 2 and parts[-1] in ("lib.rs", "main.rs") and parts[-2] == "src":
        return True
    return len(parts) >= 3 and parts[-2] == "bin" and parts[-3] == "src"


def cfg_test_spans(blanked: str) -> list[tuple[int, int]]:
    """Spans of inline `#[cfg(test)] mod … { … }` bodies, brace-matched
    on comment/char-blanked text (string bodies blanked too — braces
    are real). Same recognizer shape as streaming_open_ban's stripper,
    pointed the opposite way: we scan ONLY inside these spans."""
    spans = []
    pos = 0
    while True:
        m = CFG_TEST.search(blanked, pos)
        if not m:
            return spans
        after = MOD_AFTER.match(blanked, m.end())
        if not after or after.group(1) != "{":
            pos = m.end()
            continue
        depth = 0
        j = after.end() - 1
        while j < len(blanked):
            if blanked[j] == "{":
                depth += 1
            elif blanked[j] == "}":
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            j += 1
        spans.append((m.start(), j))
        pos = j


def mod_decls(blanked: str) -> list[tuple[int, str, str]]:
    """Out-of-line `mod NAME;` declarations on comment-blanked text:
    `(line_no_of_match_start, attribute_run, NAME)`."""
    decls = []
    for m in DECL.finditer(blanked):
        line_no = blanked.count("\n", 0, m.start()) + 1
        decls.append((line_no, m.group(1), m.group(2)))
    return decls


def child_candidates(rel: str, name: str) -> list[str]:
    """Resolution candidates for `mod name;` declared in `rel` (2018
    module rules: roots and mod.rs resolve beside themselves, any
    other file resolves under its own directory)."""
    d, _, fname = rel.rpartition("/")
    if fname in ("lib.rs", "main.rs", "mod.rs"):
        base = d
    else:
        base = f"{d}/{fname[: -len('.rs')]}"
    return [f"{base}/{name}.rs", f"{base}/{name}/mod.rs"]


def walk_scope(
    files: dict[str, str], blanked: dict[str, str]
) -> tuple[dict[str, tuple[bool, str]], list[str]]:
    """Module-graph walk from every crate root in the file set.
    Returns `({rel: (is_test, provenance_edge)}, unresolved_files)` —
    the residue list is the fail-closed half: a file in `files` that
    no walk reaches is a lint error at the caller."""
    inner_at: dict[str, int | None] = {}
    for rel, b in blanked.items():
        m = CFG_TEST_INNER.search(b)
        inner_at[rel] = (b.count("\n", 0, m.start()) + 1) if m else None
    scope: dict[str, tuple[bool, str]] = {}
    roots = sorted(r for r in files if is_crate_root(r))
    for root in roots:
        scope[root] = (inner_at[root] is not None, "crate-root")
    for root in roots:
        stack = [root]
        while stack:
            rel = stack.pop()
            rel_test = scope[rel][0]
            for line_no, attrs, name in mod_decls(blanked[rel]):
                decl_test = bool(CFG_TEST.search(attrs))
                for cand in child_candidates(rel, name):
                    if cand not in files:
                        continue
                    if inner_at[cand] is not None:
                        tedge = f"inner-attr :{inner_at[cand]}"
                    elif decl_test:
                        tedge = f"declaration {rel}:{line_no}"
                    elif is_test_file(cand):
                        tedge = "filename"
                    elif rel_test:
                        tedge = f"parent {rel}"
                    else:
                        tedge = None
                    is_test = tedge is not None
                    edge = tedge if is_test else f"declaration {rel}:{line_no}"
                    if cand not in scope:
                        scope[cand] = (is_test, edge)
                        stack.append(cand)
                    elif is_test and not scope[cand][0]:
                        # An earlier edge classified it production; the
                        # test edge wins (deny-side: scan MORE) and its
                        # descendants re-walk under test scope.
                        scope[cand] = (is_test, edge)
                        stack.append(cand)
                    break
    unresolved = sorted(r for r in files if r not in scope)
    return scope, unresolved


def allow_verdict(
    orig_lines: list[str],
    line_starts: list[int],
    comments: list[tuple[int, int]],
    line_no: int,
) -> str | None:
    """The sanction verdict for a hit at 1-based `line_no`: the lane
    name when a well-formed allow-comment with a CLOSED-alphabet lane
    sits IN A COMMENT SPAN on the hit line or the line above;
    "UNSANCTIONED:<lane>" for a comment-lane token with an
    out-of-alphabet lane; None when no comment-lane token is present
    (a token inside a string/char literal is structurally invisible)."""
    for ln in (line_no, line_no - 1):
        if not 1 <= ln <= len(orig_lines):
            continue
        for m in ALLOW.finditer(orig_lines[ln - 1]):
            pos = line_starts[ln - 1] + m.start()
            if not any(a <= pos < b for a, b in comments):
                continue
            lane = m.group(1)
            if lane in LANES:
                return lane
            return f"UNSANCTIONED:{lane}"
    return None


def scan_arms(
    rel: str, text: str, in_test: list[tuple[int, int]]
) -> tuple[list[str], list[str]]:
    """Arm engine over one file given its test-scope spans. Returns
    (hits, sanctioned) — hit strings for unsanctioned banned shapes
    (including closed-alphabet violations) and census lines for
    sanctioned ones."""
    if not in_test:
        return [], []
    # Strings visible for the arms; comments blanked. The SAME walk
    # yields the comment spans the sanction lane is confined to.
    lexed, _spans, comments = rust_strip.lex_full(text, blank_string_bodies=False)
    orig_lines = text.splitlines()
    line_starts = [0]
    for line in orig_lines:
        line_starts.append(line_starts[-1] + len(line) + 1)
    hits: list[str] = []
    sanctioned: list[str] = []
    for name, in_scope, pat, msg in ARMS:
        if not in_scope(rel):
            continue
        for m in pat.finditer(lexed):
            if not any(a <= m.start() < b for a, b in in_test):
                continue
            line_no = text.count("\n", 0, m.start()) + 1
            verdict = allow_verdict(orig_lines, line_starts, comments, line_no)
            if verdict is None:
                hits.append(f"{rel}:{line_no}: [{name}] {msg}")
            elif verdict.startswith("UNSANCTIONED:"):
                lane = verdict.split(":", 1)[1]
                hits.append(
                    f"{rel}:{line_no}: [{name}] r13-allow({lane}) is not in the "
                    f"CLOSED lane alphabet {sorted(LANES)} — the sanction "
                    "grammar admits no new lanes without a wave ruling"
                )
            else:
                sanctioned.append(f"{rel}:{line_no}: [{name}] r13-allow({verdict})")
    return hits, sanctioned


def scan_files(
    files: dict[str, str],
) -> tuple[list[str], list[str], list[str], list[str]]:
    """Scope walk + arm scan over a file set (`{rel_path: text}`).
    Returns (errors, hits, sanctioned, census)."""
    blanked = {
        rel: rust_strip.lex(t, blank_string_bodies=True)[0] for rel, t in files.items()
    }
    scope, unresolved = walk_scope(files, blanked)
    errors = [
        f"unresolvable module scope: {rel} — no `mod` declaration reaches it "
        "(fail-closed: an unrecognized module form is an error, never an "
        "exemption; extend the walk or remove the orphan)"
        for rel in unresolved
    ]
    hits: list[str] = []
    sanctioned: list[str] = []
    census: list[str] = []
    for rel in sorted(files):
        if rel not in scope:
            census.append(f"{rel}: UNRESOLVED")
            continue
        is_test, edge = scope[rel]
        if is_test:
            in_test = [(0, len(files[rel]))]
            census.append(f"{rel}: test ({edge})")
        else:
            in_test = cfg_test_spans(blanked[rel])
            suffix = f" [+{len(in_test)} inline cfg(test) span(s)]" if in_test else ""
            census.append(f"{rel}: production ({edge}){suffix}")
        h, s = scan_arms(rel, files[rel], in_test)
        hits.extend(h)
        sanctioned.extend(s)
    return errors, hits, sanctioned, census


def _pool_corpus(jobs_tests: str) -> dict[str, str]:
    """Controller pool-tests corpus: the full declaration chain, so
    every arm selftest also exercises the module walk."""
    return {
        "rio-controller/src/lib.rs": "pub mod reconcilers;\n",
        "rio-controller/src/reconcilers/mod.rs": "pub mod pool;\n",
        "rio-controller/src/reconcilers/pool/mod.rs": "#[cfg(test)]\nmod tests;\n",
        "rio-controller/src/reconcilers/pool/tests/mod.rs": "mod jobs_tests;\n",
        "rio-controller/src/reconcilers/pool/tests/jobs_tests.rs": jobs_tests,
    }


def selftest() -> list[str]:
    """Planted reds and greens; returns ALL misses (the pre-fix run
    prints every red that did not fire, not just the first)."""
    misses: list[str] = []

    # Arm A red: the bug_071 shape verbatim, in pool test scope.
    red_a = 'fn t() { owned.executor_id = "rio-builder-p-pull1-a1b2c".into(); }\n'
    _, h, _, _ = scan_files(_pool_corpus(red_a))
    if not h:
        misses.append("arm A planted red did not fire (executor_id override)")
    # Arm A green: production comparison (==) is not an override; and
    # the same shape OUTSIDE test scope does not fire.
    _, g, _, _ = scan_files(_pool_corpus("fn t() { assert!(a.executor_id == *pod); }\n"))
    if g:
        misses.append(f"arm A green fixture flagged: {g}")
    _, g, _, _ = scan_files(
        {
            "rio-controller/src/lib.rs": "pub mod reconcilers;\n",
            "rio-controller/src/reconcilers/mod.rs": "pub mod pool;\n",
            "rio-controller/src/reconcilers/pool/mod.rs": "pub mod job;\n",
            "rio-controller/src/reconcilers/pool/job.rs": red_a,
        }
    )
    if g:
        misses.append("arm A fired outside test scope (production plane)")
    # Arm A sanctioned: a tagged probe is censused, not a hit.
    _, h, s, _ = scan_files(
        _pool_corpus(
            "// r13-allow(refusal-probe): asserts the typed refusal\n"
            'fn t() { owned.executor_id = "ghost".into(); }\n'
        )
    )
    if h or not s:
        misses.append(f"arm A allow-tag not honored: hits={h} sanctioned={s}")
    # UNSANCTIONED lane: the alphabet is CLOSED.
    _, h, s, _ = scan_files(
        _pool_corpus(
            "// r13-allow(not-a-lane): nice try\n"
            'fn t() { owned.executor_id = "ghost".into(); }\n'
        )
    )
    if not h or s:
        misses.append("the UNSANCTIONED-lane red did not fire (closed alphabet broken)")
    # Round-6 sanction-lane red: a token inside a STRING LITERAL on the
    # hit line must NOT sanction — the lane grammar is comments only.
    _, h, _, _ = scan_files(
        _pool_corpus(
            'fn t() { let s = "r13-allow(refusal-probe): x"; '
            'owned.executor_id = "ghost".into(); }\n'
        )
    )
    if not h:
        misses.append(
            "a string-literal allow token sanctioned a hit "
            "(sanction lane is comments only)"
        )

    # Arm B red: the consuming-side uid mint.
    _, h, _, _ = scan_files(
        {
            "rio-scheduler/src/lib.rs": "pub mod admin;\n",
            "rio-scheduler/src/admin/mod.rs": "#[cfg(test)]\nmod tests;\n",
            "rio-scheduler/src/admin/tests/mod.rs": (
                'let r = Req { event_uid: Some("exposure:aws-8-nvme-hi:'
                '1767225600".into()) };\n'
            ),
        }
    )
    if not h:
        misses.append("arm B planted red did not fire (exposure uid input)")
    # Arm B green: the producer's golden OUTPUT assert does not match
    # the input-minting shape; a commented-out shape cannot fire.
    _, g, _, _ = scan_files(
        {
            "rio-controller/src/lib.rs": "pub mod reconcilers;\n",
            "rio-controller/src/reconcilers/mod.rs": "pub mod node_informer;\n",
            "rio-controller/src/reconcilers/node_informer.rs": (
                "#[cfg(test)]\nmod tests {\n    fn t() {\n"
                '        assert_eq!(original.uid, "exposure:m6id:1700000000");\n'
                '        // event_uid: Some("exposure:x:1")\n    }\n}\n'
            ),
        }
    )
    if g:
        misses.append(f"arm B green fixture flagged: {g}")

    # Arm C red + green.
    _, h, _, _ = scan_files(
        _pool_corpus('let meta = ObjectMeta { uid: Some("uid-1".into()) };\n')
    )
    if not h:
        misses.append("arm C planted red did not fire (literal uid)")
    _, g, _, _ = scan_files(
        _pool_corpus(
            "let meta = ObjectMeta { uid: Some(uuid::Uuid::new_v4().to_string()) };\n"
        )
    )
    if g:
        misses.append(f"arm C green fixture flagged: {g}")

    # Arm D red + green (production dispatch call site is out of test
    # scope; a call in a walked tests module fires).
    _, h, _, _ = scan_files(
        {
            "rio-scheduler/src/lib.rs": "pub mod actor;\n",
            "rio-scheduler/src/actor/mod.rs": "#[cfg(test)]\npub(crate) mod tests;\n",
            "rio-scheduler/src/actor/tests/mod.rs": "mod completion;\n",
            "rio-scheduler/src/actor/tests/completion.rs": (
                "async fn t() { actor\n        .handle_completion(report).await; }\n"
            ),
        }
    )
    if not h:
        misses.append("arm D planted red did not fire (direct handle_completion)")
    _, g, _, _ = scan_files(
        {
            "rio-scheduler/src/lib.rs": "pub mod actor;\n",
            "rio-scheduler/src/actor/mod.rs": (
                "impl Actor { async fn dispatch(&mut self) "
                "{ self.handle_completion(w).await; } }\n"
            ),
        }
    )
    if g:
        misses.append("arm D fired outside test scope (production dispatch)")

    # Round-6 scope red 1: the OUT-OF-LINE declaration form — the
    # `#[cfg(test)]` lives on `mod fixtures;` in lib.rs, the banned
    # shape in the module FILE (the rio-controller/src/fixtures.rs
    # blind spot verbatim). Arm C must fire in fixtures.rs.
    errs, h, _, census = scan_files(
        {
            "rio-controller/src/lib.rs": "#[cfg(test)]\npub(crate) mod fixtures;\n",
            "rio-controller/src/fixtures.rs": (
                'let meta = ObjectMeta { uid: Some("uid-1".into()) };\n'
            ),
        }
    )
    if not h:
        misses.append(
            "out-of-line cfg(test) module red did not fire "
            "(declaration-site scope unrecognized)"
        )
    elif errs:
        misses.append(f"out-of-line corpus errored: {errs}")
    elif (
        "rio-controller/src/fixtures.rs: test "
        "(declaration rio-controller/src/lib.rs:1)" not in census
    ):
        misses.append(f"out-of-line census edge wrong: {census}")

    # Round-6 scope red 2: the INNER-ATTRIBUTE form — the declaration
    # is PLAIN (`mod debug;`), so the kill goes through the
    # `#![cfg(test)]` recognizer alone (the actor/debug.rs blind spot,
    # isolated). Arm D must fire in debug.rs.
    errs, h, _, census = scan_files(
        {
            "rio-scheduler/src/lib.rs": "pub mod actor;\n",
            "rio-scheduler/src/actor/mod.rs": "mod debug;\n",
            "rio-scheduler/src/actor/debug.rs": (
                "#![cfg(test)]\nfn t() { actor.handle_completion(r); }\n"
            ),
        }
    )
    if not h:
        misses.append(
            "#![cfg(test)] file red did not fire (inner-attribute scope unrecognized)"
        )
    elif errs:
        misses.append(f"inner-attribute corpus errored: {errs}")
    elif "rio-scheduler/src/actor/debug.rs: test (inner-attr :1)" not in census:
        misses.append(f"inner-attribute census edge wrong: {census}")

    # Round-6 residue red: a file NO declaration reaches is an ERROR
    # (fail-closed), and the reached sibling resolves cleanly.
    errs, _, _, _ = scan_files(
        {
            "rio-controller/src/lib.rs": "pub mod config;\n",
            "rio-controller/src/config.rs": "fn c() {}\n",
            "rio-controller/src/orphan.rs": "fn o() {}\n",
        }
    )
    if len(errs) != 1 or "rio-controller/src/orphan.rs" not in errs[0]:
        misses.append(
            "unresolvable-module red did not error (unreached file silently exempt)"
        )

    return misses


def main() -> int:
    shared_err = rust_strip.selftest()
    if shared_err:
        print(f"FAIL: rust-strip self-test — {shared_err}", file=sys.stderr)
        return 1
    misses = selftest()
    if misses:
        for miss in misses:
            print(f"FAIL: fixture-provenance self-test — {miss}", file=sys.stderr)
        return 1
    args = sys.argv[1:]
    census_mode = "--census" in args
    args = [a for a in args if a != "--census"]
    src_root = pathlib.Path(args[0])
    files: dict[str, str] = {}
    for root in SCAN_ROOTS:
        droot = src_root / root
        if not droot.exists():
            continue
        for f in sorted(droot.rglob("*.rs")):
            files[str(f.relative_to(src_root))] = f.read_text()
    errors, fails, sanctioned, census = scan_files(files)
    if census_mode:
        print(f"fixture-provenance census ({len(files)} files):")
        for line in census:
            print(f"  {line}")
    else:
        print(
            f"fixture-provenance: scanned {len(files)} files "
            f"({len(sanctioned)} sanctioned fixture(s))"
        )
    for s in sanctioned:
        print(f"  allow: {s}")
    rc = 0
    if errors:
        print(
            "FAIL: module scope unresolvable — the walk could not reach these\n"
            "files from any crate root (fail-closed residue rule):",
            file=sys.stderr,
        )
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        rc = 1
    if fails:
        print(
            "FAIL: hand-rolled wire/identity fixture shape(s) — machine\n"
            "witnesses are minted through production constructors (R13);\n"
            "sanctioned exceptions tag `// r13-allow(<lane>): <reason>` with\n"
            "<lane> in {refusal-probe, frozen-legacy, opaque-consumer}:",
            file=sys.stderr,
        )
        for hit in fails:
            print(f"  {hit}", file=sys.stderr)
        rc = 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
