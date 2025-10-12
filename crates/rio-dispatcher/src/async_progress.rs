// AsyncProgress helper for wrapping async futures in Progress trait

use nix_daemon::{Progress, Stderr};
use std::future::Future;
use std::marker::PhantomData;

/// A Progress implementation that wraps an async future
pub struct AsyncProgress<F, T, E> {
    future: Option<F>,
    _phantom: PhantomData<(T, E)>,
}

impl<F, T, E> AsyncProgress<F, T, E>
where
    F: Future<Output = Result<T, E>> + Send,
    T: Send,
    E: Send + Sync,
{
    pub fn new(future: F) -> Self {
        Self {
            future: Some(future),
            _phantom: PhantomData,
        }
    }
}

impl<F, T, E> Progress for AsyncProgress<F, T, E>
where
    F: Future<Output = Result<T, E>> + Send,
    T: Send,
    E: From<nix_daemon::Error> + Send + Sync,
{
    type T = T;
    type Error = E;

    async fn next(&mut self) -> Result<Option<Stderr>, Self::Error> {
        // No intermediate progress messages
        Ok(None)
    }

    async fn result(mut self) -> Result<Self::T, Self::Error> {
        self.future
            .take()
            .expect("result() called twice on AsyncProgress")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_async_progress_success() {
        let future = async { Ok::<i32, anyhow::Error>(42) };
        let mut progress = AsyncProgress::new(future);

        // next() should return None (no intermediate messages)
        let next = progress.next().await.unwrap();
        assert_eq!(next, None);

        // result() should return the value
        let result = progress.result().await.unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_async_progress_error() {
        let future = async { Err::<i32, anyhow::Error>(anyhow::anyhow!("test error")) };
        let progress = AsyncProgress::new(future);

        // result() should return the error
        let result = progress.result().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test error"));
    }

    #[tokio::test]
    async fn test_async_progress_with_async_work() {
        let future = async {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            Ok::<String, anyhow::Error>("completed".to_string())
        };

        let progress = AsyncProgress::new(future);
        let result = progress.result().await.unwrap();
        assert_eq!(result, "completed");
    }

    #[tokio::test]
    async fn test_async_progress_next_then_result() {
        let future = async { Ok::<i32, anyhow::Error>(100) };
        let mut progress = AsyncProgress::new(future);

        // Call next() multiple times
        assert_eq!(progress.next().await.unwrap(), None);
        assert_eq!(progress.next().await.unwrap(), None);

        // Then get result
        let result = progress.result().await.unwrap();
        assert_eq!(result, 100);
    }
}
