// r[impl dash.journey.build-to-logs+2]
// r[impl dash.log.attempt-scope]
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
//
// SELECTOR LAW (bug_090, signed Q4): the TailLog selector alphabet at
// this boundary — the ONE seam every dashboard tail crosses — is
// CLOSED: `pinned` (execId != '') | `derivation` (drvPath != '').
// Those are exactly the store's two resolvers (tail.rs: the
// exec-pinned lookup and latest_build_exec by drv_log_hash). The
// empty form has NO resolver — drv_log_hash('') = '' matches no
// execution — so it dialed a guaranteed NotFound that the stream's
// first-open classification treats as retryable: an endless re-dial
// on running builds, an error banner after grace on terminal ones.
// Whole-build aggregation is an explicit non-goal (no server
// aggregation contract). The empty selector is therefore REFUSED
// here, before the transport, with the store's own PERMANENT type:
// logStream's exit law already classifies the unservable metadata as
// terminal (no re-dial), making this the backstop for any future
// unfocused mount.
import { Code, ConnectError, createClient } from '@connectrpc/connect';
import { LogService, type TailLogChunk } from '../gen/store_pb';
import { transport } from './transport';

// Mirrors rio_common::grpc::LOG_UNSERVABLE_METADATA_KEY (grpc.rs) —
// the store's permanent-unservable failure type. The TS literal is
// documented against the Rust symbol across a language boundary the
// type system cannot pin (logStream.svelte.ts's reader carries the
// sibling literal); a cross-language golden pin is the flagged
// follow-on (derivation_statuses.json is the repo precedent).
const LOG_UNSERVABLE_METADATA_KEY = 'x-rio-log-unservable';

const client = createClient(LogService, transport);

/** An async iterable whose first `next()` rejects with the typed
 * refusal and which NEVER touches the transport. */
function refuseEmptySelector(): AsyncIterable<TailLogChunk> {
  return {
    [Symbol.asyncIterator]() {
      return {
        next(): Promise<IteratorResult<TailLogChunk>> {
          return Promise.reject(
            new ConnectError(
              'TailLog: empty selector is unservable by construction ' +
                '(no derivation, no pinned exec) - whole-build ' +
                'aggregation has no resolver',
              Code.FailedPrecondition,
              new Headers({ [LOG_UNSERVABLE_METADATA_KEY]: 'empty_selector' }),
            ),
          );
        },
      };
    },
  };
}

/** The LogService client with the selector law enforced at the
 * boundary: structurally-unservable TailLog requests are refused
 * client-side; servable selectors pass through untouched. */
export const logs: typeof client = {
  ...client,
  tailLog(req, opts) {
    if ((req.derivation ?? '') === '' && (req.execId ?? '') === '') {
      return refuseEmptySelector();
    }
    return client.tailLog(req, opts);
  },
};
