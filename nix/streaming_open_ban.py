#!/usr/bin/env python3
"""streaming-open-ban scanner (see nix/misc-checks.nix for the policy).

Argv: <fds.pb> <src-root>. Exits nonzero with the hit list on a naked
generated streaming-RPC open in a daemon crate.

The banned-method list comes from the FileDescriptorSet (protoc's own
parse), snake_cased the way tonic names client methods, set-deduped
(TriggerGC declared by both store.proto and admin.proto is one token).
A planted-sample negative self-test runs first: if the scanner cannot
flag a known-naked open, the check fails loudly rather than passing
vacuously.
"""

import pathlib
import re
import sys

from google.protobuf import descriptor_pb2

DAEMON_CRATES = [
    "rio-gateway",
    "rio-store",
    "rio-scheduler",
    "rio-controller",
    "rio-builder",
]
# Sanctioned bounding combinators: a hit is legal iff one appears in
# the 6 lines up to and including the hit line.
SANCTION = re.compile(r"bounded_open|with_timeout_status|with_timeout\(|transport::bounded")
# Sanctioned wrapper files (daemon-crate side). log_upload.rs is the
# AppendLog transport impl; its conformance test is
# `appendlog_drain_deadline_enforced_while_open_awaited` (rio-builder).
ALLOW_FILES = {"rio-builder/src/log_upload.rs"}


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


def scan_lines(rel: str, lines: list[str], pat: re.Pattern) -> list[str]:
    hits = []
    # Strip the trailing `#[cfg(test)] mod …` block, if any.
    cut = len(lines)
    for i, line in enumerate(lines):
        if line.strip() == "#[cfg(test)]" and i + 1 < len(lines) and lines[i + 1].lstrip().startswith("mod "):
            cut = i
            break
    for i, line in enumerate(lines[:cut]):
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


def main() -> int:
    fds_path, src_root = sys.argv[1], pathlib.Path(sys.argv[2])
    tokens = banned_tokens(fds_path)
    if not tokens:
        print("FAIL: descriptor set yielded zero streaming methods — the ban is vacuous", file=sys.stderr)
        return 1
    pat = re.compile(r"\.(" + "|".join(sorted(tokens)) + r")\s*\(")

    # Negative self-test: a planted naked open MUST fire.
    planted = ["let stream = client", f"    .{sorted(tokens)[0]}(req)", "    .await?;"]
    if not scan_lines("planted/sample.rs", planted, pat):
        print("FAIL: negative self-test — the scanner did not flag a planted naked open", file=sys.stderr)
        return 1
    # And a sanctioned planted open MUST NOT fire.
    sanctioned = [f"let out = bounded_open(abort, BOUND, client.{sorted(tokens)[0]}(req)).await;"]
    if scan_lines("planted/sanctioned.rs", sanctioned, pat):
        print("FAIL: negative self-test — the scanner flagged a sanctioned bounded open", file=sys.stderr)
        return 1

    fails = []
    for crate in DAEMON_CRATES:
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            # Test code is out of scope: /tests/ submodule dirs and
            # test_helpers.rs are cfg(test)-compiled.
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            fails.extend(scan_lines(rel, f.read_text().splitlines(), pat))
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
