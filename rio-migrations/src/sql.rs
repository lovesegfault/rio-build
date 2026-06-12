//! Cross-crate SQL texts against shared tables.
//!
//! rio-store and rio-scheduler write the same PG (PD-13: rio-store
//! cannot link rio-scheduler), so statements both crates must execute
//! IDENTICALLY live here, in the crate both already link for
//! [`crate::MIGRATOR`]. One text, two executors — the "keep both
//! copies in sync" comment pair this replaces (bug_192) is the exact
//! drift class the wave deletes.

/// The §5.1 pin-at-ingest upsert (design §5.1; migration 093 key).
///
/// Binds: `$1 bytea[]` store_path_hash batch, `$2 text[]` parallel
/// drv_hash batch, `$3 uuid` job_id. The store-side executor binds
/// 1-element arrays; the scheduler's `pin_materialized_paths` binds
/// the full batch.
///
/// `ON CONFLICT` targets the FULL 093 primary key
/// `(store_path_hash, drv_hash, pin_kind)`: re-pinning the same
/// materialization path is an idempotent `job_id` refresh, and a
/// build_input pin for the same `(path, drv)` is a DIFFERENT row —
/// never re-kinded (bug_253; see `M_093`).
pub const PIN_MATERIALIZED_UPSERT_SQL: &str = "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash, pin_kind, job_id) \
     SELECT h, d, 'materialization', $3 \
       FROM UNNEST($1::bytea[], $2::text[]) AS u(h, d) \
     ON CONFLICT (store_path_hash, drv_hash, pin_kind) DO UPDATE \
         SET job_id = EXCLUDED.job_id";

/// The live-wanted name rows for one derivation, by drv_hash
/// (bug_027 / merged_bug_059): the store executor's
/// `live_wanted_paths` runs THIS text; the scheduler's
/// `effective_wanted_union` (db/wanted.rs) reads the same
/// `live_wanted_interest` view with its note-bearing projection. The
/// un-forkable width definition is the triple this const anchors:
/// (a) the live predicate — the `live_wanted_interest` view, defined
/// once in the migrations; (b) the name fold —
/// `rio_common::wanted_outputs::saturating_wanted_union`, one body
/// for every consumer; (c) the carrier union — UNCONDITIONAL in both
/// consumption legs (the store's walk read and the scheduler's
/// consumption coverage), so a carried job whose live interest
/// vanished mid-claim computes the SAME width on both sides.
///
/// Binds: `$1 text` drv_hash. Row: (output_names,
/// expected_output_paths, wanted_output_names) — one row per live
/// interested build.
pub const LIVE_WANTED_NAME_ROWS_BY_DRV_SQL: &str = "SELECT d.output_names, d.expected_output_paths, i.wanted_output_names \
       FROM derivations d \
       JOIN live_wanted_interest i USING (derivation_id) \
      WHERE d.drv_hash = $1";

/// THE definition of `log_ingest_sessions` liveness (bug_234): a
/// session is LIVE iff its heartbeat is younger than this. Every
/// consumer derives from here — the store's `lookup_live` routing
/// read, `acquire`'s steal arm (staleness = `NOT` this predicate),
/// the store's stale-session reap, and the scheduler's
/// `gc_exec_rows` conjunct 5. Before this const, the scheduler's
/// conjunct checked bare row EXISTENCE while every doc promised
/// liveness: an uncleanly-died stream's leftover row pinned its exec
/// row for the full `log_retention` (30 d) regardless of the
/// operator's `exec_retention_days`.
///
/// Derived at the CONSUMER'S clock (merged_bug_014): every consumer
/// of this bound evaluates the age of the last COMMITTED
/// `heartbeat_at` stamp, so the margin must cover the worst
/// committed-stamp age of a HEALTHY session that missed one beat —
/// `2 × HEARTBEAT_INTERVAL + HEARTBEAT_RPC_BOUND` (the recovery
/// tick fires one interval after the missed one, and its UPDATE may
/// take the full RPC bound to land) — plus a strictly positive
/// slack. With the store's shipped cadence (15 s beats, 10 s RPC
/// bound) the worst healthy committed age is 40 s; this value adds
/// the 5 s `SESSION_MARGIN_SLACK`. `rio-store::logs::sessions`
/// compile-asserts the full inequality (the predecessor pair —
/// 15 × 2 == 30 — certified the producer-side cadence and read a
/// healthy one-miss session dead for up to 10 s: the steal arm
/// deposed healthy owners, `lookup_live` reported `Stale`, and the
/// scheduler's gc conjunct misread).
pub const SESSION_STALE_AFTER_SECS: f64 = 45.0;

/// SQL predicate fragment: the named `heartbeat_at` column is LIVE.
/// `hb` is the (qualified) column reference; `bind` the parameter
/// carrying [`SESSION_STALE_AFTER_SECS`] (a `make_interval` bind, so
/// the query text cannot drift from the const). Staleness is `NOT
/// (...)` of this — complementary by construction, never a second
/// hand-written comparison.
pub fn live_ingest_session_sql(hb: &str, bind: &str) -> String {
    format!("{hb} > now() - make_interval(secs => {bind})")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bug_192 smoke: the shared upsert conflicts on the FULL 093 key —
    /// a conflict target narrower than the PK would re-introduce the
    /// re-kind (the DO UPDATE would fire across kinds).
    #[test]
    fn pin_upsert_conflicts_on_the_093_key() {
        assert!(
            PIN_MATERIALIZED_UPSERT_SQL
                .contains("ON CONFLICT (store_path_hash, drv_hash, pin_kind)"),
            "the upsert's conflict target must be the full 093 primary key"
        );
        assert!(
            PIN_MATERIALIZED_UPSERT_SQL.contains("SET job_id = EXCLUDED.job_id")
                && !PIN_MATERIALIZED_UPSERT_SQL.contains("SET pin_kind"),
            "the DO UPDATE refreshes job_id only — never the kind"
        );
    }
}
