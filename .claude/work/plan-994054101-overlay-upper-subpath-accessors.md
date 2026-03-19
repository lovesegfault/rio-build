# Plan 994054101: OverlayMount upper-subpath accessors — centralize 5× `join("nix/store")` pattern

Consolidator finding. [`overlay.rs:110-113`](../../rio-worker/src/overlay.rs) already **enumerates** the three subpaths callers join onto `upper_dir()`: `nix/store` (outputs), `nix/var/nix/db` (synth DB), `etc/nix` (nix.conf). The doc-comment is the spec; the method doesn't exist. [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) added the 5th production `.join("nix/store")` site at [`executor/mod.rs:515`](../../rio-worker/src/executor/mod.rs) (FOD whiteout). [P0263](plan-0263-worker-client-side-chunker.md) (UNIMPL, touches [`upload.rs`](../../rio-worker/src/upload.rs)) is positioned to add a 6th. The string `"nix/store"` appearing at N call sites means N chances to typo it as `"nix/store/"` (trailing slash — `join` handles it, but the basename extraction downstream may not) or `"/nix/store"` (absolute — `join` drops the base, silent bug).

**Five production sites + the signature-layer problem:**

| Site | Subpath | Form |
|---|---|---|
| [`spawn.rs:94`](../../rio-worker/src/executor/daemon/spawn.rs) | `nix/var/nix/db` | `.upper_dir().join(...)` |
| [`spawn.rs:95`](../../rio-worker/src/executor/daemon/spawn.rs) | `etc/nix` | `.upper_dir().join(...)` |
| [`executor/mod.rs:515`](../../rio-worker/src/executor/mod.rs) | `nix/store` | `.upper_dir().join(...)` — P0308's whiteout site |
| [`upload.rs:81`](../../rio-worker/src/upload.rs) (inside `scan_new_outputs`) | `nix/store` | `upper_dir.join(...)` on `&Path` param |
| [`upload.rs:133`](../../rio-worker/src/upload.rs) (inside `upload_output`) | `nix/store` | `upper_dir.join(...)` on `&Path` param |
| [`executor/mod.rs:1022`](../../rio-worker/src/executor/mod.rs) (inside `setup_nix_conf`) | `etc/nix` | `upper_dir.join(...)` on `&Path` param |

The three `&Path`-param functions ([`scan_new_outputs`](../../rio-worker/src/upload.rs), [`upload_output`](../../rio-worker/src/upload.rs), [`setup_nix_conf`](../../rio-worker/src/executor/mod.rs)) are the deeper problem: they take `upper_dir: &Path` but the ONLY thing they do with it is `join("nix/store")` or `join("etc/nix")`. The signature lies — they want `upper_store: &Path` / `upper_nix_conf: &Path`, not the root. Caller at [`executor/mod.rs`](../../rio-worker/src/executor/mod.rs) calls `upload_all_outputs(..., overlay_mount.upper_dir(), ...)` and the join happens inside — the type system can't catch "passed `merged_dir()` instead of `upper_dir()`".

**~6-line accessor addition, ~12-line net after migrations.** [`overlay.rs:452`](../../rio-worker/src/overlay.rs) has a comment referencing the current pattern — update it.

## Tasks

### T1 — `refactor(worker):` add OverlayMount::{upper_store,upper_synth_db,upper_nix_conf}

MODIFY [`rio-worker/src/overlay.rs`](../../rio-worker/src/overlay.rs) at the `impl OverlayMount` block (`:106-135`). Add three methods after `upper_dir()` at `:116`:

```rust
/// `{upper}/nix/store` — the overlayfs upperdir itself. Build outputs
/// materialize here. scan_new_outputs + upload_output read this;
/// executor's FOD-whiteout (mknod) writes here.
pub fn upper_store(&self) -> PathBuf {
    self.upper.join("nix/store")
}

/// `{upper}/nix/var/nix/db` — synth-db bind-mount target. Populated by
/// synth_db before the daemon starts. NOT visible through the overlay
/// (separate bind mount in spawn.rs).
pub fn upper_synth_db(&self) -> PathBuf {
    self.upper.join("nix/var/nix/db")
}

/// `{upper}/etc/nix` — nix.conf bind-mount target. setup_nix_conf
/// populates it from WORKER_NIX_CONF or the rio-nix-conf ConfigMap
/// override. NOT visible through the overlay.
pub fn upper_nix_conf(&self) -> PathBuf {
    self.upper.join("etc/nix")
}
```

