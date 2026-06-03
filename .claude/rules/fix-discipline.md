# Fix discipline — rules distilled from rounds 14–16 fix-genealogy

When a bug is the outcome of a previous bug fix, the fix for THAT bug must name the
parent commit and the pattern number (R1–R7) in its commit message. Provenance is
part of the fix.

Genealogy note: rounds 14-16. Round-16 verdict: rules adopted near-universally,
failed by precision at the axes amended above; the structural complement is the
contract-owner pattern (a remedy introducing a contract introduces, same
commit, the type owning its transitions). If round 17 clusters in the same
remedies AFTER the owner types land, escalate from "rules imprecise" to
"abstraction wrong" (F2-class lifecycle redesign becomes the plan).

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

R1(a) — PROSE READERS. The pasted site list MUST include prose readers: rg the
symbol over the FULL tree (comments, rustdocs, migrations doc-consts, test
headers — no code-only filter) and paste BOTH lists (code consumers; prose
restatements). A definitional restatement found by the sweep is rewritten as a
REFERENCE to the single defining doc, not re-synced in place. [bug_082,
merged_054 — 11 stale sites under bumped markers; manual sweeps found 6-7/11]

R1(c) — CI-GREPPABLE FORM (sharpened). "An unimplemented promise must be a
dangling symbol, not prose" now means: a strip/defer site MUST reference a real
symbol for the restorer (e.g. #[expect(dead_code)] fn + stringify! at the
message site) or carry a tracked F-marker — free prose is a violation.
[merged_038: completion.rs:1263]

R1(d) — STRUCT LIFECYCLE. Adding a field to a struct that has bulk-reset
methods, Default/FRU sites, or persist/restore tiers requires enumerating those
chokepoints (mechanically: rg 'StructName \{|fn clear|fn reset|\.\.Default')
with a per-chokepoint decision (reset / preserve / persist+restore / documented
carve-out), and shipping/extending a dirty-all completeness pin (full struct
literal, no FRU, equality vs default modulo an explicit carve-out list).
Field-name greps are structurally blind to omission sites. [merged_022, bug_100]

R1(e) — PRODUCING-STATEMENT CONSTRUCTORS. Closed-outcome enums are constructed
at producing statements: map_err(Variant) over a composite multi-source call is
a review reject — it re-creates the default bucket the enum exists to
eliminate. [merged_068]

R1(f) — CADENCE AND PRODUCER-SET. Wrapping an emitting function in a
retry/fixpoint loop, or adding a new producer of an existing error/metric
variant, re-audits terminal-vs-per-attempt semantics of everything emitted
inside the new loop body and the variant's documented meaning. [bug_085, bug_027]

R1(g) — POPULATION-SHRINK PROSE. A routing change that shrinks a consequence
arm's population re-opens every cause-enumeration and definitional comment on
BOTH arms. tracey marker bumps validate reference freshness, never statement
truth — a bumped marker atop an unread paragraph is the bug_082/merged_054
signature.

## R2 — Multi-site invariants live at chokepoints, not call-site discipline
One constructor per invariant-bearing collection (the predicate goes INSIDE);
consumer-side enforcement over caller discipline (a consumer accepting a pre-built raw
map trusts every future caller); registries iterated by every enumerator, with a CI
conformance check that new members must register. A hand-sweep is presumed incomplete
unless its site list was generated mechanically (rg the TYPE/FIELD, not prose) and
pasted into the commit message. Newtypes are the strongest form: they turn the missed
Nth site into a compile error in the next refactor.

R2 — AUTHORITATIVE EVENT. The chokepoint sits at the authoritative event the
invariant's own quantifier names (delivered/committed/settled), not the
convenient one. A chokepoint performing a destructive read of shared evidence
(swap/take/drain) upstream of that event MUST carry a typed restore path proven
by a test that kills the carrier between read and settlement. [bug_065]

