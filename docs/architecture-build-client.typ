#import "/lib/rio.typ": *

#show: rio.with(domains: none)

The native build client (`rio build`, ADR-024) is the second way into the
cluster, next to the ssh-ng gateway path described in
#cross-link("/architecture.typ")[System Architecture]. Where the gateway
translates Nix's legacy worker protocol opcode by opcode, `rio build` speaks
the cluster's own object model directly: everything that moves between client
and cluster is a content-addressed object keyed by #gls("blake3") of its
canonical bytes, and submission is digest negotiation --- ask which digests
the cluster already has, upload the misses, then submit a skeleton graph of
digests. Post-eval time to first build start drops from a measured 57~s
(cold ssh-ng) to roughly one second; a routine nixpkgs bump that changes no
derivations submits in ~150~ms. End-to-end, including the local evaluation
the protocol does not speed up, that is about #mul(5) cold and #mul(29) warm.

This chapter is the user-facing architecture view: the client process model,
the submission pipeline, and the object/digest spaces involved. The
#cross-link("/spec/components/build-client.typ")[build-client component spec]
carries the normative requirements; the
#cross-link("/guide/rio-build.typ")[rio build guide] covers installation,
configuration and troubleshooting; ADR-024 holds the measurements every
choice below is backed by.

= Client Process Architecture

`rio build` is three process layers in two binaries, with the boundaries
placed at the hard constraints: the async gRPC stack must never be in a
process that forks, and Nix's libexpr must be confined to one binary.

#figure(
  caption: [The `rio build` process tree. The coordinator (`rio`) owns the
    cluster connection, the client CAS handle and the global digest state,
    and never forks; it execs the eval parent (`rio-eval`) with one
    socketpair on fd~3. The eval parent embeds libexpr plus the rio eval
    store, does the pre-fork warmup once, and forks throwaway evaluation
    workers; worker results travel back up the same two queue edges as
    length-delimited proto frames.],
  diagram(
    spacing: (22mm, 11mm),
    node-stroke: 0.5pt,
    // ── client CAS (left column) ──
    node(
      (-0.05, 1.6),
      name: <cas>,
      shape: fletcher.shapes.cylinder,
      width: 13em,
      align(left)[
        *Client CAS* \
        #text(size: 0.75em)[
          append-only packs + index \
          dir blobs, chunk metadata \
          fingerprint index \
          cluster-ack table \
          fetched outputs
        ]
      ],
    ),
    // ── coordinator ──
    node(
      (1, 0),
      name: <coord>,
      width: 21em,
      fill: accent.lighten(88%),
      align(left)[
        *`rio` --- coordinator* \
        #text(size: 0.8em)[
          pure Rust, tokio/tonic --- never forks \
          attr work queue · global digest state · ack table \
          fold → negotiate → upload → submit → render
        ]
      ],
    ),
    // ── eval parent ──
    node(
      (1, 1.3),
      name: <parent>,
      width: 21em,
      fill: accent.lighten(88%),
      align(left)[
        *`rio-eval` --- eval parent* \
        #text(size: 0.8em)[
          embeds libexpr + rio eval store (C++ shim, Rust core) \
          locks the flake, fetches inputs, forces outputs --- once \
          single-threaded at every `fork(2)`
        ]
      ],
    ),
    // ── workers ──
    node(
      (0.55, 2.5),
      name: <w0>,
      shape: rect,
      align(left)[
        *worker-0* \
        #text(size: 0.75em)[eval one attr at a time \ `GC_DONT_GC`]
      ],
    ),
    node((1, 2.5), name: <wdots>, stroke: none, text(fill: muted)[⋯]),
    node(
      (1.45, 2.5),
      name: <wn>,
      shape: rect,
      align(left)[
        *worker-N* \
        #text(size: 0.75em)[recycled after attr quota \ or RSS threshold]
      ],
    ),
    node(
      enclose: (<w0>, <wdots>, <wn>),
      name: <workers>,
      stroke: (paint: muted, dash: "dashed"),
      inset: 7pt,
    ),
    // ── cluster (right column) ──
    node(
      (2.35, 0),
      name: <sched>,
      shape: rect,
      width: 12em,
      align(left)[
        *rio-scheduler* \
        #text(size: 0.75em)[`SubmitBuild` \ `WatchBuild` / `CancelBuild`]
      ],
    ),
    node(
      (2.35, 1),
      name: <store>,
      shape: rect,
      width: 12em,
      align(left)[
        *rio-store* (castore door) \
        #text(
          size: 0.75em,
        )[`Has*` presence \ `PutDrvBlobs`, `PutPathChunked` \ `GetPath` (fetch)]
      ],
    ),
    node(
      (2.35, -0.55),
      name: <cluster-hdr>,
      stroke: none,
      inset: 0pt,
      text(size: 0.8em, fill: muted)[Cluster (tenant-authenticated gRPC)],
    ),
    node(
      enclose: (<cluster-hdr>, <sched>, <store>),
      name: <cluster>,
      stroke: (paint: muted, dash: "dashed"),
      inset: 8pt,
    ),
    // ── edges ──
    edge(
      <coord>,
      <parent>,
      "<|-|>",
      align(
        center,
      )[exec + socketpair (fd 3) \ length-delimited proto frames \ WorkItem ↓ · ResultFrame ↑],
      label-size: 0.75em,
    ),
    edge(
      <parent>,
      <workers>,
      "<|-|>",
      align(center)[fork, no exec (COW) \ one socketpair per worker],
      label-size: 0.75em,
    ),
    edge(
      <coord>,
      <sched>,
      "-|>",
      [gRPC + tenant JWT],
      label-size: 0.75em,
    ),
    edge(
      <coord>,
      <store>,
      "-|>",
      [gRPC + tenant JWT],
      label-size: 0.75em,
      label-pos: 0.35,
    ),
    edge(
      <parent>,
      <cas>,
      "<|-|>",
      [flake inputs, fetched IFD outputs],
      label-size: 0.7em,
      label-side: left,
    ),
    edge(
      <workers>,
      <cas>,
      "<|-|>",
      align(center)[source ingest: dir blobs + \ fingerprints (no file bytes)],
      label-size: 0.7em,
      label-side: right,
      label-pos: 0.4,
    ),
    edge(
      <coord>,
      <cas>,
      "<|-|>",
      [ack table, `--fetch` outputs],
      label-size: 0.7em,
      label-pos: 0.3,
    ),
  ),
)

