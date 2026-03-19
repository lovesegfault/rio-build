"""The DAG engine. `Dag` is the runtime type — loaded, indexed, validated.

`list[PlanRow]` is the serialization format, not the query structure. The old
state.py rebuilt `{r.plan: r for r in rows}` inside each case arm; `impact()`
as a free function would rebuild reverse-adjacency per frontier-plan. Here the
constructor does it once and every method shares.

Constructor invariants are HARD: duplicate plan#, unknown dep → ValueError.
Those indicate JSONL corruption, not workflow bugs, so refusing to load is
correct. Cycle check is DELIBERATELY not in __init__ — a cycle is a workflow
bug, not corruption, and `onibus dag validate` needs to load the broken DAG
to report it.
"""

from __future__ import annotations

import sys
from collections import defaultdict, deque
from pathlib import Path
from typing import TYPE_CHECKING, Iterable

from onibus import DAG_JSONL
from onibus.jsonl import read_jsonl, write_jsonl
from onibus.models import DagValidation, Gate, PlanRow, PlanStatus

if TYPE_CHECKING:
    from onibus.collisions import CollisionIndex


class CycleError(ValueError):
    def __init__(self, cycle: list[int]):
        self.cycle = cycle
        super().__init__(f"cycle in DAG: {' → '.join(f'P{n}' for n in cycle)}")


