#import "/lib/rio.typ": *
#show: rio.with(domains: none)


GC mark-and-sweep deletes paths with no reachable references. Before enabling GC on a cluster, verify reference data is correct.

= Prerequisites (must be true before enabling GC)

+ *Builder version*: All builders running a version with the @nar reference scanner (commit `9165dc23` or later). Check: `kubectl get pods -l app=rio-builder -o jsonpath='{.items[*].spec.containers[*].image}'`

+ *Backfill complete*: All paths uploaded before the scanner fix have been re-scanned. Check: `SELECT COUNT(*) FROM narinfo WHERE refs_backfilled = false` should be 0.

+ *Empty-ref sanity check*: `SELECT COUNT(*) * 100.0 / (SELECT COUNT(*) FROM narinfo) FROM narinfo WHERE cardinality("references") = 0 AND content_address IS NULL` --- should be \<5%. Higher means backfill incomplete or a new bug.

+ *GC dry-run*: `rio-cli `#(refs.cli-sub)("gc")` --dry-run` --- review what would be deleted. Spot-check a few paths: are they actually unreferenced?

= Enabling

+ Start with conservative grace period: `rio-cli `#(refs.cli-sub)("gc")` --grace-hours 168` (1 week)
+ Monitor #(refs.metric)("rio_store_gc_path_swept_total") and
  #(refs.metric)("rio_store_s3_deletes_pending")
+ If no issues after first run, reduce grace to desired value

= Rollback

If GC deleted something it shouldn't have:

+ Pause the S3 drain job (#gls("narinfo")/manifest rows CASCADE-deleted but chunks survive in `pending_s3_deletes`)
+ `SELECT * FROM pending_s3_deletes WHERE created_at > $gc_run_time` --- these chunks can be restored
+ See #cross-link("/spec/components/store.typ")[Store §GC] for chunk restore procedure

= Chunk collector (lazy mark-and-collect)

The chunk collector derives chunk GC-eligibility from the durable
manifests (every existing `manifest_data.chunk_list`, any status) at
collect time, instead of trusting the maintained `chunks.refcount`
counter. It runs as phase 3 of every GC run and from a daily backstop
timer. Each store replica arms its own backstop timer, the first tick
fires one full interval after the pod starts (a pod boot, scale-up, or
crash-loop never triggers a cycle), and a tick that finds the GC
advisory lock held --- a GC run or another replica's cycle in flight
--- skips, so at most one cycle runs cluster-wide at a time and a
replica skipping its tick is expected, not a stall. Each live cycle
computes the live set, reports
#(refs.metric)("rio_store_gc_chunks_live"), then
soft-deletes and enqueues unreferenced chunks past grace --- at most
`COLLECT_CYCLE_VICTIM_CAP` per cycle, with a keyset cursor carrying
any remainder to the next cycle, so a backlog (the first enable's
historical-leak reclamation, a mass deletion, a collector outage)
drains across cycles instead of stretching one cycle past its
lock-held budget. Watch the drain through
#(refs.metric)("rio_store_gc_collect_backlog_chunks") (a decremental
estimate, re-anchored at zero when a pass completes),
#(refs.metric)("rio_store_gc_chunks_collected_total"), the
capped-cycles counter
#(refs.metric)("rio_store_gc_collect_cycles_capped_total"), and the
cycle-duration histogram.
#(refs.metric)("rio_store_gc_chunks_would_collect") is emitted only by
shadow (report-only) cycles --- a dry-run GC's phase 3 --- which also
re-anchor the backlog estimate; live cycles do not re-run that full
anti-join count. A dry-run GC keeps phase 3 observation-only (nothing
is soft-deleted or enqueued). The cycle's validation pass,
mark, and report all run on one REPEATABLE READ snapshot, so uploads
or rollbacks that commit while a cycle is running cannot skew its
report --- every count is taken against the snapshot the verdict was
computed on. (The Release-A-stage binaries additionally computed a
refcount drift pair on that same snapshot; Release B retired the
counter writers and the pair with them, so current binaries no longer
emit it --- see the signal-lifetime note in the checklist below.)

== Parse-failure abort: #(refs.alert)("RioStoreGcCollectParseFailure") (critical)

The mark phase is fail-closed: if any existing manifest's `chunk_list`
fails validation (version byte, 36-byte entry alignment, chunk-count
bound), the cycle aborts before computing anything, increments
#(refs.metric)("rio_store_gc_collect_parse_failures_total"), and logs
the offending `store_path_hash` values at error level. While this
persists *all chunk collection is suspended* --- every GC run's phase 3
and every backstop cycle will keep aborting --- but path GC is
unaffected and nothing is deleted (retention-erring by design).

*Find the offending manifest(s):* the store error log line
(`chunk-collect: unparseable chunk_list`) carries the hex
`store_path_hash` values; cross-check in PostgreSQL with
`SELECT octet_length(chunk_list) FROM manifest_data WHERE store_path_hash = decode('<hex>', 'hex')`.

