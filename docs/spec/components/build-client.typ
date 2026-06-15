#import "/lib/rio.typ": *
#show: rio.with(domains: ("bc",))


`rio build` --- the native-protocol build client (ADR-024 P3). The binary is
the *coordinator*: pure Rust, tokio/tonic, gRPC to the cluster. Evaluation
runs in a separate eval-parent process (`rio-eval`: libexpr + fork workers,
P3b) connected over one `socketpair(AF_UNIX, SOCK_STREAM)`; the coordinator
owns the attr work queue, the global digest state, the client CAS handle,
and the cluster connection, and it never forks.

= Worker channel

The coordinator and the eval parent exchange `rio.evaljob` proto messages as
length-delimited frames: a 4-byte big-endian length prefix, then the encoded
message. The channel is a local pipe, not an RPC surface --- no tonic, no
TLS, no compression. The payloads reuse `rio.types.DerivationNode` and
`rio.types.DrvBlob` so worker-reported skeletons reach `SubmitBuild` and
`PutDrvBlobs` without re-encoding.

#r("bc.ipc.frame-cap")[
  Frame readers MUST reject a length prefix above the 64~MiB cap and MUST
  distinguish clean EOF at a frame boundary (peer closed) from truncation
  mid-frame (an error).
]

A `ResultFrame` batch whose `root_drv_digest` is non-empty closes its attr:
the attr's transitive skeleton is complete once every digest reachable from
that root has been folded.

= Installables

An installable that names a single derivation becomes one build root. An
installable that names an attribute set (`.#checks`,
`.#checks.x86_64-linux`) is expanded instead of rejected.

#r("bc.eval.attrset-expansion")[
  An installable whose attr resolves to an attribute set rather than a
  derivation MUST be expanded: the eval worker reports the full attr paths
  of the set's derivation children (descending first into the entry named
  after the eval system when present, and into nested sets only when they
  carry `recurseForDerivations = true`), the coordinator queues each child
  as its own build root named by that attr path, a child that is neither a
  derivation nor a recursable attribute set is skipped with a warning
  naming it, and an installable that expands to zero derivations fails
  evaluation for that attr.
]

The expansion rides an `AttrsetExpansion` worker frame and is the attr's
final answer: the children come back to the eval parent as ordinary
`WorkItem`s, so they spread across the fork-worker pool exactly like
explicitly listed attrs --- per-child crash-requeue, recycling and IFD all
apply unchanged. A child that was also requested explicitly (or by another
expansion) is queued once. A child whose name cannot be written as a
re-resolvable attr path (it contains `"`, is all digits, or is empty) is
skipped the same way as a non-derivation entry.

= Pipeline

The five ADR-024 stages run overlapped --- nothing waits for "eval finished":
fold incoming nodes by digest, negotiate presence, upload misses, submit per
root, render event streams.

#r("bc.fold.dedup-by-digest")[
  The coordinator MUST fold worker-reported skeleton nodes into the global
  graph keyed by `drv_digest`, dropping duplicates (multi-attr overlap ships
  once).
]

#r("bc.negotiate.ack-short-circuit")[
  Presence negotiation MUST consult the persistent cluster-ack table before
  issuing `Has*` RPCs; an unexpired ack for a digest suppresses the probe.
  Ack records MUST be scoped to the cluster endpoints and tenant identity
  and MUST carry an expiry no later than the cluster's minimum
  unpinned-blob lifetime.
]

#r("bc.upload.origin-reread+2")[
  Upload of a source root that carries a filesystem origin MUST re-read that
  origin at upload time and verify that the recomputed NAR hash and root
  identity (root directory digest, or the file digest and executable bit, or
  the symlink target) match what eval reported; a mismatch is an error for
  that root, never a silent upload of divergent content.
]

#r("bc.upload.source-root-kinds")[
  The upload planner MUST handle directory, single-file and symlink source
  roots: file and symlink roots ship their inline castore root node via
  `PutPathChunked` with no Directory DAG, and only directory roots are
  presence-probed via `HasDirectories`.
]

File and symlink roots are negotiated through the persistent ack table
alone: they are KB-sized, the put is idempotent, and there is no path-level
`Has` RPC to probe them with.

#r("bc.upload.cas-read")[
  A source root reported with an empty origin MUST be served from the client
  CAS: chunk bodies come from its digest-verified content records and
  Directory bodies from its directory blobs, with no origin re-read.
]

