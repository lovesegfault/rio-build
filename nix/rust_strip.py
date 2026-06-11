#!/usr/bin/env python3
"""Shared exact Rust lexer for the nix/ policy scanners (merged_bug_049).

ONE grammar, one place: nix/streaming_open_ban.py and
nix/string_interior_spaces.py both consume this module's single token
walk, so escape handling can only be wrong — and fixed — here. This is
the rebuild the streaming_open_ban CLASS TRAJECTORY budget called for
after its third structural hole (merged_bug_072 → merged_bug_110 →
merged_bug_049).

The walk covers exactly the token classes the scanners need:
  - line comments (`//…`) and NESTED block comments (`/* /* */ */`);
  - string literals: plain `"…"`, byte `b"…"`, raw and byte-raw
    `r"…"`/`r#"…"#`/`br##"…"##`, with `\\`-pair stepping inside
    non-raw bodies (escape parity is exact);
  - char and byte-char literals (`'x'`, `b'\\n'`) with the FULL escape
    alphabet (`\\'`, `\\\\`, `\\n`, `\\xNN`, `\\u{…}`): the char walk
    is unified on the string branch's parity-correct shape — a
    backslash consumes the ESCAPE PAIR, and the literal closes on the
    next BARE quote. (merged_bug_049: the predecessor walks halted AT
    the escaped quote of `'\\''`, covering only the backslash, so
    scanning resumed at the real closer as a stray token and
    phantom-consumed the following tokens — the exact brace-skew
    fail-open the cfg(test) strip forbids.)
  - lifetime-vs-char disambiguation: `'ident` with no near closer is a
    lifetime and is left untouched.

API — `lex(text, blank_string_bodies=…) -> (out_text, spans)`:
  out_text: comments ALWAYS blanked; char/byte-char bodies ALWAYS
    blanked (delimiters kept — brace/quote parity for downstream
    structural passes); string bodies blanked iff
    `blank_string_bodies`. Newlines survive blanking everywhere, so
    line numbering is stable.
  spans: `(body_start, body_end, is_raw)` for every string literal,
    ORIGINAL-text coordinates, delimiters excluded.

`lex_full(text, blank_string_bodies=…) -> (out_text, spans,
comment_spans)` is the additive round-6 extension (merged_bug_073):
the SAME single walk, plus `(start, end)` for every comment —
ORIGINAL-text coordinates, delimiters INCLUDED (`//…` to
end-of-line-exclusive; `/*…*/` inclusive of the closer). A
comment-lookalike inside a string literal is NOT a span (the walk is
shared, so string and comment classification cannot disagree).
`lex` delegates to `lex_full` and keeps its two-tuple contract —
existing consumers are byte-identical.

SPAN PRIMITIVES (merged_bug_019 / bug_111 — regex-extraction scanners
fail systematically on EXTENT problems: fixed character windows,
single-anchor lazy regexes, first-binding-only captures): scanners
iterate STRUCTURED extents and run their small regexes within them.
  - `fn_extents(text) -> [(name, start, body_start, body_end)]`:
    every `fn name…{body}` — body coordinates exclusive of the
    braces, derived by brace-matching over the LEXED text (so braces
    in strings/comments never skew); bodyless trait/extern decls are
    omitted.
  - `macro_call_extents(text, names) -> [(name, args_start,
    args_end)]`: every `name!(…)` / `name![…]` / `name!{…}` for the
    requested macro names, args coordinates exclusive of the
    delimiters, delimiter-matched over the lexed text.
  - `const_array_strings(text, const_name) -> [str]`: the string
    literal bodies inside `const NAME: … = […];` — the item extent
    found over the lexed text, the VALUES read from the original
    text via the lexer's own string spans (one walk, one
    classification). CLI: `rust_strip.py --const-strings NAME FILE`
    prints one value per line — the shell-fragment face of the same
    primitive.
  - `split_top_level(text, start, end) -> [(piece_start,
    piece_end)]`: top-level comma split within an extent (depth
    tracked over the lexed text), for macro-argument walks.

`strip_cfg_test(text) -> str` is the attribute-position `#[cfg(test)]`
pruner (merged_bug_009): ONE implementation of scope-pruning for every
scanner, replacing the per-scanner truncate-at-first-marker walks that
left whole production bodies unswept after an early test module
(census_corpora.py's mid-file prune) — the xtask corpus pruner's
attribute-position semantics, lifted here. Each cfg(test)-attributed
ITEM (the attribute, any stacked attributes after it, and the item
through its matching close brace or terminating `;`) is blanked
newline-preserving; scanning RESUMES after the item. Attribute
detection runs over the LEXED text, so a `#[cfg(test)]` lookalike
inside a string or comment never prunes.

`selftest()` returns an error string on the first failed assertion (or
None): an exact span/blank table over the escaped-quote families. Both
consumers run it BEFORE their own arm-level selftests and exit 1 on
any miss — a broken shared lexer fails closed before any scan may
gate.
"""

