//! Push Nix store path closures to rio-store.
//!
//! `rio-push` discovers the closure of one or more Nix store paths,
//! deduplicates against the store via `FindMissingPaths`, and uploads
//! each missing path's NAR concurrently.

pub mod nix;
pub mod upload;
