#!/usr/bin/env python3
"""streaming-open-ban scanner (see nix/misc-checks.nix for the policy).

Argv: <fds.pb> <src-root>. Exits nonzero with the hit list on a naked
generated streaming-RPC open in a daemon crate.

The banned-method list comes from the FileDescriptorSet (protoc's own
parse), snake_cased the way tonic names client methods, set-deduped
(TriggerGC declared by both store.proto and admin.proto is one token).

Structure, not line heuristics (bughunt-3 S1, merged_bug_072): the old
scanner truncated each file at the FIRST `#[cfg(test)]` + `mod x;`
pair — but that shape is a MODULE REFERENCE near the top of mod.rs
files, so actor/mod.rs (~1480 lines), main.rs, nodeclaim_pool/mod.rs,
fuse/fetch/mod.rs and gc/mod.rs went almost entirely unscanned — and
its sanction window matched `with_timeout(` inside comments. Now the
source is lexically stripped first (comments and string contents
blanked POSITION-PRESERVINGLY, so line numbers stay true), inline
`#[cfg(test)] mod … { … }` blocks are removed by brace matching, and
both the ban pattern and the sanction window read only real code
tokens. Self-tests plant a red per arm — including both old evasion
shapes — before the real scan may run.
"""

import pathlib
import re
import sys

import rust_strip
from google.protobuf import descriptor_pb2

DAEMON_CRATES = [
    "rio-gateway",
    "rio-store",
    "rio-scheduler",
    "rio-controller",
    "rio-builder",
]
# Sanctioned bounding combinators: a hit is legal iff one appears in
# the 6 lines up to and including the hit line — in CODE, not in a
# comment or string (the window reads the stripped text).
SANCTION = re.compile(r"bounded_open|with_timeout_status|with_timeout\(|transport::bounded")
# Sanctioned wrapper files (daemon-crate side). log_upload.rs is the
# AppendLog transport impl; its conformance test is
# `appendlog_drain_deadline_enforced_while_open_awaited` (rio-builder).
ALLOW_FILES = {"rio-builder/src/log_upload.rs"}

# CLASS TRAJECTORY — BUDGET SPENT (merged_bug_049, the third
# structural hole after merged_bug_072 and merged_bug_110: the
# escaped-quote char walk halted AT the `'` of `'\''`). Per the
# recorded budget the strip pipeline is REBUILT on the shared exact
# lexer nix/rust_strip.py — one grammar consumed by every policy
# scanner, with its own span/blank selftest run before any arm-level
# selftest may gate. Escape handling can now only be wrong, and
# fixed, in one place.
#
# WO-S8-2 (merged_bug_088): the cfg(test) RECOGNIZER is the shared
# one too (rust_strip.cfg_test_attr_spans — the ported canonical
# predicate over every spelling: bare, all(test,…), any(test,…),
# inner #![cfg(test)]). The private bare-spelling CFG_TEST regex this
# replaced recognized 1 of 3 spellings, so any(test, kani)-style mods
# and #![cfg(test)] files leaked test code into the ban's population.
MOD_AFTER = re.compile(r"\s*(?:#\s*\[[^\]]*\]\s*)*(?:pub\s*(?:\([^)]*\)\s*)?)?mod\s+\w+\s*([;{])")


def snake(name: str) -> str:
    """heck-equivalent ToSnakeCase (tonic's method naming)."""
    name = re.sub(r"(?<=[a-z0-9])([A-Z])", r"_\1", name)
    name = re.sub(r"(?<=[A-Z])([A-Z][a-z])", r"_\1", name)
    return name.lower()


def banned_tokens(fds_path: str) -> set[str]:
    fds = descriptor_pb2.FileDescriptorSet()
    fds.ParseFromString(pathlib.Path(fds_path).read_bytes())
    tokens = set()
    for f in fds.file:
        for svc in f.service:
            for m in svc.method:
                if m.client_streaming or m.server_streaming:
                    tokens.add(snake(m.name))
    return tokens


def strip_noncode(text: str) -> str:
    """Blank comments (line + nested block), string-literal CONTENTS,
    and char-literal bodies (delimiters kept), preserving every byte
    position — newlines survive, so downstream line numbers, sanction
    windows, and brace matching all share the source's coordinates.
    Thin adapter over the shared exact lexer (merged_bug_049):
    nix/rust_strip.py owns the token grammar."""
    blanked, _spans = rust_strip.lex(text, blank_string_bodies=True)
    return blanked


