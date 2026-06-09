#!/usr/bin/env python3
"""fixture-provenance lint (R13, bughunt-5 WO-S8-11; see nix/misc-checks.nix).

Argv: <src-root>

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
comment is a hit.

Token-accurate scanning via the shared exact lexer (nix/rust_strip.py):
comments are blanked before matching (a commented-out shape cannot
fire), string contents stay visible (arms B/C match into literals);
allow-comments are read from the ORIGINAL text. Inline `#[cfg(test)]
mod … { … }` spans are located with the same brace-matching shape the
streaming-open ban uses (on blanked text, so braces are real).

Per the signed fail-closed rule the selftest corpus (one planted red +
one green per arm, PLUS the UNSANCTIONED-lane red) runs first; any
selftest miss exits 1 before the real scan may gate.
"""

import pathlib
import re
import sys

import rust_strip

LANES = {"refusal-probe", "frozen-legacy", "opaque-consumer"}
ALLOW = re.compile(r"r13-allow\(([a-z-]+)\):\s*\S")

CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
MOD_AFTER = re.compile(r"\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+\w+\s*([;{])")

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


def allow_verdict(orig_lines: list[str], line_no: int) -> str | None:
    """The sanction verdict for a hit at 1-based `line_no`: the lane
    name when a well-formed allow-comment with a CLOSED-alphabet lane
    sits on the hit line or the line above; "UNSANCTIONED:<lane>" for
    an allow-comment with an out-of-alphabet lane; None when no
    allow-comment is present."""
    for ln in (line_no, line_no - 1):
        if 1 <= ln <= len(orig_lines):
            m = ALLOW.search(orig_lines[ln - 1])
            if m:
                lane = m.group(1)
                if lane in LANES:
                    return lane
                return f"UNSANCTIONED:{lane}"
    return None


def scan_text(rel: str, text: str) -> tuple[list[str], list[str]]:
    """Returns (hits, sanctioned) — hit strings for unsanctioned banned
    shapes (including closed-alphabet violations) and census lines for
    sanctioned ones."""
    # Strings visible for the arms; comments blanked.
    lexed, _spans = rust_strip.lex(text, blank_string_bodies=False)
    # Test scope: whole file, or inline cfg(test) mods located on the
    # fully-blanked variant (real braces).
    if is_test_file(rel):
        in_test = [(0, len(text))]
    else:
        fully_blanked, _ = rust_strip.lex(text, blank_string_bodies=True)
        in_test = cfg_test_spans(fully_blanked)
    if not in_test:
        return [], []
    orig_lines = text.splitlines()
    hits: list[str] = []
    sanctioned: list[str] = []
    for name, in_scope, pat, msg in ARMS:
        if not in_scope(rel):
            continue
        for m in pat.finditer(lexed):
            if not any(a <= m.start() < b for a, b in in_test):
                continue
            line_no = text.count("\n", 0, m.start()) + 1
            verdict = allow_verdict(orig_lines, line_no)
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


