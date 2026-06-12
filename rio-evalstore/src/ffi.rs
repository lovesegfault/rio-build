//! `extern "C"` surface for the C++ shim (`shim/shim.cc`).
//!
//! Contract (mirrored in `shim/rio_evalstore.h`):
//! - Every function returns `0` (ok), `1` (error) or `2` (unsupported /
//!   foreign path). On nonzero, `*err` is set to a Rust-allocated
//!   NUL-terminated message the caller frees with [`rio_string_free`].
//! - Out-strings are allocated here and freed by the caller via
//!   [`rio_string_free`]. A null out-string with rc 0 means "not found".
//! - Every entry point wraps its body in `catch_unwind` — panics never
//!   cross the FFI boundary.
//! - Streaming callbacks (C → Rust pulls via `RioReadCb`, Rust → C pushes
//!   via `RioWriteCb`) return nonzero on failure; the Rust side converts
//!   that to an error return without unwinding, so the C++ side can stash
//!   and rethrow its original exception.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{self, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::store::{
    AddHashes, CaMethod, DumpMethod, EntryKind, EvalStore, EvalStoreError, PathStat, ProvidedInfo,
};

pub const RIO_OK: c_int = 0;
pub const RIO_ERR: c_int = 1;
pub const RIO_UNSUPPORTED: c_int = 2;

/// Pull bytes from the C++ `Source`. Returns 0 on success with `*n_read`
/// set (0 = EOF), nonzero on failure.
pub type RioReadCb =
    unsafe extern "C" fn(ctx: *mut c_void, buf: *mut u8, cap: usize, n_read: *mut usize) -> c_int;

/// Push bytes into the C++ `Sink`. Returns 0 on success.
pub type RioWriteCb = unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize) -> c_int;

/// Cross-check callback: receives the ingest hashes as JSON and must write
/// nix's computed store path (NUL-terminated) into `out_path`. Returns 0
/// on success.
pub type RioPathCb = unsafe extern "C" fn(
    ctx: *mut c_void,
    hashes_json: *const c_char,
    out_path: *mut c_char,
    out_cap: usize,
) -> c_int;

fn set_out_string(out: *mut *mut c_char, s: Option<String>) {
    if out.is_null() {
        return;
    }
    let val = match s {
        Some(s) => CString::new(s)
            .unwrap_or_else(|_| {
                CString::new("rio-evalstore: string contained NUL").expect("static")
            })
            .into_raw(),
        None => std::ptr::null_mut(),
    };
    // SAFETY: caller passes a valid out-pointer.
    unsafe { *out = val };
}

fn rc_for(e: &EvalStoreError) -> c_int {
    match e {
        EvalStoreError::ForeignPath(_) | EvalStoreError::Unsupported(_) => RIO_UNSUPPORTED,
        _ => RIO_ERR,
    }
}

/// Run `f` under `catch_unwind`, mapping panics and errors to return
/// codes + an error message in `*err`.
fn guard<F>(err: *mut *mut c_char, f: F) -> c_int
where
    F: FnOnce() -> Result<(), EvalStoreError>,
{
    set_out_string(err, None);
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => RIO_OK,
        Ok(Err(e)) => {
            let rc = rc_for(&e);
            set_out_string(err, Some(e.to_string()));
            rc
        }
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            set_out_string(err, Some(format!("rio-evalstore panicked: {msg}")));
            RIO_ERR
        }
    }
}

/// # Safety
/// `p` must be null or a valid NUL-terminated string.
unsafe fn opt_str<'a>(p: *const c_char) -> Result<Option<&'a str>, EvalStoreError> {
    if p.is_null() {
        return Ok(None);
    }
    // SAFETY: caller contract.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map(Some)
        .map_err(|e| EvalStoreError::Unsupported(format!("non-UTF8 string argument: {e}")))
}

/// # Safety
/// `p` must be a valid NUL-terminated string (non-null).
unsafe fn req_str<'a>(p: *const c_char) -> Result<&'a str, EvalStoreError> {
    // SAFETY: caller contract.
    unsafe { opt_str(p) }?
        .ok_or_else(|| EvalStoreError::Unsupported("unexpected null string argument".to_string()))
}

/// # Safety
/// `p` must be valid for `len` bytes when `len > 0`.
unsafe fn byte_slice<'a>(p: *const u8, len: usize) -> &'a [u8] {
    // from_raw_parts requires a non-null pointer even for len 0; a C
    // caller may legitimately pass (NULL, 0) for an empty buffer.
    if len == 0 {
        &[]
    } else {
        // SAFETY: caller contract.
        unsafe { std::slice::from_raw_parts(p, len) }
    }
}

