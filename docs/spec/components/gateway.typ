#import "/lib/rio.typ": *
#show: rio.with(domains: ("gw",))


The gateway is the entry point. It terminates SSH connections and speaks the
Nix @worker-protocol, making rio-build appear as a standard Nix remote
store/builder.

= Responsibilities

- SSH server via `russh` crate --- accepts connections, authenticates via SSH
  keys
- Implement the Nix worker protocol (version negotiation, opcode handling)
- Handle both remote store mode (full @dag submission) and @build-hook mode
  (per-derivation delegation)
- STDERR streaming loop: send `STDERR_NEXT`, `STDERR_START_ACTIVITY`,
  `STDERR_STOP_ACTIVITY`, `STDERR_RESULT`, `STDERR_LAST` during operations
- Translate protocol ops into internal gRPC calls to scheduler and store
- Each SSH channel maintains independent protocol state (separate handshake and
  option negotiation)

= Network reachability

#r("gw.ingress.v6-direct")[
  The gateway MUST be reachable from an IPv6-only client over the cluster's
  IPv6 NodePort with no translation layer.
]

#r("gw.ingress.v4-via-nat")[
  The gateway MUST be reachable from an IPv4-only client via an external v4→v6
  translator (AWS NLB `enable-prefix-for-ipv6-source-nat`, or equivalent). rio
  does not implement this translation; it is an infrastructure requirement.
]

= Critical Opcodes

#r("gw.opcode.mandatory-set")[
  The opcodes below are the mandatory implementation set for a working
  `ssh-ng://` store. Each has a dedicated wire-format section below.
]

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Opcode], [Value], [Description]),
  [`wopIsValidPath`], [1], [Check if a @store-path exists],
  [`wopAddToStore`], [7], [Legacy content-addressed store path import],
  [`wopAddTextToStore`], [8], [Legacy text file import (builtins.toFile)],
  [`wopBuildPaths`], [9], [Build a set of derivations],
  [`wopEnsurePath`], [10], [Ensure a store path is valid/available],
  [`wopAddTempRoot`], [11], [Add temporary @gc-root],
  [`wopSetOptions`], [19], [Accept client build configuration],
  [`wopQueryPathInfo`], [26], [Return full path metadata],
  [`wopQueryPathFromHashPart`],
  [29],
  [Resolve a store path from its hash
    prefix],

  [`wopQueryValidPaths`], [31], [Batch validity check],
  [`wopBuildDerivation`], [36], [Build a single derivation],
  [`wopAddSignatures`], [37], [Add signatures to a path],
  [`wopNarFromPath`], [38], [Export path as @nar],
  [`wopAddToStoreNar`], [39], [Accept NAR imports],
  [`wopQueryMissing`], [40], [Report what needs building],
  [`wopQueryDerivationOutputMap`], [41], [Get output name → path mapping],
  [`wopRegisterDrvOutput`], [42], [Register @ca derivation output],
  [`wopQueryRealisation`], [43], [Query CA realisation],
  [`wopAddMultipleToStore`], [44], [Batch NAR import],
  [`wopBuildPathsWithResults`], [46], [Build paths and return results],
)

== wopSetOptions (19) Field Sequence

#r("gw.opcode.set-options.field-order")[
  The fields are sent in order, all as `u64` unless noted. The
  *daemon-protocol* client (`ssh://`) sends `wopSetOptions` as the first opcode
  after handshake. The *ssh-ng* client does NOT send it (empirically verified
  P0215) --- `SSHStore::setOptions()` is an empty override. Client-side
  `--max-silent-time`/`--timeout` are silently non-functional over ssh-ng; see
  Override propagation below for the gateway-side fallback path.
]

+ `keepFailed` (u64 bool)
+ `keepGoing` (u64 bool)
+ `tryFallback` (u64 bool)
+ `verbosity` (u64)
+ `maxBuildJobs` (u64)
+ `maxSilentTime` (u64)
+ `obsolete_useBuildHook` (u64: always 1)
+ `verboseBuild` (u64 --- Verbosity level: `lvlError`=0 means true,
  `lvlVomit`=7 means false; daemon decodes via `lvlError == readInt()`)
+ `obsolete_logType` (u64: 0)
+ `obsolete_printBuildTrace` (u64: 0)
+ `buildCores` (u64)
+ `useSubstitutes` (u64 bool)
+ `overrides_count` (u64) followed by `overrides_count` pairs of `(key: string,
  value: string)` --- always present since the minimum accepted client version
  is 1.35

#r("gw.opcode.set-options.propagation+2")[
  *Override propagation:* The `overrides` key-value pairs contain client build
  settings. The gateway extracts relevant overrides and propagates them through
  the build pipeline: gateway → scheduler (via gRPC) → workers. *NOT reachable
  via `ssh-ng://`* --- Nix `SSHStore` overrides `RemoteStore::setOptions()`
  with an empty body (unchanged since 088ef8175, 2018-03-05; intentional, see
  NixOS/nix\#1713/\#1935), so `wopSetOptions` never hits the wire for ssh-ng
  clients. All `--option` flags are silently dropped client-side. This opcode
  fires only for `unix://` daemon-socket clients, which is not rio's production
  path. See #rref("sched.timeout.per-build") for the gRPC-only reachability of
  `build_timeout`. Upstream fix NixOS/nix 32827b9fb adds selective ssh-ng
  forwarding but requires the daemon to advertise a `set-options-map-only`
  protocol feature that rio-gateway does not implement.
]

== wopNarFromPath (38) Wire Format

#r("gw.opcode.nar-from-path")[
  Exports a store path as a NAR archive.
]

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Direction], [Field], [Type], [Description]),
  [C → S], [`path`], [string], [Store path to export],
)

#r("gw.opcode.nar-from-path.raw-bytes")[
  *Behavior:* rio-gateway sends `STDERR_LAST` to close the stderr loop, then
  streams the raw NAR bytes directly on the connection (no framing, no length
  prefix). This matches the canonical nix-daemon behavior. The Nix client's
  `copyNAR()` reads until the NAR is complete.
]

#memo(title: [Historical note (bug \#11)])[
  Earlier phases sent the NAR inside the stderr loop via `STDERR_WRITE` chunks,
  described in this document as an "intentional divergence". That was wrong ---
  the Nix client's `processStderr()` for this opcode passes no sink, so
  `STDERR_WRITE` frames caused `error: no sink`. Fixed to `STDERR_LAST` + raw
  bytes. See `handle_nar_from_path` in
  #(refs.gh)("rio-gateway/src/handler/opcodes_read.rs").
]

== wopAddToStore (7) Wire Format

Legacy content-addressed store path import. The client sends a name, a
content-address method string, references, and the raw file contents (or NAR)
as a framed stream. The server computes the store path, wraps non-recursive
data in a NAR, and returns the full `ValidPathInfo`.

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Direction], [Field], [Type], [Description]),
  [C → S], [`name`], [string], [Store path name component],
  [C → S],
  [`camStr`],
  [string],
  [Content-address method (see CAM formats
    below)],

  [C → S], [`references`], [string collection], [Referenced store paths],
  [C → S],
  [`repair`],
  [u64 bool],
  [Whether to repair/overwrite (read and
    discarded)],

  [C → S],
  [`dump`],
  [framed byte stream],
  [Raw file contents (flat) or NAR
    bytes (recursive)],
)

*Content-address method (`camStr`) formats:*

#table(
  columns: 2,
  align: (left, left),
  table.header([Format], [Meaning]),
  [`text:sha256`],
  [Text import (builtins.toFile-style); hash is over raw
    bytes; store path via `makeTextPath`],

  [`fixed:sha256`],
  [Flat fixed-output; hash is over raw bytes; gateway wraps
    in a single-file NAR],

  [`fixed:r:sha256`],
  [Recursive fixed-output; dump IS a NAR; hash is over the
    NAR bytes],

  [`fixed:git:sha1`],
  [Git tree import; *rejected* (not supported --- would
    compute wrong store path)],
)

#r("gw.opcode.add-to-store.cam-git-rejected")[
  The `fixed:git:` content-address method is rejected with `STDERR_ERROR`. Git
  ingestion is a distinct `FileIngestionMethod` in Nix (inner fingerprint
  `"fixed:out:git:..."`, CA `"fixed:git:..."`); collapsing it into recursive
  mode would silently produce a different store path than the client computed.
]

Response (after `STDERR_LAST`) is a full `ValidPathInfo`:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`path`], [string], [Computed store path],
  [`deriver`], [string], [Always empty],
  [`narHash`],
  [string],
  [SHA-256 hash of the NAR (hex-encoded digest, no
    prefix)],

  [`references`], [string collection], [Echoed references],
  [`registrationTime`], [u64], [Always 0],
  [`narSize`], [u64], [NAR size in bytes],
  [`ultimate`], [u64 bool], [Always 1 (trusted source)],
  [`sigs`], [string collection], [Always empty],
  [`ca`],
  [string],
  [Content address: `text:sha256:<nixbase32>` or
    `fixed:[r:]<algo>:<nixbase32>`],
)

== wopAddToStoreNar (39) Wire Format

#r("gw.opcode.add-to-store-nar+2")[
  For protocol >= 1.25 (always present since we target 1.35+):
]

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`path`], [string], [Store path being imported],
  [`deriver`], [string], [Deriver path (empty if unknown)],
  [`narHash`],
  [string],
  [SHA-256 hash of the NAR (hex-encoded digest, no
    algorithm prefix)],

  [`references`], [string collection], [Referenced store paths],
  [`registrationTime`], [u64], [Registration timestamp],
  [`narSize`], [u64], [Size of the NAR in bytes],
  [`ultimate`], [u64 bool], [Whether this is the ultimate trusted source],
  [`sigs`], [string collection], [Signatures],
  [`ca`], [string], [Content address (empty for input-addressed)],
  [`repair`], [u64 bool], [Whether to repair/overwrite existing path],
  [`dontCheckSigs`],
  [u64 bool],
  [Skip signature verification (read and
    discarded by rio-gateway; signature enforcement, if any, is delegated to
    rio-store)],
)

#r("gw.opcode.add-to-store-nar.framing+2")[
  After sending the metadata fields, the NAR data is transferred as a *framed
  byte stream* (protocol >= 1.23, always true for 1.35+):
  + Client sends framed data: sequence of `u64(chunk_len) + chunk_bytes`,
    terminated by `u64(0)` sentinel
  + Chunk data is NOT padded (unlike string encoding)
  + Server sends `STDERR_LAST` (`0x616c7473`) --- no result value follows
]

#memo(title: [Correction (discovered during implementation review)])[
  The original design described a `STDERR_READ` pull loop for NAR data
  transfer. This is only used for protocol versions 1.21--1.22. For protocol
  >= 1.23, the Nix C++ daemon uses `FramedSource` (in the `wopAddToStoreNar`
  handler's `protoVersion >= 1.23` branch in `daemon.cc`), and the client
  sends data via `FramedSink` (in `RemoteStore::addToStore`). The framed stream
  format is the same as used by `wopAddMultipleToStore`. Additionally, the
  original design omitted the `dontCheckSigs` field and incorrectly included a
  `u64(1)` result value after `STDERR_LAST`.
]

== wopAddMultipleToStore (44) Wire Format

#r("gw.opcode.add-multiple.batch+2")[
  Added in protocol 1.32 (always present for 1.35+). This is the primary upload
  path for modern Nix clients, replacing per-item `wopAddToStoreNar` for source
  paths.
]

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`repair`], [u64 bool], [Whether to repair/overwrite],
  [`dontCheckSigs`],
  [u64 bool],
  [Skip signature verification (see note
    below)],
)

#r("gw.opcode.add-multiple.unaligned-frames")[
  Followed by a *framed byte stream* containing a count prefix and all entries
  concatenated. The framed stream is a byte transport --- *entry boundaries do
  not align with frame boundaries*. A single frame may contain the end of one
  entry and the beginning of the next, or an entry may span multiple frames.
  The receiver must:
  + Reassemble frames into a contiguous byte stream
  + Read `num_paths: u64` from the start of the reassembled stream
  + Parse `num_paths` entries sequentially
]

The reassembled stream begins with:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`num_paths`],
  [u64],
  [Number of entries that follow (MUST be bounds-checked
    against `MAX_COLLECTION_COUNT`)],
)

Each entry in the reassembled stream contains:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`pathInfo`],
  [(same 9 fields as wopAddToStoreNar metadata, minus the
    trailing `repair` and `dontCheckSigs` flags)],
  [Path metadata],

  [NAR data],
  [`narSize` plain bytes],
  [The NAR content --- *NOT
    nested-framed*; read `narSize` bytes directly from the reassembled outer
    stream],
)

The outer framed stream terminates with a `u64(0)` sentinel.

#memo(title: [Correction (discovered via VM test)])[
  Earlier versions of this spec described the per-entry NAR as an inner framed
  stream and omitted the `num_paths` prefix. Both were wrong. Nix's
  `Store::addMultipleToStore(Source &)` reads `num_paths` first
  (`readNum<uint64_t>(source)`) and then for each entry calls `addToStore(info,
  source)` which reads `narSize` plain bytes directly. The bug was masked by a
  byte-level test written to match the buggy parser rather than the spec.
]

