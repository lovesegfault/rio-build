# Data Flows

## Remote Store: `nix build --store ssh-ng://rio .#package`

```
1. User runs: nix build --store ssh-ng://rio .#package
2. Nix evaluates the flake locally -> produces derivation DAG (.drv files)
3. Nix opens SSH connection to rio-gateway
4. Worker protocol handshake (magic bytes, version negotiation)
5. Nix sends wopSetOptions (build configuration)
6. Nix sends wopQueryValidPaths --- "which outputs do you have?"
7. rio-gateway queries rio-store -> returns valid paths
8. Nix sends wopAddToStoreNar for each missing .drv file and input source
   -> rio-gateway stores in rio-store
   (for protocol >= 1.32, sources are batched via wopAddMultipleToStore
    rather than individual wopAddToStoreNar calls)
8a. Nix sends wopQueryDerivationOutputMap for each derivation
    -> Modern Nix clients (>= 2.4) call this unconditionally for all
       derivation types (input-addressed and CA). rio-gateway resolves
       via rio-store and returns the output name -> store path mapping.
9. Nix sends wopBuildDerivation (or wopBuildPathsWithResults) for top-level
   -> wopBuildDerivation sends an inline BasicDerivation (WITHOUT inputDrvs)
   -> rio-gateway reconstructs the full DAG by parsing the .drv files
      uploaded in step 8 (each .drv contains inputDrvs references forming
      the DAG edges)
   -> forwards to rio-scheduler via gRPC SubmitBuild
10. rio-scheduler:
    a. Queries rio-store for cache hits (already-built outputs)
    b. Computes remaining build graph
    c. Computes critical-path priorities
    d. Dispatches ready derivations to workers
11. For each dispatched derivation:
    a. Scheduler sends PrefetchHint (anticipated input paths) on the
       BuildExecution stream so the worker can pre-warm its FUSE cache
    b. Worker's FUSE daemon checks local SSD cache for input paths (fast path)
    c. Cache miss: FUSE daemon fetches from rio-store via gRPC, caches on SSD
    d. Worker executes build in nix sandbox (overlay-merged /nix/store)
    e. Worker streams build logs to scheduler via bidirectional
       BuildExecution stream (log lines batched for efficiency)
       Scheduler relays logs to gateway via BuildEvent stream (from SubmitBuild)
       Gateway converts to STDERR_NEXT messages for the Nix client
    f. Worker streams output NAR via PutPath; rio-store chunks via
       FastCDC on the server side (workers never chunk locally)
    g. Worker reports completion to scheduler
    h. Scheduler stores completion, releases downstream nodes
12. When top-level derivation completes:
    a. Scheduler notifies gateway
    b. Gateway sends STDERR_LAST + BuildResult to Nix client
    c. Client requests wopNarFromPath for outputs
       -> Gateway sends STDERR_LAST first, then writes the NAR as raw
          bytes directly (no STDERR framing). The Nix client's
          processStderr(ex) has no sink argument for this opcode, so
          STDERR_WRITE would fail with 'error: no sink'. See
          rio-gateway/src/handler/opcodes_read.rs.
    d. Gateway streams NAR (reassembled from chunks) back to client
```

> **Status:** CA cutoff-compare is implemented (completion-time output-hash check against the content index); propagate (Skipped status + DAG cascade) is scheduled at [P0252](../.claude/work/plan-0252-ca-cutoff-propagate-skipped.md). Until propagate lands, the compare result is recorded but downstream derivations are still released unconditionally.

See [rio-gateway](./components/gateway.md) for protocol opcode details, [rio-scheduler](./components/scheduler.md) for the scheduling algorithm, and [rio-store](./components/store.md) for the chunked CAS.

```mermaid
sequenceDiagram
    participant Client as Nix Client
    participant GW as rio-gateway
    participant Sched as rio-scheduler
    participant Worker as rio-worker
    participant Store as rio-store

    Client->>GW: SSH connect + handshake
    Client->>GW: wopSetOptions
    Client->>GW: wopQueryValidPaths
    GW->>Store: FindMissingPaths
    Store-->>GW: missing paths
    GW-->>Client: valid paths (inverted)
    Client->>GW: wopAddToStoreNar (.drv files)
    GW->>Store: PutPath
    Client->>GW: wopQueryDerivationOutputMap
    GW->>Store: GetPath (.drv NAR)
    GW->>GW: parse ATerm -> output map
    GW-->>Client: derivation output map
    Client->>GW: wopBuildDerivation
    GW->>Sched: SubmitBuild (DAG)
    Sched->>Store: FindMissingPaths (cache check)
    Store-->>Sched: missing paths
    Sched->>Worker: WorkAssignment (via BuildExecution)
    Worker->>Store: GetPath (FUSE fetch)
    Worker->>Worker: nix sandbox build
    Worker->>Sched: BuildLogBatch
    Sched->>GW: BuildEvent (logs)
    GW->>Client: STDERR_NEXT
    Worker->>Store: PutPath (output)
    Worker->>Sched: CompletionReport
    Sched->>GW: BuildEvent (completed)
    GW->>Client: STDERR_LAST + BuildResult
    Client->>GW: wopNarFromPath
    GW->>Store: GetPath
    Store-->>GW: NAR stream
    GW-->>Client: NAR data
```

## Remote Builder: `nix build --builders 'ssh-ng://rio ...'`

```
1. User runs: nix build .#package (with rio configured as a builder)
2. Nix evaluates locally, starts building the DAG
3. For each derivation, local nix-daemon invokes the build hook
4. Build hook connects to rio-gateway via ssh-ng
5. Build hook sends the .drv path, system, and features
6. rio-gateway receives single-derivation build request
   -> creates a mini build plan in rio-scheduler
7. rio-scheduler assigns to a worker (same algorithm but single-derivation)
8. Worker builds, uploads output to rio-store
9. rio-gateway returns output to build hook
10. Build hook copies output back to local store
11. Local daemon continues with next derivation
```