import re


def _raw_prefix_len(text: str, i: int) -> int:
    """Length of an r/br raw-string opener at i (`r"`, `r#"`, `br##"`,
    …) or 0 if none."""
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


def lex_full(text: str, *, blank_string_bodies: bool):
    """Single token walk; see the module docstring for the contract.
    Returns `(out_text, string_spans, comment_spans)`."""
    out = list(text)
    spans = []
    comments = []
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
            comments.append((i, j))
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
            comments.append((i, min(j, n)))
            blank(i, j)
            i = j
        elif _raw_prefix_len(text, i):
            plen = _raw_prefix_len(text, i)
            hashes = plen - (2 if text[i] == "b" else 1) - 1
            close = '"' + "#" * hashes
            k = text.find(close, i + plen)
            body_end = n if k == -1 else k
            spans.append((i + plen, body_end, True))
            if blank_string_bodies:
                blank(i + plen, body_end)
            i = n if k == -1 else k + len(close)
        elif c == '"' or (c == "b" and nxt == '"'):
            start = i + (2 if c == "b" else 1)
            j = start
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            spans.append((start, min(j, n), False))
            if blank_string_bodies:
                blank(start, j)
            i = min(j + 1, n)
        elif c == "'" or (c == "b" and nxt == "'"):
            q = i if c == "'" else i + 1  # index of the opening quote
            j = q + 1
            if j < n and text[j] == "\\":
                # Escape: consume the PAIR first (`\'` steps past the
                # escaped quote — the merged_bug_049 fix), then close
                # on the next BARE quote. `\xNN`/`\u{…}` interiors
                # contain neither quote nor backslash, so pair-step +
                # bare-close covers them exactly.
                j += 2
                while j < n and text[j] != "'":
                    if text[j] == "\\":
                        j += 2
                    else:
                        j += 1
            elif j + 1 < n and text[j + 1] == "'":
                # Plain one-char literal: body at j, closer at j+1.
                j += 1
            else:
                # Lifetime (`'ident`, no near closer): not a literal.
                i += 1
                continue
            # j is at the closing quote (or n on malformed input).
            blank(q + 1, j)
            i = min(j + 1, n)
        else:
            i += 1
    return "".join(out), spans, comments


def lex(text: str, *, blank_string_bodies: bool):
    """The standing two-tuple API — delegates to the single walk;
    existing consumers are byte-identical (comment spans dropped)."""
    out, spans, _comments = lex_full(text, blank_string_bodies=blank_string_bodies)
    return out, spans


CFG_TEST_ATTR = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def strip_cfg_test(text: str) -> str:
    """Attribute-position `#[cfg(test)]` pruner — see the module
    docstring. Returns the ORIGINAL text with each cfg(test)-attributed
    item blanked (newlines kept; line numbers stable)."""
    lexed, _ = lex(text, blank_string_bodies=True)
    out = list(text)
    n = len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    for m in CFG_TEST_ATTR.finditer(lexed):
        i = m.end()
        # Skip whitespace and any stacked attribute blocks after the
        # cfg(test) attr (`#[allow(...)]`, multi-line attrs — bracket
        # matched on the lexed text). Doc comments are already blank.
        while True:
            while i < n and lexed[i].isspace():
                i += 1
            if i < n and lexed[i] == "#":
                j = i + 1
                while j < n and lexed[j].isspace():
                    j += 1
                if j < n and lexed[j] == "[":
                    depth = 1
                    j += 1
                    while j < n and depth:
                        if lexed[j] == "[":
                            depth += 1
                        elif lexed[j] == "]":
                            depth -= 1
                        j += 1
                    i = j
                    continue
            break
        # The item extent: through the matching close of its first
        # top-level `{` (fn/mod/impl bodies), or a `;` at depth 0 if
        # one lands first (use decls, consts, type aliases).
        depth = 0
        j = i
        end = n
        while j < n:
            c = lexed[j]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    end = j + 1
                    break
            elif c == ";" and depth == 0:
                end = j + 1
                break
            j += 1
        blank(m.start(), end)
    return "".join(out)


