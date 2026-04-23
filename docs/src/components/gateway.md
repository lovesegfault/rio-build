# rio-gateway

The gateway is the entry point. It terminates SSH connections and speaks the Nix worker protocol, making rio-build appear as a standard Nix remote store/builder.

## Responsibilities

- SSH server via `russh` crate --- accepts connections, authenticates via SSH keys
- Implement the Nix worker protocol (version negotiation, opcode handling)
- Handle both remote store mode (full DAG submission) and build hook mode (per-derivation delegation)
- STDERR streaming loop: send `STDERR_NEXT`, `STDERR_START_ACTIVITY`, `STDERR_STOP_ACTIVITY`, `STDERR_RESULT`, `STDERR_LAST` during operations
- Translate protocol ops into internal gRPC calls to scheduler and store
- Each SSH channel maintains independent protocol state (separate handshake and option negotiation)

## Network reachability

r[gw.ingress.v6-direct]

The gateway MUST be reachable from an IPv6-only client over the cluster's IPv6 NodePort with no translation layer.

r[gw.ingress.v4-via-nat]

The gateway MUST be reachable from an IPv4-only client via an external v4→v6 translator (AWS NLB `enable-prefix-for-ipv6-source-nat`, or equivalent). rio does not implement this translation; it is an infrastructure requirement.

## Critical Opcodes

r[gw.opcode.mandatory-set]
The opcodes below are the mandatory implementation set for a working `ssh-ng://` store. Each has a dedicated wire-format section below.

| Opcode | Value | Description |
|--------|-------|-------------|
| `wopIsValidPath` | 1 | Check if a store path exists |
| `wopAddToStore` | 7 | Legacy content-addressed store path import |
| `wopAddTextToStore` | 8 | Legacy text file import (builtins.toFile) |
| `wopBuildPaths` | 9 | Build a set of derivations |
| `wopEnsurePath` | 10 | Ensure a store path is valid/available |
| `wopAddTempRoot` | 11 | Add temporary GC root |
| `wopSetOptions` | 19 | Accept client build configuration |
| `wopQueryPathInfo` | 26 | Return full path metadata |
| `wopQueryPathFromHashPart` | 29 | Resolve a store path from its hash prefix |
| `wopQueryValidPaths` | 31 | Batch validity check |
| `wopBuildDerivation` | 36 | Build a single derivation |
| `wopAddSignatures` | 37 | Add signatures to a path |
| `wopNarFromPath` | 38 | Export path as NAR |
| `wopAddToStoreNar` | 39 | Accept NAR imports |
| `wopQueryMissing` | 40 | Report what needs building |
| `wopQueryDerivationOutputMap` | 41 | Get output name -> path mapping |
| `wopRegisterDrvOutput` | 42 | Register CA derivation output |
| `wopQueryRealisation` | 43 | Query CA realisation |
| `wopAddMultipleToStore` | 44 | Batch NAR import |
| `wopBuildPathsWithResults` | 46 | Build paths and return results |

### wopSetOptions (19) Field Sequence

r[gw.opcode.set-options.field-order]
The fields are sent in order, all as `u64` unless noted. The **daemon-protocol** client (`ssh://`) sends `wopSetOptions` as the first opcode after handshake. The **ssh-ng** client does NOT send it (empirically verified P0215) --- `SSHStore::setOptions()` is an empty override. Client-side `--max-silent-time`/`--timeout` are silently non-functional over ssh-ng; see Override propagation below for the gateway-side fallback path.

1. `keepFailed` (u64 bool)
2. `keepGoing` (u64 bool)
3. `tryFallback` (u64 bool)
4. `verbosity` (u64)
5. `maxBuildJobs` (u64)
6. `maxSilentTime` (u64)
7. `obsolete_useBuildHook` (u64: always 1)
8. `verboseBuild` (u64 --- Verbosity level: `lvlError`=0 means true, `lvlVomit`=7 means false; daemon decodes via `lvlError == readInt()`)
9. `obsolete_logType` (u64: 0)
10. `obsolete_printBuildTrace` (u64: 0)
11. `buildCores` (u64)
12. `useSubstitutes` (u64 bool)
13. `overrides_count` (u64) followed by `overrides_count` pairs of `(key: string, value: string)` --- always present since the minimum accepted client version is 1.35

