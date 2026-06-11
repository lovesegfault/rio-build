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

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RioEvalStore RioEvalStore;

enum {
    RIO_OK = 0,
    RIO_ERR = 1,
    RIO_UNSUPPORTED = 2,
};

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

int rio_is_valid_path(RioEvalStore * store, const char * basename, int * out_valid, char ** err);

/* *out_json: {"nar_hash":hex,"nar_size":n,"references":[full paths],
 * "ca":opt,"drv_json_blob":opt} or null when unknown. */
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

/* *out_json: {"type":"regular","size":n,"executable":b} |
 * {"type":"symlink","target":…} | {"type":"directory"} | null. */
int rio_lstat(
    RioEvalStore * store, const char * basename, const char * rel, char ** out_json, char ** err);

/* *out_json: {entry name: "regular"|"symlink"|"directory", …}. */
int rio_read_directory(
    RioEvalStore * store, const char * basename, const char * rel, char ** out_json, char ** err);

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
 * path. *out_path null on miss. */
int rio_fingerprint_lookup(
    RioEvalStore * store,
    const char * fs_path,
    int ca_method,
    const char * refs_json,
    char ** out_path,
    char ** err);

int rio_fingerprint_record(
    RioEvalStore * store,
    const char * fs_path,
    int ca_method,
    const char * refs_json,
    const char * store_path,
    char ** err);

#ifdef __cplusplus
}
#endif

#endif /* RIO_EVALSTORE_H */
