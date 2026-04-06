# §2.10 — FUSE waiter timeout arithmetic

**Parent:** [`phase4a.md` §2.10](../phase4a.md#210-fuse-waiter-timeout-arithmetic)
**Findings:** `fuse-wait-timeout-vs-stream-timeout`, `fuse-blockon-thread-exhaustion`, `fuse-index-disk-divergence`
**Status:** OPEN · P1 · three independent fixes, landable separately

---

## 0. Failure model

All three findings share a root assumption that turned out wrong: **"cache
miss is rare, fast, and self-consistent."** In practice:

| | Assumed | Reality |
|---|---|---|
| Fetch duration | sub-second | up to `GRPC_STREAM_TIMEOUT` = 300s (4 GiB @ 15 MB/s ≈ 270s) |
| Concurrent cold paths | ≤ 1–2 | a fresh build deriving from `stdenv` can look up dozens before any complete |
| Index ↔ disk | atomic | `rm -rf /var/rio/cache/*` during debugging, interrupted eviction, disk failure |

The fix set doesn't challenge those realities — large NARs and debug-rm
are legitimate — it makes the code tolerate them.

---

## 1. `fuse-wait-timeout-vs-stream-timeout` — loop `wait()`, don't give up at 30s

### 1.1 The two numbers that don't agree

```
rio-worker/src/fuse/fetch.rs:60    WAIT_TIMEOUT         = 30s   (waiter gives up)
rio-common/src/grpc.rs:21          GRPC_STREAM_TIMEOUT  = 300s  (fetcher is still healthy)
```

Timeline for a 1 GiB NAR at 15 MB/s (~70s):

```
T+0s   FUSE-thread-A: lookup(busybox) → try_start_fetch → Fetch(guard), starts gRPC stream
T+0s   FUSE-thread-B: lookup(busybox) → try_start_fetch → WaitFor(entry), parks on condvar
T+30s  FUSE-thread-B: entry.wait(30s) → false (timed out) → Err(EAGAIN)  ← BUG
T+30s  builder B:     open("/nix/store/…-busybox/bin/sh") → EAGAIN
                      nix-daemon does NOT retry EAGAIN on open(); build fails
T+70s  FUSE-thread-A: stream complete, NAR extracted, guard drops, notify_all()  ← too late
```

Thread B failed a build that would have succeeded 40 seconds later. The
fetcher was never stuck — it was making progress the whole time.

### 1.2 Why loop, not just raise WAIT_TIMEOUT

Raising `WAIT_TIMEOUT` to 300s (or `GRPC_STREAM_TIMEOUT + ε`) *works*, but
couples two constants that live in different crates and will drift again
the next time someone tunes `GRPC_STREAM_TIMEOUT` for a larger
`MAX_NAR_SIZE`. A `const _: () = assert!(...)` cross-crate guard would
help but is ugly.

**Better:** the `FetchGuard::drop` impl (`cache.rs:70-85`) already does
exactly what we need — it removes the entry from `inflight` **and** sets
`done=true` **and** fires `notify_all()`, and it fires on success, error,
*and* panic. So:

- `entry.wait()` returns `true` → fetcher finished (success or error),
  check the cache, done.
- `entry.wait()` returns `false` (timeout) **and** `done` is still
  `false` → fetcher is *still working*. Wait again.
- `entry.wait()` returns `false` **but** a fresh `try_start_fetch`
  returns `Fetch` → the fetcher is gone (should never happen — guard's
  Drop is RAII, would have notified — but if it somehow leaked, we
  recover by becoming the new fetcher).

The 30s slice becomes a **heartbeat**, not a deadline. We log at `debug`
every slice so a genuinely stuck fetcher (not progressing, not dead) is
visible in logs — but we don't fail the build for it. The `GRPC_STREAM_TIMEOUT`
on the fetcher side is the *real* deadline; when that fires, the fetcher
returns `Err(EIO)`, guard drops, waiters wake.

### 1.3 `InflightEntry` — expose `done` without re-locking `inflight`

`InflightEntry::wait()` already checks `done` internally; we just need a
cheap re-check after a timeout without going back to the `inflight` map
(which would take a lock on a hot path).

```diff
--- a/rio-worker/src/fuse/cache.rs
+++ b/rio-worker/src/fuse/cache.rs
@@ -37,10 +37,23 @@
 impl InflightEntry {
     /// Block the current thread until the fetch completes or `timeout` elapses.
     ///
-    /// Returns `true` if the fetch completed, `false` on timeout.
+    /// Returns `true` if the fetch completed, `false` on timeout. A `false`
+    /// return does NOT mean the fetcher is dead — the guard's `Drop` impl
+    /// fires on success, error, and panic alike. `false` means only "still
+    /// working after `timeout`." Callers that need to distinguish "slow"
+    /// from "dead" should loop on `wait()` while [`Self::is_done`] stays
+    /// `false`; the fetcher's own timeout (GRPC_STREAM_TIMEOUT) is the
+    /// real deadline.
     pub fn wait(&self, timeout: Duration) -> bool {
         let done = self.done.lock().unwrap_or_else(|e| e.into_inner());
         let (done, wait_result) = self
             .cv
             .wait_timeout_while(done, timeout, |d| !*d)
             .unwrap_or_else(|e| e.into_inner());
         !wait_result.timed_out() && *done
     }
+
+    /// Cheap check: has the fetcher finished (guard dropped)? No condvar wait.
+    /// Use after a timed-out `wait()` to decide whether to wait again.
+    pub fn is_done(&self) -> bool {
+        *self.done.lock().unwrap_or_else(|e| e.into_inner())
+    }
 }
```

### 1.4 `ensure_cached` — loop the `WaitFor` arm

```diff
--- a/rio-worker/src/fuse/fetch.rs
+++ b/rio-worker/src/fuse/fetch.rs
@@ -56,30 +56,55 @@
             FetchClaim::WaitFor(entry) => {
-                // Another thread is fetching. Wait for it with a timeout as
-                // belt-and-suspenders against a stuck fetcher (the guard's
-                // Drop impl fires even on panic, so this timeout is defensive).
-                const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
-                if !entry.wait(WAIT_TIMEOUT) {
-                    tracing::warn!(
-                        store_path = store_basename,
-                        timeout_secs = WAIT_TIMEOUT.as_secs(),
-                        "concurrent fetch did not complete within timeout, returning EAGAIN"
-                    );
-                    return Err(Errno::EAGAIN);
-                }
-                // Fetch completed — check cache again. Fetcher failure =>
-                // Ok(None) => ENOENT so the FUSE caller can retry.
-                // Index error => EIO (loud failure, not silent re-fetch).
-                match self.cache.get_path(store_basename) {
-                    Ok(Some(p)) => Ok(p),
-                    Ok(None) => Err(Errno::ENOENT),
-                    Err(e) => {
-                        tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed after wait");
-                        Err(Errno::EIO)
-                    }
-                }
+                // Another thread is fetching. The fetcher has GRPC_STREAM_TIMEOUT
+                // (300s) to finish; a single wait(30s) returning false means
+                // "slow", not "dead" — the guard's Drop fires even on panic, so
+                // a truly dead fetcher would have notified. We loop wait() as
+                // a heartbeat and bound the TOTAL wait at stream-timeout + slop.
+                // If we exceed that, the fetcher's own timeout should already
+                // have fired and dropped the guard; something is deeply wrong
+                // (executor starvation?) and EAGAIN is the least-bad errno.
+                self.wait_for_fetcher(&entry, store_basename)
             }
         }
     }
+
+    /// Park on the singleflight condvar until the fetcher finishes or the
+    /// global deadline passes. See `WaitFor` arm in `ensure_cached`.
+    fn wait_for_fetcher(
+        &self,
+        entry: &super::cache::InflightEntry,
+        store_basename: &str,
+    ) -> Result<PathBuf, Errno> {
+        const WAIT_SLICE: std::time::Duration = std::time::Duration::from_secs(30);
+        // The fetcher's own timeout is the real deadline. Slop absorbs
+        // the time between block_on returning and guard Drop firing.
+        const WAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(
+            rio_common::grpc::GRPC_STREAM_TIMEOUT.as_secs() + 30,
+        );
+        let started = std::time::Instant::now();
+        loop {
+            if entry.wait(WAIT_SLICE) {
+                break; // fetcher done (success, error, or panic — guard dropped)
+            }
+            if started.elapsed() >= WAIT_DEADLINE {
+                tracing::warn!(
+                    store_path = store_basename,
+                    waited_secs = started.elapsed().as_secs(),
+                    "fetcher exceeded GRPC_STREAM_TIMEOUT + slop; returning EAGAIN"
+                );
+                return Err(Errno::EAGAIN);
+            }
+            tracing::debug!(
+                store_path = store_basename,
+                waited_secs = started.elapsed().as_secs(),
+                "waiting on concurrent fetch (fetcher still working)"
+            );
+        }
+        // Fetcher finished — check cache. Fetcher-failure ⇒ Ok(None) ⇒ ENOENT
+        // (kernel will re-lookup; ATTR_TTL won't cache a negative here because
+        // we never reply.entry'd). Index error ⇒ EIO (loud, not silent retry).
+        match self.cache.get_path(store_basename) {
+            Ok(Some(p)) => Ok(p),
+            Ok(None) => Err(Errno::ENOENT),
+            Err(e) => {
+                tracing::error!(store_path = store_basename, error = %e, "cache index query failed after wait");
+                Err(Errno::EIO)
+            }
+        }
+    }
 }
```

**No behavior change for `prefetch_path_blocking`** — its `WaitFor` arm
(`fetch.rs:151-162`) already drops without waiting, which stays correct:
prefetch is a hint; if FUSE already has it in flight, prefetch's job is done.

---

## 2. `fuse-blockon-thread-exhaustion` — semaphore bound on FUSE-initiated fetches

### 2.1 The starvation case

`fuse_threads` defaults to 4 (`config.rs:108`). `fetch_extract_insert`
blocks its calling FUSE thread for the entire gRPC stream + NAR extract.
With 4 distinct cold paths requested simultaneously:

```
FUSE-thread-0: block_on(fetch glibc)     — 120s
FUSE-thread-1: block_on(fetch gcc)       — 180s
FUSE-thread-2: block_on(fetch stdenv)    — 45s
FUSE-thread-3: block_on(fetch bash)      — 30s
                                          ↑ pool exhausted
FUSE-thread-?: lookup(hello/bin)         — already on disk, <1ms work
                                          ← queued in kernel, waits 30s
```

The warm-path lookup that should take microseconds waits behind the
shortest cold fetch. A build that's running `./configure` (thousands of
`stat()` calls on already-cached paths) stalls for 30s.

