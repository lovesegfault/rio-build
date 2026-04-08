# GC Enablement Checklist

GC mark-and-sweep deletes paths with no reachable references. Before enabling GC on a cluster, verify reference data is correct.

## Prerequisites (must be true before enabling GC)

1. **Builder version**: All builders running a version with the NAR reference scanner (commit `9165dc23` or later). Check: `kubectl get pods -l app=rio-builder -o jsonpath='{.items[*].spec.containers[*].image}'`

2. **Backfill complete**: All paths uploaded before the scanner fix have been re-scanned. Check: `SELECT COUNT(*) FROM narinfo WHERE refs_backfilled = false` should be 0.

3. **Empty-ref sanity check**: `SELECT COUNT(*) * 100.0 / (SELECT COUNT(*) FROM narinfo) FROM narinfo WHERE cardinality("references") = 0 AND content_address IS NULL` — should be <5%. Higher means backfill incomplete or a new bug.

4. **GC dry-run**: `rio-cli trigger-gc --dry-run` — review what would be deleted. Spot-check a few paths: are they actually unreferenced?

## Enabling

1. Start with conservative grace period: `rio-cli trigger-gc --grace-hours 168` (1 week)
2. Monitor `rio_store_gc_paths_deleted_total` and `rio_store_gc_bytes_freed_total`
3. If no issues after first run, reduce grace to desired value

## Rollback

If GC deleted something it shouldn't have:

1. Pause the S3 drain job (narinfo/manifest rows CASCADE-deleted but chunks survive in `pending_s3_deletes`)
2. `SELECT * FROM pending_s3_deletes WHERE created_at > $gc_run_time` — these chunks can be restored
3. See `docs/src/components/store.md` §GC for chunk restore procedure
