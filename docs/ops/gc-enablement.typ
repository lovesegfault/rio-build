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
