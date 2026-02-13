//! Narinfo text format parser and generator.
//!
//! Narinfo files describe store path metadata for binary caches.
//! Each `.narinfo` contains: StorePath, URL, Compression, FileHash,
//! FileSize, NarHash, NarSize, References, Deriver, Sig.
//!
//! Needed in Phase 2a for the binary cache HTTP server.

// TODO(Phase 2a): Implement narinfo parser/generator.
// See: Nix C++ `NarInfo` in libstore/nar-info.{hh,cc}.