def strip_cfg_test_mods(stripped: str, source: str = "<input>") -> str:
    """Remove inline `#[cfg(test)] mod … { … }` blocks by brace
    matching (on already-string/comment-blanked text, so braces are
    real). `#[cfg(test)] mod x;` REFERENCES are left alone — the
    referenced files are excluded by the /tests/ and naming rules; the
    old scanner's fatal mistake was treating that reference as "test
    code starts here" and truncating the rest of the file.

    The attr recognizer is the SHARED spelling-aware one (WO-S8-2):
    every test-gating spelling shields its mod, and a file-scope
    `#![cfg(test)]` (actor/debug.rs-class) blanks the whole file —
    that file is test code the bare-spelling twin used to scan."""
    spans = rust_strip.cfg_test_attr_spans(stripped, source)
    for a, _b, inner in spans:
        if inner and rust_strip._brace_depth_at(stripped, a) == 0:
            return "".join(ch if ch == "\n" else " " for ch in stripped)
    out = stripped
    # Process right-to-left so earlier spans' offsets stay valid.
    for a, b, inner in reversed(spans):
        if inner:
            continue
        after = MOD_AFTER.match(out, b)
        if not after or after.group(1) != "{":
            continue
        # brace-match from the opening `{`
        depth = 0
        j = after.end() - 1
        while j < len(out):
            if out[j] == "{":
                depth += 1
            elif out[j] == "}":
                depth -= 1
                if depth == 0:
                    j += 1
                    break
            j += 1
        blanked = "".join(ch if ch == "\n" else " " for ch in out[a:j])
        out = out[:a] + blanked + out[j:]
    return out


def preprocess(text: str, source: str = "<input>") -> list[str]:
    return strip_cfg_test_mods(strip_noncode(text), source).splitlines()


def scan_lines(rel: str, lines: list[str], pat: re.Pattern) -> list[str]:
    """`lines` must be PREPROCESSED (strip_noncode + cfg(test)-mod
    removal) — same length/numbering as the source."""
    hits = []
    for i, line in enumerate(lines):
        m = pat.search(line)
        if not m:
            continue
        tok = m.group(1)
        if tok == "get_path" and "/fuse/" in rel:
            continue  # FuseCache homonym, not a gRPC open
        if rel in ALLOW_FILES:
            continue
        window = "\n".join(lines[max(0, i - 6) : i + 1])
        if SANCTION.search(window):
            continue
        hits.append(f"{rel}:{i + 1}: .{tok}( — naked streaming open")
    return hits


def selftest(pat: re.Pattern, tokens: set[str]) -> str | None:
    """One planted red per rule arm; returns an error string on the
    first arm that cannot demonstrate its red (banner (b))."""
    t0 = sorted(tokens)[0]
    # Arm 1: a plain naked open MUST fire.
    planted = preprocess(f"let stream = client\n    .{t0}(req)\n    .await?;\n")
    if not scan_lines("planted/sample.rs", planted, pat):
        return "a planted naked open did not fire"
    # Arm 2: a sanctioned open MUST NOT fire.
    sanctioned = preprocess(f"let out = bounded_open(abort, BOUND, client.{t0}(req)).await;\n")
    if scan_lines("planted/sanctioned.rs", sanctioned, pat):
        return "a sanctioned bounded open fired"
    # Arm 3 (merged_bug_072 evasion 1): a `#[cfg(test)] mod x;`
    # REFERENCE must not shield the production code below it.
    mod_ref = preprocess(f'#[cfg(test)]\nmod tests;\n\nfn live() {{\n    let s = client.{t0}(req);\n}}\n')
    if not scan_lines("planted/mod_ref.rs", mod_ref, pat):
        return "a naked open below a `#[cfg(test)] mod x;` reference did not fire (truncation evasion)"
    # Arm 4 (merged_bug_072 evasion 2): `with_timeout(` in a COMMENT
    # must not sanction the open.
    comment_sanction = preprocess(f"// retry with_timeout( someday\nlet s = client\n    .{t0}(req)\n    .await?;\n")
    if not scan_lines("planted/comment_sanction.rs", comment_sanction, pat):
        return "a comment-only sanction shielded a naked open"
    # Arm 5: an open INSIDE an inline `#[cfg(test)] mod … { … }` is
    # test code and MUST NOT fire.
    inline_mod = preprocess(f"fn live() {{}}\n#[cfg(test)]\nmod tests {{\n    fn t() {{ let s = client.{t0}(req); }}\n}}\n")
    if scan_lines("planted/inline_mod.rs", inline_mod, pat):
        return "an open inside an inline cfg(test) mod fired"
    # Arm 6: a banned token inside a STRING literal must not fire.
    in_string = preprocess(f'fn live() {{ let s = ".{t0}(\"; }}\n')
    if scan_lines("planted/in_string.rs", in_string, pat):
        return "a banned token inside a string literal fired"
    # Arm 7 (merged_bug_110): a '{' CHAR LITERAL inside an inline
    # cfg(test) mod must not skew the brace matcher into consuming the
    # production open below the mod.
    char_lit = preprocess(
        "#[cfg(test)]\nmod tests {\n    fn t() { let c = '{'; }\n}\n"
        f"fn live() {{\n    let s = client.{t0}(req);\n}}\n"
    )
    if not scan_lines("planted/char_lit.rs", char_lit, pat):
        return "a '{' char literal in a test mod swallowed the production open below it (brace-skew truncation)"
    # Arm 8 (merged_bug_110): an open inside a VISIBILITY-QUALIFIED
    # inline cfg(test) mod (pub(crate)/pub(super)) is test code and
    # must not fire.
    pub_crate_mod = preprocess(
        "fn live() {}\n#[cfg(test)]\npub(crate) mod tests {\n"
        f"    fn t() {{ let s = client.{t0}(req); }}\n}}\n"
    )
    if scan_lines("planted/pub_crate_mod.rs", pub_crate_mod, pat):
        return "an open inside a pub(crate) cfg(test) mod fired (MOD_AFTER visibility hole)"
    # Arm 9 (merged_bug_049): an ESCAPED-QUOTE char literal `'\''`
    # inside an inline cfg(test) mod must not skew the brace matcher
    # into consuming the production open below the mod. The corpus
    # carries the trigger shape verbatim: rio-builder
    # executor/outputs.rs `rest.split('\'')`.
    escaped_quote = preprocess(
        "#[cfg(test)]\nmod tests {\n    fn t() { let p = ('\\'','{'); }\n}\n"
        f"fn live() {{\n    let s = client.{t0}(req);\n}}\n"
    )
    if not scan_lines("planted/escaped_quote.rs", escaped_quote, pat):
        return (
            "an escaped-quote char literal swallowed the production open "
            "below it (brace-skew truncation)"
        )
    # Arm 10 (WO-S8-2, merged_bug_088 — W11-BT's twin): one derived
    # plant per test-gating SPELLING. An open inside an
    # `any(test, kani)`/`all(test, …)` mod is test code and must not
    # fire (pre-fix: the bare-spelling regex leaked it into the
    # population); the same open under `not(test)` is PRODUCTION and
    # must fire.
    for spelling in ("any(test, kani)", "all(test, target_os = \"linux\")"):
        compound_mod = preprocess(
            f"fn live() {{}}\n#[cfg({spelling})]\nmod tests {{\n"
            f"    fn t() {{ let s = client.{t0}(req); }}\n}}\n"
        )
        if scan_lines("planted/compound_mod.rs", compound_mod, pat):
            return f"an open inside a #[cfg({spelling})] mod fired (spelling leak)"
    not_test_mod = preprocess(
        f"#[cfg(not(test))]\nmod prod {{\n    fn f() {{ let s = client\n        .{t0}(req); }}\n}}\n"
    )
    if not scan_lines("planted/not_test_mod.rs", not_test_mod, pat):
        return "a production open inside #[cfg(not(test))] did not fire (over-prune)"
    # Arm 11 (WO-S8-2): a whole-file inner `#![cfg(test)]` gate — the
    # actor/debug.rs class — is test code; nothing in it may fire.
    inner_file = preprocess(
        f"#![cfg(test)]\nfn t() {{ let s = client.{t0}(req); }}\n"
    )
    if scan_lines("planted/inner_file.rs", inner_file, pat):
        return "an open inside a #![cfg(test)] file fired (inner-spelling leak)"
    return None


