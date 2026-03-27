// Barrel re-export of all protobuf-es generated types the dashboard
// consumes. Mirrors api/admin.ts (which exports the AdminService
// CLIENT; this exports the TYPES). Consumers import from '../api/types'
// instead of reaching into '../gen/*_pb' at file-depth-specific paths.
//
// RATIONALE: P0376 split types.proto into 4 domain protos; each split
// is a sed across N `../gen/*_pb` consumers. Centralizing here means
// the next split touches ONE file. Gen paths are buf.gen.yaml output —
// brittle to codegen config changes, stable when fronted by a barrel.
//
// Enumeration (not `export *`): documents what's ACTUALLY CONSUMED by
// the dashboard. Extend when a new consumer appears. `export type` is
// correct — no consumer calls create(XSchema, ...); these are used as
// TypeScript types only.

// admin_types.proto — dashboard-specific response shapes
export type {
  BuildInfo,
  ClusterStatusResponse,
  ExecutorInfo,
} from '../gen/admin_types_pb';

// types.proto (legacy monolith residue — GCProgress lives here until a
// further split moves it to admin_types or a gc_types domain)
export type { GCProgress } from '../gen/types_pb';

// admin.proto service descriptor — test-support/admin-mock.test.ts
// introspects AdminService.method for surface-parity assertions. This
// is a runtime value (GenService<...>), not a type — plain `export`.
export { AdminService } from '../gen/admin_pb';
