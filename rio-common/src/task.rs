//! Task spawning utilities with panic logging.

use std::future::Future;

use futures_util::FutureExt as _;
use tokio::task::JoinHandle;

/// Spawn a task that logs any panic at `error!` level before unwinding.
///
/// By default, `tokio::spawn` silently swallows panics unless the `JoinHandle`
/// is awaited. For long-running background tasks (actor loops, stream readers,
/// tick loops), this means a panic kills the task with zero diagnostic output.
///
/// `spawn_monitored` catches the panic, logs it with the task name and panic
/// message, and then resumes unwinding so the `JoinHandle` still reports the
/// panic to any awaiter.
pub fn spawn_monitored<F>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        // AssertUnwindSafe: we're logging and re-panicking, not recovering.
        // Any shared state is the caller's responsibility.
        let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
        if let Err(panic) = result {
            let msg = panic_message(&panic);
            tracing::error!(
                task = name,
                panic = %msg,
                "spawned task panicked"
            );
            // Resume unwinding so JoinHandle::is_finished() and .await report it.
            std::panic::resume_unwind(panic);
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
    use super::*;

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
}
