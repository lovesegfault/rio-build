//! `AdminService.ListBuilds` implementation.

use base64::Engine;
use rio_proto::types::{BuildInfo, BuildState, ListBuildsResponse};
use tonic::Status;
use uuid::Uuid;

use crate::db::{BuildListRow, SchedulerDb};

/// Cursor format version byte. Lets the encoding change later without
/// a proto wire break — clients treat cursors as opaque, so the only
/// compatibility surface is "does the scheduler that ISSUED the cursor
/// still accept it". A new scheduler can accept both v1 and a future v2
/// by dispatching on this prefix.
const CURSOR_V1: u8 = 0x01;

/// Fixed cursor length: 1B version + 8B submitted_at_micros (i64 BE)
/// plus 16B build_id UUID bytes. Fixed-width keeps decode trivial — no
/// delimiters, no length prefixes. 25 raw bytes → 34 chars in
/// URL_SAFE_NO_PAD base64, which is a reasonable opaque-token size
/// for a dashboard query param.
const CURSOR_V1_LEN: usize = 1 + 8 + 16;

/// Encode a keyset position as an opaque cursor string.
///
/// `submitted_at_micros`: PG-side `EXTRACT(EPOCH FROM submitted_at)*1e6`.
/// i64 microseconds covers ±292k years — no y2k38 / y2262 edge. Big-endian
/// so two cursors sort lexicographically the same as their decoded
/// `(micros, id)` tuples (not load-bearing for the SQL, but debugger-kind).
///
/// `build_id`: tiebreaker for rows sharing the same timestamp. PG's
/// default `now()` is microsecond-resolution, so identical-submitted_at
/// rows are rare but possible under bulk insert (batch submit from a
/// single CI job). The UUID disambiguates — and the SQL comparison is
/// row-value `(submitted_at, build_id) < (cursor_ts, cursor_id)`, so
/// both columns participate consistently.
fn encode_cursor(submitted_at_micros: i64, build_id: Uuid) -> String {
    let mut buf = [0u8; CURSOR_V1_LEN];
    buf[0] = CURSOR_V1;
    buf[1..9].copy_from_slice(&submitted_at_micros.to_be_bytes());
    buf[9..].copy_from_slice(build_id.as_bytes());
    // URL_SAFE_NO_PAD: no '/' (breaks URL paths), no '=' (breaks query-
    // string parsers that don't percent-encode). The dashboard feeds this
    // straight into `?cursor=…` so both matter.
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Decode an opaque cursor back to `(submitted_at_micros, build_id)`.
///
/// Rejects:
///   - non-base64 → `InvalidArgument("bad cursor")` (client bug or URL
///     mangling)
///   - wrong length → `InvalidArgument("bad cursor length")` (truncated
///     or a non-cursor string fed through)
///   - unknown version byte → `InvalidArgument("bad cursor version")`
///     (cursor from a future scheduler — shouldn't happen in practice
///     since cursors are short-lived page tokens, not stored state)
///
/// The error STRINGS are intentionally low-detail: the cursor is opaque,
/// so there's nothing actionable for the client beyond "start over from
/// page 1". The CODES are InvalidArgument (not Internal) because these
/// are all client-supplied-garbage cases.
fn decode_cursor(s: &str) -> Result<(i64, Uuid), Status> {
    let buf = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| Status::invalid_argument("bad cursor"))?;
    // Version-byte first, length second: a future v2 cursor with
    // different length would get "bad cursor length" (misleading) if
    // length were checked first. Version-first enables multi-version
    // dispatch. is_empty() guard makes buf[0] index safe without
    // relying on the length check.
    if buf.is_empty() || buf[0] != CURSOR_V1 {
        return Err(Status::invalid_argument("bad cursor version"));
    }
    if buf.len() != CURSOR_V1_LEN {
        return Err(Status::invalid_argument("bad cursor length"));
    }
    // Slice→array conversions: both are infallible after the len check
    // above, so `unwrap()` is structurally safe (not runtime-faith).
    let micros = i64::from_be_bytes(buf[1..9].try_into().unwrap());
    let build_id = Uuid::from_bytes(buf[9..25].try_into().unwrap());
    Ok((micros, build_id))
}