This is **not** the same as §1's singleflight — those 4 fetches are for
*distinct* paths, each legitimately owns its `FetchGuard`. The problem is
that `fuser::spawn_mount2` dispatches kernel requests round-robin across a
fixed pool, and `block_on` makes a thread invisible to the dispatcher.

### 2.2 Fix: `std::sync::Semaphore` on NixStoreFs

We already do this on the async side for prefetch (`main.rs:230`:
`prefetch_sem = Arc::new(Semaphore::new(8))`, a `tokio::sync::Semaphore`
gating `spawn_blocking` tasks). The FUSE side is sync, so we need a sync
primitive. `std` doesn't ship a semaphore; `parking_lot` doesn't either.
`tokio::sync::Semaphore` has a **blocking** `acquire` via
`Handle::block_on(sem.acquire())` but that's a second nested `block_on`
inside the existing one — it would work (the outer `block_on` hasn't
started yet when we'd acquire) but it's ugly.

**Simplest correct answer:** a counting semaphore built from
`Mutex<usize> + Condvar`, same primitives `InflightEntry` already uses.
Put it next to `InflightEntry` in `cache.rs` — the two are logically
paired (both are sync coordination for FUSE threads).

Permits = `fuse_threads.saturating_sub(1).max(1)`. With the default of 4
threads that's 3 concurrent fetches, 1 thread always free for hot-path
`lookup`/`getattr`/`read`. With `fuse_threads=1` (possible in tests or
weird configs) permits degrade to 1, which is the current behavior — no
regression.

```diff
--- a/rio-worker/src/fuse/cache.rs  (new struct, next to InflightEntry)
+++ b/rio-worker/src/fuse/cache.rs
@@ ... @@
+/// Blocking counting semaphore for FUSE-thread fetch concurrency.
+///
+/// `tokio::sync::Semaphore` is async; we're in a sync FUSE callback that
+/// hasn't entered `block_on` yet. Building on `Mutex+Condvar` (same as
+/// `InflightEntry`) avoids a dependency and a nested-block_on wart.
+///
+/// No `try_acquire` — a FUSE-initiated fetch MUST eventually happen (the
+/// build depends on it). We always block; the semaphore just serializes
+/// the rate so some FUSE threads stay free for the hot path.
+pub(super) struct FetchSemaphore {
+    permits: Mutex<usize>,
+    cv: Condvar,
+}
+
+impl FetchSemaphore {
+    pub(super) fn new(permits: usize) -> Self {
+        Self { permits: Mutex::new(permits), cv: Condvar::new() }
+    }
+
+    pub(super) fn acquire(&self) -> FetchPermit<'_> {
+        let mut p = self.permits.lock().unwrap_or_else(|e| e.into_inner());
+        while *p == 0 {
+            p = self.cv.wait(p).unwrap_or_else(|e| e.into_inner());
+        }
+        *p -= 1;
+        FetchPermit { sem: self }
+    }
+}
+
+pub(super) struct FetchPermit<'a> { sem: &'a FetchSemaphore }
+
+impl Drop for FetchPermit<'_> {
+    fn drop(&mut self) {
+        *self.sem.permits.lock().unwrap_or_else(|e| e.into_inner()) += 1;
+        self.sem.cv.notify_one();
+    }
+}
```

### 2.3 Wire it through

```diff
--- a/rio-worker/src/fuse/mod.rs
+++ b/rio-worker/src/fuse/mod.rs
@@ -41,6 +41,13 @@
 pub struct NixStoreFs {
     // ... existing fields ...
     runtime: Handle,
+    /// Bounds concurrent FUSE-initiated fetches to `fuse_threads - 1` so at
+    /// least one FUSE thread stays free for hot-path ops (lookup on cached
+    /// paths, getattr, read). Without this, N cold paths blocking in
+    /// `fetch_extract_insert` for up to 300s each starve warm-path ops
+    /// that would complete in microseconds. See phase4a §2.10
+    /// fuse-blockon-thread-exhaustion.
+    fetch_sem: cache::FetchSemaphore,
 }

 impl NixStoreFs {
     pub fn new(
         cache: Arc<Cache>,
         store_client: StoreServiceClient<Channel>,
         runtime: Handle,
         passthrough: bool,
+        fuse_threads: u32,
     ) -> Self {
+        // fuse_threads - 1, floored at 1: with n_threads=1 (tests) this
+        // degrades to current behavior (serialized). With n_threads=4
+        // (default) we get 3 concurrent fetches + 1 free thread.
+        let fetch_permits = (fuse_threads as usize).saturating_sub(1).max(1);
         // ...
+            fetch_sem: cache::FetchSemaphore::new(fetch_permits),
         }
     }

--- a/rio-worker/src/fuse/mod.rs  (mount_fuse_background already has n_threads)
+++ b/rio-worker/src/fuse/mod.rs
@@ -210 +210 @@
-    let fs = NixStoreFs::new(cache, store_client, runtime, passthrough);
+    let fs = NixStoreFs::new(cache, store_client, runtime, passthrough, n_threads);
```

### 2.4 Acquire in the `Fetch` arm — not the `WaitFor` arm

Critical placement: acquire **after** `try_start_fetch` returns `Fetch`,
**before** entering `fetch_extract_insert`. The `WaitFor` arm parks on a
condvar — that's not a `block_on`, it doesn't starve the tokio runtime,
and the waiting FUSE thread is blocked *anyway* (waiting is its whole
purpose). We're only protecting against *fetching* threads.

```diff
--- a/rio-worker/src/fuse/fetch.rs  (in ensure_cached, Fetch arm)
+++ b/rio-worker/src/fuse/fetch.rs
@@ -44,12 +44,20 @@
         match self.cache.try_start_fetch(store_basename) {
             FetchClaim::Fetch(_guard) => {
-                // We own the fetch. _guard notifies waiters on drop (success,
-                // error, or panic) — no explicit cleanup needed.
-                // Delegate to the free fn — same code path prefetch uses.
+                // We own the fetch. _guard notifies waiters on drop (success,
+                // error, or panic). The _permit bounds concurrent FUSE-thread
+                // fetches so at least one thread stays free for hot-path ops.
+                //
+                // Permit is acquired AFTER the singleflight claim, so waiters
+                // for this path don't contend for a permit — they're parked
+                // on _guard's condvar, which is a cheap sleep, not a block_on.
+                // If acquire() blocks here, we're the (fuse_threads)th
+                // concurrent fetch; the builder that triggered this lookup
+                // waits, which is the lesser evil vs. starving warm builds.
+                let _permit = self.fetch_sem.acquire();
                 fetch_extract_insert(
                     &self.cache,
                     &self.store_client,
                     &self.runtime,
                     store_basename,
                 )
             }
```

**`prefetch_path_blocking` does NOT acquire from `fetch_sem`.** It already
has its own semaphore (`main.rs:230`, 8 permits on `spawn_blocking`) and
runs on the tokio blocking pool, not the FUSE thread pool. Gating it here
would couple two unrelated thread budgets.

---

## 3. `fuse-index-disk-divergence` — stat the fast-path return, self-heal on ENOENT

### 3.1 The lie `get_path()` tells

`Cache::get_path()` (`cache.rs:341-347`) is a SQLite query. It answers
"does the index have a row for this basename?" — it does **not** stat
disk. `fetch.rs:35-36` trusts it:

```rust
match self.cache.get_path(store_basename) {
    Ok(Some(local_path)) => return Ok(local_path),  // ← never checks local_path exists
```

If `/var/rio/cache/abc-busybox` was `rm -rf`'d (manual debugging, a disk
cleanup script, an interrupted eviction that deleted disk but died before
`DELETE FROM cached_paths`), every subsequent `ensure_cached("abc-busybox")`
returns `Ok(Some(path-that-doesn't-exist))` — forever. The caller's
`symlink_metadata()` then ENOENTs and the build fails.

