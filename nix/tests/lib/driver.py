# Batch-subtest harness for VM testScripts (issue #57 task 1e).
#
# Spliced into testScript via `${common.driver}` (see common.nix
# mkBatchTest), AFTER the scenario prelude so every Machine global
# (k3s_server, client, ...), assertions.py helper, and prelude-level
# def is already in scope. Runs inside the NixOS test driver's Python
# interpreter.
#
# Design — sequential, NOT concurrent:
#   The win issue #57 is after is "one k3s boot instead of N", not
#   "N subtests in parallel". The nixos-test-driver Machine class is
#   NOT thread-safe (.claude/rules/ci-failure-patterns.md "Machine.
#   succeed() thread-unsafe — rc int-on-empty"): execute() writes to a
#   single shared shell pipe and the XML logger's nested() mutates
#   shared state. A locking shim is possible but every prelude helper
#   (sched_metric_wait, pf_exec, build()) bottoms out in tight
#   wait_until_succeeds polls — interleaving those across threads buys
#   little wall-clock and adds a flake surface. run_batch therefore
#   runs groups SERIALLY in list order with per-group try/except +
#   timing + failure aggregation. A failing group does not abort the
#   batch: failures collect, every group runs, then a single
#   AssertionError lists every failure + traceback so one CI run names
#   ALL broken groups instead of stopping at the first.
#
# `run_concurrent` is exported as an alias of run_batch so callers
# written against the issue's original spelling work; the docstring is
# the authority on actual behaviour. If a future change makes Machine
# calls thread-safe (per-instance lock + thread-safe subtest shim),
# the alias is the seam to swap in real concurrency without touching
# mkBatchTest or any scenario file.

import time as _rio_time
import traceback as _rio_tb
from dataclasses import dataclass, field


@dataclass
class SubtestCtx:
    """Per-group context handed to each group fn as its single arg.

    `tenant` / `pool` are batch-unique names a group MAY use when it
    creates its own PG tenant row or Pool CR; the harness does not
    create them itself (groups that don't need isolation ignore the
    ctx entirely — none of batch-a's groups read it today). note()
    appends to the per-group log tail surfaced in the failure report."""

    name: str
    tenant: str
    pool: str
    timeout: int
    log: list = field(default_factory=list)

    def note(self, msg: str) -> None:
        line = f"[{self.name}] {msg}"
        self.log.append(line)
        print(line, flush=True)


def run_batch(groups, isolation: str = "tenant") -> None:
    """Run group callables sequentially inside one booted fixture.

    groups     — list of (name, fn, timeout_secs). fn(ctx) is plain
                 blocking Python.
    isolation  — "tenant" → ctx.tenant = ctx.pool = f"batch-{name}";
                 "none"   → both "" (caller handles isolation).
                 Advisory only — see SubtestCtx docstring.

    Per-group timeout is BEST-EFFORT: elapsed is checked after fn()
    returns and a timeout is recorded as a failure if exceeded, but a
    hung Machine.wait_until_succeeds inside fn() cannot be interrupted
    from here — runNixOSTest.globalTimeout (set by mkBatchTest) is the
    hard backstop. After all groups finish, a PASS/FAIL summary table
    is printed and a single AssertionError raised iff any failed."""
    assert isolation in ("tenant", "none"), f"unknown isolation={isolation!r}"

    results: dict[str, tuple[bool, str]] = {}

    for name, fn, timeout in groups:
        ctx = SubtestCtx(
            name=name,
            tenant=f"batch-{name}" if isolation == "tenant" else "",
            pool=f"batch-{name}" if isolation == "tenant" else "",
            timeout=timeout,
        )
        t0 = _rio_time.monotonic()
        # Main-thread → the driver's real subtest() context manager is
        # safe here; gives each group its own fold in the HTML report.
        with subtest(f"batch group: {name}"):  # noqa: F821 — driver global
            ctx.note(f"start (timeout={timeout}s)")
            try:
                fn(ctx)
                elapsed = _rio_time.monotonic() - t0
                if elapsed > timeout:
                    results[name] = (
                        False,
                        f"TIMEOUT: ok but {elapsed:.1f}s > budget {timeout}s "
                        f"(raise the group timeout or split the group)",
                    )
                else:
                    results[name] = (True, f"ok in {elapsed:.1f}s")
            except Exception as e:  # noqa: BLE001
                elapsed = _rio_time.monotonic() - t0
                tail = "\n".join(ctx.log[-20:])
                results[name] = (
                    False,
                    f"FAIL after {elapsed:.1f}s: {e}\n"
                    f"--- log tail ({name}) ---\n{tail}\n"
                    f"--- traceback ---\n{_rio_tb.format_exc()}",
                )
            ctx.note(results[name][1].splitlines()[0])

    print("\n=== run_batch summary ===", flush=True)
    for name, _, _ in groups:
        ok, msg = results[name]
        print(f"  {'PASS' if ok else 'FAIL'}  {name}: {msg.splitlines()[0]}")
    failed = {n: m for n, (ok, m) in results.items() if not ok}
    if failed:
        raise AssertionError(
            f"{len(failed)}/{len(groups)} batch group(s) failed:\n\n"
            + "\n\n".join(f"### {n}\n{m}" for n, m in failed.items())
        )


# Alias — see module docstring. Sequential today; the seam for real
# concurrency once Machine.execute() is made thread-safe.
run_concurrent = run_batch
