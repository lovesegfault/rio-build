//! Common error types for rio-build components.

/// Errors that can occur during gRPC service operations.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("store path not found: {0}")]
    PathNotFound(String),

    #[error("NAR hash mismatch: expected {expected}, got {actual}")]
    NarHashMismatch { expected: String, actual: String },

    #[error("NAR size mismatch: expected {expected}, got {actual}")]
    NarSizeMismatch { expected: u64, actual: u64 },

    #[error("database error: {0}")]
    Database(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}