Empty-origin roots are the streamed ingests --- files and trees fetched as
flake inputs, `builtins.toFile` text --- for which no origin tree exists on
disk. The eval worker flushes its pack segment before emitting a frame that
carries one, so the coordinator's own CAS handle sees the records. Known
caveat: a `toFile` path with references registers cluster-side with an
empty reference set (the `SourceRoot` frame carries none) --- a follow-up
threads references through.

#r("bc.upload.stale-ack-once")[
  On an `UNAVAILABLE` reject from `PutPathChunked` the client MUST evict the
  chunk acks the reject names (or, when it names none, every chunk ack
  involved in that upload), re-probe presence via `HasChunks`, re-upload
  what the cluster reports missing, and retry the upload exactly once; a
  second reject is a hard error.
]

This is the chunk sibling of the drv-digest recovery below: the ack TTL is
allowed to exceed the cluster's orphan-chunk GC grace, and a `chunks` row
can outlive its S3 object, so a presence answer the client cached can stop
being true. The store rejects the upload `UNAVAILABLE` naming the digest it
could not fetch back (and demotes the lying presence row); without the
eviction-and-retry cycle the client keeps trusting its ack and fails the
same upload until the ack expires.

Drv-blob misses upload largest-first (big drvs gate the most downstream
bytes); restartability is free because a re-`Has` after any disconnect only
shrinks the miss set.

#r("bc.submit.all-acked")[
  A root MUST be submitted only once its transitive skeleton is complete and
  every referenced object (drv blobs, source roots) is uploaded-or-present
  and acked --- the committed all-acked gate.
]

#r("bc.submit.exclude-submitted")[
  Submissions MUST exclude nodes already part of an accepted-or-claimed
  submission in the same session; excluded nodes are referenced by digest
  and resolve against the store's drv blobs.
]

#r("bc.submit.paginate")[
  Submissions above the configured page limit MUST paginate: pages share a
  client-chosen `submission_id`, non-final pages are acked by an
  immediately-closed empty event stream, and the final page carries the
  build options.
]

#r("bc.submit.stale-ack-once")[
  On a `FAILED_PRECONDITION` reject naming missing drv digests the client
  MUST evict those acks, re-probe presence, re-upload what the cluster still
  misses from retained bodies, and resubmit exactly once; a second reject is
  a hard error.
]

Drv bodies are retained until the root's submission is *accepted* (not
merely until upload-ack, as the ADR's coordinator sketch suggests): stale-ack
recovery must re-upload from memory, and drvs are memory-only client-side ---
a body dropped at ack time would force a full re-eval to recover.

= Rendering

#r("bc.render.stdout-results")[
  Stdout MUST carry only the final result lines (`attr: built /nix/store/...`,
  `fetched to ...`, `cancelled <id>`); every status, log and diagnostic line
  goes to stderr.
]

This is the machine-readable surface --- a piped invocation gets clean
result paths and nothing else.

#r("bc.render.select")[
  The `--render` mode `auto` MUST pick `tty` when stderr and stdin are
  both ttys and `TERM` is set and not `dumb`; otherwise `ci` when
  `GITHUB_ACTIONS=true`; otherwise `plain`.
]

#r("bc.render.plain-default")[
  The `plain` renderer MUST keep the one-line-per-state-edge format
  (`[<id8>] <state> <drv>`) on stderr.
]

The format is a compatibility surface for scripts and the VM test.

#r("bc.render.sanitize")[
  Every emitted build-derived string (log lines, phase, error message,
  diagnostic notes) MUST be sanitized (SGR-only ANSI, no other control
  characters, length-capped) and MUST be prefixed so no line can start
  with `::`.
]

The prefix neutralises Actions `::endgroup::` / `::add-mask::`
injection from build output; the sanitizer guarantees no CR/LF survives
to fabricate a fresh line start.

#r("bc.render.failure-log-tail")[
  When a build fails with culprit attribution (`BuildFailed.culprit_*` ---
  a fail-fast on a derivation that already failed in an earlier build), the
  client MUST fetch the culprit execution's stored log via
  `GetDerivationLog` and re-print it on stderr --- the last `--log-lines`
  lines (default 20), or the full log under `-L`/`--print-build-logs` ---
  emitted one renderer note per line so the per-line sanitization
  (#rref("bc.render.sanitize")) applies; when no log content is available
  the client MUST print the persisted `culprit_error_message` instead.
]

A fail-fast build runs no execution of its own, so without the replay the
user sees only "derivation X failed" with nothing to debug. The tail is
requested server-side (`tail_lines`), so `-L` is the only mode that
transfers the whole log.