/// Query PG for builds with optional filters + pagination.
///
/// `cached_derivations` heuristic: "completed with no assignment row" —
/// a cache-hit derivation transitions directly to Completed at merge
/// time without ever being dispatched. See `actor/merge.rs` for the
/// in-memory `cached_count` tracking; this is the DB-side reconstruction
/// for historical queries.
///
/// Pagination: keyset (via `cursor`) is preferred — stable under
/// concurrent inserts, O(limit) per page regardless of depth. Offset
/// (via `offset`) is kept for backward compat with the dashboard's
/// page-number model; it degrades to O(offset + limit) scan per page
/// and can skip/duplicate rows if builds are submitted mid-walk. When
/// `cursor` is `Some`, `offset` is silently ignored (cursor wins — the
/// proto comment documents this precedence).
///
/// `next_cursor` is always populated when the returned page is full
/// (`len == limit`), even for offset-mode requests. This lets a client
/// start with offset=0 and switch to cursor-chained for page 2 onward
/// without a separate "give me a cursor" call.
///
/// `total_count` is populated only for the first page (cursor absent).
/// `count_builds` is an O(n) seq-scan — running it on every cursor page
/// would be O(n × pages), defeating keyset's O(limit)-per-page win.
/// Dashboard renders the total once from page 1; cursor pages return 0.
// r[impl sched.admin.list-builds]
pub(super) async fn list_builds(
    db: &SchedulerDb,
    status_filter: &str,
    tenant_filter: Option<Uuid>,
    limit: u32,
    offset: u32,
    cursor: Option<&str>,
) -> Result<ListBuildsResponse, Status> {
    let status_opt = (!status_filter.is_empty()).then_some(status_filter);
    let limit = limit.clamp(1, 1000) as i64;

    let (total, rows) = match cursor {
        Some(c) => {
            let (cursor_micros, cursor_id) = decode_cursor(c)?;
            let rows = db
                .list_builds_keyset(status_opt, tenant_filter, limit, cursor_micros, cursor_id)
                .await
                .map_err(|e| Status::internal(format!("list_builds: {e}")))?;
            // Cursor page → total not recomputed. Client carries the
            // first-page total forward; 0 here signals "unchanged/unknown".
            (0, rows)
        }
        None => db
            .list_builds(status_opt, tenant_filter, limit, offset as i64)
            .await
            .map_err(|e| Status::internal(format!("list_builds: {e}")))?,
    };

    // next_cursor: set iff this page is FULL (len == limit). A short
    // page is definitionally the last one — setting a cursor there would
    // invite a pointless empty-page round-trip. `rows.len() as i64` is
    // safe: clamp above caps limit at 1000, well under i64::MAX.
    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last())
        .flatten()
        .map(|r| encode_cursor(r.submitted_at_micros, r.build_id));

    Ok(ListBuildsResponse {
        builds: rows.into_iter().map(row_to_proto).collect(),
        total_count: total as u32,
        next_cursor,
    })
}

fn row_to_proto(r: BuildListRow) -> BuildInfo {
    let state = r
        .status
        .parse::<crate::state::BuildState>()
        .map(BuildState::from)
        .unwrap_or_else(|_| {
            tracing::warn!(status = %r.status, build_id = %r.build_id,
                "unknown build status from PG — rendering as Pending");
            BuildState::Pending
        }) as i32;
    BuildInfo {
        build_id: r.build_id.to_string(),
        tenant_id: r.tenant_id.unwrap_or_default(),
        priority_class: r.priority_class,
        state,
        total_derivations: r.total_derivations as u32,
        completed_derivations: r.completed_derivations as u32,
        cached_derivations: r.cached_derivations as u32,
        error_summary: r.error_summary.unwrap_or_default(),
        // Timestamps not populated in 4a — PG ::text format isn't RFC3339
        // and we don't have chrono. Same deferral as TenantInfo.created_at.
        // Dashboard can query PG directly if needed; 4b adds sqlx chrono
        // feature or extract(epoch) cast.
        submitted_at: None,
        started_at: None,
        finished_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cursor round-trips for arbitrary valid inputs.
    #[test]
    fn cursor_roundtrip() {
        for (micros, id) in [
            (0, Uuid::nil()),
            (1_700_000_000_000_000, Uuid::new_v4()),
            (i64::MAX, Uuid::max()),
            // Negative micros: not expected in practice (pre-epoch
            // submitted_at) but the codec shouldn't care — i64 BE
            // encodes the sign bit just fine.
            (-1, Uuid::new_v4()),
        ] {
            let encoded = encode_cursor(micros, id);
            let (m, i) = decode_cursor(&encoded).expect("round-trip");
            assert_eq!(m, micros);
            assert_eq!(i, id);
        }
    }

    /// URL_SAFE_NO_PAD alphabet — no characters that need percent-
    /// encoding in a query string. Proves the engine choice, not just
    /// asserts it.
    #[test]
    fn cursor_is_url_safe() {
        let c = encode_cursor(i64::MAX, Uuid::max());
        assert!(!c.contains('/'), "no path separator");
        assert!(!c.contains('+'), "no plus (space-collision in forms)");
        assert!(!c.contains('='), "no padding");
    }

    /// Bad inputs map to InvalidArgument with distinct messages.
    #[test]
    fn cursor_rejects_garbage() {
        // Not base64.
        assert_eq!(
            decode_cursor("not!base64!").unwrap_err().message(),
            "bad cursor"
        );
        // Empty buffer → version check fails (is_empty guard).
        let empty = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([] as [u8; 0]);
        assert_eq!(
            decode_cursor(&empty).unwrap_err().message(),
            "bad cursor version"
        );
        // Wrong version byte (checked first — a v2 cursor with
        // different length gets the version error, not length).
        let mut bad_ver = [0u8; CURSOR_V1_LEN];
        bad_ver[0] = 0xFF;
        let bad = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bad_ver);
        assert_eq!(
            decode_cursor(&bad).unwrap_err().message(),
            "bad cursor version"
        );
        // Right version, wrong length (10 bytes, first byte = v1).
        let mut short = [0u8; 10];
        short[0] = CURSOR_V1;
        let short_enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(short);
        assert_eq!(
            decode_cursor(&short_enc).unwrap_err().message(),
            "bad cursor length"
        );
    }
}
