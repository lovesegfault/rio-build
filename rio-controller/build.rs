fn main() {
    // `sqlx::query!` reads `.sqlx/query-*.json` (offline mode) outside
    // rustc's dep-info. Hash that set into a tracked env-dep (consumed in
    // lib.rs) so cargo and content-keyed rustc-wrapper caches (kache)
    // re-key on query-metadata changes without `.rs` edits. The directory
    // comes exclusively from SQLX_OFFLINE_DIR — the variable
    // sqlx-macros-core checks first in its per-query fallthrough chain —
    // so when it points at the real cache, the hash and the macros agree
    // by construction; degraded states and divergent fallthrough caches
    // are keyed uniquely (uncacheable) instead. See rio-buildhash for the
    // contract.
    rio_buildhash::track_sqlx();
}
