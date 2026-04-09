// r[impl dash.envoy.grpc-web-translate+3]
// r[impl dash.auth.method-gate+2]
// r[impl dash.stream.idle-timeout]
// Shared gRPC-Web transport for the AdminService client.
//
// The translation, method-gate enforcement, and 1h stream-idle-timeout are
// IN Envoy Gateway (Helm CRDs, infra/helm/rio-build/templates/
// dashboard-gateway*.yaml — tracey doesn't scan YAML so the impl markers
// live here at the client entry point). This transport is the browser-side
// contract: gRPC-Web binary
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
import { Code, ConnectError, type Interceptor } from '@connectrpc/connect';
import { createGrpcWebTransport } from '@connectrpc/connect-web';

// Retry Unavailable once with a short backoff. Belt-and-suspenders for
// the standby-replica race: with scheduler.replicas=2 a request can
// land on the non-leader, which returns Unavailable. The structural fix
// (health on the main port → standby out of Endpoints) lands when TLS
// is removed; until then this client-side retry replaces the Envoy
// BackendTrafficPolicy retry-on-unavailable. Unary only — server
// streams establish on the leader and stay there.
const retryUnavailableOnce: Interceptor = (next) => async (req) => {
  try {
    return await next(req);
  } catch (e) {
    if (req.stream || ConnectError.from(e).code !== Code.Unavailable) {
      throw e;
    }
    await new Promise((r) => setTimeout(r, 200));
    return await next(req);
  }
};

export const transport = createGrpcWebTransport({
  baseUrl: '/',
  // Binary framing (application/grpc-web+proto) matches what the Envoy
  // grpc_web filter expects without a JSON transcoder in the chain.
  useBinaryFormat: true,
  interceptors: [retryUnavailableOnce],
});