This is **already how we fault-inject** the slow-path coverage scenario
(see §4 below) — we `rm` from under the index precisely *because* nothing
reconciles. That's useful for a test but pathological for production.

### 3.2 Cost of the stat

One `symlink_metadata` per warm `ensure_cached` call. That sounds
expensive but:

1. `ensure_cached` is only called from `lookup` on the **root** parent
   (`ops.rs:106-118`) and the slow-path fallbacks in `getattr`/`open`/
   `readlink`/`readdir`. The hot path in `ops.rs:78-85` stats
   `child_path` *directly* and returns without touching `ensure_cached`
   at all — `ensure_cached` only runs when that `symlink_metadata` at
   `ops.rs:78` already ENOENTed.

2. So the real cost is: one **extra** stat on the first lookup of each
   store path root, plus one extra stat on each slow-path fallback.
   The slow-path fallback stat is vestigial (the outer caller already
   ENOENTed, which is *why* we're in `ensure_cached`) but the first-
   lookup stat is net new. That's one syscall per store-path per build.
   Negligible.

### 3.3 Self-heal on ENOENT

When the stat fails with ENOENT, we need to delete the stale index row
and fall through to the fetch path — same as if `get_path` had returned
`Ok(None)`. `cache.rs` already has the delete SQL at line 498 (inside
`evict_if_needed`); extract it to a method.

```diff
--- a/rio-worker/src/fuse/cache.rs  (new method on Cache)
+++ b/rio-worker/src/fuse/cache.rs
@@ ... @@
+    /// Remove a stale index row. Called when the index says "present" but
+    /// the file is gone from disk (external rm, interrupted eviction).
+    /// Best-effort: logs on failure but doesn't propagate — if the DELETE
+    /// fails, the next fetch will `INSERT OR REPLACE` over it anyway
+    /// (`insert()`, line 355).
+    ///
+    /// Does NOT touch the bloom filter (bloom doesn't support deletion;
+    /// `insert()` already documents stale-positive tolerance at line 125).
+    pub fn remove_stale(&self, store_path: &str) {
+        let pool = &self.pool;
+        if let Err(e) = self.runtime.block_on(async {
+            sqlx::query("DELETE FROM cached_paths WHERE store_path = ?1")
+                .bind(store_path)
+                .execute(pool)
+                .await
+        }) {
+            tracing::warn!(store_path, error = %e, "failed to remove stale index row (will be overwritten on re-fetch)");
+        }
+    }
```

```diff
--- a/rio-worker/src/fuse/fetch.rs
+++ b/rio-worker/src/fuse/fetch.rs
@@ -33,10 +33,32 @@
     pub(super) fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
         match self.cache.get_path(store_basename) {
-            Ok(Some(local_path)) => return Ok(local_path),
+            Ok(Some(local_path)) => {
+                // Self-healing fast path: the index says present — verify
+                // disk agrees. If an external rm (debugging, interrupted
+                // eviction) deleted the file but left the SQLite row,
+                // trusting the index here makes the path PERMANENTLY
+                // unfetchable (every call returns a path that doesn't exist;
+                // we never fall through to fetch). Stat is one extra syscall
+                // per store-path-root lookup — cheap, and ensure_cached only
+                // runs when ops.rs:78 already missed.
+                match local_path.symlink_metadata() {
+                    Ok(_) => return Ok(local_path),
+                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
+                        tracing::warn!(
+                            store_path = store_basename,
+                            local_path = %local_path.display(),
+                            "cache index says present but disk disagrees; purging stale row and re-fetching"
+                        );
+                        metrics::counter!("rio_worker_fuse_index_divergence_total").increment(1);
+                        self.cache.remove_stale(store_basename);
+                        // fall through to try_start_fetch below
+                    }
+                    Err(e) => {
+                        // EACCES/EIO on the stat — something else is wrong.
+                        // Don't silently re-fetch (would mask disk failure).
+                        tracing::error!(store_path = store_basename, error = %e, "cache stat failed (not ENOENT)");
+                        return Err(Errno::EIO);
+                    }
+                }
+            }
             Ok(None) => {} // not cached, fetch below
```

New metric `rio_worker_fuse_index_divergence_total` — add to
`observability.md` and the worker's `describe_metrics()`. A nonzero value
on a dashboard is a tell for disk/eviction problems worth investigating.

### 3.4 `ops.rs` callers — remove the now-redundant post-cached stat

`ops.rs:119-135` does `ensure_cached → Ok(local_path) → local_path.symlink_metadata()`.
After §3.3 lands, `ensure_cached` already verified the path exists when it
took the fast path. But the *fetch* path (`fetch_extract_insert`) doesn't
stat its return — it renames into place and trusts that. So the caller-
side stat at `ops.rs:119` is still the *only* stat when the fetch arm was
taken. Leave it. The redundancy is fast-path-only and costs one syscall
that will hit the kernel's inode cache (we just stat'd the same path).

