// r[impl dash.journey.build-to-logs+2]
// Singleton LogService client — the build-log read path.
//
// Build logs live in rio-store (immutable zstd chunks + a PG manifest),
// not on the scheduler: the LogViewer's stream comes from
// `rio.store.LogService/TailLog` on rio-store:9002, while every other
// dashboard RPC stays on the scheduler's AdminService. Same shared
// gRPC-Web transport (baseUrl '/'): nginx and the Cilium Gateway route
// by exact /<service>/<method> path, so the per-method allow-list picks
// the backend — /rio.store.LogService/TailLog → rio-store,
// /rio.admin.AdminService/* → rio-scheduler. The store serves gRPC-Web
// natively (tonic-web layer, same as the scheduler).
//
// LogService comes from ../gen/store_pb (store.proto defines the
// service and its messages; protobuf-es v2 emits both from the one
// *_pb output).
import { createClient } from '@connectrpc/connect';
import { LogService } from '../gen/store_pb';
import { transport } from './transport';

export const logs = createClient(LogService, transport);
