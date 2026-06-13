/* extern "C" surface of the rio-evalstore Rust staticlib.
 *
 * Hand-written mirror of rio-evalstore/src/ffi.rs — keep the two in sync
 * by hand; the surface is ~15 functions and deliberately boring (bytes,
 * paths, JSON strings, streaming callbacks).
 *
 * Conventions:
 *  - return code: 0 ok, 1 error, 2 unsupported/foreign-path.
 *  - on nonzero return *err is a Rust-allocated NUL-terminated message;
 *    free with rio_string_free() (never free()).
 *  - out-strings are allocated by Rust, freed with rio_string_free().
 *    A null out-string with rc 0 means "not found".
 *  - callbacks must never throw across the boundary: the C++ wrappers
 *    catch everything and return nonzero, then rethrow after the Rust
 *    call returns.
 */

#ifndef RIO_EVALSTORE_H
#define RIO_EVALSTORE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RioEvalStore RioEvalStore;

enum {
    RIO_OK = 0,
    RIO_ERR = 1,
    RIO_UNSUPPORTED = 2,
};

/* Node kinds for rio_lstat / rio_read_directory entries. 0 doubles as
 * "no such path" in RioStat so a zeroed struct reads as missing. */
enum {
    RIO_NODE_MISSING = 0,
    RIO_NODE_REGULAR = 1,
    RIO_NODE_SYMLINK = 2,
    RIO_NODE_DIRECTORY = 3,
};

/* lstat result. Hot-path op (hundreds of thousands of calls per warm
 * eval): a plain out-struct, no allocation, no JSON. The symlink target
 * is NOT part of lstat — readLink is its own op. */
typedef struct RioStat {
    uint8_t kind; /* RIO_NODE_* */
    uint8_t executable;
    uint64_t size;
} RioStat;

/* dump methods */
enum {
    RIO_DUMP_FLAT = 0,
    RIO_DUMP_NAR = 1,
};

/* content-address methods */
enum {
    RIO_CA_FLAT = 0,
    RIO_CA_NAR = 1,
    RIO_CA_TEXT = 2,
};

/* Pull bytes from the caller's stream. Return 0 with *n_read set (0 =
 * EOF), nonzero on failure. */
typedef int (*rio_read_cb)(void * ctx, unsigned char * buf, size_t cap, size_t * n_read);

/* Push bytes into the caller's sink. Return 0 on success. */
typedef int (*rio_write_cb)(void * ctx, const unsigned char * data, size_t len);

/* Cross-check callback: receives ingest hashes as JSON
 * ({"nar_sha256":hex,"nar_size":n,"content_sha256":hex}) and must write
 * nix's computed store path, NUL-terminated, into out_path. Return 0 on
 * success. */
typedef int (*rio_path_cb)(void * ctx, const char * hashes_json, char * out_path, size_t out_cap);

int rio_store_open(const char * cas_dir /* nullable */, RioEvalStore ** out_store, char ** err);
void rio_store_free(RioEvalStore * store);
void rio_string_free(char * s);

/* Duplicate a NUL-terminated string into a rio_string_free()-able
 * allocation (embedder helper for hook error out-parameters — malloc
 * is not interchangeable with the Rust-side allocator). Null in, null
 * out. */
char * rio_string_dup(const char * s);

/* Free a byte buffer returned by rio_read_directory. len must be the
 * value the call returned. */
void rio_bytes_free(unsigned char * p, size_t len);

int rio_is_valid_path(RioEvalStore * store, const char * basename, int * out_valid, char ** err);

/* *out_json: {"nar_hash":hex,"nar_size":n,"references":[full paths],
 * "ca":opt} or null when unknown. */
int rio_query_path_info(RioEvalStore * store, const char * basename, char ** out_json, char ** err);

int rio_query_path_from_hash_part(
    RioEvalStore * store, const char * hash_part, char ** out_path, char ** err);

/* addToStoreFromDump. refs_json: JSON array of full store paths.
 * *out_json: {"path":…,"nar_sha256":hex,"nar_size":n}. */
int rio_add_from_dump(
    RioEvalStore * store,
    const char * name,
    int dump_method,
    int ca_method,
    const char * refs_json,
    rio_read_cb read_cb,
    void * read_ctx,
    rio_path_cb path_cb,
    void * path_ctx,
    char ** out_json,
    char ** err);

/* addToStore(info, source). info_json:
 * {"path":full,"nar_hash":hex,"nar_size":n,"references":[…],"ca":opt}. */
int rio_add_nar(
    RioEvalStore * store, const char * info_json, rio_read_cb read_cb, void * read_ctx, char ** err);

