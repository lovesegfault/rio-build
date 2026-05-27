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
replica skipping its tick is expected, not a stall. The collector
ships in *shadow mode* first: each cycle computes the
live set and reports
#(refs.metric)("rio_store_gc_chunks_live"),
#(refs.metric)("rio_store_gc_chunks_would_collect"),
the refcount drift pair
(#(refs.metric)("rio_store_gc_refcount_drift_leaked"),
#(refs.metric)("rio_store_gc_refcount_drift_undercount")),
#(refs.metric)("rio_store_gc_collect_backlog_chunks") and a
cycle-duration histogram, but modifies nothing. A later release turns
on the collecting arm (soft-delete + S3-delete enqueue), capped per
cycle so a backlog drains across cycles. The cycle's validation pass,
mark, and report all run on one REPEATABLE READ snapshot, so the drift
gauges measure real refcount drift --- uploads or rollbacks that commit
while a cycle is running cannot show up as drift, and a nonzero
under-count reading is a real abort signal, not cycle-concurrent
traffic.

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