*Remediation ladder:*

+ *Parser/validator regression* (the manifest is valid, the validator
  is wrong --- e.g. after a manifest-format change): fix the validator
  and redeploy rio-store; collection resumes on the next cycle.
+ *Real data damage* (the blob is genuinely corrupt): repair the row if
  the original manifest can be reconstructed, or delete/quarantine the
  manifest (and its narinfo path) so the next mark no longer sees it
  --- condemning the path lets path GC remove it. Collection resumes
  within one cycle of the offending manifest disappearing.

Escalate if the alert re-fires after remediation: repeated aborts with
*different* `store_path_hash` values suggest active corruption (storage
or write path), not a one-off.

== Stalled cycles: #(refs.alert)("RioStoreGcCollectStalled") (warning)

No successful collect cycle (a capped cycle counts as success) on *any*
store replica for more than 25 hours --- the daily backstop plus slack.
The expression is aggregated (`sum`) across replicas: each replica arms
its own daily backstop and a replica that skips its tick because
another replica or a GC run holds the GC advisory lock --- or that
simply never personally wins a cycle --- is expected behaviour, not a
stall. The 30-minute `for:` keeps a freshly rolled-out store from
paging while its first cycle is still pending. Causes, in rough order
of likelihood: the store has not run GC and the backstop task is not
firing (check store logs for `gc-collect-backstop`), every cycle is
aborting on a parse failure (the parse-failure alert should also be
firing), or cycles are erroring against PostgreSQL (check the
`outcome="error"` rate of
#(refs.metric)("rio_store_gc_collect_cycles_total") and the store
error logs). The collector holds the GC advisory lock for the duration
of a cycle, so a wedged GC run also blocks backstop cycles.

== Slow upgrade transactions: #(refs.alert)("RioStoreChunkUpgradeTxSlow") (warning / critical)

#(refs.metric)("rio_store_chunk_upgrade_tx_seconds") measures every
*committed* chunked-upgrade transaction (begin to commit; an aborted
upgrade commits no manifest and is not recorded) --- the single
transaction that makes chunks referenced. The collector's soundness
argument assumes no such transaction outlives the collect grace
window: a manifest that commits after a cycle's mark snapshot is
protected by its own upsert touch only if its transaction is shorter
than grace. The two severities measure different things: *warning*
fires when the p99 over 15 minutes exceeds half the grace window
(150 s) --- the margin is eroding as a trend; *critical* fires when at
least one committed transaction exceeded grace minus 60 s (240 s) in
the last 15 minutes, counted exactly from the 240 s histogram bucket
--- the assumption is at the edge of violation regardless of how much
upload volume surrounds the one slow transaction. A transaction that
is still open is invisible to the histogram (and to both alert arms)
until it commits; the `pg_stat_activity` query below is the only live
view of an in-flight hang.

