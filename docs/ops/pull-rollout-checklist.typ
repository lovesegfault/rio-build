#import "/lib/rio.typ": *
#show: rio.with(domains: none)


Deployment-time validation checklist for the pull-mode dispatch rollout
(the executor-lifecycle campaign's operator handoff, relocated from the
retired executor invariant map). The campaign closed with a no-deploy
directive: nothing in it is deployment-validated, and the rows below are
exactly the observations that remain open until a completed workstream
deploys.

*What remains unsigned, pending deployment data:* the OA1 baseline
accumulation and the AD5 numeric cancel/preempt + death→requeue budget
signature (D1); the post-flip latency comparison against that signed budget
(D2); the OA2 live-signal observation and alert triage (D3); the post-flip
watch with its ledger spot-checks (D4); the production rollback drill or its
recorded waiver (D5); the deletion-gate gauge-at-zero evaluation over the
computed horizon (D6); the post-deletion-release watches and the OA5
live-fleet confirmation (D7). None of these gate anything in the development
tree; all of them gate the staged rollout. The VM timing figures recorded at
1b (cancel 9.9 s, preempt 64.1 s, establishment charge at attempt age 307 s)
are VM-topology measurements used to validate the AD5 component structure,
not budgets.

*Knob-retirement annotation (read before executing).* The rows were written
against the staged-rollout tree, whose levers included the Pool
`spec.dispatchMode` knob and the per-pod `RIO_DISPATCH_MODE` discriminator.
That knob was retired end-to-end after the close-out (PR \#46 Track C: the
CRD field, the controller gates and the chart knob are gone;
#(refs.migration)("076_drop_dispatch_mode") dropped
`drv_executions.dispatch_mode`; the builder always pulls). On any
tree at or past that retirement: D0 collapses to its fresh-fleet arm (deploy
with pull as the only mode — there is no template to flip), D5 is satisfied
by its recorded waiver path (the VM demonstration plus the knob retirement
record), and D6 is trivially zero (no stream registrations can exist). Rows
D1--D4 and D7 are still executed in full — they validate the replacement
against real load, not against a fleet shape.

