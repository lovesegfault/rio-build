// r[impl dash.journey.build-to-logs]
// Singleton AdminService client. Import { admin } wherever a page needs
// an RPC — no per-page createClient boilerplate, and the transport is
// shared (connection pooling, interceptors land in one place).
//
// All three legs of the killer journey (Builds list → Graph → LogViewer)
// call through this one client: ListBuilds → GetBuildGraph → GetBuildLogs.
// The journey pages carry documentary tracey markers in .svelte (which
// tracey doesn't parse); this .ts entry point is the scannable impl anchor.
//
// AdminService comes from ../gen/admin_pb (NOT *_connect.ts — protobuf-es v2
// unified the service descriptor into the single *_pb output; see
// buf.gen.yaml for the one-plugin rationale).
import { createClient } from '@connectrpc/connect';
import { AdminService } from '../gen/admin_pb';
import { transport } from './transport';

export const admin = createClient(AdminService, transport);