#r("gw.opcode.add-multiple.dont-check-sigs-ignored")[
  *`dontCheckSigs` handling:* The gateway reads and discards `dontCheckSigs`.
  The gateway does not perform signature verification itself; signature
  enforcement (if any) is delegated to rio-store. The field is consumed to
  maintain wire compatibility.
]

*Response:* The server sends `STDERR_LAST` with no result value (matching the
`wopAddMultipleToStore` handler's `logger->stopWork()` sequence in
`daemon.cc`).

== DerivedPath Wire Format

#r("gw.wire.derived-path")[
  `DerivedPath` is used by `wopBuildPaths` (9) and `wopBuildPathsWithResults`
  (46) to specify what to build. It is sent as a single string that the server
  must parse. There are three forms:
]

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Form], [Syntax], [Example], [Description]),
  [Opaque],
  [plain store path],
  [`/nix/store/abc...-foo`],
  [Build/fetch this
    exact path],

  [Built (explicit outputs)],
  [`drvPath!output1,output2`],
  [`/nix/store/abc...-foo.drv!out,dev`],
  [Build specific outputs of a
    derivation],

  [Built (all outputs)],
  [`drvPath!*`],
  [`/nix/store/abc...-foo.drv!*`],
  [Build
    all outputs of a derivation],
)

The `!*` form is the *default* used by `nix build`. When a client runs `nix
build /nix/store/abc...-foo.drv`, it sends the `drvPath!*` form.

Both `wopBuildPaths` and `wopBuildPathsWithResults` send a `string collection`
of `DerivedPath` values. The gateway must parse each string to determine the
form and extract the derivation path and requested outputs.

== wopBuildDerivation (36) --- BasicDerivation Wire Format

#r("gw.opcode.build-derivation+2")[
  Sends an inline `BasicDerivation` (without `inputDrvs`). For protocol 1.35+:
]

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`drvPath`], [string], [The `.drv` store path],
  [`outputs`], [collection of output tuples], [See below],
  [`inputSrcs`], [string collection], [Input source store paths],
  [`platform`], [string], [e.g. `x86_64-linux`],
  [`builder`], [string], [Builder executable path],
  [`args`], [string collection], [Builder arguments],
  [`env`], [string-pair collection], [Environment variables],
  [`buildMode`], [u64], [0=Normal, 1=Repair, 2=Check],
)

Each *output tuple* (protocol >= 1.32):

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`name`], [string], [Output name (e.g. `out`, `dev`)],
  [`path`], [string], [Output store path],
  [`hashAlgo`],
  [string],
  [Hash algorithm for CA outputs (empty for
    input-addressed)],

  [`hash`],
  [string],
  [Expected hash for CA outputs (empty for
    input-addressed)],
)

*DAG reconstruction:* The gateway cannot reconstruct the dependency DAG from
`BasicDerivation` alone (it has no `inputDrvs`). The gateway reconstructs the
full DAG by parsing the `.drv` files uploaded in the preceding
`wopAddToStoreNar`/`wopAddMultipleToStore` step. Each `.drv` file contains
`inputDrvs` references that form the DAG edges.

*Response* (after the STDERR loop): a single `BuildResult` (see the
_BuildResult Wire Format_ section below). On success, `builtOutputs` covers
every declared output — declared paths for input-addressed and fixed-CA
outputs, the registered realisation for floating-CA outputs — gated by the
store verification of #rref("gw.opcode.build-results-honest") when the
gateway holds the resolved `.drv`; in the single-node fallback no
verification is possible and `builtOutputs` stays empty.

== wopQueryDerivationOutputMap (41) Wire Format

#r("gw.opcode.query-derivation-output-map")[
  *Important:* This opcode is called by all modern Nix clients
  unconditionally, not just for CA derivations. For input-addressed
  derivations, it must return the statically-known output paths (computable
  from the derivation itself).
]

*Resolution strategy:* The gateway computes the output map locally from the
parsed `.drv` file (obtained from the per-session `.drv` cache built during
`wopAddToStoreNar`/`wopAddMultipleToStore`, or fetched from rio-store if the
`.drv` was uploaded in a previous session). For input-addressed derivations,
the output paths are deterministic and computed from the derivation's @aterm
representation. For CA derivations (Phase 5), the gateway first checks
rio-store for realized output paths via `QueryPathInfo`; if unknown, it returns
the placeholder output paths from the `.drv`.

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Direction], [Field], [Type], [Description]),
  [C → S], [`drvPath`], [string], [The `.drv` store path to query],
  [S → C], [`count`], [u64], [Number of output mappings],
  [S → C], [(per output) `name`], [string], [Output name],
  [S → C], [(per output) `path`], [string], [Output store path],
)

== wopIsValidPath (1) Wire Format

#r("gw.opcode.is-valid-path")[
  #table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Direction], [Field], [Type], [Description]),
    [C → S], [`path`], [string], [Store path to check],
  )
]

Response (after @stderr-loop):

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Direction], [Field], [Type], [Description]),
  [S → C], [`valid`], [u64 bool], [1 if path exists in store, 0 otherwise],
)

== wopQueryPathInfo (26) Wire Format

#r("gw.opcode.query-path-info")[
  #table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Direction], [Field], [Type], [Description]),
    [C → S], [`path`], [string], [Store path to query],
  )
]

Response (after STDERR loop). First, a validity flag:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`valid`], [u64 bool], [1 if path exists, 0 if not (stop here if 0)],
)

If `valid == 1`, the following fields are sent in order:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`deriver`], [string], [Deriver path (empty if unknown)],
  [`narHash`], [string], [NAR hash (hex-encoded digest, no algorithm prefix)],
  [`references`], [string collection], [Referenced store paths],
  [`registrationTime`], [u64], [Registration timestamp],
  [`narSize`], [u64], [NAR size in bytes],
  [`ultimate`], [u64 bool], [Whether this is the ultimate source],
  [`sigs`], [string collection], [Signatures],
  [`ca`], [string], [Content address (empty for input-addressed)],
)

== wopQueryValidPaths (31) Wire Format

#r("gw.opcode.query-valid-paths")[
  #table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Direction], [Field], [Type], [Description]),
    [C → S], [`paths`], [string collection], [Store paths to check],
    [C → S],
    [`substitute`],
    [u64 bool],
    [Whether to attempt substitution for
      missing paths (ignored by rio-build)],
  )
]

Response (after STDERR loop):

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Direction], [Field], [Type], [Description]),
  [S → C],
  [`validPaths`],
  [string collection],
  [Subset of input paths that
    exist in the store],
)

== wopBuildPaths (9) Wire Format

#r("gw.opcode.build-paths")[
  #table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Direction], [Field], [Type], [Description]),
    [C → S],
    [`paths`],
    [string collection],
    [`DerivedPath` values (see
      DerivedPath Wire Format above)],

    [C → S], [`buildMode`], [u64], [0=Normal, 1=Repair, 2=Check],
  )
]

Response (after STDERR loop): `u64(1)` for success. On failure, the STDERR loop
includes `STDERR_ERROR`.

#info[
  Unlike `wopBuildPathsWithResults`, this opcode does NOT return per-path
  `BuildResult` structures.
]

== wopQueryMissing (40) Wire Format

#r("gw.opcode.query-missing")[
  #table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Direction], [Field], [Type], [Description]),
    [C → S], [`paths`], [string collection], [`DerivedPath` values to check],
  )
]

Response (after STDERR loop):

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`willBuild`], [string collection], [Store paths that need building],
  [`willSubstitute`],
  [string collection],
  [Store paths that can be substituted
    (populated from `FindMissingPathsResponse.substitutable_paths` when the
    tenant has upstream caches configured --- see
    #rref("store.substitute.upstream"))],

  [`unknown`], [string collection], [Store paths with unknown status],
  [`downloadSize`], [u64], [Estimated download size in bytes],
  [`narSize`], [u64], [Estimated total NAR size in bytes],
)

== wopBuildPathsWithResults (46) Response Wire Format

#r("gw.opcode.build-paths-with-results")[
  `wopBuildPathsWithResults` (opcode 46) returns one `KeyedBuildResult` per
  requested path --- the key echoes the `DerivedPath` string the client sent.
  Response structure (after the STDERR loop):
]

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`count`], [u64], [Number of result entries],
  [(per entry) `derivedPath`],
  [string],
  [The `DerivedPath` string exactly as
    the client sent it],

  [(per entry) `buildResult`], [BuildResult], [See `BuildResult` format below],
)

=== Per-Target Result Honesty

#r("gw.opcode.build-results-honest+2")[
  A derivation (`Built`) target in a `wopBuildPathsWithResults` batch MUST be
  reported successful only if every output the client requested for that
  target is valid in the rio store (tenant-scoped) at completion time, and a
  successful entry's `builtOutputs` MUST cover exactly the requested outputs
  with paths reflecting store reality (declared paths for input-addressed and
  fixed-CA outputs, the registered realisation for floating-CA outputs). A
  target that fails this verification MUST carry a failure status whose
  `errorMsg` names the missing store paths or unrealized outputs, while the
  remaining entries of the batch are still reported normally (one entry per
  requested path, in request order, no batch-wide `STDERR_ERROR` for an
  individual target's failure). The same verification MUST gate the success
  reply (`u64(1)`) of `wopBuildPaths`, and — whenever the gateway holds the
  resolved derivation — the single `BuildResult` of `wopBuildDerivation`;
  on a success gated by that verification, its `builtOutputs` MUST cover
  exactly the declared outputs (the opcode carries no client-side output
  selection).
]

This is defense in depth on top of the scheduler-side completion guarantees:
the gateway submits all targets of a batch as one combined @dag and receives
a single aggregate outcome, so without a per-target check every entry would
inherit that outcome — a target whose outputs were never produced could be
reported `Built` with fabricated `builtOutputs` (wrong-success), and a target
whose outputs are all present would be reported failed because an unrelated
target in the batch failed (partial outcome). Reporting an unrealized
floating-CA output as successful is also a client-crash bug, not just a
truthfulness one: the entry would carry an empty `outPath`, which stock Nix
rejects (`Realisation::fromJSON` parse failure / `nix-build.cc:722` assert).
Verification consults the store (one batched `FindMissingPaths` over the
union of wanted paths, plus the Realisations table for floating-CA outputs)
rather than the build-event stream because the store is what the client will
actually fetch from — events describe what the scheduler believes happened
and may be replayed across failover, while store validity at reporting time
is the contract the success status hands to the client. When the aggregate
outcome was a failure, a target is promoted to success only on positive
store evidence for every requested output; outputs that cannot be mapped to
a queryable store path leave the scheduler's outcome authoritative.

`wopBuildDerivation` (the build-hook path) is a single-target reply, so the
batch-aggregation hazards above do not apply, but the client-crash one does:
an unrealized floating-CA output reported as successful would carry an empty
`outPath` in `builtOutputs`, and in hook mode the local `nix-daemon` registers
the returned outputs in its own database, propagating the wrong-success
further than an ssh-ng client would. Verification requires the resolved
`.drv` — the modular hash that keys the Realisations lookup needs `inputDrvs`,
which the inline `BasicDerivation` lacks — so in the single-node fallback
(#rref("gw.hook.single-node-dag")) the gateway cannot verify and keeps the
pre-verification behavior: empty `builtOutputs`, scheduler outcome passed
through.

#r("gw.build.scheduler-rejection-permanent")[
  When `SubmitBuild` fails with `INVALID_ARGUMENT` or `FAILED_PRECONDITION`,
  the gateway MUST report the failure to the client as a permanent rejection
  (`BuildResult` status `InputRejected`), never as `TransientFailure`; all
  other submission failures (timeout, `UNAVAILABLE`, transport errors) remain
  transient. The gateway MUST NOT mint fallback nodes it knows the scheduler
  will reject: `build_fallback_node` refuses non-content-bound derivations.
]

