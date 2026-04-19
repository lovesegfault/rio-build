// r[impl dash.envoy.grpc-web-translate+3]
// r[impl dash.auth.method-gate+2]
// r[impl dash.stream.idle-timeout]
// Shared gRPC-Web transport for the AdminService client.
//
// gRPC-Web translation happens in-process at rio-scheduler via tonic-web
// (GrpcWebLayer + accept_http1, rio-scheduler/src/main.rs — D3). The Cilium
// Gateway is a plain HTTPRoute that path-matches /<svc>/<method> for the
// per-method allowlist (infra/helm/rio-build/templates/dashboard-gateway.yaml
// — readonly methods always; mutating methods only when
// dashboard.enableMutatingMethods=true). Long-lived streams are kept alive by
// the scheduler's 30s http2 keepalive PING, not a proxy idle-timeout.
//
// createGrpcWebTransport, NOT createConnectTransport: tonic-web speaks the
// gRPC-Web protocol (application/grpc-web+proto), not the Connect protocol.
// The VM dashboard test (nix/tests/dashboard.nix) validates against that
// content-type with curl — a Connect transport would produce the wrong
// framing and 415 at GrpcWebLayer.
//
// baseUrl '/' — one build artifact serves both environments:
//   * prod: nginx proxies /rio.admin.AdminService/* → the Cilium Gateway
//     listener. See infra/helm/rio-build/templates/dashboard-*.yaml.
//   * dev: vite.config.ts server.proxy forwards the same prefix to a
//     localhost port-forward.
// No VITE_* env var plumbing, no per-env rebuild.
import { Code, ConnectError, type Interceptor } from '@connectrpc/connect';
import { createGrpcWebTransport } from '@connectrpc/connect-web';

// Retry Unavailable once with a short backoff. Belt-and-suspenders for
// the standby-replica race: with scheduler.replicas=2 a request can land
// on the non-leader, which returns Unavailable as Trailers-Only (status
// in HTTP headers, no body). The HTTPRoute backendRef is the ClusterIP
// rio-scheduler Service whose tcpSocket readiness marks BOTH replicas
// Ready, and there is no proxy-level retry or active health-check (no
// BackendTrafficPolicy — D3 dropped vendor CRDs). This applies to unary
// AND server-stream RPCs alike: a stream's opening POST is load-balanced
// exactly like a unary call, and Trailers-Only Unavailable surfaces as a
// throw from `next(req)` before the first message yields, so the catch
// below covers it.
export const retryUnavailableOnce: Interceptor = (next) => async (req) => {
  try {
    return await next(req);
  } catch (e) {
    if (ConnectError.from(e).code !== Code.Unavailable) {
      throw e;
    }
    await new Promise((r) => setTimeout(r, 200));
    return await next(req);
  }
};

export const transport = createGrpcWebTransport({
  baseUrl: '/',
  // Binary framing (application/grpc-web+proto) matches what tonic-web's
  // GrpcWebLayer expects without a JSON transcoder in the chain.
  useBinaryFormat: true,
  interceptors: [retryUnavailableOnce],
});
