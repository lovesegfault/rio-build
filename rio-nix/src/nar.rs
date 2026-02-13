//! NAR (Nix ARchive) streaming reader and writer.
//!
//! NAR is a deterministic archive format used by Nix to serialize store paths.
//! It produces identical output regardless of filesystem metadata (timestamps,
//! permissions beyond executable bit, ownership).
//!
//! Needed in Phase 1b for `wopAddToStoreNar` / `wopNarFromPath` content handling.

// TODO(Phase 1b): Implement streaming NAR reader/writer.
// Format: "nix-archive-1" header, then recursive directory/file/symlink entries.
// See: Nix C++ `dumpPath` and `restorePath` in libutil/archive.{hh,cc}.