_OPEN_TO_CLOSE = {"(": ")", "[": "]", "{": "}"}


def _match_delim(lexed: str, i: int) -> int:
    """Index just past the delimiter matching lexed[i] (one of ([{),
    or len(lexed) on malformed input. Operates on LEXED text only."""
    open_c = lexed[i]
    close_c = _OPEN_TO_CLOSE[open_c]
    depth = 0
    n = len(lexed)
    while i < n:
        c = lexed[i]
        if c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


FN_DECL = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def fn_extents(text: str):
    """See the module docstring. Coordinates index the ORIGINAL text
    (lexing is newline- and offset-preserving)."""
    lexed, _ = lex(text, blank_string_bodies=True)
    out = []
    n = len(lexed)
    for m in FN_DECL.finditer(lexed):
        i = m.end()
        # Walk to the body's `{` at bracket depth 0 — past the
        # parameter list, generics, return type, where clauses. A `;`
        # at depth 0 first means a bodyless decl: skip.
        depth = 0
        body_start = None
        while i < n:
            c = lexed[i]
            if c in "([":
                i = _match_delim(lexed, i)
                continue
            if c == "{":
                body_start = i + 1
                break
            if c == ";" and depth == 0:
                break
            i += 1
        if body_start is None:
            continue
        body_end = _match_delim(lexed, body_start - 1) - 1
        out.append((m.group(1), m.start(), body_start, body_end))
    return out


def macro_call_extents(text: str, names):
    """See the module docstring. `names`: iterable of macro names."""
    lexed, _ = lex(text, blank_string_bodies=True)
    alt = "|".join(re.escape(x) for x in names)
    out = []
    for m in re.finditer(r"\b(" + alt + r")!\s*([(\[{])", lexed):
        open_i = m.end() - 1
        close_i = _match_delim(lexed, open_i)
        out.append((m.group(1), open_i + 1, close_i - 1))
    return out


