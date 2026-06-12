#!/usr/bin/env python3
"""exposure-producer-census scanner (see nix/misc-checks.nix).

Argv: <src-root>. merged_bug_076: the node_informer drop classifier's
rationale shipped a FALSE producer cite — it justified a permanent
Refused exit for OutOfRange by citing server validation gates that
actually emit invalid_argument, and mapped FailedPrecondition (whose
only live emitters are leader-churn shapes) to the same permanent
exit. Prose producer claims drift; this scan makes them
machine-checkable.

Grammar (rows live in the classifier's doc comment in
CLASSIFIER_FILE; one row per claimed code):

    producer-census: <snake_code> = emitted        # >=1 Status::<code>( in the server module
    producer-census: <snake_code> = never-emitted  # ZERO Status::<code>( in the server module
    producer-census: <snake_code> = defaulted      # disposition derives from the refusal
                                                   # authority, no producer claim made

The server module is SERVER_FILE (the AdminService surface that owns
AppendInterruptSample). A `emitted` row whose constructor does not
appear = the false-cite defect (FAIL). A `never-emitted` row whose
constructor appears = emitter-set drift: the classifier arm was
derived from a producer set that has since changed — re-derive the
arm (FAIL). Comments are stripped from the server scan so a
constructor mentioned in prose cannot satisfy an `emitted` row.

Self-test arms run first (the house pattern): a scan that cannot fail
its planted fixtures does not gate. Arm A plants the SHIPPED defect
verbatim (out_of_range = emitted against a gate set that mints
invalid_argument only); arm B plants emitter-set drift
(never-emitted, but the source emits); arm C plants grammar rot (an
unknown disposition word).
"""

import pathlib
import re
import sys

import rust_strip

CLASSIFIER_FILE = "rio-controller/src/reconcilers/node_informer.rs"
SERVER_FILE = "rio-scheduler/src/admin/mod.rs"

ROW = re.compile(r"producer-census:\s*([a-z_]+)\s*=\s*([a-z-]+)")
SNAKE = re.compile(r"^[a-z][a-z_]*$")
DISPOSITIONS = {"emitted", "never-emitted", "defaulted"}


def strip_comments(text: str, source: str = "<input>") -> str:
    # Shared exact lexer (merged_bug_009) + the attribute-position
    # cfg(test) pruner (WO-S8-9, merged_bug_150): the sibling
    # production-population definition — SERVER_FILE carries in-file
    # #[cfg(test)] modules, so a test-lane Status constructor could
    # launder an emitted row or false-red the load-bearing
    # never-emitted row. Comments AND string bodies blanked — a
    # constructor named in prose or inside a string cannot satisfy an
    # `emitted` row. Newline-preserving.
    pruned = rust_strip.strip_cfg_test(text, source=source)
    out, _ = rust_strip.lex(pruned, blank_string_bodies=True)
    return out


def parse_rows(classifier_text: str):
    """Rows are read from the RAW text (they live in doc comments)."""
    return ROW.findall(classifier_text)


_CAMEL = re.compile(r"(?<!^)(?=[A-Z])")


def emitted_codes(server_text: str, source: str = "<input>"):
    """(codes, refusals) — Status constructors in the production
    (cfg(test)-pruned, comment-stripped) server module.

    WO-S8-9 (merged_bug_150): the constructor IDIOM axis is closed
    FAIL-CLOSED — `Status::new(Code::X, …)` maps to snake(X) (the
    idiom live in the adjacent admin/gc.rs; a mint migration to it
    previously kept never-emitted rows green while the rationale
    rotted), and any `Status::new(` whose code argument the needle
    cannot map (dynamic codes, locals) REFUSES with a named row
    rather than capturing the useless token `new`."""
    stripped = strip_comments(server_text, source)
    codes = {
        c for c in re.findall(r"Status::([a-z_]+)\s*\(", stripped) if c != "new"
    }
    refusals = []
    for m in re.finditer(r"Status::new\s*\(", stripped):
        tail = stripped[m.end() : m.end() + 200]
        cm = re.match(r"\s*(?:tonic::)?Code::([A-Za-z][A-Za-z0-9]*)", tail)
        if cm:
            codes.add(_CAMEL.sub("_", cm.group(1)).lower())
        else:
            lineno = stripped[: m.start()].count("\n") + 1
            refusals.append(
                f"{source}:{lineno}: Status::new( with a code argument the "
                f"census cannot map (dynamic/local code) — refusing "
                f"(fail-closed; a producer the census cannot see makes "
                f"every never-emitted row unfalsifiable)"
            )
    return codes, refusals


