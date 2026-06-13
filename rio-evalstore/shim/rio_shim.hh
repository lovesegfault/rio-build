/* Embedder surface of the shim (rio-eval only — the plugin never
 * includes this). shim.cc and the includer compile into ONE binary;
 * the plugin .so exports none of it. */

#pragma once

#include "rio_evalstore.h"

namespace nix {
class Store;

/* The Rust core handle behind an opened rio:// store, or nullptr if
 * `store` is not a RioStore. */
RioEvalStore * rioShimStoreHandle(Store & store);
} // namespace nix

extern "C" {
/* IFD relay hook: relays one import-from-derivation and blocks until
 * the outputs are imported into the store. nonzero = failure; *err is
 * a rio_string_free()-able message. */
typedef int (*rio_shim_ifd_fn)(void * ctx, const char * drv_path, char ** err);

/* Register the worker-side IFD handler (nullptr-able). Without a
 * handler, buildPaths on the rio store throws Unsupported. */
void rio_shim_set_ifd_handler(rio_shim_ifd_fn fn, void * ctx);
}
