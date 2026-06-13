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

#r("bc.upload.origin-reread")[
  Source-tree upload MUST re-read the origin tree at upload time and verify
  that the recomputed NAR hash and root directory digest match the values
  eval reported; a mismatch is an error for that root, never a silent upload
  of divergent content.
]

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