---

## 4. `ops.rs` — lift the 5-constraint doc into module scope

The slow-path fallbacks in `getattr`/`open`/`readlink`/`readdir`
(`ops.rs:176-197,231-249,265-283,436-455`) are near-unreachable in normal
operation, and *how* to reach them for coverage is subtle enough that it
took five iterations to get the VM-test fault injection right. That
knowledge currently lives only in memory notes. Promote it.

### 4.1 Module doc comment

```diff
--- a/rio-worker/src/fuse/ops.rs
+++ b/rio-worker/src/fuse/ops.rs
@@ -1,2 +1,56 @@
-//! FUSE `Filesystem` trait implementation for `NixStoreFs`.
+//! FUSE `Filesystem` trait implementation for `NixStoreFs`.
+//!
+//! # Slow-path reachability (the five constraints)
+//!
+//! Each of `getattr`/`open`/`readlink`/`readdir` has a slow-path fallback:
+//! fast-path `File::open`/`read_link`/etc ENOENTs → `ensure_cached` → retry.
+//! These fallbacks are **structurally unreachable** in normal operation
+//! because `lookup()` at line ~118 eagerly materializes the entire NAR on
+//! first root-child access. By the time any other op fires for an inode
+//! under that root, the file is on disk. Reaching the slow path for a test
+//! requires fault-injecting around five interacting constraints:
+//!
+//! 1. **`ensure_cached` checks the SQLite index, not disk.** `rm -rf` the
+//!    cache file with the index row intact → `ensure_cached` returns
+//!    `Ok(path-that-doesn't-exist)`, slow path's `&& let Err(errno)` is
+//!    false, second `File::open` still ENOENTs → "failed after
+//!    ensure_cached" branch. (§3 above makes this self-healing in prod;
+//!    tests bypass by deleting the *child* file, not the store-path root
+//!    — the root stat in `ensure_cached` passes, the child stat in ops
+//!    fails.)
+//!
+//! 2. **`ATTR_TTL = 3600s`** — kernel won't re-ask FUSE `getattr` within
+//!    a short test. Attrs cached from a prior `ls -la`. BUT `open`/
+//!    `readlink`/`readdir` don't consult the attr cache: kernel resolves
+//!    path via cached dentry → calls FUSE op by inode. **`getattr`'s slow
+//!    path stays dark within ATTR_TTL.** Mark COVERAGE-unreachable.
+//!
+//! 3. **`lookup`'s `ensure_cached` is root-only** (`parent == ROOT` gate).
+//!    Delete a subdir → fresh `lookup(parent_ino, "subdir")` returns plain
+//!    ENOENT without trying `ensure_cached`. So `echo 3 > drop_caches`
+//!    BREAKS the test — must rely on cached dentries.
+//!
+//! 4. **`readdir` doesn't populate dcache; `ls -la`'s stat-per-entry
+//!    does.** A file only seen in plain `ls` (no `-la`) has no dentry;
+//!    the kernel re-looks-up on access → constraint 3 applies → plain
+//!    ENOENT, not slow path. `ls -la` the parent first.
+//!
+//! 5. **Intervening worker restart clears the dentry cache.**
+//!    `MountOption::AutoUnmount` + systemd `Restart=on-failure` → fresh
+//!    mount → kernel drops ALL dentries. If an earlier subtest SIGKILLed
+//!    the target worker, re-seed dentries (`ls -la`) *inside* the
+//!    fault-inject subtest, immediately before the `rm`.
+//!
+//! **Tell for partial success:** journalctl shows a subset of expected
+//! `failed after ensure_cached` warns; the ones that DID fire have low
+//! inode numbers (fresh counter post-restart); the missing ones are for
+//! files no build accesses by name after the restart.
+//!
+//! Pattern (VM test `testScript`):
+//! ```text
+//! ls -la /var/rio/fuse-store/<busybox>/   # seed dentries (c. 4, 5)
+//! rm /var/rio/cache/<busybox>/bin/sh      # child of root, index untouched (c. 1)
+//! # DO NOT drop_caches (c. 3)
+//! cat /var/rio/fuse-store/<busybox>/bin/sh     → open slow path
+//! # getattr: unreachable within ATTR_TTL (c. 2)
+//! ```
```

### 4.2 Mark `getattr` slow-path as COVERAGE-unreachable

```diff
--- a/rio-worker/src/fuse/ops.rs  (inside getattr, at the slow-path branch)
+++ b/rio-worker/src/fuse/ops.rs
@@ -175,6 +175,12 @@
-                // If it's a known store path, try ensuring it's cached
+                // COVERAGE: unreachable within ATTR_TTL (3600s). Constraint 2
+                // in the module doc — the kernel caches attrs from lookup()'s
+                // reply.entry and never re-asks getattr for that inode within
+                // a VM test's runtime. open/readlink/readdir don't consult the
+                // attr cache (they route by cached dentry → FUSE op by inode)
+                // so THEIR slow paths are reachable; this one is not. Kept as
+                // belt-and-suspenders for the post-TTL case and ATTR_TTL=0
+                // debug builds.
                 if let Some(basename) = self.store_basename_for_inode(ino.0) {
```

---

## 5. Tests

### 5.1 Concurrent waiters during slow fetch — no EAGAIN

This is the direct verification of §1. Needs a MockStore that stalls
`GetPath` long enough for multiple waiters to park and at least one
`WAIT_SLICE` to elapse, then releases. MockStore currently has
`fail_get_path: AtomicBool` and `get_path_garbage: AtomicBool`
(`rio-test-support/src/grpc.rs:52,55`) but no delay knob — add one.

```diff
--- a/rio-test-support/src/grpc.rs
+++ b/rio-test-support/src/grpc.rs
@@ ... @@
 pub struct MockStore {
     // ... existing knobs ...
+    /// If set, `GetPath` awaits this Notify before responding. Tests set
+    /// it at construction, spawn concurrent callers, then `.notify_waiters()`
+    /// to release all at once. Distinct from `fail_get_path` (which errors
+    /// immediately) — this holds-then-succeeds.
+    pub get_path_gate: Arc<tokio::sync::Notify>,
+    /// Whether `get_path_gate` is armed. When false, `GetPath` ignores the
+    /// gate (backwards-compatible with existing tests).
+    pub get_path_gate_armed: Arc<AtomicBool>,
```

```rust
// rio-worker/src/fuse/fetch.rs, new #[cfg(test)] test

/// Concurrent `ensure_cached` calls for the same path during a slow fetch
/// all succeed — none get EAGAIN. Before the §1 fix, waiters timed out at
/// WAIT_TIMEOUT=30s while the fetcher was still healthy.
///
/// This test can't literally wait 30s. Instead: shorten WAIT_SLICE via a
/// #[cfg(test)] const override, gate MockStore's GetPath on a Notify,
/// spawn N concurrent ensure_cached, wait for all to reach WaitFor (or
/// Fetch), assert at least one WAIT_SLICE debug-log fired, release the
/// gate, join all, assert every result is Ok(path) with identical path.
///
/// r[verify builder.fuse.lookup-caches]  (waiters don't spuriously fail)
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_waiters_no_eagain_during_slow_fetch() {
    let (cache, client, store, dir, rt, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("slowfetch");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"slow-payload");
    store.seed(make_path_info(&store_path, &nar, hash), nar);
    store.get_path_gate_armed.store(true, Ordering::SeqCst);

    // Spawn N=5 concurrent ensure_cached. One wins Fetch, four get WaitFor.
    // With fuse_threads=4 this is MORE waiters than FUSE threads — stresses
    // the condvar broadcast too.
    const N: usize = 5;
    // NixStoreFs is not Send (fuser internals); we test via prefetch_path_blocking
    // which shares the same singleflight map. prefetch's WaitFor arm returns
    // immediately (doesn't wait) — that's wrong for this test. So: build a
    // minimal NixStoreFs and call ensure_cached directly from spawn_blocking.
    //
    // NOTE: NixStoreFs::new needs fuse_threads after §2 — use N here so the
    // fetch semaphore doesn't interfere (permits = N-1 = 4 ≥ 1 fetcher).
    let fs = std::sync::Arc::new(NixStoreFs::new(
        Arc::clone(&cache), client.clone(), rt.clone(), false, N as u32,
    ));

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let fs = Arc::clone(&fs);
        let bn = basename.clone();
        handles.push(tokio::task::spawn_blocking(move || fs.ensure_cached(&bn)));
    }

    // Let the fetcher reach GetPath and the waiters reach the condvar.
    // The gate blocks the fetcher in block_on; waiters park on cv.wait.
    // Sleep longer than WAIT_SLICE so at least one heartbeat fires —
    // this is where the OLD code would have returned EAGAIN.
    tokio::time::sleep(std::time::Duration::from_millis(
        WAIT_SLICE_FOR_TEST.as_millis() as u64 + 100,
    )).await;

    // Release the fetcher.
    store.get_path_gate.notify_waiters();

    // All N must succeed with the same path. Zero EAGAIN.
    let mut paths = Vec::with_capacity(N);
    for h in handles {
        let r = h.await.expect("join");
        let p = r.expect("ensure_cached must succeed (no EAGAIN)");
        paths.push(p);
    }
    assert!(paths.iter().all(|p| p == &paths[0]), "all waiters see same path");
    assert!(paths[0].exists(), "fetched path on disk: {:?}", paths[0]);
    drop(dir); // keep tempdir alive to here
}
```

Implementation detail: `WAIT_SLICE` should be `#[cfg(not(test))] const =
30s` / `#[cfg(test)] const = 200ms` so the test runs in under a second.
Same pattern `EVICT_GRACE_SECS` already uses elsewhere in the codebase
if applicable — if not, this is a clean first instance.

### 5.2 Index/disk divergence — self-heal re-fetches

```rust
// rio-worker/src/fuse/fetch.rs, new #[cfg(test)] test

/// `ensure_cached` with a stale index row (file rm'd, SQLite row intact)
/// detects the divergence, purges the row, re-fetches, and succeeds.
/// Before the §3 fix, this returned Ok(path-that-doesn't-exist) forever.
///
/// r[verify builder.fuse.cache-lru]  (index self-heals on disk divergence)
#[tokio::test(flavor = "multi_thread")]
async fn test_ensure_cached_self_heals_index_disk_divergence() {
    let (cache, client, store, dir, rt, _srv) = setup_fetch_harness().await;

    let basename = test_store_basename("diverge");
    let store_path = format!("/nix/store/{basename}");
    let (nar, hash) = make_nar(b"heal-me");
    store.seed(make_path_info(&store_path, &nar, hash), nar);

    // First fetch: populates both disk and index.
    let fs = NixStoreFs::new(
        Arc::clone(&cache), client.clone(), rt.clone(), false, 4,
    );
    let p1 = tokio::task::spawn_blocking({
        let fs = fs.clone_for_test(); // or Arc-wrap as above
        let bn = basename.clone();
        move || fs.ensure_cached(&bn)
    }).await.expect("join").expect("first fetch");
    assert!(p1.exists());

    // Simulate external rm: delete the file, leave the index row.
    // (single-file NAR extracts to a plain file, not a dir — see
    // test_prefetch_success_roundtrip.)
    std::fs::remove_file(&p1).expect("rm");
    assert!(!p1.exists(), "file gone");
    // Index still says present (use spawn_blocking for block_on):
    let bn = basename.clone();
    let cache_cl = Arc::clone(&cache);
    let still_indexed = tokio::task::spawn_blocking(move || cache_cl.contains(&bn))
        .await.expect("join").expect("contains");
    assert!(still_indexed, "precondition: index row survives rm");

    // Second ensure_cached: should detect ENOENT on the fast-path stat,
    // purge the row, re-fetch, and return a VALID path.
    let p2 = tokio::task::spawn_blocking({
        let bn = basename.clone();
        move || fs.ensure_cached(&bn)
    }).await.expect("join").expect("second fetch (self-heal)");
    assert!(p2.exists(), "self-healed path exists: {:?}", p2);
    assert_eq!(std::fs::read(&p2).expect("read"), b"heal-me");
    drop(dir);
}
```

### 5.3 What we're NOT testing (and why)

| Scenario | Why deferred |
|---|---|
| `FetchSemaphore` actually leaves a thread free under load | Needs a real FUSE mount (kernel round-trip); unit-testing the semaphore in isolation proves `Mutex+Condvar` works, which we trust. VM-test would need N+1 concurrent builds with distinct cold paths — expensive, flaky. Defer to dashboard observation of `rio_worker_fuse_fetch_duration_seconds` p99 under load. |
| `getattr` slow path | Marked COVERAGE-unreachable within ATTR_TTL (§4.2). Fault-inject would need `ATTR_TTL=0` cfg override + separate VM-test variant. Not worth the matrix entry for a belt-and-suspenders branch. |
| `WAIT_DEADLINE` exceeded (fetcher stuck past 330s) | Would need 330s of wall clock OR `#[tokio::test(start_paused)]` — but the condvar is `std`, not tokio-time, so pause doesn't help. The branch is 5 lines of log + return; trust it. |

---

## 6. Landing order

1. **§3 (self-heal)** — standalone, no dependency, smallest diff. Land
   first so the §5.2 test can use the public `remove_stale` without
   touching `evict_if_needed`.
2. **§1 (loop wait)** — depends on the `InflightEntry::is_done` addition.
   `#[cfg(test)]` WAIT_SLICE override lands with it.
3. **§2 (semaphore)** — changes `NixStoreFs::new` signature → touches
   `mount_fuse_background` callers. Land after §1 so the §5.1 test's
   `NixStoreFs::new(…, N as u32)` call has both the loop and the permit.
4. **§4 (module doc)** — doc-only. Stash to end per
   `.rs doc-comment → full rebuild` (tooling-gotchas). Single commit,
   no test changes.