The *coordinator* is the only process that talks to the cluster. It owns the
attribute work queue, the global digest state and the persistent cluster-ack
table, and it never calls `fork(2)` --- the exec boundary keeps tokio, tonic
and TLS out of every process that does fork. The *eval parent* is the only
binary that links libexpr. Before forking anything it does the expensive
work once: open the rio eval store, lock the flake, fetch its inputs through
the eval store, and force the flake's outputs attrset.
*Workers* are forked without exec --- inheriting that locked, parsed state
copy-on-write --- evaluate one
attribute at a time with the Boehm GC disabled, and are recycled --- shut
down between attributes and replaced by a fresh fork --- once they hit an
attribute quota or an RSS threshold; process exit is the evaluator's garbage
collection. A crashed worker costs at most one attribute's work: its
in-flight attribute is re-queued to a fresh fork, and everything it had
already reported is safe in the coordinator.

That is the complete cross-process inventory: two queue edges (coordinator to
parent, parent to worker), an advisory claim table that stops two live
workers ingesting the same large tree twice, and the disk CAS's file locks.
There is no upload spool, no daemon, and no sibling-worker IPC.

= Submission Pipeline

The five pipeline stages --- fold, negotiate, upload, submit, render --- run
overlapped with evaluation; nothing waits for "eval finished". A root is
submitted as soon as *its* transitive skeleton is complete and every object
it references has been uploaded or was already present (the all-acked gate);
other attributes may still be evaluating at that point.

#let pipeline-fig(factor, body) = if is-html-target() {
  scale(factor, reflow: true, origin: top + left, body)
} else {
  block(
    width: 100%,
    align(left, scale(factor, reflow: true, origin: top + left, body)),
  )
}