def selftest() -> str | None:
    """One planted red + green per arm, plus the UNSANCTIONED-lane red."""
    # Arm A red: the bug_071 shape verbatim, in pool test scope.
    red_a = 'fn t() { owned.executor_id = "rio-builder-p-pull1-a1b2c".into(); }\n'
    h, _ = scan_text("rio-controller/src/reconcilers/pool/tests/jobs_tests.rs", red_a)
    if not h:
        return "arm A planted red did not fire (executor_id override)"
    # Arm A green: production comparison (==) is not an override; and
    # the same shape OUTSIDE test scope does not fire.
    g, _ = scan_text(
        "rio-controller/src/reconcilers/pool/tests/jobs_tests.rs",
        "fn t() { assert!(a.executor_id == *pod); }\n",
    )
    if g:
        return f"arm A green fixture flagged: {g}"
    g, _ = scan_text("rio-controller/src/reconcilers/pool/job.rs", red_a)
    if g:
        return "arm A fired outside test scope (production plane)"
    # Arm A sanctioned: a tagged probe is censused, not a hit.
    h, s = scan_text(
        "rio-controller/src/reconcilers/pool/tests/jobs_tests.rs",
        '// r13-allow(refusal-probe): asserts the typed refusal\n'
        'fn t() { owned.executor_id = "ghost".into(); }\n',
    )
    if h or not s:
        return f"arm A allow-tag not honored: hits={h} sanctioned={s}"
    # UNSANCTIONED lane: the alphabet is CLOSED.
    h, s = scan_text(
        "rio-controller/src/reconcilers/pool/tests/jobs_tests.rs",
        '// r13-allow(not-a-lane): nice try\n'
        'fn t() { owned.executor_id = "ghost".into(); }\n',
    )
    if not h or s:
        return "the UNSANCTIONED-lane red did not fire (closed alphabet broken)"
    # Arm B red: the consuming-side uid mint.
    h, _ = scan_text(
        "rio-scheduler/src/admin/tests/mod.rs",
        'let r = Req { event_uid: Some("exposure:aws-8-nvme-hi:1767225600".into()) };\n',
    )
    if not h:
        return "arm B planted red did not fire (exposure uid input)"
    # Arm B green: the producer's golden OUTPUT assert does not match
    # the input-minting shape; a commented-out shape cannot fire.
    g, _ = scan_text(
        "rio-controller/src/reconcilers/node_informer.rs",
        '#[cfg(test)]\nmod tests {\n    fn t() {\n'
        '        assert_eq!(original.uid, "exposure:m6id:1700000000");\n'
        '        // event_uid: Some("exposure:x:1")\n    }\n}\n',
    )
    if g:
        return f"arm B green fixture flagged: {g}"
    # Arm C red + green.
    h, _ = scan_text(
        "rio-controller/src/reconcilers/pool/tests/jobs_tests.rs",
        'let meta = ObjectMeta { uid: Some("uid-1".into()) };\n',
    )
    if not h:
        return "arm C planted red did not fire (literal uid)"
    g, _ = scan_text(
        "rio-controller/src/reconcilers/pool/tests/jobs_tests.rs",
        "let meta = ObjectMeta { uid: Some(uuid::Uuid::new_v4().to_string()) };\n",
    )
    if g:
        return f"arm C green fixture flagged: {g}"
    # Arm D red + green (production dispatch call site is out of test
    # scope; an inline cfg(test) call fires).
    h, _ = scan_text(
        "rio-scheduler/src/actor/tests/completion.rs",
        "async fn t() { actor.handle_completion(report).await; }\n".replace(
            "actor.handle", "actor\n        .handle"
        ),
    )
    if not h:
        return "arm D planted red did not fire (direct handle_completion)"
    g, _ = scan_text(
        "rio-scheduler/src/actor/mod.rs",
        "impl Actor { async fn dispatch(&mut self) { self.handle_completion(w).await; } }\n",
    )
    if g:
        return "arm D fired outside test scope (production dispatch)"
    return None


def main() -> int:
    shared_err = rust_strip.selftest()
    if shared_err:
        print(f"FAIL: rust-strip self-test — {shared_err}", file=sys.stderr)
        return 1
    err = selftest()
    if err:
        print(f"FAIL: fixture-provenance self-test — {err}", file=sys.stderr)
        return 1
    src_root = pathlib.Path(sys.argv[1])
    fails: list[str] = []
    sanctioned: list[str] = []
    scanned = 0
    for root in SCAN_ROOTS:
        droot = src_root / root
        if not droot.exists():
            continue
        for f in sorted(droot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            scanned += 1
            h, s = scan_text(rel, f.read_text())
            fails.extend(h)
            sanctioned.extend(s)
    print(
        f"fixture-provenance: scanned {scanned} files "
        f"({len(sanctioned)} sanctioned fixture(s))"
    )
    for s in sanctioned:
        print(f"  allow: {s}")
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
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
