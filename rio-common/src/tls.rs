//! TLS configuration for mTLS between rio components.
//!
//! Each component's Config struct embeds [`TlsConfig`]. When all three
//! paths are set (cert, key, ca), the component speaks mTLS: presents
//! its own cert to peers, and requires peers to present a cert signed
//! by the CA. When unset, plaintext — dev mode, VM tests, backward
//! compatibility with pre-Phase-3b deployments.
//!
//! # Certificate provisioning
//!
//! In K8s, cert-manager issues per-component Certificates from a shared
//! CA Issuer. The `ca.crt` in each Secret is the same (the CA); `tls.crt`
//! + `tls.key` are per-component. Mounted at `/etc/rio/tls/`.
//!
//! SNI / SAN verification: [`load_client_tls`] does NOT set
//! `ClientTlsConfig::domain_name` — tonic derives the SNI hostname
//! from the connect URL's authority. So connecting to
//! `rio-scheduler:9001` sends SNI=`rio-scheduler`; connecting to
//! `rio-store:9002` sends SNI=`rio-store`. rustls verifies the
//! server cert has a SAN matching the SNI. cert-manager's `dnsNames`
//! populates SANs — include the SHORT name (how clients address)
//! and the FQDN (for fully-qualified addressing).
//!
//! `ClientTlsConfig::domain_name` must NOT be set: the gateway connects
//! to BOTH scheduler and store with a SINGLE config; a fixed
//! `domain_name="rio-scheduler"` would fail the store handshake (store
//! cert has SAN `rio-store`). Letting tonic derive SNI from the URL is
//! correct per-connection.
//!
//! # Health probes
//!
//! K8s gRPC health probes speak PLAINTEXT. mTLS on the main port means
//! the probe fails. The scheduler + store each spawn a second, plaintext
//! server on `health_addr` serving ONLY `grpc.health.v1.Health`, sharing
//! the SAME `HealthReporter` with the main server — leadership toggles
//! propagate to both. Without the shared reporter, the plaintext port
//! would always report SERVING → standby scheduler passes readiness →
//! K8s load-balances to a non-leader → cluster split.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

/// Typed errors for TLS config loading. Every variant carries the
/// offending path so an operator can grep-and-fix without guessing
/// which of the three files is bad. Callers in `anyhow` contexts get
/// these via `?` (auto `From<TlsError> for anyhow::Error`).
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// Some-but-not-all paths set. Partial config is operator mistake,
    /// not a valid "half-TLS" mode — better to fail startup than to
    /// silently run encrypted-but-unauthenticated.
    #[error(
        "incomplete TLS config: cert_path={cert}, key_path={key}, ca_path={ca} \
         — all three must be set or all unset"
    )]
    Incomplete { cert: bool, key: bool, ca: bool },

    /// `read_to_string` failed. The default `io::Error` doesn't say
    /// WHICH file, which is infuriating with three paths.
    #[error("TLS file I/O ({path}): {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// File readable but no `-----BEGIN` marker — empty Secret mount,
    /// DER-format binary, wrong file pointed at.
    #[error("TLS file {path} doesn't contain a PEM block (no '-----BEGIN' marker)")]
    NoPemMarker { path: PathBuf },

    /// File has a `-----BEGIN` header but the body fails PEM parse —
    /// corrupt base64, truncated before `-----END`, mismatched section
    /// type. tonic's `Identity`/`Certificate` are byte holders; they
    /// don't validate until the FIRST handshake, which surfaces a
    /// path-less rustls error. This catches it at startup.
    #[error("TLS file {path} has a PEM header but the body is malformed: {detail}")]
    MalformedPem { path: PathBuf, detail: String },
}

/// TLS file paths. Nested in each binary's `Config` as `tls: TlsConfig`.
///
/// Env vars use the `RIO_TLS__*` prefix (double underscore = figment
/// nesting): `RIO_TLS__CERT_PATH=/etc/rio/tls/tls.crt` etc.
///
/// All paths Optional: `None` = TLS disabled. The combination "some set,
/// some not" is caught at [`load_server_tls`] / [`load_client_tls`] time
/// with a clear error — partial config is operator mistake, not a valid
/// "half-TLS" mode.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Our own certificate (PEM). Server: presented on accept. Client:
    /// presented for mTLS handshake.
    pub cert_path: Option<PathBuf>,
    /// Our own private key (PEM, PKCS8). Must pair with `cert_path`.
    /// cert-manager's `encoding: PKCS8` emits this format. PKCS1 (the
    /// `BEGIN RSA PRIVATE KEY` header) also works for RSA but PKCS8
    /// is required for EC keys (which cert-manager defaults to).
    pub key_path: Option<PathBuf>,
    /// CA bundle (PEM, may contain multiple certs). Server: required
    /// client certs must be signed by one of these. Client: the server's
    /// cert must chain to one of these.
    pub ca_path: Option<PathBuf>,
}