def split_top_level(text: str, start: int, end: int):
    """Top-level comma split of text[start:end] (see docstring)."""
    lexed, _ = lex(text, blank_string_bodies=True)
    pieces = []
    depth = 0
    piece_start = start
    for i in range(start, min(end, len(lexed))):
        c = lexed[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            pieces.append((piece_start, i))
            piece_start = i + 1
    if piece_start < end:
        pieces.append((piece_start, end))
    return pieces


CONST_DECL_TMPL = r"\bconst\s+%s\b"


def const_array_strings(text: str, const_name: str):
    """See the module docstring: string bodies inside `const NAME … ;`,
    read from the ORIGINAL text via the lexer's string spans."""
    lexed, spans, _ = lex_full(text, blank_string_bodies=True)
    m = re.search(CONST_DECL_TMPL % re.escape(const_name), lexed)
    if not m:
        return []
    i = m.end()
    n = len(lexed)
    # Item extent: to the `;` at depth 0 (delimiters skipped whole).
    while i < n:
        c = lexed[i]
        if c in "([{":
            i = _match_delim(lexed, i)
            continue
        if c == ";":
            break
        i += 1
    end = i
    return [text[a:b] for a, b, _raw in spans if m.end() <= a and b <= end]


def selftest() -> str | None:
    """Exact span/blank assertions, escaped-quote families included."""
    # The merged_bug_049 family: '\'' then a tuple/char soup. The walk
    # must close '\'' at its real closer and keep parity for the
    # neighbors. Source: ('\'','{')
    t = "let p = ('\\'','{');"
    blanked, spans = lex(t, blank_string_bodies=True)
    if spans:
        return f"char-only line produced string spans: {spans}"
    # The '{' body must be blanked (parity), the structure kept.
    if blanked != "let p = ('  ',' ');":
        return f"escaped-quote blank table mismatch: {blanked!r}"
    # The corpus trigger shape verbatim (rio-builder executor/outputs.rs).
    t = "rest.split('\\'')"
    blanked, spans = lex(t, blank_string_bodies=True)
    if blanked != "rest.split('  ')" or spans:
        return f"split-escaped-quote mismatch: {blanked!r} {spans}"
    # A char literal then a REAL string: the string must be spanned,
    # not phantom-shifted (the sis half of merged_bug_049).
    t = "let g = ('\\'','\"'); let s = \"bad          run\";"
    _, spans = lex(t, blank_string_bodies=False)
    bodies = [t[a:b] for a, b, _ in spans]
    if bodies != ["bad          run"]:
        return f"phantom span table: {bodies}"
    # Escape alphabet: \xNN and \u{…} close at the bare quote.
    t = "let a = '\\x41'; let b = '\\u{7FFF}'; let c = b'\\n';"
    blanked, spans = lex(t, blank_string_bodies=True)
    if spans:
        return f"escape-alphabet line produced string spans: {spans}"
    if "'" + " " * 4 + "'" not in blanked or "'" + " " * 8 + "'" not in blanked:
        return f"escape-alphabet blank table mismatch: {blanked!r}"
    # Lifetimes are untouched; nested comments blank fully.
    t = "fn f<'a>(x: &'a str) { /* a /* b */ c */ }"
    blanked, _ = lex(t, blank_string_bodies=True)
    if "'a" not in blanked or "/*" in blanked.replace(" ", "")[12:]:
        # crude but exact-enough containment: lifetimes kept, comment gone
        pass
    if blanked.count("'a") != 2:
        return f"lifetime mishandled: {blanked!r}"
    if "b */ c" in blanked:
        return f"nested comment not fully blanked: {blanked!r}"
    # Raw string with hashes: body spanned, closer found.
    t = 'let r = r#"a " inside"#; let after = 1;'
    blanked, spans = lex(t, blank_string_bodies=True)
    if len(spans) != 1 or t[spans[0][0] : spans[0][1]] != 'a " inside' or not spans[0][2]:
        return f"raw-string span mismatch: {spans}"
    if "let after = 1;" not in blanked:
        return f"raw-string closer lost: {blanked!r}"
    # Newlines survive blanking (line numbering stable).
    t = '"one\ntwo"'
    blanked, _ = lex(t, blank_string_bodies=True)
    if blanked != '"   \n   "':
        return f"newline not preserved under blanking: {blanked!r}"
    # Comment-span API (lex_full) — exact original-coordinate rows.
    # Line comment: delimiters included, end-of-line exclusive.
    t = "let a = 1; // tail\nlet b = 2;"
    _, _, comments = lex_full(t, blank_string_bodies=True)
    if comments != [(11, 18)]:
        return f"line-comment span mismatch: {comments}"
    # Nested block comment: ONE span covering the whole nest.
    t = "a /* x /* y */ z */ b"
    _, _, comments = lex_full(t, blank_string_bodies=True)
    if comments != [(2, 19)]:
        return f"nested block-comment span mismatch: {comments}"
    # Comment-lookalike inside a string literal: NOT a span (shared
    # walk — string and comment classification cannot disagree).
    t = 'let s = "// not a comment"; let c = \'/\';'
    _, _, comments = lex_full(t, blank_string_bodies=False)
    if comments != []:
        return f"comment-lookalike inside a string produced spans: {comments}"
    # cfg(test) pruner: a MID-FILE test module is blanked and scanning
    # resumes — production code after it survives (the merged_bug_009
    # scope axis: the old per-scanner walks truncated here).
    t = "fn a() {}\n#[cfg(test)]\nmod tests {\n  fn t() { prod_marker(); }\n}\nfn b() { after_marker(); }\n"
    pruned = strip_cfg_test(t)
    if "prod_marker" in pruned:
        return f"cfg(test) body survived the prune: {pruned!r}"
    if "after_marker" not in pruned or "fn a()" not in pruned:
        return f"prune ate production code around the test module: {pruned!r}"
    if pruned.count("\n") != t.count("\n"):
        return "cfg(test) prune broke line numbering"
    # Stacked attributes after the cfg attr ride the same item.
    t = "#[cfg(test)]\n#[allow(dead_code)]\nfn t() { x(); }\nfn keep() {}\n"
    pruned = strip_cfg_test(t)
    if "x()" in pruned or "keep" not in pruned:
        return f"stacked-attr prune wrong: {pruned!r}"
    # Braceless items terminate at `;`; a cfg(test) LOOKALIKE inside a
    # string or comment never prunes.
    t = '#[cfg(test)]\nuse x::y;\nfn keep() {}\nlet s = "#[cfg(test)]"; // #[cfg(test)]\nfn also_keep() {}\n'
    pruned = strip_cfg_test(t)
    if "use x::y" in pruned or "keep" not in pruned or "also_keep" not in pruned:
        return f"semicolon/lookalike prune wrong: {pruned!r}"
    # fn extents: body coordinates exact; braces in strings/comments
    # never skew; bodyless decls omitted; nested fns both reported.
    t = 'fn outer(a: &str) -> u32 { let s = "}{"; inner(); 1 }\nfn bodyless();\nfn inner() { /* } */ }\n'
    exts = fn_extents(t)
    names = [e[0] for e in exts]
    if names != ["outer", "inner"]:
        return f"fn-extent names wrong: {names}"
    body = t[exts[0][2] : exts[0][3]]
    if 'let s = "}{"; inner(); 1' not in body or body.count("{") != 1:
        return f"fn-extent body wrong: {body!r}"
    # macro-call extents: args exact, string/comment delims inert.
    t = 'counter!("a_total", "k" => v(x, y), "k2" => "v,2").increment(1); describe!{ inner }'
    exts = macro_call_extents(t, ["counter", "describe"])
    if [e[0] for e in exts] != ["counter", "describe"]:
        return f"macro-extent names wrong: {exts}"
    args = t[exts[0][1] : exts[0][2]]
    if args != '"a_total", "k" => v(x, y), "k2" => "v,2"':
        return f"macro-extent args wrong: {args!r}"
    # top-level split: commas inside calls and strings never split.
    pieces = [t[a:b].strip() for a, b in split_top_level(t, exts[0][1], exts[0][2])]
    if pieces != ['"a_total"', '"k" => v(x, y)', '"k2" => "v,2"']:
        return f"top-level split wrong: {pieces}"
    # const-array strings: values from the original text (digits and
    # escapes live), extent stops at the item's `;`.
    t = (
        "pub const REASONS: &[&str] = &[\n"
        '    "alpha",\n'
        '    // "commented_out",\n'
        '    "beta_v2",\n'
        "];\n"
        'const OTHER: &str = "gamma";\n'
    )
    vals = const_array_strings(t, "REASONS")
    if vals != ["alpha", "beta_v2"]:
        return f"const-array strings wrong: {vals}"
    if const_array_strings(t, "MISSING") != []:
        return "const-array on a missing const not empty"
    return None


if __name__ == "__main__":
    import sys

    err = selftest()
    if err:
        print(f"FAIL: rust-strip self-test — {err}", file=sys.stderr)
        sys.exit(1)
    if len(sys.argv) >= 2 and sys.argv[1] == "--const-strings":
        # The shell-fragment face of the const-array primitive (the
        # 42-reason-alert-sync extraction): selftest above gates first.
        if len(sys.argv) != 4:
            print("usage: rust_strip.py --const-strings NAME FILE", file=sys.stderr)
            sys.exit(2)
        vals = const_array_strings(
            open(sys.argv[3], encoding="utf-8").read(), sys.argv[2]
        )
        if not vals:
            print(
                f"FAIL: zero string values under const {sys.argv[2]} in {sys.argv[3]}",
                file=sys.stderr,
            )
            sys.exit(1)
        for v in vals:
            print(v)
        sys.exit(0)
    print("rust-strip: self-test green")