R2 — CROSS-LANGUAGE TWINS. A cross-language/cross-layer twin of an in-memory
predicate REQUIRES an axis-isolated differential conformance test (identical
fixtures through both sides, one axis varied per case) in the same commit that
creates or changes either side; prose cross-reference is a review reject.
[merged_087]

R2 — NEW WRITE SITES. Adding a write site to a documented collection routes
through the existing cap/init-owning chokepoint or re-sanctions the documented
invariant in the same commit. [merged_026]

R2 — PAIRED WRITERS. Multi-part durable invariants (flag<=>rows,
mark<=>breadcrumb) are written by ONE paired tx helper; two writers with
different population scopes is the round-16 closure-hole signature. [bug_045]

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

R3 — STATE SPACE. RESIDUAL STATES declares its state space as the PRODUCT of
supervised lifecycles (process levels, definition epochs, row lifecycles, the
data's NEXT lifecycle stage), with mandatory rows for concurrent-self,
out-of-domain decode coercions, and one witness per control-flow exit class on
universal claims. [merged_046, merged_015, merged_038, bug_084, bug_073]

R3 — HARNESS FROM PRODUCTION TOPOLOGY. Near-miss harnesses reproduce the
production topology (tree depth, evidence channels) — preferably by factoring
the production loop into a harness-reusable function, never by re-modeling.
[merged_046: spawn_exiting was one level too shallow for three rounds]

R3 — EVIDENCE CARRIERS. When corroborating evidence is carried by a supervised
component, kills/failures OF THE CARRIER are mandatory enumeration axes: a kill
must not be able to manufacture its own corroboration. [merged_046]

R3 — CONCURRENCY-WITH-SELF. Fixes touching multi-write external state include
an explicit concurrency-with-self row: safe-by-lock / safe-by-idempotence /
safe-by-ordering — exactly one, with evidence (lock object, idempotence
argument, or ordering proof + near-miss test). [merged_015]

R3 — REVIEWER FLAG TEETH. The commit names the population cells its fix-added
tests do NOT cover, so "pins the residual" is visible at review. [065/046/023/004]

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

R4 — UNIT OF ACCOUNT. Live budget instances per spec-named accounting unit are
part of the dominance argument: charges within one spec-named unit share one
ledger, or the const states the worst-case aggregate (Nx) explicitly. [bug_052]

R4 — AGGREGATE/ADMISSION. Any spawn site of budgeted work carries an aggregate
clause (semaphore/singleflight/negative-memo named; aggregate bound = permits ×
per-instance cap stated). [bug_080, bug_079]

R4 — SCALE PER DIMENSION. One adversarial scale test PER cost dimension that
DOMINANCE names, including BYTE-shaped floods (padded payloads at every
retention site). [bug_079]

R4 — EXIT CLASSES. Universal claims quantified over control-flow exits ("every
exit persists", "always drains") enumerate the exit classes (typed verdict, Err
propagation, panic/cancel where relevant) with one witness each; structurally,
route exits through owner-type finish()/fail() so a new exit class cannot
bypass the obligation. [bug_084, merged_086]

## R5 — New gates ship a population × producer audit
Any new MUST/fail-closed check lands with: (a) every object class that can reach the
gate (upload kinds, lifecycle states incl. re-upload/already-complete, evidence ranks,
configs incl. dev/helm defaults); (b) every existing producer whose output now flows
through it (relays, substitution, batch, service tokens, recovery); (c) every
previously-irrelevant property that becomes load-bearing (ordering, chunking,
residency, timing). Each cell gets an explicit per-class decision or a typed exclusion —
preferably an enum at the boundary so the per-arm decision is compile-forced.

R5 — TRIGGERS. The audit applies to permanence/consequence RE-ROUTES of
existing verdicts, codification of implicit policies into registries/tables,
and consequence-arm population shrink — not only "new gates". A permanence
claim additionally names which premises are system-mutated (reap, recovery,
displacement, GC, dispatch overwrite) and why each mutation cannot occur or is
covered. [bug_029, bug_069]

R5 — CONSUMER-DEFINED POPULATIONS. The gate population is defined by the
CONSUMING contract's accept-set (the parser of record, cross-crate where
necessary), and the source is named in the pasted audit. [bug_023]

