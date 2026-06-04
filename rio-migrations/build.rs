fn main() {
    // `sqlx::migrate!` embeds `migrations/*.sql` at expansion time, outside
    // rustc's dep-info. `rerun-if-changed` alone re-runs rustc, but leaves
    // every input of a content-keyed rustc-wrapper cache (kache) unchanged
    // — which would restore a stale embedded MIGRATOR, silently defeating
    // the checksum-pinning workflow. Hash the macro-visible set (top-level
    // *.sql) into a tracked env-dep (consumed in lib.rs) so the content
    // reaches every cache key. (No .sqlx/ tracking here: this crate has no
    // query! callsites — schema.rs only mentions query_as! in doc
    // comments; the consumers that expand it track .sqlx themselves.)
    // See rio-buildhash.
    rio_buildhash::track_migrations();
}
