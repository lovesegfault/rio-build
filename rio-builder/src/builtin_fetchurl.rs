//! Native `builtin:fetchurl` — the in-sandbox half.
//!
//! A `builtin:fetchurl` derivation is NOT executed by running its
//! (nonexistent) builder script. Under the daemon it ran as C++ code
//! inside the daemon's forked sandbox child; under the native executor
//! the rio-builder binary re-execs **itself** inside the rio-exec
//! sandbox as `rio-builder __builtin-fetchurl` (see
//! `executor::glue::builtin` for the request construction). That keeps
//! the attacker-facing surface (HTTP parsing, xz decompression, NAR
//! restore of remote bytes) at the build uid inside the chroot, with
//! the per-build cgroup/timeout/cancel machinery applying unchanged.
//!
//! Parameters arrive as `RIO_FETCHURL_*` environment variables (set by
//! the glue from the derivation's env + worker config), NOT argv — the
//! values are tenant-controlled and env vars avoid any argv-quoting
//! ambiguity.
//!
//! Contract notes:
//! - **No content-hash verification happens here.** `verify_fod_hashes`
//!   in the result glue is the sole verifier (fail-closed); this
//!   process only has to produce *bytes at the output path*.
//! - Mirrors are tried before the origin URL, each candidate with
//!   bounded retries (parity with Nix's curl layer: 5 attempts).
//! - `unpack` treats the payload as xz-compressed **iff the original
//!   `url` attribute ends in `.xz`** (a hashed-mirror URL has no
//!   meaningful suffix), and the decompressed stream is a NAR that is
//!   restored at the output path. The restored size is capped to keep
//!   a hostile stream from filling the disk past any plausible source
//!   archive.
//! - `s3://` URLs are not supported (documented divergence from Nix).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail};
use futures_util::StreamExt as _;
use rio_common::backoff::{Backoff, Jitter};

/// Env-var names shared between the glue (writer) and this subcommand
/// (reader). Keep in one place so they cannot drift.
pub mod env_vars {
    /// Origin URL (the derivation's `url` attribute). Required.
    pub const URL: &str = "RIO_FETCHURL_URL";
    /// Absolute in-sandbox output path. Required.
    pub const OUTPUT: &str = "RIO_FETCHURL_OUTPUT";
    /// "1" → decompress (xz iff URL ends in .xz) and restore as a NAR.
    pub const UNPACK: &str = "RIO_FETCHURL_UNPACK";
    /// "1" → chmod the (single-file) output 0755.
    pub const EXECUTABLE: &str = "RIO_FETCHURL_EXECUTABLE";
    /// Space-separated hashed-mirror base URLs (may be empty/unset).
    pub const MIRRORS: &str = "RIO_FETCHURL_MIRRORS";
    /// Lower-case hash algorithm (`sha256`, …) for mirror URL
    /// construction only.
    pub const HASH_ALGO: &str = "RIO_FETCHURL_HASH_ALGO";
    /// Base16 content hash for mirror URL construction only.
    pub const HASH_B16: &str = "RIO_FETCHURL_HASH_B16";
    /// Optional path (inside the sandbox) of a netrc file.
    pub const NETRC: &str = "RIO_FETCHURL_NETRC";
}

/// Attempts per candidate URL. Nix's curl layer retries 5× with
/// backoff; a single-attempt GET would be a real fetcher-fleet
/// robustness regression even though the scheduler retries whole
/// builds.
const ATTEMPTS_PER_URL: u32 = 5;

/// Retry backoff: 1s, 2s, 4s, 8s (capped). Jitter-free — inside the
/// sandbox there is exactly one fetch in flight, there is no thundering
/// herd to spread.
const RETRY_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(8),
    jitter: Jitter::None,
};

