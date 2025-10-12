// Common types and gRPC service definitions for Rio

// Re-export generated protobuf types
pub mod proto {
    tonic::include_proto!("rio.v1");
}

pub mod derivation;
pub mod types;

// Re-export commonly used types
pub use derivation::{DerivationInfo, DerivationOutput};
pub use types::{BuilderId, JobId, Platform};
