# Fix discipline — rules distilled from rounds 14–15 fix-genealogy

When a bug is the outcome of a previous bug fix, the fix for THAT bug must name the
parent commit and the pattern number (R1–R6) in its commit message. Provenance is
part of the fix.

## R1 — Changing what a value can mean requires auditing every reader, in the same change
A change to the legal states of a shared value, error variant, or counter (new legal
None/degraded state, widened variant meaning, replaced bound, third outcome on a
two-outcome path) MUST: (a) `rg` the field/variant and paste the consumer-site list
into the commit message; (b) prefer the structural form — state-bearing Options
consumed by trusted-plane code become enums with exhaustive matches (`if let
Some(..)`-skip on evidence/lifecycle state is a review reject); counters with two
roles are split behind a `charge(FailureClass) -> Decision` API with per-class caps;
error variants acquiring a second meaning are split, with the wire code DERIVED from
the variant. (c) Code that strips/erases evidence must call or reference the restoring
function — an unimplemented promise must be a dangling symbol, not prose. (d) Replacing
an implicit bound with an explicit one requires a written dominance argument over all
input classes, reviewed with the diff.

## R2 — Multi-site invariants live at chokepoints, not call-site discipline
One constructor per invariant-bearing collection (the predicate goes INSIDE);
consumer-side enforcement over caller discipline (a consumer accepting a pre-built raw
map trusts every future caller); registries iterated by every enumerator, with a CI
conformance check that new members must register. A hand-sweep is presumed incomplete
unless its site list was generated mechanically (rg the TYPE/FIELD, not prose) and
pasted into the commit message. Newtypes are the strongest form: they turn the missed
Nth site into a compile error in the next refactor.

## R3 — A race/authorization fix must enumerate what remains
Mandatory commit-message section RESIDUAL STATES: list the windows/states that remain
and why each is safe; "the race is closed" without the enumeration is a review reject.
Verdicts derive from the authoritative event, never a shadow: corroborate kill reasons
against the wait status; observe-then-flip-then-reap (waitid WNOWAIT); authorize
evidence upgrades by positive witness from the owning authority — NEVER by
absence-of-veto over submitter-controlled sets (spec rule sched.evidence.positive-witness).
A commit asserting "X can never happen" must construct the near-miss window in a test.
Reviewers treat any fix-added test that pins current behavior in the changed region as
a flag: confirm it pins the intended invariant, not the residual bug.

## R4 — Every bound is a typed budget with dominance, progress, and scale calibration
Bounds ship as typed WorkBudget/BoundedQueue values with three reviewed-together
obligations: DOMINANCE — the charge function covers every cost of the loop body (each
await, each push, retained bytes including size_of::<T>()); structurally, metered ops
take &mut budget so unmetered work is unwritable. PROGRESS — every attempt persists
what it proved (monotone convergence), or exhaustion is a TYPED terminal outcome routed
to poison/strip/RESOURCE_EXHAUSTED-with-instructions, never transient retry; an
uncapped retry arm on a deterministic input must not compile under charge(). SCALE —
a CI test at the measured real-world shape (≥2k-drv closure, chain depth ≥ the measured
236 class, adversarial zero-cost floods) counting OPS not wall-clock, plus a
budget-consumed histogram landing with the const.

## R5 — New gates ship a population × producer audit
Any new MUST/fail-closed check lands with: (a) every object class that can reach the
gate (upload kinds, lifecycle states incl. re-upload/already-complete, evidence ranks,
configs incl. dev/helm defaults); (b) every existing producer whose output now flows
through it (relays, substitution, batch, service tokens, recovery); (c) every
previously-irrelevant property that becomes load-bearing (ordering, chunking,
residency, timing). Each cell gets an explicit per-class decision or a typed exclusion —
preferably an enum at the boundary so the per-arm decision is compile-forced.

## R6 — Prose is coupled to mechanism; parity claims are evidence-backed
Client-facing remediation/permanence text is GENERATED from the gate's outcome arm.
"X can never happen", "parity", and "self-heals via Y" claims require the witnessing
artifact in the same commit (adversarial window test, oracle-derived corpus entry, or
the function Y existing — grep every symbol a comment names as the mechanism).
Oracle-parity claims are screened against the PINNED 2.34.7 SOURCE (rg the source tree
in the nix store) before any suggested fix is considered; every deliberate divergence
registers as a tracey rule (nix.divergence.* or component equivalent) carrying either
an oracle-derived pin or an explicitly labeled oracle-unproducible adversarial pin plus
the oracle-producible sibling. Deleting an implementation migrates its comment-borne
invariants into the replacement's types/tests BEFORE the delete lands.
