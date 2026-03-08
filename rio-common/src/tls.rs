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
//! [`load_client_tls`]'s `server_name` MUST match a SAN in the server's
//! certificate (tonic's `ClientTlsConfig::domain_name` sets the SNI and
//! rustls verifies the cert against it). cert-manager's `dnsNames` field
//! populates SANs — include BOTH the short name (e.g., `rio-scheduler`)
//! and the FQDN (e.g., `rio-scheduler.rio-system.svc.cluster.local`) so
//! clients can use either addressing style.
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

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("TLS file I/O ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Partial config is never valid. Better to fail startup than to
    /// silently run half-TLS (client presents a cert, server doesn't
    /// verify — false sense of security).
    #[error(
        "incomplete TLS config: cert_path={cert}, key_path={key}, ca_path={ca} \
         — all three must be set or all unset"
    )]
    Incomplete { cert: bool, key: bool, ca: bool },
}

/// Read a PEM file, returning the UTF-8 contents.
///
/// PEM is always ASCII (base64 + header lines) so UTF-8 is safe. We
/// don't parse the PEM structure — tonic/rustls do that. This just
/// surfaces I/O errors with the path attached (the default io::Error
/// doesn't say WHICH file, which is infuriating with three paths).
fn read_pem(path: &PathBuf) -> Result<String, TlsError> {
    std::fs::read_to_string(path).map_err(|source| TlsError::Io {
        path: path.clone(),
        source,
    })
}

/// Build a server-side TLS config.
///
/// Returns `Ok(None)` if TLS is entirely unconfigured (all paths
/// `None`) — plaintext mode, not an error. Returns `Err` if partially
/// configured OR if a file can't be read. Returns `Ok(Some(...))` on
/// full valid config.
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
/// `server_name` is the expected SNI/SAN — must match a `dnsNames`
/// entry in the server's certificate. Use the SHORT K8s Service name
/// (e.g., `"rio-scheduler"`) since that's how `scheduler_addr` is
/// typically configured and cert-manager includes it in SANs.
///
/// Like [`load_server_tls`], returns `Ok(None)` for unconfigured,
/// `Err` for partial, `Ok(Some)` for valid.
pub fn load_client_tls(
    cfg: &TlsConfig,
    server_name: &str,
) -> Result<Option<ClientTlsConfig>, TlsError> {
    match (&cfg.cert_path, &cfg.key_path, &cfg.ca_path) {
        (None, None, None) => Ok(None),
        (Some(cert), Some(key), Some(ca)) => {
            let identity = Identity::from_pem(read_pem(cert)?, read_pem(key)?);
            let ca = Certificate::from_pem(read_pem(ca)?);
            Ok(Some(
                ClientTlsConfig::new()
                    .identity(identity)
                    .ca_certificate(ca)
                    .domain_name(server_name),
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

    // A minimal self-signed cert + key, generated once via rcgen and
    // frozen here. We don't VERIFY the cert (rustls does that at
    // handshake time, which we can't easily trigger in a unit test) —
    // we're testing the LOADING logic: does partial-config fail, does
    // missing-file fail, does full-config succeed, does empty-config
    // return None.
    //
    // The actual handshake verification is in tests/tls_integration.rs
    // (with a real tonic server + client).
    const DUMMY_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
        MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAdummy\n\
        -----END CERTIFICATE-----\n";

    #[test]
    fn unconfigured_returns_none() {
        let cfg = TlsConfig::default();
        assert!(!cfg.is_configured());
        assert!(load_server_tls(&cfg).unwrap().is_none());
        assert!(load_client_tls(&cfg, "rio-scheduler").unwrap().is_none());
    }

    #[test]
    fn partial_config_is_error() {
        // cert + key but no ca → Err, not None. This catches the
        // "I configured TLS but forgot the CA mount" case with a
        // startup error instead of a handshake failure at runtime.
        let cert = write_tmp(DUMMY_PEM);
        let key = write_tmp(DUMMY_PEM);
        let cfg = TlsConfig {
            cert_path: Some(cert.path().to_path_buf()),
            key_path: Some(key.path().to_path_buf()),
            ca_path: None,
        };
        assert!(cfg.is_configured());
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(matches!(
            err,
            TlsError::Incomplete {
                cert: true,
                key: true,
                ca: false
            }
        ));
        // Same for client.
        let err = load_client_tls(&cfg, "x").unwrap_err();
        assert!(matches!(err, TlsError::Incomplete { .. }));
    }

    #[test]
    fn only_ca_is_also_incomplete() {
        // Just the CA without our own identity → can't do mTLS (we'd
        // have nothing to present). Still partial.
        let ca = write_tmp(DUMMY_PEM);
        let cfg = TlsConfig {
            cert_path: None,
            key_path: None,
            ca_path: Some(ca.path().to_path_buf()),
        };
        let err = load_server_tls(&cfg).unwrap_err();
        assert!(matches!(
            err,
            TlsError::Incomplete {
                cert: false,
                key: false,
                ca: true
            }
        ));
    }

    #[test]
    fn missing_file_is_io_error_with_path() {
        // Full config but a path points nowhere → Err(Io), and the
        // message includes the path so the operator knows WHICH file.
        let exists = write_tmp(DUMMY_PEM);
        let cfg = TlsConfig {
            cert_path: Some(exists.path().to_path_buf()),
            key_path: Some("/nonexistent/key.pem".into()),
            ca_path: Some(exists.path().to_path_buf()),
        };
        let err = load_server_tls(&cfg).unwrap_err();
        match err {
            TlsError::Io { path, .. } => {
                assert_eq!(path, PathBuf::from("/nonexistent/key.pem"));
            }
            other => panic!("expected Io error with path, got {other:?}"),
        }
    }

    #[test]
    fn full_config_loads_some() {
        // All three paths exist and are readable → Ok(Some). We don't
        // validate the PEM content here (that's rustls's job at
        // handshake) — just the loading/config-assembly logic.
        let cert = write_tmp(DUMMY_PEM);
        let key = write_tmp(DUMMY_PEM);
        let ca = write_tmp(DUMMY_PEM);
        let cfg = TlsConfig {
            cert_path: Some(cert.path().to_path_buf()),
            key_path: Some(key.path().to_path_buf()),
            ca_path: Some(ca.path().to_path_buf()),
        };
        assert!(cfg.is_configured());
        let server = load_server_tls(&cfg).unwrap();
        assert!(server.is_some(), "full config → Some(ServerTlsConfig)");
        let client = load_client_tls(&cfg, "rio-store").unwrap();
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
}