/// Raw-byte names/targets cross this boundary as JSON strings or C
/// strings, so non-UTF-8 is a clean error here — never lossy mangling
/// (a wrong name is worse than a refusal).
fn utf8_name(bytes: Vec<u8>, what: &str) -> Result<String, EvalStoreError> {
    String::from_utf8(bytes).map_err(|e| {
        EvalStoreError::Unsupported(format!(
            "non-UTF-8 {what} cannot cross the FFI boundary: {e}"
        ))
    })
}

fn parse_refs_json(json: &str) -> Result<Vec<String>, EvalStoreError> {
    serde_json::from_str(json)
        .map_err(|e| EvalStoreError::Unsupported(format!("malformed references JSON: {e}")))
}

fn dump_method(v: c_int) -> Result<DumpMethod, EvalStoreError> {
    match v {
        0 => Ok(DumpMethod::Flat),
        1 => Ok(DumpMethod::NixArchive),
        _ => Err(EvalStoreError::Unsupported(format!(
            "unknown dump method {v}"
        ))),
    }
}

fn ca_method(v: c_int) -> Result<CaMethod, EvalStoreError> {
    match v {
        0 => Ok(CaMethod::Flat),
        1 => Ok(CaMethod::NixArchive),
        2 => Ok(CaMethod::Text),
        _ => Err(EvalStoreError::Unsupported(format!(
            "unsupported content-address method {v} (git hashing is not implemented)"
        ))),
    }
}

struct CbReader {
    cb: RioReadCb,
    ctx: *mut c_void,
}

impl Read for CbReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut n: usize = 0;
        // SAFETY: cb/ctx supplied by the shim; buffer is valid for cap bytes.
        let rc = unsafe { (self.cb)(self.ctx, buf.as_mut_ptr(), buf.len(), &mut n) };
        if rc != 0 {
            return Err(io::Error::other("source callback failed"));
        }
        if n > buf.len() {
            return Err(io::Error::other("source callback overflowed buffer"));
        }
        Ok(n)
    }
}

struct CbWriter {
    cb: RioWriteCb,
    ctx: *mut c_void,
}

impl Write for CbWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: cb/ctx supplied by the shim; data valid for len bytes.
        let rc = unsafe { (self.cb)(self.ctx, buf.as_ptr(), buf.len()) };
        if rc != 0 {
            return Err(io::Error::other("sink callback failed"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn call_path_cb(
    cb: RioPathCb,
    ctx: *mut c_void,
    hashes: &AddHashes,
) -> Result<String, EvalStoreError> {
    let json = serde_json::to_string(hashes)
        .map_err(|e| EvalStoreError::Unsupported(format!("hash JSON encode failed: {e}")))?;
    let cjson = CString::new(json)
        .map_err(|e| EvalStoreError::Unsupported(format!("hash JSON contained NUL: {e}")))?;
    let mut buf = [0u8; 1024];
    // SAFETY: cb/ctx supplied by the shim; buffer valid for its length.
    let rc = unsafe {
        cb(
            ctx,
            cjson.as_ptr(),
            buf.as_mut_ptr().cast::<c_char>(),
            buf.len(),
        )
    };
    if rc != 0 {
        return Err(EvalStoreError::Unsupported(
            "nix path cross-check callback failed".to_string(),
        ));
    }
    let nul = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| EvalStoreError::Unsupported("path callback result not terminated".into()))?;
    String::from_utf8(buf[..nul].to_vec())
        .map_err(|e| EvalStoreError::Unsupported(format!("path callback returned non-UTF8: {e}")))
}

fn store_ref<'a>(store: *mut EvalStore) -> &'a EvalStore {
    // SAFETY: handle produced by rio_store_open and not yet freed.
    unsafe { &*store }
}

// ---------------------------------------------------------------------------
// exported functions
// ---------------------------------------------------------------------------

/// Open the store. `cas_dir` may be null/empty → XDG default.
///
/// # Safety
/// `out_store` and `err` must be valid pointers; `cas_dir` null or a valid
/// C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_store_open(
    cas_dir: *const c_char,
    out_store: *mut *mut EvalStore,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let dir = unsafe { opt_str(cas_dir) }?;
        let store = EvalStore::open(dir)?;
        // SAFETY: caller passes a valid out-pointer.
        unsafe { *out_store = Box::into_raw(Box::new(store)) };
        Ok(())
    })
}

/// # Safety
/// `store` must come from [`rio_store_open`]; not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_store_free(store: *mut EvalStore) {
    if !store.is_null() {
        // Dropping dumps stats when RIO_EVALSTORE_STATS=1. A panicking
        // Drop must not unwind into C++.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: caller contract.
            drop(unsafe { Box::from_raw(store) });
        }));
    }
}

