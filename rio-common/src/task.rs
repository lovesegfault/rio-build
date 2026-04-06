//! Task spawning utilities with panic logging.

use std::future::Future;
use std::time::Duration;

use futures_util::FutureExt as _;
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::signal::Token;

/// Spawn a task that logs any panic at `error!` level before unwinding.
///
/// By default, `tokio::spawn` silently swallows panics unless the `JoinHandle`
/// is awaited. For long-running background tasks (actor loops, stream readers,
/// tick loops), this means a panic kills the task with zero diagnostic output.
///
/// `spawn_monitored` catches the panic, logs it with the task name and panic
/// message, and then resumes unwinding so the `JoinHandle` still reports the
/// panic to any awaiter.
///
/// Trace context is propagated across the task boundary via a CHILD span.
/// The root span's `component` field still appears in logs emitted from
/// background tasks (via span_list ancestry) — see
/// `r[obs.log.required-fields]`. `tokio::spawn` does NOT do this on its own;
/// the parent must be captured here, before the spawn point, because
/// `Span::current()` inside the spawned future would return `Span::none()`.
///
/// The child span has the same `trace_id` but an independent lifetime: the
/// caller's span closes when the caller returns, the child closes when the
/// spawned future completes. Previously this re-entered the parent span
/// directly (`.instrument(Span::current())`), which held the caller's
/// `#[instrument]` span open for the spawned task's entire lifetime — a
/// short handler spawning a long-lived bridge task showed `idle_ns=61.9s`
/// on a span that should have closed in <1ms.
pub fn spawn_monitored<F>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Capture BEFORE tokio::spawn — inside the async block, Span::current()
    // is the executor's ambient span (none), not the caller's.
    let parent = tracing::Span::current();
    // Child span, not the parent itself: same trace_id, separate span_id,
    // separate lifetime. The parent handle drops at the end of this fn.
    let span = tracing::info_span!(parent: &parent, "spawned_task", task = name);
    tokio::spawn(
        async move {
            // AssertUnwindSafe: we're logging and re-panicking, not recovering.
            // Any shared state is the caller's responsibility.
            let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
            if let Err(panic) = result {
                let msg = panic_message(&panic);
                tracing::error!(task = name, panic = %msg, "spawned task panicked");
                // Resume unwinding so JoinHandle::is_finished() and .await report it.
                std::panic::resume_unwind(panic);
            }
        }
        // Instrument the WRAPPER, not `fut` — the panic error! above also
        // needs the span context.
        .instrument(span),
    )
}

/// Spawn a periodic background task: run `body` every `interval` until
/// `shutdown` fires. The `select!` is `biased;` — shutdown wins over a
/// ready tick deterministically (tokio's `select!` defaults to RANDOM
/// branch choice, which can delay shutdown by up to one interval; see
/// `r[common.task.periodic-biased]`).
///
/// `MissedTickBehavior::Skip` by default: if one `body` call overruns
/// its interval, the next tick fires immediately once, then the
/// interval resynchronizes (no catch-up burst). Override via
/// [`spawn_periodic_with`] if `Burst` or `Delay` is needed.
///
/// The first tick fires immediately (tokio's default). If the caller
/// needs to skip the startup tick, use [`spawn_periodic_with`] with a
/// pre-consumed first tick.
///
/// Panic inside `body` is caught and logged by the [`spawn_monitored`]
/// wrapper (task name in the error). The periodic loop does NOT restart
/// — panic ends the task. If restart-on-panic is wanted, wrap the body
/// in `catch_unwind` at the call site.
///
/// # Closure state
///
/// `body` is `FnMut() -> impl Future`; the returned future cannot
/// borrow from `body`'s captured state across `.await` (the future
/// must be `'static`). For stateless loops the usual pattern is
/// `move || { let x = x.clone(); async move { x.do_thing().await } }`.
/// Stateful loops (cross-tick mutable state like edge-detection
/// `prev: Option<bool>`) should stay inline as `spawn_monitored` with
/// a manual `biased;` `select!` — see the `health-toggle-loop` in
/// `rio-scheduler/src/main.rs` for an example.
// r[impl common.task.periodic-biased]
pub fn spawn_periodic<F, Fut>(
    name: &'static str,
    interval: Duration,
    shutdown: Token,
    body: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    spawn_periodic_with(name, ticker, shutdown, body)
}