A scheduler `INVALID_ARGUMENT` / `FAILED_PRECONDITION` means the scheduler
validated the submission's content and refused it --- resubmitting the
identical request can never succeed, so reporting it as transient sends
clients (and CI retry wrappers) into a retry loop against a deterministic
rejection. The producer half closes the one known way the gateway itself
could manufacture such a rejection: an inline fallback node for a
non-content-bound derivation, which the scheduler's authoritative-content
validation always refuses. The contract's load-bearing inverse is pinned
scheduler-side
(#rref("sched.merge.store-evidence-displacement+1")): conditions that are
NOT deterministic content refusals --- store silence while resolving a
settled conflict, or store-evidence fetch-budget exhaustion --- carry
`UNAVAILABLE` / `RESOURCE_EXHAUSTED` precisely so they flow through this
classifier's transient arm untouched; the gateway needs no knowledge of
those variants, and widening the permanent set here without a
scheduler-side code pin would re-create the inversion this pairing
exists to prevent.

=== BuildResult Wire Format

All fields below are present for 1.35+ except `cpu_user`/`cpu_system`, which
are gated on protocol >= 1.37 (Lix at 1.35 omits them):

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`status`], [u64], [See status enum below],
  [`errorMsg`], [string], [Error message (empty on success)],
  [`timesBuilt`], [u64], [Number of times this derivation was built],
  [`isNonDeterministic`],
  [u64 bool],
  [Whether non-deterministic output was
    detected],

  [`startTime`], [u64], [Build start time (Unix epoch)],
  [`stopTime`], [u64], [Build stop time (Unix epoch)],
  [`cpuUser`],
  [optional i64],
  [CPU user time (u64 tag: 0=absent, 1=present; if
    present, followed by u64 value interpreted as i64)],

  [`cpuSystem`], [optional i64], [CPU system time (same encoding as cpuUser)],
  [`builtOutputs`], [collection], [Output entries (see below)],
)

*BuildResult status enum:*

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Value], [Name], [Description]),
  [0], [Built], [Successfully built],
  [1], [Substituted], [Fetched from substituter],
  [2], [AlreadyValid], [Output already existed],
  [3], [PermanentFailure], [Build failed (not retryable)],
  [4], [InputRejected], [Input was rejected],
  [5], [OutputRejected], [Output was rejected],
  [6], [TransientFailure], [Build failed (may succeed on retry)],
  [7], [CachedFailure], [Previously recorded failure],
  [8], [TimedOut], [Build exceeded timeout],
  [9], [MiscFailure], [Other failure],
  [10], [DependencyFailed], [A dependency failed],
  [11], [LogLimitExceeded], [Build log exceeded size limit],
  [12], [NotDeterministic], [Non-deterministic output detected],
  [13],
  [ResolvesToAlreadyValid],
  [Derivation resolves to already valid
    output],

  [14], [NoSubstituters], [No substituters available],
)

Each *builtOutput* entry is a `(DrvOutput, Realisation)` pair:

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`drv_output_id`], [string], [DrvOutput key, e.g. `sha256:abcdef...!out`],
  [`realisation_json`],
  [string],
  [Realisation as JSON:
    `{"id":"...","outPath":"...","signatures":[],"dependentRealisations":{}}`],
)

= Wire Format

#r("gw.wire.all-ints-u64")[
  All wire integers are 64-bit unsigned little-endian. There are *no
  exceptions* --- even logically-u8 values (BuildStatus, verbosity) and magic
  bytes are sent as u64 LE.
]

#r("gw.wire.string-encoding")[
  Strings/buffers: `u64(len) + bytes + zero-pad-to-8-byte-boundary`. Empty
  strings: `u64(0)` with no bytes and no padding.
]

#r("gw.wire.collection-max")[
  Collections: `u64(count) + elements`. *Every* count-prefixed loop MUST
  enforce `MAX_COLLECTION_COUNT` before entering the loop --- not just in
  `read_strings`/`read_string_pairs`, but in any custom reader (e.g.,
  `read_basic_derivation` output loop, `read_build_result` built-outputs loop,
  STDERR trace/field readers).
]

#r("gw.wire.framed-no-padding")[
  Framed data (for NARs): sequence of `u64(chunk_len) + chunk_data` terminated
  by `u64(0)` --- chunk data is NOT padded (unlike strings).
]

#r("gw.wire.framed-max-total+3")[
  `MAX_FRAMED_TOTAL` MUST be `>= MAX_NAR_SIZE`. The gateway's `wopAddToStoreNar`
  handler gates on `nar_size ≤ MAX_NAR_SIZE` before constructing the
  `FramedStreamReader`; if `MAX_FRAMED_TOTAL < MAX_NAR_SIZE`, the reader's
  internal clamp silently shrinks the effective limit, causing NARs between the
  two bounds to fail mid-stream with a confusing framed-total error instead of
  the upfront size-gate message. A `const_assert!(MAX_FRAMED_TOTAL >=
  MAX_NAR_SIZE)` enforces the inequality at compile time. (The two are
  currently equal at 4 GiB, but the assertion is `>=` so raising
  `MAX_FRAMED_TOTAL` alone is permitted.) `wopAddMultipleToStore` uses an
  _unbounded_ `FramedStreamReader` (`new_unbounded`) --- the per-entry
  `nar_size ≤ MAX_NAR_SIZE` check inside the de-framed stream and `num_paths ≤
  MAX_COLLECTION_COUNT` are the DoS gates; the aggregate is unbounded by design
  (#glspl("closure") legitimately exceed 4 GiB; per-frame ≤ `MAX_FRAME_SIZE` and
  streaming processing keep memory bounded regardless).
]

#r("gw.wire.narhash-hex")[
  `narHash` fields on the wire are hex-encoded SHA-256 digests with *no
  algorithm prefix and no nixbase32*. Use `hex::decode` + `NixHash::new`, not
  `NixHash::parse_colon`. The `sha256:nixbase32` format appears in narinfo
  text, not on the wire.
]

- All integers: 64-bit unsigned, little-endian
- Strings/buffers: `u64(len) + bytes + zero-pad-to-8-byte-boundary`
- Empty strings: `u64(0)` with no bytes and no padding
- Collections: `u64(count) + elements`
- Framed data (for NARs): sequence of `u64(chunk_len) + chunk_data` terminated
  by `u64(0)` --- chunk data is NOT padded (unlike strings)

== Handshake Sequence (Protocol 1.38+)

#memo(title: [Correction (discovered during implementation)])[
  The original design stated that magic bytes are u32, the only exception to
  the u64 rule. This is incorrect. In the actual Nix C++ source, `readInt()` /
  `writeInt()` serialize all integers as u64 LE, *including the magic bytes*.
  The handshake uses u64 throughout, with no exceptions.
]

#r("gw.handshake.phases")[
  The handshake has three phases: magic+version exchange
  (`BasicClientConnection::handshake`), feature exchange (protocol >= 1.38),
  and post-handshake (`postHandshake`).
]

#r("gw.handshake.magic")[
  *Phase 1: Magic + Version Exchange*
]

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Step], [Direction], [Data], [Type]),
  [1], [C → S], [`WORKER_MAGIC_1` (`0x6e697863`)], [u64],
  [2], [S → C], [`WORKER_MAGIC_2` (`0x6478696f`)], [u64],
  [3],
  [S → C],
  [Protocol version (encoded as `(major << 8) | minor`, e.g.
    `0x126` = 1.38)],
  [u64],

  [4], [C → S], [Client protocol version], [u64],
)

#r("gw.handshake.version-negotiation+2")[
  The negotiated version is `min(client_version, server_version)`. If the
  client version < 1.35, the server should send `STDERR_ERROR` and close the
  connection.
]

#r("gw.handshake.features")[
  *Phase 2: Feature Exchange (protocol >= 1.38)*
]

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Step], [Direction], [Data], [Type]),
  [5], [C → S], [Client feature set], [string collection],
  [6], [S → C], [Server feature set], [string collection],
)

The feature sets are intersected to determine the negotiated features.
rio-build currently advertises an empty feature set.

*Phase 3: Post-Handshake (`postHandshake`)*

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Step], [Direction], [Data], [Type]),
  [7],
  [C → S],
  [Obsolete CPU affinity (always 0; if non-zero, followed by a
    second u64 mask)],
  [u64],

  [8], [C → S], [`reserveSpace` (always 0)], [u64],
  [9], [S → C], [Nix version string (e.g. `"rio-gateway 0.1.0"`)], [string],
  [10],
  [S → C],
  [Trusted status: 0 = unknown, 1 = trusted, 2 = not-trusted. rio always
    sends 2 (#rref("gw.handshake.untrusted"))],
  [u64],
)

#r("gw.handshake.untrusted")[
  The gateway MUST report not-trusted (`2`) in the handshake trusted-status
  field and MUST NOT report trusted.
]
rio is multi-tenant and has no CppNix trusted-user concept: reporting
trusted would invite inline input-addressed `wopBuildDerivation`
submissions whose declared output paths cannot be validated (the
path-squatting shape #rref("gw.reject.output-path-mismatch+2") exists to
prevent), while not-trusted steers stock `build-remote` (Nix ≥ 2.16,
Lix) onto the copy-the-`.drv`-closure + `wopBuildPathsWithResults` flow
that the gateway fully supports and gates. Beyond `build-remote`'s flow
selection, clients only display the flag (`nix store info`).

#r("gw.handshake.initial-stderr-last")[
  *Phase 4: Initial STDERR_LAST*
]

#table(
  columns: 4,
  align: (left, left, left, left),
  table.header([Step], [Direction], [Data], [Type]),
  [11], [S → C], [`STDERR_LAST` (`0x616c7473`)], [u64],
)

The client calls `processStderrReturn()` after the handshake, which reads
messages until `STDERR_LAST`. The server must send `STDERR_LAST` to complete
the handshake before the client will send any opcodes.

#r("gw.handshake.flush-points")[
  The server must flush after steps 2--3, after step 6, after steps 9--10, and
  after step 11. Without explicit flushes, data may remain buffered and the
  client will block waiting for the response.
]

#r("gw.handshake.timeout")[
  The gateway MUST bound the pre-handshake read with a timeout (default 30s)
  and close the channel on expiry. An authenticated client that opens a channel
  and sends the SSH `exec_request` but never sends `WORKER_MAGIC_1` would
  otherwise hold a session slot indefinitely (russh keepalives keep the
  transport alive, and the inter-opcode idle timeout only applies after the
  handshake completes).
]

= Protocol Multiplexing

#r("gw.conn.sequential")[
  The Nix worker protocol is strictly sequential within a single connection ---
  the client sends a request, waits for the full response (including the STDERR
  streaming loop), then sends the next request. There is no pipelining or
  out-of-order execution.
]

#r("gw.conn.per-channel-state")[
  Multiple clients require multiple SSH channels or connections. The gateway
  multiplexes at the SSH channel level (one protocol session per channel), not
  at the protocol level. Each SSH channel has independent protocol state,
  including separate handshake and option negotiation. During long
  `wopBuildDerivation` calls, the connection is blocked (the STDERR loop runs
  for the duration of the build). Nix handles this by opening separate SSH
  channels for concurrent operations (e.g., IFD during evaluation).
]

= DAG Reconstruction

