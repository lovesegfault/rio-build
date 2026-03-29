# Plan 483: substitute — stale `'uploading'` manifest blocks subsequent substitution

The **final rsb blocker**: an interrupted `try_substitute` leaves an incomplete narinfo + manifest with `status='uploading'`, and the top-down pre-check (from [P0481](../../.claude/dag.jsonl)) then fails **silently** on every subsequent request for the same path. The orphan scanner at [`gc/orphan.rs`](../../rio-store/src/gc/orphan.rs) reclaims these, but with `STALE_THRESHOLD = 2h` — an interrupted rsb run means **2 hours** of dead substitution for that path. Once manually cleaned, rsb completed in 6.7s (top-down pruned 950/951 derivations).

The failure sequence, at [`substitute.rs:462-472`](../../rio-store/src/substitute.rs):

1. `do_substitute` calls `insert_manifest_uploading` → placeholder row
2. Fetch/ingest crashes (network, OOM, process termination) before the `delete_manifest_uploading` error-path fires
3. Next `try_substitute` for same path: `insert_manifest_uploading` returns `inserted=false` ("concurrent uploader holds it") → returns miss
4. Top-down pre-check sees miss → path treated as must-build → but it's cache.nixos.org-available
5. Repeats for 2h until orphan scan reclaims

Discovered via coverage-sink; `discovered_from=473` (the eager-fetch concurrency bound — interruption became more likely under bounded concurrency since a single slow fetch holds a permit longer).

## Entry criteria

- [P0473](../../.claude/dag.jsonl) merged (scheduler bounds substitute eager-fetch concurrency — this is what made interruption observable)

## Tasks

### T1 — `fix(store):` try_substitute detects + reclaims stale uploading placeholder

MODIFY [`rio-store/src/substitute.rs`](../../rio-store/src/substitute.rs) in `do_substitute` around the `insert_manifest_uploading` call (currently ~line 462). When `inserted=false`, check the placeholder's age:

```rust
let inserted = metadata::insert_manifest_uploading(&self.pool, &store_path_hash).await?;
if !inserted {
    // Another uploader holds the placeholder, OR a crashed uploader
    // left it. Check age: if older than substitute-stale threshold,
    // reclaim and retry. The orphan scanner would catch this in 2h;
    // we can't wait that long on the hot path.
    // r[impl store.substitute.stale-reclaim]
    let age = metadata::manifest_uploading_age(&self.pool, &store_path_hash).await?;
    if age > SUBSTITUTE_STALE_THRESHOLD {
        warn!(?store_path_hash, ?age, "stale 'uploading' placeholder — reclaiming");
        metadata::delete_manifest_uploading(&self.pool, &store_path_hash).await?;
        metrics::counter!("rio_store_substitute_stale_reclaimed_total").increment(1);
        // Retry the insert. If THIS also fails, a live concurrent
        // uploader really is active — fall through to miss.
        if !metadata::insert_manifest_uploading(&self.pool, &store_path_hash).await? {
            return Ok(None);
        }
    } else {
        // Young placeholder — genuine concurrent upload. Miss + next try picks it up.
        return Ok(None);
    }
}
```

`SUBSTITUTE_STALE_THRESHOLD` default **5 minutes** — long enough that a real concurrent substitution (even multi-GB NAR over slow link) finishes, short enough that an rsb retry loop doesn't wait 2h. Configurable via `store.toml` `[substitute] stale_threshold_secs`.

Add `metadata::manifest_uploading_age(pool, hash) -> Result<Option<Duration>>` helper in [`rio-store/src/metadata/`](../../rio-store/src/metadata/) — `SELECT now() - created_at FROM manifests WHERE store_path_hash = $1 AND status = 'uploading'`.

### T2 — `fix(store):` orphan scanner — tighten STALE_THRESHOLD under substitution load

MODIFY [`rio-store/src/gc/orphan.rs`](../../rio-store/src/gc/orphan.rs). `STALE_THRESHOLD = 2h` is tuned for PutPath crash recovery (rare). Under substitution load, interrupted fetches are common (network blips, pod rollouts). Options:

- **Preferred:** T1 makes this moot for the hot path. Leave orphan scanner as the safety-net sweep.
- **Belt-and-braces:** drop `STALE_THRESHOLD` from 2h → 15min. A real PutPath upload that takes >15min is pathological (multi-GB over <10Mbps); the orphan scanner reaping it mid-flight means the uploader's `delete_manifest_uploading` error-path fires on a now-absent row — harmless (`DELETE ... WHERE` affects 0 rows).

Implement belt-and-braces: `STALE_THRESHOLD = 15min`, document the trade-off in the doc-comment (was 2h → 15min because substitution made stale placeholders a hot-path blocker, not just a GC leak).