/// Free a string returned through any out-parameter.
///
/// # Safety
/// `s` must be a string allocated by this library (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: allocated via CString::into_raw.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// # Safety
/// Standard contract: valid store handle, valid C strings, valid out-pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_is_valid_path(
    store: *mut EvalStore,
    basename: *const c_char,
    out_valid: *mut c_int,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        let valid = store_ref(store).is_valid_path(basename);
        // SAFETY: caller contract.
        unsafe { *out_valid = c_int::from(valid) };
        Ok(())
    })
}

/// Query path info. `*out_json` is null when the path is unknown.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_query_path_info(
    store: *mut EvalStore,
    basename: *const c_char,
    out_json: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        let info = store_ref(store).query_path_info(basename)?;
        let json =
            match info {
                Some(info) => Some(serde_json::to_string(&info).map_err(|e| {
                    EvalStoreError::Corrupt(format!("path info encode failed: {e}"))
                })?),
                None => None,
            };
        set_out_string(out_json, json);
        Ok(())
    })
}

/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_query_path_from_hash_part(
    store: *mut EvalStore,
    hash_part: *const c_char,
    out_path: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let part = unsafe { req_str(hash_part) }?;
        set_out_string(out_path, store_ref(store).query_path_from_hash_part(part)?);
        Ok(())
    })
}

/// Ingest a dump (`addToStoreFromDump`). `refs_json` is a JSON array of
/// full store paths. On success `*out_json` carries
/// `{"path":…,"nar_sha256":…,"nar_size":…}`.
///
/// # Safety
/// Standard contract; `read_cb`/`path_cb` must be valid for the duration
/// of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_add_from_dump(
    store: *mut EvalStore,
    name: *const c_char,
    dump_method_raw: c_int,
    ca_method_raw: c_int,
    refs_json: *const c_char,
    read_cb: RioReadCb,
    read_ctx: *mut c_void,
    path_cb: RioPathCb,
    path_ctx: *mut c_void,
    out_json: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let name = unsafe { req_str(name) }?;
        // SAFETY: caller contract.
        let refs = parse_refs_json(unsafe { req_str(refs_json) }?)?;
        let dm = dump_method(dump_method_raw)?;
        let cm = ca_method(ca_method_raw)?;
        let mut reader = CbReader {
            cb: read_cb,
            ctx: read_ctx,
        };
        let result =
            store_ref(store).add_from_dump(name, dm, cm, &refs, &mut reader, &mut |h| {
                call_path_cb(path_cb, path_ctx, h)
            })?;
        set_out_string(
            out_json,
            Some(
                serde_json::to_string(&result).map_err(|e| {
                    EvalStoreError::Corrupt(format!("add result encode failed: {e}"))
                })?,
            ),
        );
        Ok(())
    })
}

/// Ingest a NAR with caller-provided path info (`addToStore(info, source)`).
/// `info_json`: `{"path":…,"nar_hash":hex,"nar_size":…,"references":[…],"ca":…}`.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_add_nar(
    store: *mut EvalStore,
    info_json: *const c_char,
    read_cb: RioReadCb,
    read_ctx: *mut c_void,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let info: ProvidedInfo = serde_json::from_str(unsafe { req_str(info_json) }?)
            .map_err(|e| EvalStoreError::Unsupported(format!("malformed path info JSON: {e}")))?;
        let mut reader = CbReader {
            cb: read_cb,
            ctx: read_ctx,
        };
        store_ref(store).add_nar(&info, &mut reader)?;
        Ok(())
    })
}

/// Capture a derivation. `name` is the store-path name (`foo-1.2.drv`),
/// `aterm` the canonical bytes nix hashed, `nix_drv_path` nix's computed
/// path (cross-checked). On success `*out_path` is the (identical) drv
/// path. `drv_json` is accepted for ABI stability but unused: drvs are
/// memory-only ATerm bytes (ADR-024); the canonical proto form is P2.
///
/// # Safety
/// Standard contract; `aterm`/`drv_json` valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_write_derivation(
    store: *mut EvalStore,
    name: *const c_char,
    aterm: *const u8,
    aterm_len: usize,
    _drv_json: *const u8,
    _drv_json_len: usize,
    nix_drv_path: *const c_char,
    out_path: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let name = unsafe { req_str(name) }?;
        // SAFETY: caller contract.
        let nix_path = unsafe { req_str(nix_drv_path) }?;
        // SAFETY: caller contract.
        let aterm = unsafe { byte_slice(aterm, aterm_len) };
        let path = store_ref(store).write_derivation(name, aterm, nix_path)?;
        set_out_string(out_path, Some(path));
        Ok(())
    })
}

