//! Nix protocol types and wire format.
//!
//! Provides the core Nix types ([`StorePath`](store_path::StorePath),
//! [`NixHash`](hash::NixHash), [`Derivation`](derivation::Derivation),
//! [`NarInfo`](narinfo::NarInfo)), the worker-protocol wire primitives
//! ([`protocol::wire`]), and handshake/STDERR framing. Fuzzed parsers
//! live in `rio-nix/fuzz/`.

pub mod derivation;
pub mod hash;

pub use derivation::DerivationLike;
pub mod nar;
pub mod narinfo;
pub mod protocol;
pub mod refscan;
pub mod store_path;