**Return `PathBuf` not `&Path`** — the subpaths aren't stored fields. The allocation is trivial (once per build per subpath, not hot-path). Storing them as fields would bloat `OverlayMount` for no benefit.

**`upper_dir()` stays** — it's the root for `create_dir_all` in overlay setup and the doc-comment's enumeration becomes the accessors' doc-comments (shorten the `:107-113` comment to "see the three `upper_*` accessors below").

Update [`overlay.rs:452`](../../rio-worker/src/overlay.rs) comment: `` `upper_dir().join("nix/store")` `` → `` `upper_store()` ``.

### T2 — `refactor(worker):` migrate .upper_dir().join() call sites

MODIFY three direct-call sites:

| File | Before | After |
|---|---|---|
| [`spawn.rs:94`](../../rio-worker/src/executor/daemon/spawn.rs) | `overlay_mount.upper_dir().join("nix/var/nix/db")` | `overlay_mount.upper_synth_db()` |
| [`spawn.rs:95`](../../rio-worker/src/executor/daemon/spawn.rs) | `overlay_mount.upper_dir().join("etc/nix")` | `overlay_mount.upper_nix_conf()` |
| [`executor/mod.rs:515`](../../rio-worker/src/executor/mod.rs) | `overlay_mount.upper_dir().join("nix/store")` | `overlay_mount.upper_store()` |

### T3 — `refactor(worker):` tighten fn signatures — upper_dir → upper_store / upper_nix_conf

MODIFY the three `&Path`-param functions. Rename the parameter AND delete the `join` inside:

**[`upload.rs:80`](../../rio-worker/src/upload.rs) `scan_new_outputs`:**
```rust
// Before: pub fn scan_new_outputs(upper_dir: &Path) -> ...
//         let store_dir = upper_dir.join("nix/store");
pub fn scan_new_outputs(upper_store: &Path) -> std::io::Result<Vec<String>> {
    let read_dir = match std::fs::read_dir(upper_store) {
```

**[`upload.rs:125-133`](../../rio-worker/src/upload.rs) `upload_output`:**
```rust
// Before: upper_dir: &Path,
//         let output_path = upper_dir.join("nix/store").join(output_basename);
async fn upload_output(
    store_client: &mut StoreServiceClient<Channel>,
    upper_store: &Path,   // ← renamed
    output_basename: &str,
    // ...
) -> Result<UploadResult, UploadError> {
    let output_path = upper_store.join(output_basename);  // ← one join gone
```

**[`upload.rs:485-492`](../../rio-worker/src/upload.rs) `upload_all_outputs`:**
```rust
// Before: upper_dir: &Path,
//         let outputs = scan_new_outputs(upper_dir)?;
pub async fn upload_all_outputs(
    store_client: &StoreServiceClient<Channel>,
    upper_store: &Path,   // ← renamed; passed through to scan + upload_output
    // ...
```

**[`executor/mod.rs:1021-1023`](../../rio-worker/src/executor/mod.rs) `setup_nix_conf`:**
```rust
// Before: fn setup_nix_conf(upper_dir: &Path) -> ...
//         let conf_dir = upper_dir.join("etc/nix");
fn setup_nix_conf(upper_nix_conf: &Path) -> Result<(), ExecutorError> {
    std::fs::create_dir_all(upper_nix_conf).map_err(ExecutorError::NixConf)?;
    // body uses upper_nix_conf directly, no join
```

**Update callers in [`executor/mod.rs`](../../rio-worker/src/executor/mod.rs)** — grep for `upload_all_outputs(` and `setup_nix_conf(` — pass `overlay_mount.upper_store()` / `overlay_mount.upper_nix_conf()` instead of `.upper_dir()`. The `&PathBuf` → `&Path` coercion is automatic.

**Test callers:** `grep 'scan_new_outputs\|upload_all_outputs\|setup_nix_conf' rio-worker/src/**/*.rs rio-worker/tests/` — any test that passes a temp dir now passes `temp.join("nix/store")` explicitly (or the test setup already creates that subdir — keep the test-side join; the function contract is "give me the store dir", not "give me the root and I'll find it").

