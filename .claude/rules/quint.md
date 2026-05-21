---
paths:
  - "docs/spec/models/**/*.qnt"
  - "nix/quint.nix"
  - "**/mbt_tests.rs"
---

# Quint specification rules

Distilled from [quint-llm-kit](https://github.com/informalsystems/quint-llm-kit) (Informal
Systems' Claude Code toolkit for Quint; last reviewed at rev `520e563`) plus rio-build's own
porting experience. When you need an idiom this file doesn't cover, clone the kit and grep its
curated knowledge base — the language/builtin docs, a dozen pattern cards, and ~75 curated
example specs:
`git clone --depth 1 https://github.com/informalsystems/quint-llm-kit ~/tmp/quint-llm-kit` →
`mcp-servers/kb/kb/docs/{builtin,lang}.md`, `kb/patterns/`, and `kb/examples/` (`classic/` has
Paxos, TwoPhaseCommit, ReliableBroadcast; `cosmos/` has Tendermint).

Quint is TLA+ with a programmer-facing syntax, a static type system, and an effect system.
Same semantics: a spec is `init` + a nondeterministic `step` relation + invariants. The
verification stack is `typecheck` (static) → `test` (deterministic runs) → `run` (random
simulation) → `verify` (model checking). The CI wiring, the backend trade-offs, and the
budget policy live in `nix/quint.nix`'s header — read it before adding or tuning a check.

## Hard language constraints

Violating these is a compile error with no workaround. Check here FIRST on any parse/type error.

| Constraint | Wrong | Right |
|---|---|---|
| **No string manipulation** — strings are opaque (compare, use as keys; nothing else) | `"a" + "b"`, `s.length()` | IDs, sum types, records |
| **No nested pattern matching** | `\| Req(Prepare(n,v)) =>` | Match one level, then match the inner binding |
| **No destructuring** | `val (x,y) = p`, `val {a} = r` | `p._1`, `p._2`, `r.a` |
| **No mutation, loops, early return** | `var x = 1; x = 2`, `for`, `return` | `val` rebinding, `.map`/`.fold`, `if-else`/`match` as expressions |
| **Parameterless pure def has no parens** | `pure def f() = e` | `pure def f = e` |
| **Map type is an arrow** | `Map[str, int]` | `str -> int` |
| **Record update is spread** | `r.with(f, v)` | `{ ...r, f: v }` |
| **Variant constructors take ONE argument** | `Timeout(h, r)` | `Timeout((h, r))` then `t._1`, `t._2` |
| **`oneOf` is a method** | `oneOf(S)` | `S.oneOf()` |
| **Empty collections need type context** | `Set()` standalone | `Set(1,2,3)` or an annotated binding |
| **Builtin names cannot be redefined** — `get`, `put`, `set`, `keys`, `size`, `to`, `in`, … are map/set/list builtins | `action get(n) = …` → `QNT101` | Prefix domain actions: `apiGet`, `apiPut` |

## Undefined behavior (the simulator/verifier will not save you)

| Operation | Undefined when | Defense |
|---|---|---|
| `map.get(k)` | `k` not in the map | **Pre-populate every map in `init` with `KEYS.mapBy(k => v0)`**; never start from `Map()` |
| `set.getOnlyElement()` | size ≠ 1 | `chooseSome()` / `oneOf()` |
| `list.head()`/`.tail()`/`.nth(i)` | empty / out of bounds | Guard on `.length()` first |
| `i.to(j)` | `i > j` | Guard `i <= j` |

## The effect system (what TLA+ makes you discover at TLC-time, Quint checks statically)

| Keyword | May read state? | May update state (`x' = e`)? | Use for |
|---|---|---|---|
| `pure val` / `pure def` | no | no | Constants, all business logic |
| `val` / `def` | yes | no | Derived state, **invariants, witnesses** |
| `action` | yes | yes | `init`, `step`, transitions |
| `run` | — | — | Deterministic test scenarios (`init.then(a).expect(p)`) |
| `nondet x = S.oneOf()` | — | — | Nondeterministic VALUE choice (TLA+ `\E x \in S`); only inside actions/runs |

`all { … }` = conjunction (TLA+ `/\`) — every state variable must be assigned exactly once
across the action. `any { … }` = nondeterministic ACTION choice (TLA+ `\/`). An action whose
`all{}` contains a false condition is *disabled*, not an error.

## Spec structure (the canonical section order — follow it, reviewers expect it)

```
module name {
  // 1. types        — a `State` record type only if the spec is large; type aliases for
  //                    domain ints/strs
  // 2. constants    — pure val NODES = Set("n1", "n2"); use `const` for anything a fault
  //                    regime overrides via module instantiation
  // 3. pure defs    — ALL business logic; (State, params) => {success, newState} for the
  //                    thin-actions pattern, or plain pure predicates for guards
  // 4. state vars   — var x: T  (one per line; maps pre-populated in init)
  // 5. invariants   — val name = <property over vars>   (safety: must ALWAYS hold)
  //    witnesses    — val canReachX = not(<interesting state>)   (must be VIOLATED)
  // 6. actions      — thin: evaluate a pure def, assign every var exactly once
  // 7. init         — action init = all { every var gets its initial value }
  // 8. step         — action step = { nondet n = S.oneOf()  any { a1(n), a2(n), … } }
  // 9. runs         — deterministic scenarios
  // N. fault regimes — module nameRegime { import name(Const = v, …).* } — one per regime,
  //                    each the --main of its own check (replaces one-.tla-N-.cfgs)
}
```

## Witnesses vs invariants — the non-vacuity discipline

This is rio-build's TLA+ "deliberately-weakened test" / "non-vacuity check" discipline,
productized:

| | Invariant (safety) | Witness (reachability) |
|---|---|---|
| Form | `val inv = <always true>` | `val canReachX = not(<interesting state>)` |
| **VIOLATED means** | ❌ safety bug — capture the trace | ✅ the state IS reachable — the spec is not over-constrained |
| **SATISFIED means** | ✅ safety holds | ⚠️ the scenario is unreachable — every invariant that depends on it is vacuous |

**An invariant that holds over a state space that never reaches the contended state proves
nothing.** Every safety invariant MUST be paired with at least one witness showing the state
it protects against is reachable, and (for the load-bearing guard conditions) a documented
weakened-test procedure showing the invariant is falsified without the guard. Wire the
load-bearing witnesses as expect-violation checks (`mkQuintWitnessCheck` in `nix/quint.nix`,
the `quint-leader-election-witness-*` checks are the exemplar): the check passes only when the
checker violates the witness, so "the scenario is still reachable" is re-proven by CI instead
of resting on a verified-at-a-commit note. The header comment records the PROCEDURE and the
CLAIM ("this witness is violated; deleting guard X falsifies invariant Y"); the measured
depths, state counts, and wall-clocks go in the commit message and the check's output
transcript ONLY — volatile measurements never go in comments, because they shift on every
model change and a stale figure is worse than none.

## The verification workflow

| Stage | Command | Catches | Budget |
|---|---|---|---|
| 1. Typecheck | `quint typecheck spec.qnt` | Type and effect errors, unassigned state vars | instant; run after EVERY edit |
| 2. Deterministic runs | `quint test spec.qnt --main=<mod> --match=<run>` | The scenario you hand-wrote doesn't behave as narrated | seconds; **always pass `--match`** (bare `quint test` runs builtin tests and produces false confidence) |
| 3. Random simulation | `quint run spec.qnt --invariant=<i> --max-steps=100 --max-samples=1000 --backend=rust` | Shallow violations, witness reachability | seconds; a smoke test, NOT a proof |
| 4. Shallow BMC (dev loop) | `quint verify spec.qnt --invariant=<i>` (default = Apalache, 10 steps) | ALL executions up to 10 steps, symbolically | seconds to find a shallow counterexample. **Single-threaded and superlinear in `--max-steps` — do not raise the bound past ~10–15.** |
| 5. **Exhaustive check (CI)** | `quint verify --backend=tlc spec.qnt --invariant=<i>` | **Every reachable state** | the real proof; parallel across all cores; requires a FINITE state space |

### TLC-backend requirements (these are what the hand-written `.cfg`s used to provide)

- **A finite state space.** Every variable that can grow without bound (a counter, an rv, a
  clock) needs a ceiling **as an action precondition** (`s.lease.rv < MAX_RV` inside the
  guard), NOT a saturating clamp (two nodes parked at the same ceiling value falsify
  distinctness invariants as a state-space artifact). Without the ceiling TLC never
  terminates — the unbounded CAS fragment from the migration had to be killed mid-run, still
  growing. Apalache does not need the ceiling; add it anyway so the same
  spec works under both backends.
- **Deadlock checking is already off** (quint passes `-deadlock` to TLC). The state-space
  ceiling is a "deadlock" to TLC; nothing to configure.
- **Multiple invariants are conjoined into one generated `q_inv`.** `--invariant=a,b,c` works,
  but a violation reports `Invariant q_inv is violated` — it does NOT name the failing
  conjunct. CI runs (expected green) can conjoin; when one goes red, re-run per-invariant to
  identify the culprit, or read the final state of the trace.
- **`--tlc-config=file.json`** accepts exactly `{"workers", "maxHeap", "stackSize"}` —
  JVM/runtime knobs only. It is NOT a `.cfg`-directive escape hatch.
- **`SYMMETRY` and `CONSTRAINT` are not expressible** under the TLC backend. Symmetry
  reduction is lost (~2× the state count for a 2-node symmetric model — measure, don't assume
  it matters). State constraints must be encoded as action preconditions — which is what this
  project's models already prefer (TLC checks invariants on a state *before* a CONSTRAINT
  discards it).
- **One model, N fault regimes**: declare tunable constants as `const` and write one thin
  module per regime that instantiates the core with that regime's values; each regime module
  is the `--main` of one check.
- The escape hatch for anything the generated `.cfg` cannot express: `quint compile
  --target tlaplus spec.qnt` + a hand-written `.cfg` + a `runCommand` that invokes `tlc`
  directly (the pre-migration `nix/tla.nix` in the git history is the template). The
  generated TLA+ is a build artifact; never edit it.

## Debugging a failing `run`

**Quint reports the error at the chain's FIRST line (`init`), not at the `.expect()` that
failed.** Re-run with `--verbosity=3`, count the `[Frame N]` lines to find the last action
that executed, then check every `.expect()` after that frame against the dumped state.
Reproduce a random-simulation violation with `--seed=<0x…>` from the failure output.

## Debugging an unreachable witness

When a witness refuses to be violated (or a simulation never finds the scenario you expected),
work the spec, not the seed:

- **Decompose the guard.** Find the action(s) that advance the goal variable, split each guard
  into its conjuncts, and add one throwaway witness per conjunct (`val witness_guard_N =
  not(<conjunct>)`). Run them all — the one that is never violated names the conjunct that is
  never satisfiable, and that is where the bug (or the over-constraint) lives.
- **Relax and compare.** Quantitative guard → halve the threshold; conjunction → drop the last
  clause; `all(n => P)` → `exists(n => P)`. If the original is never violated but the relaxed
  version is, the problem lives in the gap between the two.
- **Read the simulator's behavior, not just its verdict.** No actions executed ⇒ stuck at
  `init` (a precondition is false in the initial state). The same action looping ⇒ nothing
  else is enabled — a progress bug. The key actions never appearing ⇒ the witness (or a guard
  on the way to it) is too strong. Otherwise ⇒ raise `--max-steps`; the scenario may just be
  deeper than the bound.
- **Probe states in the REPL** for one-off questions instead of editing the spec:
  `{ echo 'init'; echo 'someAction("n1")'; echo 'varName'; } | quint repl -r
  docs/spec/models/<file>.qnt::<module>`.
- **Classify before fixing.** A failing run or witness is either a spec bug (the model does
  the wrong thing) or a test bug (the run/witness asks for the wrong thing) — decide which
  before editing either. Remember that every `.expect(...)` after the same `.then(...)`
  evaluates against that same frame's state, so a "wrong" expectation may just be reading the
  wrong frame.

## MBT (model-based testing) — conformance between the model and the implementation

The model checks prove the *protocol*; the MBT layer proves the *implementation is that
protocol*: quint generates traces from the spec, a Rust driver replays every step against the
real components, and the implementation's projected state is diffed against the model's after
each one. `rio-lease/src/mbt_tests.rs` (driver + projection + tests) and the `mbt-rio-lease`
check in `nix/quint.nix` are the exemplar — copy that shape. The full pattern for a component
whose state machine multiple actors observe concurrently:

1. **Model the protocol** in `docs/spec/models/<name>.qnt` (core module + one instantiation
   module per fault regime, ceilings as preconditions, witness per invariant).
2. **Verify it exhaustively** with `mkQuintCheck`, one check per regime.
3. **Write the MBT driver** against the healthy regime: the action `switch!`, the state
   projection, named runs for the known-critical scenarios, one seeded simulation.
4. **Kani** the pure decision functions the model abstracts over.
5. **One VM subtest** for the end-to-end integration the mocks cannot reach.

Rules the rio-lease driver established (the reasoning lives in its module header):

- **Every action reachable from `step` must be a named action — recursively.** quint's `--mbt`
  tracker records the *innermost* `any` disjunct's name; an anonymous `all {…}` nested inside
  any disjunct makes the whole step anonymous and undrivable. Name nested disjunctions as
  their own actions (the model's `claimOk`/`claimFails`) and re-verify the state-count
  regression — a naming refactor must not change the transition relation.
- **Grain-match the implementation to the model.** One model action ↔ one implementation call
  the driver can make. Split composed operations behind `pub(crate)` seams (rio-lease's
  `fetch_and_decide`/`act`); the public API stays the composition. The split is a feature: it
  makes the races the model explores expressible as plain Rust tests.
- **The projection is the abstraction function.** Project only what the implementation
  observably realizes; omit model-only history/bookkeeping variables, and justify every
  omission in a comment next to the projection struct.
- **Determinism policy.** The simulation's seed is pinned in the test attribute (an input,
  not a measurement). Unseeded exploration is a local activity — pin any seed that finds a
  divergence. Named runs are deterministic by construction.
- **Tooling reality.** `#[quint_run]` (simulation) is the quint-connect macro path that works.
  `quint test` cannot emit `--mbt` instrumentation, so named-run replay parses
  `quint test --out-itf` traces directly via the `itf` crate and mirrors the run's action
  sequence in Rust.
- **The mbt tests are `#[ignore]`d** (they shell out to `quint`). The dedicated check stages
  the model into the nextest workspace and runs them with `--run-ignored`; editing the model
  re-runs the conformance check — that coupling is the point.
- **A divergence is a finding to classify** — driver bug vs genuine model↔implementation
  mismatch — never something to paper over in the driver. Genuine mismatches get written down
  (the driver's module-header findings list) and either fixed or explicitly scoped out.

## Review checklist (Quint-specific; generic spec review still applies)

- [ ] Every `var` is assigned in `init` and in every branch of every action.
- [ ] Every map is pre-populated over its full key domain in `init` (`mapBy`), never `Map()`.
- [ ] Every safety invariant has a paired witness proving its contended state is reachable.
- [ ] New (or behavior-changed) actions reachable from `step` have a reachability witness
      wired as an expect-violation check (`mkQuintWitnessCheck`) — a witness only verified by
      hand at review time goes stale on the next constant change; the check keeps it honest.
- [ ] Every load-bearing guard has a documented weakened-test result (delete it → which
      invariant fails; the depth it failed at goes in the commit message, not the comment).
- [ ] Business logic lives in `pure def`s; actions are thin (the pure logic is what an MBT
      harness can replay against the implementation).
- [ ] `quint test` invocations all pass `--match`.
- [ ] `quint run` results are never cited as proof — and `quint verify` with the default
      Apalache backend only proves up to its step bound. Only `--backend=tlc` gives the
      exhaustive guarantee the CI checks claim.
- [ ] Every unbounded variable has a ceiling as an action precondition.
- [ ] The header comment states: what the spec models, the invariants and their witnesses,
      and the weakened-test procedures. It does NOT state measured figures (state counts,
      depths, wall-clocks) — those live in the introducing commit's message and the check's
      output transcript, where they cannot go stale.
- [ ] Properties that reason about "the node had the opportunity to act" are encoded as
      action PRECONDITIONS, not inferred from enabledness — a thin action is always enabled
      (a rejected attempt is a no-op self-loop), so enabledness-based arguments from TLA+ do
      not transfer.

## Deliberate omissions (revisit when the trigger occurs)

Re-reviewed against the kit at rev `520e563`; everything below still stands.

- **Choreo** (the kit's message-passing scaffolding): rio-build's protocols modeled so far
  are shared-register CAS races against the apiserver, not message-passing. Revisit when
  modeling a genuinely message-passing protocol (the builder↔scheduler heartbeat/assignment
  stream is the likely first candidate).
- **The kit's `/code:*` spec-to-implementation workflow and `label-transitions` tooling**:
  rio-build's MBT goes through quint-connect instead (see the MBT section above); revisit the
  kit's tooling only if a future component needs transition labeling quint-connect cannot
  express.
- **The kit's Docker container and MCP servers**: rio-build uses the nix dev shell; the KB is
  consumed by grepping the cloned repo. The kit now packages its KB-search and LSP MCP
  servers as a nix flake (`mcp-servers/flake.nix`), so packaging is no longer the obstacle —
  the trigger for revisiting is model-authoring becoming frequent enough that grepping the
  clone stops being good enough.
- **Apalache's `--inductive-invariant` mode**: unnecessary while every regime's state space
  stays small enough for exhaustive TLC; it is the escape hatch if a future model outgrows
  that (prove the invariant inductive instead of enumerating the states).
