//! Task spawning utilities with panic logging.

use std::future::Future;

use futures_util::FutureExt as _;
use tokio::task::JoinHandle;
use tracing::Instrument as _;

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
/// The current tracing span is propagated across the task boundary. This is
/// why the root span's `component` field appears in logs emitted from
/// background tasks — see `r[obs.log.required-fields]`. `tokio::spawn` does
/// NOT do this on its own; the span must be captured here, before the spawn
/// point, because `Span::current()` inside the spawned future would return
/// `Span::none()`.
pub fn spawn_monitored<F>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Capture BEFORE tokio::spawn — inside the async block, Span::current()
    // is the executor's ambient span (none), not the caller's.
    let span = tracing::Span::current();
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
        // needs the component field.
        .instrument(span),
    )
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
    use std::sync::{Arc, Mutex};

    use super::*;

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
    /// refactor that breaks `.instrument(span)` but coincidentally leaves
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