#r("gw.dag.reconstruct+4")[
  When the gateway receives `wopBuildDerivation`, `wopBuildPaths`, or
  `wopBuildPathsWithResults`, it reconstructs the full derivation DAG to send
  to the scheduler via `SubmitBuild`. `wopBuildDerivation` (build-hook path)
  attempts the same full-DAG walk: it resolves the `.drv` from the session
  cache or the store and runs `reconstruct_dag`; only if resolution fails does
  it fall back to a single-node DAG built from the inline `BasicDerivation`,
  and only when every statically-declared output of that derivation is
  content-bound (fixed-output / floating-CA) --- an unresolvable inline
  *input-addressed* derivation is rejected rather than trusted
  (see #rref("gw.hook.single-node-dag+2")). Alongside the node/edge walk, the
  gateway computes each node's `wanted_output_names` --- the union of every
  consumer's `inputDrvs` output-name references for it plus the root request's
  output selection, empty meaning every declared output --- which feeds the
  scheduler's demand-driven cache-hit classification
  (#rref("sched.merge.wanted-outputs")). The algorithm:
]

+ *During store uploads:* The gateway intercepts each uploaded path. If the
  path ends in `.drv`, the gateway extracts the `.drv` file from the NAR,
  parses the ATerm-format derivation, and caches the parsed result in
  per-session memory (keyed by store path). This applies to all upload opcodes:
  `wopAddToStore` (7), `wopAddTextToStore` (8), `wopAddToStoreNar` (39), and
  `wopAddMultipleToStore` (44). For `wopAddToStoreNar` and
  `wopAddMultipleToStore`, the handler branches on the path name: `.drv` paths
  (small --- typically \<10KB, capped at `DRV_NAR_BUFFER_LIMIT` = 16MiB) are
  buffered and parsed via `try_cache_drv`; non-`.drv` paths stream directly to
  the store via `grpc_put_path_streaming` without buffering. `wopAddToStore`
  and `wopAddTextToStore` still buffer (they compute the store path from the
  content hash, so need the full bytes before `PutPath` metadata). A `.drv` NAR
  exceeding `DRV_NAR_BUFFER_LIMIT` is streamed without caching ---
  `resolve_derivation` fetches it from the store later during DAG
  reconstruction.
+ *On `wopBuildDerivation`/`wopBuildPathsWithResults`:* The gateway identifies
  all requested derivation paths. For each, it looks up the parsed derivation
  from the session cache (step 1). If a `.drv` was not uploaded in the current
  session (e.g., it was uploaded in a previous session and already exists in
  the store), the gateway fetches it from rio-store via `GetPath`, unpacks the
  NAR, and parses the ATerm.
+ *DAG construction:* Starting from the requested derivation(s), the gateway
  walks `inputDrvs` references recursively (BFS) to build the full DAG. *DAG
  reconstruction is capped at 1,048,576 transitive input derivations*
  (`DEFAULT_MAX_TRANSITIVE_INPUTS`, overridable via
  `RIO_MAX_TRANSITIVE_INPUTS`) to prevent DoS via pathological derivation
  graphs. The gateway sends the *full DAG* to the scheduler; cache-hit
  determination (which nodes have outputs already in the store) happens in the
  scheduler, not here --- the one exception is the bounded tenant-scoped
  realization probe that decides unverifiable-hash-algo offenders
  (#rref("gw.reject.unsupported-hash-algo+4")).
+ *Validation:* Malformed `.drv` files and missing `.drv`
  files (referenced by `inputDrvs` but not in the store) are rejected via
  `BuildResult::failure` delivered through `STDERR_LAST` --- the session stays
  open, subsequent opcodes are accepted. (Previously `STDERR_ERROR` terminal;
  changed in remediation-07 to avoid the ERROR→LAST desync when called from
  `wopBuildPaths`/`wopBuildPathsWithResults`, which wrap the error.)
  `validate_dag` enforces the cheap submission gates before the gRPC
  round-trip:
  - `nodes.len() > MAX_DAG_NODES` (scheduler enforces this too, but
    gateway-side early reject saves the submission);
  - any derivation with `__noChroot=1` in its env (sandbox escape --- this
    check is ONLY at the gateway; the scheduler does not re-check;
    #rref("gw.reject.nochroot+2"));
  - any output declaring an `outputHash`/`outputHashAlgo` the builder cannot
    verify or finalize, unless every declared output of that derivation is
    already realized for the submitting tenant or substitutable from its
    upstreams --- a single bounded tenant-scoped `FindMissingPaths` probe
    decides, fail-closed (#rref("gw.reject.unsupported-hash-algo+4"));
  - any floating-CA-shaped output (`outputHashAlgo` set, `outputHash` empty)
    that nevertheless declares an output path — a shape CppNix refuses to
    parse (#rref("gw.reject.floating-ca-declared-path"));
  - declared-hash (fixed-output) outputs whose declared path does not derive
    from the declared hash, including CppNix's single-`out` shape rule
    (#rref("gw.reject.output-path-mismatch+2")).
  The expensive input-addressed output-path binding over the full closure
  runs later in the pipeline --- after the per-tenant rate-limit and quota
  gates, on the blocking pool --- but still before `SubmitBuild`
  (#rref("gw.reject.output-path-mismatch+2")). `validate_dag` is invoked from
  all three build handlers (`wopBuildDerivation`, `wopBuildPaths`,
  `wopBuildPathsWithResults`).
+ *The reconstructed DAG is sent to the scheduler via `SubmitBuild`.* The
  gateway holds the SSH connection open and converts the `BuildEvent` response
  stream into STDERR messages for the Nix client.

Alongside `wanted_output_names`, the gateway also marks every node the client
named as a build target (`explicitly_requested`: the BFS root of each
requested target's sub-DAG, the `wopBuildDerivation` single-node fallback,
OR-merged across duplicate copies when multi-target sub-DAGs are deduped into
one submission), so the scheduler's top-down prune
(#rref("sched.merge.substitute-topdown")) retains and verifies a requested
target even when another target's closure swallows it as a non-root.

#r("gw.dag.modulo-hash-all-nodes")[
  After DAG reconstruction, the gateway MUST populate `ca_modular_hash`
  (the `hashDerivationModulo` SHA-256) on every submitted node whose full
  derivation is in the BFS cache --- content-addressed, deferred
  input-addressed, and plain input-addressed nodes alike. Hash-computation
  failure for a node degrades that node only (empty hash, warning logged);
  it MUST NOT fail the submission.
]
Plain IA nodes previously carried no hash ("dead bytes on the wire"),
which made every declared IA output path unverifiable at scheduler
ingress: verifying `path == input_addressed_output_paths(bytes)` requires
the modulo hashes of the node's inputs, and those inputs' own nodes are
the only place the scheduler can get them without store access. With
every node carrying its hash, the scheduler can seed the derivation-hash
cache from sibling nodes and bind declared IA paths to inline bytes
(#rref("sched.merge.ingress-inline-drv-binding")), and the follow-up
store-evidence displacement can compare a store-fetched `.drv` against
the persisted hash. The per-node degrade keeps the prior availability
semantics: a node whose closure the gateway could not fully parse
submits without evidence, and the scheduler's fail-closed rules decide
what that node may then claim.

#r("gw.reject.nochroot+2")[
  The gateway MUST reject any derivation (at SubmitBuild time) whose env
  contains `__noChroot = "1"` --- and, fail-closed, any derivation whose
  `__noChroot` is present but not a JSON boolean or whose `__json` blob does
  not parse: a sandbox-shape attribute the gateway cannot type is rejected,
  never guessed at. This is a sandbox-escape request that rio-build
  does not honor. Rejection happens at two points with different frame
  semantics: (1) `validate_dag` rejects via `BuildResult::failure` →
  `STDERR_LAST` (opcodes 36/46 wrap the error; session stays open); (2)
  `wopBuildDerivation`'s inline check sends `STDERR_ERROR` terminal (opcode 9
  doesn't wrap --- this is a protocol-level reject). The scheduler does not see
  the `__noChroot` env (DerivationNode doesn't carry it), so this check is
  gateway-only.
]

The fail-closed clause is oracle parity, not extra strictness: CppNix's
`getBoolAttr("__noChroot")` routes through `getBoolean`, which THROWS on a
non-boolean — there is no coercion of `1` or `"true"`, and an unparseable
structured-attrs blob fails the build before options are read. The pre-fix
gateway read the attribute through a lenient accessor where a wrong-typed
value (or a malformed blob) degraded to "absent" — i.e. exactly the
derivations whose sandbox intent could not be read were the ones waved
through.

#r("gw.reject.unsupported-hash-algo+4")[
  The gateway MUST reject at submission any derivation output declaring an
  `outputHash` and/or `outputHashAlgo` that the builder cannot verify
  (fixed-output) or finalize (floating-CA) --- the supported set is `sha1`,
  `sha256`, `sha512`, each optionally `r:`-prefixed, mirroring
  #rref("builder.fod.verify-hash+2") and the floating-CA finalization rules
  --- unless every declared output path of that derivation is already
  present and visible to the submitting tenant, or substitutable from that
  tenant's configured upstreams, verified by a single bounded
  `FindMissingPaths` probe at submission time that carries the session
  tenant token (anonymous only in dual-mode sessions, matching the
  scheduler's anonymous merge-time probe in that mode). The exemption is
  fail-closed: a floating-CA output with an unsupported algorithm (no
  declared path), an empty or unparseable declared path, an offender set
  larger than the probe cap, an indeterminate probe answer, a probe error,
  or a probe timeout all reject the submission. The rejection applies both
  in `validate_dag` over cached full derivations (`BuildResult::failure` →
  `STDERR_LAST`) and inline on `wopBuildDerivation`'s `BasicDerivation`
  (`STDERR_ERROR`); the probe-backed exemption applies only where a
  resolvable `.drv` binds the claim — on the inline single-node fallback
  (full `.drv` unresolvable) the gateway MUST reject an unverifiable-algo
  offender even when its declared outputs are already realized, with
  remediation naming both options (copy the outputs directly, or upload
  the `.drv` first so the cached-DAG exemption applies).
]
Both code paths are fail-closed on the worker side too, but only after the
build has burned a pod; rejecting at submission lands the error on the
submitting client immediately. The exemption exists because a derivation
whose declared outputs are all present (and visible to the submitting
tenant) or substitutable from its upstreams never dispatches --- the
scheduler cache-cuts it or completes it through the substitute lane --- so
rejecting the whole submission for a legacy (e.g. `md5`) fixed-output
derivation that already exists for that tenant would block otherwise-valid
DAGs that merely reference it. The exemption mirrors the scheduler's
no-dispatch predicate (tenant-scoped cache-cut OR substitute lane),
evaluated with the same tenant identity the submission itself will carry,
and never lets a node that would actually build escape the gate. Residual
divergences, accepted deliberately: a garbage-collection race between the
probe and dispatch makes the node dispatch and fail at the worker's
fail-closed FOD gate (a node-level failure instead of a submission
rejection); a substitute fetch that fails after a positive upstream probe
ends the same way; and dual-mode sessions probe anonymously, which matches
the scheduler's anonymous merge-time probe in that mode. The exemption
deliberately does not extend to the inline single-node fallback: with no
resolvable `.drv` nothing binds the claimed `drv_path`, output names, or
declared paths to real derivation text, so forwarding the claim would mint
an unvalidated node under a client-chosen `drv_path` — the squat shape the
scheduler's merge protections (`sched.merge.authoritative-conflict`) exist
to prevent — and the rejection's remediation (upload the `.drv`, or copy
the already-realized outputs directly) restores the exemption on the
cached-DAG path.

#r("gw.reject.floating-ca-declared-path+1")[
  A floating content-addressed output (`outputHashAlgo` set, `outputHash`
  empty) declaring a non-empty output path MUST be rejected, and the
  enforcement point is the typed parse boundary
  (#rref("nix.drv.output-typed")): the shape fails `Derivation` /
  `BasicDerivation` construction with CppNix's own wording
  ("content-addressing derivation output should not specify output path"),
  so neither the session derivation cache nor the inline
  `wopBuildDerivation` payload can ever contain it. The gateway MUST
  surface the parse failure as a rejection — `STDERR_ERROR` then close for
  the mid-payload inline case — rather than masking it.
]
No legitimate client can produce this shape, so the rule rejects only
crafted submissions; proper floating-CA outputs (empty declared path) are
unaffected. Accepting it would have exempted the declared path from both
the input-addressed and the declared-hash output-path bindings.

The binding gates dispatch on the typed output model: shape rules
(malformed paths, floating-with-path, fixed-without-path, mixed sets) are
enforced once at the parse boundary (#rref("nix.drv.output-typed"),
#rref("nix.drv.type-classify+1")), and the gateway's validators retain only
the SEMANTIC half — deriving paths from derivation or declared-hash
contents and comparing. Re-introducing a divergent shape classification in
the gateway would require re-adding data the types no longer carry.

#r("gw.reject.output-path-mismatch+2")[
  The gateway MUST NOT trust declared output paths. For input-addressed
  outputs it MUST re-derive the paths from the derivation contents
  (`hashDerivationModulo` + output-path derivation, CppNix parity) over the
  submitted closure and reject any mismatch before `SubmitBuild`; a non-empty
  declared output path that does not parse as a store path MUST be rejected
  (empty paths — deferred shapes — are skipped); the check MUST be
  fail-closed when derivation is impossible (incomplete `inputDrvs` closure,
  mixed content-bound and static-path shapes), and submissions MUST NOT be
  rejected on derivation-chain depth (only the documented size caps apply).
  For declared-hash (fixed-output) outputs it MUST require the declared path
  to equal the path derived from the declared hash (`makeFixedOutputPath`)
  and enforce CppNix's fixed-output shape rule (exactly one output, named
  `out`). On `wopBuildDerivation`'s single-node fallback (full `.drv`
  unresolvable) the gateway MUST reject inline derivations with ANY
  input-addressed output — declared, malformed, or deferred (empty) — and
  MUST apply the same declared-hash binding to inline fixed-output
  declarations.
]
Workers are untrusted (#rref("sec.trust.workers-untrusted")), so this is the
authoritative half of output-path enforcement; the builder-side checks are
defense in depth and the store additionally verifies content-addressed
uploads at registration time. Malformed declared paths cannot alias a store
object, but they CAN reach the worker glue and the result pipeline as
tenant-controlled strings where store paths are expected — rejecting them at
the trusted plane is what keeps every downstream `Path::join` over a declared
path total. Content-bound outputs (fixed-output / floating-CA) have no static
derivation-derived path; their binding is the declared-hash rule above and
the store-side content verification, and a floating-CA output that
nevertheless declares a path is rejected outright
(#rref("gw.reject.floating-ca-declared-path")) rather than exempted.

#r("gw.dag.drv-cache-text-ca")[
  The gateway MUST NOT use cached derivation content whose text
  content-address does not match the store path it was uploaded under: when
  intercepting `.drv` uploads for the session derivation cache, content
  whose recomputed text-CA store path differs from the claimed path MUST NOT
  be cached under that path (the upload itself is rejected by the store's
  text-CA verification).
]
This keeps the copy of a derivation that the gateway validates identical to
the copy workers fetch from the store --- a tenant must not be able to make
the two diverge.

#r("gw.reject.build-mode")[
  The gateway MUST reject `wopBuildDerivation` / `wopBuildPaths` /
  `wopBuildPathsWithResults` with `STDERR_ERROR` when `build_mode ≠ Normal`.
  Repair/Check semantics are not representable in `SubmitBuildRequest`;
  silently downgrading to `Normal` yields a false-positive reproducibility
  (`--rebuild`) or repair result.
]

== Inline .drv Optimization

After DAG construction, the gateway optionally inlines the ATerm content of
`.drv` files into the `drv_content` field of each `DerivationNode`. This saves
one executor → store round-trip per dispatched derivation (the `GetPath`
fetch). The optimization:

- Is *gated by a single batched `FindMissingPaths` call* over all expected
  output paths. Only nodes with at least one missing output (i.e., nodes that
  will actually dispatch) are inlined. Cache-hit nodes stay empty --- the
  scheduler short-circuits them to `Completed` and they never dispatch.
- Applies a *per-node cap of 64 KB* (`MAX_INLINE_DRV_BYTES`). Larger `.drv`
  files (e.g., flake inputs serialized into `env`) fall back to executor-fetch.
- Applies a *total budget of 16 MB* (`INLINE_BUDGET_BYTES`) across all inlined
  nodes. Once the budget is exhausted, remaining nodes fall back to
  executor-fetch.
- Is *best-effort*: on any error (`FindMissingPaths` timeout, store
  unreachable), inlining is skipped entirely and all nodes fall back to
  executor-fetch. This is an optimization, not a correctness requirement.

#info(title: [Session state])[
  Although the gateway is described as "stateless beyond the lifetime of a
  single SSH connection," each SSH channel does accumulate per-session state:
  the parsed `.drv` cache and the `wopSetOptions` configuration.
  `wopAddTempRoot` is acknowledged as a no-op (rio's GC is store-side with
  explicit pins; a gateway-session-scoped set would be invisible to it). This
  state is connection-scoped and discarded when the SSH channel closes.
]

= Authentication + Tenant Identity

#r("gw.auth.tenant-from-key-comment")[
  The tenant name lives in the *server-side `authorized_keys` entry's comment
  field*, not the client's key (SSH key authentication sends raw key data
  only). During `auth_publickey`, the gateway matches the client's presented
  key against its loaded entries via `.find()`, then reads `.comment()` from
  the *matched entry* to get the tenant name. This is stored on the connection
  and passed through to `SubmitBuildRequest.tenant_name`. Empty comment =
  single-tenant mode (tenant name is empty string → scheduler treats as
  `None`).
]

#r("gw.keys.hot-reload")[
  `authorized_keys` is hot-reloaded by a background watcher that polls the
  file's mtime every `AUTHORIZED_KEYS_POLL_INTERVAL` (10s). On change, the file
  is re-parsed and atomically swapped into the shared
  `ArcSwap<Vec<PublicKey>>`; in-flight SSH handshakes see the new set on their
  next `auth_publickey_offered` call (each `.load()` reads the current `Arc`).
  *mtime polling, not inotify*: kubelet refreshes Secret mounts via a `..data`
  symlink swap that an `IN_MODIFY` watch on the file path never sees;
  `std::fs::metadata` follows symlinks so the swap surfaces as a changed mtime.
  *Reload failures keep the old set*: an empty/all-invalid/transiently-unreadable
  file logs WARN and retries next tick --- never swap to an empty set (would
  lock everyone out). I-109: prior to this, rotating a tenant key required a
  pod restart.
]

#r("gw.jwt.claims")[
  JWT claims: `sub` = tenant_id UUID (server-resolved at mint time), `iat`,
  `exp` (SSH session duration + grace), `jti` (unique token ID for revocation).
  Signed ed25519, public key distributed via ConfigMap.
]