r[gw.opcode.set-options.propagation+2]
**Override propagation:** The `overrides` key-value pairs contain client build settings. The gateway extracts relevant overrides and propagates them through the build pipeline: gateway -> scheduler (via gRPC) -> workers. **NOT reachable via `ssh-ng://`** --- Nix `SSHStore` overrides `RemoteStore::setOptions()` with an empty body (unchanged since 088ef8175, 2018-03-05; intentional, see NixOS/nix#1713/#1935), so `wopSetOptions` never hits the wire for ssh-ng clients. All `--option` flags are silently dropped client-side. This opcode fires only for `unix://` daemon-socket clients, which is not rio's production path. See `r[sched.timeout.per-build]` for the gRPC-only reachability of `build_timeout`. Upstream fix NixOS/nix 32827b9fb adds selective ssh-ng forwarding but requires the daemon to advertise a `set-options-map-only` protocol feature that rio-gateway does not implement.

### wopNarFromPath (38) Wire Format

r[gw.opcode.nar-from-path]
Exports a store path as a NAR archive.

| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `path` | string | Store path to export |

r[gw.opcode.nar-from-path.raw-bytes]
**Behavior:** rio-gateway sends `STDERR_LAST` to close the stderr loop, then streams the raw NAR bytes directly on the connection (no framing, no length prefix). This matches the canonical nix-daemon behavior. The Nix client's `copyNAR()` reads until the NAR is complete.

**Historical note (bug #11):** Earlier phases sent the NAR inside the stderr loop via `STDERR_WRITE` chunks, described in this document as an "intentional divergence". That was wrong — the Nix client's `processStderr()` for this opcode passes no sink, so `STDERR_WRITE` frames caused `error: no sink`. Fixed to `STDERR_LAST` + raw bytes. See `handle_nar_from_path` in `rio-gateway/src/handler/opcodes_read.rs`.

### wopAddToStore (7) Wire Format

Legacy content-addressed store path import. The client sends a name, a content-address method string, references, and the raw file contents (or NAR) as a framed stream. The server computes the store path, wraps non-recursive data in a NAR, and returns the full `ValidPathInfo`.

| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `name` | string | Store path name component |
| C -> S | `camStr` | string | Content-address method (see CAM formats below) |
| C -> S | `references` | string collection | Referenced store paths |
| C -> S | `repair` | u64 bool | Whether to repair/overwrite (read and discarded) |
| C -> S | `dump` | framed byte stream | Raw file contents (flat) or NAR bytes (recursive) |

**Content-address method (`camStr`) formats:**

| Format | Meaning |
|--------|---------|
| `text:sha256` | Text import (builtins.toFile-style); hash is over raw bytes; store path via `makeTextPath` |
| `fixed:sha256` | Flat fixed-output; hash is over raw bytes; gateway wraps in a single-file NAR |
| `fixed:r:sha256` | Recursive fixed-output; dump IS a NAR; hash is over the NAR bytes |
| `fixed:git:sha1` | Git tree import; **rejected** (not supported — would compute wrong store path) |

r[gw.opcode.add-to-store.cam-git-rejected]
The `fixed:git:` content-address method is rejected with `STDERR_ERROR`. Git ingestion is a distinct `FileIngestionMethod` in Nix (inner fingerprint `"fixed:out:git:..."`, CA `"fixed:git:..."`); collapsing it into recursive mode would silently produce a different store path than the client computed.

Response (after `STDERR_LAST`) is a full `ValidPathInfo`:

| Field | Type | Description |
|-------|------|-------------|
| `path` | string | Computed store path |
| `deriver` | string | Always empty |
| `narHash` | string | SHA-256 hash of the NAR (hex-encoded digest, no prefix) |
| `references` | string collection | Echoed references |
| `registrationTime` | u64 | Always 0 |
| `narSize` | u64 | NAR size in bytes |
| `ultimate` | u64 bool | Always 1 (trusted source) |
| `sigs` | string collection | Always empty |
| `ca` | string | Content address: `text:sha256:<nixbase32>` or `fixed:[r:]<algo>:<nixbase32>` |

### wopAddToStoreNar (39) Wire Format

r[gw.opcode.add-to-store-nar+2]
For protocol >= 1.25 (always present since we target 1.35+):

| Field | Type | Description |
|-------|------|-------------|
| `path` | string | Store path being imported |
| `deriver` | string | Deriver path (empty if unknown) |
| `narHash` | string | SHA-256 hash of the NAR (hex-encoded digest, no algorithm prefix) |
| `references` | string collection | Referenced store paths |
| `registrationTime` | u64 | Registration timestamp |
| `narSize` | u64 | Size of the NAR in bytes |
| `ultimate` | u64 bool | Whether this is the ultimate trusted source |
| `sigs` | string collection | Signatures |
| `ca` | string | Content address (empty for input-addressed) |
| `repair` | u64 bool | Whether to repair/overwrite existing path |
| `dontCheckSigs` | u64 bool | Skip signature verification (read and discarded by rio-gateway; signature enforcement, if any, is delegated to rio-store) |

r[gw.opcode.add-to-store-nar.framing+2]
After sending the metadata fields, the NAR data is transferred as a **framed byte stream** (protocol >= 1.23, always true for 1.35+):

1. Client sends framed data: sequence of `u64(chunk_len) + chunk_bytes`, terminated by `u64(0)` sentinel
2. Chunk data is NOT padded (unlike string encoding)
3. Server sends `STDERR_LAST` (`0x616c7473`) — no result value follows

> **Correction (discovered during implementation review):** The original design described a `STDERR_READ` pull loop for NAR data transfer. This is only used for protocol versions 1.21-1.22. For protocol >= 1.23, the Nix C++ daemon uses `FramedSource` (in the `wopAddToStoreNar` handler's `protoVersion >= 1.23` branch in `daemon.cc`), and the client sends data via `FramedSink` (in `RemoteStore::addToStore`). The framed stream format is the same as used by `wopAddMultipleToStore`. Additionally, the original design omitted the `dontCheckSigs` field and incorrectly included a `u64(1)` result value after `STDERR_LAST`.

### wopAddMultipleToStore (44) Wire Format

r[gw.opcode.add-multiple.batch+2]
Added in protocol 1.32 (always present for 1.35+). This is the primary upload path for modern Nix clients, replacing per-item `wopAddToStoreNar` for source paths.

| Field | Type | Description |
|-------|------|-------------|
| `repair` | u64 bool | Whether to repair/overwrite |
| `dontCheckSigs` | u64 bool | Skip signature verification (see note below) |

r[gw.opcode.add-multiple.unaligned-frames]
Followed by a **framed byte stream** containing a count prefix and all entries concatenated. The framed stream is a byte transport --- **entry boundaries do not align with frame boundaries**. A single frame may contain the end of one entry and the beginning of the next, or an entry may span multiple frames. The receiver must:

1. Reassemble frames into a contiguous byte stream
2. Read `num_paths: u64` from the start of the reassembled stream
3. Parse `num_paths` entries sequentially

The reassembled stream begins with:

| Field | Type | Description |
|-------|------|-------------|
| `num_paths` | u64 | Number of entries that follow (MUST be bounds-checked against `MAX_COLLECTION_COUNT`) |

Each entry in the reassembled stream contains:

| Field | Type | Description |
|-------|------|-------------|
| `pathInfo` | (same 9 fields as wopAddToStoreNar metadata, minus the trailing `repair` and `dontCheckSigs` flags) | Path metadata |
| NAR data | `narSize` plain bytes | The NAR content — **NOT nested-framed**; read `narSize` bytes directly from the reassembled outer stream |

The outer framed stream terminates with a `u64(0)` sentinel.

> **Correction (discovered via VM test):** Earlier versions of this spec described the per-entry NAR as an inner framed stream and omitted the `num_paths` prefix. Both were wrong. Nix's `Store::addMultipleToStore(Source &)` reads `num_paths` first (`readNum<uint64_t>(source)`) and then for each entry calls `addToStore(info, source)` which reads `narSize` plain bytes directly. The bug was masked by a byte-level test written to match the buggy parser rather than the spec.

r[gw.opcode.add-multiple.dont-check-sigs-ignored]
**`dontCheckSigs` handling:** The gateway reads and discards `dontCheckSigs`. The gateway does not perform signature verification itself; signature enforcement (if any) is delegated to rio-store. The field is consumed to maintain wire compatibility.

**Response:** The server sends `STDERR_LAST` with no result value (matching the `wopAddMultipleToStore` handler's `logger->stopWork()` sequence in `daemon.cc`).

### DerivedPath Wire Format

r[gw.wire.derived-path]
`DerivedPath` is used by `wopBuildPaths` (9) and `wopBuildPathsWithResults` (46) to specify what to build. It is sent as a single string that the server must parse. There are three forms:

| Form | Syntax | Example | Description |
|------|--------|---------|-------------|
| Opaque | plain store path | `/nix/store/abc...-foo` | Build/fetch this exact path |
| Built (explicit outputs) | `drvPath!output1,output2` | `/nix/store/abc...-foo.drv!out,dev` | Build specific outputs of a derivation |
| Built (all outputs) | `drvPath!*` | `/nix/store/abc...-foo.drv!*` | Build all outputs of a derivation |

The `!*` form is the **default** used by `nix build`. When a client runs `nix build /nix/store/abc...-foo.drv`, it sends the `drvPath!*` form.

Both `wopBuildPaths` and `wopBuildPathsWithResults` send a `string collection` of `DerivedPath` values. The gateway must parse each string to determine the form and extract the derivation path and requested outputs.

### wopBuildDerivation (36) -- BasicDerivation Wire Format

r[gw.opcode.build-derivation+2]
Sends an inline `BasicDerivation` (without `inputDrvs`). For protocol 1.35+:

| Field | Type | Description |
|-------|------|-------------|
| `drvPath` | string | The `.drv` store path |
| `outputs` | collection of output tuples | See below |
| `inputSrcs` | string collection | Input source store paths |
| `platform` | string | e.g. `x86_64-linux` |
| `builder` | string | Builder executable path |
| `args` | string collection | Builder arguments |
| `env` | string-pair collection | Environment variables |
| `buildMode` | u64 | 0=Normal, 1=Repair, 2=Check |

Each **output tuple** (protocol >= 1.32):

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Output name (e.g. `out`, `dev`) |
| `path` | string | Output store path |
| `hashAlgo` | string | Hash algorithm for CA outputs (empty for input-addressed) |
| `hash` | string | Expected hash for CA outputs (empty for input-addressed) |

**DAG reconstruction:** The gateway cannot reconstruct the dependency DAG from `BasicDerivation` alone (it has no `inputDrvs`). The gateway reconstructs the full DAG by parsing the `.drv` files uploaded in the preceding `wopAddToStoreNar`/`wopAddMultipleToStore` step. Each `.drv` file contains `inputDrvs` references that form the DAG edges.

### wopQueryDerivationOutputMap (41) Wire Format

r[gw.opcode.query-derivation-output-map]
**Important:** This opcode is called by all modern Nix clients unconditionally, not just for CA derivations. For input-addressed derivations, it must return the statically-known output paths (computable from the derivation itself).

**Resolution strategy:** The gateway computes the output map locally from the parsed `.drv` file (obtained from the per-session `.drv` cache built during `wopAddToStoreNar`/`wopAddMultipleToStore`, or fetched from rio-store if the `.drv` was uploaded in a previous session). For input-addressed derivations, the output paths are deterministic and computed from the derivation's ATerm representation. For CA derivations (Phase 5), the gateway first checks rio-store for realized output paths via `QueryPathInfo`; if unknown, it returns the placeholder output paths from the `.drv`.

| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `drvPath` | string | The `.drv` store path to query |
| S -> C | `count` | u64 | Number of output mappings |
| S -> C | (per output) `name` | string | Output name |
| S -> C | (per output) `path` | string | Output store path |

### wopIsValidPath (1) Wire Format

r[gw.opcode.is-valid-path]
| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `path` | string | Store path to check |

Response (after STDERR loop):

| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| S -> C | `valid` | u64 bool | 1 if path exists in store, 0 otherwise |

### wopQueryPathInfo (26) Wire Format

r[gw.opcode.query-path-info]
| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `path` | string | Store path to query |

Response (after STDERR loop). First, a validity flag:

| Field | Type | Description |
|-------|------|-------------|
| `valid` | u64 bool | 1 if path exists, 0 if not (stop here if 0) |

If `valid == 1`, the following fields are sent in order:

| Field | Type | Description |
|-------|------|-------------|
| `deriver` | string | Deriver path (empty if unknown) |
| `narHash` | string | NAR hash (hex-encoded digest, no algorithm prefix) |
| `references` | string collection | Referenced store paths |
| `registrationTime` | u64 | Registration timestamp |
| `narSize` | u64 | NAR size in bytes |
| `ultimate` | u64 bool | Whether this is the ultimate source |
| `sigs` | string collection | Signatures |
| `ca` | string | Content address (empty for input-addressed) |

### wopQueryValidPaths (31) Wire Format

r[gw.opcode.query-valid-paths]
| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `paths` | string collection | Store paths to check |
| C -> S | `substitute` | u64 bool | Whether to attempt substitution for missing paths (ignored by rio-build) |

Response (after STDERR loop):

| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| S -> C | `validPaths` | string collection | Subset of input paths that exist in the store |

### wopBuildPaths (9) Wire Format

r[gw.opcode.build-paths]
| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `paths` | string collection | `DerivedPath` values (see DerivedPath Wire Format above) |
| C -> S | `buildMode` | u64 | 0=Normal, 1=Repair, 2=Check |

Response (after STDERR loop): `u64(1)` for success. On failure, the STDERR loop includes `STDERR_ERROR`.

**Note:** Unlike `wopBuildPathsWithResults`, this opcode does NOT return per-path `BuildResult` structures.

### wopQueryMissing (40) Wire Format

r[gw.opcode.query-missing]
| Direction | Field | Type | Description |
|-----------|-------|------|-------------|
| C -> S | `paths` | string collection | `DerivedPath` values to check |

Response (after STDERR loop):

| Field | Type | Description |
|-------|------|-------------|
| `willBuild` | string collection | Store paths that need building |
| `willSubstitute` | string collection | Store paths that can be substituted (populated from `FindMissingPathsResponse.substitutable_paths` when the tenant has upstream caches configured — see [store.substitute.upstream](store.md#upstream-cache-substitution)) |
| `unknown` | string collection | Store paths with unknown status |
| `downloadSize` | u64 | Estimated download size in bytes |
| `narSize` | u64 | Estimated total NAR size in bytes |

### wopBuildPathsWithResults (46) Response Wire Format

r[gw.opcode.build-paths-with-results]
`wopBuildPathsWithResults` (opcode 46) returns one `KeyedBuildResult` per requested path --- the key echoes the `DerivedPath` string the client sent. Response structure (after the STDERR loop):

| Field | Type | Description |
|-------|------|-------------|
| `count` | u64 | Number of result entries |
| (per entry) `derivedPath` | string | The `DerivedPath` string exactly as the client sent it |
| (per entry) `buildResult` | BuildResult | See `BuildResult` format below |

#### BuildResult Wire Format

All fields below are present for 1.35+ except `cpu_user`/`cpu_system`, which are gated on protocol >= 1.37 (Lix at 1.35 omits them):

| Field | Type | Description |
|-------|------|-------------|
| `status` | u64 | See status enum below |
| `errorMsg` | string | Error message (empty on success) |
| `timesBuilt` | u64 | Number of times this derivation was built |
| `isNonDeterministic` | u64 bool | Whether non-deterministic output was detected |
| `startTime` | u64 | Build start time (Unix epoch) |
| `stopTime` | u64 | Build stop time (Unix epoch) |
| `cpuUser` | optional i64 | CPU user time (u64 tag: 0=absent, 1=present; if present, followed by u64 value interpreted as i64) |
| `cpuSystem` | optional i64 | CPU system time (same encoding as cpuUser) |
| `builtOutputs` | collection | Output entries (see below) |

**BuildResult status enum:**

| Value | Name | Description |
|-------|------|-------------|
| 0 | Built | Successfully built |
| 1 | Substituted | Fetched from substituter |
| 2 | AlreadyValid | Output already existed |
| 3 | PermanentFailure | Build failed (not retryable) |
| 4 | InputRejected | Input was rejected |
| 5 | OutputRejected | Output was rejected |
| 6 | TransientFailure | Build failed (may succeed on retry) |
| 7 | CachedFailure | Previously recorded failure |
| 8 | TimedOut | Build exceeded timeout |
| 9 | MiscFailure | Other failure |
| 10 | DependencyFailed | A dependency failed |
| 11 | LogLimitExceeded | Build log exceeded size limit |
| 12 | NotDeterministic | Non-deterministic output detected |
| 13 | ResolvesToAlreadyValid | Derivation resolves to already valid output |
| 14 | NoSubstituters | No substituters available |

Each **builtOutput** entry is a `(DrvOutput, Realisation)` pair:

| Field | Type | Description |
|-------|------|-------------|
| `drv_output_id` | string | DrvOutput key, e.g. `sha256:abcdef...!out` |
| `realisation_json` | string | Realisation as JSON: `{"id":"...","outPath":"...","signatures":[],"dependentRealisations":{}}` |

## Wire Format

r[gw.wire.all-ints-u64]
All wire integers are 64-bit unsigned little-endian. There are **no exceptions** --- even logically-u8 values (BuildStatus, verbosity) and magic bytes are sent as u64 LE.

r[gw.wire.string-encoding]
Strings/buffers: `u64(len) + bytes + zero-pad-to-8-byte-boundary`. Empty strings: `u64(0)` with no bytes and no padding.

r[gw.wire.collection-max]
Collections: `u64(count) + elements`. **Every** count-prefixed loop MUST enforce `MAX_COLLECTION_COUNT` before entering the loop --- not just in `read_strings`/`read_string_pairs`, but in any custom reader (e.g., `read_basic_derivation` output loop, `read_build_result` built-outputs loop, STDERR trace/field readers).

r[gw.wire.framed-no-padding]
Framed data (for NARs): sequence of `u64(chunk_len) + chunk_data` terminated by `u64(0)` --- chunk data is NOT padded (unlike strings).

r[gw.wire.framed-max-total+3]
`MAX_FRAMED_TOTAL` MUST be `>= MAX_NAR_SIZE`. The gateway's
`wopAddToStoreNar` handler gates on `nar_size ≤ MAX_NAR_SIZE` before
constructing the `FramedStreamReader`; if `MAX_FRAMED_TOTAL <
MAX_NAR_SIZE`, the reader's internal clamp silently shrinks the
effective limit, causing NARs between the two bounds to fail
mid-stream with a confusing framed-total error instead of the upfront
size-gate message. A `const_assert!(MAX_FRAMED_TOTAL >= MAX_NAR_SIZE)`
enforces the inequality at compile time. (The two are currently equal
at 4 GiB, but the assertion is `>=` so raising `MAX_FRAMED_TOTAL`
alone is permitted.) `wopAddMultipleToStore` uses an *unbounded*
`FramedStreamReader` (`new_unbounded`) — the per-entry `nar_size ≤
MAX_NAR_SIZE` check inside the de-framed stream and `num_paths ≤
MAX_COLLECTION_COUNT` are the DoS gates; the aggregate is unbounded by
design (closures legitimately exceed 4 GiB; per-frame ≤
`MAX_FRAME_SIZE` and streaming processing keep memory bounded
regardless).

r[gw.wire.narhash-hex]
`narHash` fields on the wire are hex-encoded SHA-256 digests with **no algorithm prefix and no nixbase32**. Use `hex::decode` + `NixHash::new`, not `NixHash::parse_colon`. The `sha256:nixbase32` format appears in narinfo text, not on the wire.

- All integers: 64-bit unsigned, little-endian
- Strings/buffers: `u64(len) + bytes + zero-pad-to-8-byte-boundary`
- Empty strings: `u64(0)` with no bytes and no padding
- Collections: `u64(count) + elements`
- Framed data (for NARs): sequence of `u64(chunk_len) + chunk_data` terminated by `u64(0)` --- chunk data is NOT padded (unlike strings)

### Handshake Sequence (Protocol 1.38+)

> **Correction (discovered during implementation):** The original design stated that magic bytes are u32, the only exception to the u64 rule. This is incorrect. In the actual Nix C++ source, `readInt()` / `writeInt()` serialize all integers as u64 LE, **including the magic bytes**. The handshake uses u64 throughout, with no exceptions.

r[gw.handshake.phases]
The handshake has three phases: magic+version exchange (`BasicClientConnection::handshake`), feature exchange (protocol >= 1.38), and post-handshake (`postHandshake`).

r[gw.handshake.magic]
**Phase 1: Magic + Version Exchange**

| Step | Direction | Data | Type |
|------|-----------|------|------|
| 1 | C -> S | `WORKER_MAGIC_1` (`0x6e697863`) | u64 |
| 2 | S -> C | `WORKER_MAGIC_2` (`0x6478696f`) | u64 |
| 3 | S -> C | Protocol version (encoded as `(major << 8) \| minor`, e.g. `0x126` = 1.38) | u64 |
| 4 | C -> S | Client protocol version | u64 |

r[gw.handshake.version-negotiation+2]
The negotiated version is `min(client_version, server_version)`. If the client version < 1.35, the server should send `STDERR_ERROR` and close the connection.

r[gw.handshake.features]
**Phase 2: Feature Exchange (protocol >= 1.38)**

| Step | Direction | Data | Type |
|------|-----------|------|------|
| 5 | C -> S | Client feature set | string collection |
| 6 | S -> C | Server feature set | string collection |

The feature sets are intersected to determine the negotiated features. rio-build currently advertises an empty feature set.

**Phase 3: Post-Handshake (`postHandshake`)**

| Step | Direction | Data | Type |
|------|-----------|------|------|
| 7 | C -> S | Obsolete CPU affinity (always 0; if non-zero, followed by a second u64 mask) | u64 |
| 8 | C -> S | `reserveSpace` (always 0) | u64 |
| 9 | S -> C | Nix version string (e.g. `"rio-gateway 0.1.0"`) | string |
| 10 | S -> C | Trusted status: 0 = unknown, 1 = trusted, 2 = not-trusted | u64 |

r[gw.handshake.initial-stderr-last]
**Phase 4: Initial STDERR_LAST**

| Step | Direction | Data | Type |
|------|-----------|------|------|
| 11 | S -> C | `STDERR_LAST` (`0x616c7473`) | u64 |

The client calls `processStderrReturn()` after the handshake, which reads messages until `STDERR_LAST`. The server must send `STDERR_LAST` to complete the handshake before the client will send any opcodes.

r[gw.handshake.flush-points]
> **Note on flush points:** The server must flush after steps 2-3, after step 6, after steps 9-10, and after step 11. Without explicit flushes, data may remain buffered and the client will block waiting for the response.

r[gw.handshake.timeout]
The gateway MUST bound the pre-handshake read with a timeout (default 30s) and close the channel on expiry. An authenticated client that opens a channel and sends the SSH `exec_request` but never sends `WORKER_MAGIC_1` would otherwise hold a session slot indefinitely (russh keepalives keep the transport alive, and the inter-opcode idle timeout only applies after the handshake completes).

## Protocol Multiplexing

r[gw.conn.sequential]
The Nix worker protocol is strictly sequential within a single connection --- the client sends a request, waits for the full response (including the STDERR streaming loop), then sends the next request. There is no pipelining or out-of-order execution.

r[gw.conn.per-channel-state]
Multiple clients require multiple SSH channels or connections. The gateway multiplexes at the SSH channel level (one protocol session per channel), not at the protocol level. Each SSH channel has independent protocol state, including separate handshake and option negotiation. During long `wopBuildDerivation` calls, the connection is blocked (the STDERR loop runs for the duration of the build). Nix handles this by opening separate SSH channels for concurrent operations (e.g., IFD during evaluation).

## DAG Reconstruction

r[gw.dag.reconstruct+2]
When the gateway receives `wopBuildDerivation`, `wopBuildPaths`, or `wopBuildPathsWithResults`, it reconstructs the full derivation DAG to send to the scheduler via `SubmitBuild`. `wopBuildDerivation` (build-hook path) attempts the same full-DAG walk: it resolves the `.drv` from the session cache or the store and runs `reconstruct_dag`; only if resolution fails does it fall back to a single-node DAG built from the inline `BasicDerivation` (see `r[gw.hook.single-node-dag]`). The algorithm:

1. **During store uploads:** The gateway intercepts each uploaded path. If the path ends in `.drv`, the gateway extracts the `.drv` file from the NAR, parses the ATerm-format derivation, and caches the parsed result in per-session memory (keyed by store path). This applies to all upload opcodes: `wopAddToStore` (7), `wopAddTextToStore` (8), `wopAddToStoreNar` (39), and `wopAddMultipleToStore` (44). For `wopAddToStoreNar` and `wopAddMultipleToStore`, the handler branches on the path name: `.drv` paths (small --- typically <10KB, capped at `DRV_NAR_BUFFER_LIMIT` = 16MiB) are buffered and parsed via `try_cache_drv`; non-`.drv` paths stream directly to the store via `grpc_put_path_streaming` without buffering. `wopAddToStore` and `wopAddTextToStore` still buffer (they compute the store path from the content hash, so need the full bytes before `PutPath` metadata). A `.drv` NAR exceeding `DRV_NAR_BUFFER_LIMIT` is streamed without caching --- `resolve_derivation` fetches it from the store later during DAG reconstruction.
2. **On `wopBuildDerivation`/`wopBuildPathsWithResults`:** The gateway identifies all requested derivation paths. For each, it looks up the parsed derivation from the session cache (step 1). If a `.drv` was not uploaded in the current session (e.g., it was uploaded in a previous session and already exists in the store), the gateway fetches it from rio-store via `GetPath`, unpacks the NAR, and parses the ATerm.
3. **DAG construction:** Starting from the requested derivation(s), the gateway walks `inputDrvs` references recursively (BFS) to build the full DAG. **DAG reconstruction is capped at 1,048,576 transitive input derivations** (`DEFAULT_MAX_TRANSITIVE_INPUTS`, overridable via `RIO_MAX_TRANSITIVE_INPUTS`) to prevent DoS via pathological derivation graphs. The gateway sends the **full DAG** to the scheduler; cache-hit determination (which nodes have outputs already in the store) happens in the scheduler, not here.
4. **Validation (`validate_dag`):** Malformed `.drv` files and missing `.drv` files (referenced by `inputDrvs` but not in the store) are rejected via `BuildResult::failure` delivered through `STDERR_LAST` — the session stays open, subsequent opcodes are accepted. (Previously `STDERR_ERROR` terminal; changed in remediation-07 to avoid the ERROR→LAST desync when called from `wopBuildPaths`/`wopBuildPathsWithResults`, which wrap the error.) `validate_dag` also enforces two early rejections before the gRPC round-trip: (a) `nodes.len() > MAX_DAG_NODES` (scheduler enforces this too, but gateway-side early reject saves the submission), and (b) any derivation with `__noChroot=1` in its env (sandbox escape --- this check is ONLY at the gateway; the scheduler does not re-check). `validate_dag` is invoked from all three build handlers (`wopBuildDerivation`, `wopBuildPaths`, `wopBuildPathsWithResults`).
5. **The reconstructed DAG is sent to the scheduler via `SubmitBuild`.** The gateway holds the SSH connection open and converts the `BuildEvent` response stream into STDERR messages for the Nix client.

r[gw.reject.nochroot]
The gateway MUST reject any derivation (at SubmitBuild time) whose env contains `__noChroot = "1"`. This is a sandbox-escape request that rio-build does not honor. Rejection happens at two points with different frame semantics: (1) `validate_dag` rejects via `BuildResult::failure` → `STDERR_LAST` (opcodes 36/46 wrap the error; session stays open); (2) `wopBuildDerivation`'s inline check sends `STDERR_ERROR` terminal (opcode 9 doesn't wrap — this is a protocol-level reject). The scheduler does not see the `__noChroot` env (DerivationNode doesn't carry it), so this check is gateway-only.

r[gw.reject.build-mode]
The gateway MUST reject `wopBuildDerivation` / `wopBuildPaths` / `wopBuildPathsWithResults` with `STDERR_ERROR` when `build_mode ≠ Normal`. Repair/Check semantics are not representable in `SubmitBuildRequest`; silently downgrading to `Normal` yields a false-positive reproducibility (`--rebuild`) or repair result.

### Inline .drv Optimization

After DAG construction, the gateway optionally inlines the ATerm content of `.drv` files into the `drv_content` field of each `DerivationNode`. This saves one executor -> store round-trip per dispatched derivation (the `GetPath` fetch). The optimization:

- Is **gated by a single batched `FindMissingPaths` call** over all expected output paths. Only nodes with at least one missing output (i.e., nodes that will actually dispatch) are inlined. Cache-hit nodes stay empty — the scheduler short-circuits them to `Completed` and they never dispatch.
- Applies a **per-node cap of 64 KB** (`MAX_INLINE_DRV_BYTES`). Larger `.drv` files (e.g., flake inputs serialized into `env`) fall back to executor-fetch.
- Applies a **total budget of 16 MB** (`INLINE_BUDGET_BYTES`) across all inlined nodes. Once the budget is exhausted, remaining nodes fall back to executor-fetch.
- Is **best-effort**: on any error (`FindMissingPaths` timeout, store unreachable), inlining is skipped entirely and all nodes fall back to executor-fetch. This is an optimization, not a correctness requirement.

> **Session state:** Although the gateway is described as "stateless beyond the lifetime of a single SSH connection," each SSH channel does accumulate per-session state: the parsed `.drv` cache and the `wopSetOptions` configuration. `wopAddTempRoot` is acknowledged as a no-op (rio's GC is store-side with explicit pins; a gateway-session-scoped set would be invisible to it). This state is connection-scoped and discarded when the SSH channel closes.

## Authentication + Tenant Identity

r[gw.auth.tenant-from-key-comment]
The tenant name lives in the **server-side `authorized_keys` entry's comment field**, not the client's key (SSH key authentication sends raw key data only). During `auth_publickey`, the gateway matches the client's presented key against its loaded entries via `.find()`, then reads `.comment()` from the **matched entry** to get the tenant name. This is stored on the connection and passed through to `SubmitBuildRequest.tenant_name`. Empty comment = single-tenant mode (tenant name is empty string → scheduler treats as `None`).

r[gw.keys.hot-reload]
`authorized_keys` is hot-reloaded by a background watcher that polls the file's mtime every `AUTHORIZED_KEYS_POLL_INTERVAL` (10s). On change, the file is re-parsed and atomically swapped into the shared `ArcSwap<Vec<PublicKey>>`; in-flight SSH handshakes see the new set on their next `auth_publickey_offered` call (each `.load()` reads the current `Arc`). **mtime polling, not inotify**: kubelet refreshes Secret mounts via a `..data` symlink swap that an `IN_MODIFY` watch on the file path never sees; `std::fs::metadata` follows symlinks so the swap surfaces as a changed mtime. **Reload failures keep the old set**: an empty/all-invalid/transiently-unreadable file logs WARN and retries next tick — never swap to an empty set (would lock everyone out). I-109: prior to this, rotating a tenant key required a pod restart.

r[gw.jwt.claims]
JWT claims: `sub` = tenant_id UUID (server-resolved at mint time), `iat`, `exp` (SSH session duration + grace), `jti` (unique token ID for revocation). Signed ed25519, public key distributed via ConfigMap.

r[gw.jwt.issue]
On successful SSH authentication, the gateway MUST mint a JWT with `sub` set to the resolved tenant UUID and store it on the session context. The scheduler reads `jti` from the interceptor-attached `Claims` extension (per `r[gw.jwt.verify]` below) — NO proto body field. For audit, the `SubmitBuild` handler INSERTs `jti` into `builds.jwt_jti` (column added in migration 016). Zero wire redundancy: `jti` lives once in the JWT, parsed once by the interceptor, read once by the handler.

r[gw.jwt.refresh-on-expiry+2]
The gateway MUST re-mint the session JWT before injecting it on any outbound gRPC call if the cached token is within 5 minutes of `exp`. Refresh is checked lazily on every token access (`SessionJwt::token()`): both a new channel on a `ControlMaster`-mux'd connection AND a single channel whose lifetime exceeds `JWT_SESSION_TTL_SECS` (e.g. a long `wopBuildDerivation` — keepalive resets `inactivity_timeout`, so the SSH layer never drops it) re-mint between opcodes. Re-mint is local (`sub` and the signing key are already on hand) — no `ResolveTenant` round-trip. The refreshed token gets a fresh `jti`; the old `jti` is NOT revoked (it expires naturally).

r[gw.jwt.verify]
The tonic interceptor on scheduler and store MUST extract `x-rio-tenant-token`, verify signature+expiry, attach `Claims` to request extensions, and reject invalid tokens with `Status::unauthenticated`. (Controller has no gRPC ingress — kube reconcile loop + raw-TCP /healthz only.) The scheduler ADDITIONALLY checks `jti NOT IN jwt_revoked` (PG lookup — gateway stays PG-free).

r[gw.jwt.dual-mode+2]
Gateway auth is two-branched PERMANENTLY: `x-rio-tenant-token` header present → JWT verify; absent → SSH-comment fallback. Operator selects the deployment posture via the **two `[jwt]` knobs in `gateway.toml`** — `key_path` and `required` (there is no separate `auth_mode` enum): `key_path` absent → comment-fallback only (no JWT minting); `key_path` set with `required=false` → mint-with-degrade (JWT minted on auth, downstream falls back to comment if verify fails); `key_path` set with `required=true` → mint-or-reject. Both paths stay maintained. (Does NOT bump `r[gw.auth.tenant-from-key-comment]`.)

r[gw.jwt.propagate]
Every gateway-originated gRPC to rio-scheduler or rio-store MUST be wrapped via `with_jwt`, including reconnect (`WatchBuild`), cleanup (`CancelBuild`), and post-build resolution (`QueryRealisation`) paths. `with_jwt` is the single injection point for both W3C `traceparent` and `x-rio-tenant-token`; a bare-struct call site silently loses both (orphan trace span today, hard auth failure once downstream tenant authz lands).

r[gw.jwt.anon-drv-lookup]
Read-path opcodes (`wopIsValidPath`, `wopEnsurePath`, `wopQueryPathInfo`, `wopQueryValidPaths`) MUST send the JWT to the store **except for `.drv` paths**, which are looked up anonymously (`jwt_unless_drv`). `.drv` files are build INPUTS, not tenant-owned OUTPUTS: a `.drv` uploaded under one identity then queried under another has no `path_tenants` row for the querying tenant, so a tenant-filtered `QueryPathInfo` would return NotFound for a `.drv` the client just uploaded. Output paths keep tenant-scoped visibility (`r[store.tenant.narinfo-filter]`); only the `.drv` lookup is exempt.

## Connection Lifecycle

r[gw.conn.lifecycle]
Each SSH channel follows this lifecycle:

```mermaid
stateDiagram-v2
    [*] --> handshake : SSH channel opened + exec "nix-daemon --stdio"
    handshake --> ready : handshake complete (magic + version + features + postHandshake + STDERR_LAST)
    ready --> ready : any opcode (wopSetOptions, query, upload, build)
    ready --> [*] : SSH channel closed
    handshake --> [*] : version mismatch (STDERR_ERROR)
```

> **Correction:** The original design required `wopSetOptions` as the mandatory first opcode after handshake. In practice, the real `nix-daemon` does not enforce this --- it accepts any opcode after the handshake completes. Nix clients conventionally send `wopSetOptions` first, but may send other opcodes (e.g., `wopQueryMissing`) first on multiplexed SSH channels. rio-build accepts any opcode after handshake.

r[gw.conn.exec-request]
**SSH transport:** Nix connects via `ssh ... nix-daemon --stdio`. The gateway must handle `exec_request` for this command and start the protocol on the SSH channel data stream. The `channel_open_session` alone does not start the protocol.

The gateway matches the **suffix** of the second-to-last whitespace-separated argument (`ends_with("nix-daemon")`) and requires the last argument to be exactly `--stdio`. This allows clients that send a full store path (e.g., `/nix/store/...-nix-2.20.0/bin/nix-daemon --stdio`) to connect successfully.

r[gw.conn.session-error-visible]
Any error propagated from an SSH handler method (via `?`) is logged at
`error!` and increments `rio_gateway_errors_total{type="session"}`. The
russh default swallows these silently.

r[gw.conn.cancel-on-disconnect+2]
The gateway MUST send `CancelBuild` to the scheduler for every build in
`active_build_ids` when an SSH channel drops, via ALL disconnect shapes:
(1) clean EOF between opcodes — `session.rs` opcode-read returns
`UnexpectedEof`, iterates the map; (2) russh `channel_close` callback —
`ChannelSession::Drop` fires a graceful-shutdown signal that `session.rs`
selects on and runs the same cancel loop; (3) `OPCODE_IDLE_TIMEOUT`
expiry — `session.rs` idle-timer fires after 600s with no opcode, runs
the same cancel loop before returning. All three paths MUST complete the
cancel loop before the protocol task exits; hard `abort()` on the task
handle defeats this. Builds not cancelled leak an executor slot until
`r[sched.backstop.timeout]`.

r[gw.conn.channel-limit+2]
A single SSH connection may open at most `MAX_CHANNELS_PER_CONNECTION`
(default 4) active protocol sessions. Additional `channel_open_session`
requests receive `SSH_MSG_CHANNEL_OPEN_FAILURE`; an `exec_request` on an
already-open channel that would exceed the cap receives `channel_failure`
(the load-bearing check — `channel_open_session` does not insert into the
session map, so a burst of opens followed by execs would otherwise bypass
the open-time gate). The limit matches Nix's default `max-jobs`.

r[gw.conn.keepalive+2]
The gateway sends SSH keepalive requests every 30 seconds. After 9
consecutive unanswered keepalives (~300 s — russh increments then
compares with `>`, so the drop fires at `interval × (max+1)`), the
connection is closed. This detects half-open TCP that kernel-level
keepalive would not. I-161: `keepalive_max` was 3 (=120 s), which
fired during a client's cold-eval idle window over the SSM-tunnel
path; raised so direct `nix --store ssh-ng://` clients without
`ServerAliveInterval` get a 5-minute budget.

r[gw.conn.nodelay]
TCP_NODELAY is set on all accepted sockets. The worker protocol's
small-request/small-response pattern interacts pathologically with
Nagle's algorithm (~40 ms added per round-trip).

r[gw.conn.real-connection-marker]
`rio_gateway_connections_total{result="new"}` and
`rio_gateway_connections_active` count connections that reached the SSH
authentication layer (any `auth_*` callback). TCP probes that close
before the SSH handshake are logged at `trace!` only.

r[gw.health.readiness-gated]
The gateway's gRPC health endpoint MUST report `NOT_SERVING` for the
empty-string service from process start until `connect_forever` has
established both store and scheduler channels, and MUST report `SERVING`
thereafter (until drain). tonic-health's `health_reporter()` initializes
`""` to SERVING — the gateway explicitly flips it to `NOT_SERVING`
immediately after construction via
`rio_common::server::health_reporter_not_serving`.

r[gw.conn.session-drain]
On SIGTERM, the gateway sets readiness `NOT_SERVING`, waits
`drain_grace_secs` for the load balancer to deregister, stops accepting
new SSH connections, then waits up to `session_drain_secs` for open
sessions to close on their own before exiting. Stopping the accept loop
must not disconnect already-established sessions — `nix build --store
ssh-ng://` clients with builds in flight stay connected until their
build completes or the session-drain timeout expires.

r[gw.drain.three-stage]
Shutdown is three-staged: (1) `spawn_drain_task` flips health
`NOT_SERVING`, sleeps `drain_grace_secs`, then fires `serve_shutdown` →
the SSH accept loop returns but spawned per-connection tasks continue;
(2) `wait_for_session_drain` polls `active_conns` until 0 OR
`session_drain_secs` elapses; (3) on drain timeout it fires
`sessions_shutdown` (every protocol task selects on this and runs
`cancel_active_builds`), then waits a final `CANCEL_GRACE` (5 s) for the
`CancelBuild` RPCs to land before process exit. I-081: without stage 3,
process exit Drops the proto tasks mid-flight and the scheduler never
hears `CancelBuild`. `terminationGracePeriodSeconds` in helm must be ≥
`drain_grace_secs + session_drain_secs + CANCEL_GRACE` + slack.

The deployed `session_drain_secs` (3600 s) is the operator SLA: a deploy
may interrupt builds running >1 h on the evicted gateway replica.
Karpenter is held off the control-plane NodePool entirely
(`disruption.budgets: nodes=0, reasons=[Drifted]` on `rio-general`), so
AMI drift never auto-evicts gateway pods; the operator runs `cargo xtask
k8s rotate-general` during a quiet window to roll those nodes onto a new
AMI under the same 1 h drain budget.

## STDERR Message Types

r[gw.stderr.message-types]
| Constant | Value | Direction | Meaning |
|----------|-------|-----------|---------|
| `STDERR_NEXT` | `0x6f6c6d67` | S -> C | Log/trace message (followed by: `string msg`) |
| `STDERR_READ` | `0x64617461` | S -> C | Server needs data from client (followed by: `u64 count` bytes requested) |
| `STDERR_WRITE` | `0x64617416` | S -> C | Server sending data to client (followed by: `string data`) |
| `STDERR_LAST` | `0x616c7473` | S -> C | End of stderr stream; result follows |
| `STDERR_ERROR` | `0x63787470` | S -> C | Error occurred (see format below) |
| `STDERR_START_ACTIVITY` | `0x53545254` | S -> C | Start structured activity |
| `STDERR_STOP_ACTIVITY` | `0x53544f50` | S -> C | End structured activity |
| `STDERR_RESULT` | `0x52534c54` | S -> C | Structured result for an activity |

### STDERR_ERROR Wire Format

r[gw.stderr.error-format]
This is a complex nested structure. The gateway must construct it correctly for every error response (rejected opcodes, failed builds, etc.):

r[gw.stderr.error-before-return+2]
**`STDERR_ERROR` and `STDERR_LAST` are mutually exclusive terminal frames. A handler sends exactly one of them, exactly once.**

- If a handler returns `Err(...)`, it MUST send `STDERR_ERROR` first, and the session loop MUST NOT follow up with `STDERR_LAST`. Never use bare `?` to propagate errors from store operations, NAR extraction, or ATerm parsing --- always wrap in a match that sends `STDERR_ERROR` before returning.
- If a handler sends `STDERR_ERROR`, it MUST `return Err(...)` immediately after. It MUST NOT call `stderr.finish()`, and it MUST NOT write a result payload. `STDERR_ERROR` is terminal for the operation --- the client stops reading STDERR frames and throws, so any bytes that follow are stranded in the TCP buffer and corrupt the next opcode on a pooled connection.
- To report a **recoverable** per-operation failure while keeping the session open for subsequent opcodes, use `BuildResult::failure` (or the opcode's equivalent failure-carrying result type) delivered via `STDERR_LAST` + result. For batch opcodes like `wopBuildPathsWithResults`, per-entry errors push `BuildResult::failure` and `continue` --- they do not abort the batch.

The `StderrWriter` API enforces this: `error()` poisons the writer so that subsequent `finish()` returns `Err` and `inner_mut()` panics.

> **Exception — `wopQueryRealisation`:** The handler invokes the store first, then sends `STDERR_LAST` unconditionally, then matches on the store result. A store error (already past `STDERR_LAST`) is too late for `STDERR_ERROR`; instead the handler returns empty-set (`u64(0)`) and logs a warning. This is a degraded path (one missed CA cache hit), not a correctness violation; the next opcode on the session will hit the same store and fail through its own error path. (This could be restructured to match before `STDERR_LAST` --- the result is already buffered --- but the degraded-path cost is trivial and the structure is simpler.)

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Error type (e.g. `"Error"`, `"nix::Interrupted"`) |
| `level` | u64 | Error level |
| `name` | string | Program name (e.g. `"rio-build"`) |
| `message` | string | Human-readable error message |
| `havePos` | u64 bool | Whether position info follows |
| (if havePos) `file` | string | Source file |
| (if havePos) `line` | u64 | Line number |
| (if havePos) `column` | u64 | Column number |
| `traceCount` | u64 | Number of trace entries |
| (per trace) `havePos` | u64 bool | Whether trace position follows |
| (per trace, if havePos) `file`, `line`, `column` | string, u64, u64 | Trace position |
| (per trace) `message` | string | Trace message |

### STDERR_START_ACTIVITY Wire Format

r[gw.stderr.activity+2]
| Field | Type | Description |
|-------|------|-------------|
| `id` | u64 | Activity ID (unique per session) |
| `level` | u64 | Verbosity level |
| `type` | u64 | Activity type (see enum below) |
| `text` | string | Human-readable activity description |
| `fieldsCount` | u64 | Number of structured fields |
| (per field) | u64 type + value | Typed field data |
| `parentId` | u64 | Parent activity ID (0 = no parent) |

**Activity type enum** (matches upstream `nix::ActivityType`, `libutil/logging.hh`):

| Value | Name | Description |
|-------|------|-------------|
| 0 | Unknown | Unknown/unclassified activity |
| 100 | CopyPath | Copying a single store path |
| 101 | FileTransfer | Downloading/uploading a file |
| 102 | Realise | Realising a derivation output |
| 103 | CopyPaths | Copying multiple store paths |
| 104 | Builds | Top-level "building N derivations" |
| 105 | Build | Building a single derivation |
| 106 | OptimiseStore | Optimising the store (dedup) |
| 107 | VerifyPaths | Verifying store paths |
| 108 | Substitute | Substituting a path |
| 109 | QueryPathInfo | Querying path info from a substituter |
| 110 | PostBuildHook | Running post-build hook |
| 111 | BuildWaiting | Build waiting for a lock |
| 112 | FetchTree | Fetching a flake input tree |

Note: values 1--99 are unused. The enum starts at 0 (Unknown) then jumps to 100. For `Build` (105) the `fields` array is `[drvPath, machineName, curRound, nrRounds]`; nom and `--log-format bar` read `fields[0]` as the derivation name and `fields[1]` as the "on `<machine>`" suffix. For `Substitute` (108) the `fields` array is `[storePath, substituterUri]`; the gateway emits one per `DerivationEventKind::SUBSTITUTING` (`r[sched.substitute.detached]`) with `fields[1]` empty (the store picks the upstream — the scheduler doesn't see which), and stops it on the paired `CACHED` (success) or `STARTED` (fetch failed → fell through to a build).

r[gw.activity.stop-parity]

Every `STDERR_START_ACTIVITY` the gateway emits to a client MUST be matched by a `STDERR_STOP_ACTIVITY` for the same id before the build's terminal `BuildResult` is written. The scheduler routes `Event::Log` on a separate broadcast ring from `DerivationEvent` so log volume cannot evict per-derivation `Completed` events; the gateway additionally drains any still-tracked activity ids at terminus to cover upstream loss.

r[gw.activity.progress-before-stop]

For each per-derivation terminal transition (`COMPLETED`/`FAILED`), the root `actBuilds` `STDERR_RESULT{resProgress, [done, expected, running, failed]}` reflecting that derivation's increment MUST reach the client before the derivation's `STDERR_STOP_ACTIVITY`. nom marks an `actBuild` ✔ only when `done` increments while the activity is still open (matching native nix's `Goal::done()` ordering: parent counter update precedes `Activity` destructor). The scheduler emits `Event::Progress` before `DerivationEvent::Completed`/`Failed` on the state channel; the gateway relays in arrival order.

r[gw.activity.subst-progress]

For each `DerivationEventKind::SUBSTITUTING` the gateway emits an `actSubstitute` (108) activity AND a child `actCopyPath` (100) activity (`fields=[storePath, "", machineName]`), and increments the root `actBuilds` `SetExpected{actCopyPath, N}` so nom shows an "X/Y copied" denominator. `Event::SubstituteProgress` (display-only, routed via the log broadcast ring; see `r[store.substitute.progress-stream]`) maps to `STDERR_RESULT{copy_aid, resProgress, [bytes_done, bytes_expected, 0, 0]}` so nom renders a per-derivation download bar. Both activities stop together (child first) on the paired `CACHED`/`STARTED`/`COMPLETED`/`FAILED`.

### STDERR_RESULT BuildEvent mapping

r[gw.stderr.result.build-log-line]
Build log lines are emitted as `STDERR_RESULT` with `result_type=101` (`BuildLogLine`) and one string field, attached to the per-derivation `actBuild` activity ID. `STDERR_NEXT` is reserved for gateway-originated diagnostics (trace_id, reconnect notices) and for log lines that arrive before the derivation's `Started` event.

r[gw.stderr.result.progress]
On `BuildStarted` the gateway emits a top-level `actBuilds` (104) activity and a `SetExpected` (106) result `[actBuild, total-cached]`. On each `BuildProgress` it emits a `Progress` (105) result `[completed, total, running, 0]` against that activity. The activity is stopped on the build's terminal event.

r[gw.stderr.result.set-phase]
`BuildPhase` events are emitted as `STDERR_RESULT` with `result_type=104` (`SetPhase`) and one string field (the phase name), attached to the per-derivation `actBuild` activity.

## Protocol Compatibility

r[gw.compat.version-range+2]
rio-build advertises protocol **1.38** (`0x126`) to support the feature-exchange step. Minimum accepted client version is **1.35** (`0x123`) — the version Lix is policy-frozen at (CppNix 2.18 fork point). Older clients are rejected at handshake with a human-readable error. The only field rio gates between 1.35 and 1.38 is `BuildResult.cpu_user`/`cpu_system` (added in 1.37); the feature exchange itself is already gated on >= 1.38.

r[gw.compat.unknown-opcode-close]
Unknown or unsupported opcodes return `STDERR_ERROR` and **close the connection**. This is necessary because the opcode's payload remains unread in the stream and its format is unknown, making it impossible to skip to the next opcode without corrupting the protocol. The Nix client will reconnect automatically.

| Category | Opcodes |
|----------|---------|
| Fully implemented | `wopIsValidPath`, `wopQueryPathInfo`, `wopQueryValidPaths`, `wopAddToStore`, `wopAddTextToStore`, `wopAddToStoreNar`, `wopEnsurePath`, `wopNarFromPath`, `wopBuildDerivation`, `wopBuildPaths`, `wopBuildPathsWithResults`, `wopQueryMissing`, `wopAddTempRoot`, `wopSetOptions`, `wopAddMultipleToStore`, `wopQueryDerivationOutputMap`, `wopQueryPathFromHashPart`, `wopAddSignatures`, `wopRegisterDrvOutput`, `wopQueryRealisation` |
| Rejected (STDERR_ERROR) | Everything else |

**Note on CA opcodes:** `wopRegisterDrvOutput` / `wopQueryRealisation` parse the Nix Realisation JSON (`{"id":"sha256:<hex>!<name>","outPath":...,"signatures":[...],"dependentRealisations":{}}`) and call the store's RegisterRealisation/QueryRealisation RPCs. Soft-fail on malformed input (discard/empty-set rather than STDERR_ERROR) to avoid regressing buggy clients that worked against the old stubs. `dependentRealisations` is always `{}` from current Nix ([ADR-018](../decisions/018-ca-resolution.md) — field removed upstream in schema v2); the gateway's `{}` stub is correct and should not be changed. Full CA early cutoff (nar_hash comparison via content_index) is Phase 5.

**Note on `wopQueryDerivationOutputMap`:** Moved from "CA-aware" to "Fully implemented" because modern Nix clients call this for ALL derivation types. For input-addressed derivations, it returns the statically-known output paths. For CA derivations, it returns the realized output paths if known.

**Note on `wopAddTempRoot`:** Accepts the store path and records it as a connection-scoped temporary GC root in-memory. These temp roots prevent GC of paths the client is actively using. They are lost on gateway pod restart, which is acceptable given the store's GC grace period (default 2h). The store's GC relies on the grace period rather than querying gateways for active temp roots.

## Build Hook Protocol Path

r[gw.hook.single-node-dag]
When a Nix client uses `--builders` (build hook mode) instead of `--store ssh-ng://` (remote store mode), the interaction pattern changes significantly:

**How it works:** The local `nix-daemon` drives DAG traversal and delegates individual derivations to rio-build one at a time via `wopBuildDerivation`. Each hook invocation is an independent SSH session that submits a single derivation without the full DAG context.

**What the gateway does:**
- Receives `wopAddToStoreNar` for the derivation's inputs, then `wopBuildDerivation` for the target derivation
- **Attempts full-DAG reconstruction** (`r[gw.dag.reconstruct]`) by resolving the `.drv` from the session cache or store and walking `inputDrvs`. If the `.drv` cannot be resolved, falls back to a **single-node "DAG"** built from the inline `BasicDerivation` (no edges, no DAG context). DAG-reconstruction errors (transitive-input cap exceeded, child-`.drv` resolve failure mid-BFS) are surfaced to the client — degrading to single-node would dispatch an input-addressed root with missing inputs.
- Submits to the scheduler via `SubmitBuild` as usual

**Scheduling optimizations lost in build hook mode:**
- **No critical-path analysis** --- the scheduler sees each derivation in isolation, not as part of a graph
- **No multi-build DAG merging** --- shared derivations between concurrent builds cannot be deduplicated at the scheduling level
- **No CA early cutoff** --- without the full DAG, the scheduler cannot propagate cutoffs to downstream nodes

r[gw.hook.ifd-detection+2]
**IFD detection:** When a `wopBuildDerivation` call arrives without a preceding `wopBuildPathsWithResults` on the same session, the gateway sets `SubmitBuildRequest.priority_class = "interactive"` (otherwise `"ci"`). There is no dedicated `is_ifd_hint` proto field — the hint is encoded entirely in the `priority_class` string and the gateway, not the scheduler, makes the assignment.

> **Recommendation:** Prefer `ssh-ng://` (remote store mode) over `--builders` (build hook mode) for better scheduling. The build hook path exists for compatibility with existing `nix.conf` setups, but delivers worse throughput and scheduling quality for large builds.

## Rate Limiting & Connection Management

r[gw.rate.per-tenant]
Per-tenant build-submit rate limiting via `governor`
`DefaultKeyedRateLimiter<String>` keyed on `tenant_name` (from
authorized_keys comment — operator-controlled, cannot be forged by
client; empty/absent → key `"__anon__"`). **Disabled by default** — no
quota unless `gateway.toml [rate_limit]` section (or
`RIO_RATE_LIMIT__PER_MINUTE` + `RIO_RATE_LIMIT__BURST` env vars) is
present. When enabled: quota is operator-configured (`per_minute` =
refill rate, `burst` = bucket capacity; both must be ≥1). Checked in
all three build opcodes (`wopBuildDerivation`, `wopBuildPaths`,
`wopBuildPathsWithResults`) immediately before `SubmitBuild`. On
rate-limit violation: `STDERR_ERROR` with wait-hint (rounded up to
nearest second) + tenant name, early return — the SSH connection
stays open for retry. Phase 5: key becomes `Claims.sub` (tenant
UUID from JWT) instead of `tenant_name` (SSH comment) — same bounded
keyspace, JWT-native source. No eviction needed either way.

r[gw.conn.cap]
Global connection cap via `Arc<Semaphore>` (default 1000, configurable
via `gateway.toml max_connections` or `RIO_MAX_CONNECTIONS`).
`try_acquire_owned()` in `new_client` (the russh accept callback); the
permit is held by the `ConnectionHandler` and released in `Drop` so
every disconnect path (EOF, error, abort) frees the slot. At cap: the
handler's `conn_permit` is `None`, and the first `auth_*` callback
returns `Err` to tear down the connection before any channel work.
`log_session_end` logs the reject with `stage=auth-attempted`.

r[gw.store.transient-retry]

Gateway→store RPCs that traverse `r[store.substitute.admission]`
(`QueryPathInfo`, `GetPath`) MUST retry on transient gRPC status
(`ResourceExhausted`, `Unavailable`, `Unknown`, `Aborted` per
`rio_common::grpc::is_transient`). Retry budget is 2 attempts (one
retry). Under sustained admission saturation each attempt blocks
`SUBSTITUTE_ADMISSION_WAIT` (25 s) server-side, so worst-case latency
before surfacing to the user is ~50 s — bounded, but operators should
treat sustained `RESOURCE_EXHAUSTED` here as a scaling signal.
Non-transient status (`NotFound`, `Internal`, `DeadlineExceeded`)
surfaces on the first attempt. The store maps the placeholder-race case
(`SubstituteError::Raced` — a concurrent replica is still fetching the
NAR) to `NotFound`, NOT `Unavailable`: the 2-attempt budget can't
outlast a multi-second NAR fetch, so the gateway treats it as
`valid=false` (miss) and the caller re-probes later. The upstream-429
case (`SubstituteError::RateLimited{retry_after}`, including a bare
429 with no `Retry-After`) is `Unavailable` and retried here.

r[gw.put.aborted-retry]
The buffered `grpc_put_path` helper (used by `wopAddToStore`, `wopAddTextToStore`, and the `.drv`-buffered branch of `wopAddToStoreNar`/`wopAddMultipleToStore`) MUST retry on store `Code::Aborted` up to `PUT_PATH_ABORTED_MAX_ATTEMPTS` (8) with full-jitter exponential backoff (50 ms base, ×2, 2 s cap → ≤ ~6 s total budget). The store returns `Aborted` when another upload holds the placeholder row for the same path (I-068) or on PG serialization conflicts. Each retry rebuilds the request stream from the `Arc<[u8]>`-held NAR without copying. Emits `rio_gateway_putpath_aborted_retries_total{attempt}` per retry. The streaming `grpc_put_path_streaming` helper is **not** retried on `Aborted` — its reader is consumed and the bytes were forwarded as they arrived, so there is nothing to replay; in practice that path only fires for oversize non-`.drv` entries where the I-068 collision case does not apply.

## High Availability

r[gw.sched.balanced]
The gateway connects to the scheduler in one of two modes selected by `scheduler.balance_host` in `gateway.toml`: **balanced** (K8s, multi-replica) — DNS-resolve the headless Service, probe `grpc.health.v1/Check` on each pod IP, and route to the SERVING (= leader) endpoint via `BalancedChannel`; or **single** (VM tests, single-replica) — plain connect to `scheduler.addr`. In balanced mode, `scheduler.addr` is still required: it is the ClusterIP Service, used as the TLS-verify domain (the cert's SAN). The `BalancedChannel` guard is held for process lifetime — dropping it stops the probe loop. The store connection is single-channel only (no `balance_host`; store load is builder-driven, the gateway's `QueryPathInfo` traffic is light).

- Multiple gateway replicas sit behind a TCP load balancer (NLB on EKS with idle timeout ≥ 3600s).
- Session state is connection-scoped --- the gateway is stateless beyond the lifetime of a single SSH connection.
- If a gateway pod dies, the affected SSH connections drop. Clients reconnect automatically (standard Nix retry behavior) and land on a healthy replica.
- Builds that were already in progress continue in the scheduler; only the log-streaming link is lost.

r[gw.reconnect.backoff]
**WatchBuild reconnect:** When the `SubmitBuild` / `WatchBuild` response stream breaks (scheduler failover, transient network), the gateway's `process_stream` distinguishes error classes via `StreamProcessError`:
- `Transport` (scheduler connection dropped) and `EofWithoutTerminal` (stream closed cleanly without a terminal `BuildCompleted`/`Failed`/`Cancelled` event — typical leader-failover signature: SIGTERM → graceful shutdown → TCP FIN → `Ok(None)`) → retried up to **10 times** with exponential backoff **1 s/2 s/4 s/8 s/16 s, capped at 16 s for attempts 6–10**. The scheduler replays `BuildEvent`s from `build_event_log` starting at `since_sequence`.
- `Wire` → **not** retried; the gateway returns `MiscFailure` to the Nix client immediately. This indicates a protocol bug, not a transient connectivity issue.
The reconnect counter resets on the first successful `BuildEvent` received after a reconnect (NOT on `WatchBuild` returning `Ok` — accepting the RPC doesn't prove the stream will yield events).

r[gw.reconnect.since-seq]
The gateway MUST track the sequence number of the first peeked `BuildEvent` and use it as the initial `since_sequence` for reconnect, not hardcode `0`. The scheduler never emits `sequence=0` (it's the `WatchBuildRequest`-side "from start" sentinel); hardcoding `0` causes every first-event reconnect to replay one extra event.

- The gateway does not own durable state. All persistent data lives in the scheduler (PostgreSQL) and the store.
- Consider using a non-standard SSH port (e.g., 2222) to avoid conflicts with host SSH daemons and corporate firewalls blocking port 22 for non-standard destinations.
- The chart sets a `preStop.sleep` of `nlbDeregisterSecs` (NLB health-check round) before SIGTERM, then the three-stage drain runs with `sessionDrainSecs` (default 600s) so in-flight SSH sessions complete during rolling updates. `terminationGracePeriodSeconds` is computed from all three.

## Key Files

- `rio-gateway/src/server.rs` --- SSH server setup (russh), per-channel task spawning, `exec_request` matching
- `rio-gateway/src/session.rs` --- Per-SSH-channel protocol session loop (`run_protocol`), CancelBuild on disconnect
- `rio-gateway/src/handler/` --- Nix worker protocol opcode handlers:
    - `mod.rs` --- opcode dispatch (`handle_opcode`), `SessionContext`, `.drv` cache
    - `opcodes_read.rs` --- read-only opcodes (`wopIsValidPath`, `wopQueryPathInfo`, `wopNarFromPath`, ...)
    - `opcodes_write.rs` --- write opcodes (`wopAddToStore`, `wopAddToStoreNar`, `wopAddMultipleToStore`, ...)
    - `build.rs` --- build opcodes (`wopBuildDerivation`, `wopBuildPaths`, `wopBuildPathsWithResults`)
    - `grpc.rs` --- gRPC helpers for store put/get
- `rio-gateway/src/translate.rs` --- DAG reconstruction from `.drv` references, inline-`.drv` optimization, proto <-> wire translation