def main() -> int:
    fds_path, src_root = sys.argv[1], pathlib.Path(sys.argv[2])
    tokens = banned_tokens(fds_path)
    if not tokens:
        print("FAIL: descriptor set yielded zero streaming methods — the ban is vacuous", file=sys.stderr)
        return 1
    pat = re.compile(r"\.(" + "|".join(sorted(tokens)) + r")\s*\(")

    shared_err = rust_strip.selftest()
    if shared_err:
        print(f"FAIL: rust-strip self-test — {shared_err}", file=sys.stderr)
        return 1
    err = selftest(pat, tokens)
    if err:
        print(f"FAIL: streaming-open-ban self-test — {err}", file=sys.stderr)
        return 1

    fails = []
    scanned = 0
    for crate in DAEMON_CRATES:
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            # Test code is out of scope: /tests/ submodule dirs and
            # test_helpers.rs are cfg(test)-compiled.
            # merged_bug_110: cfg(test)-gated module FILES match
            # neither exclusion above — tests.rs / *_tests.rs (the
            # naming convention backing `#[cfg(test)] mod tests;` /
            # `mod mbt_tests;` declarations) are test code.
            if "/tests/" in rel or rel.endswith("test_helpers.rs") or f.name == "tests.rs" or f.name.endswith("_tests.rs"):
                continue
            scanned += 1
            try:
                lines = preprocess(f.read_text(), rel)
            except rust_strip.StripError as e:
                # R22″ fail-closed: an unclassifiable cfg extent is a
                # named failure, never a silently mis-pruned scan.
                fails.append(f"{e} [streaming-open ban: file not classifiable]")
                continue
            fails.extend(scan_lines(rel, lines, pat))
    print(f"streaming-open-ban: scanned {scanned} files (full bodies; comments/strings/test-mods stripped structurally)")
    if fails:
        print(
            "FAIL: naked generated streaming-RPC open(s) in daemon crates —\n"
            "route through rio_common::transport::bounded_open (or a sanctioned\n"
            "combinator within the preceding 6 lines):",
            file=sys.stderr,
        )
        for h in fails:
            print(f"  {h}", file=sys.stderr)
        print(f"banned (descriptor-derived): {sorted(tokens)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