/// lstat within a store object. `rel` may be empty (object root).
/// `*out_json` is null when missing; otherwise
/// `{"type":"regular","size":…,"executable":…}` / `{"type":"symlink","target":…}`
/// / `{"type":"directory"}`.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_lstat(
    store: *mut EvalStore,
    basename: *const c_char,
    rel: *const c_char,
    out_json: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        // SAFETY: caller contract.
        let rel = unsafe { req_str(rel) }?;
        let json = match store_ref(store).lstat(basename, rel)? {
            None => None,
            Some(PathStat::Regular { size, executable }) => Some(
                serde_json::json!({"type": "regular", "size": size, "executable": executable})
                    .to_string(),
            ),
            Some(PathStat::Symlink { target }) => {
                let target = utf8_name(target, "symlink target")?;
                Some(serde_json::json!({"type": "symlink", "target": target}).to_string())
            }
            Some(PathStat::Directory) => Some(serde_json::json!({"type": "directory"}).to_string()),
        };
        set_out_string(out_json, json);
        Ok(())
    })
}

/// Read a directory. `*out_json` maps entry name → "regular" | "symlink"
/// | "directory".
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_read_directory(
    store: *mut EvalStore,
    basename: *const c_char,
    rel: *const c_char,
    out_json: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        // SAFETY: caller contract.
        let rel = unsafe { req_str(rel) }?;
        let entries = store_ref(store).read_directory(basename, rel)?;
        let mut map = serde_json::Map::new();
        for (name, kind) in entries {
            let kind = match kind {
                EntryKind::Regular => "regular",
                EntryKind::Symlink => "symlink",
                EntryKind::Directory => "directory",
            };
            map.insert(
                utf8_name(name, "directory entry name")?,
                serde_json::Value::String(kind.to_string()),
            );
        }
        set_out_string(out_json, Some(serde_json::Value::Object(map).to_string()));
        Ok(())
    })
}

/// Stream a file's contents into `write_cb`.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_read_file(
    store: *mut EvalStore,
    basename: *const c_char,
    rel: *const c_char,
    write_cb: RioWriteCb,
    write_ctx: *mut c_void,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        // SAFETY: caller contract.
        let rel = unsafe { req_str(rel) }?;
        let mut writer = CbWriter {
            cb: write_cb,
            ctx: write_ctx,
        };
        store_ref(store).read_file(basename, rel, &mut writer)?;
        Ok(())
    })
}

/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_read_link(
    store: *mut EvalStore,
    basename: *const c_char,
    rel: *const c_char,
    out_target: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        // SAFETY: caller contract.
        let rel = unsafe { req_str(rel) }?;
        let target = utf8_name(store_ref(store).read_link(basename, rel)?, "symlink target")?;
        set_out_string(out_target, Some(target));
        Ok(())
    })
}

/// Regenerate the NAR for a store object into `write_cb`.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_nar_from_path(
    store: *mut EvalStore,
    basename: *const c_char,
    write_cb: RioWriteCb,
    write_ctx: *mut c_void,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let basename = unsafe { req_str(basename) }?;
        let mut writer = CbWriter {
            cb: write_cb,
            ctx: write_ctx,
        };
        store_ref(store).nar_from_path(basename, &mut writer)?;
        Ok(())
    })
}

/// Stat-fingerprint shortcut for `addToStore(SourcePath)` on a physical
/// path. `name` is the store-path name the caller will mint the path
/// under (part of the record key). `*out_path` is null on miss.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_fingerprint_lookup(
    store: *mut EvalStore,
    fs_path: *const c_char,
    name: *const c_char,
    ca_method_raw: c_int,
    refs_json: *const c_char,
    out_path: *mut *mut c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let fs_path = unsafe { req_str(fs_path) }?;
        // SAFETY: caller contract.
        let name = unsafe { req_str(name) }?;
        // SAFETY: caller contract.
        let refs = parse_refs_json(unsafe { req_str(refs_json) }?)?;
        let key = EvalStore::method_key(name, ca_method(ca_method_raw)?, &refs);
        set_out_string(
            out_path,
            store_ref(store).fingerprint_lookup(fs_path, &key)?,
        );
        Ok(())
    })
}

/// Record a fingerprint after a successful ingest of `fs_path`.
///
/// # Safety
/// Standard contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rio_fingerprint_record(
    store: *mut EvalStore,
    fs_path: *const c_char,
    name: *const c_char,
    ca_method_raw: c_int,
    refs_json: *const c_char,
    store_path: *const c_char,
    err: *mut *mut c_char,
) -> c_int {
    guard(err, || {
        // SAFETY: caller contract.
        let fs_path = unsafe { req_str(fs_path) }?;
        // SAFETY: caller contract.
        let name = unsafe { req_str(name) }?;
        // SAFETY: caller contract.
        let refs = parse_refs_json(unsafe { req_str(refs_json) }?)?;
        // SAFETY: caller contract.
        let store_path = unsafe { req_str(store_path) }?;
        let key = EvalStore::method_key(name, ca_method(ca_method_raw)?, &refs);
        store_ref(store).fingerprint_record(fs_path, &key, store_path)?;
        Ok(())
    })
}