#r("bc.render.tty-restore")[
  Terminal attributes (termios, cursor visibility) MUST be restored on
  every exit path, including panic and SIGINT.
]

= Attach, detach, results

#r("bc.interrupt.cancel-default")[
  On SIGINT/SIGTERM the client MUST cancel every build submitted by this
  invocation (the same `CancelBuild` RPC behind `--cancel`), print each
  cancelled build id, tear down the eval parent as on a completed run, and
  exit non-zero; a second interrupt while cancellation is in flight MUST
  stop waiting for the cancel acknowledgements and print the remaining
  build ids with their `--attach` hints instead.
]

#r("bc.interrupt.detach-flag")[
  Under `--detach` an interrupt MUST NOT cancel anything: the client exits,
  the builds keep running cluster-side, and each in-flight build id is
  printed with its `--attach` reattach hint.
]

#r("bc.interrupt.scope")[
  Interrupt cancellation MUST be scoped to builds submitted by the current
  invocation; a build watched via `--attach` is never cancelled by an
  interrupt --- interrupting an attach only stops watching.
]

Either way the cluster needs no client to make progress: `rio build
--attach <id>` resumes the event stream via `WatchBuild` from any machine
with the tenant credential; `--cancel <id>` is the explicit cancel from
anywhere else.

#r("bc.fetch.narhash-verify")[
  `--fetch` MUST verify the streamed NAR's SHA-256 against the server's
  claimed `nar_hash` before materializing into the client CAS; a mismatch
  refuses materialization.
]

= The eval parent (`rio-eval`, P3b)

`rio-eval` embeds the flake-pinned nix libexpr plus the rio-evalstore Rust
staticlib behind the same C++ store shim as the `rio://` plugin. The parent
does the pre-fork half once --- open the rio store, build the EvalState,
lock the flake and fetch inputs through the rio store --- then forks
eval workers on demand (fork-no-exec, `GC_DONT_GC`; process exit is the
evaluator GC). Workers evaluate assigned attrs, assemble their skeleton
subgraphs from the in-memory drv map (canonical proto bytes + blake3
digests, computed once at `writeDerivation` capture), and stream
`ResultFrame`s on their socketpair; the parent relays raw frames to the
coordinator (two queue edges --- the ADR's complete inventory).

#r("bc.evalparent.fork-safety")[
  Workers MUST be forked from a single-threaded parent: no live threads may
  exist at any `fork(2)`. Ingest-pipeline threads are scoped inside a single
  ingest call and joined before it returns; the parent loop itself spawns
  none.
]

#r("bc.evalparent.recycle")[
  A worker MUST be recycled (shut down and replaced by a fresh fork) after
  the configured attr quota or when its RSS exceeds the configured
  threshold, checked between attrs --- never mid-eval. Results MUST be
  identical across recycle generations.
]

The recycle decision is the parent's (it sends the worker a Shutdown frame
between attrs); a worker is deliberately too dumb to exit on its own, so
assignment can never race a voluntary exit.

#r("bc.evalparent.crash-requeue")[
  The parent MUST survive any worker death: a reaped worker's in-flight
  attr is re-queued to a fresh fork (bounded retries), the crash is
  surfaced upstream as a non-fatal `WorkerError`, and only an attr whose
  retries are exhausted is reported lost --- named --- while other attrs
  proceed.
]

A crash report whose `attr` is empty is visibility-only: the coordinator
logs it without failing anything (the attr was re-queued, not lost).

#r("bc.evalparent.ifd-relay")[
  A worker hitting import-from-derivation MUST first stream the needed
  drv's transitive skeleton as an intermediate batch, then relay an
  `IfdRequest` upstream and BLOCK on its socketpair until the matching
  `IfdCompletion`; a completion error fails the import with the
  coordinator's message, and a successful completion's outputs are imported
  into the worker's eval store before eval resumes.
]

#r("bc.evalparent.claim-advisory")[
  The cross-worker claim table is advisory only --- correctness MUST NOT
  depend on it. A claim is stale (ignorable, take-over-able) once its pid
  is dead or it is older than 60 seconds.
]

The claim table is a fork-shared `memfd` keyed by `blake3(origin path)`; it
exists to stop two live workers ingesting the same big tree simultaneously.
The CAS's single-writer segments and idempotent index already make
concurrent ingest safe, so a lost or stale claim costs duplicate work,
never wrong content.
