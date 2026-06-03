fn main() {
    // `sqlx::query!` reads `.sqlx/query-*.json` (offline mode) outside
    // rustc's dep-info. Hash that set into a tracked env-dep (consumed in
    // lib.rs) so cargo and content-keyed rustc-wrapper caches (kache)
    // re-key on query-metadata changes without `.rs` edits. The directory
    // comes exclusively from SQLX_OFFLINE_DIR — the same variable
    // sqlx-macros-core checks first, so the hash and the macros always
    // agree. See rio-buildhash for the contract.
    rio_buildhash::track_sqlx();
}