def check(rows, emitted: set):
    fails = []
    if not rows:
        fails.append("zero producer-census rows parsed from the classifier — the grammar or the doc rotted")
    for code, disposition in rows:
        if not SNAKE.match(code):
            fails.append(f"row code {code!r} is not a snake_case tonic constructor name")
            continue
        if disposition not in DISPOSITIONS:
            fails.append(f"row {code} has unknown disposition {disposition!r} (one of {sorted(DISPOSITIONS)})")
            continue
        if disposition == "emitted" and code not in emitted:
            fails.append(
                f"FALSE PRODUCER CITE: classifier claims {code} is emitted by the server module "
                f"but zero Status::{code}( constructors exist there (the merged_bug_076 shape)"
            )
        if disposition == "never-emitted" and code in emitted:
            fails.append(
                f"EMITTER-SET DRIFT: classifier claims {code} is never emitted but the server "
                f"module now mints Status::{code}( — re-derive the classifier arm"
            )
    return fails


def main() -> int:
    src_root = pathlib.Path(sys.argv[1])

    # A broken shared lexer fails closed before any scan may gate.
    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1

    # --- self-test arms (planted, must fail) ---------------------------
    gates_iv_only = 'fn append(){ return Err(Status::invalid_argument("kind")); }'
    # Arm A: the shipped defect verbatim — out_of_range cited as
    # emitted against gates that mint invalid_argument only.
    codes_a, _r = emitted_codes(gates_iv_only)
    f_a = check([("out_of_range", "emitted")], codes_a)
    if len(f_a) != 1 or "FALSE PRODUCER CITE" not in f_a[0]:
        print(f"FAIL: self-test arm A (shipped false cite) expected 1 false-cite failure, got {f_a}", file=sys.stderr)
        return 1
    # Arm B: emitter-set drift — never-emitted vs an emitting source.
    codes_b, _r = emitted_codes('Err(Status::out_of_range("v"))')
    f_b = check([("out_of_range", "never-emitted")], codes_b)
    if len(f_b) != 1 or "EMITTER-SET DRIFT" not in f_b[0]:
        print(f"FAIL: self-test arm B (emitter-set drift) expected 1 drift failure, got {f_b}", file=sys.stderr)
        return 1
    # Arm C: grammar rot.
    f_c = check([("out_of_range", "speculative")], set())
    if len(f_c) != 1 or "unknown disposition" not in f_c[0]:
        print(f"FAIL: self-test arm C (grammar rot) expected 1 grammar failure, got {f_c}", file=sys.stderr)
        return 1
    # Comment-lane pin: a constructor named only in prose must not
    # satisfy an emitted row.
    codes_d, _r = emitted_codes("// Status::out_of_range( in prose only")
    f_d = check([("out_of_range", "emitted")], codes_d)
    if len(f_d) != 1:
        print(f"FAIL: self-test arm D (comment-lane) expected the prose mention ignored, got {f_d}", file=sys.stderr)
        return 1
    # --- W12-BG (WO-S8-9, merged_bug_150): the plant pair + refusal --
    # (a) a cfg(test)-lane constructor stays OUT of the production
    # emitted set (it could launder an emitted row or false-red the
    # load-bearing never-emitted row — red pre-fix).
    test_lane = (
        "fn live() {}\n#[cfg(test)]\nmod tests {\n"
        '    fn t() { let _ = Status::out_of_range("v"); }\n}\n'
    )
    codes_t, _r = emitted_codes(test_lane, "planted/test_lane.rs")
    if "out_of_range" in codes_t:
        print("FAIL: W12-BG (a) — a cfg(test)-lane constructor entered the emitted set", file=sys.stderr)
        return 1
    f_t = check([("out_of_range", "never-emitted")], codes_t)
    if f_t:
        print(f"FAIL: W12-BG (a) — the never-emitted row false-red against test-lane code: {f_t}", file=sys.stderr)
        return 1
    # (b) the Status::new(Code::X) idiom maps IN (red pre-fix: the
    # needle captured `new` and the never-emitted row stayed green
    # through a mint migration).
    codes_n, refusals_n = emitted_codes('Err(Status::new(tonic::Code::OutOfRange, msg))')
    if codes_n != {"out_of_range"} or refusals_n:
        print(f"FAIL: W12-BG (b) — Status::new(Code::X) did not map: {codes_n}, {refusals_n}", file=sys.stderr)
        return 1
    # (c) a dynamic-code Status::new REFUSES, never captures `new`
    # (the triage's live-specimen note: adjacent gc.rs uses dynamic
    # codes — the refusal covers them honestly).
    codes_dy, refusals_dy = emitted_codes("Err(Status::new(code_for(e), msg))", "planted/dyn.rs")
    if "new" in codes_dy or len(refusals_dy) != 1 or "cannot map" not in refusals_dy[0]:
        print(f"FAIL: W12-BG (c) — the dynamic-code constructor did not refuse: {codes_dy}, {refusals_dy}", file=sys.stderr)
        return 1

    # --- the real scan --------------------------------------------------
    classifier = (src_root / CLASSIFIER_FILE).read_text()
    server = (src_root / SERVER_FILE).read_text()
    rows = parse_rows(classifier)
    emitted, refusals = emitted_codes(server, SERVER_FILE)
    fails = refusals + check(rows, emitted)
    print(
        f"exposure-producer-census: {len(rows)} classifier rows checked against "
        f"{len(emitted)} distinct Status constructors in {SERVER_FILE}"
    )
    if fails:
        print("FAIL: producer-census violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
