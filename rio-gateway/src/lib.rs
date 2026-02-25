//! SSH gateway and Nix protocol frontend for rio-build.
//!
//! Terminates SSH connections, speaks the Nix worker protocol, and
//! translates protocol operations into gRPC calls to the scheduler
//! and store services.

pub mod handler;
pub mod server;
pub mod session;
pub mod translate;

pub use server::{GatewayServer, load_authorized_keys, load_or_generate_host_key};