## Exit criteria

- `/nbr .#ci` green
- `grep '.upper_dir().join(' rio-worker/src/` → 0 hits in `.rs` files (all direct-join sites migrated; the `:110-112` doc-comment is prose so `.rs` grep with `.upper_dir().join(` open-paren won't match it, but verify visually that `:107-113` got shortened)
- `grep 'upper_dir: &Path' rio-worker/src/upload.rs rio-worker/src/executor/mod.rs` → 0 hits (signatures renamed)
- `grep 'upper_store\|upper_synth_db\|upper_nix_conf' rio-worker/src/overlay.rs` → ≥3 hits (three `pub fn` definitions; plus doc-comment refs)
- `grep '"nix/store"\|"nix/var/nix/db"\|"etc/nix"' rio-worker/src/` — the three string literals appear ONLY in `overlay.rs` (accessors) and test setup (temp-dir construction). No hits in `upload.rs`, `executor/mod.rs`, `spawn.rs` production code. This is the centralization proof.
- `cargo clippy --all-targets -- --deny warnings` passes — rules out unused-import from the `Path`→`PathBuf` return shift

## Tracey

No new markers. Pure internal-API refactor — the overlay subpath layout was already spec'd by the `:107-113` doc-comment (prose, not a formal `r[...]` marker) and [`overlay.rs:452`](../../rio-worker/src/overlay.rs) comment. The three existing overlay markers (`r[worker.overlay.per-build]`, `r[worker.overlay.stacked-lower]`, `r[worker.overlay.upper-not-overlayfs]`) describe mount semantics, not path layout; this plan doesn't change mount semantics.

## Files

```json files
[
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "T1: add upper_store/upper_synth_db/upper_nix_conf pub fn after :116; shorten :107-113 doc-comment; update :452 comment"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "T2: :94-95 .upper_dir().join() → .upper_synth_db() / .upper_nix_conf()"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "T2: :515 → .upper_store(); T3: setup_nix_conf sig :1021 + caller; upload_all_outputs caller"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T3: scan_new_outputs :80 + upload_output :125 + upload_all_outputs :485 — upper_dir→upper_store, drop inner join"}
]
```

```
rio-worker/src/
├── overlay.rs              # T1: +3 accessors, doc-comment shrink
├── upload.rs               # T3: 3 fn sigs renamed, inner joins dropped
└── executor/
    ├── mod.rs              # T2+T3: .upper_store() call, setup_nix_conf sig+caller
    └── daemon/spawn.rs     # T2: 2 direct-join sites migrated
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [263, 308], "note": "No hard deps — all touched code exists on sprint-1. Soft-dep P0263 (UNIMPL, touches upload.rs T1 chunk-before-upload flow): P0263's files fence lists upload.rs MODIFY. If P0263 dispatches first, it may add a 6th .join('nix/store') that this plan then migrates; if this lands first, P0263 uses the new upper_store param directly (cleaner — prio=45 here vs P0263 prio=50 suggests P0263 goes first, which is fine; rebase picks up the renamed param trivially). Soft-dep P0308 (DONE — discovered_from): executor/mod.rs:515 is P0308's whiteout site, the 5th join that triggered the consolidator finding. No conflict, P0308 merged. Followup description mentioned 'inputs.rs' — no such file exists in rio-worker/src/; all three &Path-param functions are in upload.rs + executor/mod.rs."}
```

**Depends on:** none — all sites exist on sprint-1.

**Conflicts with:** [`executor/mod.rs`](../../rio-worker/src/executor/mod.rs) count=24 — mid-hot. T2 touches `:515` (one-line), T3 touches `:1021-1023` (sig rename) and the `upload_all_outputs` call site (one arg changed). Grep other UNIMPL plans for these line regions: none at `:515` or `:1021`. [`upload.rs`](../../rio-worker/src/upload.rs) — only [P0263](plan-0263-worker-client-side-chunker.md) touches it (T1 chunk-before-upload); that's the `upload_output` body, T3 here renames the `:127` param and deletes `:133`'s first `join`. P0263 will see a renamed param on rebase — trivial. [`overlay.rs`](../../rio-worker/src/overlay.rs) and [`spawn.rs`](../../rio-worker/src/executor/daemon/spawn.rs) not in top-30.
