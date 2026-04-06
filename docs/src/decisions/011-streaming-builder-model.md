# ADR-011: Streaming Builder Model

## Status
Accepted

## Context
Workers execute builds on behalf of the scheduler. The communication model between scheduler and workers determines latency, failure handling, and operational characteristics. Workers may run multiple concurrent builds, and the scheduler must be able to send control signals (cancel, prefetch hints) to active builds.

## Decision
Workers connect to the scheduler via a bidirectional `BuildExecution` streaming RPC. A single long-lived stream per worker carries:

**Scheduler to worker:**
- Build assignments (derivation + input closure metadata).
- Prefetch hints for cache warming (ADR-009).
- Cancel signals for individual builds.

**Worker to scheduler:**
- Build log batches (streamed incrementally).
- Build completion reports (success/failure, output paths, timing).
- Acknowledgments.

> **Implementation note:** Heartbeats are a **separate unary RPC** (`Worker.Heartbeat`, see `rio-proto/proto/worker.proto`), not carried on the `BuildExecution` stream. The heartbeat loop runs independently of stream lifecycle so liveness reporting survives stream reconnection.

The stream provides natural backpressure: if a worker is overwhelmed, gRPC flow control slows the scheduler's assignment rate. Connection drops are detected via gRPC keepalives, enabling fast failure detection and rescheduling.

## Alternatives Considered
- **Unary RPC polling (worker pulls jobs)**: Workers poll the scheduler for new jobs. Simple but adds polling latency, generates unnecessary traffic when idle, and requires separate mechanisms for cancel signals and log streaming.
- **Message queue (NATS, RabbitMQ, Kafka)**: Decouple scheduler and workers via a message broker. Adds infrastructure complexity and another failure domain. Build log streaming over a message queue is awkward, and ordered delivery of cancel signals is harder to guarantee.
- **Server-sent events + REST**: Workers receive assignments via SSE and report results via REST. No bidirectional streaming, so log streaming and cancel signals require separate channels. HTTP/1.1 SSE has reconnection overhead.
- **gRPC unary RPCs with server streaming for logs**: Separate RPCs for assignment, completion, and log streaming. More granular but requires correlating multiple streams per build and managing their lifecycles independently.

## Consequences
- **Positive**: Single stream per worker simplifies connection management and multiplexes all communication.
- **Positive**: Natural backpressure prevents worker overload without explicit rate limiting.
- **Positive**: Incremental log streaming gives dashboard users real-time build output.
- **Negative**: Long-lived streams are sensitive to network instability. Requires robust reconnection logic with state reconciliation.
- **Negative**: Debugging stream-level issues is harder than debugging individual request/response RPCs.