class Dag:
    """Indexed DAG. Construct via Dag.load() or Dag(rows). Immutable after."""

    __slots__ = ("_by_num", "_rev", "_topo")

    def __init__(self, rows: Iterable[PlanRow]):
        by_num: dict[int, PlanRow] = {}
        for r in rows:
            if r.plan in by_num:
                raise ValueError(f"duplicate plan {r.plan}")
            by_num[r.plan] = r
        for r in by_num.values():
            for d in r.deps:
                if d not in by_num:
                    raise ValueError(f"P{r.plan} dep P{d} not in DAG")
        rev: dict[int, list[int]] = defaultdict(list)
        for r in by_num.values():
            for d in r.deps:
                rev[d].append(r.plan)
        self._by_num = by_num
        self._rev = dict(rev)
        self._topo: list[int] | None = None

    @classmethod
    def load(cls) -> Dag:
        return cls(read_jsonl(DAG_JSONL, PlanRow))

    def __getitem__(self, n: int) -> PlanRow:
        return self._by_num[n]

    def __contains__(self, n: int) -> bool:
        return n in self._by_num

    def __len__(self) -> int:
        return len(self._by_num)

    def rows(self) -> list[PlanRow]:
        return sorted(self._by_num.values(), key=lambda r: r.plan)

    # ─── frontier ────────────────────────────────────────────────────────────

    def frontier(self) -> set[int]:
        """UNIMPL/PARTIAL plans whose integer deps are all DONE. deps_raw-only
        rows (escape-hatch) conservatively excluded — unknown dep state."""
        done = {n for n, r in self._by_num.items() if r.status == "DONE"}
        return {
            n for n, r in self._by_num.items()
            if r.status in ("UNIMPL", "PARTIAL")
            and r.deps_raw is None
            and all(d in done for d in r.deps)
        }

    def _frontier_with(self, extra_done: set[int]) -> set[int]:
        done = {n for n, r in self._by_num.items() if r.status == "DONE"} | extra_done
        return {
            n for n, r in self._by_num.items()
            if n not in extra_done
            and r.status in ("UNIMPL", "PARTIAL")
            and r.deps_raw is None
            and all(d in done for d in r.deps)
        }

    # ─── reverse deps ────────────────────────────────────────────────────────

    def blocks(self, n: int, *, transitive: bool = False) -> list[int]:
        """Plans that list n in their deps. Transitive follows the reverse-dep
        chain (everything downstream of n)."""
        if not transitive:
            return sorted(self._rev.get(n, []))
        seen: set[int] = set()
        queue = deque(self._rev.get(n, []))
        while queue:
            m = queue.popleft()
            if m in seen:
                continue
            seen.add(m)
            queue.extend(self._rev.get(m, []))
        return sorted(seen)

    def unblocked_by(self, n: int) -> list[int]:
        """If n→DONE, which plans enter the frontier? (Hypothetical — no mutation.)
        What dag-run/SKILL.md:49 has the coordinator compute by eyeballing."""
        current = self.frontier()
        return sorted(self._frontier_with({n}) - current)

    def impact(self) -> list[tuple[int, int]]:
        """(plan, transitive_reverse_dep_count) per frontier plan, sorted desc.
        Top entry = do-this-first. All calls share self._rev."""
        f = self.frontier()
        return sorted(
            ((n, len(self.blocks(n, transitive=True))) for n in f),
            key=lambda t: (-t[1], t[0]),
        )

    def effective_priorities(self) -> dict[int, int]:
        """eff(n) = max(own declared priority, max declared priority in
        downstream closure). A plan blocking something high-prio inherits
        that priority — launching it is on the critical path. Demotion
        (prio<50) does NOT propagate — max semantics means a slow-lane plan
        doesn't drag its blockers down. Short-circuits when nothing is
        elevated (only elevations propagate)."""
        base = {n: r.priority for n, r in self._by_num.items()}
        if not any(p > 50 for p in base.values()):
            return base
        eff = dict(base)
        for n in self._by_num:
            downstream = self.blocks(n, transitive=True)
            if downstream:
                dmax = max(base[m] for m in downstream)
                if dmax > eff[n]:
                    eff[n] = dmax
        return eff

    # ─── topo + hotpath ──────────────────────────────────────────────────────

    @property
    def topo(self) -> list[int]:
        """Kahn's. Memoized. Raises CycleError (with a cycle path) if cyclic."""
        if self._topo is None:
            self._topo = self._kahn()
        return self._topo

    def _kahn(self) -> list[int]:
        indeg = {n: len(r.deps) for n, r in self._by_num.items()}
        q = deque(sorted(n for n, d in indeg.items() if d == 0))
        order = []
        while q:
            n = q.popleft()
            order.append(n)
            for m in sorted(self._rev.get(n, [])):
                indeg[m] -= 1
                if indeg[m] == 0:
                    q.append(m)
        if len(order) != len(self._by_num):
            # Find a cycle among the unvisited. Walk deps (every remaining
            # node has indeg≥1) until we revisit.
            remaining = set(self._by_num) - set(order)
            start = min(remaining)
            path, seen = [], set()
            cur = start
            while cur not in seen:
                seen.add(cur)
                path.append(cur)
                cur = next(d for d in self._by_num[cur].deps if d in remaining)
            cycle = path[path.index(cur):] + [cur]
            raise CycleError(cycle)
        return order

    def hotpath(self) -> list[int]:
        """Longest chain of not-DONE plans via dep edges. DP over topo order.
        Tells you which plan is furthest from launchable — the deepest tail."""
        not_done = {n for n, r in self._by_num.items() if r.status != "DONE"}
        # dist[n] = length of longest not-DONE chain ENDING at n
        dist: dict[int, int] = {}
        pred: dict[int, int | None] = {}
        for n in self.topo:
            if n not in not_done:
                dist[n], pred[n] = 0, None
                continue
            best, bp = 1, None
            for d in self._by_num[n].deps:
                if d in not_done and dist[d] + 1 > best:
                    best, bp = dist[d] + 1, d
            dist[n], pred[n] = best, bp
        if not not_done:
            return []
        end = max(not_done, key=lambda n: (dist[n], -n))
        chain = []
        cur: int | None = end
        while cur is not None:
            chain.append(cur)
            cur = pred[cur]
        chain.reverse()
        return chain

    # ─── launchable (takes CollisionIndex — explicit coupling) ───────────────

    def launchable(self, cx: CollisionIndex, k: int) -> list[int]:
        """Greedy: (effective_priority desc, impact desc) frontier, skip on
        file-collision. Strict sort — priority beats impact. Priority
        propagates backward through deps so bumping a blocked plan
        automatically bumps its blockers."""
        eff = self.effective_priorities()
        impact = {n: c for n, c in self.impact()}
        order = sorted(impact, key=lambda n: (-eff[n], -impact[n], n))
        picked: list[int] = []
        picked_files: set[str] = set()
        for n in order:
            if len(picked) >= k:
                break
            n_files = cx.files_of(n)
            if n_files & picked_files:
                continue
            picked.append(n)
            picked_files |= n_files
        return picked

    # ─── write gates ─────────────────────────────────────────────────────────

    def try_append(self, new: PlanRow) -> DagValidation:
        """Would appending create a problem? Doesn't mutate — caller writes
        JSONL only if .ok."""
        errors: list[str] = []
        if new.plan in self._by_num:
            errors.append(f"plan P{new.plan} already exists")
        for d in new.deps:
            if d not in self._by_num:
                errors.append(f"dep P{d} not in DAG")
        # Cycle: new depending on something that (transitively) depends on new
        # is impossible if new.plan isn't in the DAG yet. But if a dep is in
        # new.plan's own future reverse-closure... that's not possible either
        # for a fresh node. Cycle-via-append only happens if new.plan already
        # exists (which we already flag). No additional check needed.
        return DagValidation(ok=not errors, errors=errors)

    def try_transition(self, n: int, new_status: PlanStatus) -> str | None:
        """Returns a warning string for suspicious transitions, None if fine.
        DONE→UNIMPL is legit (revert) but surprising — caller decides on --force.
        Precondition: n in self. Caller checks membership; this is about
        transition validity only."""
        old = self._by_num[n].status
        if old == new_status:
            return None
        if old == "DONE" and new_status in ("UNIMPL", "PARTIAL"):
            return f"P{n} DONE→{new_status} is a revert — use --force if intentional"
        if old == "RESERVED" and new_status != "RESERVED":
            return f"P{n} is RESERVED — use --force to un-reserve"
        return None

    def validate(self) -> DagValidation:
        """Full integrity check. Constructor already enforced dup+unknown-dep;
        this adds cycle detection and soft-warning audit."""
        errors: list[str] = []
        warnings: list[str] = []
        try:
            _ = self.topo
        except CycleError as e:
            errors.append(str(e))
        for r in self._by_num.values():
            if r.deps_raw is not None:
                warnings.append(f"P{r.plan} uses deps_raw escape-hatch: {r.deps_raw!r}")
            if r.status == "DONE":
                bad = [d for d in r.deps if self._by_num[d].status != "DONE"]
                if bad:
                    warnings.append(
                        f"P{r.plan} is DONE but deps {bad} are not — out-of-order merge?"
                    )
        return DagValidation(ok=not errors, errors=errors, warnings=warnings)

    # ─── render (lifted) ─────────────────────────────────────────────────────

    def render(self) -> str:
        frontier = self.frontier()
        eff = self.effective_priorities()
        lines = [
            "| P# | Title | Deps | Exit | Mrk | Crates | Prio | Status | Ready |",
            "|---|---|---|---|---|---|---|---|---|",
        ]
        lines.extend(r.render(r.plan in frontier, eff[r.plan]) for r in self.rows())
        return "\n".join(lines)