#figure(
  caption: [The submission pipeline for one build root. Skeleton nodes and
    canonical derivation bytes stream up from the workers; the coordinator
    folds by digest, negotiates presence per object kind, uploads only the
    misses, and submits a digest-only skeleton once the root's
    all-acked gate opens. The stale-ack branch runs at most once.],
  pipeline-fig(60%, chronos.diagram({
    import chronos: *
    _par("Worker", display-name: [Eval worker])
    _par("Coord", display-name: [Coordinator])
    _par("Store", display-name: [rio-store])
    _par("Sched", display-name: [rio-scheduler])

    _seq(
      "Worker",
      "Worker",
      comment: [evaluate attr; ingest sources via the rio eval store \ (one walk → chunks + dir blobs + NAR hash)],
    )
    _seq(
      "Worker",
      "Coord",
      comment: [`ResultFrame` --- skeleton nodes (digests), \ canonical drv bytes, source-root digests],
    )
    _seq("Coord", "Coord", comment: [fold by `drv_digest`; consult ack table])
    _seq(
      "Coord",
      "Store",
      comment: [`HasDrvs` / `HasDirectories` / `HasChunks` (digest lists)],
    )
    _seq("Store", "Coord", comment: [presence bitmaps], dashed: true)
    _seq(
      "Coord",
      "Store",
      comment: [`PutDrvBlobs` --- missing drv blobs, largest first],
    )
    _seq(
      "Coord",
      "Store",
      comment: [`PutPathChunked` --- missing dir blobs + chunks \ (origin re-read and re-verified at upload time)],
    )
    _seq("Store", "Coord", comment: [acks → ack table], dashed: true)
    _seq(
      "Coord",
      "Coord",
      comment: [all-acked gate opens for the root],
    )
    _seq(
      "Coord",
      "Sched",
      comment: [`SubmitBuild` --- digest-only skeleton, \ \~334 B/node, paginated above `page_max_nodes`],
    )
    _alt(
      [accepted],
      {
        _seq("Sched", "Sched", comment: [bulk-verify digests against the store])
        _seq(
          "Sched",
          "Coord",
          comment: [`BuildEvent` stream: queued / building / built / completed],
          dashed: true,
        )
        _seq(
          "Coord",
          "Store",
          comment: [`GetPath` (`--fetch`, narHash-verified)],
        )
      },
      [stale acks (`FAILED_PRECONDITION`, missing digests named)],
      {
        _seq(
          "Coord",
          "Coord",
          comment: [evict named acks; re-`Has`; re-upload from retained bodies],
        )
        _seq(
          "Coord",
          "Sched",
          comment: [resubmit --- once; a second reject is a hard error],
        )
      },
    )
  })),
)

What travels on each edge is deliberately narrow. Worker frames carry
derivation *digests* plus the canonical derivation *bytes* keyed by digest
--- derivations never touch the client disk; they live in memory until the
submission that references them is accepted. Negotiation sends digest lists
and receives bitmaps, one bulk round-trip per object kind rather than a
per-level Merkle walk (the real derivation DAG is 233 levels deep; a level
walk would cost ~10~s of round-trips at 40~ms RTT). Uploads carry only the
misses, largest first so downstream builds unblock early; a
disconnect costs nothing because a re-`Has` after reconnect only shrinks the
miss set. The store door speaks plain gRPC --- compression is the store's
at-rest job --- while the scheduler channel is zstd-compressed both ways.
The submission itself is a skeleton --- digests and edges, no
derivation content --- so a warm cluster receives kilobytes per rebuild.
Multi-attribute invocations do not multiply any of this: nodes already part
of an accepted submission in the same session are excluded and resolve
against the store's drv blobs.

Interrupting the client (Ctrl-C) cancels the builds this invocation
submitted by default; `--detach` instead leaves them running cluster-side.
Either way the cluster never depends on a connected client: `rio build
--attach` resumes the event stream via `WatchBuild` from any machine holding
the tenant credential, a coordinator crash leaks nothing cluster-side for the
same reason, and a re-run is cheap by construction --- warm CAS, warm
cluster, near-zero misses.

= Objects and Digest Spaces

One digest space covers everything the protocol moves: file-content chunks
(#gls("fastcdc"), 16/64/256~KiB), source-tree directories (per-directory
castore proto blobs), and derivations (canonical rio-proto bytes). The digest
is always #gls("blake3") over the canonical encoding, and the client-side
format shares the cluster's by construction --- same bytes, same
canonical-encode rule --- so upload is pure negotiation with no conversion
layer and no double hashing.