*Deliverables that make the checklist executable* (ship with the code): the
OA1 histogram pair #(refs.metric)("rio_scheduler_attempt_requeue_seconds") (by
cause) / #(refs.metric)("rio_controller_job_terminal_report_seconds") (by
reason); the OA5 surface
(`AdminService.ListOpenAttempts`, the re-pointed `ListExecutors` /
`ClusterStatus`, the #(refs.metric)("rio_scheduler_open_attempts") gauge, the
controller Job census); the OA2 wedge clustering
(#(refs.metric)("rio_controller_node_wedge_marked_total"))
plus the #(refs.alert)("RioSchedulerAttemptEstablishmentCluster") alert and the
#cross-link("/ops/hung-node-manual-reap.typ")[hung-node manual-reap runbook];
the scheduler Config `establishment_report_slack` (env
`establishment_report_slack_secs`, default 120 s); the deletion-gate
recording rules `rio:scheduler_stream_registrations:max` and
`rio:scheduler_stream_attempts:rate5m` (present on pre-deletion-stage
releases only); and the 45 s pull-mode `terminationGracePeriodSeconds` in
the rendered pod template. Stop-and-report discipline applies to every
FAIL: the lever named per row is the sanctioned response, adjudicated by
the owner/operator — never a silent retune of a budget, slack, or gate.

= Ordering preamble (D0)

The staged rollout preserves the slice ordering the code was constructed
for: deploy the pre-deletion tree first (the 1c-stage release — full pull
path present and verified, stream path intact), flip pool templates to Pull
(canary-first at the operator's discretion), and only after rows D1--D6
pass roll out the deletion-stage releases (the 1c'-stage tree, then the
1d-stage tree — kept as separate releases precisely so the template-flip
rollback path exists until the gauge evidence says nothing needs it). On a
fresh fleet with no stream-mode pools ever having run, the coexistence
hazards are vacuous and the operator may collapse the additive/flip stages;
the deletion-stage-after-D6 rule always applies (trivially satisfied if no
stream registrations ever existed), and rows D1--D5 are still executed.

= The rows

== D0 — staged-rollout ordering

- *Run/observe:* pre-deletion (1c-stage) release → per-pool dispatch-mode
  flips (canary-first, preferring a pool whose drvs the remaining stream
  pools rarely take) → 1c'-stage deletion release → 1d-stage cleanup
  release.
- *Pass:* the order above is what is actually executed; no deletion-stage
  image rolls out before rows D1--D6 pass; the 1d release follows a healthy
  1c' watch (D7).
- *Lever:* re-stage; do not proceed out of order — the coexistence hazards
  (busy bridge, mixed-era exclusion keys, template-flip rollback) are
  exactly what the ordering preserves.

== D1 — OA1 baseline + AD5 numeric budget signature

- *Run/observe:* #(refs.metric)("rio_scheduler_attempt_requeue_seconds") (by
  cause) and #(refs.metric)("rio_controller_job_terminal_report_seconds")
  (by reason) over the
  pre-flip window (as-built baseline, if any stream pools run) and the
  post-flip window; sign the AD5 composite cancel/preempt + death→requeue
  budget against that data; re-baseline `establishment_report_slack`
  against the controller report p99.
- *Pass:* the AD5 numeric budget is signed by the campaign owner against
  measured data; the slack value confirmed or re-set from the same
  instrument.
- *Lever:* no usable distribution → extend the observation window before
  flipping further pools; budget cannot be met structurally → lengthen
  `establishment_report_slack` (config-only; degrades requeue latency, not
  correctness) or owner adjudication / design re-entry — never a silent
  number.

== D2 — post-flip latency comparison

- *Run/observe:* death→requeue p50/p99 by cause for pull-mode attempts vs
  the D1 baseline; cancel/preempt observed latencies vs the signed AD5
  budget; per pool as each flips, then fleet-wide.
- *Pass:* latencies within the signed budgets; no regression past what the
  owner signed at D1.
- *Lever:* stop flipping further pools (or flip back — D5's path); lengthen
  the establishment slack; investigate against the T-1b.9 VM component
  evidence; owner adjudication before the deletion stage.

== D3 — OA2 node-wedge signal observation

- *Run/observe:* #(refs.metric)("rio_controller_node_wedge_marked_total") and
  the wedge-clustered Dead-arm reaps for flipped pools; the
  #(refs.alert)("RioSchedulerAttemptEstablishmentCluster") alert + the manual
  NodeClaim-reap runbook over the same window.
- *Pass:* the clustering feeds `reap_unhealthy` as designed; the alert is
  quiet, or every firing triages per the runbook to a real node wedge.
- *Lever:* signal absent/broken → fix before the deletion stage (from the
  1c'-stage on there is no fallback detector); alert noisy → tune the
  ≥2-distinct-derivations / 30-minute clustering thresholds with a recorded
  justification; real wedge → runbook manual reap + AD2 exclusion bounds
  the blast radius.

== D4 — post-flip watch (the former soak)

- *Run/observe:* suggested ≥ 7 calendar days covering ≥ 1 organic
  pod-terminal classification and ≥ 1 preemption/cancel; ledger
  spot-checks: one open attempt per `PullAssignment`, no second attempt row
  per exec id, no establishment inside the deadline+slack window, no state
  created by no-attempt reports; the OA5 view consistent with the Job
  census.
- *Pass:* zero double-charge / fabricated-completion /
  in-window-establishment occurrences; the organic pod-terminal and
  preempt/cancel events classified as second-installment fills, never new
  rows; OA5 matches the census.
- *Lever:* any occurrence: STOP the staged rollout before the deletion
  stage, flip templates back (D5), investigate against the
  model/tests/red-first batteries, owner adjudication.

== D5 — rollback drill

- *Run/observe:* flip one pull-mode pool's template back to stream mode
  once, in production, and observe the next pod register/build/report on
  the stream path (only meaningful while the pre-deletion release is still
  deployed; the VM demonstration is the T-1b.8 record).
- *Pass:* the flip-back works as demonstrated, or the owner explicitly
  records the VM demonstration as the accepted evidence in lieu of a
  production drill.
- *Lever:* fix the template/CR tooling before proceeding; the drill (or its
  waiver) must be recorded before the deletion-stage release removes the
  flip-back path.

== D6 — deletion-gate evaluation

- *Run/observe:* `max_over_time(rio:scheduler_stream_registrations:max[...])`
  `== 0` with the horizon = max(`activeDeadlineSeconds` over live intents)
  + builder `idle_timeout` (default 120 s) + `establishment_report_slack`
  (default 120 s), on every environment sharing the scheduler, evaluated
  against the pre-deletion release (the only release that emits the gauge);
  no pool template references stream mode.
- *Pass:* the gauge reads zero for the full horizon everywhere; on an
  environment already running 1c'-stage code the equivalent read is
  `rio:scheduler_stream_attempts:rate5m == 0` (the stub-call counter); both
  series are absent from the 1d-stage release.
- *Lever:* wait; identify which pools/pods still register stream executors
  and flip or drain them; never roll out the deletion-stage release early —
  the template-flip rollback (D5) ceases to exist once it ships.

== D7 — post-deletion-release watch + OA5 live confirmation

- *Run/observe:* 2--3 days after the 1c' stage and again after the 1d
  stage: scheduler/controller error rates; absence of calls to removed RPCs
  (1c' stage: the stream stub-call counter; 1d stage: gateway
  unknown-method observability and error rates — no dedicated counter
  exists, the recorded accepted shape of the second deletion release's
  watch); latency steady vs D2; `ListOpenAttempts` / the re-pointed
  `ListExecutors`/`ClusterStatus` / the dashboard confirmed against the
  live fleet; the OA2 aggregation still feeding reap decisions.
- *Pass:* no regressions attributable to the deletions; OA5 confirmed live
  by the operator; the 1d release only follows a healthy 1c' watch.
- *Lever:* roll back the deletion-stage release (the previous stage's
  images — recorded safe: the proto surface survives until the 1d stage and
  pool templates are unaffected); investigate before re-attempting; any
  double-charge-class signal routes to D4's stop rule.

= D6/D7 signal-shape note

The `workers_active` gauge, the stream stub-call counter and their
recording rules are gone with the surfaces they read on 1d-era code. Rows
D6/D7 are evaluated against the release being upgraded FROM (which still
emits them); the 1d-stage D7 watch falls back to scheduler error rates and
the gateway's unknown-method observability rather than a dedicated counter.
