# Plan 0209: FUSE circuit breaker — worker-side, std::sync ONLY

Wave 1. Circuit breaker for the FUSE fetch path: `threshold` (default 5) consecutive `ensure_cached` failures open the circuit → subsequent calls return `EIO` immediately (fail-fast, don't stall every build waiting on a dead store). After `auto_close_after` (default 30s) the circuit goes half-open — next call probes; success closes, failure re-opens.

**CRITICAL R-SYNC constraint:** [`rio-worker/src/fuse/fetch.rs:268-269`](../../rio-worker/src/fuse/fetch.rs) is explicit:

> "SYNC with internal block_on — caller is either a FUSE thread (dedicated blocking) or spawn_blocking. **Never call from async.**"

`ensure_cached` at `:54` is `fn` not `async fn`. FUSE callbacks run on fuser's thread pool, NOT in a tokio context. **No `tokio::sync`. No `.await`.** `AtomicU32` + `parking_lot::Mutex<Option<Instant>>` only.

Pattern reference: [`rio-scheduler/src/actor/breaker.rs`](../../rio-scheduler/src/actor/breaker.rs) has the 3-state machine shape, but it's single-threaded-actor so uses plain `u32` — this one needs atomics because FUSE is multi-threaded.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[worker.fuse.circuit-breaker]` exists in `worker.md`)

## Tasks

### T1 — `feat(worker):` circuit.rs — 3-state breaker, std::sync ONLY

NEW file `rio-worker/src/fuse/circuit.rs`:

```rust
//! FUSE fetch circuit breaker. std::sync ONLY — FUSE callbacks are NOT
//! in a tokio context (see fetch.rs:268-269). No tokio::sync, no .await.
//!
//! r[impl worker.fuse.circuit-breaker]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use parking_lot::Mutex;

/// Testability: inject time so tests don't need real sleeps.
/// tokio::time::pause does NOT work here — this is std::time::Instant.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant { Instant::now() }
}

#[cfg(test)]
pub struct MockClock(pub Mutex<Instant>);
#[cfg(test)]
impl Clock for MockClock {
    fn now(&self) -> Instant { *self.0.lock() }
}

pub struct CircuitBreaker<C: Clock = SystemClock> {
    consecutive_failures: AtomicU32,
    open_since: Mutex<Option<Instant>>,
    threshold: u32,
    auto_close_after: Duration,
    clock: C,
}

impl CircuitBreaker<SystemClock> {
    pub fn new(threshold: u32, auto_close_after: Duration) -> Self {
        Self::with_clock(threshold, auto_close_after, SystemClock)
    }
}

impl<C: Clock> CircuitBreaker<C> {
    pub fn with_clock(threshold: u32, auto_close_after: Duration, clock: C) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            open_since: Mutex::new(None),
            threshold,
            auto_close_after,
            clock,
        }
    }

    /// EIO if open (elapsed < auto_close). Ok if closed or half-open.
    /// Pure sync — no .await.
    pub fn check(&self) -> Result<(), libc::c_int> {
        let guard = self.open_since.lock();
        match *guard {
            Some(since) if self.clock.now().duration_since(since) < self.auto_close_after => {
                Err(libc::EIO)  // open — fail fast
            }
            _ => Ok(()),  // closed, or half-open (elapsed) — probe
        }
    }

    /// success → reset + close. failure → increment + maybe-open.
    /// Pure sync — no .await.
    pub fn record(&self, ok: bool) {
        if ok {
            self.consecutive_failures.store(0, Ordering::Relaxed);
            let was_open = self.open_since.lock().take().is_some();
            if was_open {
                metrics::gauge!("rio_worker_fuse_circuit_open").set(0.0);
            }
        } else {
            let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= self.threshold {
                let mut guard = self.open_since.lock();
                if guard.is_none() {
                    *guard = Some(self.clock.now());
                    metrics::gauge!("rio_worker_fuse_circuit_open").set(1.0);
                }
            }
        }
    }

    /// For heartbeat (P0210). Includes half-open as "not open".
    pub fn is_open(&self) -> bool {
        let guard = self.open_since.lock();
        matches!(*guard, Some(since) if self.clock.now().duration_since(since) < self.auto_close_after)
    }
}
```

Defaults: `threshold=5`, `auto_close_after=Duration::from_secs(30)`.

### T2 — `feat(worker):` wire into `ensure_cached`

At [`rio-worker/src/fuse/fetch.rs:54`](../../rio-worker/src/fuse/fetch.rs) (`ensure_cached` entry):

```rust
self.circuit.check().map_err(|e| /* map to your Errno type */)?;
```

After the fetch result (post-`block_on`):

```rust
// Timeout (WAIT_DEADLINE at :44 exceeded) is also a failure.
self.circuit.record(result.is_ok());
```

### T3 — `feat(worker):` expose circuit handle on `NixStoreFs`

In [`rio-worker/src/fuse/mod.rs`](../../rio-worker/src/fuse/mod.rs):

- `pub mod circuit;`
- Add `circuit: CircuitBreaker` (or `Arc<CircuitBreaker>` if heartbeat loop needs independent access) field on `NixStoreFs`
- `pub fn circuit(&self) -> &CircuitBreaker { &self.circuit }` — P0210 reads this for heartbeat

### T4 — `feat(worker):` register metric

In [`rio-worker/src/lib.rs`](../../rio-worker/src/lib.rs) describe block: `metrics::describe_gauge!("rio_worker_fuse_circuit_open", "1.0 when FUSE circuit breaker is open (store unreachable)")`.

### T5 — `test(worker):` state transition tests (plain `#[test]`, NOT tokio)

Using `MockClock` (since `tokio::time::pause` does NOT work on `std::time::Instant`):

```rust
// r[verify worker.fuse.circuit-breaker]
#[test]
fn four_failures_stay_closed() { /* record(false)×4 → check() Ok */ }

#[test]
fn fifth_failure_opens() { /* record(false)×5 → check() Err(EIO) */ }

#[test]
fn half_open_after_auto_close() {
    // record(false)×5 → open
    // advance MockClock by 31s → check() Ok (half-open probe)
}

#[test]
fn half_open_success_closes() {
    // open → advance 31s → record(true) → check() Ok, is_open() false
}

#[test]
fn half_open_failure_reopens() {
    // open → advance 31s → record(false) → check() Err(EIO), is_open() true
}
```

## Exit criteria

- `/nbr .#ci` green
- State-transition tests cover both paths: closed→open→half-open→**closed** AND closed→open→half-open→**open**
- `rio_worker_fuse_circuit_open` registered in lib.rs describe block
- `grep -E 'tokio::sync|async fn' rio-worker/src/fuse/circuit.rs` returns **empty** (std::sync only — the hard constraint)

## Tracey

References existing markers:
- `r[worker.fuse.circuit-breaker]` — T1 implements (circuit.rs struct + methods); T5 verifies (state-transition tests)

## Files

```json files
[
  {"path": "rio-worker/src/fuse/circuit.rs", "action": "NEW", "note": "T1: CircuitBreaker struct, std::sync ONLY, Clock trait for testability; r[impl worker.fuse.circuit-breaker]"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "T2: circuit.check() at :54 entry; circuit.record() after block_on result"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "T3: pub mod circuit; circuit field on NixStoreFs; pub fn circuit() getter"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "T4: describe_gauge rio_worker_fuse_circuit_open"}
]
```

```
rio-worker/src/
├── fuse/
│   ├── circuit.rs                 # T1 (NEW): breaker + Clock trait + tests
│   ├── fetch.rs                   # T2: check + record hooks
│   └── mod.rs                     # T3: pub mod + field + getter
└── lib.rs                         # T4: metric describe
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "File-disjoint from all other Wave 1 plans. P0210 needs is_open() — that's a downstream dep, not ours."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[worker.fuse.circuit-breaker]` must exist.

**Conflicts with:** **none in Wave 1.** `circuit.rs` is new; `fetch.rs`/`fuse/mod.rs`/`lib.rs` are single-writer in 4b. P0210 later reads `circuit().is_open()` from `runtime.rs` — that's a downstream dep (P0210 lists us), not a conflict.
