// Common types and gRPC service definitions for Rio

// Re-export generated protobuf types
pub mod proto {
    tonic::include_proto!("rio.v1");
}

pub mod types;

// Re-export commonly used types
pub use types::{BuilderId, JobId, Platform};