> **Key difference:** in build hook mode, the local daemon drives the DAG traversal. rio only sees one derivation at a time. Less optimal scheduling, but fully compatible with any existing Nix setup.

> **Note on --builders mode:** In `--builders` mode, the local nix-daemon (not the build hook program directly) connects to rio-gateway via ssh-ng. What rio-gateway sees is a normal ssh-ng session with a specific operation pattern. The build hook is a local daemon concept; rio-gateway doesn't distinguish build hook vs direct client connections.

```mermaid
sequenceDiagram
    participant Daemon as Local nix-daemon
    participant Hook as Build Hook
    participant GW as rio-gateway
    participant Sched as rio-scheduler
    participant Worker as rio-worker

    Daemon->>Hook: delegate derivation
    Hook->>GW: SSH connect
    Hook->>GW: wopBuildDerivation (single)
    GW->>Sched: SubmitBuild (single node)
    Sched->>Worker: WorkAssignment
    Worker->>Worker: build
    Worker->>Sched: CompletionReport
    Sched->>GW: BuildEvent (completed)
    GW->>Hook: BuildResult
    Hook->>Daemon: output path
    Daemon->>Daemon: continue DAG
```

## Client Disconnection

```
1. Client SSH connection drops (network failure, ctrl-c, etc.)
2. rio-gateway detects SSH channel close
3. Gateway sends CancelBuild to scheduler with reason="client_disconnect"
4. Scheduler policy:
   a. For derivations shared with other active builds: continue building
      (the DAG merge logic keeps shared derivation nodes live as long as
      at least one interested build remains)
   b. For derivations unique to this build: removed from the queue
      immediately. If already Running, the worker is allowed to complete
      (wasted work is bounded by one derivation per worker)
5. Completed outputs remain in rio-store regardless of client state
6. If the client reconnects and re-submits, the scheduler's DAG merge
   re-inserts the derivations. Any outputs already stored in step 5 are
   cache hits (instant completion via FindMissingPaths)
```

> **Not implemented (by design):** No orphan timeout window or explicit "reattach" mechanism. Reconnection safety comes from (a) shared-derivation DAG merge and (b) cache hits on already-stored outputs. A timed orphan grace period is not planned.

```mermaid
sequenceDiagram
    participant Client as Nix Client
    participant GW as rio-gateway
    participant Sched as rio-scheduler
    participant Worker as rio-worker

    Client-xGW: SSH connection drops
    GW->>Sched: CancelBuild (client_disconnect)
    alt Shared derivation
        Sched->>Sched: continue (other builds need it)
    else Unique derivation
        Sched->>Sched: remove from queue immediately
    end
    Worker->>Sched: CompletionReport (if already Running)
    Note over Sched: Outputs kept in store regardless
    Client->>GW: Reconnect + re-submit
    Sched->>Sched: DAG merge + cache hits on stored outputs
```

## Scheduler Failover

```
1. Scheduler leader pod dies (crash, node failure, rolling update)
2. New scheduler pod acquires the Kubernetes Lease for leader election
3. New leader reconstructs in-memory state from PostgreSQL
   (see scheduler.md State Recovery). Dispatch is gated on
   recovery_complete.
4. Workers detect stream break, reconnect BuildExecution streams to new leader
5. For gateway connections with active SubmitBuild streams:
   a. The SubmitBuild response stream (BuildEvent) breaks with a gRPC
      Transport error
   b. Gateway's process_stream classifies the error as
      StreamProcessError::Transport and re-subscribes via
      WatchBuild(build_id, since_sequence) — up to 5 times with
      exponential backoff (1/2/4/8/16s)
   c. New scheduler replays BuildEvents from build_event_log starting
      at since_sequence. Nix client sees continuous STDERR streaming
      (possibly a brief pause during backoff)
   d. If all 5 reconnects fail OR the error is EofWithoutTerminal/Wire
      → gateway returns MiscFailure to the client (manual retry)
   e. If the gateway itself also restarted, see Client Disconnection above
6. Log events between the old leader's last S3 flush and its crash may
   be lost (bounded by the 30s periodic flush; see observability.md)
```

## Import-From-Derivation (IFD)

IFD occurs when Nix evaluation depends on a build result. The flow is:

```
1. Client begins evaluation, discovers it needs to build a derivation
   before it can continue evaluation
2. Client opens a separate SSH channel (the primary channel is blocked
   in evaluation) and sends wopBuildDerivation for the IFD derivation
3. rio-gateway receives a single-derivation build request on the new channel
   -> rio-gateway forwards to rio-scheduler as a SubmitBuild with
      priority_class = "interactive" (IFD builds are evaluation-blocking)
4. rio-scheduler detects IFD priority: the scheduler assigns maximum
   priority to this derivation (above all queued non-IFD work)
5. Worker builds the derivation, uploads output
6. rio-gateway returns BuildResult to the client on the IFD channel
7. Client retrieves the output via wopNarFromPath on the IFD channel
8. Client resumes evaluation using the IFD output
9. Client may submit the full DAG (including the IFD derivation) on the
   primary channel --- the IFD derivation is already cached (instant hit)
```

> **Detection heuristic:** IFD builds arrive as individual `wopBuildDerivation` calls, typically before the full DAG is submitted via `wopBuildPathsWithResults`. The gateway annotates the `SubmitBuildRequest` with `priority_class = "interactive"` when the session has not yet seen a `wopBuildPathsWithResults` call (see `rio-gateway/src/handler/build.rs`). There is no dedicated `is_ifd_hint` field; priority classification is conveyed entirely through `priority_class`.
