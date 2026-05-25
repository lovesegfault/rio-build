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
//! [`writer::CompatWriter`] is the only producer. The deferred
//! reconciler (P0582) backfills paths whose `narinfo.compat_file_hash`
//! is NULL — crash windows, paths ingested while compat was OFF, and
//! paths committed via ingest paths that don't carry the whole NAR in
//! RAM (`PutPathChunked`, upstream substitution).

pub mod writer;

pub use writer::{CompatError, CompatWriter};