#r("gw.jwt.issue")[
  On successful SSH authentication, the gateway MUST mint a JWT with `sub` set
  to the resolved tenant UUID and store it on the session context. The
  scheduler reads `jti` from the interceptor-attached `Claims` extension (per
  #rref("gw.jwt.verify") below) --- NO proto body field. For audit, the
  `SubmitBuild` handler INSERTs `jti` into `builds.jwt_jti` (column added in
  migration 016). Zero wire redundancy: `jti` lives once in the JWT, parsed
  once by the interceptor, read once by the handler.
]

#r("gw.jwt.refresh-on-expiry+2")[
  The gateway MUST re-mint the session JWT before injecting it on any outbound
  gRPC call if the cached token is within 5 minutes of `exp`. Refresh is
  checked lazily on every token access (`SessionJwt::token()`): both a new
  channel on a `ControlMaster`-mux'd connection AND a single channel whose
  lifetime exceeds `JWT_SESSION_TTL_SECS` (e.g. a long `wopBuildDerivation` ---
  keepalive resets `inactivity_timeout`, so the SSH layer never drops it)
  re-mint between opcodes. Re-mint is local (`sub` and the signing key are
  already on hand) --- no `ResolveTenant` round-trip. The refreshed token gets a
  fresh `jti`; the old `jti` is NOT revoked (it expires naturally).
]

#r("gw.jwt.verify")[
  The tonic interceptor on scheduler and store MUST extract
  `x-rio-tenant-token`, verify signature+expiry, attach `Claims` to request
  extensions, and reject invalid tokens with `Status::unauthenticated`.
  (Controller has no gRPC ingress --- kube reconcile loop + raw-TCP /healthz
  only.) The scheduler ADDITIONALLY checks `jti NOT IN jwt_revoked` (PG lookup
  --- gateway stays PG-free).
]

#r("gw.jwt.dual-mode+2")[
  Gateway auth is two-branched PERMANENTLY: `x-rio-tenant-token` header present
  → JWT verify; absent → SSH-comment fallback. Operator selects the deployment
  posture via the *two `[jwt]` knobs in `gateway.toml`* --- `key_path` and
  `required` (there is no separate `auth_mode` enum): `key_path` absent →
  comment-fallback only (no JWT minting); `key_path` set with `required=false`
  → mint-with-degrade (JWT minted on auth, downstream falls back to comment if
  verify fails); `key_path` set with `required=true` → mint-or-reject. Both
  paths stay maintained. (Does NOT bump #rref("gw.auth.tenant-from-key-comment").)
]

#r("gw.jwt.propagate")[
  Every gateway-originated gRPC to rio-scheduler or rio-store MUST be wrapped
  via `with_jwt`, including reconnect (`WatchBuild`), cleanup (`CancelBuild`),
  and post-build resolution (`QueryRealisation`) paths. `with_jwt` is the
  single injection point for both W3C `traceparent` and `x-rio-tenant-token`; a
  bare-struct call site silently loses both (orphan trace span today, hard auth
  failure once downstream tenant authz lands).
]

#r("gw.jwt.anon-drv-lookup")[
  Read-path opcodes (`wopIsValidPath`, `wopEnsurePath`, `wopQueryPathInfo`,
  `wopQueryValidPaths`) MUST send the JWT to the store *except for `.drv`
  paths*, which are looked up anonymously (`jwt_unless_drv`). `.drv` files are
  build INPUTS, not tenant-owned OUTPUTS: a `.drv` uploaded under one identity
  then queried under another has no `path_tenants` row for the querying tenant,
  so a tenant-filtered `QueryPathInfo` would return NotFound for a `.drv` the
  client just uploaded. Output paths keep tenant-scoped visibility
  (#rref("store.tenant.narinfo-filter")); only the `.drv` lookup is exempt.
]

= Connection Lifecycle

#r("gw.conn.lifecycle")[
  Each SSH channel follows this lifecycle:
]

#figure(
  automaton(
    (
      handshake: (ready: "ok", closed: "mismatch"),
      ready: (ready: "op", closed: "eof"),
      closed: none,
    ),
    initial: "handshake",
    final: ("closed",),
    input-labels: (
      ok: [handshake complete\ (magic + version + features\ + postHandshake + `STDERR_LAST`)],
      op: [any opcode\ (`wopSetOptions`, query,\ upload, build)],
      eof: [SSH channel closed],
      mismatch: [version mismatch\ (`STDERR_ERROR`)],
    ),
    state-format: name => name,
    style: (
      handshake: (initial: [SSH channel opened\ + exec `nix-daemon --stdio`]),
      handshake-ready: (curve: 0.6),
      handshake-closed: (curve: -1.2, label: (pos: 0.5, dist: -0.4)),
      ready-closed: (curve: 0.6),
      state: (radius: 1.1),
    ),
    layout: finite-layout.linear.with(spacing: 4.5),
  ),
  caption: [SSH channel lifecycle.],
)

#memo(title: [Correction])[
  The original design required `wopSetOptions` as the mandatory first opcode
  after handshake. In practice, the real `nix-daemon` does not enforce this ---
  it accepts any opcode after the handshake completes. Nix clients
  conventionally send `wopSetOptions` first, but may send other opcodes (e.g.,
  `wopQueryMissing`) first on multiplexed SSH channels. rio-build accepts any
  opcode after handshake.
]

#r("gw.conn.exec-request")[
  *SSH transport:* Nix connects via `ssh ... nix-daemon --stdio`. The gateway
  must handle `exec_request` for this command and start the protocol on the SSH
  channel data stream. The `channel_open_session` alone does not start the
  protocol.
]

The gateway matches the *suffix* of the second-to-last whitespace-separated
argument (`ends_with("nix-daemon")`) and requires the last argument to be
exactly `--stdio`. This allows clients that send a full store path (e.g.,
`/nix/store/...-nix-2.20.0/bin/nix-daemon --stdio`) to connect successfully.

#r("gw.conn.exit-status+3")[
  When the protocol handler returns, the gateway MUST send `exit-status` (RFC
  4254 §6.10) on the channel before `eof`/`close`; openssh under
  `ControlMaster` waits for `exit-status` before its foreground process returns
  to the parent (nix), so omitting it leaves `nom build` hung until
  `ControlPersist` expires. A rejected `exec_request` MUST send
  `channel_failure` followed by `exit-status 1` + `eof` + `close` for the same
  reason. When a connection has had zero active protocol sessions (admitted
  `nix-daemon --stdio` execs) continuously for `EMPTY_CONNECTION_GRACE`
  (60 s), the gateway MUST send `SSH_MSG_DISCONNECT` so an abandoned TCP
  socket closes promptly instead of waiting for `inactivity_timeout` (which an
  idle-but-keepalive-answering client never trips). Emptiness is measured on
  exec'd sessions, not open SSH channels: a channel that is opened but never
  exec'd has no protocol task and therefore no handshake/idle deadline of its
  own, so it MUST NOT count as activity. A connection that has not completed
  authentication within the same grace period MUST likewise be bounded:
  dropped outright if it has not yet completed the SSH version exchange,
  otherwise sent `SSH_MSG_DISCONNECT` as soon as the transport is able to
  deliver it (russh provides no login-grace deadline, and the pre-auth phase
  has no channels or sessions for the empty-connection clock to key on). The
  gateway MUST NOT disconnect the instant the last session ends: a
  `ControlMaster` mux's in-flight session count transits through zero between
  builds, and killing its transport there makes the master exit, OpenSSH
  unlink the "stale" control socket, and every remaining nix process in the
  batch silently fall back to a direct connection whose handshake is
  corrupted by Nix's `LocalCommand` --- one touch-zero event poisons the rest
  of a 64-worker run.
]

"As soon as the transport is able to deliver it" is a real qualifier: russh
only drains server-queued messages (including this disconnect) between key
exchanges, so a peer that keeps a key exchange perpetually in flight defers
delivery for as long as it keeps the exchange active --- an upstream russh
constraint --- and the transport's own limits never step in for such a peer,
because every packet it trickles (e.g. `SSH_MSG_IGNORE`) resets both the
keepalive failure counter and the inactivity timer. Delivery is also only
half the story: a peer that does receive the disconnect can simply never
close its end, and russh then waits in a post-disconnect drain-read loop
that has no timeout and arms no keepalives. The bound is therefore
delivered in two stages for both the pre-auth and the authenticated
populations: the polite `SSH_MSG_DISCONNECT` the moment the gateway decides
the connection must go (the pre-auth deadline or the empty-connection
grace), then a forced transport close `FORCE_CLOSE_SLACK` (5 s) later if
the connection is still open --- the accept-site read deadline covers the
never-authenticated peer, and the same wrapper enforces a force-close
deadline armed whenever the gateway queues a disconnect for the connection
(the empty-connection grace timer, and the authentication-timeout path ---
whose target may finish authenticating after the disconnect was queued and
thereby leave the pre-auth deadline's reach, so the decision itself must
arm the bound), so the failed read ends russh's session loop (or its drain
loop) and releases the connection slot and fd through the normal drop
path. One arming site queues no disconnect at all: a protocol session
whose channel-data or close-out sends the connection's handle queue has
refused to accept for `HANDLE_SEND_TIMEOUT` (\~300 s --- far beyond any
legitimate congestion on a fully loaded multiplexed connection) treats
the transport as wedged, releases its session capacity, and arms the
same force-close --- a queue in that state could not have delivered a
polite disconnect anyway. The wrapper enforces these deadlines on both
the read and the write path of the transport: once a session is exec'd
the gateway streams bulk
channel data (build logs, NAR bytes) to the client through the same
stream, and russh awaits that write inline --- a peer that simply stops
reading at the TCP level parks the session loop in the write, where the
read-side check would never run again. A deadline armed only after the
loop is already parked in such a write is beyond every in-process timer
(none of them get polled any more), so the gateway also sets a
kernel-level `TCP_USER_TIMEOUT` on every accepted socket, aligned with the
keepalive bound (\~300 s), and the kernel errors the connection out once
its data has been undeliverable for that long. That forced close at grace
+ slack is what reaps a peer that stalls or squats mid-exchange --- long
before `keepalive_max` (\~300 s) would --- leaving keepalive as the
backstop only for connections the gateway has not (yet) decided to
disconnect, and `TCP_USER_TIMEOUT` as the equally-sized backstop for a
peer that wedges the transport itself by refusing to read.

