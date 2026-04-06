// Shared gRPC-Web transport for all AdminService/SchedulerService clients.
//
// createGrpcWebTransport, NOT createConnectTransport: Envoy's grpc_web
// filter speaks the gRPC-Web protocol (application/grpc-web+proto), not the
// Connect protocol. The VM dashboard test (nix/tests/dashboard.nix) validates
// against that content-type with curl — a Connect transport would produce the
// wrong framing and 415 at the Envoy filter chain.
//
// baseUrl '/' — one build artifact serves both environments:
//   * prod: nginx proxies /rio.admin.AdminService/* → the Envoy Gateway
//     listener. See infra/helm/rio-build/templates/dashboard-*.yaml.
//   * dev: vite.config.ts server.proxy forwards the same prefix to
//     localhost:8080 (port-forwarded Envoy).
// No VITE_* env var plumbing, no per-env rebuild.
import { createGrpcWebTransport } from '@connectrpc/connect-web';

export const transport = createGrpcWebTransport({
  baseUrl: '/',
  // Binary framing (application/grpc-web+proto) matches what the Envoy
  // grpc_web filter expects without a JSON transcoder in the chain.
  useBinaryFormat: true,
});
