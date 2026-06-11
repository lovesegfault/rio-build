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

CLASSIFIER_FILE = "rio-controller/src/reconcilers/node_informer.rs"
SERVER_FILE = "rio-scheduler/src/admin/mod.rs"

ROW = re.compile(r"producer-census:\s*([a-z_]+)\s*=\s*([a-z-]+)")
SNAKE = re.compile(r"^[a-z][a-z_]*$")
DISPOSITIONS = {"emitted", "never-emitted", "defaulted"}


def strip_comments(text: str) -> str:
    return "\n".join(line.split("//")[0] if "//" in line and '"//' not in line else line for line in text.splitlines())


def parse_rows(classifier_text: str):
    """Rows are read from the RAW text (they live in doc comments)."""
    return ROW.findall(classifier_text)


def emitted_codes(server_text: str) -> set:
    """Status constructors in the COMMENT-STRIPPED server module."""
    return set(re.findall(r"Status::([a-z_]+)\s*\(", strip_comments(server_text)))


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

    # --- self-test arms (planted, must fail) ---------------------------
    gates_iv_only = 'fn append(){ return Err(Status::invalid_argument("kind")); }'
    # Arm A: the shipped defect verbatim — out_of_range cited as
    # emitted against gates that mint invalid_argument only.
    f_a = check([("out_of_range", "emitted")], emitted_codes(gates_iv_only))
    if len(f_a) != 1 or "FALSE PRODUCER CITE" not in f_a[0]:
        print(f"FAIL: self-test arm A (shipped false cite) expected 1 false-cite failure, got {f_a}", file=sys.stderr)
        return 1
    # Arm B: emitter-set drift — never-emitted vs an emitting source.
    f_b = check([("out_of_range", "never-emitted")], emitted_codes('Err(Status::out_of_range("v"))'))
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
    f_d = check([("out_of_range", "emitted")], emitted_codes("// Status::out_of_range( in prose only"))
    if len(f_d) != 1:
        print(f"FAIL: self-test arm D (comment-lane) expected the prose mention ignored, got {f_d}", file=sys.stderr)
        return 1

    # --- the real scan --------------------------------------------------
    classifier = (src_root / CLASSIFIER_FILE).read_text()
    server = (src_root / SERVER_FILE).read_text()
    rows = parse_rows(classifier)
    emitted = emitted_codes(server)
    fails = check(rows, emitted)
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