#r("gw.conn.session-error-visible")[
  Any error propagated from an SSH handler method (via `?`) is logged at
  `error!` and increments #(refs.metric)("rio_gateway_errors_total")`{type="session"}`. The russh
  default swallows these silently.
]

#r("gw.conn.cancel-on-disconnect+2")[
  The gateway MUST send `CancelBuild` to the scheduler for every build in
  `active_build_ids` when an SSH channel drops, via ALL disconnect shapes: (1)
  clean EOF between opcodes --- `session.rs` opcode-read returns
  `UnexpectedEof`, iterates the map; (2) russh `channel_close` callback ---
  `ChannelSession::Drop` fires a graceful-shutdown signal that `session.rs`
  selects on and runs the same cancel loop; (3) `OPCODE_IDLE_TIMEOUT` expiry
  --- `session.rs` idle-timer fires after 600s with no opcode, runs the same
  cancel loop before returning. All three paths MUST complete the cancel loop
  before the protocol task exits; hard `abort()` on the task handle defeats
  this. Builds not cancelled leak an executor slot until
  #rref("sched.backstop.timeout").
]

#r("gw.conn.channel-limit+4")[
  A single SSH connection may have at most `max_channels_per_connection`
  (default 512) channels open at the SSH level; a connection that attempts to
  exceed the bound MUST be terminated --- the `channel_open_session` handler
  returns an error and the SSH session ends. The
  count MUST be taken at `channel_open_session`/`channel_close` (SSH-level
  opens), not from the exec'd-session map, so a burst of opens with no exec is
  bounded too. This is an absurdity bound on a channel-leaking or hostile
  client, NOT a resource bound --- resource protection is
  #rref("gw.conn.session-cap"), because an attacker distributes sessions
  across the #rref("gw.conn.cap") allowed connections and only a global cap
  bounds the instance. The bound MUST sit far above any legitimate
  `ControlMaster` fan-out: one mux'd connection legitimately carries one
  channel per concurrent nix process on the client machine (64--128 for a CI
  box running `nix-fast-build`).
]

Termination rather than per-open refusal: russh allocates and registers a
channel's state for any non-error handler result --- a refusal included ---
and never frees it for a refused open (the client sends no `CHANNEL_CLOSE`
for an open that failed, and russh's open-failure removal only covers
server-initiated opens), so answering each over-bound open with
`SSH_MSG_CHANNEL_OPEN_FAILURE` is an unbounded per-connection memory leak ---
the client just keeps looping opens. Ending the connection is the only
response that keeps russh-side state bounded. The `ControlMaster`-fallback
concern that motivates the polite exec-time refusal of
#rref("gw.conn.session-cap") (OpenSSH silently falls back to a direct
connection where Nix's unconsumed `LocalCommand` output lands in front of the
worker-protocol handshake --- `protocol mismatch, got 'started\noixd'`) does
not apply to a client already 512 channels deep, which this rule already
characterizes as leaking or hostile.

The previous revision capped this at 4 "to match Nix's default `max-jobs`".
That rationale was wrong on both ends: `max-jobs` controls local build
parallelism (one `nix build -j64 --store ssh-ng://` opens *one* channel), and
the thing that does stack channels on one connection --- N independent nix
processes behind the user's `ControlMaster` --- is unbounded and legitimate.
Like #rref("gw.conn.keepalive+2") (I-161), this was an SSH-hardening limit
calibrated to an assumption about stock-client behavior that stock clients
violate.

#r("gw.conn.channel-types")[
  The gateway accepts only `session` channel opens. Any other channel-open
  type (`direct-tcpip`, `x11`, `forwarded-tcpip`, `direct-streamlocal`) MUST
  terminate the connection: the handler returns an error and the SSH session
  ends. These channel types are never part of the build-submission protocol,
  and refusing one per-open would leave per-connection russh state unbounded
  for the same reason as #rref("gw.conn.channel-limit").
]

The same russh behavior drives both rules: a refused open's channel state is
registered in the per-connection map for any non-error handler result and
never freed, and non-session opens are not even counted toward
`max_channels_per_connection` (only accepted session opens are), so a client
looping `direct-tcpip` opens would leak with no bound ever tripping.
Terminating is the only bounded response. The clients that send these are
either hostile or carry a stray `LocalForward`/`DynamicForward`/ProxyJump
configuration pointed at the gateway --- a forward this single-purpose
`nix-daemon --stdio` ingress was never going to honor, so a terminated
connection (instead of a politely refused open) is an acceptable outcome for
them.

#r("gw.conn.session-cap+2")[
  The gateway MUST bound the total number of concurrently active protocol
  sessions across all connections on the instance (`max_sessions`, default
  4096). The bound MUST be enforced at `exec_request` time, before the
  session's buffers are allocated, by rejecting the exec per
  #rref("gw.conn.exit-status+3") --- never by refusing the channel open. An
  exec-time `channel_failure` is a clean `ssh` exit for a `ControlMaster` mux
  client (the master has already reported `MUX_S_SESSION_OPENED` to its
  client by the time the exec reply arrives), while a channel-open refusal
  triggers OpenSSH's silent fallback to a corrupted direct connection. The
  cap bounds the per-session steady-state cost (\~550 KiB of duplex buffers,
  \~2.2 GiB at the default), and it is only an effective memory backstop
  because each session's egress is itself flow-controlled: the gateway MUST
  NOT hand russh more channel data than the client has granted SSH window
  for (response sends go through the channel's window-aware writer and
  block until window is available, bounded by the wedged-send timeout), so
  per-session buffering inside russh stays at roughly one client-advertised
  window plus the bounded per-connection handle queue rather than growing
  with the response stream. The remaining large per-session allocation ---
  the transient fully-buffered NAR (≤ `MAX_NAR_SIZE`) a `wopNarFromPath`
  holds while in flight, required because the protocol cannot signal an
  error after raw NAR bytes start --- is deliberate and sits outside both
  bounds. The cap is per instance: horizontal scaling adds aggregate
  capacity for additional client connections, but a `ControlMaster` pins
  all of its channels to one instance's TCP connection, so the cap must
  accommodate the largest single multiplexed client on its own.
]

#r("gw.conn.keepalive+2")[
  The gateway sends SSH keepalive requests every 30 seconds. After 9
  consecutive unanswered keepalives (\~300 s --- russh increments then compares
  with `>`, so the drop fires at `interval × (max+1)`), the connection is
  closed. This detects half-open TCP that kernel-level keepalive would not.
  I-161: `keepalive_max` was 3 (=120 s), which fired during a client's
  cold-eval idle window over the SSM-tunnel path; raised so direct `nix --store
  ssh-ng://` clients without `ServerAliveInterval` get a 5-minute budget.
]

#r("gw.conn.nodelay")[
  TCP_NODELAY is set on all accepted sockets. The worker protocol's
  small-request/small-response pattern interacts pathologically with Nagle's
  algorithm (\~40 ms added per round-trip).
]

#r("gw.conn.real-connection-marker")[
  #(refs.metric)("rio_gateway_connections_total")`{result="new"}` and
  #(refs.metric)("rio_gateway_connections_active") count connections that
  reached the SSH authentication layer (any `auth_*` callback). TCP probes that
  close before the SSH handshake are logged at `trace!` only.
]

#r("gw.health.readiness-gated")[
  The gateway's gRPC health endpoint MUST report `NOT_SERVING` for the
  empty-string service from process start until `connect_forever` has
  established both store and scheduler channels, and MUST report `SERVING`
  thereafter (until drain). tonic-health's `health_reporter()` initializes `""`
  to SERVING --- the gateway explicitly flips it to `NOT_SERVING` immediately
  after construction via `rio_common::server::health_reporter_not_serving`.
]

#r("gw.conn.session-drain")[
  On SIGTERM, the gateway sets readiness `NOT_SERVING`, waits
  `drain_grace_secs` for the load balancer to deregister, stops accepting new
  SSH connections, then waits up to `session_drain_secs` for open sessions to
  close on their own before exiting. Stopping the accept loop must not
  disconnect already-established sessions --- `nix build --store ssh-ng://`
  clients with builds in flight stay connected until their build completes or
  the session-drain timeout expires.
]

#r("gw.drain.three-stage")[
  Shutdown is three-staged: (1) `spawn_drain_task` flips health `NOT_SERVING`,
  sleeps `drain_grace_secs`, then fires `serve_shutdown` → the SSH accept loop
  returns but spawned per-connection tasks continue; (2)
  `wait_for_session_drain` polls `active_conns` until 0 OR `session_drain_secs`
  elapses; (3) on drain timeout it fires `sessions_shutdown` (every protocol
  task selects on this and runs `cancel_active_builds`), then waits a final
  `CANCEL_GRACE` (5 s) for the `CancelBuild` RPCs to land before process exit.
  I-081: without stage 3, process exit Drops the proto tasks mid-flight and the
  scheduler never hears `CancelBuild`. `terminationGracePeriodSeconds` in helm
  must be ≥ `drain_grace_secs + session_drain_secs + CANCEL_GRACE` + slack.
]

The deployed `session_drain_secs` (3600 s) is the operator #gls("sla"): a deploy may
interrupt builds running >1 h on the evicted gateway replica. @karpenter is held
off the control-plane NodePool entirely (`disruption.budgets: nodes=0,
reasons=[Drifted]` on `rio-general`), so AMI drift never auto-evicts gateway
pods; the operator runs `cargo xtask k8s rotate-general` during a quiet window
to roll those nodes onto a new AMI under the same 1 h drain budget.

= STDERR Message Types

#r("gw.stderr.message-types")[
  #table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Constant], [Value], [Direction], [Meaning]),
    [`STDERR_NEXT`],
    [`0x6f6c6d67`],
    [S → C],
    [Log/trace message (followed by:
      `string msg`)],

    [`STDERR_READ`],
    [`0x64617461`],
    [S → C],
    [Server needs data from client
      (followed by: `u64 count` bytes requested)],

    [`STDERR_WRITE`],
    [`0x64617416`],
    [S → C],
    [Server sending data to client
      (followed by: `string data`)],

    [`STDERR_LAST`],
    [`0x616c7473`],
    [S → C],
    [End of stderr stream; result
      follows],

    [`STDERR_ERROR`],
    [`0x63787470`],
    [S → C],
    [Error occurred (see format
      below)],

    [`STDERR_START_ACTIVITY`],
    [`0x53545254`],
    [S → C],
    [Start structured
      activity],

    [`STDERR_STOP_ACTIVITY`],
    [`0x53544f50`],
    [S → C],
    [End structured
      activity],

    [`STDERR_RESULT`],
    [`0x52534c54`],
    [S → C],
    [Structured result for an
      activity],
  )
]

== STDERR_ERROR Wire Format

#r("gw.stderr.error-format")[
  This is a complex nested structure. The gateway must construct it correctly
  for every error response (rejected opcodes, failed builds, etc.):
]

#r("gw.stderr.error-before-return+2")[
  *`STDERR_ERROR` and `STDERR_LAST` are mutually exclusive terminal frames. A
  handler sends exactly one of them, exactly once.*
  - If a handler returns `Err(...)`, it MUST send `STDERR_ERROR` first, and the
    session loop MUST NOT follow up with `STDERR_LAST`. Never use bare `?` to
    propagate errors from store operations, NAR extraction, or ATerm parsing
    --- always wrap in a match that sends `STDERR_ERROR` before returning.
  - If a handler sends `STDERR_ERROR`, it MUST `return Err(...)` immediately
    after. It MUST NOT call `stderr.finish()`, and it MUST NOT write a result
    payload. `STDERR_ERROR` is terminal for the operation --- the client stops
    reading STDERR frames and throws, so any bytes that follow are stranded in
    the TCP buffer and corrupt the next opcode on a pooled connection.
  - To report a *recoverable* per-operation failure while keeping the session
    open for subsequent opcodes, use `BuildResult::failure` (or the opcode's
    equivalent failure-carrying result type) delivered via `STDERR_LAST` +
    result. For batch opcodes like `wopBuildPathsWithResults`, per-entry errors
    push `BuildResult::failure` and `continue` --- they do not abort the batch.
]

The `StderrWriter` API enforces this: `error()` poisons the writer so that
subsequent `finish()` returns `Err` and `inner_mut()` panics.

