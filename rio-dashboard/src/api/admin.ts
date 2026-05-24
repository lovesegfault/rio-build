// r[impl dash.journey.build-to-logs]
// r[impl dash.clear-poison]
// r[impl dash.executors.kind-filter]
// Singleton AdminService client. Import { admin } wherever a page needs
// an RPC — no per-page createClient boilerplate, and the transport is
// shared (connection pooling, interceptors land in one place).
//
// The first two legs of the killer journey (Builds list → Graph) call
// through this client: ListBuilds → GetBuildGraph. The third leg
// (LogViewer) calls rio-store's LogService/TailLog via api/logs.ts —
// build logs live in the store, not on the scheduler. ClearPoisonButton
// and the Executors-page kind filter likewise call ClearPoison/
// ListExecutors through this client. The journey pages carry
// documentary tracey markers in .svelte (which tracey doesn't parse);
// this .ts entry point is the scannable impl anchor.
//
// AdminService comes from ../gen/admin_pb (NOT *_connect.ts — protobuf-es v2
// unified the service descriptor into the single *_pb output; see
// buf.gen.yaml for the one-plugin rationale).
import { createClient } from '@connectrpc/connect';
import { AdminService } from '../gen/admin_pb';
import { transport } from './transport';

export const admin = createClient(AdminService, transport);
