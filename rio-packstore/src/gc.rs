//! GC = mark + repack, one mechanism.
//!
//! Runs synchronously under the exclusive `gc.lock` flock, taken (try,
//! never wait) at open time when a trigger fires — no daemon, no
//! background threads (the store lives inside a nix eval process;
//! fork-safety forbids spawning threads at construction).
//!
//! Mark: live blobs are the union of the surviving roots' digests.
//! Size-cap pressure evicts LRU roots first (eviction unit = the
//! store-path root entry; roots used within the in-flight grace window
//! are never evicted), then repack reclaims the bytes.
//!
//! Repack: ONE source segment at a time — copy live records into the
//! consolidated pack, swap the index, unlink the segment, repeat.
//! Transient disk is one segment of headroom, never ~2× live bytes —
//! the all-at-once variant ENOSPCs exactly when the size cap fires and
//! the store can no longer shrink itself (ADR-024).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::index::{self, IndexView};
use crate::segment::{self, Segment};
use crate::{Digest, KIND_ROOT, Options, Result, lock};

/// What the last GC pass did. `peak_pack_bytes` is the observable for
/// the one-segment-headroom invariant: the maximum total size of
/// `packs/` seen at any point during repack.
#[derive(Clone, Debug, Default)]
pub struct GcStats {
    pub roots_evicted: usize,
    pub packs_repacked: usize,
    pub packs_skipped_live_writer: usize,
    pub records_copied: usize,
    pub records_dropped: usize,
    /// Torn tails dropped during repack — legal only here, where we
    /// hold the exclusive GC flock AND the segment's writer flock.
    pub torn_tails_dropped: usize,
    pub peak_pack_bytes: u64,
}

/// Cheap open-time trigger check, git-gc-auto style: a directory
/// listing and two counters — no pack is opened or scanned.
pub(crate) fn triggers_fire(dir: &Path, opts: &Options, view: &IndexView) -> Result<bool> {
    let packs = segment::list_packs(&dir.join(segment::PACKS_DIR))?;
    if packs.len() > opts.max_segments {
        return Ok(true);
    }
    let total: u64 = packs.iter().map(|p| p.size).sum();
    if total > 0 && (view.approx_dead as f64) > (total as f64) * opts.dead_ratio_trigger {
        return Ok(true);
    }
    if let Some(cap) = opts.size_cap_bytes
        && total > cap
    {
        return Ok(true);
    }
    Ok(false)
}

