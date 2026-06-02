fn main() {
    // `sqlx::query!` reads `.sqlx/*.json` (offline mode) outside rustc's
    // dep-info. Hash it into a tracked env-dep (consumed in lib.rs) so
    // cargo and content-keyed rustc-wrapper caches (kache) re-key on
    // query-metadata changes without `.rs` edits. See rio-buildhash.
    rio_buildhash::track_dir_upwards_or_env("RIO_SQLX_HASH", "SQLX_OFFLINE_DIR", ".sqlx");
}