/// Cap on the bytes restored from an unpacked (xz→NAR) payload.
/// A decompression bomb otherwise turns a few KiB of download into an
/// arbitrarily large write inside the build scratch. 64 GiB is far
/// beyond any plausible single fetched source archive while still
/// bounding the damage to roughly the disk headroom a large build
/// already needs. (Plain, non-unpack downloads are bounded by the HTTP
/// body itself — the server cannot amplify.)
const MAX_RESTORED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// In-sandbox path of the operator-configured CA bundle.
///
/// This is the single source of truth for the writer/reader contract:
/// the request glue mounts the host bundle here (the generic FOD path
/// in `executor::glue::prepare_request` and the builtin path in
/// `executor::glue::builtin::prepare_fetchurl` — both read-only,
/// `optional: true`, only when the worker has a bundle configured),
/// and the in-sandbox fetch half reads it from here. Operator-facing
/// prose (`Config::ca_bundle`, `default_ca_bundle`) mirrors the value
/// descriptively; any code that needs the path must use this constant.
///
/// The fetch must keep working without it: fetcher pods have no system
/// trust store of their own, and plain-HTTP origins and hashed mirrors
/// are valid configurations — the FOD hash gate, not TLS, is the
/// content-integrity boundary. TLS fetches simply require the bundle
/// to be present.
pub const SANDBOX_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Everything the subcommand needs, parsed from `RIO_FETCHURL_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchurlParams {
    pub url: String,
    pub output: PathBuf,
    pub unpack: bool,
    pub executable: bool,
    pub mirrors: Vec<String>,
    pub hash_algo: String,
    pub hash_b16: String,
    pub netrc: Option<PathBuf>,
}

impl FetchurlParams {
    /// Parse from the process environment. Missing required vars are
    /// hard errors — the glue always sets them; their absence means
    /// this binary was invoked outside its contract.
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let url = var(env_vars::URL).context("RIO_FETCHURL_URL is required")?;
        let output = var(env_vars::OUTPUT).context("RIO_FETCHURL_OUTPUT is required")?;
        Ok(Self {
            url,
            output: PathBuf::from(output),
            unpack: var(env_vars::UNPACK).as_deref() == Some("1"),
            executable: var(env_vars::EXECUTABLE).as_deref() == Some("1"),
            mirrors: var(env_vars::MIRRORS)
                .map(|m| m.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            hash_algo: var(env_vars::HASH_ALGO).unwrap_or_default(),
            hash_b16: var(env_vars::HASH_B16).unwrap_or_default(),
            netrc: var(env_vars::NETRC).map(PathBuf::from),
        })
    }

    /// Candidate URLs in fetch order: each hashed mirror as
    /// `<mirror>/<algo>/<base16-hash>`, then the origin URL. Mirrors
    /// are skipped when either hash component is missing (the glue
    /// only passes them for flat-mode FODs).
    // r[impl fetcher.mirrors.hashed+2]
    pub fn candidate_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        if !self.hash_algo.is_empty() && !self.hash_b16.is_empty() {
            for mirror in &self.mirrors {
                let base = mirror.trim_end_matches('/');
                urls.push(format!("{base}/{}/{}", self.hash_algo, self.hash_b16));
            }
        }
        urls.push(self.url.clone());
        urls
    }

    /// Whether the payload should be xz-decoded before NAR restore.
    /// Decided by the *origin* URL's suffix, never the mirror URL.
    pub fn is_xz(&self) -> bool {
        self.url.ends_with(".xz")
    }
}

/// Subcommand entry point. Never returns control to the normal builder
/// path — the caller passes the returned code to `std::process::exit`.
/// 0 on success, 1 on failure, after writing a human-readable error
/// line to stderr (which lands in the build log via the sandbox pty).
pub async fn run() -> i32 {
    let params = match FetchurlParams::from_env() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("builtin:fetchurl: invalid invocation: {e:#}");
            return 1;
        }
    };
    match fetch(&params).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("builtin:fetchurl: {e:#}");
            1
        }
    }
}

/// Typed marker for HTTP failures that will not change on retry
/// (4xx other than 408/429): the retry loop skips the remaining
/// attempts for that URL. Carried as an `anyhow` error so the context
/// chains in the build log stay intact; the retry loop detects it by
/// downcast instead of string matching.
#[derive(Debug, thiserror::Error)]
#[error("HTTP {status} from {url}")]
struct PermanentHttpError {
    status: reqwest::StatusCode,
    url: String,
}

/// Fetch `params.url` (or a mirror) to `params.output`.
async fn fetch(params: &FetchurlParams) -> anyhow::Result<()> {
    if params.url.starts_with("s3://") {
        bail!(
            "s3:// URLs are not supported by the native builtin:fetchurl \
             (use an https:// endpoint URL instead)"
        );
    }

    let (client, tls_roots_available) = build_client(Path::new(SANDBOX_CA_BUNDLE))?;
    let candidates = params.candidate_urls();
    let mut last_err: Option<anyhow::Error> = None;

    for url in &candidates {
        for attempt in 0..ATTEMPTS_PER_URL {
            if attempt > 0 {
                tokio::time::sleep(RETRY_BACKOFF.duration(attempt - 1)).await;
            }
            eprintln!(
                "builtin:fetchurl: fetching {url} (attempt {}/{ATTEMPTS_PER_URL})",
                attempt + 1
            );
            match try_fetch_one(&client, url, params, tls_roots_available).await {
                Ok(()) => {
                    eprintln!("builtin:fetchurl: fetched {url}");
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("builtin:fetchurl: attempt failed: {e:#}");
                    // Permanent (non-retryable) HTTP statuses skip the
                    // remaining attempts for THIS url and move on to the
                    // next candidate immediately.
                    let permanent = e.downcast_ref::<PermanentHttpError>().is_some();
                    last_err = Some(e);
                    if permanent {
                        break;
                    }
                }
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no candidate URLs (empty mirror list and URL)")))
    .with_context(|| format!("all candidates failed (tried {})", candidates.join(", ")))
}

/// Build the HTTP client. rustls; netrc credentials (if provided) are
/// applied per-request in [`try_fetch_one`] because reqwest has no
/// built-in netrc support in our feature set.
///
/// TLS roots come exclusively from the sandbox CA bundle at
/// `ca_bundle` (mounted by the glue when the operator configured one).
/// reqwest's default behavior of loading the *system* trust store is
/// disabled: fetcher pods have no system store, and treating that as a
/// construction-time error broke every fetch — including plain-HTTP
/// ones that never needed TLS in the first place. Returns the client
/// plus whether any roots were loaded, so HTTPS attempts without roots
/// can fail with an actionable message instead of a bare TLS error.
fn build_client(ca_bundle: &Path) -> anyhow::Result<(reqwest::Client, bool)> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("rio-build/", env!("CARGO_PKG_VERSION")))
        // Redirects are common for source tarballs (GitHub → S3).
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(30))
        // A server that stalls mid-body should fail the attempt (and
        // fall through to the next mirror/origin) instead of hanging
        // until the sandbox's max-silent kill. Idle-read bound, not a
        // total-transfer bound — multi-GB sources stay fine as long as
        // bytes keep flowing.
        .read_timeout(Duration::from_secs(300));

    let tls_roots_available = match std::fs::read(ca_bundle) {
        Ok(pem) => {
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .with_context(|| format!("parsing CA bundle {}", ca_bundle.display()))?;
            if certs.is_empty() {
                bail!("CA bundle {} contains no certificates", ca_bundle.display());
            }
            // Trust exactly the operator-provided bundle; never consult
            // a (nonexistent) platform trust store.
            builder = builder.tls_certs_only(certs);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            builder = builder.use_preconfigured_tls(no_trust_tls_config());
            false
        }
        Err(e) => {
            return Err(e).with_context(|| format!("reading CA bundle {}", ca_bundle.display()));
        }
    };

    let client = builder.build().context("constructing HTTP client")?;
    Ok((client, tls_roots_available))
}

/// A rustls config whose certificate verifier rejects every server
/// certificate, used when no CA bundle is mounted in the sandbox. This
/// keeps client construction (and therefore plain-HTTP fetches) working
/// on workers with no trust store at all, while an https:// URL fails
/// verification with a message that names the fix instead of reqwest's
/// platform-store "No CA certificates were loaded from the system"
/// construction error. Certificate verification is never disabled — it
/// simply cannot succeed without roots.
fn no_trust_tls_config() -> rustls::ClientConfig {
    #[derive(Debug)]
    struct NoTrustedRoots;

    impl rustls::client::danger::ServerCertVerifier for NoTrustedRoots {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Err(rustls::Error::General(format!(
                "no CA roots are configured in the sandbox (set RIO_CA_BUNDLE on \
                 the worker so a bundle is mounted at {SANDBOX_CA_BUNDLE})"
            )))
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            // Unreachable in practice: certificate verification above
            // fails before any signature check.
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("aws-lc-rs supports the default TLS protocol versions")
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(NoTrustedRoots))
    .with_no_client_auth()
}

/// One GET attempt: stream the body to a temp file next to the output,
/// then finalize (rename, or xz→NAR restore) so a failed attempt never
/// leaves a partial output behind.
async fn try_fetch_one(
    client: &reqwest::Client,
    url: &str,
    params: &FetchurlParams,
    tls_roots_available: bool,
) -> anyhow::Result<()> {
    let parent = params
        .output
        .parent()
        .context("output path has no parent directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;

    let mut req = client.get(url);
    if let Some((user, pass)) = netrc_credentials(params.netrc.as_deref(), url)? {
        req = req.basic_auth(user, Some(pass));
    }
    let resp = req.send().await.with_context(|| {
        if url.starts_with("https://") && !tls_roots_available {
            format!(
                "request failed (https URL, but no CA roots are available in the \
                 sandbox: configure RIO_CA_BUNDLE on the worker so a bundle is \
                 mounted at {SANDBOX_CA_BUNDLE}, or use an http:// origin/mirror)"
            )
        } else {
            "request failed".to_string()
        }
    })?;
    let status = resp.status();
    if !status.is_success() {
        // 5xx / 408 / 429 are worth retrying against the same URL;
        // anything else (404 from a mirror, 403, …) will not change on
        // the next attempt — fail fast so the next candidate URL is
        // tried without burning the backoff budget.
        if status.is_server_error()
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            bail!("HTTP {status} from {url}");
        }
        return Err(PermanentHttpError {
            status,
            url: url.to_string(),
        }
        .into());
    }

    // Download to a temp file in the same directory (same filesystem →
    // the final rename is atomic; a hostile/flaky server can't leave a
    // half-written output).
    let tmp = parent.join(format!(
        ".fetchurl-tmp-{}",
        params
            .output
            .file_name()
            .map(|n| n.display().to_string())
            .unwrap_or_else(|| "out".to_owned())
    ));
    let download_result = download_to(resp, &tmp).await;
    if download_result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return download_result;
    }

    let finalize = finalize_output(&tmp, params).await;
    // The temp file is consumed by rename on the plain path; on the
    // unpack path (and on any failure) it must not linger in the store
    // scratch where the output scan would reject it as a stray.
    let _ = tokio::fs::remove_file(&tmp).await;
    finalize
}

/// Stream an HTTP response body to `dest`.
async fn download_to(resp: reqwest::Response, dest: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response body")?;
        file.write_all(&chunk).await.context("writing download")?;
    }
    file.flush().await.context("flushing download")?;
    Ok(())
}

/// Turn the downloaded temp file into the final output per the params.
async fn finalize_output(tmp: &Path, params: &FetchurlParams) -> anyhow::Result<()> {
    if params.unpack {
        restore_unpacked(tmp, params).await?;
    } else {
        tokio::fs::rename(tmp, &params.output)
            .await
            .with_context(|| format!("renaming download to {}", params.output.display()))?;
    }
    // CppNix's builtinFetchurl applies the `executable = "1"` chmod 0755
    // to the output path AFTER either branch (restorePath for unpack,
    // writeFile for plain) — builtins/fetchurl.cc. Matching that matters
    // for the FOD hash: when an unpacked NAR's root is a regular file,
    // the executable bit changes the recursive NAR hash, so a derivation
    // declaring both `unpack = true` and `executable = true` must get the
    // same bit Nix would give it.
    if params.executable {
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&params.output, perms)
            .await
            .context("chmod 0755 on executable output")?;
    }
    Ok(())
}

use std::os::unix::fs::PermissionsExt as _;

/// `unpack = true`: the payload (xz-compressed iff the origin URL ends
/// in `.xz`) is a NAR; restore it at the output path.
///
/// The decode + restore runs on a blocking thread: the NAR restorer is
/// synchronous (`rio_nix::nar::restore_path_streaming`), and bridging
/// it over the async decoder via `SyncIoBridge` avoids buffering the
/// whole decompressed archive anywhere.
async fn restore_unpacked(tmp: &Path, params: &FetchurlParams) -> anyhow::Result<()> {
    use async_compression::tokio::bufread::XzDecoder;
    use tokio::io::BufReader;

    let file = tokio::fs::File::open(tmp)
        .await
        .with_context(|| format!("opening {}", tmp.display()))?;
    let buf = BufReader::new(file);

    // The reader chain is built here (async context) and then driven
    // from the blocking thread through SyncIoBridge, which uses the
    // current runtime handle.
    let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> = if params.is_xz() {
        Box::new(XzDecoder::new(buf))
    } else {
        Box::new(buf)
    };

    let dest = params.output.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let bridge = tokio_util::io::SyncIoBridge::new(reader);
        // `take(cap + 1)`: if the payload decompresses to more than the
        // cap the NAR restore hits a truncated stream and fails, rather
        // than this process filling the disk. Same pattern as
        // rio-store's substituter decode cap.
        let mut limited = bridge.take(MAX_RESTORED_BYTES + 1);
        let restore = rio_nix::nar::restore_path_streaming(&mut limited, &dest)
            .with_context(|| format!("restoring NAR to {}", dest.display()))
            .and_then(|()| {
                if limited.limit() == 0 {
                    bail!(
                        "unpacked payload exceeds the {MAX_RESTORED_BYTES}-byte cap \
                         (decompression bomb?)"
                    );
                }
                Ok(())
            });
        if restore.is_err() {
            // A half-restored tree at the output path would be scanned
            // (and rejected) as a stray on the next attempt, and a
            // retried fetch would fail on the existing destination.
            let _ = std::fs::remove_dir_all(&dest);
            let _ = std::fs::remove_file(&dest);
        }
        restore
    })
    .await
    .context("NAR restore task panicked")??;
    Ok(())
}

/// Minimal netrc lookup: returns the `(login, password)` for the host
/// of `url`, preferring an exact `machine` match and falling back to a
/// `default` entry. Only the token forms emitted by real netrc writers
/// are recognized (`machine X login Y password Z`, one or more per
/// file, whitespace/newline separated).
fn netrc_credentials(netrc: Option<&Path>, url: &str) -> anyhow::Result<Option<(String, String)>> {
    let Some(path) = netrc else { return Ok(None) };
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    let Some(host) = host else { return Ok(None) };
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading netrc {}", path.display()))?;

    let tokens: Vec<&str> = contents.split_whitespace().collect();
    let mut best: Option<(String, String)> = None;
    let mut i = 0;
    while i < tokens.len() {
        let (machine, start) = match tokens[i] {
            "machine" if i + 1 < tokens.len() => (Some(tokens[i + 1]), i + 2),
            "default" => (None, i + 1),
            _ => {
                i += 1;
                continue;
            }
        };
        let mut login = None;
        let mut password = None;
        let mut j = start;
        while j + 1 < tokens.len() {
            match tokens[j] {
                "login" => login = Some(tokens[j + 1].to_owned()),
                "password" => password = Some(tokens[j + 1].to_owned()),
                "machine" | "default" => break,
                _ => {}
            }
            j += 2;
        }
        if let (Some(l), Some(p)) = (login, password) {
            match machine {
                Some(m) if m == host => return Ok(Some((l, p))),
                None if best.is_none() => best = Some((l, p)),
                _ => {}
            }
        }
        i = start;
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn params(url: &str, mirrors: &[&str]) -> FetchurlParams {
        FetchurlParams {
            url: url.to_owned(),
            output: PathBuf::from("/out/x"),
            unpack: false,
            executable: false,
            mirrors: mirrors.iter().map(|s| s.to_string()).collect(),
            hash_algo: "sha256".into(),
            hash_b16: "ab".repeat(32),
            netrc: None,
        }
    }

    #[test]
    fn mirrors_are_tried_before_origin_and_origin_is_last() {
        let p = params(
            "https://example.org/src.tar.gz",
            &["http://m1/", "http://m2"],
        );
        let urls = p.candidate_urls();
        assert_eq!(urls.len(), 3);
        assert_eq!(urls[0], format!("http://m1/sha256/{}", "ab".repeat(32)));
        assert_eq!(urls[1], format!("http://m2/sha256/{}", "ab".repeat(32)));
        assert_eq!(urls[2], "https://example.org/src.tar.gz");
    }

    #[test]
    fn mirrors_skipped_without_hash() {
        let mut p = params("https://example.org/src", &["http://m1/"]);
        p.hash_b16 = String::new();
        assert_eq!(p.candidate_urls(), vec!["https://example.org/src"]);
    }

    #[test]
    fn xz_decision_uses_origin_url_not_mirror() {
        let p = params("https://example.org/source.nar.xz", &["http://m1/"]);
        assert!(p.is_xz());
        let p2 = params("https://example.org/source.nar", &["http://m1/"]);
        assert!(!p2.is_xz());
    }

    #[test]
    fn from_env_round_trip() {
        // Serialize via the same names the glue writes.
        let vars = [
            (env_vars::URL, "https://e.org/a.xz"),
            (env_vars::OUTPUT, "/nix/store/abc-a"),
            (env_vars::UNPACK, "1"),
            (env_vars::EXECUTABLE, "1"),
            (env_vars::MIRRORS, "http://m1/ http://m2"),
            (env_vars::HASH_ALGO, "sha256"),
            (env_vars::HASH_B16, "00ff"),
            (env_vars::NETRC, "/build/.netrc"),
        ];
        // std::env mutation is process-global: this test never runs
        // concurrently with another env-reading test in this module
        // (it is the only one).
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        let p = FetchurlParams::from_env().expect("parse");
        for (k, _) in vars {
            unsafe { std::env::remove_var(k) };
        }
        assert_eq!(p.url, "https://e.org/a.xz");
        assert_eq!(p.output, PathBuf::from("/nix/store/abc-a"));
        assert!(p.unpack && p.executable);
        assert_eq!(p.mirrors, vec!["http://m1/", "http://m2"]);
        assert_eq!(p.netrc.as_deref(), Some(Path::new("/build/.netrc")));
    }

    #[test]
    fn netrc_exact_machine_beats_default() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "default login dlogin password dpass\n\
             machine example.org login alice password s3cret"
        )
        .unwrap();
        let got = netrc_credentials(Some(f.path()), "https://example.org/x").unwrap();
        assert_eq!(got, Some(("alice".into(), "s3cret".into())));
        let fallback = netrc_credentials(Some(f.path()), "https://other.net/x").unwrap();
        assert_eq!(fallback, Some(("dlogin".into(), "dpass".into())));
    }

    #[test]
    fn s3_urls_rejected() {
        let p = params("s3://bucket/key", &[]);
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch(&p))
            .unwrap_err();
        assert!(err.to_string().contains("s3://"), "{err}");
    }

    /// End-to-end against a local axum server: plain download,
    /// executable bit, retry-after-failure, and unpack(.xz NAR).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetches_from_local_server() {
        use axum::{Router, routing::get};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        // A NAR containing one regular file "hello\n", xz-compressed at
        // fixture-build time below.
        let inner = tempfile::tempdir().unwrap();
        std::fs::write(inner.path().join("file"), b"hello\n").unwrap();
        let nar = rio_nix::nar::dump_path(&inner.path().join("file")).unwrap();
        let mut xz_nar = Vec::new();
        {
            use async_compression::tokio::write::XzEncoder;
            use tokio::io::AsyncWriteExt as _;
            let mut enc = XzEncoder::new(&mut xz_nar);
            enc.write_all(&nar).await.unwrap();
            enc.shutdown().await.unwrap();
        }

        let flaky_hits = Arc::new(AtomicU32::new(0));
        let fh = flaky_hits.clone();
        let app = Router::new()
            .route("/plain", get(|| async { "plain-contents" }))
            .route(
                "/flaky",
                get(move || {
                    let fh = fh.clone();
                    async move {
                        if fh.fetch_add(1, Ordering::SeqCst) == 0 {
                            Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
                        } else {
                            Ok("eventually")
                        }
                    }
                }),
            )
            .route(
                "/archive.nar.xz",
                get(move || {
                    let body = xz_nar.clone();
                    async move { body }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");

        let out_dir = tempfile::tempdir().unwrap();

        // Plain + executable.
        let mut p = params(&format!("{base}/plain"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("plain-out");
        p.executable = true;
        fetch(&p).await.expect("plain fetch");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"plain-contents");
        let mode = std::fs::metadata(&p.output).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        // Retry: first attempt 500s, second succeeds.
        let mut p = params(&format!("{base}/flaky"), &[]);
        p.output = out_dir.path().join("flaky-out");
        fetch(&p).await.expect("flaky fetch retried");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"eventually");
        assert!(flaky_hits.load(Ordering::SeqCst) >= 2);

        // Unpack: .xz NAR restored to a regular file with contents.
        let mut p = params(&format!("{base}/archive.nar.xz"), &[]);
        p.output = out_dir.path().join("unpacked-out");
        p.unpack = true;
        fetch(&p).await.expect("unpack fetch");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"hello\n");

        // Mirror preferred over a dead origin: candidate order is
        // mirror first, so a working mirror masks a 404 origin.
        let mut p = params(&format!("{base}/does-not-exist"), &[]);
        p.mirrors = vec![format!("{base}/")];
        // mirror URL = {base}/sha256/<hash> — serve it.
        // (Re-bind a tiny router for this case.)
        let app2 = Router::new().route("/sha256/{hash}", get(|| async { "from-mirror" }));
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a2 = l2.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l2, app2).await.unwrap() });
        p.mirrors = vec![format!("http://{a2}/")];
        p.output = out_dir.path().join("mirror-out");
        fetch(&p).await.expect("mirror fetch");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"from-mirror");
    }

    /// A missing CA bundle must not be fatal: fetcher pods carry no
    /// system trust store, and plain-HTTP fetches never need one. (This
    /// is the regression vm-fetcher-split-k3s caught: reqwest's default
    /// system-store loading turned "no bundle" into a construction-time
    /// error for every fetch.)
    #[test]
    fn client_builds_without_ca_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.crt");
        let (_client, roots) = build_client(&missing).expect("client without bundle");
        assert!(!roots, "no roots should be reported as available");
    }

    /// A present-but-useless bundle (no parseable certificates) is a
    /// configuration error worth failing loudly on (silently proceeding
    /// without the operator's roots would downgrade every TLS fetch to
    /// the actionable-failure path for no visible reason).
    #[test]
    fn garbage_ca_bundle_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("garbage.crt");
        std::fs::write(&bundle, b"not a pem").unwrap();
        let err = build_client(&bundle).unwrap_err();
        assert!(
            format!("{err:#}").contains("contains no certificates"),
            "unexpected error: {err:#}"
        );
    }

    /// An https URL attempted with no roots fails with a message that
    /// names the knob (RIO_CA_BUNDLE / the sandbox bundle path), not a
    /// bare TLS handshake error.
    #[tokio::test]
    async fn https_without_roots_names_the_knob() {
        use axum::{Router, routing::get};

        // A plain-HTTP listener; speaking TLS at it fails the handshake,
        // which is exactly the no-roots failure mode we want to wrap.
        let app = Router::new().route("/file", get(|| async { "irrelevant" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let (client, roots) = build_client(&dir.path().join("none.crt")).unwrap();
        assert!(!roots);

        let mut p = params(&format!("https://{addr}/file"), &[]);
        p.output = dir.path().join("out");
        let err = try_fetch_one(&client, &p.url, &p, roots)
            .await
            .expect_err("https without roots must fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("RIO_CA_BUNDLE"),
            "error should name the CA-bundle knob: {chain}"
        );
    }

    /// `unpack = true` + `executable = true`: CppNix chmods the restored
    /// root 0755 after unpacking (builtins/fetchurl.cc applies the chmod
    /// after either branch); the FOD hash of a single-file NAR root
    /// depends on that bit, so rio must do the same.
    #[tokio::test]
    async fn unpack_applies_executable_bit_like_cppnix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("payload"), b"#!/bin/sh\necho hi\n").unwrap();
        let nar = rio_nix::nar::dump_path(&dir.path().join("payload")).unwrap();
        let tmp = dir.path().join("download.nar");
        std::fs::write(&tmp, &nar).unwrap();

        let mut p = params("https://example.org/x.nar", &[]);
        p.unpack = true;
        p.executable = true;
        p.output = dir.path().join("out");

        finalize_output(&tmp, &p).await.unwrap();
        let meta = std::fs::metadata(&p.output).unwrap();
        assert!(meta.is_file(), "single-file NAR restores to a regular file");
        assert_eq!(
            meta.permissions().mode() & 0o111,
            0o111,
            "executable bits must be set after unpack, got {:o}",
            meta.permissions().mode()
        );
    }
}