#info(title: [Exception --- `wopQueryRealisation`])[
  The handler invokes the store first, then sends `STDERR_LAST`
  unconditionally, then matches on the store result. A store error (already
  past `STDERR_LAST`) is too late for `STDERR_ERROR`; instead the handler
  returns empty-set (`u64(0)`) and logs a warning. This is a degraded path (one
  missed CA cache hit), not a correctness violation; the next opcode on the
  session will hit the same store and fail through its own error path. (This
  could be restructured to match before `STDERR_LAST` --- the result is already
  buffered --- but the degraded-path cost is trivial and the structure is
  simpler.)
]

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Field], [Type], [Description]),
  [`type`], [string], [Error type (e.g. `"Error"`, `"nix::Interrupted"`)],
  [`level`], [u64], [Error level],
  [`name`], [string], [Program name (e.g. `"rio-build"`)],
  [`message`], [string], [Human-readable error message],
  [`havePos`], [u64 bool], [Whether position info follows],
  [(if havePos) `file`], [string], [Source file],
  [(if havePos) `line`], [u64], [Line number],
  [(if havePos) `column`], [u64], [Column number],
  [`traceCount`], [u64], [Number of trace entries],
  [(per trace) `havePos`], [u64 bool], [Whether trace position follows],
  [(per trace, if havePos) `file`, `line`, `column`],
  [string, u64, u64],
  [Trace position],

  [(per trace) `message`], [string], [Trace message],
)

== STDERR_START_ACTIVITY Wire Format

#r("gw.stderr.activity+2")[
  #table(
    columns: 3,
    align: (left, left, left),
    table.header([Field], [Type], [Description]),
    [`id`], [u64], [Activity ID (unique per session)],
    [`level`], [u64], [Verbosity level],
    [`type`], [u64], [Activity type (see enum below)],
    [`text`], [string], [Human-readable activity description],
    [`fieldsCount`], [u64], [Number of structured fields],
    [(per field)], [u64 type + value], [Typed field data],
    [`parentId`], [u64], [Parent activity ID (0 = no parent)],
  )
]

*Activity type enum* (matches upstream `nix::ActivityType`,
`libutil/logging.hh`):

#table(
  columns: 3,
  align: (left, left, left),
  table.header([Value], [Name], [Description]),
  [0], [Unknown], [Unknown/unclassified activity],
  [100], [CopyPath], [Copying a single store path],
  [101], [FileTransfer], [Downloading/uploading a file],
  [102], [Realise], [Realising a derivation output],
  [103], [CopyPaths], [Copying multiple store paths],
  [104], [Builds], [Top-level "building N derivations"],
  [105], [Build], [Building a single derivation],
  [106], [OptimiseStore], [Optimising the store (dedup)],
  [107], [VerifyPaths], [Verifying store paths],
  [108], [Substitute], [Substituting a path],
  [109], [QueryPathInfo], [Querying path info from a substituter],
  [110], [PostBuildHook], [Running post-build hook],
  [111], [BuildWaiting], [Build waiting for a lock],
  [112], [FetchTree], [Fetching a flake input tree],
)

Note: values 1--99 are unused. The enum starts at 0 (Unknown) then jumps to
100. For `Build` (105) the `fields` array is `[drvPath, machineName, curRound,
nrRounds]`; nom and `--log-format bar` read `fields[0]` as the derivation name
and `fields[1]` as the "on `<machine>`" suffix. For `Substitute` (108) the
`fields` array is `[storePath, substituterUri]`; the gateway emits one per
`DerivationEventKind::SUBSTITUTING` (#rref("sched.substitute.detached")) with
`fields[1]` empty (the store picks the upstream --- the scheduler doesn't see
which), and stops it on the paired `CACHED` (success) or `STARTED` (fetch
failed → fell through to a build).

#r("gw.activity.stop-parity")[
  Every `STDERR_START_ACTIVITY` the gateway emits to a client MUST be matched
  by a `STDERR_STOP_ACTIVITY` for the same id before the build's terminal
  `BuildResult` is written. The scheduler routes `Event::Log` on a separate
  broadcast ring from `DerivationEvent` so log volume cannot evict
  per-derivation `Completed` events; the gateway additionally drains any
  still-tracked activity ids at terminus to cover upstream loss.
]

#r("gw.activity.progress-before-stop")[
  For each per-derivation terminal transition (`COMPLETED`/`FAILED`), the root
  `actBuilds` `STDERR_RESULT{resProgress, [done, expected, running, failed]}`
  reflecting that derivation's increment MUST reach the client before the
  derivation's `STDERR_STOP_ACTIVITY`. nom marks an `actBuild` ✔ only when
  `done` increments while the activity is still open (matching native nix's
  `Goal::done()` ordering: parent counter update precedes `Activity`
  destructor). The scheduler emits `Event::Progress` before
  `DerivationEvent::Completed`/`Failed` on the state channel; the gateway
  relays in arrival order.
]

#r("gw.activity.subst-progress")[
  For each `DerivationEventKind::SUBSTITUTING` the gateway emits an
  `actSubstitute` (108) activity AND a child `actCopyPath` (100) activity
  (`fields=[storePath, "", machineName]`), and increments the root `actBuilds`
  `SetExpected{actCopyPath, N}` so nom shows an "X/Y copied" denominator.
  `Event::SubstituteProgress` (display-only, routed via the log broadcast ring;
  see #rref("store.substitute.progress-stream")) maps to
  `STDERR_RESULT{copy_aid, resProgress, [bytes_done, bytes_expected, 0, 0]}` so
  nom renders a per-derivation download bar. Both activities stop together
  (child first) on the paired `CACHED`/`STARTED`/`COMPLETED`/`FAILED`.
]

== STDERR_RESULT BuildEvent mapping

#r("gw.stderr.result.build-log-line")[
  Build log lines are emitted as `STDERR_RESULT` with `result_type=101`
  (`BuildLogLine`) and one string field, attached to the per-derivation
  `actBuild` activity ID. `STDERR_NEXT` is reserved for gateway-originated
  diagnostics (trace_id, reconnect notices) and for log lines that arrive
  before the derivation's `Started` event.
]

#r("gw.stderr.result.progress")[
  On `BuildStarted` the gateway emits a top-level `actBuilds` (104) activity
  and a `SetExpected` (106) result `[actBuild, total-cached]`. On each
  `BuildProgress` it emits a `Progress` (105) result `[completed, total,
  running, 0]` against that activity. The activity is stopped on the build's
  terminal event.
]

#r("gw.stderr.result.set-phase")[
  `BuildPhase` events are emitted as `STDERR_RESULT` with `result_type=104`
  (`SetPhase`) and one string field (the phase name), attached to the
  per-derivation `actBuild` activity.
]

= Protocol Compatibility

#r("gw.compat.version-range+2")[
  rio-build advertises protocol *1.38* (`0x126`) to support the
  feature-exchange step. Minimum accepted client version is *1.35* (`0x123`)
  --- the version Lix is policy-frozen at (CppNix 2.18 fork point). Older
  clients are rejected at handshake with a human-readable error. The only field
  rio gates between 1.35 and 1.38 is `BuildResult.cpu_user`/`cpu_system` (added
  in 1.37); the feature exchange itself is already gated on >= 1.38.
]

#r("gw.compat.unknown-opcode-close")[
  Unknown or unsupported opcodes return `STDERR_ERROR` and *close the
  connection*. This is necessary because the opcode's payload remains unread in
  the stream and its format is unknown, making it impossible to skip to the
  next opcode without corrupting the protocol. The Nix client will reconnect
  automatically.
]

#table(
  columns: 2,
  align: (left, left),
  table.header([Category], [Opcodes]),
  [Fully implemented],
  [`wopIsValidPath`, `wopQueryPathInfo`,
    `wopQueryValidPaths`, `wopAddToStore`, `wopAddTextToStore`,
    `wopAddToStoreNar`, `wopEnsurePath`, `wopNarFromPath`,
    `wopBuildDerivation`, `wopBuildPaths`, `wopBuildPathsWithResults`,
    `wopQueryMissing`, `wopAddTempRoot`, `wopSetOptions`,
    `wopAddMultipleToStore`, `wopQueryDerivationOutputMap`,
    `wopQueryPathFromHashPart`, `wopAddSignatures`, `wopRegisterDrvOutput`,
    `wopQueryRealisation`],

  [Rejected (STDERR_ERROR)], [Everything else],
)

*Note on CA opcodes:* `wopRegisterDrvOutput` / `wopQueryRealisation` parse the
Nix Realisation JSON
(`{"id":"sha256:<hex>!<name>","outPath":...,"signatures":[...],"dependentRealisations":{}}`)
and call the store's RegisterRealisation/QueryRealisation RPCs. Soft-fail on
malformed input (discard/empty-set rather than STDERR_ERROR) to avoid
regressing buggy clients that worked against the old stubs.
`dependentRealisations` is always `{}` from current Nix (ADR-018 --- field
removed upstream in schema v2); the gateway's `{}` stub is correct and should
not be changed. Full CA early cutoff (nar_hash comparison via content_index) is
Phase 5.

*Note on `wopQueryDerivationOutputMap`:* Moved from "CA-aware" to "Fully
implemented" because modern Nix clients call this for ALL derivation types. For
input-addressed derivations, it returns the statically-known output paths. For
CA derivations, it returns the realized output paths if known.

*Note on `wopAddTempRoot`:* Accepts the store path and records it as a
connection-scoped temporary GC root in-memory. These #glspl("temp-root") prevent GC of
paths the client is actively using. They are lost on gateway pod restart, which
is acceptable given the store's GC grace period (default #(refs.const)("DEFAULT_GC_GRACE_HOURS")h). The store's GC
relies on the grace period rather than querying gateways for active temp roots.

= Build Hook Protocol Path

#r("gw.hook.single-node-dag+2")[
  When a Nix client uses `--builders` (build hook mode) instead of `--store
  ssh-ng://` (remote store mode), the local daemon drives DAG traversal and
  delegates derivations one at a time. Because the gateway reports
  not-trusted (#rref("gw.handshake.untrusted")), `build-remote` (Nix ≥ 2.16,
  Lix) copies the `.drv` closure and drives `wopBuildPathsWithResults` for
  input-addressed derivations; the inline `wopBuildDerivation` single-node
  fallback remains available only for derivations whose statically-declared
  outputs are all content-bound (fixed-output / floating-CA) --- an inline
  input-addressed derivation whose full `.drv` cannot be resolved is
  rejected with remediation guidance.
]

*How it works:* The local `nix-daemon` drives DAG traversal and delegates
individual derivations to rio-build one at a time. Each hook invocation is
an independent SSH session.

