//! Store-side materialization executor (substitution-replacement
//! design §2.2/§5).
//!
//! Each store replica runs the pull-protocol client side of
//! materialization jobs: poll the scheduler's leader for claimable
//! jobs ([`client::poll_and_claim`]), claim one open attempt per job
//! through `PullAssignment` (kind=MATERIALIZATION + the per-replica
//! [`executor_instance`] identity), execute the in-process
//! reference-closure walk against this replica's own substitution
//! machinery, pin every ingested/verified path at ingest, and report
//! the outcome through `ReportOutcome` retried until acknowledged.
//!
//! **Phase A dormancy:** everything here is reachable ONLY when
//! `materialization.enabled = true` (default `false`). Flag-off,
//! `main.rs` never spawns the executor task set and the store is
//! byte-for-byte the as-built store — the dormancy proof is the
//! unchanged store battery plus the Wave-6 VM assertion.
//!
//! **Identity** (BC-1 + the Wave-3/4 security obligations):
//! - The *credential* is the kind-attested store-service token —
//!   `ServiceClaims { caller: "rio-store" }` signed with the service
//!   HMAC key (`service_hmac_key_path`), attached per-request by
//!   [`rio_auth::hmac::ServiceTokenInterceptor`]. Executor tokens are
//!   builder/fetcher pod-class credentials and never authorize
//!   materialization operations; the scheduler rejects them.
//! - The *replica identity* (`executor_instance`) is derived from this
//!   pod's own identity ([`executor_instance`]: the `HOSTNAME` pod
//!   name, a DNS-1123 label) and validated again scheduler-side. The
//!   full token-claim binding of the instance (the scheduler verifying
//!   rather than trusting it) is a recorded Phase B obligation — it
//!   requires a ServiceClaims field addition, which is a cross-cutting
//!   rio-auth wire change (`deny_unknown_fields` skew, the bug_011
//!   class).
//!
//! Spec: `store.materialize.executor`; design §2.2 (store as pull
//! client), §5 (pin-at-ingest).
// r[impl store.materialize.executor]

pub mod client;

/// The per-replica executor identity (BC-1): the pod name.
///
/// `HOSTNAME` is set by the kubelet to the pod name — a DNS-1123 label
/// (lowercase alphanumerics + interior hyphens, ≤63 chars), which is
/// exactly the alphabet the scheduler validates `executor_instance`
/// against (the composite ExecutorId `{intent}@{instance}` must stay
/// unambiguous). `RIO_STORE_REPLICA_ID` (the pod *IP* injected for the
/// TailLog proxy) is NOT used here: an IP literal contains dots/colons
/// and is not a DNS-1123 label.
///
/// Values that fail the label check (non-k8s dev hosts with uppercase
/// or dotted hostnames) are sanitized: lowercased, invalid bytes
/// replaced with `-`, trimmed to 63 chars, stripped of edge hyphens.
/// Empty/unset falls back to `"rio-store-dev"`.
// r[impl store.materialize.executor]
pub fn executor_instance() -> String {
    let raw = std::env::var("HOSTNAME").unwrap_or_default();
    sanitize_dns1123_label(&raw)
}

/// Sanitize an arbitrary hostname into a DNS-1123 label (the
/// scheduler-side validation alphabet — keep in sync with
/// `is_dns1123_label` in rio-scheduler/src/grpc/executor_service.rs).
fn sanitize_dns1123_label(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '-',
        })
        .take(63)
        .collect();
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "rio-store-dev".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instance derivation produces a scheduler-acceptable DNS-1123
    /// label from every input shape: a real pod name passes through
    /// unchanged; uppercase/dotted dev hostnames are sanitized; empty
    /// falls back to the dev constant. (The Wave-4 instance-attestation
    /// obligation, Phase-A form: identity from the pod's own
    /// environment, alphabet-validated on both sides.)
    // r[verify store.materialize.executor]
    #[test]
    fn executor_instance_is_always_a_dns1123_label() {
        let is_label = |s: &str| {
            !s.is_empty()
                && s.len() <= 63
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !s.starts_with('-')
                && !s.ends_with('-')
        };

        // A real pod name is unchanged.
        assert_eq!(
            sanitize_dns1123_label("rio-store-7d4b8f9c6-x2vpl"),
            "rio-store-7d4b8f9c6-x2vpl"
        );
        // Uppercase / dots / underscores are sanitized, not rejected.
        for raw in [
            "MyDevBox.local",
            "host_with_underscores",
            "UPPER",
            "a".repeat(100).as_str(),
            "-leading-and-trailing-",
            "",
        ] {
            let label = sanitize_dns1123_label(raw);
            assert!(
                is_label(&label),
                "sanitize({raw:?}) produced a non-label: {label:?}"
            );
        }
        // Empty input → the dev fallback.
        assert_eq!(sanitize_dns1123_label(""), "rio-store-dev");
        assert_eq!(sanitize_dns1123_label("..."), "rio-store-dev");
    }
}
