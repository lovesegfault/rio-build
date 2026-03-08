# ADR-006: Custom Chunked CAS

## Status
Accepted

## Context
Nix store paths are serialized as NAR archives for transport and storage. A content-addressable store must decide how to deduplicate data. NAR archives for different store paths often share significant content (e.g., common libraries, similar build outputs). Efficient storage and transfer require sub-NAR deduplication.

## Decision
NAR archives are chunked using content-defined chunking (FastCDC). Identical chunks across store paths are stored once. The system has two tiers:

- **Inline fast-path**: NARs below 256KB are stored as a single blob in PostgreSQL (`manifests.inline_blob BYTEA`) with no chunking overhead and no S3 round-trip. Most small derivations (scripts, config files, small libraries) fall into this category.
- **Chunked path**: Larger NARs are split by FastCDC. A chunk manifest in PostgreSQL (`manifest_data.chunk_list`) maps each NAR to its ordered list of chunk references; the chunk bodies themselves are stored in S3.

Blob storage is split: inline NARs in PostgreSQL, FastCDC chunks in S3. Metadata (narinfo, references, chunk manifests, refcounts) is entirely in PostgreSQL.

Hash domains are strictly separated:
- **SHA-256** for all Nix-facing hashes (store path hashes, NAR hashes, output hashes).
- **BLAKE3** for internal chunk addressing only. BLAKE3 is faster for chunking workloads but is not part of the Nix protocol.

## Alternatives Considered
- **Whole-NAR storage (no chunking)**: Simplest approach. Each NAR is stored as a single S3 object. No cross-path deduplication. For large closures with shared libraries, storage costs and transfer times are significantly higher.
- **Borg/restic-style variable chunking with a shared chunk index**: Full deduplication across all paths. However, a global chunk index becomes a write bottleneck and complicates garbage collection. The simpler per-NAR manifest approach is sufficient for store-path-level deduplication.
- **Casync-style chunking**: Similar to the chosen approach but casync is designed for filesystem images, not NAR archives. Its chunk size parameters and index format are not optimized for Nix workloads.
- **Use Nix's built-in NAR hashing only (SHA-256 everywhere)**: Avoid BLAKE3 entirely. Simpler but SHA-256 is ~3x slower than BLAKE3 for chunk hashing. Since chunk hashes are internal and never exposed to Nix, BLAKE3 is a free performance win.

## Consequences
- **Positive**: Significant storage savings for large closures with shared content (e.g., Python environments, Haskell package sets).
- **Positive**: Inline fast-path avoids chunking overhead for small paths, which are the majority by count.
- **Positive**: Strict hash domain separation prevents confusion between Nix-facing and internal hashes.
- **Negative**: Two hash algorithms add implementation complexity.
- **Negative**: Chunk manifest lookups add latency compared to whole-NAR retrieval. Must be mitigated with caching.