*What the gateway does (two sub-flows, selected by the client from the
untrusted handshake):*
- *Input-addressed derivations (Nix ≥ 2.16 / Lix):* `build-remote` copies
  the realized inputs *and the `.drv` closure* (`wopAddToStoreNar` /
  `wopAddMultipleToStore`), then drives `wopBuildPathsWithResults`. The
  gateway runs the normal full-DAG reconstruction
  (#rref("gw.dag.reconstruct+4")) and every submission gate, including the
  output-path binding (#rref("gw.reject.output-path-mismatch+2")).
- *Content-bound derivations (any client):* the inline `wopBuildDerivation`
  single-node fallback still applies when the full `.drv` is unavailable ---
  the output paths are governed by the content-hash rules, not by trust in
  the declared paths. The serialized derivation is carried inline in the
  submission (#rref("gw.hook.inline-drv-content")) because the `.drv`
  exists in no store for the worker to fetch. DAG-reconstruction errors
  (transitive-input cap exceeded, child-`.drv` resolve failure mid-BFS) are
  surfaced to the client --- degrading to single-node would dispatch an
  input-addressed root with missing inputs.
- Submits to the scheduler via `SubmitBuild` as usual
- On a successful outcome with the resolved `.drv` available, verifies the
  declared outputs against the store
  (#rref("gw.opcode.build-results-honest")) before writing the `BuildResult`;
  a missing or unrealized output is reported as a failure result, not an
  empty-`outPath` success

#r("gw.hook.inline-drv-content+4")[
  When the gateway accepts a content-bound derivation through the inline
  `wopBuildDerivation` single-node fallback (the full `.drv` cannot be
  resolved from the session cache or the store), it MUST embed the
  serialized derivation in the submitted node's `drv_content` so the worker
  can execute it without the `.drv` existing in any store, and it MUST
  reject submissions whose serialized derivation exceeds the fallback
  inline cap (1 MiB, `MAX_FALLBACK_INLINE_DRV_BYTES`) with remediation
  guidance (upload the `.drv` first via `nix copy --derivation`, or use
  `--store ssh-ng://`). It MUST mark the node as carrying the authoritative
  copy (`drv_content_authoritative`) so the scheduler persists those bytes
  for recovery (#rref("sched.recovery.inline-drv-durability")). The inlined
  bytes are never written to the store or
  the session derivation cache: re-serialized content does not text-hash to
  the client's claimed `.drv` path, so persisting it would poison later
  full-DAG builds of the same derivation.
]

The fallback inline cap is the same constant the scheduler enforces per
node at `SubmitBuild` ingress (`MAX_DRV_CONTENT_BYTES` in
#src("rio-common/src/limits.rs")), so a fallback submission the gateway
accepts is never size-rejected downstream.

#r("gw.hook.fallback-built-outputs")[
  When a content-bound (fixed-output or floating-CA) inline fallback build
  succeeds, the gateway MUST return `builtOutputs` for the derivation's
  outputs keyed by the modular hash of the inline derivation --- CppNix
  `staticOutputHashes` over the received `BasicDerivation`, i.e.
  `hashDerivationModulo` with empty `inputDrvs` --- with floating-CA
  outputs carrying the realized path from the realisations table, and it
  MUST carry that hash on the submitted node (`ca_modular_hash`) so the
  scheduler registers the realisation at completion.
]
Build-remote registers exactly that realisation locally and the client's
resolved-derivation goal looks it up under the same key (the derivation
delegated to the hook for CA builds is already resolved, so the
inputDrvs-less hash is the canonical one). Without this the hook client
receives an empty `builtOutputs`, cannot locate the CA output path, and
fails after an otherwise successful build; repeat submissions of the same
resolved derivation now also benefit from merge-time realisation cache
hits.

Pre-2.16 hook clients that send inline input-addressed derivations without
uploading the `.drv` receive the rejection described in
#rref("gw.reject.output-path-mismatch+2"); the documented client floor for
hook-mode input-addressed builds is Nix 2.16 or Lix.

*Scheduling optimizations lost in build hook mode:*
- *No critical-path analysis* --- the scheduler sees each derivation in
  isolation, not as part of a graph
- *No multi-build DAG merging* --- shared derivations between concurrent builds
  cannot be deduplicated at the scheduling level
- *No CA early cutoff* --- without the full DAG, the scheduler cannot propagate
  cutoffs to downstream nodes

Interactive priority is *not* lost: the hook-shaped first
`wopBuildPathsWithResults` of a session is classified `"interactive"` exactly
like the inline `wopBuildDerivation` flow (#rref("gw.hook.ifd-detection+3")),
so steering ≥ 2.16 clients onto the `.drv`-upload flow does not silently
demote their hook builds to `"ci"`.

#r("gw.hook.ifd-detection+3")[
  *IFD / hook detection:* The gateway sets
  `SubmitBuildRequest.priority_class = "interactive"` for exactly two request
  shapes, and `"ci"` for everything else: (1) a `wopBuildDerivation` call that
  arrives without a preceding `wopBuildPathsWithResults` on the same session
  (an IFD or inline hook delegation); (2) the FIRST
  `wopBuildPathsWithResults` of a session whose only target is a single
  `DerivedPath::Built` with the all-outputs spec (`<drv>!*`) --- the shape
  stock `build-remote` (Nix ≥ 2.16, Lix) emits when delegating one
  derivation in build-hook mode against an untrusted remote. Named-output
  targets, multi-target batches, `wopBuildPaths` (opcode 9), and any
  subsequent `wopBuildPathsWithResults` on the session remain `"ci"`. There
  is no dedicated `is_ifd_hint` proto field --- the hint is encoded entirely
  in the `priority_class` string and the gateway, not the scheduler, makes
  the assignment.
]

#tip(title: [Recommendation])[
  Prefer `ssh-ng://` (remote store mode) over `--builders` (build hook mode)
  for better scheduling. The build hook path exists for compatibility with
  existing `nix.conf` setups, but delivers worse throughput and scheduling
  quality for large builds.
]

= Rate Limiting & Connection Management

#r("gw.rate.per-tenant")[
  Per-tenant build-submit rate limiting via `governor`
  `DefaultKeyedRateLimiter<String>` keyed on `tenant_name` (from
  authorized_keys comment --- operator-controlled, cannot be forged by client;
  empty/absent → key `"__anon__"`). *Disabled by default* --- no quota unless
  `gateway.toml [rate_limit]` section (or `RIO_RATE_LIMIT__PER_MINUTE` +
  `RIO_RATE_LIMIT__BURST` env vars) is present. When enabled: quota is
  operator-configured (`per_minute` = refill rate, `burst` = bucket capacity;
  both must be ≥1). Checked in all three build opcodes (`wopBuildDerivation`,
  `wopBuildPaths`, `wopBuildPathsWithResults`) immediately before
  `SubmitBuild`. On rate-limit violation: `STDERR_ERROR` with wait-hint
  (rounded up to nearest second) + tenant name, early return --- the SSH
  connection stays open for retry. Phase 5: key becomes `Claims.sub` (tenant
  UUID from JWT) instead of `tenant_name` (SSH comment) --- same bounded
  keyspace, JWT-native source. No eviction needed either way.
]

#r("gw.conn.cap")[
  Global connection cap via `Arc<Semaphore>` (default 1000, configurable via
  `gateway.toml max_connections` or `RIO_MAX_CONNECTIONS`).
  `try_acquire_owned()` in `new_client` (the russh accept callback); the permit
  is held by the `ConnectionHandler` and released in `Drop` so every disconnect
  path (EOF, error, abort) frees the slot. At cap: the handler's `conn_permit`
  is `None`, and the first `auth_*` callback returns `Err` to tear down the
  connection before any channel work. `log_session_end` logs the reject with
  `stage=auth-attempted`.
]

#r("gw.store.transient-retry")[
  Gateway→store RPCs that traverse #rref("store.substitute.admission")
  (`QueryPathInfo`, `GetPath`) MUST retry on transient gRPC status
  (`ResourceExhausted`, `Unavailable`, `Unknown`, `Aborted` per
  `rio_common::grpc::is_transient`). Retry budget is 2 attempts (one retry).
  Under sustained admission saturation each attempt blocks
  `SUBSTITUTE_ADMISSION_WAIT` (25 s) server-side, so worst-case latency before
  surfacing to the user is \~50 s --- bounded, but operators should treat
  sustained `RESOURCE_EXHAUSTED` here as a scaling signal. Non-transient status
  (`NotFound`, `Internal`, `DeadlineExceeded`) surfaces on the first attempt.
  The store maps the placeholder-race case (`SubstituteError::Raced` --- a
  concurrent replica is still fetching the NAR) to `NotFound`, NOT
  `Unavailable`: the 2-attempt budget can't outlast a multi-second NAR fetch,
  so the gateway treats it as `valid=false` (miss) and the caller re-probes
  later. The upstream-429 case (`SubstituteError::RateLimited{retry_after}`,
  including a bare 429 with no `Retry-After`) is `Unavailable` and retried
  here.
]

#r("gw.put.aborted-retry")[
  The buffered `grpc_put_path` helper (used by `wopAddToStore`,
  `wopAddTextToStore`, and the `.drv`-buffered branch of
  `wopAddToStoreNar`/`wopAddMultipleToStore`) MUST retry on store
  `Code::Aborted` up to `PUT_PATH_ABORTED_MAX_ATTEMPTS` (8) with full-jitter
  exponential backoff (50 ms base, ×2, 2 s cap → ≤ \~6 s total budget). The
  store returns `Aborted` when another upload holds the placeholder row for the
  same path (I-068) or on PG serialization conflicts. Each retry rebuilds the
  request stream from the `Arc<[u8]>`-held NAR without copying. Emits
  #(refs.metric)("rio_gateway_putpath_aborted_retries_total")`{attempt}` per retry. The streaming
  `grpc_put_path_streaming` helper is *not* retried on `Aborted` --- its reader
  is consumed and the bytes were forwarded as they arrived, so there is nothing
  to replay; in practice that path only fires for oversize non-`.drv` entries
  where the I-068 collision case does not apply.
]

= High Availability

#r("gw.sched.balanced")[
  The gateway connects to the scheduler in one of two modes selected by
  `scheduler.balance_host` in `gateway.toml`: *balanced* (K8s, multi-replica)
  --- DNS-resolve the headless Service, probe `grpc.health.v1/Check` on each
  pod IP, and route to the SERVING (= leader) endpoint via `BalancedChannel`;
  or *single* (VM tests, single-replica) --- plain connect to `scheduler.addr`.
  In balanced mode, `scheduler.addr` is still required: it is the ClusterIP
  Service, used as the TLS-verify domain (the cert's SAN). The
  `BalancedChannel` guard is held for process lifetime --- dropping it stops
  the probe loop. The store connection is single-channel only (no
  `balance_host`; store load is builder-driven, the gateway's `QueryPathInfo`
  traffic is light).
]

- Multiple gateway replicas sit behind a TCP load balancer (@nlb on EKS with
  idle timeout ≥ 3600s).
- Session state is connection-scoped --- the gateway is stateless beyond the
  lifetime of a single SSH connection.
- If a gateway pod dies, the affected SSH connections drop. Clients reconnect
  automatically (standard Nix retry behavior) and land on a healthy replica.
- Builds that were already in progress continue in the scheduler; only the
  log-streaming link is lost.

#r("gw.reconnect.backoff")[
  *WatchBuild reconnect:* When the `SubmitBuild` / `WatchBuild` response stream
  breaks (scheduler failover, transient network), the gateway's
  `process_stream` distinguishes error classes via `StreamProcessError`:
  - `Transport` (#(refs.error-doc)("StreamProcessError", "Transport")) and
    `EofWithoutTerminal`
    (#(refs.error-doc)("StreamProcessError", "EofWithoutTerminal")) → retried
    up to *#(refs.const)("MAX_RECONNECT") times* with exponential backoff
    *1 s/2 s/4 s/8 s/16 s, capped at 16 s for the remaining attempts*. The
    scheduler replays `BuildEvent`s from `build_event_log` starting at
    `since_sequence`.
  - `Wire` → *not* retried; the gateway returns `MiscFailure` to the Nix
    client immediately. #(refs.error-doc)("StreamProcessError", "Wire")
  The reconnect counter resets on the first successful `BuildEvent` received
  after a reconnect (NOT on `WatchBuild` returning `Ok` --- accepting the RPC
  doesn't prove the stream will yield events).
]

#r("gw.reconnect.since-seq")[
  The gateway MUST track the sequence number of the first peeked `BuildEvent`
  and use it as the initial `since_sequence` for reconnect, not hardcode `0`.
  The scheduler never emits `sequence=0` (it's the `WatchBuildRequest`-side
  "from start" sentinel); hardcoding `0` causes every first-event reconnect to
  replay one extra event.
]

- The gateway does not own durable state. All persistent data lives in the
  scheduler (PostgreSQL) and the store.
- Consider using a non-standard SSH port (e.g., 2222) to avoid conflicts with
  host SSH daemons and corporate firewalls blocking port 22 for non-standard
  destinations.
- The chart sets a `preStop.sleep` of `nlbDeregisterSecs` (NLB health-check
  round) before SIGTERM, then the three-stage drain runs with
  `sessionDrainSecs` (default 600s) so in-flight SSH sessions complete during
  rolling updates. `terminationGracePeriodSeconds` is computed from all three.

= Key Files

- #(refs.gh)("rio-gateway/src/server/") --- SSH server setup (russh),
  per-channel task spawning, `exec_request` matching
- #(refs.gh)("rio-gateway/src/session.rs") --- Per-SSH-channel protocol session
  loop (`run_protocol`), CancelBuild on disconnect
- #(refs.gh)("rio-gateway/src/handler/") --- Nix worker protocol opcode
  handlers:
  - `mod.rs` --- opcode dispatch (`handle_opcode`), `SessionContext`, `.drv`
    cache
  - `opcodes_read.rs` --- read-only opcodes (`wopIsValidPath`,
    `wopQueryPathInfo`, `wopNarFromPath`, ...)
  - `opcodes_write.rs` --- write opcodes (`wopAddToStore`, `wopAddToStoreNar`,
    `wopAddMultipleToStore`, ...)
  - `build.rs` --- build opcodes (`wopBuildDerivation`, `wopBuildPaths`,
    `wopBuildPathsWithResults`)
  - `grpc.rs` --- gRPC helpers for store put/get
- #(refs.gh)("rio-gateway/src/translate.rs") --- DAG reconstruction from `.drv`
  references, inline-`.drv` optimization, proto ↔ wire translation

= Failure modes

#table(
  columns: (auto, 1fr),
  align: (left, left),
  [*Immediate effect*],
  [Active SSH connections drop; clients see connection
    reset.],

  [*Cascading effect*],
  [Log streams for in-progress builds are lost; builds
    continue in the scheduler.],

  [*Recovery*],
  [Clients reconnect to surviving replicas via NLB. No data
    loss.],
)

A network partition between gateway and scheduler causes the gateway's
`SubmitBuild` calls to fail with `UNAVAILABLE`; the gateway returns
`STDERR_ERROR` to the Nix client, which retries (standard Nix behavior). Builds
already submitted continue in the scheduler; the gateway re-attaches via
`WatchBuild` after reconnection (#rref("gw.reconnect.backoff")).