### T3 — `test(store):` stale-uploading reclaim — interrupted then retry succeeds

NEW test in [`rio-store/src/substitute.rs`](../../rio-store/src/substitute.rs) `#[cfg(test)]` mod (alongside existing `try_substitute` tests at ~line 810+):

```rust
// r[verify store.substitute.stale-reclaim]
#[tokio::test]
async fn try_substitute_reclaims_stale_uploading() {
    // 1. Manually insert an 'uploading' placeholder with created_at = 10min ago
    // 2. Call try_substitute for the same path
    // 3. Assert: returns Some(ValidatedPathInfo) — reclaim fired, re-insert succeeded, fetch completed
    // 4. Assert: rio_store_substitute_stale_reclaimed_total incremented
}

#[tokio::test]
async fn try_substitute_respects_young_uploading() {
    // 1. Insert 'uploading' placeholder with created_at = now()
    // 2. Call try_substitute
    // 3. Assert: returns None — young placeholder = real concurrent uploader, no reclaim
}
```

### T4 — `docs(observability):` add rio_store_substitute_stale_reclaimed_total metric

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add the counter row to the store metrics table. Name follows `rio_{component}_` convention.

## Exit criteria

- `cargo nextest run -p rio-store try_substitute_reclaims_stale_uploading try_substitute_respects_young_uploading` → 2 passed
- `grep 'SUBSTITUTE_STALE_THRESHOLD\|stale_threshold_secs' rio-store/src/substitute.rs` → ≥2 hits (const + config-wired)
- `grep 'rio_store_substitute_stale_reclaimed_total' rio-store/src/substitute.rs docs/src/observability.md` → ≥2 hits (emit + documented)
- `grep '15.*60\|Duration::from_secs(15' rio-store/src/gc/orphan.rs` — STALE_THRESHOLD tightened (T2)
- Manual rsb smoke: stop `rio-store` mid-substitution, restart, retry within 5min → substitution completes (not blocked 2h)

## Tracey

References existing markers:
- `r[store.substitute.upstream]` — T1 hardens the ingest path described here
- `r[store.put.wal-manifest]` — the `'uploading'` placeholder mechanism T1 reclaims

Adds new markers to component specs:
- `r[store.substitute.stale-reclaim]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) (see Spec additions below)

## Spec additions

Add to [`docs/src/components/store.md`](../../docs/src/components/store.md) after `r[store.substitute.tenant-sig-visibility]` (currently line 225):

```
r[store.substitute.stale-reclaim]
When `try_substitute` finds an existing `'uploading'` placeholder for the requested path, it MUST check the placeholder's age. If older than `stale_threshold_secs` (default 5 minutes), the substituter reclaims the placeholder (DELETE + re-INSERT) and proceeds with the fetch. A young placeholder indicates a live concurrent uploader and returns a miss. This prevents a crashed substitution from blocking the path for the full orphan-scanner interval (15 minutes). The `rio_store_substitute_stale_reclaimed_total` counter tracks reclaim events.
```

## Files

```json files
[
  {"path": "rio-store/src/substitute.rs", "action": "MODIFY", "note": "T1: stale-reclaim in do_substitute; T3: two new tests"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T1: manifest_uploading_age helper"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "T2: STALE_THRESHOLD 2h->15min + doc-comment"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T1: r[store.substitute.stale-reclaim] marker"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: stale_reclaimed_total counter row"}
]
```

```
rio-store/src/
├── substitute.rs          # T1: stale-reclaim branch; T3: 2 tests
├── metadata/mod.rs        # T1: manifest_uploading_age helper
└── gc/orphan.rs           # T2: STALE_THRESHOLD 2h→15min
docs/src/
├── components/store.md    # r[store.substitute.stale-reclaim]
└── observability.md       # T4: metric row
```

## Dependencies

```json deps
{"deps": [473], "soft_deps": [481], "note": "473 (eager-fetch concurrency bound) made interruption observable under load. 481 (top-down pre-check) is what fails silently when the stale placeholder exists — soft-dep because the stale-reclaim logic here works with or without the pre-check, but the rsb failure mode it fixes only manifests post-481."}
```

**Depends on:** P0473 (ad-hoc rsb fix, merged [`ceaebfd6`](https://github.com/search?q=ceaebfd6&type=commits)) — bounded eager-fetch concurrency made slow-fetch interruptions observable.
**Soft-dep:** P0481 (top-down pre-check, merged [`76696d55`](https://github.com/search?q=76696d55&type=commits)) — the pre-check is what silently fails on the stale placeholder.
**Conflicts with:** [`substitute.rs`](../../rio-store/src/substitute.rs) — no open plans. [`gc/orphan.rs`](../../rio-store/src/gc/orphan.rs) — low traffic.
