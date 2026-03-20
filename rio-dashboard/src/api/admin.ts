// Singleton AdminService client. Import { admin } wherever a page needs
// an RPC — no per-page createClient boilerplate, and the transport is
// shared (connection pooling, interceptors land in one place).
//
// AdminService comes from ../gen/admin_pb (NOT *_connect.ts — protobuf-es v2
// unified the service descriptor into the single *_pb output; see
// buf.gen.yaml for the one-plugin rationale).
import { createClient } from '@connectrpc/connect';
import { AdminService } from '../gen/admin_pb';
import { transport } from './transport';

export const admin = createClient(AdminService, transport);