impl TlsConfig {
    /// True if any path is set. Used by callers to log "TLS enabled".
    /// Doesn't guarantee the paths are VALID — load_* does that.
    pub fn is_configured(&self) -> bool {
        self.cert_path.is_some() || self.key_path.is_some() || self.ca_path.is_some()
    }
}

/// Read a PEM file, returning the UTF-8 contents.
///
/// PEM is always ASCII (base64 + header lines) so UTF-8 is safe.
/// Validates structure in two passes: a cheap `-----BEGIN` substring
/// check (catches empty/DER/wrong-file) then a full section parse via
/// [`validate_pem_sections`] (catches valid-header-garbage-body). Both
/// attach the path; rustls's own handshake-time error doesn't.
fn read_pem(path: &PathBuf) -> Result<String, TlsError> {
    let contents = std::fs::read_to_string(path).map_err(|source| TlsError::Io {
        path: path.clone(),
        source,
    })?;
    if !contents.contains("-----BEGIN") {
        return Err(TlsError::NoPemMarker { path: path.clone() });
    }
    validate_pem_sections(&contents).map_err(|detail| TlsError::MalformedPem {
        path: path.clone(),
        detail,
    })?;
    Ok(contents)
}

/// Structurally validate every PEM section: each `-----BEGIN <label>-----`
/// has a matching `-----END <label>-----` and the body between them is
/// valid base64. tonic's `Identity`/`Certificate` are byte holders that
/// only fail at HANDSHAKE time with a path-less rustls error; this
/// catches corrupt/truncated mounts at startup. We don't decode the DER
/// (rustls does that) — just confirm the PEM envelope is well-formed.
fn validate_pem_sections(contents: &str) -> Result<(), String> {
    use base64::Engine as _;
    let mut lines = contents.lines().peekable();
    let mut sections = 0usize;
    while let Some(line) = lines.next() {
        let Some(label) = line
            .trim()
            .strip_prefix("-----BEGIN ")
            .and_then(|s| s.strip_suffix("-----"))
        else {
            continue;
        };
        let end = format!("-----END {label}-----");
        let mut body = String::new();
        loop {
            match lines.next() {
                None => {
                    return Err(format!(
                        "section '{label}' truncated before '-----END {label}-----'"
                    ));
                }
                Some(l) if l.trim() == end => break,
                Some(l) => body.push_str(l.trim()),
            }
        }
        base64::engine::general_purpose::STANDARD
            .decode(&body)
            .map_err(|e| format!("section '{label}' body is not valid base64: {e}"))?;
        sections += 1;
    }
    if sections == 0 {
        return Err("no complete PEM section found".into());
    }
    Ok(())
}

/// Build a server-side TLS config.
///
/// Returns `Ok(None)` if TLS is entirely unconfigured (all paths
/// `None`) — plaintext mode, not an error. Returns `Err` if partially
/// configured OR if a file can't be read/parsed. Returns `Ok(Some(...))`
/// on full valid config.
///
/// The `ServerTlsConfig` has `.client_ca_root(ca)` set, which tells
/// tonic/rustls to REQUIRE a client cert and verify it against the CA.
/// Without that call, the server would accept any client (TLS for
/// encryption only, not authentication) — not the mTLS we want.
pub fn load_server_tls(cfg: &TlsConfig) -> Result<Option<ServerTlsConfig>, TlsError> {
    match (&cfg.cert_path, &cfg.key_path, &cfg.ca_path) {
        (None, None, None) => Ok(None),
        (Some(cert), Some(key), Some(ca)) => {
            let identity = Identity::from_pem(read_pem(cert)?, read_pem(key)?);
            let ca = Certificate::from_pem(read_pem(ca)?);
            Ok(Some(
                ServerTlsConfig::new().identity(identity).client_ca_root(ca),
            ))
        }
        (c, k, a) => Err(TlsError::Incomplete {
            cert: c.is_some(),
            key: k.is_some(),
            ca: a.is_some(),
        }),
    }
}

