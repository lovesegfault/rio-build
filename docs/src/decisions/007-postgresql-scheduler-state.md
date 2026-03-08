# ADR-007: PostgreSQL for Scheduler State

## Status
Accepted

## Context
The scheduler must persist build DAGs, job assignments, build history, and dashboard data. This state must survive scheduler restarts, support complex queries for analytics and the web UI, and handle concurrent access from multiple scheduler replicas.

## Decision
Build DAGs, job assignments, build history, and dashboard data are stored in PostgreSQL. The scheduler state uses separate schemas from the store metadata but shares the same PostgreSQL cluster to reduce operational overhead.

PostgreSQL provides:
- ACID transactions for consistent DAG state updates.
- Rich query capability for the web dashboard (build history, analytics, worker utilization).
- Advisory locks for distributed scheduler coordination.
- LISTEN/NOTIFY for event-driven scheduling without polling.

## Alternatives Considered
- **Redis or etcd for scheduler state**: Fast for simple key-value lookups but lacks the relational query capability needed for DAG operations and dashboard analytics. DAG traversal queries (find all ready-to-build derivations, compute critical path) are natural in SQL but awkward in key-value stores.
- **SQLite embedded in the scheduler**: No network overhead, simple deployment. But does not support concurrent access from multiple scheduler replicas and cannot handle the query load from the dashboard.
- **Separate database cluster for scheduler vs. store**: Maximum isolation but doubles operational overhead (backups, monitoring, upgrades). Separate schemas in the same cluster provide sufficient isolation with lower cost.
- **In-memory state with WAL for persistence**: Fast but limits state size to available memory and requires custom recovery logic. PostgreSQL's battle-tested WAL and replication are more reliable.

## Consequences
- **Positive**: PostgreSQL is a well-understood, battle-tested database. Operational tooling (backups, monitoring, replication) is mature.
- **Positive**: Complex dashboard queries (build time percentiles, cache hit rates, worker utilization) are natural SQL.
- **Positive**: Shared cluster with separate schemas balances isolation and operational simplicity.
- **Negative**: PostgreSQL is a stateful dependency that must be provisioned and managed.
- **Negative**: Schema migrations must be coordinated across scheduler and store schemas during upgrades.

## Implementation Note (Phase 3a)

The decision to use PostgreSQL for scheduler state stands, but two mechanisms listed in the "PostgreSQL provides" section above were never built:

- **Advisory locks:** Leader election uses a Kubernetes Lease instead (see `rio-scheduler/src/lease.rs`). The original rationale was that coupling leader election to PG availability ensures the leader always has access to its state backend; in practice a K8s Lease is operationally simpler and the generation-fence mechanism (workers reject stale-generation assignments) makes brief dual-leader windows harmless.
- **LISTEN/NOTIFY:** Scheduling is tick-based (10s dispatch interval). No event-driven pubsub was implemented. The actor model's internal message channel handles intra-process events; PG is polled, not subscribed.
- **Separate schemas:** All migrations share the default `public` schema. The `migrations/` directory contains both scheduler (`001_scheduler.sql`) and store (`002_store.sql`) tables with no `CREATE SCHEMA` separation.

The decision rationale (PG for durability + rich queries over a KV store) is unchanged.