If it fires: find the long transactions
(`SELECT now() - xact_start, query FROM pg_stat_activity WHERE state <> 'idle' ORDER BY xact_start LIMIT 10`)
and address the cause --- PostgreSQL contention or I/O stalls on the
store, or pathologically large chunked uploads. If long upgrade
transactions are legitimate for the deployment, raise the chunk grace
window (and re-derive the collect predicate's headroom) before
enabling or keeping the live collect arm; adding a
`statement_timeout` on the upgrade path is the enforcement
alternative the design names but does not take by default.

== Refcount-cutover deployment validation checklist (rows D0--D7)

The chunk collector and the retirement of the legacy refcount readers
were developed and verified without a long-lived cluster (code review,
formal models, and the test suite are the development-time evidence).
The observations that would normally have come from a staged
soak/canary are collected here as an explicit checklist, executed when
the completed workstream is eventually deployed. Run the rows in
order; every FAIL is a stop-and-report to the rollout owner --- the
lever column is the sanctioned response, never a silent retune.

*Signal lifetime, up front:* the refcount drift pair (the
leak-direction and under-count-direction gauges; their exact names are
in the Release-A-stage image's own metric catalog and runbook) is
emitted only by the additive/Release-A-stage binaries: it is valid
from the first deployment of the collector through the pre-Release-B
observation window and is retired at Release B (the release that
deletes the counter writers --- the current tree). After that the
counter is no longer maintained and the pair stops being emitted,
which is also why the pair is not listed here as a validated metric
reference. Train dashboards and alert expectations on that lifetime
from the start.

*Unexplained drift (the stop-the-rollout definition):* any nonzero
under-count reading (a chunk referenced by an existing manifest while
its refcount reads zero) at any point while the increment still fires;
or leak-direction growth that cannot be attributed to (a) the known
historical leak classes (corrupt-`chunk_list` decrement skips, crashed
uploads never reclaimed) or (b) post-cutover stopped-decrement
artifacts (the path sweep no longer decrements, so swept paths'
chunks keep their counts until Release B). Anything else is
unexplained --- stop before the Release-B stage and investigate.

*Staged order (D0).* Deploy the additive/Release-A tree first
(migration 068, the upsert touch, the live collector); only after rows
D1--D5 pass deploy the Release-B tree (069 + writer deletion); only
after rows D6--D7 pass apply migration 070 (the column drop). The
Release-B image must not contain 070. On a fresh fleet/database with
no pre-existing pods the additive→A→B stages may be collapsed at the
operator's discretion, but 070-after-B-rollout-complete always
applies, and rows D1--D6 are still executed (they validate the
collector against real data, not against a fleet shape).

+ *D1 --- production-class mark/cycle timing.* Watch
  #(refs.metric)("rio_store_gc_collect_cycle_seconds") over the first
  GC runs and backstop cycles (at least 3 GC cycles and 1 backstop
  run) before any destructive stage proceeds. Pass: cycle duration
  within the five-minute-class lock-held budget at current scale,
  consistent with the recorded bench envelope. Lever: lower
  `COLLECT_CYCLE_VICTIM_CAP` (one-line, measurement-justified edit),
  record an explicit cadence/lock-budget relaxation (e.g.
  backstop-only collect), exploit PostgreSQL parallel-query headroom;
  the junction-table fallback remains the named escalation. Owner
  adjudication, never silent.
+ *D2 --- drift-gauge observation window.* Watch the drift pair
  (emitted by the additive/Release-A-stage image; see the
  signal-lifetime note) from the first collector deployment through
  the pre-Release-B window (suggested: at least 14 days, covering at
  least one backstop run and every GC invocation in it). Pass: zero
  unexplained drift per the definition above. Lever: any under-count
  occurrence stops the staged rollout before Release B (the writers
  must not be deleted); unexplained leak growth is triaged before
  proceeding.
+ *D3 --- alert quietness.*
  #(refs.alert)("RioStoreGcCollectParseFailure"),
  #(refs.alert)("RioStoreGcCollectStalled"), and
  #(refs.alert)("RioStoreChunkUpgradeTxSlow") over the same window,
  plus the parse-failure counter and the upgrade-tx histogram. Pass:
  all three quiet, or every firing triaged (parse failures zero or
  each one a real corrupt blob; upgrade-tx p99 comfortably under
  half the grace window). Lever: the remediation ladders earlier in
  this page; for upgrade-tx, consider the statement-timeout
  enforcement alternative.
+ *D4 --- drain/backlog observation (the one-time reclamation).* Watch
  #(refs.metric)("rio_store_gc_collect_backlog_chunks"),
  #(refs.metric)("rio_store_gc_collect_cycles_capped_total"),
  #(refs.metric)("rio_store_gc_chunks_collected_total"), and per-cycle
  durations after the live cutover. Pass: the backlog gauge trends to
  zero over `ceil(backlog / cap)` cycles, the capped-cycles counter
  goes quiet once the drain completes, durations stay within budget.
  Lever: trigger additional GC runs to accelerate the drain;
  persistent budget breaches route to D1's levers.
+ *D5 --- integrity spot-checks.* `GetPath` error rate on chunked
  paths, a `VerifyChunks` admin scan, and the `i201-stranded-chunks`
  probe. Pass: no DataLoss/NotFound regressions; VerifyChunks and i201
  clean. Lever: treat as a data-loss-relevant signal --- STOP,
  operationally roll back to the previous stage's binaries (additive
  or Release A, both recorded safe), investigate before resuming.
+ *D6 --- Release-B post-rollout watch.* For 2--3 days after the
  Release-B stage: upload error rate, collector metrics against the
  Release-A baselines, drift-pair emission (B pods no longer emit it;
  an A-pod cycle during the rollout may emit a non-zero under-count
  --- expected, not a stop signal), `GetPath`. Pass: no
  `Aborted`/CHECK errors from remaining A pods, collector metrics
  steady, no `GetPath` regressions. Lever: redeploy Release-A binaries
  (recorded safe: nothing names the dropped CHECK/index; the column
  still exists until 070); investigate before re-attempting.
+ *D7 --- migration 070 precondition.* Confirm the Release-B rollout
  is complete on every environment sharing the database (no Release-A
  store pod remains) before applying 070 (the `chunks.refcount` column
  drop). Pass: rollout completion confirmed, then 070 applied. Lever:
  wait; never bundle 070 into the Release-B image; if an A pod must
  persist, leave 070 unapplied (harmless --- nothing reads the
  column).

*Release B go/no-go template.* Proceed to the Release-B stage only
when: D1 within budget (or an explicit recorded relaxation); D2 zero
unexplained drift over the full window; D3 quiet or fully triaged; D4
drain complete or visibly converging within budget; D5 clean. Record
the verdict (date, window, dashboard links, residual anomalies and
their dispositions) alongside the deployment notes; silence is not
acceptance.