#figure(
  caption: [Object kinds and where their bytes live. Client and cluster share
    one blake3 digest space per kind; presence (`Has*`) is tenant-scoped on
    the cluster side. Derivations are memory-only on the client; local
    working trees are never copied into the client CAS --- the origin tree is
    the byte store and is re-read at upload time.],
  diagram(
    spacing: (34mm, 9mm),
    node-stroke: 0.5pt,
    node(
      (0, -0.7),
      name: <c-hdr>,
      stroke: none,
      inset: 0pt,
      text(size: 0.8em, fill: muted)[Client (`rio build`)],
    ),
    node(
      (1, -0.7),
      name: <s-hdr>,
      stroke: none,
      inset: 0pt,
      text(size: 0.8em, fill: muted)[Cluster (rio-store)],
    ),
    // ── chunks row ──
    node(
      (0, 0),
      name: <c-chunks>,
      width: 17em,
      align(left)[
        *file chunks* \
        #text(size: 0.75em)[
          origin tree is the byte store --- re-read, \
          re-chunked and verified at upload; \
          only fetched content persists in the CAS
        ]
      ],
    ),
    node(
      (1, 0),
      name: <s-chunks>,
      width: 15em,
      fill: accent.lighten(90%),
      align(left)[
        *chunks* \
        #text(
          size: 0.75em,
        )[S3, zstd at rest \ digest-keyed, deduplicated globally]
      ],
    ),
    // ── directories row ──
    node(
      (0, 1.5),
      name: <c-dirs>,
      width: 17em,
      align(left)[
        *directories* \
        #text(size: 0.75em)[
          per-directory castore blobs in append-only \
          packs + decoded-dir cache; fingerprint \
          index skips unchanged trees
        ]
      ],
    ),
    node(
      (1, 1.5),
      name: <s-dirs>,
      width: 15em,
      fill: accent.lighten(90%),
      align(left)[
        *directories* \
        #text(size: 0.75em)[castore Directory blobs \ (PostgreSQL, refcounted)]
      ],
    ),
    // ── derivations row ──
    node(
      (0, 3),
      name: <c-drvs>,
      width: 17em,
      align(left)[
        *derivations* \
        #text(size: 0.75em)[
          memory-only: canonical proto bytes in the \
          eval/coordinator process, dropped once the \
          referencing submission is accepted
        ]
      ],
    ),
    node(
      (1, 3),
      name: <s-drvs>,
      width: 15em,
      fill: accent.lighten(90%),
      align(left)[
        *drv blobs* \
        #text(
          size: 0.75em,
        )[canonical proto bytes, \ build-pinned GC, re-verified on upload]
      ],
    ),
    // ── enclosures ──
    node(
      enclose: (<c-hdr>, <c-chunks>, <c-dirs>, <c-drvs>),
      name: <client>,
      stroke: (paint: muted, dash: "dashed"),
      inset: 8pt,
    ),
    node(
      enclose: (<s-hdr>, <s-chunks>, <s-dirs>, <s-drvs>),
      name: <cluster>,
      stroke: (paint: muted, dash: "dashed"),
      inset: 8pt,
    ),
    // ── per-kind negotiation edges ──
    edge(
      <c-chunks>,
      <s-chunks>,
      "-|>",
      [`HasChunks` → upload misses],
      label-size: 0.75em,
    ),
    edge(
      <c-dirs>,
      <s-dirs>,
      "-|>",
      [`HasDirectories` → `PutPathChunked`],
      label-size: 0.75em,
    ),
    edge(
      <c-drvs>,
      <s-drvs>,
      "-|>",
      [`HasDrvs` → `PutDrvBlobs`],
      label-size: 0.75em,
    ),
  ),
)

The split of what persists where follows ADR-024's "fetch cache plus an
index, not a mirror" rule. The client CAS stores only what has no other
local home: fetched content (flake inputs, fetched IFD outputs, `--fetch`
results), the directory metadata packs, and the two persistent indexes ---
the stat-fingerprint index that lets a second invocation skip re-hashing
unchanged trees, and the cluster-ack table that lets it skip re-negotiating
digests the cluster was recently confirmed to hold (scoped to cluster
endpoints and tenant, expiring within the cluster's unpinned-blob lifetime).
Local working trees are never copied in: upload re-reads the origin and
verifies the recomputed hashes still match what evaluation reported, so a
mutated tree is a named error rather than a silent wrong-content build.
Derivations are not stored at all --- they are a few kilobytes each,
deterministically recomputed by every evaluation, and needed only until the
cluster acknowledges them.

The per-directory granularity is also what fixed the first iteration's
client-side performance: a warm flake evaluation through the original
monolithic source-DAG blob ran #mul(92) slower than stock Nix because every
`lstat` re-parsed an 8.9~MB blob; per-directory blobs behind a decoded-dir
cache bring the same trace to a few milliseconds, and they are what makes
upload dedup work --- on a real nixpkgs day-bump, monolithic encodings
re-upload 100% of their chunks while the per-directory layout re-uploads
about 1.4~MB.

On the cluster side these are the same objects the rest of the system
already stores and serves: chunks and directories land in the existing
castore surface (#cross-link("/spec/components/store.typ")[rio-store]), and
derivation blobs are a castore blob kind with build-pinned GC, so the
scheduler's submit-time verification, the builders' input fetches and the
client's `--fetch` reads all resolve against one store. Presence answers are
tenant-scoped for all three kinds: negotiation never reveals whether another
tenant's bytes exist, while storage itself stays digest-keyed and
deduplicated.
