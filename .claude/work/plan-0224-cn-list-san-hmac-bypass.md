# Plan 0224: CN-list config + SAN check for HMAC bypass

phase4c.md:61 — the `PutPath` HMAC-bypass check at [`rio-store/src/grpc/put_path.rs:65-127`](../../rio-store/src/grpc/put_path.rs) currently hardcodes `CN=rio-gateway` as the single bypass identity (per [`store.md:98`](../../docs/src/components/store.md)). Two gaps: (a) the CN is hardcoded — no config escape hatch for deployments that name their gateway cert differently; (b) modern cert deployments put identities in SAN extensions, not CN. cert-manager-issued certs may have an empty CN and a populated `DNSName` SAN.

This plan: `hmac_bypass_cns: Vec<String>` config (default `["rio-gateway"]`) + SAN DNSName check via [`x509-parser`](https://docs.rs/x509-parser). Bypass if **CN OR any SAN DNS** matches the allowlist.

## Tasks

### T1 — `feat(store):` hmac_bypass_cns config field

MODIFY [`rio-store/src/config.rs`](../../rio-store/src/config.rs):

```rust
/// Client-cert CNs/SAN-DNSNames that bypass HMAC verification on PutPath.
/// The gateway handles `nix copy --to` and has no assignment token;
/// its client cert identity is allowlisted instead. Default: ["rio-gateway"].
#[serde(default = "default_hmac_bypass_cns")]
pub hmac_bypass_cns: Vec<String>,

fn default_hmac_bypass_cns() -> Vec<String> {
    vec!["rio-gateway".to_string()]
}
```

Plumb the config into the `PutPath` handler (likely via the service struct that holds `config: Arc<StoreConfig>`).

### T2 — `feat(store):` SAN check in put_path.rs

MODIFY [`rio-store/src/grpc/put_path.rs`](../../rio-store/src/grpc/put_path.rs) — the CN block at `:65-127` becomes:

```rust
use x509_parser::prelude::*;

// r[impl store.hmac.san-bypass]
fn cert_identity_in_allowlist(cert_der: &[u8], allowlist: &[String]) -> bool {
    let Ok((_, cert)) = X509Certificate::from_der(cert_der) else { return false };

    // Check CN
    if let Some(cn) = cert.subject().iter_common_name().next()
        .and_then(|a| a.as_str().ok())
    {
        if allowlist.iter().any(|a| a == cn) { return true; }
    }

    // Check SAN DNSNames
    // TbsCertificate::subject_alternative_name() → Option<Result<&ParsedExtension>>
    if let Ok(Some(san)) = cert.tbs_certificate.subject_alternative_name() {
        for gn in &san.value.general_names {
            if let GeneralName::DNSName(dns) = gn {
                // dns is &str (IA5String decoded)
                if allowlist.iter().any(|a| a == dns) { return true; }
            }
        }
    }

    false
}
```

Replace the existing hardcoded `cn == "rio-gateway"` with `cert_identity_in_allowlist(&cert_der, &self.config.hmac_bypass_cns)`.

**Encoding gotcha:** `GeneralName::DNSName` wraps `&str` (already IA5String-decoded by x509-parser), not raw bytes. Match on the variant, use the inner `&str` directly. DO NOT try to decode IA5 yourself.

### T3 — `test(store):` SAN-only cert unit test

Extend the existing `make_cert_with_cn` test helper (grep for it in put_path tests) → `make_cert_with_san(dns_names: &[&str], cn: Option<&str>)`. Use [`rcgen`](https://docs.rs/rcgen) (already a dev-dep or add it):

```rust
// r[verify store.hmac.san-bypass]
#[test]
fn san_only_cert_no_cn_bypasses() {
    // Cert with empty CN, SAN DNSName = "rio-gateway"
    let cert = make_cert_with_san(&["rio-gateway"], None);
    assert!(cert_identity_in_allowlist(&cert.der, &["rio-gateway".into()]));
}

#[test]
fn san_mismatch_does_not_bypass() {
    let cert = make_cert_with_san(&["rio-worker"], None);
    assert!(!cert_identity_in_allowlist(&cert.der, &["rio-gateway".into()]));
}

#[test]
fn cn_still_bypasses_backward_compat() {
    let cert = make_cert_with_cn("rio-gateway");  // existing helper
    assert!(cert_identity_in_allowlist(&cert.der, &["rio-gateway".into()]));
}

#[test]
fn custom_allowlist_works() {
    let cert = make_cert_with_cn("my-custom-gateway");
    assert!(cert_identity_in_allowlist(&cert.der, &["my-custom-gateway".into()]));
    assert!(!cert_identity_in_allowlist(&cert.der, &["rio-gateway".into()]));
}
```

### T4 — `docs(store):` spec ¶

MODIFY [`docs/src/components/store.md`](../../docs/src/components/store.md) — add `r[store.hmac.san-bypass]` after the existing `:98` authorization paragraph (near the `## Key Operations` section):

```markdown
r[store.hmac.san-bypass]
The HMAC bypass check accepts a client certificate whose CN **or** any SAN `DNSName` entry matches the configured allowlist (`hmac_bypass_cns`, default `["rio-gateway"]`). SAN matching enables cert-manager-issued certificates that place identity in SAN extensions rather than CN. The check is CN-first, SAN-second; either match grants bypass.
```

## Exit criteria

- `/nbr .#ci` green
- `tracey query rule store.hmac.san-bypass` shows spec + impl + verify
- `san_only_cert_no_cn_bypasses` test passes (SAN-only cert with empty CN grants bypass)
- Existing HMAC-enforced tests still pass (worker cert without allowlisted CN/SAN still requires HMAC)

## Tracey

Adds new marker to component specs:
- `r[store.hmac.san-bypass]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) (see ## Spec additions below)

## Spec additions

**`r[store.hmac.san-bypass]`** — placed in `docs/src/components/store.md` after the existing `:98` HMAC authorization paragraph, standalone paragraph, blank line before, col 0:

```
r[store.hmac.san-bypass]
The HMAC bypass check accepts a client certificate whose CN **or** any SAN `DNSName` entry matches the configured allowlist (`hmac_bypass_cns`, default `["rio-gateway"]`). SAN matching enables cert-manager-issued certificates that place identity in SAN extensions rather than CN. The check is CN-first, SAN-second; either match grants bypass.
```

## Files

```json files
[
  {"path": "rio-store/src/config.rs", "action": "MODIFY", "note": "T1: hmac_bypass_cns: Vec<String> (default [\"rio-gateway\"])"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T2+T3: cert_identity_in_allowlist (CN OR SAN DNSName); r[impl]+r[verify] tests"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T4: r[store.hmac.san-bypass] spec ¶ after :98"}
]
```

```
rio-store/src/
├── config.rs            # T1: hmac_bypass_cns
└── grpc/put_path.rs     # T2+T3: r[impl]+r[verify]
docs/src/components/store.md  # T4: spec ¶
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Wave-1 frontier. put_path.rs not hot — no serialization needed."}
```

**Depends on:** none. `x509-parser` is already a dependency (grep `Cargo.lock` to confirm; if not, `cargo add x509-parser -p rio-store`).
**Conflicts with:** none. `put_path.rs` and `config.rs` are not in the hot-file matrix.