R5 — DECISION SURFACES. Decision surfaces are enumerated as victims ×
incomings × lifecycle MATCH ARMS; continue/skip-chain `if`s on evidence or
lifecycle state at decision surfaces are a structural reject (enforcing the
existing R1(b) clause). [bug_072]

R5 — WRITER SIDE. For any multi-part durable invariant the audit enumerates
WRITER sites per part. [bug_045]

R5 — SIBLING INVARIANTS. Sweep sibling invariants per CONSUMER (every
assumption the same consumers make of the same data — e.g. distinctness AND
arity), not per invariant across surfaces. [bug_098]

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

R6 — PRODUCER FOLLOWABILITY. Generated remediation must be executable by the
production producers of the refused population (the gateway's actual submission
shapes), verified against the producer's unconditional behavior. [merged_020,
merged_038]

R6 — CAPABILITY WITNESS. Spec prose asserting an operator-facing capability in
the present tense requires, same commit, EITHER a wired-in-production witness
(the producing knob and delivery path named; r[impl] on the producer) OR an
explicit not-yet-wired marker set: a deliberately-uncovered #r delivery rule
(visible in `tracey query uncovered`), a "not yet a config surface" note at the
hardcoded None/default site, and — where a frozen config schema exists — a
schema tripwire test whose failure message is the wiring checklist. Findings
strictly inside an unwired channel cap at latent-correctness; an unreachable
subsystem gets a wire-or-gate disposition within one round. [merged_057]

R6 — WITNESS FIDELITY. Byte-format pins use byte-exact comparators (cmp, never
POSIX command-substitution equality); mock producers are format-faithful in the
pinned dimension; every client-facing COUNT/diagnostic field is asserted equal
to structural ground truth in at least one fixture; mirrored hand-maintained
tables get a set-equality chokepoint assert. [merged_004, merged_086, merged_055]

R6 — PARITY SWEEP. Touching a function/file whose docs or markers claim oracle
parity obligates a sweep of the WHOLE claimed-parity unit against the pinned
2.34.7 source (two-layer when the oracle delegates, e.g. libcurl) — every
behavioral delta (errno arms, case folding, set membership, prefix handling,
error class) matched exactly or registered. The commit message carries a
PARITY-SWEEP section: unit, oracle file:lines, per-delta disposition. The
boundary is the unit claiming parity, never the axis being fixed. [bug_101,
merged_048, bug_024]

R6 — SPANNING COMPENSATIONS. Compensating-mechanism claims ("re-raised by",
"covered by Y", "self-heals via") require the named mechanism greppable on
EVERY path the claim spans, and "covered by Y" names the GATE Y governs with a
witness exercising THAT gate. [parse_lossy specimen, bug_053]

## R7 — Bidirectional / composition audit
(a) A fix that changes what a value MEANS at a decision point audits every
    PRODUCER and default of that value — parse/decode fallbacks, Default impls,
    recovery re-raises — including their doc-claimed compensations; a
    floor/ceiling chosen for one role is re-derived for every role the value
    now plays. [bug_073]
(b) A fix that newly DEPENDS on a property of existing state (field shape,
    residency, ordering, row absence) audits every WRITER of that property in
    the same change — the dual of R1's reader audit. [bug_094, bug_029]
(c) Same-push sibling fixes CROSS-AUDIT: each names the populations the other
    mints and shows its own gates have a cell for them, walking one lifecycle
    stage forward (settled-row resubmission, post-failover rehydration) — the
    composition cell lives at the data's NEXT stage, not the commit's surface.
    [merged_038 — the canonical case; bug_011, bug_069]