/// Build a client-side TLS config.
///
/// Does NOT set `domain_name` — tonic derives SNI from the connect
/// URL's host. Each connection verifies against THAT host's SAN,
/// which is correct when one client connects to multiple servers
/// (gateway → scheduler AND store). Server certs must have SANs
/// matching how clients address them (cert-manager `dnsNames`).
///
/// Like [`load_server_tls`], returns `Ok(None)` for unconfigured,
/// `Err` for partial, `Ok(Some)` for valid.
pub fn load_client_tls(cfg: &TlsConfig) -> Result<Option<ClientTlsConfig>, TlsError> {
    match (&cfg.cert_path, &cfg.key_path, &cfg.ca_path) {
        (None, None, None) => Ok(None),
        (Some(cert), Some(key), Some(ca)) => {
            let identity = Identity::from_pem(read_pem(cert)?, read_pem(key)?);
            let ca = Certificate::from_pem(read_pem(ca)?);
            Ok(Some(
                ClientTlsConfig::new().identity(identity).ca_certificate(ca),
            ))
        }
        (c, k, a) => Err(TlsError::Incomplete {
            cert: c.is_some(),
            key: k.is_some(),
            ca: a.is_some(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    /// Generate a real self-signed cert + key PEM pair. The deep PEM
    /// parse in `read_pem` rejects fake `-----BEGIN…dummy…-----END`
    /// fixtures, so unit tests need structurally-valid PEM. rcgen is
    /// already a dev-dep for the integration suite.
    fn gen_pems() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["test".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn unconfigured_returns_none() {
        let cfg = TlsConfig::default();
        assert!(!cfg.is_configured());
        assert!(load_server_tls(&cfg).unwrap().is_none());
        assert!(load_client_tls(&cfg).unwrap().is_none());
    }

    #[test]
    fn partial_config_is_error() {
        // cert + key but no ca → Err, not None. This catches the
        // "I configured TLS but forgot the CA mount" case with a
        // startup error instead of a handshake failure at runtime.
        let (cert_pem, key_pem) = gen_pems();
        let cert = write_tmp(&cert_pem);
        let key = write_tmp(&key_pem);
        let cfg = TlsConfig {
            cert_path: Some(cert.path().to_path_buf()),
            key_path: Some(key.path().to_path_buf()),
            ca_path: None,
        };
        assert!(cfg.is_configured());
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(
            matches!(
                err,
                TlsError::Incomplete {
                    cert: true,
                    key: true,
                    ca: false
                }
            ),
            "got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("incomplete TLS config")
                && msg.contains("cert_path=true")
                && msg.contains("ca_path=false"),
            "got: {msg}"
        );
        // Same for client.
        assert!(matches!(
            load_client_tls(&cfg).unwrap_err(),
            TlsError::Incomplete { .. }
        ));
    }

    #[test]
    fn only_ca_is_also_incomplete() {
        // Just the CA without our own identity → can't do mTLS (we'd
        // have nothing to present). Still partial.
        let (cert_pem, _) = gen_pems();
        let ca = write_tmp(&cert_pem);
        let cfg = TlsConfig {
            cert_path: None,
            key_path: None,
            ca_path: Some(ca.path().to_path_buf()),
        };
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(
            matches!(
                err,
                TlsError::Incomplete {
                    cert: false,
                    key: false,
                    ca: true
                }
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn missing_file_is_io_error_with_path() {
        // Full config but a path points nowhere → Err(Io), and the
        // message includes the path so the operator knows WHICH file.
        let (cert_pem, _) = gen_pems();
        let exists = write_tmp(&cert_pem);
        let cfg = TlsConfig {
            cert_path: Some(exists.path().to_path_buf()),
            key_path: Some("/nonexistent/key.pem".into()),
            ca_path: Some(exists.path().to_path_buf()),
        };
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(matches!(err, TlsError::Io { .. }), "got: {err:?}");
        assert!(
            err.to_string().contains("/nonexistent/key.pem"),
            "error must name the bad path, got: {err}"
        );
    }

    #[test]
    fn full_config_loads_some() {
        // All three paths exist and parse → Ok(Some). Handshake
        // verification is in tests/tls_integration.rs; this is the
        // loading/config-assembly logic only.
        let (cert_pem, key_pem) = gen_pems();
        let cert = write_tmp(&cert_pem);
        let key = write_tmp(&key_pem);
        let ca = write_tmp(&cert_pem);
        let cfg = TlsConfig {
            cert_path: Some(cert.path().to_path_buf()),
            key_path: Some(key.path().to_path_buf()),
            ca_path: Some(ca.path().to_path_buf()),
        };
        assert!(cfg.is_configured());
        let server = load_server_tls(&cfg).unwrap();
        assert!(server.is_some(), "full config → Some(ServerTlsConfig)");
        let client = load_client_tls(&cfg).unwrap();
        assert!(client.is_some(), "full config → Some(ClientTlsConfig)");
    }

    /// is_configured is "any path set", not "all paths set". Guards
    /// against a caller checking is_configured and then assuming
    /// load_* will succeed — it won't for partial config.
    #[test]
    fn is_configured_true_for_partial() {
        let cfg = TlsConfig {
            cert_path: Some("/x".into()),
            ..Default::default()
        };
        assert!(
            cfg.is_configured(),
            "is_configured should be true for any path set — \
             the loader will catch partial config with an Err"
        );
    }

    /// Empty file → NoPemMarker with path. Catches empty mounted
    /// Secret, DER-format cert, wrong file.
    #[test]
    fn empty_pem_is_malformed() {
        let empty = write_tmp("");
        let cfg = TlsConfig {
            cert_path: Some(empty.path().to_path_buf()),
            key_path: Some(empty.path().to_path_buf()),
            ca_path: Some(empty.path().to_path_buf()),
        };
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(matches!(err, TlsError::NoPemMarker { .. }), "got: {err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("doesn't contain a PEM block")
                && msg.contains(&empty.path().display().to_string()),
            "got: {msg}"
        );
    }

    #[test]
    fn garbage_pem_is_malformed() {
        // Content without -----BEGIN → NoPemMarker, not left for
        // rustls to fail at handshake with a cryptic error.
        let garbage = write_tmp("this is definitely not a PEM file");
        let (cert_pem, _) = gen_pems();
        let valid = write_tmp(&cert_pem);
        let cfg = TlsConfig {
            cert_path: Some(valid.path().to_path_buf()),
            key_path: Some(garbage.path().to_path_buf()),
            ca_path: Some(valid.path().to_path_buf()),
        };
        let err = load_client_tls(&cfg).unwrap_err();
        assert!(matches!(err, TlsError::NoPemMarker { .. }), "got: {err:?}");
    }

    /// Valid `-----BEGIN` header but garbage body → MalformedPem.
    /// The substring check passes; only `validate_pem_sections` catches
    /// this. Without it, the error surfaces at the first handshake as a
    /// path-less rustls message.
    #[test]
    fn corrupt_pem_body_is_malformed() {
        let corrupt = write_tmp(
            "-----BEGIN CERTIFICATE-----\n\
             this is not base64 at all !!!\n\
             -----END CERTIFICATE-----\n",
        );
        let (cert_pem, key_pem) = gen_pems();
        let valid_cert = write_tmp(&cert_pem);
        let valid_key = write_tmp(&key_pem);
        let cfg = TlsConfig {
            cert_path: Some(valid_cert.path().to_path_buf()),
            key_path: Some(valid_key.path().to_path_buf()),
            ca_path: Some(corrupt.path().to_path_buf()),
        };
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(
            matches!(err, TlsError::MalformedPem { .. }),
            "expected MalformedPem, got: {err:?}"
        );
        assert!(
            err.to_string()
                .contains(&corrupt.path().display().to_string()),
            "error must name the corrupt file, got: {err}"
        );
    }

    /// Header present but truncated before `-----END` → MalformedPem
    /// via `validate_pem_sections`' explicit truncated-section error.
    #[test]
    fn truncated_pem_is_malformed() {
        let truncated = write_tmp("-----BEGIN CERTIFICATE-----\nMIIBIjAN\n");
        let err = read_pem(&truncated.path().to_path_buf()).unwrap_err();
        assert!(
            matches!(err, TlsError::MalformedPem { .. }),
            "expected MalformedPem, got: {err:?}"
        );
    }
}