/* addToStore(SourcePath) on a physical path: ingest the local tree at
 * fs_path through the single-read two-plane pipeline. Stores NO file
 * content (chunk metadata + directory blobs only — the origin tree is
 * the byte store). Recursive (NAR) sha256 content addressing only; use
 * rio_add_from_dump for everything else. refs_json / *out_json as in
 * rio_add_from_dump. */
int rio_add_source_tree(
    RioEvalStore * store,
    const char * fs_path,
    const char * name,
    const char * refs_json,
    rio_path_cb path_cb,
    void * path_ctx,
    char ** out_json,
    char ** err);

/* writeDerivation. name = store-path name ("foo-1.2.drv"); aterm = the
 * canonical bytes nix hashed; drv_json = nix's derivation JSON;
 * nix_drv_path = nix's computed path (hard cross-check). */
int rio_write_derivation(
    RioEvalStore * store,
    const char * name,
    const unsigned char * aterm,
    size_t aterm_len,
    const unsigned char * drv_json,
    size_t drv_json_len,
    const char * nix_drv_path,
    char ** out_path,
    char ** err);

/* lstat within a store object. rel may be empty (object root). On rc 0,
 * *out is filled; kind RIO_NODE_MISSING means no such path. */
int rio_lstat(
    RioEvalStore * store, const char * basename, const char * rel, RioStat * out, char ** err);

/* Read a directory as a flat byte buffer (hot path — the shim walks it,
 * no parse). Layout, little-endian, unaligned:
 *   u32 count, then per entry: u8 kind (RIO_NODE_*), u32 name_len,
 *   name bytes (raw, not NUL-terminated — entry names may be any bytes).
 * Free *out_buf with rio_bytes_free(*out_buf, *out_len). */
int rio_read_directory(
    RioEvalStore * store,
    const char * basename,
    const char * rel,
    unsigned char ** out_buf,
    size_t * out_len,
    char ** err);

int rio_read_file(
    RioEvalStore * store,
    const char * basename,
    const char * rel,
    rio_write_cb write_cb,
    void * write_ctx,
    char ** err);

int rio_read_link(
    RioEvalStore * store, const char * basename, const char * rel, char ** out_target, char ** err);

int rio_nar_from_path(
    RioEvalStore * store, const char * basename, rio_write_cb write_cb, void * write_ctx, char ** err);

/* Stat-fingerprint shortcut for addToStore(SourcePath) on a physical
 * path. name = the store-path name the path will be minted under (part
 * of the record key). *out_path null on miss. */
int rio_fingerprint_lookup(
    RioEvalStore * store,
    const char * fs_path,
    const char * name,
    int ca_method,
    const char * refs_json,
    char ** out_path,
    char ** err);

int rio_fingerprint_record(
    RioEvalStore * store,
    const char * fs_path,
    const char * name,
    int ca_method,
    const char * refs_json,
    const char * store_path,
    char ** err);

/* ── eval-parent surface (ADR-024 P3b) ─────────────────────────────────
 * Used only by the rio-eval binary (never the plugin). */

/* Worker eval callback: invoked IN THE FORKED WORKER once per assigned
 * attr. Evaluate `attr`, call rio_emit_result with the root drv path,
 * return 0. On failure write a NUL-terminated message into err_buf
 * (capacity err_cap) and return nonzero — the worker reports a
 * non-fatal WorkerError for the attr. */
typedef int (*rio_eval_cb)(
    void * ctx, const char * attr, int worker_fd, char * err_buf, size_t err_cap);

/* Run the eval-parent orchestration loop (fork workers, relay frames,
 * route IFD completions, recycle, crash-requeue) until the
 * coordinator's Shutdown drains. chan_fd = the coordinator channel
 * (fd 3). opts_json (nullable): {"max_workers":N,"recycle_attrs":N,
 * "recycle_rss_mb":N,"attr_retries":N}. The process MUST be
 * single-threaded at the call (fork-no-exec). */
int rio_eval_parent_run(
    RioEvalStore * store,
    int chan_fd,
    const char * opts_json,
    rio_eval_cb cb,
    void * ctx,
    char ** err);

/* Assemble + send the final ResultFrame for `attr` (root drv = full
 * /nix/store/....drv path) on the worker channel fd. */
int rio_emit_result(
    RioEvalStore * store, int fd, const char * attr, const char * root_drv_path, char ** err);

/* Relay an import-from-derivation to the coordinator and BLOCK until
 * it resolves. On success the outputs are imported into this worker's
 * eval store and *out_json is a JSON array of output store paths. */
int rio_ifd_request(
    RioEvalStore * store, int fd, const char * drv_path, char ** out_json, char ** err);

#ifdef __cplusplus
}
#endif

#endif /* RIO_EVALSTORE_H */
