fn main() {
    // `sqlx::migrate!` embeds `migrations/*.sql` at expansion time, outside
    // rustc's dep-info. `rerun-if-changed` alone re-runs rustc, but leaves
    // every input of a content-keyed rustc-wrapper cache (kache) unchanged
    // — which would restore a stale embedded MIGRATOR, silently defeating
    // the checksum-pinning workflow. Hash the directory into tracked
    // env-deps (consumed in lib.rs) so the content reaches every cache key;
    // track_dir also emits the rerun-if-changed. Same treatment for
    // `.sqlx/` (`query!` in src/schema.rs). See rio-buildhash.
    rio_buildhash::track_dir("RIO_MIGRATIONS_HASH", std::path::Path::new("migrations"));
    rio_buildhash::track_dir_upwards_or_env("RIO_SQLX_HASH", "SQLX_OFFLINE_DIR", ".sqlx");
}
