#!/usr/bin/env python3
"""string-interior-spaces scanner (see nix/misc-checks.nix).

Argv: <src-root> [crate-src-dirs...]. Flags collapsed backslash
continuations inside .rs string literals (merged_bug_016):

  arm A — a single-line literal carrying an 8+ interior space run
          that is not the indent of a legitimate `\\n` template;
  arm B — (merged_bug_193) a NON-raw literal whose interior LITERAL
          newline is followed by an 8+ space run: the backslash was
          dropped but the newline kept. Raw strings are intentional
          multi-line formatting and exempt.

Structure, not line regexes: a lexer pass blanks comments and records
every string-literal span (raw vs non-raw), so quote parity is exact —
the rg prototype of arm B drowned in closing-quote/comment false
positives (309 files) because regex cannot know which `"` opens a
string. Per-arm planted red+green self-tests run before the real scan
may gate (banner (b)).
"""

import pathlib
import re
import sys

RUN = re.compile(r"\S {8,}\S")
ESCAPED_NL_INDENT = re.compile(r"\\n +")


def lex(text: str):
    """Returns (comment_blanked_text, spans) where spans are
    (start, end, is_raw) for every string literal body (delimiters
    excluded). Positions index into the ORIGINAL text."""
    out = list(text)
    spans = []
    n = len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if c == "/" and nxt == "/":
            j = text.find("\n", i)
            j = n if j == -1 else j
            blank(i, j)
            i = j
        elif c == "/" and nxt == "*":
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif _raw_prefix_len(text, i):
            plen = _raw_prefix_len(text, i)
            hashes = plen - (2 if text[i] == "b" else 1) - 1
            close = '"' + "#" * hashes
            k = text.find(close, i + plen)
            k = n if k == -1 else k
            spans.append((i + plen, k, True))
            i = min(k + len(close), n)
        elif c == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            spans.append((i + 1, min(j, n), False))
            i = min(j + 1, n)
        elif c == "'":
            if i + 2 < n and (text[i + 1] == "\\" or text[i + 2] == "'"):
                j = i + 1
                if text[j] == "\\":
                    j += 1
                    while j < n and text[j] != "'":
                        j += 1
                else:
                    j += 1
                i = min(j + 1, n)
            else:
                i += 1
        else:
            i += 1
    return "".join(out), spans


def _raw_prefix_len(text: str, i: int) -> int:
    j = i
    if j < len(text) and text[j] == "b":
        j += 1
    if j >= len(text) or text[j] != "r":
        return 0
    j += 1
    while j < len(text) and text[j] == "#":
        j += 1
    if j < len(text) and text[j] == '"':
        return j - i + 1
    return 0


def _continued(body: str, idx: int) -> bool:
    """True iff the newline at `idx` is a true `\\`-continuation.

    bug_347: escape parity. The run of consecutive backslashes
    immediately before the newline decides it — ODD means the last
    backslash escapes the newline (continuation); EVEN means the run is
    escaped-backslash pairs and the newline is BARE (the garbled-output
    shape this lint exists to catch). The lexer half was already
    parity-correct (j += 2 on escapes); these re-scans were the blind
    arm. Returns the run start via parity only; callers needing the
    leading-idiom distinction check `_run_start(body, idx) == 0`."""
    return (idx - _run_start(body, idx)) % 2 == 1


def _run_start(body: str, idx: int) -> int:
    k = idx
    while k > 0 and body[k - 1] == "\\":
        k -= 1
    return k


def scan_text(rel: str, text: str) -> list[str]:
    _, spans = lex(text)
    hits = []
    for start, end, is_raw in spans:
        body = text[start:end]
        line = text.count("\n", 0, start) + 1
        if "\n" in body:
            # arm B: dropped-backslash continuation — the sharp
            # signature is MIXED style: a non-raw literal that uses a
            # `\<newline>` continuation MID-STRING (prose join) but
            # carries a BARE newline too. Pure bare-newline literals
            # are the intentional multi-line house style for SQL (68
            # legitimate instances at the census tree); a continuation
            # whose backslash RUN starts at offset 0 is the `"\`
            # fixture idiom (suppress the leading newline of embedded
            # config/CLI text — 9 legitimate instances); pure
            # continuation literals have no bare newline to flag. Only
            # the mix remains: a prose join that lost one of its
            # backslashes. Both scans share `_continued` (escape
            # parity, bug_347) — a `\\`+newline is an escaped
            # backslash and a BARE newline, never a continuation.
            if not is_raw:
                has_mid_continuation = False
                idx = 0
                while (idx := body.find("\n", idx)) != -1:
                    if idx > 0 and _continued(body, idx) and _run_start(body, idx) > 0:
                        has_mid_continuation = True
                        break
                    idx += 1
                if has_mid_continuation:
                    idx = 0
                    while (idx := body.find("\n", idx)) != -1:
                        if idx == 0 or not _continued(body, idx):
                            hits.append(
                                f"{rel}:{line}: bare newline inside a `\\`-continued string (dropped `\\` continuation)"
                            )
                            break
                        idx += 1
            continue
        # arm A: single-line interior run, minus `\n`-template indents.
        if RUN.search(ESCAPED_NL_INDENT.sub("\\\\n ", body)):
            hits.append(f"{rel}:{line}: interior space run inside a string literal")
    return hits


def selftest() -> str | None:
    red_a = 'let m = "garbled continuation          left inside";\n'
    if not scan_text("p.rs", red_a):
        return "arm A planted red did not fire"
    red_b = 'let m = "joined \\\n          properly\n          but this line lost its backslash";\n'
    if not scan_text("p.rs", red_b):
        return "arm B planted red did not fire (mixed continuation + bare newline)"
    # bug_347 red: `\\` before newline is an ESCAPED backslash + BARE
    # newline (even parity) — the pre-fix single-char lookbehind read
    # it as a continuation and passed the literal clean.
    red_c = 'let m = "joined \\\n          truly\\\\\n          escaped backslash then bare newline";\n'
    if not scan_text("p.rs", red_c):
        return "arm B escape-parity red did not fire (`\\\\` before newline mis-read as continuation)"
    green = (
        "// comment table:   col1          col2\n"
        'let y = "rules:\\n        - alert: x";\n'
        'let z = "single spaced help";\n'
        'let r = r#"sql:\n          SELECT 1"#;\n'
        'let w = "joined \\\n          continuation";\n'
        'let q = "SELECT a, b\n           FROM t\n          WHERE x";\n'
    )
    g = scan_text("p.rs", green)
    if g:
        return f"green fixture flagged: {g}"
    return None


def main() -> int:
    err = selftest()
    if err:
        print(f"FAIL: string-interior-spaces self-test — {err}", file=sys.stderr)
        return 1
    src_root = pathlib.Path(sys.argv[1])
    fails = []
    scanned = 0
    for d in sys.argv[2:]:
        droot = src_root / d
        if not droot.exists():
            continue  # crate has no .rs files staged (e.g. rio-dashboard)
        for f in sorted(droot.rglob("*.rs")):
            scanned += 1
            fails.extend(scan_text(str(f.relative_to(src_root)), f.read_text()))
    print(f"string-interior-spaces: scanned {scanned} files (lexer-exact string spans)")
    if fails:
        print(
            "FAIL: collapsed backslash continuation inside .rs string literal(s) —\n"
            "re-join with single spaces or keep the ` \\` continuation (merged_bug_016/merged_bug_193):",
            file=sys.stderr,
        )
        for h in fails:
            print(f"  {h}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
