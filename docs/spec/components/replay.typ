#import "/lib/rio.typ": *
#show: rio.with(domains: ("replay",))


Build-replay campaign engine (`rio-replay`). The full subsystem design ---
archive format v1, recorders, the campaign engine's stages, verdict and
disposition vocabularies, supply planning, scheduling modes --- lives in the
build-replay design document (`docs/dev/2026-05-28-build-replay-design.md`);
this chapter carries only the normative requirements whose regressions ship
operator-facing false alarms: the canary-probe ladder of the infra-rate
backpressure pause, and the supply stage's gateway-collapse breaker.

= Infra-rate pause and the canary-probe ladder

The engine pauses new submissions when the rolling infra-indeterminate rate
over recent terminal records crosses `knobs.infra_pause_pct` (timeless mode;
timed campaigns get an abort recommendation instead, because pausing would
distort the recorded cadence they exist to measure).

The infra-rate pause is the one backpressure source whose evidence the pause
itself suppresses: the rolling window is computed over terminal records, and
a paused loop produces none --- once in-flight work drains, the window would
freeze over threshold and re-assert forever. The canary-probe ladder is the
bounded middle between that wedge and releasing the full wave into a live
outage.

#r("replay.probe.single-job")[
  While the infra-rate pause holds with nothing in flight, the engine MUST
  release at most ONE single-job canary-probe batch per probe cycle --- never
  a full wave --- and an infra-shaped probe failure MUST be budget-exempt: no
  auto-retry budget consumed, no requeue journal entry, no terminal record
  for the conscripted job.
]

A probe that consumed budgets or retired its job would convert a recoverable
outage into mass terminal retirement, one unit per cycle; the carve-out is
what makes the probe a sacrificial evidence vehicle instead.

#r("replay.probe.work-evidence")[
  A probe cycle MUST score success only when the probed job holds a
  WORK-EVIDENCING terminal record --- a class the cluster could only have
  produced by resolving the unit end to end (built, genuine failure, source
  rot, an in-band target substitution); bare terminality is NOT the witness,
  because outage-minted classes (an infra-indeterminate exhaustion terminal,
  a supply-failed exclusion) are terminal records the outage itself produces
  on the probe job.
]

Both misreadings of this rule have shipped false operator signals: scoring
outage-minted terminals as success lets the outage retire one workload unit
per cycle with the operator escalation never firing, and refusing to score a
genuine in-band target substitution (a healthy cluster answering the probe
from a warm upstream) walked a healthy campaign into the operator pause.

#r("replay.probe.escalate-pause")[
  After `INFRA_PROBE_PAUSE_AFTER` consecutive failed probe cycles the engine
  MUST escalate by writing the operator `PAUSE` file instead of probing
  further; removing the `PAUSE` file MUST reset the ladder so probing
  restarts against the presumably-repaired infrastructure, and a successful
  probe MUST reset the consecutive-failure count.
]

Each failed cycle costs one budget-exempt single-job batch, so the
escalation bound is also the cap on what an outage can spend before a human
must look.

= Supply-stage breaker neutrality

The prewarm upload arms share a run-wide gateway circuit breaker:
consecutive transport failures above the collapse threshold (`max(2 ×
knobs.upload_workers, 6)`) mean the gateway is gone, remaining uploads are
recorded `skipped` without further transport calls, and the campaign pauses
for the operator ("supply upload collapse") before execution starts.

#r("replay.supply.relay-payload-neutral")[
  A relay payload-source failure --- the relay's byte stream feeding a
  streamed upload dying or coming up short mid-body --- MUST settle that
  path's own `failed` supply row and MUST NOT charge (or reset) the gateway
  circuit breaker: per-path relay degradation MUST NOT skip-stamp unrelated
  uploads or pause the campaign against a healthy gateway.
]

The breaker's evidence is channel-only by design: a relay's failure is a
third party's failure, and its wire-op retry re-fetches the same failing
relay, so the interleaved-success self-correction that licenses counting
ambiguous channel deaths can structurally never fire for payload deaths ---
charging them would let one failing relay (16 large relay paths run serially
before the batch lane at default knobs) trip the collapse with no success in
between.
