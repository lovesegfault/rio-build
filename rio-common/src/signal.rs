//! Graceful shutdown signal handling.
//!
//! Provides a [`CancellationToken`] that fires on SIGTERM/SIGINT, letting
//! `main()` return normally instead of the default SIGTERM behavior
//! (immediate termination, skipping atexit handlers).
//!
//! This matters for:
//! - LLVM coverage profraw flushing (written via `atexit`)
//! - tracing/OpenTelemetry span flush
//! - in-flight work drain
//!
//! Usage: call [`shutdown_signal()`] once at the top of `main()`, clone the
//! token for each background task, pass `.cancelled_owned()` to tonic's
//! `serve_with_shutdown` / axum's `with_graceful_shutdown`.

use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

// Re-export so downstream crates don't need to add tokio-util directly.
pub use tokio_util::sync::CancellationToken as Token;

/// Returns a token that is cancelled when SIGTERM or SIGINT is received.
///
/// Spawns a background task to listen for signals. Call once near the top
/// of `main()`. Clone the returned token for each background loop;
/// each loop should `select!` on `token.cancelled()` and break when it
/// fires. For tonic/axum servers, pass `token.cancelled_owned()` to
/// `serve_with_shutdown` / `with_graceful_shutdown`.
///
/// # Panics
///
/// Panics if signal handler installation fails (e.g., not in a Unix
/// environment). This is a programming error — signal handling is a
/// hard requirement for binaries using this helper.
pub fn shutdown_signal() -> CancellationToken {
    // Install handlers BEFORE spawning — otherwise there's a race where
    // a signal delivered between return and the spawned task's first poll
    // hits the default handler (terminate). signal() registers the
    // process-wide disposition synchronously; the Signal struct can be
    // moved into the spawned task for async .recv().
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received, shutting down");
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received, shutting down");
            }
        }
        t.cancel();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the token transitions to cancelled after a self-delivered
    /// SIGTERM. Uses a bounded wait to avoid hanging if signal delivery
    /// is broken.
    #[tokio::test]
    async fn test_shutdown_on_sigterm() {
        let token = shutdown_signal();
        assert!(!token.is_cancelled(), "token should start uncancelled");

        // Raise SIGTERM to this process. tokio's signal handler will
        // catch it (installed above), so the default action (terminate)
        // is overridden.
        //
        // SAFETY: raise(3) is always safe — it just queues a signal
        // to the calling thread. No memory safety invariants.
        unsafe {
            libc::raise(libc::SIGTERM);
        }

        // Bound the wait; if this times out, signal delivery is broken.
        tokio::time::timeout(std::time::Duration::from_secs(5), token.cancelled())
            .await
            .expect("token should cancel within 5s of SIGTERM");

        assert!(token.is_cancelled());
    }
}
