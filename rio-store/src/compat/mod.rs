//! Stock-Nix binary-cache compatibility layer (ADR-022 Design Overview
//! §10).
//!
//! When `binary_cache_compat.enabled`, every path committed through the
//! buffered upload RPCs is *additionally* published to the S3-standard
//! bucket as a stock-Nix object pair — `{hash-part}.narinfo` +
//! `nar/{file-hash}.nar.<ext>` — so plain `nix` clients substitute
//! straight from the bucket with no rio process running (migration
//! on-ramp + PG-outage floor). The chunked store remains authoritative;
//! compat objects are a derived, regenerable projection.
//!
//! [`writer::CompatWriter`] is the only producer; it runs from two
//! places. The upload handlers call it inline (sync after commit) for
//! the buffered RPCs, and the [`reconciler`] loop backfills every path
//! whose `narinfo.compat_file_hash` is NULL — crash windows, paths
//! ingested while compat was OFF, and paths committed via ingest paths
//! that don't carry the whole NAR in RAM (`PutPathChunked`, upstream
//! substitution).
//!
//! Together those two producers are what make the bucket a valid
//! stock-Nix binary cache on its own — substitutable by a plain `nix`
//! client with no rio process running; the `vm-store-compat` scenario
//! proves that end to end.
// r[impl store.compat.stock-nix-substitute]

pub mod reconciler;
pub mod writer;

pub use reconciler::spawn_reconciler_loop;
pub use writer::{CompatError, CompatWriter};