/// Like [`spawn_periodic`] but the caller constructs the [`Interval`]
/// (to set a non-default `MissedTickBehavior`, or pre-consume the
/// first tick before the loop starts).
///
/// [`Interval`]: tokio::time::Interval
pub fn spawn_periodic_with<F, Fut>(
    name: &'static str,
    mut ticker: tokio::time::Interval,
    shutdown: Token,
    mut body: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    spawn_monitored(name, async move {
        loop {
            tokio::select! {
                // Deterministic shutdown-wins. Without `biased;`,
                // tokio::select! RANDOMIZES branch choice when multiple
                // arms are ready — a ready tick could beat a ready
                // cancellation, delaying shutdown by one interval.
                biased;
                _ = shutdown.cancelled() => {
                    tracing::debug!(task = name, "periodic task shutting down");
                    break;
                }
                _ = ticker.tick() => {}
            }
            body().await;
        }
    })
}

/// Extract a human-readable message from a panic payload.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// `biased;` means shutdown wins over a ready tick. Without it,
    /// `tokio::select!` picks randomly — under load, shutdown could
    /// lose to tick indefinitely (unlikely but possible).
    ///
    /// Mutation check: drop `biased;` from `spawn_periodic_with` →
    /// this test FAILS (50/50 chance per round that the loop ticks
    /// once more before breaking; across 24 rounds the probability
    /// of NO extra ticks is 2^-24 ≈ 6e-8).
    ///
    /// Mechanics: the body holds a gate (`Notify`) open until the
    /// test has BOTH armed the next interval AND cancelled the
    /// shutdown token. When the gate releases, the loop re-enters
    /// `select!` with BOTH arms ready — biased; picks shutdown;
    /// random picks either.
    // r[verify common.task.periodic-biased]
    #[tokio::test(start_paused = true)]
    async fn spawn_periodic_biased_shutdown_wins() {
        use tokio::sync::Notify;

        const ROUNDS: u32 = 24;
        let mut extra_ticks = 0u32;

        for _round in 0..ROUNDS {
            let shutdown = Token::new();
            let ticks = Arc::new(AtomicU32::new(0));
            let t = Arc::clone(&ticks);
            // Gate the body: it increments tick then awaits. While
            // blocked, we prime both the interval and the cancellation.
            // When released, body returns → loop re-enters select!
            // with BOTH arms ready.
            let gate = Arc::new(Notify::new());
            let g = Arc::clone(&gate);

            let handle = spawn_periodic(
                "test-biased",
                Duration::from_millis(10),
                shutdown.clone(),
                move || {
                    let t = Arc::clone(&t);
                    let g = Arc::clone(&g);
                    async move {
                        t.fetch_add(1, Ordering::Relaxed);
                        g.notified().await;
                    }
                },
            );

            // Drive first tick (immediate) — body blocks on gate.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(ticks.load(Ordering::Relaxed), 1);

            // Prime BOTH arms while body is parked on the gate:
            //   interval: advance past 10ms (next tick ready)
            //   shutdown: cancel (cancelled() ready)
            tokio::time::advance(Duration::from_millis(15)).await;
            shutdown.cancel();

            // Release the gate. body() returns → loop re-enters
            // select! with both arms ready. biased; → shutdown
            // arm wins, break, tick stays at 1. random → ~50%
            // chance tick arm wins → body runs again (tick=2),
            // re-blocks on gate.
            gate.notify_one();
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }

            // If tick went to 2, the body is blocked on gate
            // again — release it so the task can observe shutdown
            // on the NEXT select pass and exit cleanly. Loop:
            // without biased;, each re-entry to select has ~50%
            // chance of choosing tick again.
            let mut safety = 0;
            while !handle.is_finished() {
                gate.notify_one();
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                safety += 1;
                assert!(safety < 100, "task stuck (neither arm chosen?)");
            }

            let final_ticks = ticks.load(Ordering::Relaxed);
            assert!(
                final_ticks >= 1,
                "at least the first immediate tick must have fired"
            );
            extra_ticks += final_ticks - 1;

            handle.await.expect("clean shutdown");
        }

        // With biased; → shutdown ALWAYS wins → extra_ticks == 0.
        // Without biased; → P(extra_ticks == 0) = 2^-ROUNDS ≈ 6e-8.
        assert_eq!(
            extra_ticks, 0,
            "biased; should make shutdown win over ready tick every time; \
             observed {extra_ticks} extra ticks across {ROUNDS} rounds"
        );
    }

    /// Panic inside body is caught by spawn_monitored — logged,
    /// task ends. JoinHandle reports the panic. The periodic loop
    /// does NOT restart.
    #[tokio::test(start_paused = true)]
    async fn spawn_periodic_panic_ends_task() {
        let shutdown = Token::new();
        let ticks = Arc::new(AtomicU32::new(0));
        let t = Arc::clone(&ticks);

        let handle = spawn_periodic(
            "test-panic",
            Duration::from_millis(10),
            shutdown,
            move || {
                let t = Arc::clone(&t);
                async move {
                    if t.fetch_add(1, Ordering::Relaxed) == 2 {
                        panic!("third tick panics");
                    }
                }
            },
        );

        // Drive 5 intervals. The third tick (fetch_add returns 2)
        // panics; subsequent advances don't run the body (task dead).
        for _ in 0..5 {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(Duration::from_millis(11)).await;
        }
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let err = handle.await.unwrap_err();
        assert!(err.is_panic());
        // Exactly 3 ticks ran (0, 1, 2 — panic on 2). Loop did not
        // restart after panic.
        assert_eq!(ticks.load(Ordering::Relaxed), 3);
    }

    /// `MakeWriter` into a shared buffer. `tracing_subscriber::fmt::TestWriter`
    /// goes to stdout — no good for asserting on the bytes.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn take_lines(buf: &BufWriter) -> Vec<String> {
        let bytes = std::mem::take(&mut *buf.0.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[tokio::test]
    async fn test_spawn_monitored_normal_completion() -> anyhow::Result<()> {
        let handle = spawn_monitored("test-normal", async {});
        handle.await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_monitored_panic_reported() {
        let handle = spawn_monitored("test-panic", async {
            panic!("intentional test panic");
        });
        let result = handle.await;
        assert!(
            result.is_err(),
            "JoinHandle should report the panic after logging"
        );
        let err = result.unwrap_err();
        assert!(err.is_panic(), "error should be a panic");
    }

    /// Span fields from the caller's context appear on logs emitted inside
    /// the spawned task. This is how `component` (set on the root span in
    /// each binary's main()) ends up on every log line — including those
    /// from the heartbeat loop, GC sweeper, lease renewer, etc.
    ///
    /// Guard local, not `set_global_default`: the test binary shares a
    /// global slot across all tests in this crate; setting it here would
    /// either race or panic on the second test to hit it.
    ///
    /// `#[tokio::test]` defaults to the current-thread runtime, so the
    /// spawned task runs on the same OS thread that holds the thread-local
    /// `DefaultGuard`. If this test is ever moved to `flavor = "multi_thread"`,
    /// switch to `set_global_default` with a `#[serial]` gate.
    // r[verify obs.log.required-fields]
    #[tokio::test]
    async fn test_spawn_monitored_propagates_span_component() {
        use tracing_subscriber::{fmt, layer::SubscriberExt as _};

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .json()
                // flatten span fields into the top-level JSON object —
                // matches rio-common's production layout (observability.rs)
                // and makes the assertion a straight key lookup.
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(buf.clone()),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        // Enter the span BEFORE spawn_monitored — this is what each binary's
        // main() does with its root span.
        let root = tracing::info_span!("root", component = "test-component");
        let handle = root.in_scope(|| {
            spawn_monitored("propagation-test", async {
                tracing::info!("hello from spawned task");
            })
        });
        handle.await.unwrap();

        let lines = take_lines(&buf);
        // Exactly one INFO line — the "hello" above. No span-open/close
        // events (the fmt layer doesn't emit those by default).
        let line = lines
            .iter()
            .find(|l| l.contains("hello from spawned task"))
            .unwrap_or_else(|| panic!("no log line found; captured: {lines:?}"));

        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        // With with_span_list(true), span fields live under "spans":[{...}].
        // Look there — the exact nesting is a tracing-subscriber detail
        // we don't want to over-fit to. substring on the raw line would
        // pass for `"message":"component"` too; walk the JSON.
        let haystack = parsed.to_string();
        assert!(haystack.contains("test-component"), "parsed: {parsed}");
    }

    /// Negative control: bare `tokio::spawn` does NOT propagate the span.
    /// Proves `test_spawn_monitored_propagates_span_component` actually
    /// detects the bug it's guarding against — without this, a future
    /// refactor that breaks the child-span wiring but coincidentally leaves
    /// `component` in the line (e.g., via a different span) passes silently.
    #[tokio::test]
    async fn test_bare_tokio_spawn_does_not_propagate_span() {
        use tracing_subscriber::{fmt, layer::SubscriberExt as _};

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(buf.clone()),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        let root = tracing::info_span!("root", component = "test-component");
        let handle = root.in_scope(|| {
            // Bare tokio::spawn — NO span capture, NO .instrument().
            tokio::spawn(async {
                tracing::info!("hello from bare spawn");
            })
        });
        handle.await.unwrap();

        let lines = take_lines(&buf);
        let line = lines
            .iter()
            .find(|l| l.contains("hello from bare spawn"))
            .unwrap_or_else(|| panic!("no log line found; captured: {lines:?}"));

        // The component field must NOT appear — the span was not propagated.
        assert!(
            !line.contains("test-component"),
            "bare tokio::spawn should NOT propagate span; got: {line}"
        );
    }
}
