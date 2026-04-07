// r[impl dash.envoy.grpc-web-translate+2]
// r[impl dash.auth.method-gate+2]
// Shared gRPC-Web transport for the AdminService client.
//
// The translation and method-gate enforcement are IN Envoy Gateway (Helm
// CRDs, infra/helm/rio-build/templates/dashboard-gateway*.yaml — tracey
// doesn't scan YAML so the impl markers live here at the client entry
// point). This transport is the browser-side contract: gRPC-Web binary
// framing over HTTP/1.1 POST, which Envoy's auto-injected grpc_web filter
// translates to HTTP/2 gRPC+mTLS. Every dashboard RPC flows through the
// method-gated GRPCRoute (readonly methods always; mutating methods only
// when dashboard.enableMutatingMethods=true).
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
