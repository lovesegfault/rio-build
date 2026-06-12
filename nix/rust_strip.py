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

`strip_cfg_test(text, *, source=…) -> str` is the attribute-position
`#[cfg(test)]` pruner (merged_bug_009): ONE implementation of
scope-pruning for every scanner, replacing the per-scanner
truncate-at-first-marker walks that left whole production bodies
unswept after an early test module (census_corpora.py's mid-file
prune) — the xtask corpus pruner's attribute-position semantics,
lifted here. Each cfg(test)-attributed construct (the attribute, any
stacked attributes after it, and the construct's grammatical extent)
is blanked newline-preserving; scanning RESUMES after it. Attribute
detection runs over the LEXED text, so a `#[cfg(test)]` lookalike
inside a string or comment never prunes.

The extent walk (WO-S8-1, merged_bug_018) is BRACKET-AWARE and
classifies into the derived item/attachment alphabet
(`CFG_VECTOR_ALPHABET`): `;`/`,` inside `()`/`[]`/`{}` groups never
terminate (the `[T; 11]` array-const header no longer ends the blank
mid-signature), and `,`-terminated attachment positions — struct
fields, enum variants, match arms, struct-expr fields, statements —
have their own extent rule instead of driving the brace count
negative and blanking production code below them. The walk FAILS
CLOSED (R22″): depth underflow, an unmatched delimiter (the
`_match_delim` malformed-input return-n face), a missing terminator
at EOF, or an extent it cannot classify within the alphabet raises
`StripError` naming `source:line` — never a silent break, skip, or
blank-to-EOF. Consumers convert the refusal into a named check
failure (census_corpora's F″ arm is the founding plant).

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


# The historical bare-spelling recognizer (kept for reference and for
# the spelling table's outer-bare row): merged_bug_088's defect was
# that this regex was the WHOLE production-population predicate —
# compound heads (`all(test, …)`, `any(test, …)`) and the inner
# `#![cfg(test)]` leaked test code into every census population.
CFG_TEST_ATTR = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


class StripError(Exception):
    """Fail-closed scanner refusal (R22″, WO-S8-1): the extent walk
    REFUSES any input it cannot classify within the derived alphabet —
    depth underflow, an unmatched delimiter (the `_match_delim`
    malformed-input face, which previously yielded a silent
    blank-to-EOF), a missing item terminator at EOF, an unbalanced
    generic-angle run, or an attachment head outside the alphabet.
    Never a silent break, skip, or blank-to-EOF. The message carries
    `<source>:<line>`; callers supply `source` (their file path) so
    the refusal names the site."""


def _line_of(text: str, i: int) -> int:
    return text.count("\n", 0, min(i, len(text))) + 1


def _match_delim_strict(lexed: str, i: int, source: str) -> int:
    """`_match_delim`, fail-closed: an unmatched delimiter is a
    StripError naming the opener's line — never a silent
    run-to-EOF (the `_match_delim` malformed-input return-n face)."""
    open_c = lexed[i]
    close_c = _OPEN_TO_CLOSE[open_c]
    depth = 0
    n = len(lexed)
    j = i
    while j < n:
        c = lexed[j]
        if c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return j + 1
        j += 1
    raise StripError(
        f"{source}:{_line_of(lexed, i)}: unmatched `{open_c}` — the "
        f"cfg(test) extent walk refuses malformed delimiters "
        f"(fail-closed; previously a silent blank-to-EOF)"
    )


_WORD_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


# --- the cfg-gating recognizer (WO-S8-2, merged_bug_088) --------------
#
# TWO independent implementations, differentially pinned:
#
#   1. `cfg_pred_gates_test` — the RECURSION axis: a python port of
#      the CANONICAL xtask predicate (`cfg_pred_gates_test`,
#      xtask/src/lint.rs — "prune ANY cfg…" — quantifier: census(cfg-pruner-parity)
#      ("…whose argument tokens mention the bare `test` ident"),
#      evaluated by HEAD: a bare `test` element gates; `not(..)`
#      NEVER gates (quantifier: census(CFG_SPELLING_VECTORS not-head row)); `any(..)`/
#      `all(..)` gate iff any inner element gates, recursing;
#      `feature = "…"` and every other key never gate). The port is
#      pinned to the canonical's SOURCE via nix/cfg-pruner-canonical
#      .pin ([GEN-SET]: `rust_strip.py --extract-canonical
#      xtask/src/lint.rs`) — if the canonical's text drifts, the
#      parity gate goes red and the port is re-derived by a human,
#      never silently.
#   2. `classify_cfg_spelling` — the ENUMERATION axis: the spelling
#      table over the grammatical forms the live tree carries (outer
#      bare / flat all(…)-head / flat any(…)-head / not(…)-head /
#      plain non-test). A form OUTSIDE the table returns None.
#
# The parity differential (`--parity`, the cfg-pruner-parity check)
# runs BOTH over every cfg attribute in the live tree and fails on
# None (a spelling outside the table — the fourth-spelling tripwire,
# W11-BU) or on any table-vs-port disagreement. The strip pipeline
# consumes axis 1 (total by recursion), so a leak needs BOTH
# implementations wrong AND the pin stale.


def _split_top_commas(s: str):
    parts, depth, start = [], 0, 0
    for i, c in enumerate(s):
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append(s[start:i])
            start = i + 1
    parts.append(s[start:])
    return parts


def cfg_pred_gates_test(pred: str) -> bool:
    """The ported canonical predicate (axis 1; see the block comment
    above). `pred` is the text inside `cfg( … )`, lexed or raw."""
    for elem in _split_top_commas(pred):
        elem = elem.strip()
        m = _WORD_RE.match(elem)
        if not m:
            continue
        head = m.group(0)
        if head == "test":
            return True
        if head == "not":
            continue
        if head in ("any", "all"):
            rest = elem[m.end() :].lstrip()
            if rest.startswith("(") and rest.endswith(")"):
                if cfg_pred_gates_test(rest[1:-1]):
                    return True
        # every other head (feature/unix/target_os/…) never gates
    return False


def classify_cfg_spelling(pred: str):
    """The enumerated spelling table (axis 2): returns
    `(spelling, gates)` for forms the table names, None outside it —
    a nested compound or novel form is UNKNOWN by design (the
    differential turns UNKNOWN into a red, never a silent guess)."""
    s = pred.strip()
    if re.fullmatch(r"test", s):
        return ("outer-bare", True)
    m = re.match(r"(all|any)\s*\(", s)
    if m and s.endswith(")"):
        elems = [e.strip() for e in _split_top_commas(s[m.end() : -1])]
        if any(re.match(r"(all|any|not)\s*\(", e) for e in elems):
            return None  # nested compound — outside the flat table
        gates = any(
            (w := _WORD_RE.match(e)) is not None and w.group(0) == "test"
            for e in elems
        )
        return (f"compound-{m.group(1)}-head", gates)
    if re.match(r"not\s*\(", s):
        return ("not-head", False)
    m = _WORD_RE.match(s)
    if m and m.group(0) not in ("test", "all", "any", "not"):
        return ("plain-non-test", False)
    return None


CFG_ATTR_HEAD = re.compile(r"#(!?)\s*\[")


def iter_cfg_attrs(lexed: str, source: str = "<input>"):
    """Every `#[cfg(<pred>)]` / `#![cfg(<pred>)]` attribute in the
    LEXED text: yields `(attr_start, attr_end, inner, pred)`. Non-cfg
    attributes are skipped; bracket matching is fail-closed
    (StripError on malformed input)."""
    for m in CFG_ATTR_HEAD.finditer(lexed):
        lb = m.end() - 1
        end = _match_delim_strict(lexed, lb, source)
        body = lexed[m.end() : end - 1]
        bm = re.match(r"\s*cfg\s*\(", body)
        if not bm:
            continue
        po = m.end() + bm.end() - 1  # the '(' of cfg(…) in lexed
        pe = _match_delim_strict(lexed, po, source)
        yield m.start(), end, bool(m.group(1)), lexed[po + 1 : pe - 1]


def _brace_depth_at(lexed: str, i: int) -> int:
    return lexed.count("{", 0, i) - lexed.count("}", 0, i)


# Item keywords that terminate at a top-level `;` (brace groups along
# the way — struct-literal/match initializers — are skipped whole).
_SEMI_ITEMS = {"const", "static", "type", "use", "extern crate"}
# Item keywords whose extent ends at their first top-level `{…}` body
# (or a `;` arriving first: `mod m;`, bodyless fn, unit/tuple struct).
_BODY_ITEMS = {"fn", "mod", "impl", "trait", "enum", "struct", "union", "extern"}
# Heads a COMMA-position attachment (field / variant / arm /
# struct-expr field / statement) may legally start with. Anything
# else is outside the derived alphabet and REFUSES.
_ATTACH_HEAD_OK = set("([&|<'._-\"") | {"digit", "word"}


def _skip_ws(lexed: str, i: int) -> int:
    n = len(lexed)
    while i < n and lexed[i].isspace():
        i += 1
    return i


def _classify_construct(lexed: str, i: int, source: str):
    """Classify the construct at `i` (first token after the cfg attr
    and stacked attrs) into the derived alphabet: returns
    `(mode, scan_start)` where mode ∈ {"semi", "body", "macro",
    "comma"}. Qualifiers (pub/default/unsafe/async/extern-abi/const-fn)
    are consumed. Unclassifiable heads REFUSE (StripError)."""
    n = len(lexed)
    while True:
        i = _skip_ws(lexed, i)
        if i >= n:
            raise StripError(
                f"{source}:{_line_of(lexed, i)}: cfg(test) attribute at "
                f"EOF with no attached construct — refusing"
            )
        m = _WORD_RE.match(lexed, i)
        if not m:
            break
        w = m.group(0)
        if w == "pub":
            i = _skip_ws(lexed, m.end())
            if i < n and lexed[i] == "(":
                i = _match_delim_strict(lexed, i, source)
            continue
        if w in ("default", "unsafe", "async", "auto"):
            i = m.end()
            continue
        if w == "const":
            j = _skip_ws(lexed, m.end())
            m2 = _WORD_RE.match(lexed, j)
            if m2 and m2.group(0) == "fn":
                i = j
                continue  # `const fn` — the const is a qualifier
            return "semi", m.end()
        if w == "extern":
            j = _skip_ws(lexed, m.end())
            if j < n and lexed[j] == '"':
                # ABI string (body blanked by the lexer, delims kept):
                # skip it, then loop — `extern "C" fn` / `extern "C" {`.
                k = j + 1
                while k < n and lexed[k] != '"':
                    k += 1
                i = k + 1
                continue
            m2 = _WORD_RE.match(lexed, j)
            if m2 and m2.group(0) == "crate":
                return "semi", m2.end()
            return "body", j  # bare `extern { … }` foreign mod
        if w == "macro_rules":
            return "macro", m.end()
        if w in _SEMI_ITEMS:
            return "semi", m.end()
        if w in _BODY_ITEMS:
            return "body", m.end()
        # A (possibly `::`-qualified) ident: a macro invocation in item
        # position (`name! { … }` / `path::name!(…);`) or a COMMA-mode
        # attachment head (field name, variant, pattern start).
        j = m.end()
        while True:
            k = _skip_ws(lexed, j)
            if lexed.startswith("::", k):
                k = _skip_ws(lexed, k + 2)
                m2 = _WORD_RE.match(lexed, k)
                if not m2:
                    break
                j = m2.end()
                continue
            break
        k = _skip_ws(lexed, j)
        if k < n and lexed[k] == "!":
            return "macro", k + 1
        return "comma", i
    # Non-word head: a bare `{…}` group (extern-block body reached via
    # the qualifier loop, block statement) rides the comma walk's
    # group rule; otherwise legal only for COMMA-position attachments
    # (tuple-field types start at `(`… patterns at `&`/`|`/literals).
    c = lexed[i]
    if c == "{":
        return "comma", i
    head = "digit" if c.isdigit() else c
    if head in _ATTACH_HEAD_OK:
        return "comma", i
    raise StripError(
        f"{source}:{_line_of(lexed, i)}: cfg(test) attribute attached to "
        f"an unclassifiable construct head `{c}` — outside the derived "
        f"item/attachment alphabet; refusing (fail-closed)"
    )


def _extent_end(lexed: str, attr_start: int, i: int, source: str) -> int:
    """End (exclusive) of the cfg-gated construct starting at `i`.
    Bracket-aware: `;`/`,` inside `()`/`[]`/`{}` groups never
    terminate (the merged_bug_018 array-const hole); `,`-terminated
    attachment positions (struct fields, enum variants, match arms,
    struct-expr fields — the negative-depth blank-out hole) get their
    own rule. Fail-closed per R22″ (StripError, see class doc)."""
    n = len(lexed)
    mode, i = _classify_construct(lexed, i, source)
    err_line = _line_of(lexed, attr_start)

    if mode == "macro":
        # `macro_rules! name {…}` / `name!{…}` (optional `;`) /
        # `name!(…);` / `name![…];` — the PD-6 invocation cells.
        i = _skip_ws(lexed, i)
        if i < n and lexed[i] == "!":  # macro_rules path: `!` not yet eaten
            i = _skip_ws(lexed, i + 1)
        m = _WORD_RE.match(lexed, i)  # `macro_rules! NAME`
        if m:
            i = _skip_ws(lexed, m.end())
        if i >= n or lexed[i] not in "([{":
            raise StripError(
                f"{source}:{err_line}: cfg(test) macro form without a "
                f"delimiter group — refusing"
            )
        brace_form = lexed[i] == "{"
        i = _match_delim_strict(lexed, i, source)
        j = _skip_ws(lexed, i)
        if brace_form:
            return j + 1 if j < n and lexed[j] == ";" else i
        if j >= n or lexed[j] != ";":
            raise StripError(
                f"{source}:{err_line}: cfg(test) `name!(…)`/`name![…]` "
                f"item without its terminating `;` — refusing"
            )
        return j + 1

    if mode in ("semi", "body"):
        while i < n:
            c = lexed[i]
            if c in "([":
                i = _match_delim_strict(lexed, i, source)
                continue
            if c == "{":
                end = _match_delim_strict(lexed, i, source)
                if mode == "body":
                    return end
                i = end  # semi items skip initializer braces whole
                continue
            if c == ";":
                return i + 1
            if c in ")]}":
                raise StripError(
                    f"{source}:{err_line}: cfg(test) item extent hit the "
                    f"enclosing `{c}` before its terminator — depth "
                    f"underflow; refusing (fail-closed, previously a "
                    f"silent mis-extent)"
                )
            i += 1
        raise StripError(
            f"{source}:{err_line}: cfg(test) item extent ran to EOF "
            f"without a terminator — refusing (previously a silent "
            f"blank-to-EOF)"
        )

    # COMMA mode: struct field / enum variant / match arm /
    # struct-expr field / statement. Ends at a top-level `,` or `;`
    # (consumed), BEFORE the enclosing closer (last member, no
    # trailing comma), or after a `{…}` group not followed by `,`,
    # `;`, or `else` (block arms, brace variants; the `else` chain is
    # the WO-S8-1/bug_049 if/else-initializer cell — the gc/collect.rs
    # :1371 specimen).
    #
    # ANGLE TRACKING IS TOKEN-ADJACENCY CLASSIFIED (WO-S8-1, bug_049):
    # the old walk counted every `<` as a generic opener, so a spaced
    # comparison (`a < b;`) opened a phantom angle run whose `;` never
    # terminated — the extent silently swallowed the FOLLOWING
    # production statement when a later stray `>` re-balanced it
    # (fail-open population shrink), and a legal shift discriminant
    # (`A = 1 << 4,`) false-StripError'd. The second derivation axis
    # (token alphabet within extents — CFG_EXPR_TOKEN_VECTORS) closes
    # both: a `<` is a GENERIC OPENER iff its DIRECT predecessor
    # character is type-position adjacency (`ident`/`>`/`)`/`]`/`<`/
    # `:` — rustfmt never spaces `Ident<`, `::<`, nested `<<T`),
    # otherwise it is an EXPRESSION token (comparison; a spaced `<<`
    # pair is one shift token, consumed whole). The classification is
    # exact on formatter-normalized input; the standing treefmt gate
    # is the machine-bound compensating control for unformatted
    # spellings, and the refusal belts below catch the residue
    # loudly: a `;` reached inside an open angle run REFUSES (never
    # legal in type position at bracket level — `[u8; N]` rides the
    # bracket matcher), as does any group/closer/EOF boundary with an
    # unbalanced run. `,` inside an open angle run still extends —
    # that is the legal generic-interior cell (`HashMap<String, u64>`,
    # `Result<T, E>` — the gc/collect.rs specimen's own type), and
    # with adjacency classification an open run can only be a real
    # type-position run.
    angle = 0
    prev = ""
    while i < n:
        c = lexed[i]
        if c in "([":
            i = _match_delim_strict(lexed, i, source)
            prev = ")"
            continue
        if c == "{":
            end = _match_delim_strict(lexed, i, source)
            j = _skip_ws(lexed, end)
            if angle:
                raise StripError(
                    f"{source}:{err_line}: cfg(test) attachment extent "
                    f"closed a brace group inside an unbalanced generic-"
                    f"angle run — refusing (fail-closed)"
                )
            if j < n and lexed[j] in ",;":
                return j + 1
            # The if/else-initializer cell (bug_049's live corroboration,
            # gc/collect.rs:1371): a brace group followed by `else`
            # continues the same expression — ending here left the
            # dangling `else {…};` unblanked (under-blank residue).
            m_else = _WORD_RE.match(lexed, j)
            if m_else and m_else.group(0) == "else":
                i = m_else.end()
                prev = "e"
                continue
            return end
        if c == "<":
            if i > 0 and (lexed[i - 1].isalnum() or lexed[i - 1] in "_>)]<:"):
                # Type-position adjacency: a real generic opener.
                angle += 1
            elif i + 1 < n and lexed[i + 1] == "<":
                # Spaced `<<`: one shift token, consumed whole (the
                # `A = 1 << 4,` false-StripError cell).
                i += 2
                prev = "<"
                continue
            # else: spaced `<`/`<=` — an expression comparison token;
            # never tracked (the `a < b;` swallow cell).
        elif c == ">" and prev not in "-=" and angle > 0:
            angle -= 1
        elif c == ";" and angle > 0:
            # The refusal belt: `;` is never legal inside a
            # type-position angle run the walk can see (array `;`
            # rides the bracket matcher) — an open run crossing a
            # statement boundary is ambiguous input; refuse rather
            # than extend into the next statement (the silent
            # production-blanking face, now structurally closed).
            raise StripError(
                f"{source}:{err_line}: cfg(test) attachment extent "
                f"reached `;` inside an open generic-angle run — "
                f"ambiguous comparison/generic spelling; refusing "
                f"(fail-closed; parenthesize the comparison or run "
                f"treefmt)"
            )
        elif c in ",;" and angle == 0:
            return i + 1
        elif c in ")]}":
            if angle:
                raise StripError(
                    f"{source}:{err_line}: cfg(test) attachment extent "
                    f"hit the enclosing `{c}` with an unbalanced "
                    f"generic-angle run — refusing (fail-closed)"
                )
            return i  # enclosing closer: end exclusive (last member)
        if not c.isspace():
            prev = c
        i += 1
    raise StripError(
        f"{source}:{err_line}: cfg(test) attachment extent ran to EOF — "
        f"refusing (previously a silent blank-to-EOF)"
    )


def strip_cfg_test(text: str, *, source: str = "<input>") -> str:
    """Attribute-position `#[cfg(test)]` pruner — see the module
    docstring. Returns the ORIGINAL text with each cfg(test)-attributed
    construct blanked (newlines kept; line numbers stable).

    WO-S8-1 (merged_bug_018): the extent walk is bracket-aware (`;`
    inside `[]`/`()` is not an item end — the `[T; 11]` array-const
    header no longer ends the blank mid-signature) and `,`-terminated
    attachment positions (cfg(test) struct fields, enum variants,
    match arms, struct-expr fields) have their own extent rule instead
    of driving the brace count negative and blanking production code.
    Refusals are StripError naming `source:line` (R22″ fail-closed);
    callers pass `source` so the error names the real file.

    WO-S8-2 (merged_bug_088): the recognizer covers EVERY test-gating
    cfg spelling via the ported canonical predicate — outer bare
    `#[cfg(test)]`, compound heads (`all(test, …)`, `any(test, …)`,
    arbitrarily nested), and the inner `#![cfg(test)]` (file scope:
    the whole file is test code and blanks entirely; an inner gating
    attr BELOW file scope refuses — outside the alphabet). `not(…)`
    and `feature = "…"` forms never prune (the canonical's head
    rule)."""
    lexed, _ = lex(text, blank_string_bodies=True)
    out = list(text)
    n = len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    for a, b, inner, pred in iter_cfg_attrs(lexed, source):
        if not cfg_pred_gates_test(pred):
            continue
        if inner:
            if _brace_depth_at(lexed, a) == 0:
                # `#![cfg(test)]` at file scope: the whole file is
                # test-gated (actor/debug.rs:5 — the live instance).
                return "".join(c if c == "\n" else " " for c in text)
            raise StripError(
                f"{source}:{_line_of(lexed, a)}: inner `#![cfg(…)]` "
                f"test gate below file scope — outside the derived "
                f"alphabet; refusing (fail-closed)"
            )
        i = b
        # Skip whitespace and any stacked attribute blocks after the
        # cfg attr (`#[allow(...)]`, multi-line attrs — bracket
        # matched fail-closed on the lexed text). Doc comments are
        # already blank.
        while True:
            i = _skip_ws(lexed, i)
            if i < n and lexed[i] == "#":
                j = _skip_ws(lexed, i + 1)
                if j < n and lexed[j] == "[":
                    i = _match_delim_strict(lexed, j, source)
                    continue
            break
        end = _extent_end(lexed, a, i, source)
        blank(a, end)
    return "".join(out)


# --- the derived vector alphabet (WO-S8-1, R22″ banner) ---------------
#
# [GEN-SET] — the vector ALPHABET is derived from the HOST GRAMMAR's
# attribute-position list, never author-typed: syn 2.x's `syn::Item`
# variant set (Const / Enum / ExternCrate / Fn / ForeignMod / Impl /
# Macro / Mod / Static / Struct / Trait / TraitAlias / Type / Union /
# Use — the item positions the canonical xtask pruner walks) PLUS the
# non-item attachment positions an attribute may legally occupy
# (enum Variant, named Field, tuple Field, match Arm, struct-expr
# FieldValue, Statement). Per cell the generator emits one vector per
# bracket-`;`/`,` placement the form admits (array-`;` in types and
# initializers, brace initializers, optional trailing `;` on
# brace-form macro invocations, trailing-comma vs last-member
# attachment) — so every grammatical cell the walk must handle has a
# vector, and the selftest's completeness pin asserts every alphabet
# cell yielded vectors and every vector passed (a silently dropped
# cell is a red). Regenerate by re-deriving against syn's Item enum;
# excluded by record: const-generic default-brace headers
# (`<const N: usize = {3}>`) — outside the placement product, refused
# or mis-extented by no live form.
_T = "cfg_gated_payload"  # the gated token: must be blanked
_P = "prod_marker"  # the production token: must survive

CFG_VECTOR_ALPHABET = [
    # (cell, [vector source, …]) — each vector embeds _T inside the
    # gated construct and _P in production code around it.
    ("Const", [
        f"#[cfg(test)]\nconst {_T}: u8 = 1;\nfn {_P}() {{}}\n",
        # W11-BQ: the floor.rs array-const header — `;` inside the
        # type's `[…]` plus a brace-block initializer.
        f"#[cfg(test)]\nconst {_T}: [u8; 11] = {{\n    [1; 11]\n}};\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nconst {_T}: [u8; 3] = [1, 2, 3];\nfn {_P}() {{}}\n",
    ]),
    ("Enum", [f"#[cfg(test)]\nenum {_T} {{ A, B }}\nfn {_P}() {{}}\n"]),
    ("ExternCrate", [f"#[cfg(test)]\nextern crate {_T};\nfn {_P}() {{}}\n"]),
    ("Fn", [
        f"#[cfg(test)]\nfn {_T}() {{ x(); }}\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nfn {_T}(x: [u8; 2]) -> [u8; 2] {{ x }}\nfn {_P}() {{}}\n",
        f"trait Tr {{\n    #[cfg(test)]\n    fn {_T}(&self);\n    fn {_P}(&self);\n}}\n",
    ]),
    ("ForeignMod", [f'#[cfg(test)]\nextern "C" {{ fn {_T}(); }}\nfn {_P}() {{}}\n']),
    ("Impl", [
        f"struct S;\n#[cfg(test)]\nimpl S {{ fn {_T}(&self) {{}} }}\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nimpl Tr for [u8; 3] {{ fn {_T}(&self) {{}} }}\nfn {_P}() {{}}\n",
    ]),
    ("Macro", [
        # PD-6: item-position macro invocations — brace form without
        # `;`, brace form WITH `;`, paren/bracket forms with `;`.
        f"#[cfg(test)]\nlazy_static! {{ static ref {_T}: u8 = 1; }}\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nmint! {{ {_T} }};\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nthread_local!(static {_T}: u8 = 1);\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\ndeclare![{_T}, 2];\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nmacro_rules! {_T} {{ () => {{}} }}\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\npaste::paste! {{ fn {_T}() {{}} }}\nfn {_P}() {{}}\n",
    ]),
    ("Mod", [
        f"#[cfg(test)]\nmod {_T} {{ fn t() {{}} }}\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nmod {_T};\nfn {_P}() {{}}\n",
    ]),
    ("Static", [f'#[cfg(test)]\nstatic {_T}: [u8; 4] = [0; 4];\nfn {_P}() {{}}\n']),
    ("Struct", [
        f"#[cfg(test)]\nstruct {_T} {{ a: u8 }}\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nstruct {_T}(u8);\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nstruct {_T};\nfn {_P}() {{}}\n",
    ]),
    ("Trait", [f"#[cfg(test)]\ntrait {_T} {{ fn m(&self); }}\nfn {_P}() {{}}\n"]),
    ("TraitAlias", [f"#[cfg(test)]\ntrait {_T} = Send;\nfn {_P}() {{}}\n"]),
    ("Type", [f"#[cfg(test)]\ntype {_T} = [u8; 8];\nfn {_P}() {{}}\n"]),
    ("Union", [f"#[cfg(test)]\nunion {_T} {{ a: u8, b: i8 }}\nfn {_P}() {{}}\n"]),
    ("Use", [
        f"#[cfg(test)]\nuse {_T}::y;\nfn {_P}() {{}}\n",
        f"#[cfg(test)]\nuse x::{{{_T}, z}};\nfn {_P}() {{}}\n",
    ]),
    # Non-item attachment positions (the comma-rule alphabet).
    ("Variant", [
        # PD-6: the enum-variant comma cell — the same negative-depth
        # class as the field hole.
        f"enum E {{\n    A,\n    #[cfg(test)]\n    {_T},\n    {_P},\n}}\n",
        f"enum E {{\n    A,\n    #[cfg(test)]\n    {_T}(u8),\n    {_P},\n}}\n",
        f"enum E {{\n    {_P},\n    #[cfg(test)]\n    {_T} {{ x: u8 }}\n}}\n",
    ]),
    ("FieldNamed", [
        # W11-BR: the executor.rs `,`-terminated cfg(test) struct
        # field — the live false-PASS blank-out hole.
        f"struct S {{\n    a: u8,\n    #[cfg(test)]\n    {_T}: Option<X>,\n    {_P}: u8,\n}}\nfn live() {{ {_P}2(); }}\n",
        f"struct S {{\n    #[cfg(test)]\n    {_T}: HashMap<String, u64>,\n    {_P}: u8,\n}}\n",
        f"struct S {{\n    {_P}: u8,\n    #[cfg(test)]\n    {_T}: u8\n}}\n",
    ]),
    ("FieldTuple", [
        # PD-6: the tuple-field paren cell.
        f"struct S(u8, #[cfg(test)] {_T}, {_P});\nfn live() {{}}\n",
        f"struct S({_P}, #[cfg(test)] {_T});\nfn live() {{}}\n",
    ]),
    ("Arm", [
        f"fn f(x: u8) {{ match x {{\n    1 => a(),\n    #[cfg(test)]\n    2 => {_T}(),\n    _ => {_P}(),\n}} }}\n",
        f"fn f(x: u8) {{ match x {{\n    #[cfg(test)]\n    2 => {{ {_T}() }}\n    _ => {_P}(),\n}} }}\n",
    ]),
    ("FieldValue", [
        # The executor.rs:80 cell: cfg(test) on a struct-EXPRESSION
        # field init.
        f"fn f() -> S {{ S {{\n    a: 1,\n    #[cfg(test)]\n    {_T}: None,\n    b: {_P},\n}} }}\n",
    ]),
    ("Statement", [
        f"fn f() {{\n    #[cfg(test)]\n    let {_T} = 1;\n    {_P}();\n}}\n",
    ]),
]

# Refusal cells (R22″ fail-closed): one co-located plant per refusal
# arm of the walk — (cell, source, line the error must name).
CFG_REFUSAL_VECTORS = [
    ("unmatched-delim", "#[cfg(test)]\nfn broken( {\n", 2),
    ("missing-terminator-eof", "#[cfg(test)]\nconst X: u8 = 1", 1),
    ("item-depth-underflow", "#[cfg(test)]\nuse a::b }\n", 1),
    ("unclassifiable-head", "#[cfg(test)]\n}\n", 2),
    ("attachment-eof", "struct S {\n    #[cfg(test)]\n    f: u8", 2),
    ("angle-imbalance", "struct S {\n    #[cfg(test)]\n    f: Vec<u8,\n}\n", 2),
    ("macro-missing-semi", "#[cfg(test)]\nm!(x)\n", 1),
    # WO-S8-2: an inner test gate below file scope is outside the
    # alphabet (file-scope inner attrs blank the whole file instead).
    ("inner-below-file-scope", "mod m {\n    #![cfg(test)]\n    fn t() {}\n}\n", 2),
    # WO-S8-1 (bug_049): an UNFORMATTED comparison (`a<b` — adjacency
    # reads the `<` as a generic opener) crossing a `;` refuses
    # loudly instead of extending into the next statement; treefmt
    # normalizes the spelling out of the committed tree.
    ("angle-run-semicolon", "fn f() {\n    #[cfg(test)]\n    let x = a<b;\n    keep();\n}\n", 2),
]

# --- the expression-token vector axis (WO-S8-1, bug_049) ---------------
#
# [GEN-SET] — the SECOND derivation axis the bw11 corpus lacked: the
# position axis above enumerates WHICH constructs exist; this table
# enumerates the expression-position TOKEN ALPHABET within extents —
# the tokens the comma walk must classify (comparison `<`, shift
# `<<`, if/else `{` chains) — as the product token × attachment
# position, restricted to the cells the GRAMMAR admits (else-chains
# exist only where block expressions do; discriminants and field
# inits are expression positions; guards ride match arms). Per cell
# the vector embeds the gated payload and a production neighbor; the
# walk must blank exactly the payload — refuse-or-correct, never a
# silent neighbor blank (W12-AY). The pre-fix reds (every-`<`-opens
# walk): lt-cmp/Statement silently blanked the FOLLOWING production
# statement; shl/Variant raised a false StripError on `1 << 4`;
# brace-else/Statement left the dangling `else {…};` unblanked (the
# live gc/collect.rs:1371 specimen, its shape the last vector below).
_EXPR_TOKENS = ("lt-cmp", "shl", "brace-else")
_EXPR_POSITIONS = ("Statement", "Variant", "FieldValue", "Arm")
# Grammar-admitted product cells (token, position) -> vector.
CFG_EXPR_TOKEN_VECTORS = {
    ("lt-cmp", "Statement"): (
        f"fn f() {{\n    #[cfg(test)]\n    let {_T} = a < b;\n"
        f"    let {_P} = c > d;\n    {_P}2();\n}}\n"
    ),
    ("lt-cmp", "Variant"): (
        f"enum E {{\n    #[cfg(test)]\n    {_T} = A < B,\n    {_P},\n}}\n"
    ),
    ("lt-cmp", "FieldValue"): (
        f"fn g() -> S {{ S {{\n    a: 1,\n    #[cfg(test)]\n    {_T}: a < b,\n    b: {_P},\n}} }}\n"
    ),
    ("lt-cmp", "Arm"): (
        f"fn f(x: u8) {{ match x {{\n    #[cfg(test)]\n    2 if a < b => {_T}(),\n    _ => {_P}(),\n}} }}\n"
    ),
    ("shl", "Statement"): (
        f"fn f() {{\n    #[cfg(test)]\n    let {_T} = 1 << 4;\n    {_P}();\n}}\n"
    ),
    ("shl", "Variant"): (
        f"enum E {{\n    #[cfg(test)]\n    {_T} = 1 << 4,\n    {_P},\n}}\n"
    ),
    ("shl", "FieldValue"): (
        f"fn g() -> S {{ S {{\n    #[cfg(test)]\n    {_T}: 1 << 4,\n    b: {_P},\n}} }}\n"
    ),
    ("shl", "Arm"): (
        f"fn f(x: u8) {{ match x {{\n    #[cfg(test)]\n    2 if x << 1 == 0 => {_T}(),\n    _ => {_P}(),\n}} }}\n"
    ),
    ("brace-else", "Statement"): (
        f"fn f() {{\n    #[cfg(test)]\n    let {_T} = if c {{ 1 }} else {{ 2 }};\n    {_P}();\n}}\n"
    ),
    ("brace-else", "FieldValue"): (
        f"fn g() -> S {{ S {{\n    #[cfg(test)]\n    {_T}: if c {{ 1 }} else {{ 2 }},\n    b: {_P},\n}} }}\n"
    ),
    ("brace-else", "Arm"): (
        f"fn f(x: u8) {{ match x {{\n    #[cfg(test)]\n    2 => if c {{ {_T}() }} else {{ {_T}2() }}\n    _ => {_P}(),\n}} }}\n"
    ),
    # The live corroboration specimen (gc/collect.rs:1371-:1382, the
    # bug_049 over-scan): a cfg(test) statement whose TYPE carries a
    # legal generic-interior comma (`Result<T, E>` — the cell the
    # naive `,`-refusal alternative would have falsely refused) AND
    # whose initializer is an if/else chain ending `};`.
    ("brace-else", "Specimen"): (
        f"fn f() {{\n    #[cfg(test)]\n    let {_T}: Result<PgQueryResult, sqlx::Error> ="
        f" if inject() {{\n        Err({_T}2())\n    }} else {{\n        run()\n    }};\n"
        f"    #[cfg(not(test))]\n    let {_P} = run();\n    {_P}2();\n}}\n"
    ),
}
# Cells the grammar does NOT admit (recorded, asserted absent): enum
# variants and struct fields have no else-chains in type position;
# the brace-else/Variant cell is excluded BY DERIVATION, not dropped.
CFG_EXPR_EXCLUDED_CELLS = {("brace-else", "Variant")}

# WO-S8-2: the spelling plants — one derived plant per test-gating
# spelling, consumed by THIS pruner's selftest and (via
# `cfg_test_attr_spans`) by streaming_open_ban's twin selftest. The
# non-gating rows pin the canonical's head rule (not(…) is
# production-only code; feature keys never gate).
CFG_SPELLING_VECTORS = [
    ("outer-bare", "test", True),
    ("compound-all-head", "all(test, target_os = \"linux\")", True),
    ("compound-any-head", "any(test, kani)", True),
    ("compound-any-feature", "any(test, feature = \"test-utils\")", True),
    ("not-head", "not(test)", False),
    ("plain-feature", "feature = \"test-utils\"", False),
    # The fourth-spelling face: nested compounds are GATING per the
    # ported canonical (recursion) while the flat spelling table
    # reads them UNKNOWN — the parity differential's red (W11-BU).
    ("nested-compound", "any(all(test, unix), kani)", True),
]


def cfg_test_attr_spans(text: str, source: str = "<input>"):
    """Spans `(start, end, inner)` of every TEST-GATING cfg attribute
    in `text` (any spelling — the ported canonical predicate decides).
    The shared recognizer for every scanner's cfg(test) handling
    (streaming_open_ban's mod-pruner twin consumes this instead of a
    private bare-spelling regex)."""
    lexed, _ = lex(text, blank_string_bodies=True)
    return [
        (a, b, inner)
        for (a, b, inner, pred) in iter_cfg_attrs(lexed, source)
        if cfg_pred_gates_test(pred)
    ]


# --- the cfg(test)-reachability resolver (WO-S8-6, bug_152, OQ-13) ----
#
# Test-code FILE membership is decided ONCE, from the module graph --
# never re-decided per scanner by path convention. The old per-scanner
# filename lists (`/tests/` dirs, `test_helpers.rs`, `tests.rs`,
# `*_tests.rs`) could not see a sibling-file test module whose
# `#[cfg(test)]` lives on the PARENT `mod` declaration (per-file
# strip_cfg_test cannot either -- the gate is in the parent file), so
# `mbt_tests.rs`-class files enrolled as production rows and sat
# undischargeable in shrink-only grandfathers. The derivation: per
# crate-src tree, parse every file's `mod NAME;` declarations from the
# LEXED text; a declaration blanked by strip_cfg_test is test-gated
# (one classification source -- the pruner itself); a file named by a
# gated declaration, or declared FROM a test-reachable file, is
# test-reachable (transitive). RECORDED HOME: this module -- the
# shared exact lexer every scanner already imports (one grammar, one
# place; OQ-13's "the slot RECORDS which" duty).
_MOD_DECL = re.compile(r"(?:^|[;{}\n])\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;")


def _mod_decl_dir(rel: str) -> str:
    """Directory (POSIX rel, '' = crate-src root) a file's `mod x;`
    declarations resolve in: mod.rs/lib.rs/main.rs declare siblings;
    a 2018-style `foo.rs` declares into `foo/`."""
    parts = rel.split("/")
    name = parts[-1]
    if name in ("mod.rs", "lib.rs", "main.rs"):
        return "/".join(parts[:-1])
    return "/".join(parts[:-1] + [name[: -len(".rs")]])


_RESOLVER_CACHE: dict = {}


def cfg_test_reachable_files(crate_src):
    """The ONE resolver (bug_152): the set of .rs files (POSIX paths
    relative to `crate_src`) reachable only through cfg(test)-gated
    `mod` declarations. Files the graph never names stay PRODUCTION
    (scanning more is the bans' fail-closed direction; a duration or
    exit-edge row in such a file is honest debt, not a false
    enrollment).

    Memoized per (path, stat-signature): the three census walks
    resolve the same crates in one process; the signature keeps the
    cache honest under selftest fixtures that rewrite a path."""
    import pathlib

    crate_src = pathlib.Path(crate_src)
    sig = (
        str(crate_src.resolve()),
        tuple(
            (f.relative_to(crate_src).as_posix(), st.st_size, st.st_mtime_ns)
            for f in sorted(crate_src.rglob("*.rs"))
            for st in (f.stat(),)
        ),
    )
    cached = _RESOLVER_CACHE.get(sig)
    if cached is not None:
        return cached
    decls = {}  # parent rel -> [(child rel, gated)]
    rels = []
    for f in sorted(crate_src.rglob("*.rs")):
        rels.append(f.relative_to(crate_src).as_posix())
    rel_set = set(rels)
    for rel in rels:
        text = (crate_src / rel).read_text(encoding="utf-8")
        lexed, _ = lex(text, blank_string_bodies=True)
        try:
            pruned_text = strip_cfg_test(text, source=rel)
        except StripError:
            # The pruner's consumers fail closed on their own scans;
            # for the graph an unclassifiable file contributes no
            # gated edges (its own membership is its parents' call).
            pruned = lexed
        else:
            pruned, _ = lex(pruned_text, blank_string_bodies=True)
        live = {m.group(1) for m in _MOD_DECL.finditer(pruned)}
        edges = []
        d = _mod_decl_dir(rel)
        for m in _MOD_DECL.finditer(lexed):
            name = m.group(1)
            for cand in (
                f"{d}/{name}.rs" if d else f"{name}.rs",
                f"{d}/{name}/mod.rs" if d else f"{name}/mod.rs",
            ):
                if cand in rel_set:
                    edges.append((cand, name not in live))
                    break
        decls[rel] = edges
    test_set = set()
    changed = True
    while changed:
        changed = False
        for parent, edges in decls.items():
            parent_test = parent in test_set
            for child, gated in edges:
                if (gated or parent_test) and child not in test_set:
                    test_set.add(child)
                    changed = True
    _RESOLVER_CACHE[sig] = test_set
    return test_set


CANONICAL_PIN = "nix/cfg-pruner-canonical.pin"


def extract_canonical(lint_rs_text: str) -> str:
    """The normalized source of xtask's `cfg_pred_gates_test` (the
    canonical predicate this module ports): whitespace-collapsed fn
    text, doc comment excluded. [GEN-SET]: `rust_strip.py
    --extract-canonical xtask/src/lint.rs > nix/cfg-pruner-canonical
    .pin` mints the pin; the parity gate re-extracts at every run and
    fails on drift, so the canonical cannot change without the port
    being re-derived."""
    for name, start, _bs, body_end in fn_extents(lint_rs_text):
        if name == "cfg_pred_gates_test":
            return " ".join(lint_rs_text[start : body_end + 1].split())
    raise SystemExit(
        "FAIL: fn cfg_pred_gates_test not found — the canonical moved; "
        "re-derive the port and the pin"
    )


# ((vvvvv)) — THE STAGING-HAZARD MARKER, definition site (WO-S8-2,
# merged_bug_008; the triage refutation is binding: the marker is
# INTENTIONAL, never a paste artifact — do not delete occurrences).
#
# (vvvvv) is the wave-log hazard letter for the staging discipline:
# a nix check validating the EXISTENCE or content of repo paths must
# STAGE the surface it quantifies over — a check whose fileset omits
# the surface goes green vacuously while resolving locally (the
# round-8 wave-close incident; full text at the wave-log anchor).
# A comment or diagnostic tagged `((vvvvv))` marks a refusal arm or
# fileset whose verdict depends on staging completeness: the arm
# fires exactly when the staged tree lacks a surface the check needs
# (the occurrences across nix/misc-checks.nix `Full-tree staging`
# fileset notes and scanner refusal arms, incl. the cfg-pruner-parity
# check's own, are this cross-reference). Grep for `(vvvvv)` to find
# every staged-surface dependency; this block is the greppable
# definition the idiom previously lacked.
def parity_scan(root, lint_rel="xtask/src/lint.rs"):
    """The cfg-pruner parity gate (WO-S8-2): over every cfg attribute
    in rio-*/src + xtask/src, the flat spelling table (axis 2) and the
    ported canonical predicate (axis 1) must agree, and no attribute
    may be outside the table (the fourth-spelling tripwire); plus the
    canonical-source pin must match xtask's live text.

    ONE result shape (merged_bug_008): EVERY arm — the early refusal
    arms included — returns `(fails, n_attrs)`; the refusal arms
    previously returned a bare 1-element list and the caller's
    2-tuple unpack died as ValueError exactly when the staging
    regression the diagnostics guard against occurred (the crafted
    remediation text was unreachable)."""
    import pathlib

    root = pathlib.Path(root)
    fails = []
    pin_path = root / CANONICAL_PIN
    lint_path = root / lint_rel
    if not lint_path.is_file():
        return [f"{lint_rel} missing — the canonical surface is not staged ((vvvvv))"], 0
    live_canonical = extract_canonical(lint_path.read_text())
    if not pin_path.is_file():
        return [f"{CANONICAL_PIN} missing — mint it: rust_strip.py --extract-canonical {lint_rel}"], 0
    if pin_path.read_text().strip() != live_canonical:
        fails.append(
            f"{lint_rel}: cfg_pred_gates_test drifted from {CANONICAL_PIN} — "
            f"the canonical changed; RE-DERIVE the python port "
            f"(cfg_pred_gates_test/classify_cfg_spelling) against the new "
            f"semantics, then re-mint the pin"
        )
    files = []
    for crate_src in sorted(root.glob("rio-*/src")):
        files.extend(sorted(crate_src.rglob("*.rs")))
    x = root / "xtask" / "src"
    if x.is_dir():
        files.extend(sorted(x.rglob("*.rs")))
    n_attrs = 0
    for f in files:
        rel = str(f.relative_to(root))
        text = f.read_text(encoding="utf-8")
        lexed, _ = lex(text, blank_string_bodies=True)
        try:
            for a, _b, _inner, pred in iter_cfg_attrs(lexed, rel):
                n_attrs += 1
                table = classify_cfg_spelling(pred)
                port = cfg_pred_gates_test(pred)
                if table is None:
                    fails.append(
                        f"{rel}:{_line_of(lexed, a)}: cfg spelling outside the "
                        f"enumerated table (`{' '.join(pred.split())}`, port "
                        f"says gates={port}) — extend CFG_SPELLINGS/"
                        f"classify_cfg_spelling with this form and its plant"
                    )
                elif table[1] != port:
                    fails.append(
                        f"{rel}:{_line_of(lexed, a)}: spelling-table/"
                        f"ported-canonical DISAGREEMENT on "
                        f"`{' '.join(pred.split())}` (table {table} vs port "
                        f"gates={port}) — one axis rotted; re-derive both "
                        f"against the canonical"
                    )
        except StripError as e:
            fails.append(str(e))
    if n_attrs == 0:
        fails.append("zero cfg attributes found — the parity population is vacuous")
    return fails, n_attrs


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
    # --- WO-S8-1 (merged_bug_018, R22″ banner): the derived vector
    # alphabet + the fail-closed refusal arms -------------------------
    # W11-BS — the generator's completeness pin, at the quantifier it
    # actually certifies: every cell of the DERIVED alphabet (the syn
    # Item set + attachment positions, grammar⊇language by derivation,
    # never a hand list) has ≥1 vector; every vector blanks its gated
    # payload, keeps every production marker, and preserves line
    # numbering. A silently dropped cell is a red here.
    expected_cells = {
        # syn::Item variants (the host grammar's item positions) …
        "Const", "Enum", "ExternCrate", "Fn", "ForeignMod", "Impl",
        "Macro", "Mod", "Static", "Struct", "Trait", "TraitAlias",
        "Type", "Union", "Use",
        # … plus the non-item attachment positions.
        "Variant", "FieldNamed", "FieldTuple", "Arm", "FieldValue",
        "Statement",
    }
    got_cells = {cell for cell, _ in CFG_VECTOR_ALPHABET}
    if got_cells != expected_cells:
        return (
            f"W11-BS: vector alphabet drifted from the derived cell set — "
            f"missing {sorted(expected_cells - got_cells)}, "
            f"extra {sorted(got_cells - expected_cells)}"
        )
    for cell, vectors in CFG_VECTOR_ALPHABET:
        if not vectors:
            return f"W11-BS: alphabet cell {cell} has zero vectors"
        for v in vectors:
            try:
                stripped = strip_cfg_test(v, source=f"vector/{cell}")
            except StripError as e:
                return f"vector ({cell}) refused a grammatical form: {e}"
            if _T in stripped:
                return f"vector ({cell}): gated payload classified production: {stripped!r}"
            if stripped.count(_P) != v.count(_P):
                return f"vector ({cell}): production blanked (the W11-BR class): {stripped!r}"
            if stripped.count("\n") != v.count("\n"):
                return f"vector ({cell}): line numbering broke"
    # W11-BQ post-fix: the floor.rs array-const artifacts die — no
    # stray `11] = {` mid-signature tail, no leaked const body.
    bq = strip_cfg_test(
        "#[cfg(test)]\nconst W: [u8; 11] = {\n    [1; 11]\n};\nfn live() {}\n"
    )
    if "11] = {" in bq or "[1; 11]" in bq:
        return f"W11-BQ: array-const mid-signature break survived: {bq!r}"
    if "fn live" not in bq:
        return f"W11-BQ: production after the const was blanked: {bq!r}"
    # --- WO-S8-1 (bug_049): the expression-token axis (W12-AY) ------
    # The product completeness pin: every grammar-admitted token ×
    # position cell has a vector (a silently dropped cell is a red
    # here), the exclusion set is exact, and every vector blanks its
    # payload, keeps every production marker, and preserves line
    # numbering — refuse-or-correct, never a silent neighbor blank.
    admitted = {
        (t, p)
        for t in _EXPR_TOKENS
        for p in _EXPR_POSITIONS
        if (t, p) not in CFG_EXPR_EXCLUDED_CELLS
    }
    got_expr = {k for k in CFG_EXPR_TOKEN_VECTORS if k[1] != "Specimen"}
    if got_expr != admitted:
        return (
            f"W12-AY: expression-token vector table drifted from the "
            f"admitted product — missing {sorted(admitted - got_expr)}, "
            f"extra {sorted(got_expr - admitted)}"
        )
    for (tok, pos), v in CFG_EXPR_TOKEN_VECTORS.items():
        try:
            stripped = strip_cfg_test(v, source=f"expr/{tok}/{pos}")
        except StripError as e:
            return f"W12-AY ({tok}/{pos}): grammatical vector refused: {e}"
        if _T in stripped:
            return f"W12-AY ({tok}/{pos}): gated payload survived: {stripped!r}"
        if stripped.count(_P) != v.count(_P):
            return (
                f"W12-AY ({tok}/{pos}): production neighbor blanked — the "
                f"bug_049 swallow face: {stripped!r}"
            )
        if tok == "brace-else" and "else" in stripped:
            return (
                f"W12-AY ({tok}/{pos}): dangling else residue (the "
                f"gc/collect.rs under-blank face): {stripped!r}"
            )
        if stripped.count("\n") != v.count("\n"):
            return f"W12-AY ({tok}/{pos}): line numbering broke"
    # The refusal arms (R22″ fail-closed): one co-located plant per
    # arm; the error must NAME source:line, never silently accept.
    # (W11-BR's negative-depth vector doubles as the attachment-eof /
    # underflow refusal family; the unmatched-delim plant covers the
    # `_match_delim` return-n face.)
    for cell, src, want_line in CFG_REFUSAL_VECTORS:
        try:
            strip_cfg_test(src, source=f"refusal/{cell}")
        except StripError as e:
            if f"refusal/{cell}:{want_line}" not in str(e):
                return f"refusal ({cell}): error does not name source:line — {e}"
        else:
            return f"refusal ({cell}): malformed input silently accepted (fail-open)"
    # --- WO-S8-2 (merged_bug_088): one derived plant per spelling,
    # both differential axes, and the canonical-pin comparator -------
    # W11-BT (red-first): pre-fix, the compound spellings leaked test
    # code into the censused population — every gating spelling must
    # now prune, every non-gating spelling must now SURVIVE.
    for spelling, pred, gates in CFG_SPELLING_VECTORS:
        v = f"#[cfg({pred})]\nfn {_T}() {{ x(); }}\nfn {_P}() {{}}\n"
        stripped = strip_cfg_test(v, source=f"spelling/{spelling}")
        if gates and _T in stripped:
            return f"W11-BT ({spelling}): test code leaked into the production population: {stripped!r}"
        if not gates and _T not in stripped:
            return f"spelling ({spelling}): production-only code was pruned (the not(test) class): {stripped!r}"
        if _P not in stripped:
            return f"spelling ({spelling}): production neighbor blanked: {stripped!r}"
        # Axis agreement per vector (UNKNOWN allowed only for the
        # nested-compound tripwire cell).
        table = classify_cfg_spelling(pred)
        port = cfg_pred_gates_test(pred)
        if port != gates:
            return f"spelling ({spelling}): ported canonical wrong (gates={port}, want {gates})"
        if spelling == "nested-compound":
            if table is not None:
                return "W11-BU precondition broke: the nested compound is inside the flat table"
        elif table is None or table[1] != gates:
            return f"spelling ({spelling}): table classification wrong ({table})"
    # The inner spelling: `#![cfg(test)]` at file scope blanks the
    # WHOLE file (actor/debug.rs:5 — the live instance).
    inner_v = f"#![cfg(test)]\nfn {_T}() {{}}\nfn {_T}2() {{}}\n"
    stripped = strip_cfg_test(inner_v, source="spelling/inner-bare")
    if _T in stripped or stripped.count("\n") != inner_v.count("\n"):
        return f"spelling (inner-bare): the inner file gate did not blank the whole file: {stripped!r}"
    # … and an inner NON-gating attr (`#![cfg(unix)]`) must not blank.
    stripped = strip_cfg_test(f"#![cfg(unix)]\nfn {_P}() {{}}\n")
    if _P not in stripped:
        return "spelling (inner-non-gating): #![cfg(unix)] blanked the file"
    # The shared recognizer surface the streaming twin consumes.
    spans = cfg_test_attr_spans("#[cfg(any(test, kani))]\nmod t { }\n#[cfg(not(test))]\nmod p { }\n")
    if len(spans) != 1:
        return f"cfg_test_attr_spans: expected exactly the gating attr, got {spans}"
    # W11-BU — the parity differential reds: (a) a one-sided spelling
    # (nested compound: port gates, table UNKNOWN) is a NAMED red at
    # the parity layer; (b) a doctored canonical fails the pin
    # comparison shape (the drift tripwire).
    table = classify_cfg_spelling("any(all(test, unix), kani)")
    port = cfg_pred_gates_test("any(all(test, unix), kani)")
    if table is not None or port is not True:
        return f"W11-BU (a): the one-sided spelling did not split the axes (table={table}, port={port})"
    import pathlib as _pl
    import tempfile as _tf

    # --- WO-S8-6 (bug_152, W12-BD): the module-graph resolver -------
    # The parent-mod plant vector: a sibling FILE whose #[cfg(test)]
    # lives on the parent `mod` declaration is excluded BY DERIVATION
    # (red pre-fix: path conventions enrolled it as production); an
    # ungated sibling stays production; gating propagates through the
    # graph (a file declared from a test file is test code).
    with _tf.TemporaryDirectory() as _td:
        _c = _pl.Path(_td) / "src"
        (_c / "logs").mkdir(parents=True)
        (_c / "lib.rs").write_text(
            "mod logs;\n#[cfg(test)]\nmod mbt_tests;\nfn live() {}\n"
        )
        (_c / "mbt_tests.rs").write_text(
            "mod helper_grid;\nconst FAKE_TTL_SECS: u64 = 1;\n"
        )
        (_c / "mbt_tests").mkdir()
        (_c / "mbt_tests" / "helper_grid.rs").write_text("pub fn h() {}\n")
        (_c / "logs" / "mod.rs").write_text("pub mod sweep;\n")
        (_c / "logs" / "sweep.rs").write_text("pub fn s() {}\n")
        got = cfg_test_reachable_files(_c)
        if got != {"mbt_tests.rs", "mbt_tests/helper_grid.rs"}:
            return f"W12-BD: resolver derivation wrong: {sorted(got)}"
    pinned = " ".join("fn cfg_pred_gates_test(ts: T) -> bool { real }".split())
    doctored = " ".join("fn cfg_pred_gates_test(ts: T) -> bool { DOCTORED }".split())
    if pinned == doctored:
        return "W11-BU (b): the pin comparator cannot see a doctored canonical"
    # --- WO-S8-2 (merged_bug_008, W12-AZ): every parity_scan arm
    # returns the caller's (fails, n_attrs) shape, and every refusal
    # arm is selftest-driven THROUGH that unpack — pre-fix the two
    # early arms returned a bare list and this exact unpack died as
    # `ValueError: not enough values to unpack (expected 2, got 1)`,
    # losing the crafted staging diagnostics at the moment they were
    # needed (selftest had zero parity_scan coverage).
    with _tf.TemporaryDirectory() as _td:
        _root = _pl.Path(_td)
        # Arm 1: the canonical surface not staged ((vvvvv)).
        fails, n = parity_scan(_root)
        if n != 0 or len(fails) != 1 or "not staged ((vvvvv))" not in fails[0]:
            return f"W12-AZ (missing-lint arm): {fails!r}, {n}"
        # Arm 2: the pin not minted.
        (_root / "xtask" / "src").mkdir(parents=True)
        (_root / "xtask" / "src" / "lint.rs").write_text(
            "fn cfg_pred_gates_test(p: &str) -> bool { false }\n"
        )
        fails, n = parity_scan(_root)
        if n != 0 or len(fails) != 1 or "--extract-canonical" not in fails[0]:
            return f"W12-AZ (missing-pin arm): {fails!r}, {n}"
        # Arm 3: canonical drift — the pin disagrees with the live fn;
        # the scan still runs and counts attributes (one staged here).
        (_root / CANONICAL_PIN).parent.mkdir(parents=True, exist_ok=True)
        (_root / CANONICAL_PIN).write_text("fn cfg_pred_gates_test(OLD) { }\n")
        (_root / "rio-x" / "src").mkdir(parents=True)
        (_root / "rio-x" / "src" / "lib.rs").write_text(
            "#[cfg(test)]\nmod t { }\nfn keep() {}\n"
        )
        fails, n = parity_scan(_root)
        if n != 1 or len(fails) != 1 or "drifted" not in fails[0]:
            return f"W12-AZ (pin-drift arm): {fails!r}, {n}"
        # Arm 4: the vacuous-population refusal (zero cfg attributes).
        (_root / CANONICAL_PIN).write_text(
            " ".join((_root / "xtask" / "src" / "lint.rs").read_text().split()) + "\n"
        )
        (_root / "rio-x" / "src" / "lib.rs").write_text("fn keep() {}\n")
        fails, n = parity_scan(_root)
        if n != 0 or len(fails) != 1 or "vacuous" not in fails[0]:
            return f"W12-AZ (vacuous-population arm): {fails!r}, {n}"
    return None


if __name__ == "__main__":
    import sys

    err = selftest()
    if err:
        print(f"FAIL: rust-strip self-test — {err}", file=sys.stderr)
        sys.exit(1)
    if len(sys.argv) >= 2 and sys.argv[1] == "--extract-canonical":
        # [GEN-SET] mint for nix/cfg-pruner-canonical.pin (WO-S8-2).
        if len(sys.argv) != 3:
            print("usage: rust_strip.py --extract-canonical LINT_RS", file=sys.stderr)
            sys.exit(2)
        print(extract_canonical(open(sys.argv[2], encoding="utf-8").read()))
        sys.exit(0)
    if len(sys.argv) >= 2 and sys.argv[1] == "--parity":
        # The cfg-pruner parity gate (WO-S8-2; the misc-checks
        # `cfg-pruner-parity` attr drives this).
        if len(sys.argv) != 3:
            print("usage: rust_strip.py --parity ROOT", file=sys.stderr)
            sys.exit(2)
        fails, n_attrs = parity_scan(sys.argv[2])
        if fails:
            print("FAIL: cfg-pruner parity —", file=sys.stderr)
            for x in fails:
                print(f"  {x}", file=sys.stderr)
            sys.exit(1)
        print(
            f"cfg-pruner-parity: {n_attrs} cfg attributes, spelling table == "
            f"ported canonical everywhere; canonical pin verified"
        )
        sys.exit(0)
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
