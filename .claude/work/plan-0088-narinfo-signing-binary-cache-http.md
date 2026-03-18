# Plan 0088: Narinfo signing + binary cache HTTP server

## Design

Three commits building the Nix binary cache protocol: the signing fingerprint format, ed25519 signing at `PutPath` time, and an axum HTTP server serving `/nix-cache-info`, `/{hash}.narinfo`, and `/nar/{narhash}.nar.zst`.

### Narinfo signing fingerprint

The canonical string that ed25519 signatures cover. A client verifying a narinfo `Sig:` line reconstructs this from the narinfo fields, verifies the signature, trusts the path iff a key in `trusted-public-keys` validates. Format: `1;{store_path};sha256:{nixbase32(hash)};{nar_size};{sorted_refs}`. Matches `ValidPathInfo::fingerprint()` in Nix's `path-info.cc` — one wrong separator or encoding and every signature we produce is invalid.

Load-bearing details: nixbase32 NOT hex NOT SRI-base64 (52 chars for SHA-256). FULL store paths in refs, not basenames — narinfo TEXT uses basenames (saves space, store dir implicit), but the fingerprint uses full paths. Easy to confuse if you've been staring at narinfo text. Refs sorted lexicographically (Nix uses `BTreeSet` internally); caller passes unsorted, we sort a clone. Empty refs → nothing after the last semicolon (not `,`, not `{}`).

Free function, not a `NarInfo` method: the signer calls this with fields from `ValidatedPathInfo` (store's internal type). A method would force constructing a `NarInfo` just to sign — wasteful and backwards (signing happens BEFORE building the narinfo text).

### ed25519 signing at PutPath time

Signatures are computed at PutPath-complete time and stored in `narinfo.signatures`. The HTTP server serves them as `Sig:` lines from the DB — never touches the privkey. Means: key rotation doesn't re-sign old paths (they keep their old-key sig, valid as long as the old pubkey is in `trusted-public-keys`), the HTTP server can be a separate less-privileged process, a compromised HTTP server can't forge.

Key file format: `{name}:{base64(secret)}` — same as `nix-store --generate-binary-cache-key`. Accepts both 64-byte (seed+pubkey, Nix's format — we take the first 32) and 32-byte (seed-only). `ed25519-dalek` derives the pubkey from the seed anyway; the stored pubkey is redundant. base64 STANDARD alphabet (not URL_SAFE): Nix's `nix-base64.cc` uses RFC 4648 `+` and `/`, not `-` and `_`. Getting this wrong means every real key file fails to load with "invalid byte" on the first `+`. `Signer::load` trims trailing whitespace: `echo > keyfile` produces `\n`, untrimmed → base64 decode fails cryptically.

`maybe_sign()` in `grpc.rs`: called just before `complete_manifest_*` writes narinfo. References are FULL store paths (`ValidatedPathInfo` stores `StorePath`, stringifies to full paths). No error path: ed25519 signing is pure math on valid inputs, we control all inputs. `StoreServiceImpl::with_signer()` builder-style, chains after `new()` or `with_chunk_backend()`.

Security test `test_putpath_sig_covers_references`: sig verifies against fingerprint WITH ref, does NOT verify against fingerprint WITHOUT ref. If the second assertion passed, refs wouldn't be covered — an attacker could serve a path claiming different dependencies.

### Axum binary cache HTTP server

Three routes serving the standard Nix binary cache protocol. `/nix-cache-info` is static (`StoreDir`, `WantMassQuery: 1`, `Priority: 40`). `/{hash}.narinfo` reuses P0083's `query_by_hash_part` (same `LIKE` lookup, same injection validation). URL in the narinfo points to `nar/{nixbase32(nar_hash)}.nar.zst` — NarHash URL (standard-ish, matches cache.nixos.org). References in narinfo TEXT are basenames (narinfo format), not full paths (fingerprint format) — strip `/nix/store/`.

`/nar/{narhash}.nar.zst`: `nixbase32::decode` the URL segment → 32-byte hash → `path_by_nar_hash` (uses `idx_narinfo_nar_hash` from migration 006) → `get_manifest` → reassemble (inline or chunked, same `buffered(8)` as P0087's `GetPath`) → zstd level 3 in `spawn_blocking` → respond. `spawn_blocking` because a 1GiB NAR at 500MB/s is 2 seconds of CPU; holding an async task starves other requests. TODO(phase3a): streaming compression. Current version buffers the full NAR before compressing — 4GiB NAR = 4GiB peak. Correct but not great.

No privkey here: signatures come from the DB. Compromised HTTP server can serve wrong data but can't forge signatures. `Cache-Control: immutable` (content-addressed). `ETag = hex(nar_hash)`. Only zstd (`.nar.zst`), no `.nar.xz`.

Test gotcha: `TestDb`'s `Drop` deletes the database. `setup()` returned `db.pool.clone()` but dropped `db` → router's pool points at a dead DB. Return `TestDb` itself; tests hold a `_db` binding.

## Files

```json files
[
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "fingerprint() free function: 1;path;sha256:nixbase32;size;sorted_full_refs"},
  {"path": "rio-store/src/signing.rs", "action": "NEW", "note": "Signer: ed25519-dalek, {name}:{base64} key format, STANDARD alphabet, trim trailing whitespace"},
  {"path": "rio-store/src/cache_server.rs", "action": "NEW", "note": "axum: /nix-cache-info, /{hash}.narinfo (reuses query_by_hash_part), /nar/{narhash}.nar.zst (buffered(8) reassembly + zstd in spawn_blocking)"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "maybe_sign() before complete_manifest; with_signer() builder"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pub mod signing, cache_server"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "path_by_nar_hash using idx_narinfo_nar_hash"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "3 signing integration; security test sig-covers-references"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `narinfo.signatures` column, `idx_narinfo_nar_hash` index from migration 006.
- Depends on **P0083**: `query_by_hash_part` reused by narinfo route.
- Depends on **P0087**: chunked reassembly via `get_manifest` + `ChunkCache` for NAR route. (Note: only the third commit `87816d8` needs this; signing commits `8c49724`+`a9aaec7` are independent of chunking.)

## Exit

Merged as `8c49724..87816d8` (3 commits). Tests: 713 → 742 (+29: 5 fingerprint, 13 signing unit, 3 signing integration, 8 cache server).

`tower` dev-dep for `Router::oneshot` (`ServiceExt` trait). `axum` + `zstd` prod deps.
