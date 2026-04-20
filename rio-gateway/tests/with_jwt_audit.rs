//! Structural audit: every outbound gRPC call from the gateway must
//! either go through `with_jwt` (which injects W3C `traceparent` +
//! tenant JWT) or carry a `// no-jwt:` opt-out comment documenting why
//! the call is intentionally anonymous.
//!
//! `with_jwt` is explicit per-call (not a tonic `Interceptor` layer —
//! see `handler/mod.rs` rustdoc for why). The per-call discipline only
//! works if every site remembers; this test makes omission a CI
//! failure rather than a silent orphan-root span on the server side.
//!
//! Regression guard for bug_359/bug_103/bug_171 (three of ~14 sites
//! were unwrapped at one point: `tenant_quota`, `watch_build`,
//! `query_realisation`).

/// Source files that contain `*_client.method(...)` outbound calls.
/// `include_str!` so the audit operates on the source as-compiled.
const SOURCES: &[(&str, &str)] = &[
    ("handler/build.rs", include_str!("../src/handler/build.rs")),
    (
        "handler/opcodes_read.rs",
        include_str!("../src/handler/opcodes_read.rs"),
    ),
    ("handler/grpc.rs", include_str!("../src/handler/grpc.rs")),
    ("quota.rs", include_str!("../src/quota.rs")),
    ("session.rs", include_str!("../src/session.rs")),
    ("translate.rs", include_str!("../src/translate.rs")),
    ("drv_cache.rs", include_str!("../src/drv_cache.rs")),
];

/// Is `line` a gRPC client call we want to audit? Matches
/// `<ident>_client.<method>(` where `<method>` is not `clone`.
fn is_client_call(line: &str) -> bool {
    let t = line.trim_start();
    // Reject comments and strings (best-effort — no rust parser here).
    if t.starts_with("//") || t.starts_with('*') {
        return false;
    }
    let Some(idx) = line.find("_client.") else {
        return false;
    };
    let after = &line[idx + "_client.".len()..];
    // Method-call open paren on this line; skip `.clone()` and field
    // access. The method name is the prefix up to `(`.
    let Some(paren) = after.find('(') else {
        return false;
    };
    let method = &after[..paren];
    !method.is_empty()
        && method != "clone"
        && method
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[test]
fn all_grpc_calls_use_with_jwt() {
    // Look-back window for `with_jwt` / `no-jwt:` evidence. Multi-field
    // request literals + the `match with_jwt(...) { Ok(r) => r, Err =>
    // fail_open }` shape can span ~15-18 lines; 20 covers those without
    // admitting an unrelated earlier call's wrapper (calls are spaced
    // by at least one fn/match boundary).
    const LOOKBACK: usize = 20;

    let mut violations: Vec<String> = Vec::new();

    for (name, src) in SOURCES {
        let lines: Vec<&str> = src.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !is_client_call(line) {
                continue;
            }
            // Argument is `req` / `request` / `watch_req` etc — a
            // pre-wrapped tonic::Request. Sufficient evidence the
            // call site delegated wrapping to its builder.
            let start = i.saturating_sub(LOOKBACK);
            let window = &lines[start..=i];
            let covered = window.iter().any(|l| {
                l.contains("with_jwt(")
                    || l.contains("no-jwt:")
                    || l.contains("jwt_metadata(")
                    || l.contains("grpc_get_path(")
            });
            if !covered {
                violations.push(format!("{name}:{}: {}", i + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "outbound gRPC calls without with_jwt() or `// no-jwt:` opt-out \
         (orphan trace spans + missing tenant context):\n  {}",
        violations.join("\n  ")
    );
}