/// Run mark + repack. The caller holds the exclusive `gc.lock` flock
/// for the whole call, and `view` was (re)loaded under that lock — so
/// no flush can interleave and plain index writes are safe here (the
/// load+merge discipline is satisfied by construction).
pub(crate) fn run(
    dir: &Path,
    opts: &Options,
    now: u64,
    mut view: IndexView,
) -> Result<(IndexView, GcStats)> {
    let packs_dir = dir.join(segment::PACKS_DIR);
    let grace_secs = opts.grace.as_secs();
    let mut stats = GcStats::default();

    // ── Evict: size-cap pressure removes LRU roots first.
    if let Some(cap) = opts.size_cap_bytes {
        while live_bytes(&view) > cap {
            let candidate = view
                .roots
                .iter()
                .filter(|(_, e)| now.saturating_sub(e.last_use) >= grace_secs)
                // Oldest last_use first; name tiebreak keeps eviction
                // deterministic when timestamps collide.
                .min_by(|a, b| (a.1.last_use, a.0).cmp(&(b.1.last_use, b.0)))
                .map(|(name, _)| name.clone());
            let Some(name) = candidate else { break };
            let entry = view
                .roots
                .remove(&name)
                .expect("candidate came from this map");
            // approx_dead += Σ bytes referenced by the evicted root.
            // Overestimates by the shared fraction; worst harm is one
            // spurious repack — the mark below is ground truth.
            view.approx_dead += entry
                .digests
                .iter()
                .filter_map(|d| view.blobs.get(d))
                .map(|l| u64::from(l.len))
                .sum::<u64>();
            stats.roots_evicted += 1;
        }
    }

    // ── Mark: live = union of surviving roots' digests.
    let live: HashSet<Digest> = view
        .roots
        .values()
        .flat_map(|e| e.digests.iter().copied())
        .collect();

    // ── Repack, one source segment at a time.
    // Sources are listed BEFORE the consolidated pack exists, so the
    // consolidated pack is never its own source.
    let sources = segment::list_packs(&packs_dir)?;
    let mut consolidated: Option<Segment> = None;
    // Digests already copied into the consolidated pack this run. The
    // same digest can sit in several source segments (two racing
    // writers each append their own copy); without this, every copy is
    // re-copied and the duplicates persist through every future GC.
    let mut copied: HashSet<Digest> = HashSet::new();
    let mut repacked_any = false;
    stats.peak_pack_bytes = segment::total_pack_bytes(&packs_dir)?;

    for src in sources {
        // In-flight grace: a pack younger than the grace window may
        // hold blobs whose roots haven't been added yet (an eval in
        // progress, possibly by a crashed-and-restarting process).
        // Skip it; it ages into eligibility.
        if now.saturating_sub(src.mtime_unix) < grace_secs {
            continue;
        }
        let path = segment::pack_path(dir, &src.name);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue, // raced with nothing we own; skip
        };
        // A live writer holds a shared flock → skip, never wait.
        if !lock::try_lock_exclusive(&file)? {
            stats.packs_skipped_live_writer += 1;
            continue;
        }

        // We now hold the exclusive GC flock AND this segment's writer
        // flock exclusively (owner provably gone) — the only context
        // allowed to drop a torn tail. The drop is physical via the
        // unlink below: the tail simply isn't copied.
        let data = fs::read(&path)?;
        let outcome = crate::record::scan(&data);
        if outcome.tail_garbage_from.is_some() {
            stats.torn_tails_dropped += 1;
        }
        let mut redirects = Vec::new();
        for rec in &outcome.records {
            if rec.kind == KIND_ROOT {
                // Stale root snapshots; fresh ones are re-emitted into
                // the consolidated pack on creation.
                stats.records_dropped += 1;
                continue;
            }
            if !live.contains(&rec.digest) {
                stats.records_dropped += 1;
                continue;
            }
            if !copied.insert(rec.digest) {
                // Duplicate of a record already consolidated this run;
                // the index points at that copy. Not "dropped" — the
                // content survives.
                continue;
            }
            let payload = &data[rec.payload_offset as usize..][..rec.payload_len as usize];
            let cons = ensure_consolidated(&packs_dir, &mut consolidated, &view)?;
            let encoded = crate::record::encode(rec.kind, payload, &rec.digest)?;
            let rec_offset = cons.append(&encoded)?;
            redirects.push((
                rec.digest,
                index::BlobLoc {
                    pack: cons.name.clone(),
                    offset: rec_offset + crate::RECORD_HEADER_LEN as u64,
                    len: rec.payload_len,
                    kind: rec.kind,
                },
            ));
            stats.records_copied += 1;
        }
        // A source about to be unlinked may hold the only on-disk ROOT
        // records — a warm eval's segment is often roots-only (every
        // blob dedup'd, so no live record forces consolidation above).
        // Packs alone must rebuild the root table, so the consolidated
        // pack (seeded with fresh ROOT records at creation) must exist
        // before this source's ROOT bytes disappear.
        if !view.roots.is_empty() {
            ensure_consolidated(&packs_dir, &mut consolidated, &view)?;
        }
        if let Some(cons) = &consolidated {
            cons.sync()?;
        }

        // Swap the index BEFORE unlinking the source: a crash between
        // the two leaves a duplicate pack on disk (harmless — rebuild
        // dedups by digest), never an index entry pointing at nothing.
        for (digest, loc) in redirects {
            view.blobs.insert(digest, loc);
        }
        view.blobs.retain(|_, loc| loc.pack != src.name);
        index::write(dir, &view)?;
        stats.peak_pack_bytes = stats
            .peak_pack_bytes
            .max(segment::total_pack_bytes(&packs_dir)?);
        fs::remove_file(&path)?;
        repacked_any = true;
        stats.packs_repacked += 1;
        // file drops here; its exclusive flock dies with the fd.
    }

    if repacked_any {
        // Dead bytes in repacked segments are reclaimed. Segments we
        // skipped (live writer / grace) may still hold dead bytes — an
        // undercount until their writers exit; the next mark is ground
        // truth either way.
        view.approx_dead = 0;
    }
    index::write(dir, &view)?;
    Ok((view, stats))
}

/// Live bytes under the current root set — the size-cap currency.
fn live_bytes(view: &IndexView) -> u64 {
    let live: HashSet<Digest> = view
        .roots
        .values()
        .flat_map(|e| e.digests.iter().copied())
        .collect();
    live.iter()
        .filter_map(|d| view.blobs.get(d))
        .map(|l| u64::from(l.len))
        .sum()
}

/// Create the consolidated pack on first need, seeding it with one
/// fresh ROOT record per surviving root — source segments holding the
/// old root snapshots are about to be unlinked, and packs alone must
/// be able to rebuild the root table.
fn ensure_consolidated<'a>(
    packs_dir: &Path,
    consolidated: &'a mut Option<Segment>,
    view: &IndexView,
) -> Result<&'a mut Segment> {
    if consolidated.is_none() {
        let mut seg = Segment::create(packs_dir)?;
        let mut roots: Vec<(&String, &index::RootEntry)> = view.roots.iter().collect();
        roots.sort_unstable_by_key(|(name, _)| name.as_str());
        for (name, entry) in roots {
            let payload = index::encode_root_payload(name, entry)?;
            let digest = Digest::of(&payload);
            let encoded = crate::record::encode(KIND_ROOT, &payload, &digest)?;
            seg.append(&encoded)?;
        }
        *consolidated = Some(seg);
    }
    Ok(consolidated.as_mut().expect("just ensured"))
}
