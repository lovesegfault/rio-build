//! Chunk storage backend.
//!
//! BLAKE3-addressed chunk storage for the chunked CAS. Three impls: S3
//! (prod), filesystem (dev), memory (tests). See [`chunk::ChunkBackend`].

pub mod chunk;