# ─── gate_is_clear (lifted — takes Dag now, not list[PlanRow]) ───────────────


def gate_is_clear(gate: Gate | None, dag: Dag) -> bool:
    """Check whether a merge-queue gate condition is satisfied."""
    if gate is None:
        return True
    match gate.kind:
        case "plan_merged":
            assert gate.plan is not None, "plan_merged gate requires plan"
            return gate.plan in dag and dag[gate.plan].status == "DONE"
        case "ci_green":
            assert gate.log_path is not None, "ci_green gate requires log_path"
            p = Path(gate.log_path)
            return p.exists() and "status = Built" in p.read_text()
        case "manual":
            return False  # never auto-clears; coordinator removes explicitly
    raise AssertionError(f"unreachable: GateKind={gate.kind!r}")


# ─── set_status (mutates dag.jsonl — the one write-through) ──────────────────


def set_status(plan_num: int, new_status: PlanStatus, note: str = "", *, force: bool = False) -> str:
    dag = Dag.load()
    if plan_num not in dag:
        raise KeyError(f"plan {plan_num} not in DAG")
    warn = dag.try_transition(plan_num, new_status)
    if warn and not force:
        print(f"warning: {warn}", file=sys.stderr)
    rows = dag.rows()
    for r in rows:
        if r.plan == plan_num:
            r.status = new_status  # validate_assignment=True → pydantic checks the Literal
            r.note = note
            break
    write_jsonl(DAG_JSONL, rows)
    return f"P{plan_num:04d} → {new_status}" + (f" {note}" if note else "")


def set_priority(plan_num: int, priority: int) -> tuple[int, list[int]]:
    """Write-through priority bump. 1-100 range enforced by PlanRow's
    validate_assignment. Raises KeyError if plan not found. Returns
    (old_priority, frontier_plans_that_inherit_via_propagation)."""
    dag = Dag.load()
    rows = dag.rows()
    for r in rows:
        if r.plan == plan_num:
            old = r.priority
            r.priority = priority  # validate_assignment → ge=1,le=100 enforced
            break
    else:
        raise KeyError(f"plan {plan_num} not in DAG")
    write_jsonl(DAG_JSONL, rows)
    eff = Dag.load().effective_priorities()
    bumped = sorted(n for n in dag.frontier() if eff[n] >= priority and dag[n].priority < priority)
    return old, bumped
