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

`selftest()` returns an error string on the first failed assertion (or
None): an exact span/blank table over the escaped-quote families. Both
consumers run it BEFORE their own arm-level selftests and exit 1 on
any miss — a broken shared lexer fails closed before any scan may
gate.
"""


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


def lex(text: str, *, blank_string_bodies: bool):
    """Single token walk; see the module docstring for the contract."""
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
    return "".join(out), spans


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
    return None


if __name__ == "__main__":
    import sys

    err = selftest()
    if err:
        print(f"FAIL: rust-strip self-test — {err}", file=sys.stderr)
        sys.exit(1)
    print("rust-strip: self-test green")
