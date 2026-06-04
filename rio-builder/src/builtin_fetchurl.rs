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
//! - `s3://` URLs are not supported as a *transport* (documented
//!   divergence from Nix). The limitation is per-candidate: an s3://
//!   origin is skipped with a log line while hashed mirrors are still
//!   consulted, so mirror-served content fetches normally.

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
    /// Canonical algorithm spelling (`sha256`, …) for mirror URL
    /// construction only. The planner derives it from the PARSED
    /// declaration (`OutputHashAlgo::parse`), never the raw string —
    /// the raw spelling appears here only via the registered
    /// undecodable-algo fallback (`nix.divergence.fod-fallback-fingerprint+1`),
    /// which is unreachable behind the glue's declaration gate.
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

/// Cap on the bytes moved by ONE fetch attempt — a single budget
/// shared by every phase of the attempt: the HTTP body charge and, on
/// the unpack path, the decompressed-restore charge draw from the SAME
/// meter, so an attempt can neither move nor co-occupy on disk more
/// than 1× this bound in aggregate. (The previous shape gave the
/// download and the restore independent full budgets — one unpack
/// attempt could move 2× and hold compressed + restored payloads
/// totalling 2× simultaneously, contradicting this constant's own
/// "ONE fetch attempt" wording.) 64 GiB is far beyond any plausible
/// source archive plus its compressed form while still bounding the
/// damage to roughly the disk headroom a large build already needs.
///
/// Both paths are capped: the previous shape exempted plain downloads
/// ("the server cannot amplify") — but the origin URL is
/// tenant-controlled, so the server IS the adversary and can stream
/// arbitrarily many body bytes regardless of what any header claims.
// r[impl fetcher.fetchurl.transfer-cap+2]
const MAX_TRANSFER_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Bytes between transfer progress lines on build stderr.
///
/// Liveness arithmetic: progress lines reach the sandbox pty, which is
/// what feeds rio-exec's activity watch (the max-silent clock). At a
/// 16 MiB cadence, any transfer sustaining ≥ ~28 KiB/s under the
/// default `max_silent = 600s` emits at least one line per window and
/// the build survives; a fully stalled connection is partitioned off
/// earlier by the HTTP client's 300s idle `read_timeout` (transient,
/// candidate retried). Transfers alive-but-slower than ~28 KiB/s are
/// treated as silent by the build's own policy — deliberately: at that
/// rate a 100 MiB source takes over an hour.
// r[impl fetcher.fetchurl.transfer-progress]
const PROGRESS_INTERVAL_BYTES: u64 = 16 * 1024 * 1024;

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
    /// Per-attempt transfer budget, shared across the attempt's phases
    /// (HTTP body + decompressed restore charge one meter — the
    /// aggregate an attempt moves or co-occupies never exceeds 1×).
    /// Always `MAX_TRANSFER_BYTES` in production (`from_env` pins it;
    /// this is NOT operator-configurable) — a field only so tests can
    /// exercise exhaustion without 64 GiB fixtures.
    pub transfer_cap: u64,
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
            transfer_cap: MAX_TRANSFER_BYTES,
        })
    }

    /// Fetch candidates in order: each hashed mirror as
    /// `<mirror>/<algo>/<base16-hash>`, then the origin URL. Mirrors
    /// are skipped when either hash component is missing (the glue
    /// only passes them for flat-mode FODs).
    ///
    /// Every candidate carries its provenance: mirrors are
    /// operator-configured (pool spec), the origin is
    /// tenant-controlled (the derivation's `url`). Credential
    /// resolution consumes the provenance — there is no plain URL
    /// list to flatten it away.
    // r[impl fetcher.mirrors.hashed+3]
    pub fn candidates(&self) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        if !self.hash_algo.is_empty() && !self.hash_b16.is_empty() {
            for mirror in &self.mirrors {
                let base = mirror.trim_end_matches('/');
                candidates.push(Candidate {
                    url: format!("{base}/{}/{}", self.hash_algo, self.hash_b16),
                    kind: CandidateKind::Mirror,
                });
            }
        }
        candidates.push(Candidate {
            url: self.url.clone(),
            kind: CandidateKind::Origin,
        });
        candidates
    }

    /// Whether the payload should be xz-decoded before NAR restore.
    /// Decided by the *origin* URL's suffix, never the mirror URL.
    pub fn is_xz(&self) -> bool {
        self.url.ends_with(".xz")
    }
}

/// Where a fetch candidate came from — the security boundary for
/// credential attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// Operator-configured hashed mirror (pool spec): trusted to
    /// receive the operator's `default` netrc credentials.
    Mirror,
    /// The derivation's own URL: tenant-controlled. Receives
    /// credentials only on an exact netrc `machine` match — the
    /// operator opted that specific host in.
    Origin,
}

/// One fetch candidate: the URL plus its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub kind: CandidateKind,
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

/// Closed permanence classification for one fetch attempt.
///
/// Every failure source inside [`try_fetch_one`] must choose a class at
/// the point where the error is produced — there is deliberately no
/// `From<anyhow::Error>` impl and no default bucket, so a future
/// failure source cannot silently inherit retryability (the
/// downcast-marker shape this replaces let exactly that happen: any
/// error NOT carrying the marker was retryable by omission). The retry
/// loop matches exhaustively.
#[derive(Debug)]
enum FetchError {
    /// Worth retrying against the SAME candidate: transport blips,
    /// 5xx / 408 / 429, interrupted bodies — the next attempt can
    /// genuinely see a different answer.
    Transient(anyhow::Error),
    /// Will not change on retry for THIS candidate: other HTTP
    /// statuses, payloads that fail finalization deterministically.
    /// The loop skips the candidate's remaining attempts and moves to
    /// the next candidate (which may serve different bytes).
    PermanentForCandidate(anyhow::Error),
}

impl FetchError {
    /// Unwrap the carried error (test assertions on the context chain).
    #[cfg(test)]
    fn into_inner(self) -> anyhow::Error {
        match self {
            FetchError::Transient(e) | FetchError::PermanentForCandidate(e) => e,
        }
    }
}

/// Typed budget-exhaustion failure, produced INSIDE
/// [`TransferMeter::charge`] — the statement that detects it — so every
/// downstream classification site identifies exhaustion by DOWNCAST,
/// never by string-matching an error message. Exhaustion is permanent
/// for the candidate (the same payload exhausts the same budget every
/// retry); carrying that fact as a type is what lets the unpack
/// restore's error chain distinguish "the payload is over budget"
/// (permanent) from "the worker's disk hiccuped" (transient errno)
/// at the statement that produced each.
// r[impl fetcher.fetchurl.permanence-at-source+3]
#[derive(Debug, thiserror::Error)]
#[error(
    "{what} exceeded the {cap}-byte per-attempt transfer cap (decompression bomb or unbounded body?)"
)]
struct CapExhausted {
    what: &'static str,
    cap: u64,
}

/// Typed per-attempt transfer budget: every byte path charges it, and
/// it owns the progress cadence — there is no way to move payload
/// bytes without metering them, because both copy loops read through
/// it ([`download_to`] charges per chunk; the unpack restore reads
/// through [`MeteredRead`]).
///
/// ONE meter exists per attempt: [`try_fetch_one`] constructs it and
/// threads the same instance download → restore (relabeled at the
/// phase boundary), so the budget is the attempt's AGGREGATE — a
/// second full budget for a later phase is unconstructible from this
/// flow, which is what pins the documented 1× movement/co-occupancy
/// bound.
///
/// Exhaustion is a hard error the caller classifies
/// `PermanentForCandidate`: the same candidate serves the same
/// over-budget payload on every retry, so retrying is pure waste —
/// typed exhaustion, never silent truncation (a truncated tarball
/// would fail the FOD hash gate with a misleading "hash mismatch").
struct TransferMeter {
    /// What is being metered, for the progress line ("download" /
    /// "unpack").
    what: &'static str,
    cap: u64,
    total: u64,
    next_mark: u64,
    /// Progress sink: `(phase_label, total_bytes)` at every
    /// [`PROGRESS_INTERVAL_BYTES`] boundary. The label is passed PER
    /// CALL from `self.what` so [`Self::relabel`] is live on the
    /// production line (round-17 merged_bug_005: the old sink captured
    /// the construction-time label by value, so every unpack-phase
    /// line still printed "download"). Production emits a line on
    /// build stderr (the sandbox pty — what feeds the max-silent
    /// activity watch); tests inject a recorder.
    emit: Box<dyn FnMut(&'static str, u64) + Send>,
}

impl TransferMeter {
    fn new(what: &'static str, cap: u64) -> Self {
        Self {
            what,
            cap,
            total: 0,
            next_mark: PROGRESS_INTERVAL_BYTES,
            emit: Box::new(|what, total| {
                eprintln!(
                    "builtin:fetchurl: {what}: {} MiB transferred",
                    total / (1024 * 1024)
                );
            }),
        }
    }

    #[cfg(test)]
    fn with_emit(
        what: &'static str,
        cap: u64,
        emit: Box<dyn FnMut(&'static str, u64) + Send>,
    ) -> Self {
        Self {
            what,
            cap,
            total: 0,
            next_mark: PROGRESS_INTERVAL_BYTES,
            emit,
        }
    }

    /// Move to the next metered phase of the SAME attempt: the label on
    /// progress lines changes, the running total and budget do not —
    /// that continuity is the single-budget property.
    fn relabel(&mut self, what: &'static str) {
        self.what = what;
    }

    /// Charge `n` transferred bytes: emit any crossed progress marks,
    /// fail when the budget is exhausted. The only failure is the typed
    /// [`CapExhausted`] — classification happens where the failure is
    /// produced, not at a caller boundary.
    // r[impl fetcher.fetchurl.transfer-cap+2]
    // r[impl fetcher.fetchurl.transfer-progress]
    fn charge(&mut self, n: u64) -> Result<(), CapExhausted> {
        self.total = self.total.saturating_add(n);
        while self.total >= self.next_mark {
            (self.emit)(self.what, self.total);
            self.next_mark = self.next_mark.saturating_add(PROGRESS_INTERVAL_BYTES);
        }
        if self.total > self.cap {
            return Err(CapExhausted {
                what: self.what,
                cap: self.cap,
            });
        }
        Ok(())
    }
}

/// Synchronous read adapter charging a [`TransferMeter`] per read —
/// the unpack path's restore loop cannot move a byte without metering
/// it. Cap exhaustion surfaces as an `io::Error` and aborts the
/// restore.
struct MeteredRead<R> {
    inner: R,
    meter: TransferMeter,
}

impl<R: std::io::Read> std::io::Read for MeteredRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        // The typed CapExhausted rides the io chain as the source so
        // the restore-site classifier can downcast it back out.
        self.meter.charge(n as u64).map_err(std::io::Error::other)?;
        Ok(n)
    }
}

/// ASCII-case-insensitive scheme test. RFC 3986 §3.1: scheme names
/// are case-insensitive (`S3://bucket` names the same transport as
/// `s3://bucket`), so every scheme-keyed verdict in this module MUST
/// match through this helper — a literal `starts_with("s3://")` lets
/// one uppercased letter route a candidate around its skip/permanence
/// arm and into the transient retry ladder (round-17 merged_bug_017;
/// the `verdict-fold-policy` check denies the literal form).
fn has_scheme(url: &str, scheme: &str) -> bool {
    let n = scheme.len();
    url.len() >= n + 3
        && url.is_char_boundary(n)
        && url[..n].eq_ignore_ascii_case(scheme)
        && url[n..].starts_with("://")
}

/// Fetch `params.url` (or a mirror) to `params.output`.
async fn fetch(params: &FetchurlParams) -> anyhow::Result<()> {
    let (client, tls_roots_available) = build_client(Path::new(SANDBOX_CA_BUNDLE))?;
    let candidates = params.candidates();
    let mut last_err: Option<anyhow::Error> = None;

    for candidate in &candidates {
        let url = &candidate.url;
        // r[impl fetcher.divergence.s3-transport]
        // The s3 transport limitation is a property of ONE candidate,
        // never of the whole fetch: an s3:// origin must not veto the
        // hashed mirrors that can serve the same content by hash (an
        // air-gapped pool with mirrors configured is exactly the
        // population that hits this). Skip-with-log, no attempt or
        // backoff budget consumed; if every candidate is skipped the
        // remembered error still names the limitation.
        if has_scheme(url, "s3") {
            eprintln!(
                "builtin:fetchurl: skipping {url}: s3:// URLs are not \
                 supported by the native builtin:fetchurl (use an \
                 https:// endpoint URL, or rely on hashed mirrors)"
            );
            if last_err.is_none() {
                last_err = Some(anyhow::anyhow!(
                    "s3:// URLs are not supported by the native \
                     builtin:fetchurl (use an https:// endpoint URL \
                     instead); no other candidate served the content"
                ));
            }
            continue;
        }
        for attempt in 0..ATTEMPTS_PER_URL {
            if attempt > 0 {
                tokio::time::sleep(RETRY_BACKOFF.duration(attempt - 1)).await;
            }
            eprintln!(
                "builtin:fetchurl: fetching {url} (attempt {}/{ATTEMPTS_PER_URL})",
                attempt + 1
            );
            match try_fetch_one(&client, candidate, params, tls_roots_available).await {
                Ok(()) => {
                    eprintln!("builtin:fetchurl: fetched {url}");
                    return Ok(());
                }
                Err(FetchError::Transient(e)) => {
                    eprintln!("builtin:fetchurl: attempt failed: {e:#}");
                    last_err = Some(e);
                }
                Err(FetchError::PermanentForCandidate(e)) => {
                    // Will not change on the next attempt — skip the
                    // remaining attempts for THIS url and move on to
                    // the next candidate immediately, without burning
                    // the backoff budget.
                    eprintln!("builtin:fetchurl: attempt failed permanently: {e:#}");
                    last_err = Some(e);
                    break;
                }
            }
        }
    }
    let tried: Vec<&str> = candidates.iter().map(|c| c.url.as_str()).collect();
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no candidate URLs (empty mirror list and URL)")))
    .with_context(|| format!("all candidates failed (tried {})", tried.join(", ")))
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

    // Redirects are common for source tarballs (GitHub → S3). The
    // policy is where a redirect TARGET is first known, so the
    // rootless-sandbox determinism verdict for redirected-into-https
    // requests is minted HERE, as a typed error the send-site
    // classifier downcasts back out — reqwest attaches the ORIGINAL
    // request URL to redirect-target failures, so classifying by
    // `e.url()` after the fact would mistake the redirected https
    // failure for a plain-http transient (round-17 merged_bug_017).
    // r[impl fetcher.fetchurl.permanence-at-source+3]
    builder = builder.redirect(if tls_roots_available {
        reqwest::redirect::Policy::limited(10)
    } else {
        reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" {
                let url = attempt.url().clone();
                attempt.error(HttpsRedirectWithoutRoots(url.to_string()))
            } else if attempt.previous().len() > 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        })
    });

    let client = builder.build().context("constructing HTTP client")?;
    Ok((client, tls_roots_available))
}

/// Typed marker minted by the redirect policy when a request is
/// redirected to an https URL in a sandbox with no CA roots: no such
/// request can ever verify a certificate, so following the redirect
/// could only burn the retry ladder on a deterministic failure.
/// Carried through reqwest's error source chain and downcast back out
/// by [`classify_send_error`].
#[derive(Debug, thiserror::Error)]
#[error("redirected to {0}, an https URL, but no CA roots are available in the sandbox")]
struct HttpsRedirectWithoutRoots(String);

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
///
/// Every failure is classified HERE, where it is produced
/// ([`FetchError`] has no blanket conversion): transport and
/// interrupted-body errors are transient; non-retryable HTTP statuses
/// and deterministic finalize failures are permanent for this
/// candidate.
async fn try_fetch_one(
    client: &reqwest::Client,
    candidate: &Candidate,
    params: &FetchurlParams,
    tls_roots_available: bool,
) -> Result<(), FetchError> {
    let url = candidate.url.as_str();
    // Worker-local fs preparation: transient (retry may land on a
    // recovered filesystem; a persistent fault fails all candidates and
    // surfaces as infra).
    let parent = params
        .output
        .parent()
        .context("output path has no parent directory")
        .map_err(FetchError::PermanentForCandidate)?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))
        .map_err(FetchError::Transient)?;

    let mut req = client.get(url);
    // Read-vs-parse permanence is decided at the producing statement
    // (classify_netrc_error): a malformed netrc is permanent, a
    // worker-environmental read fault retries.
    if let Some((user, pass)) =
        netrc_credentials(params.netrc.as_deref(), candidate).map_err(classify_netrc_error)?
    {
        req = req.basic_auth(user, Some(pass));
    }
    // Transport errors (DNS, connect, TLS, read timeout): transient —
    // with the deterministic exceptions classified per error in
    // [`classify_send_error`].
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => return Err(classify_send_error(e, url, tls_roots_available)),
    };
    let status = resp.status();
    if !status.is_success() {
        let err = anyhow::anyhow!("HTTP {status} from {url}");
        // 5xx / 408 / 429 are worth retrying against the same URL;
        // anything else (404 from a mirror, 403, …) will not change on
        // the next attempt.
        return Err(
            if status.is_server_error()
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                FetchError::Transient(err)
            } else {
                FetchError::PermanentForCandidate(err)
            },
        );
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
    // An interrupted body is transient: the server already proved it
    // can answer, the stream just died.
    //
    // ONE meter for the whole attempt: the download charges it first,
    // then `finalize_output` threads the SAME meter (relabeled) through
    // the unpack restore — body bytes + decompressed bytes draw from
    // one budget, so the attempt's aggregate movement and disk
    // co-occupancy are both bounded by 1× the cap.
    let mut meter = TransferMeter::new("download", params.transfer_cap);
    let download_result = download_to(resp, &tmp, &mut meter).await;
    if let Err(e) = download_result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    // Finalize classifies its own failures at the statements that
    // produce them (payload-decode → permanent for these bytes;
    // worker-local output-tree I/O → transient): no boundary map_err
    // here — the deleted blanket `map_err(PermanentForCandidate)` was
    // exactly the default bucket the FetchError contract forbids, and
    // it skipped retries for ENOSPC/EIO faults that the identical
    // download-phase fault would have retried.
    let finalize = finalize_output(&tmp, params, meter).await;
    // The temp file is consumed by rename on the plain path; on the
    // unpack path (and on any failure) it must not linger in the store
    // scratch where the output scan would reject it as a stray.
    let _ = tokio::fs::remove_file(&tmp).await;
    finalize
}

/// Classify a `send()` failure AT THE PRODUCING STATEMENT, by the
/// error's own properties — never by the literal candidate string the
/// attempt started from:
///
/// - **Builder errors** (`e.is_builder()`: malformed URL, unsupported
///   scheme — reqwest validates lazily, so the candidate's own defects
///   surface here): deterministic per candidate, permanent. Oracle
///   parity: the pinned CppNix transfer loop classifies
///   `CURLE_URL_MALFORMAT` / `CURLE_UNSUPPORTED_PROTOCOL` as `Misc`
///   (`filetransfer.cc:689-707`) and only `Transient` codes re-enter
///   its retry loop (`filetransfer.cc:747`).
/// - **TLS-impossible candidates**: with no CA roots in the sandbox,
///   no https request can EVER verify a certificate, so the failure is
///   deterministic regardless of which transport step surfaced it.
///   The https test covers the EFFECTIVE request, not just the
///   candidate string: redirects are followed inside `send()`, and a
///   redirect into https in a rootless sandbox is refused BY THE
///   REDIRECT POLICY — the one place the redirect target is first
///   known (reqwest attaches the ORIGINAL request URL to
///   redirect-target failures, so `e.url()` cannot make this call) —
///   as the typed [`HttpsRedirectWithoutRoots`], downcast back out
///   here. The candidate-string scheme (matched case-insensitively)
///   covers the direct-https form. The chart-default mirror
///   `http://tarballs.nixos.org/` hits the redirect form on every
///   flat-FOD fetch in a rootless sandbox.
/// - Everything else (DNS, connect, timeouts, TLS with roots
///   available): transient.
// r[impl fetcher.fetchurl.permanence-at-source+3]
fn classify_send_error(
    e: reqwest::Error,
    candidate_url: &str,
    tls_roots_available: bool,
) -> FetchError {
    if e.is_builder() {
        return FetchError::PermanentForCandidate(anyhow::Error::new(e).context(
            "request could not be constructed for this candidate \
             (malformed URL or unsupported scheme)",
        ));
    }
    let redirected_https = {
        let mut src = std::error::Error::source(&e);
        let mut found = false;
        while let Some(s) = src {
            if s.downcast_ref::<HttpsRedirectWithoutRoots>().is_some() {
                found = true;
                break;
            }
            src = s.source();
        }
        found
    };
    if (redirected_https || has_scheme(candidate_url, "https")) && !tls_roots_available {
        return FetchError::PermanentForCandidate(anyhow::Error::new(e).context(format!(
            "request failed (https effective URL, but no CA roots are available in \
             the sandbox: configure RIO_CA_BUNDLE on the worker so a bundle is \
             mounted at {SANDBOX_CA_BUNDLE}, or use an http:// origin/mirror that \
             does not redirect to https)"
        )));
    }
    FetchError::Transient(anyhow::Error::new(e).context("request failed"))
}

/// Stream an HTTP response body to `dest`.
/// Stream an HTTP response body to `dest`, charging every chunk
/// against the transfer budget.
///
/// Classification at source: stream/file errors are transient (the
/// connection died — retry can succeed); budget exhaustion is
/// permanent for this candidate (the same payload exhausts it again).
async fn download_to(
    resp: reqwest::Response,
    dest: &Path,
    meter: &mut TransferMeter,
) -> Result<(), FetchError> {
    use tokio::io::AsyncWriteExt as _;
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))
        .map_err(FetchError::Transient)?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .context("reading response body")
            .map_err(FetchError::Transient)?;
        meter
            .charge(chunk.len() as u64)
            .map_err(|e| FetchError::PermanentForCandidate(e.into()))?;
        file.write_all(&chunk)
            .await
            .context("writing download")
            .map_err(FetchError::Transient)?;
    }
    file.flush()
        .await
        .context("flushing download")
        .map_err(FetchError::Transient)?;
    Ok(())
}

/// Turn the downloaded temp file into the final output per the params.
///
/// `meter` is the attempt's ONE transfer budget, already charged with
/// the downloaded body; the unpack restore continues charging it (the
/// plain path moves no further payload bytes — rename is a metadata
/// operation).
// r[impl fetcher.fetchurl.attempt-atomic]
async fn finalize_output(
    tmp: &Path,
    params: &FetchurlParams,
    meter: TransferMeter,
) -> Result<(), FetchError> {
    // Attempt atomicity is the FUNCTION's property, not each step's:
    // the guard arms over the output path for the whole fallible scope
    // and is disarmed only after every step — including the trailing
    // chmod — succeeded. Any early `?` (either materialization branch
    // OR the chmod after them) drops the guard and removes whatever
    // was materialized, so a failed attempt can never strand an output
    // that poisons the next candidate's attempt or reaches the FOD
    // hash gate half-finalized. (The previous shape hand-cleaned
    // inside `restore_unpacked` only — a chmod failure AFTER a
    // successful restore left the fully-restored tree in place.)
    //
    // Oracle note: CppNix's builtinFetchurl has no in-builtin retry
    // after materialization (a chmod failure fails the whole builtin),
    // so it never needs this invariant; rio retries the next candidate
    // inside the process, which makes attempt-atomicity rio-owned.
    let guard = FreshOutput::arm(&params.output);
    if params.unpack {
        // Already classified per source inside (decode → permanent,
        // worker fs errno → transient, cap exhaustion → permanent).
        restore_unpacked(tmp, params, meter).await?;
    } else {
        // Worker-local output-tree I/O: ENOSPC/EIO/EROFS here is the
        // worker's disk, not the payload — the identical fault during
        // the download phase is retried, so this one is too.
        // r[impl fetcher.fetchurl.permanence-at-source+3]
        tokio::fs::rename(tmp, &params.output)
            .await
            .with_context(|| format!("renaming download to {}", params.output.display()))
            .map_err(FetchError::Transient)?;
    }
    // CppNix's builtinFetchurl applies the `executable = "1"` chmod 0755
    // to the output path AFTER either branch (restorePath for unpack,
    // writeFile for plain) — builtins/fetchurl.cc. Matching that matters
    // for the FOD hash: when an unpacked NAR's root is a regular file,
    // the executable bit changes the recursive NAR hash, so a derivation
    // declaring both `unpack = true` and `executable = true` must get the
    // same bit Nix would give it.
    if params.executable {
        // Same class as the rename: worker-local fs metadata I/O.
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&params.output, perms)
            .await
            .context("chmod 0755 on executable output")
            .map_err(FetchError::Transient)?;
    }
    guard.disarm();
    Ok(())
}

/// RAII cleanup guard for one finalize attempt: on drop, removes
/// whatever exists at the output path. [`FreshOutput::disarm`] is the
/// ONLY way to keep the output, and it is reachable only at the end of
/// the fully-successful path — failure scope = cleanup scope by
/// construction, so a new fallible step added to the finalize cannot
/// fall outside the cleanup (the gap the previous hand-rolled,
/// branch-local cleanup left for the chmod).
struct FreshOutput<'a> {
    path: Option<&'a Path>,
}

impl<'a> FreshOutput<'a> {
    fn arm(path: &'a Path) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for FreshOutput<'_> {
    fn drop(&mut self) {
        if let Some(p) = self.path {
            // Both forms: the output may be a tree (unpack) or a file/
            // symlink (plain or single-file NAR). remove_file unlinks
            // symlinks themselves (never follows), so a dangling
            // symlink output — invisible to `Path::exists`, which
            // stats THROUGH the link — is still cleaned.
            let _ = std::fs::remove_dir_all(p);
            let _ = std::fs::remove_file(p);
        }
    }
}

use std::os::unix::fs::PermissionsExt as _;

/// `unpack = true`: the payload (xz-compressed iff the origin URL ends
/// in `.xz`) is a NAR; restore it at the output path.
///
/// The decode + restore runs on a blocking thread: the NAR restorer is
/// synchronous (`rio_nix::nar::restore_path_streaming`), and bridging
/// it over the async decoder via `SyncIoBridge` avoids buffering the
/// whole decompressed archive anywhere.
async fn restore_unpacked(
    tmp: &Path,
    params: &FetchurlParams,
    mut meter: TransferMeter,
) -> Result<(), FetchError> {
    use async_compression::tokio::bufread::XzDecoder;
    use tokio::io::BufReader;

    // Worker-local fs read of our own temp file: transient.
    let file = tokio::fs::File::open(tmp)
        .await
        .with_context(|| format!("opening {}", tmp.display()))
        .map_err(FetchError::Transient)?;
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
    // Same attempt, same budget: only the progress label changes. The
    // restore's decompressed bytes stack on top of the already-charged
    // body bytes, keeping the attempt's aggregate at ≤ 1× cap.
    meter.relabel("unpack");
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // The restore reads through the meter, so it counts
        // DECOMPRESSED bytes — the dimension a decompression bomb
        // amplifies. Exhaustion aborts the restore mid-stream (typed,
        // inside the read) instead of the old `take(cap+1)` shape,
        // which surfaced as a misleading truncated-NAR parse error.
        let bridge = tokio_util::io::SyncIoBridge::new(reader);
        let mut metered = MeteredRead {
            inner: bridge,
            meter,
        };
        // No branch-local cleanup here: half-restored trees (and every
        // other failure shape, including ones in steps AFTER this one)
        // are removed by `finalize_output`'s FreshOutput guard — the
        // cleanup scope is the whole finalize attempt.
        rio_nix::nar::restore_path_streaming(&mut metered, &dest)
            .with_context(|| format!("restoring NAR to {}", dest.display()))
    })
    .await
    // A spawn_blocking JoinError is a restore PANIC (blocking tasks
    // are never cancelled mid-flight): a panic in a pure function of
    // the payload bytes reproduces on the same bytes — permanent for
    // the candidate, exactly like a typed decode failure. The old
    // Transient arm here re-downloaded a panic-triggering payload on
    // every attempt (round-17 merged_bug_022).
    // r[impl fetcher.fetchurl.permanence-at-source+3]
    .context("NAR restore task panicked")
    .map_err(FetchError::PermanentForCandidate)?
    .map_err(classify_restore_error)?;
    Ok(())
}

/// Classify a restore failure AT ITS PRODUCING FUNCTION by walking the
/// error chain — the restore is one statement whose single error value
/// interleaves three distinct sources, and each keeps its own
/// permanence:
///
/// - typed [`CapExhausted`] (riding the metered read's io chain): the
///   payload's nature — permanent for the candidate;
/// - an `io::Error` whose errno is in the worker-environmental
///   ALLOWLIST (`ENOSPC`/`EIO`/`EROFS`/`EDQUOT`/`ENOMEM`): the
///   worker's filesystem/memory — transient, exactly like the
///   identical fault during the download phase;
/// - EVERYTHING else — errno-free decode errors (xz format, NAR
///   structure) and errno-bearing failures outside the allowlist
///   alike — deterministic for these bytes: permanent for the
///   candidate. Errno PRESENCE is not transience: the restorer
///   performs syscalls with payload-controlled arguments, so a
///   payload can compose errnos (`ENAMETOOLONG` before the
///   kernel-equivalent bounds; `EILSEQ`-class oddities on exotic
///   filesystems) — an errno the worker did not cause must not buy
///   the payload another download (round-17 merged_bug_022; under the
///   presence rule a crafted NAR moved up to attempts × cap bytes).
// r[impl fetcher.fetchurl.permanence-at-source+3]
fn classify_restore_error(e: anyhow::Error) -> FetchError {
    if e.chain()
        .any(|c| c.downcast_ref::<CapExhausted>().is_some())
    {
        return FetchError::PermanentForCandidate(e);
    }
    let worker_environmental = e
        .chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .filter_map(|io| io.raw_os_error())
        .any(|errno| {
            matches!(
                errno,
                libc::ENOSPC | libc::EIO | libc::EROFS | libc::EDQUOT | libc::ENOMEM
            )
        });
    if worker_environmental {
        FetchError::Transient(e)
    } else {
        FetchError::PermanentForCandidate(e)
    }
}

/// One parsed netrc entry: a `machine <name>` or `default` block plus
/// whatever credentials followed it. `machine: None` is the `default`
/// entry.
#[derive(Debug, Default)]
struct NetrcEntry {
    machine: Option<String>,
    login: Option<String>,
    password: Option<String>,
}

/// Shape class of an offending netrc token — the ONLY thing an error
/// may say about it besides its length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenClass {
    /// Starts with `"` (the rejected quoted form).
    Quoted,
    /// Any other token.
    Bare,
}

/// Position-and-shape summary of a netrc token: length and class,
/// never bytes. This is the only token-derived payload type
/// [`NetrcParseError`] carries, which is what makes the no-echo
/// property TOTAL: an arm that wanted to echo would need a `String`
/// field, and the exhaustive fixture table in tests forces every new
/// arm through review with its payload type in plain sight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenSummary {
    len: usize,
    class: TokenClass,
}

impl From<&str> for TokenSummary {
    fn from(tok: &str) -> Self {
        TokenSummary {
            len: tok.len(),
            class: if tok.starts_with('"') {
                TokenClass::Quoted
            } else {
                TokenClass::Bare
            },
        }
    }
}

impl std::fmt::Display for TokenSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let class = match self.class {
            TokenClass::Quoted => "quoted",
            TokenClass::Bare => "bare",
        };
        write!(f, "{class} token, {} bytes", self.len)
    }
}

/// netrc failure split at its two producing statements: reading the
/// file and parsing its contents are different error sources with
/// different permanence, and folding them into one arm is exactly the
/// composite blanket amended R1(e) forbids.
#[derive(Debug, thiserror::Error)]
enum NetrcError {
    /// `read_to_string` failed. The PATH is operator config (the
    /// sandbox netrc location), never tenant input — safe to name.
    #[error("reading netrc {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The file read fine and parsed the same way it always will.
    #[error("parsing netrc")]
    Parse(#[source] NetrcParseError),
}

/// Classify a netrc failure AT ITS PRODUCING STATEMENT — replaces the
/// call-site blanket (`map_err(PermanentForCandidate)` over the whole
/// `netrc_credentials` composite) that round-15's permanence pass
/// introduced and the round-17 R1(e) exemplar deliberately left for
/// this commit:
///
/// - [`NetrcError::Parse`]: deterministic — a malformed netrc parses
///   the same way every time. Permanent for the candidate.
/// - [`NetrcError::Read`] with a worker-environmental errno
///   (`ENOSPC`/`EIO`/`EROFS`/`EDQUOT`/`ENOMEM` — the same allowlist as
///   [`classify_restore_error`], same rationale): the worker's disk or
///   memory, not the file. Transient.
/// - [`NetrcError::Read`] otherwise — including the documented
///   NotFound DELTA from the restore classifier's context: there a
///   missing path is a dropped-input infra signal; here the operator
///   configured a netrc path that does not exist in the sandbox, which
///   is deterministic until the pool spec changes. Permanent for the
///   candidate.
// r[impl fetcher.fetchurl.permanence-at-source+3]
fn classify_netrc_error(e: NetrcError) -> FetchError {
    match &e {
        NetrcError::Parse(_) => FetchError::PermanentForCandidate(e.into()),
        NetrcError::Read { source, .. } => {
            let worker_environmental = source.raw_os_error().is_some_and(|errno| {
                matches!(
                    errno,
                    libc::ENOSPC | libc::EIO | libc::EROFS | libc::EDQUOT | libc::ENOMEM
                )
            });
            if worker_environmental {
                FetchError::Transient(e.into())
            } else {
                FetchError::PermanentForCandidate(e.into())
            }
        }
    }
}

/// Closed netrc parse-error surface. Every variant's payload is either
/// a [`TokenSummary`] (length + class, never bytes) or a `&'static
/// str` naming a CANONICAL keyword (`"login"`, `"password"`, … — the
/// matched literal, never the input's spelling), so no arm CAN
/// interpolate input bytes — the no-echo property holds by
/// construction in every position, keyword and machine-name positions
/// included.
///
/// That REVERSES round-16's documented carve-out ("keyword positions
/// echo the token, which is what makes a typo diagnosable"): parse
/// errors flow into build-failure logs whose audience is every tenant
/// of the pool plus the operator, and a malformed netrc can land a
/// CREDENTIAL in keyword position (a stray value after a consumed
/// pair, a quoted password where a keyword belongs). Diagnosis keeps
/// the position, length, and shape — enough to find the line — and
/// gives up the bytes.
// r[impl fetcher.divergence.netrc-strict-parse]
#[derive(Debug, thiserror::Error)]
enum NetrcParseError {
    #[error(
        "netrc: quoted token in keyword position ({0}) is not supported \
         (fetcher.divergence.netrc-strict-parse)"
    )]
    QuotedKeyword(TokenSummary),
    #[error(
        "netrc: quoted value for `{key}` is not supported \
         (fetcher.divergence.netrc-strict-parse)"
    )]
    QuotedValue { key: &'static str },
    #[error("netrc: `machine` is missing its host name")]
    MachineMissingName,
    #[error("netrc: `{key}` is missing its value")]
    MissingValue { key: &'static str },
    #[error("netrc: `{key}` before any `machine`/`default` entry")]
    CredentialBeforeEntry { key: &'static str },
    #[error(
        "netrc: `macdef` is not supported — its body ends at a blank line, \
         which a whitespace tokenizer cannot see \
         (fetcher.divergence.netrc-strict-parse)"
    )]
    Macdef,
    #[error(
        "netrc: unrecognized token in keyword position ({0}) \
         (fetcher.divergence.netrc-strict-parse)"
    )]
    Unrecognized(TokenSummary),
}

/// Strict whitespace-token netrc parser: ONE cursor, and every keyword
/// consumes its value in the same step (consume-on-key), so no token
/// is ever scanned twice and a credential VALUE can never be re-read
/// as a keyword or an entry delimiter. The oracle's delegated parser
/// has the same consume shape for the tokens it understands —
/// `login`/`password` values are stored by keyword state, never
/// re-matched (curl `netrc.c:275-299`) — and recognizes keywords
/// ASCII-case-insensitively (`curl_strequal`, `netrc.c:237-318`);
/// values are taken verbatim.
///
/// Where the oracle is lenient, this parser fails closed; the
/// divergence is deliberate and registered
/// (`fetcher.divergence.netrc-strict-parse`):
///
/// - `macdef`: the macro body runs to a BLANK LINE (`netrc.c:153-156`),
///   invisible to a whitespace tokenizer — tolerating it would feed
///   macro text to the credential parser. Rejected.
/// - quoted tokens: the oracle lexes quotes and escapes
///   (`netrc.c:163-226`), which `split_whitespace` cannot reproduce;
///   mis-splitting a quoted password silently truncates it. Rejected.
/// - unknown tokens: the oracle skips them one at a time, which is
///   exactly how an unrecognized value-carrying keyword corrupts the
///   stream (under curl, `account password login Z` stores `login` as
///   the password, `netrc.c:290-299`). Rejected — except `account`
///   itself, a real netrc keyword whose value is consumed and ignored.
///
/// Comment lines ARE oracle-parity, not a divergence: curl drops them
/// at LOAD time in `file2memory` (`netrc.c:91-94`) — pass leading
/// blanks, then `*line == '#'` drops the whole line — before the
/// tokenizer ever sees them. "Blank" is `ISBLANK` = space or tab
/// EXACTLY (`curl_ctype.h:45`), deliberately narrower than Unicode
/// whitespace: a line led by VT/FF is NOT a comment to curl even if
/// `#` follows, so it must not be one here either (it flows to the
/// tokenizer, where the strict parser rejects the `#` token — curl
/// would skip it; that residue is the registered unknown-token
/// divergence above, not a comment-handling one).
// r[impl fetcher.divergence.netrc-strict-parse]
fn parse_netrc(contents: &str) -> Result<Vec<NetrcEntry>, NetrcParseError> {
    /// `file2memory`'s load-time comment test (`netrc.c:91-94`):
    /// leading ISBLANK (space/tab ONLY, `curl_ctype.h:45`) passed,
    /// then a `#` first byte drops the line.
    fn is_comment_line(line: &str) -> bool {
        line.trim_start_matches([' ', '\t']).starts_with('#')
    }
    // Quote rejection applies in EVERY token position, value positions
    // included: the oracle would unquote `login "u"` to `u`, while a
    // whitespace tokenizer would store the quotes verbatim — a silent
    // credential mangle, not a parse. NO position echoes the token
    // (see [`NetrcParseError`] — the round-16 keyword-position
    // carve-out is reversed); errors carry a [`TokenSummary`] or a
    // canonical keyword name only.
    fn unquoted_keyword(tok: &str) -> Result<&str, NetrcParseError> {
        if tok.starts_with('"') {
            return Err(NetrcParseError::QuotedKeyword(TokenSummary::from(tok)));
        }
        Ok(tok)
    }
    fn unquoted_value<'t>(key: &'static str, tok: &'t str) -> Result<&'t str, NetrcParseError> {
        if tok.starts_with('"') {
            return Err(NetrcParseError::QuotedValue { key });
        }
        Ok(tok)
    }
    let mut entries: Vec<NetrcEntry> = Vec::new();
    // One cursor over the comment-filtered lines: `flat_map` keeps a
    // single token stream, so consume-on-key still means no token is
    // ever scanned twice (the filter only removes whole lines the
    // oracle never tokenizes either).
    let mut tokens = contents
        .lines()
        .filter(|line| !is_comment_line(line))
        .flat_map(str::split_whitespace);
    while let Some(tok) = tokens.next() {
        let tok = unquoted_keyword(tok)?;
        // Keyword recognition is case-insensitive like the oracle's
        // `curl_strequal`; the values consumed below stay verbatim.
        // The bound `key` is the CANONICAL literal, never the input
        // spelling — it is the only keyword text errors may carry.
        match tok.to_ascii_lowercase().as_str() {
            "machine" => {
                let name = tokens
                    .next()
                    .map(unquoted_keyword)
                    .transpose()?
                    .ok_or(NetrcParseError::MachineMissingName)?;
                entries.push(NetrcEntry {
                    machine: Some(name.to_owned()),
                    ..NetrcEntry::default()
                });
            }
            "default" => entries.push(NetrcEntry::default()),
            lowered @ ("login" | "password" | "account") => {
                let key: &'static str = match lowered {
                    "login" => "login",
                    "password" => "password",
                    _ => "account",
                };
                let value = tokens
                    .next()
                    .map(|t| unquoted_value(key, t))
                    .transpose()?
                    .ok_or(NetrcParseError::MissingValue { key })?;
                let entry = entries
                    .last_mut()
                    .ok_or(NetrcParseError::CredentialBeforeEntry { key })?;
                match key {
                    "login" => entry.login = Some(value.to_owned()),
                    "password" => entry.password = Some(value.to_owned()),
                    // `account` is consumed — so its value cannot land
                    // in keyword position — and ignored.
                    _ => {}
                }
            }
            "macdef" => return Err(NetrcParseError::Macdef),
            _ => return Err(NetrcParseError::Unrecognized(TokenSummary::from(tok))),
        }
    }
    Ok(entries)
}

/// netrc lookup, scoped by candidate provenance: an exact `machine`
/// match (the URL's host) applies to any candidate; the `default`
/// entry applies to operator-configured mirrors ONLY. A
/// tenant-controlled origin URL with no exact `machine` entry gets no
/// credentials — the operator's catch-all secret must never travel to
/// a host the tenant chose. Parsing is strict (see [`parse_netrc`]);
/// `machine` matching folds ASCII case on both sides like the oracle's
/// `curl_strequal(host, tok)` (`netrc.c:264`), because the URL host
/// arrives lowercase-normalized from the URL parser and an uppercase
/// `machine` entry must still match (`fetcher.netrc-host-case-fold`).
/// This is the OPPOSITE posture from FOD hash-algo spellings, which
/// are case-exact (`rio_nix::hash::OutputHashAlgo`) — different axes,
/// no shared normalization helper.
///
/// Deliberate divergence from CppNix, recorded: the oracle hands its
/// netrc to curl with `CURL_NETRC_OPTIONAL`
/// (`filetransfer.cc:566-567`), which applies machine *and default*
/// matching to every URL it fetches — including tenant origins. In a
/// multi-tenant deployment that is a credential-exfiltration channel
/// (a tenant submits a FOD pointing at their own server and reads the
/// Authorization header), so rio scopes the default entry to mirror
/// candidates, the same way the operator-vs-tenant trust split already
/// narrows `impureEnvVars` sources to the operator-configured map.
/// Residual, accepted (owner Q2): per-attempt `HTTP {status} from
/// {url}` log lines remain a status oracle for hosts the operator
/// explicitly listed as exact `machine` entries — a per-host opt-in.
// r[impl fetcher.fetchurl.netrc-origin-scope]
// r[impl fetcher.netrc-host-case-fold]
fn netrc_credentials(
    netrc: Option<&Path>,
    candidate: &Candidate,
) -> Result<Option<(String, String)>, NetrcError> {
    let Some(path) = netrc else { return Ok(None) };
    let host = reqwest::Url::parse(&candidate.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    let Some(host) = host else { return Ok(None) };
    let contents = std::fs::read_to_string(path).map_err(|source| NetrcError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let entries = parse_netrc(&contents).map_err(NetrcError::Parse)?;

    // Only a complete login+password pair authenticates; incomplete
    // entries are inert (the oracle likewise reports success only once
    // both are found, `netrc.c:325`).
    let complete: Vec<&NetrcEntry> = entries
        .iter()
        .filter(|e| e.login.is_some() && e.password.is_some())
        .collect();
    let creds = |e: &NetrcEntry| Some((e.login.clone()?, e.password.clone()?));
    if let Some(exact) = complete.iter().find(|e| {
        e.machine
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case(&host))
    }) {
        return Ok(creds(exact));
    }
    match candidate.kind {
        CandidateKind::Mirror => Ok(complete
            .iter()
            .find(|e| e.machine.is_none())
            .and_then(|e| creds(e))),
        CandidateKind::Origin => Ok(None),
    }
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
            transfer_cap: MAX_TRANSFER_BYTES,
        }
    }

    /// Candidate order AND provenance: mirrors first (tagged Mirror),
    /// origin last (tagged Origin) — the tags are what the credential
    /// scope keys on, so they are pinned alongside the order.
    // r[verify fetcher.mirrors.hashed+3]
    // r[verify fetcher.fetchurl.netrc-origin-scope]
    #[test]
    fn mirrors_are_tried_before_origin_and_origin_is_last() {
        let p = params(
            "https://example.org/src.tar.gz",
            &["http://m1/", "http://m2"],
        );
        let candidates = p.candidates();
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates[0],
            Candidate {
                url: format!("http://m1/sha256/{}", "ab".repeat(32)),
                kind: CandidateKind::Mirror,
            }
        );
        assert_eq!(
            candidates[1],
            Candidate {
                url: format!("http://m2/sha256/{}", "ab".repeat(32)),
                kind: CandidateKind::Mirror,
            }
        );
        assert_eq!(
            candidates[2],
            Candidate {
                url: "https://example.org/src.tar.gz".into(),
                kind: CandidateKind::Origin,
            }
        );
    }

    #[test]
    fn mirrors_skipped_without_hash() {
        let mut p = params("https://example.org/src", &["http://m1/"]);
        p.hash_b16 = String::new();
        assert_eq!(
            p.candidates(),
            vec![Candidate {
                url: "https://example.org/src".into(),
                kind: CandidateKind::Origin,
            }]
        );
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

    fn origin(url: &str) -> Candidate {
        Candidate {
            url: url.into(),
            kind: CandidateKind::Origin,
        }
    }

    fn mirror(url: &str) -> Candidate {
        Candidate {
            url: url.into(),
            kind: CandidateKind::Mirror,
        }
    }

    /// THE bug_095 pin (flipped from the old default-fallback test): a
    /// tenant-controlled origin URL receives credentials only on an
    /// exact `machine` match. The operator's `default` entry — a
    /// catch-all secret — never travels to a host the tenant chose.
    // r[verify fetcher.fetchurl.netrc-origin-scope]
    #[test]
    fn netrc_origin_requires_exact_machine_match() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "default login dlogin password dpass\n\
             machine example.org login alice password s3cret"
        )
        .unwrap();
        let exact = netrc_credentials(Some(f.path()), &origin("https://example.org/x")).unwrap();
        assert_eq!(exact, Some(("alice".into(), "s3cret".into())));
        let unmatched = netrc_credentials(Some(f.path()), &origin("https://other.net/x")).unwrap();
        assert_eq!(
            unmatched, None,
            "the default entry must never reach a tenant-controlled origin"
        );
    }

    /// The `default` entry still works where it is safe: operator-
    /// configured mirrors (and an exact machine match still wins on a
    /// mirror too).
    // r[verify fetcher.fetchurl.netrc-origin-scope]
    #[test]
    fn netrc_mirror_default_preserved() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "default login dlogin password dpass\n\
             machine mirror.example login alice password s3cret"
        )
        .unwrap();
        let fallback = netrc_credentials(Some(f.path()), &mirror("https://cache.other/x")).unwrap();
        assert_eq!(fallback, Some(("dlogin".into(), "dpass".into())));
        let exact = netrc_credentials(Some(f.path()), &mirror("https://mirror.example/x")).unwrap();
        assert_eq!(exact, Some(("alice".into(), "s3cret".into())));
    }

    /// THE bug_024 pin: URL parsers hand us a lowercase-normalized
    /// host, so an upper/mixed-case `machine` entry must still match
    /// (curl folds both layers via `curl_strequal`, `netrc.c:264`).
    /// Keyword recognition folds too (`netrc.c:237-318`); credential
    /// VALUES stay byte-exact.
    // r[verify fetcher.netrc-host-case-fold]
    #[test]
    fn netrc_machine_match_folds_ascii_case() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "MACHINE Example.ORG LOGIN Alice PASSWORD S3cret").unwrap();
        let lower = netrc_credentials(Some(f.path()), &origin("https://example.org/x")).unwrap();
        assert_eq!(
            lower,
            Some(("Alice".into(), "S3cret".into())),
            "mixed-case machine entry + folded keywords must match; values verbatim"
        );
        let upper = netrc_credentials(Some(f.path()), &origin("https://EXAMPLE.org/x")).unwrap();
        assert_eq!(upper, Some(("Alice".into(), "S3cret".into())));
    }

    /// THE merged_bug_047 pin: one cursor, consume-on-key. Credential
    /// VALUES that spell keywords (`machine`, `default`, `password`)
    /// are consumed by their key and never re-enter keyword position:
    /// no phantom entries, no cross-wired credentials, and no phantom
    /// `default` leaking to an unmatched host.
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_values_never_reparsed_as_keywords() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "machine a.example login machine password default\n\
             machine b.example login bob password pw"
        )
        .unwrap();
        let a = netrc_credentials(Some(f.path()), &origin("https://a.example/x")).unwrap();
        assert_eq!(a, Some(("machine".into(), "default".into())));
        let b = netrc_credentials(Some(f.path()), &origin("https://b.example/x")).unwrap();
        assert_eq!(b, Some(("bob".into(), "pw".into())));
        // The VALUE "default" must not have minted a default entry.
        let c = netrc_credentials(Some(f.path()), &mirror("https://c.example/x")).unwrap();
        assert_eq!(c, None, "value-position `default` minted a phantom entry");
    }

    /// `account` is a real netrc keyword: its value is consumed (and
    /// ignored), never left in keyword position. Under the oracle's
    /// skip-unknown lexing, `account password login p` stores `login`
    /// as the password (`netrc.c:290-299`) — here it parses cleanly.
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_account_value_is_consumed() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "machine a.example account password login u password p").unwrap();
        let got = netrc_credentials(Some(f.path()), &origin("https://a.example/x")).unwrap();
        assert_eq!(got, Some(("u".into(), "p".into())));
    }

    /// Strict-parse fail-closed arms: unknown tokens, `macdef`, quoted
    /// tokens, a keyword at EOF, and credentials before any entry all
    /// reject the whole file (the call site classifies the error
    /// permanent — a malformed netrc parses the same way every time).
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_strict_parse_fails_closed() {
        for (contents, needle) in [
            (
                "machine a.example port 8080 login u password p",
                "unrecognized token in keyword position (bare token, 4 bytes)",
            ),
            ("macdef init\nlogin u password p", "macdef"),
            (
                "machine a.example login \"u\" password p",
                "quoted value for `login`",
            ),
            (
                "\"machine\" a.example login u password p",
                "quoted token in keyword position (quoted token, 9 bytes)",
            ),
            ("machine a.example login", "`login` is missing its value"),
            (
                "login u password p",
                "`login` before any `machine`/`default`",
            ),
            ("machine", "`machine` is missing its host name"),
        ] {
            let err = parse_netrc(contents).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "{contents:?}: expected {needle:?} in {err:#}"
            );
        }
    }

    /// Comment lines are dropped at load like the oracle's
    /// `file2memory` (`netrc.c:91-94`): bare `#`, space- and TAB-led
    /// `#` lines vanish before tokenization — a comment-headed
    /// operator netrc (the common documented form) parses instead of
    /// bricking every fetchurl in the pool, and credential-shaped
    /// text inside a comment never reaches the entry table.
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_comment_lines_are_skipped_at_load() {
        let contents = "# operator notes: machine bogus.example login trap\n\
                        \t # also a comment\n\
                        machine a.example\n\
                        login u\n\
                        # password not-the-real-one\n\
                        password p\n";
        let entries = parse_netrc(contents).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "comment machine lines must not open entries"
        );
        assert_eq!(entries[0].machine.as_deref(), Some("a.example"));
        assert_eq!(entries[0].login.as_deref(), Some("u"));
        assert_eq!(
            entries[0].password.as_deref(),
            Some("p"),
            "the commented `password` line must not override the real one"
        );
    }

    /// ISBLANK counter-fixture: curl passes ONLY space/tab before the
    /// `#` test (`curl_ctype.h:45`), so a VT-led `#` line is NOT a
    /// comment to the oracle — and must not be one here. It flows to
    /// the tokenizer, where the strict parser rejects the `#` token
    /// (the registered unknown-token divergence; curl would skip it
    /// one token at a time). A wider "trim all whitespace" comment
    /// test would silently drop a line the oracle tokenizes.
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_vt_led_hash_line_is_not_a_comment() {
        let err = parse_netrc("\u{0B}# machine a.example\nmachine b.example login u password p")
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unrecognized token"),
            "VT-led `#` must reach the tokenizer and fail closed, not vanish: {err:#}"
        );
    }

    /// The error-surface pin: a quoted CREDENTIAL is rejected without
    /// echoing its bytes. Parse errors flow into build-failure logs
    /// (PermanentForCandidate context, tenant- and operator-visible),
    /// so a value-position token — which may BE a credential — is
    /// never interpolated; the message names the keyword instead.
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_quoted_value_error_never_echoes_the_credential() {
        let err = parse_netrc("machine a.example login u password \"hunter2\"").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("hunter2"),
            "credential bytes leaked into the parse error: {msg}"
        );
        assert!(msg.contains("quoted value for `password`"), "{msg}");
    }

    /// Read-vs-parse permanence split (the composite blanket is gone):
    /// a worker-environmental read errno retries, everything else —
    /// including the documented NotFound delta (a configured netrc
    /// path missing from the sandbox is deterministic until the pool
    /// spec changes) and every parse failure — is permanent for the
    /// candidate.
    // r[verify fetcher.fetchurl.permanence-at-source+3]
    #[test]
    fn netrc_read_errors_classify_by_errno_and_parse_stays_permanent() {
        let read = |errno: i32| NetrcError::Read {
            path: "/build/.netrc".into(),
            source: std::io::Error::from_raw_os_error(errno),
        };
        // Worker-environmental allowlist → transient.
        for errno in [
            libc::ENOSPC,
            libc::EIO,
            libc::EROFS,
            libc::EDQUOT,
            libc::ENOMEM,
        ] {
            assert!(
                matches!(classify_netrc_error(read(errno)), FetchError::Transient(_)),
                "errno {errno} must be transient (worker-environmental)"
            );
        }
        // The NotFound DELTA: configured-but-missing is deterministic.
        assert!(matches!(
            classify_netrc_error(read(libc::ENOENT)),
            FetchError::PermanentForCandidate(_)
        ));
        // Permission and other non-allowlist errnos: permanent.
        assert!(matches!(
            classify_netrc_error(read(libc::EACCES)),
            FetchError::PermanentForCandidate(_)
        ));
        // Parse failures: deterministic, permanent.
        let parse = NetrcError::Parse(parse_netrc("machine").unwrap_err());
        assert!(matches!(
            classify_netrc_error(parse),
            FetchError::PermanentForCandidate(_)
        ));
        // End to end: a missing configured netrc file errs as Read.
        let gone = netrc_credentials(
            Some(Path::new("/nonexistent/netrc-for-this-test")),
            &origin("https://a.example/x"),
        )
        .unwrap_err();
        assert!(matches!(gone, NetrcError::Read { .. }));
    }

    /// TOTAL no-echo: one fixture per [`NetrcParseError`] variant
    /// (the `variant_name` match is exhaustive — adding an arm fails
    /// compilation here, forcing a fixture row whose payload type is
    /// reviewed), each planting a credential-shaped sentinel at the
    /// position the error reports. No variant may carry input bytes:
    /// the round-16 keyword-position echo carve-out is REVERSED —
    /// build-failure logs reach every tenant of the pool, and a
    /// malformed netrc can land a credential in ANY position.
    /// Diagnosis keeps position + length + shape, never bytes.
    // r[verify fetcher.divergence.netrc-strict-parse]
    #[test]
    fn netrc_parse_errors_never_echo_input_in_any_position() {
        fn variant_name(e: &NetrcParseError) -> &'static str {
            match e {
                NetrcParseError::QuotedKeyword(_) => "QuotedKeyword",
                NetrcParseError::QuotedValue { .. } => "QuotedValue",
                NetrcParseError::MachineMissingName => "MachineMissingName",
                NetrcParseError::MissingValue { .. } => "MissingValue",
                NetrcParseError::CredentialBeforeEntry { .. } => "CredentialBeforeEntry",
                NetrcParseError::Macdef => "Macdef",
                NetrcParseError::Unrecognized(_) => "Unrecognized",
            }
        }
        const SENTINEL: &str = "sw0rdf1shSENTINEL";
        let table: &[(&str, String)] = &[
            ("QuotedKeyword", format!("\"{SENTINEL}\" a.example")),
            (
                "QuotedValue",
                format!("machine a.example password \"{SENTINEL}\""),
            ),
            ("MachineMissingName", "machine".to_owned()),
            (
                "MissingValue",
                format!("machine {SENTINEL}.example password"),
            ),
            ("CredentialBeforeEntry", format!("password {SENTINEL}")),
            ("Macdef", format!("macdef {SENTINEL}")),
            (
                "Unrecognized",
                format!("machine a.example login u password p {SENTINEL}"),
            ),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (expect_variant, contents) in table {
            let err = parse_netrc(contents).unwrap_err();
            assert_eq!(
                variant_name(&err),
                *expect_variant,
                "fixture routed to the wrong arm for {contents:?}"
            );
            let msg = format!("{err:#} / {err:?}");
            assert!(
                !msg.contains(SENTINEL),
                "{expect_variant}: input bytes leaked (Display or Debug): {msg}"
            );
            seen.insert(*expect_variant);
        }
        // Every variant the exhaustive match knows about has a row.
        for v in [
            "QuotedKeyword",
            "QuotedValue",
            "MachineMissingName",
            "MissingValue",
            "CredentialBeforeEntry",
            "Macdef",
            "Unrecognized",
        ] {
            assert!(seen.contains(v), "no fixture exercises {v}");
        }
    }

    /// All-candidates-skipped arm: an s3 origin with no mirrors still
    /// fails, naming the unsupported scheme (no silent empty loop).
    // r[verify fetcher.divergence.s3-transport]
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

    /// RFC 3986 scheme case-insensitivity: `S3://` is the same
    /// transport as `s3://` and takes the same skip arm — an
    /// uppercased letter must not route the candidate into the
    /// transient retry ladder (round-17 merged_bug_017).
    // r[verify fetcher.fetchurl.permanence-at-source+3]
    #[test]
    fn s3_scheme_skip_is_case_insensitive() {
        for url in ["S3://bucket/key", "s3://bucket/key", "S3://BUCKET/key"] {
            let p = params(url, &[]);
            let err = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(fetch(&p))
                .unwrap_err();
            // The full chain ({err:#}) — the skip-arm message is the
            // INNER error; the outer context merely echoes the URL,
            // which would make a literal-prefix miss invisible here.
            let msg = format!("{err:#}");
            assert!(
                msg.contains("s3:// URLs are not supported"),
                "case variant {url} must take the s3 skip arm: {msg}"
            );
        }
    }

    /// A candidate whose request cannot be CONSTRUCTED (unsupported
    /// scheme — reqwest validates lazily, so it surfaces at `send()`)
    /// is permanent for the candidate: one attempt, no backoff ladder.
    /// Oracle parity: `CURLE_UNSUPPORTED_PROTOCOL` is non-retriable
    /// `Misc` in the pinned transfer loop (`filetransfer.cc:689-707`).
    // r[verify fetcher.fetchurl.permanence-at-source+3]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn builder_error_is_permanent_for_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = params("ftp://example.invalid/file", &[]);
        p.mirrors.clear();
        p.output = dir.path().join("out");
        let candidates = p.candidates();
        let (client, _) = build_client(Path::new("/nonexistent/ca-bundle.crt")).unwrap();
        let err = try_fetch_one(&client, &candidates[0], &p, false)
            .await
            .expect_err("unsupported scheme must fail");
        assert!(
            matches!(err, FetchError::PermanentForCandidate(_)),
            "builder errors are deterministic per candidate: {:#}",
            err.into_inner()
        );
    }

    /// The TLS-impossible verdict keys on the EFFECTIVE request: an
    /// `http://` candidate that 301s into https (followed inside
    /// `send()`) is just as deterministic in a rootless sandbox as a
    /// literal https candidate — the chart-default
    /// `http://tarballs.nixos.org/` mirror hits exactly this shape
    /// (round-17 merged_bug_017).
    // r[verify fetcher.fetchurl.permanence-at-source+3]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_redirect_into_https_without_roots_is_permanent() {
        use axum::{Router, routing::get};

        let app = Router::new().route(
            "/file",
            get(|| async {
                (
                    axum::http::StatusCode::MOVED_PERMANENTLY,
                    [(axum::http::header::LOCATION, "https://127.0.0.1:1/file")],
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/file"), &[]);
        p.mirrors.clear();
        p.output = dir.path().join("out");
        let candidates = p.candidates();
        let (client, roots) = build_client(Path::new("/nonexistent/ca-bundle.crt")).unwrap();
        assert!(!roots, "test premise: no roots loaded");
        let err = try_fetch_one(&client, &candidates[0], &p, false)
            .await
            .expect_err("redirect into https with no roots must fail");
        match err {
            FetchError::PermanentForCandidate(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("CA roots"),
                    "remediation must name the CA-roots fix: {msg}"
                );
            }
            FetchError::Transient(e) => {
                panic!("https-effective failure must not re-enter the retry ladder: {e:#}")
            }
        }
    }

    /// The skip is per-candidate: an s3 origin must not veto hashed
    /// mirrors. A local server playing the mirror serves the hash
    /// path; the fetch succeeds without ever touching the s3 URL.
    // r[verify fetcher.divergence.s3-transport]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn s3_origin_consults_hashed_mirrors() {
        use axum::{Router, routing::get};

        let body = b"mirror-served-content".to_vec();
        let hex = "ab".repeat(32); // params() declares this hash_b16
        let app = Router::new().route(
            &format!("/sha256/{hex}"),
            get({
                let body = body.clone();
                move || {
                    let body = body.clone();
                    async move { body }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let mut p = params("s3://bucket/key", &[&format!("http://{addr}/")]);
        p.output = out.clone();

        fetch(&p).await.expect("mirror should serve the content");
        assert_eq!(std::fs::read(&out).unwrap(), body);
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

    /// bug_100 regression — the failure-path test the parent fix
    /// omitted: a chmod that fails AFTER a fully-successful unpack
    /// must leave NO output. Staged with a NAR whose root is a
    /// dangling symlink: the restore succeeds (symlinks restore
    /// without following), then `set_permissions` follows the link to
    /// a nonexistent target and fails. Asserted via symlink_metadata —
    /// `Path::exists()` stats THROUGH the dangling link and reports
    /// false even when the stranded symlink is right there.
    // r[verify fetcher.fetchurl.attempt-atomic]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_chmod_after_unpack_leaves_no_output() {
        use axum::{Router, routing::get};

        // NAR of a symlink pointing at a target that does not exist.
        let inner = tempfile::tempdir().unwrap();
        let link = inner.path().join("link");
        std::os::unix::fs::symlink("/nonexistent-target-for-rio-test", &link).unwrap();
        let nar = rio_nix::nar::dump_path(&link).unwrap();

        let app = Router::new().route(
            "/dangling.nar",
            get(move || {
                let body = nar.clone();
                async move { body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/dangling.nar"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("dangling-out");
        p.unpack = true;
        p.executable = true; // chmod follows the dangling link → fails
        let err = fetch(&p)
            .await
            .expect_err("chmod through dangling link fails");
        assert!(format!("{err:#}").contains("chmod"), "{err:#}");
        assert!(
            std::fs::symlink_metadata(&p.output).is_err(),
            "the stranded symlink output must be cleaned (symlink_metadata, \
             not exists(): exists() follows the dangling link and lies)"
        );
    }

    /// Attempt-atomicity across candidates: a first candidate whose
    /// finalize fails must not poison the second candidate's attempt
    /// (a stranded output makes the next restore fail on the existing
    /// path). Mirror serves the poisoned payload, origin serves the
    /// good one.
    // r[verify fetcher.fetchurl.attempt-atomic]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_finalize_does_not_poison_next_candidate() {
        use axum::{Router, routing::get};

        // Bad payload: dangling-symlink NAR (finalize fails at chmod).
        let inner = tempfile::tempdir().unwrap();
        let link = inner.path().join("link");
        std::os::unix::fs::symlink("/nonexistent-target-for-rio-test", &link).unwrap();
        let bad_nar = rio_nix::nar::dump_path(&link).unwrap();
        // Good payload: regular-file NAR.
        std::fs::write(inner.path().join("file"), b"good\n").unwrap();
        let good_nar = rio_nix::nar::dump_path(&inner.path().join("file")).unwrap();

        // The mirror candidate URL is {mirror}/{algo}/{hex}; serve the
        // bad payload there. The origin serves the good payload.
        let app = Router::new()
            .route(
                "/sha256/{hash}",
                get(move || {
                    let body = bad_nar.clone();
                    async move { body }
                }),
            )
            .route(
                "/good.nar",
                get(move || {
                    let body = good_nar.clone();
                    async move { body }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/good.nar"), &[]);
        p.mirrors = vec![format!("http://{addr}/")];
        p.output = out_dir.path().join("retry-out");
        p.unpack = true;
        p.executable = true;
        fetch(&p)
            .await
            .expect("the origin attempt must succeed on a clean path");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"good\n");
        let mode = std::fs::metadata(&p.output).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "good candidate fully finalized");
    }

    /// Happy-path disarm pin: success keeps the output (the guard must
    /// not clean up what it was guarding).
    // r[verify fetcher.fetchurl.attempt-atomic]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn successful_finalize_keeps_output() {
        use axum::{Router, routing::get};
        let app = Router::new().route("/f", get(|| async { "kept" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/f"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("kept-out");
        p.executable = true;
        fetch(&p).await.expect("fetch");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"kept");
    }

    /// Progress cadence with an injected writer: marks at every 16 MiB
    /// boundary, none in between, totals reported.
    // r[verify fetcher.fetchurl.transfer-progress]
    #[test]
    fn meter_emits_progress_at_fixed_cadence() {
        use std::sync::{Arc, Mutex};
        let marks = Arc::new(Mutex::new(Vec::new()));
        let m = marks.clone();
        let mut meter = TransferMeter::with_emit(
            "download",
            u64::MAX,
            Box::new(move |what, total| m.lock().unwrap().push((what, total))),
        );
        // 40 MiB in 1 MiB chunks: marks when crossing 16 and 32 MiB —
        // and a relabel mid-stream changes the LABEL on later marks
        // while the running total continues (round-17 merged_bug_005:
        // the label is read per-call, so relabel is live on the
        // production line; the old sink froze the construction-time
        // label and every unpack-phase line printed "download").
        for _ in 0..20 {
            meter.charge(1024 * 1024).expect("under cap");
        }
        meter.relabel("unpack");
        for _ in 0..20 {
            meter.charge(1024 * 1024).expect("under cap");
        }
        let marks = marks.lock().unwrap();
        assert_eq!(marks.len(), 2, "exactly the 16 MiB and 32 MiB marks");
        assert_eq!(
            marks[0],
            ("download", 16 * 1024 * 1024),
            "pre-relabel mark carries the download label"
        );
        assert_eq!(
            marks[1],
            ("unpack", 32 * 1024 * 1024),
            "post-relabel mark carries the unpack label on the SAME \
             running total (single-budget continuity)"
        );
    }

    /// Plain-download budget: a body larger than the per-attempt cap is
    /// permanent for the candidate — attempted exactly once, no retry
    /// burned re-downloading an over-budget payload. (The old shape
    /// exempted the plain path entirely: "the server cannot amplify" —
    /// but the origin IS the tenant's server.)
    // r[verify fetcher.fetchurl.transfer-cap+2]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn plain_download_over_cap_is_permanent() {
        use axum::{Router, routing::get};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/big",
            get(move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    vec![0u8; 64 * 1024]
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/big"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("big-out");
        p.transfer_cap = 16 * 1024; // 16 KiB budget vs 64 KiB body
        let err = fetch(&p).await.expect_err("over-cap must fail");
        assert!(
            format!("{err:#}").contains("transfer cap"),
            "typed exhaustion, not a hash mismatch: {err:#}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "an over-budget payload must not be re-downloaded"
        );
        assert!(
            !p.output.exists(),
            "no partial output left behind by the aborted attempt"
        );
    }

    /// Unpack budget counts DECOMPRESSED bytes: a small xz that
    /// expands past the cap (a decompression bomb) is permanent for
    /// the candidate and is not retried.
    // r[verify fetcher.fetchurl.transfer-cap+2]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn xz_bomb_is_permanent_not_retried() {
        use axum::{Router, routing::get};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        // A NAR of 1 MiB of zeros, xz-compressed to a few KiB.
        let inner = tempfile::tempdir().unwrap();
        std::fs::write(inner.path().join("file"), vec![0u8; 1024 * 1024]).unwrap();
        let nar = rio_nix::nar::dump_path(&inner.path().join("file")).unwrap();
        let mut xz_nar = Vec::new();
        {
            use async_compression::tokio::write::XzEncoder;
            use tokio::io::AsyncWriteExt as _;
            let mut enc = XzEncoder::new(&mut xz_nar);
            enc.write_all(&nar).await.unwrap();
            enc.shutdown().await.unwrap();
        }
        assert!(
            xz_nar.len() < 64 * 1024,
            "the bomb must be small on the wire"
        );

        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/bomb.nar.xz",
            get(move || {
                let h = h.clone();
                let body = xz_nar.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    body
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/bomb.nar.xz"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("bomb-out");
        p.unpack = true;
        p.transfer_cap = 64 * 1024; // wire bytes fit; decompressed do not
        let err = fetch(&p).await.expect_err("bomb must fail");
        assert!(
            format!("{err:#}").contains("transfer cap"),
            "typed exhaustion names the cap: {err:#}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "bombs are not retried");
        assert!(!p.output.exists(), "no half-restored tree left behind");
    }

    /// A truncated body (server closes mid-stream after promising
    /// more) stays TRANSIENT — the documented asymmetry with the cap:
    /// truncation is the connection's fault and a retry can genuinely
    /// succeed; over-budget is the payload's nature.
    // r[verify fetcher.fetchurl.transfer-cap+2]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn truncated_body_is_transient_and_retried() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::AsyncWriteExt as _;

        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let n = h.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    use tokio::io::AsyncReadExt as _;
                    let _ = sock.read(&mut buf).await;
                    if n == 0 {
                        // Promise 100 bytes, deliver 10, slam the door.
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789")
                            .await;
                        let _ = sock.shutdown().await;
                    } else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nwhole")
                            .await;
                        let _ = sock.shutdown().await;
                    }
                });
            }
        });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/file"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("trunc-out");
        fetch(&p)
            .await
            .expect("transient truncation retried to success");
        assert_eq!(std::fs::read(&p.output).unwrap(), b"whole");
        assert!(
            hits.load(Ordering::SeqCst) >= 2,
            "the truncated attempt must be retried"
        );
    }

    /// The restore classifier's arms, each pinned on the chain shape
    /// the real flow produces (round-16 merged_bug_068; round-17
    /// merged_bug_022): typed cap exhaustion → permanent; an
    /// ALLOWLISTED worker-environmental errno → transient;
    /// decode/structure errors AND payload-composable errnos →
    /// permanent. Errno presence is not transience.
    // r[verify fetcher.fetchurl.permanence-at-source+3]
    #[test]
    fn restore_classifier_discriminates_at_source() {
        // A payload-composable errno (a too-long name reaching the
        // syscall on a pre-bounds tree shape) must NOT buy the payload
        // another download: outside the allowlist → permanent.
        let payload_errno: anyhow::Error = anyhow::Error::from(rio_nix::nar::NarError::Io(
            std::io::Error::from_raw_os_error(libc::ENAMETOOLONG),
        ))
        .context("restoring NAR to /out");
        assert!(
            matches!(
                classify_restore_error(payload_errno),
                FetchError::PermanentForCandidate(_)
            ),
            "ENAMETOOLONG is a function of the payload, not the worker — permanent"
        );

        // Every allowlisted worker-environmental errno stays transient.
        for errno in [
            libc::ENOSPC,
            libc::EIO,
            libc::EROFS,
            libc::EDQUOT,
            libc::ENOMEM,
        ] {
            let chain: anyhow::Error = anyhow::Error::from(rio_nix::nar::NarError::Io(
                std::io::Error::from_raw_os_error(errno),
            ))
            .context("restoring NAR to /out");
            assert!(
                matches!(classify_restore_error(chain), FetchError::Transient(_)),
                "allowlisted errno {errno} is the worker's environment — transient"
            );
        }
        // Cap exhaustion rides the metered-read io chain.
        let cap_chain: anyhow::Error = anyhow::Error::from(rio_nix::nar::NarError::Io(
            std::io::Error::other(CapExhausted {
                what: "unpack",
                cap: 64,
            }),
        ))
        .context("restoring NAR to /out");
        assert!(
            matches!(
                classify_restore_error(cap_chain),
                FetchError::PermanentForCandidate(_)
            ),
            "cap exhaustion is the payload's nature — permanent"
        );

        // A genuine worker-fs fault carries an errno.
        let fs_chain: anyhow::Error = anyhow::Error::from(rio_nix::nar::NarError::Io(
            std::io::Error::from_raw_os_error(libc::ENOSPC),
        ))
        .context("restoring NAR to /out");
        assert!(
            matches!(classify_restore_error(fs_chain), FetchError::Transient(_)),
            "ENOSPC during restore is the worker's disk — transient, \
             like the identical fault during download"
        );

        // NAR structure errors are deterministic for these bytes.
        let decode_chain: anyhow::Error =
            anyhow::Error::from(rio_nix::nar::NarError::InvalidMagic("nope".into()))
                .context("restoring NAR to /out");
        assert!(
            matches!(
                classify_restore_error(decode_chain),
                FetchError::PermanentForCandidate(_)
            ),
            "bad NAR bytes are permanent for the candidate"
        );
    }

    /// The no-roots https predicate classifies at the producing
    /// statement: with no CA roots in the sandbox no https attempt can
    /// ever verify a certificate, so the send failure is permanent for
    /// the candidate (one attempt, no backoff ladder); the same
    /// transport failure WITH roots available stays transient.
    // r[verify fetcher.fetchurl.permanence-at-source+3]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn https_without_roots_is_permanent_for_candidate() {
        // A listener that accepts and immediately closes: any https
        // handshake against it dies at the transport layer.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                drop(sock);
            }
        });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("https://{addr}/x"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("https-out");
        let candidates = p.candidates();
        let candidate = &candidates[0];

        // No CA bundle anywhere: build with the no-trust config.
        let (client, roots) = build_client(Path::new("/nonexistent/ca-bundle.crt")).unwrap();
        assert!(!roots, "test premise: no roots loaded");

        let err = try_fetch_one(&client, candidate, &p, false)
            .await
            .expect_err("https with no roots must fail");
        assert!(
            matches!(err, FetchError::PermanentForCandidate(_)),
            "no-roots https is deterministic for the candidate: {:#}",
            err.into_inner()
        );

        // Same failure with roots claimed available: transient (the
        // transport may genuinely recover).
        let err = try_fetch_one(&client, candidate, &p, true)
            .await
            .expect_err("handshake against a closing socket fails");
        assert!(
            matches!(err, FetchError::Transient(_)),
            "with roots available the transport failure stays transient: {:#}",
            err.into_inner()
        );
    }

    /// The 1× aggregate pin (round-16 bug_052): an unpack attempt's
    /// download AND restore charge ONE budget. The payload here fits
    /// the cap in each phase ALONE (64 KiB body, 64 KiB restored, cap
    /// 1.5× body) — under the old two-meter shape both phases passed
    /// and the attempt moved 2× body bytes; under the shared budget
    /// the restore crosses the aggregate mid-stream and the attempt
    /// fails as typed permanent exhaustion, attempted exactly once.
    // r[verify fetcher.fetchurl.transfer-cap+2]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpack_phases_share_one_attempt_budget() {
        use axum::{Router, routing::get};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        // A plain (non-xz) NAR payload: the body bytes and the restored
        // bytes are the same size, so each phase alone is under the
        // cap while their sum is not.
        let inner = tempfile::tempdir().unwrap();
        std::fs::write(inner.path().join("file"), vec![0x5a_u8; 64 * 1024]).unwrap();
        let nar = rio_nix::nar::dump_path(&inner.path().join("file")).unwrap();
        let body_len = nar.len() as u64;

        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/payload.nar",
            get(move || {
                let h = h.clone();
                let body = nar.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    body
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/payload.nar"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("agg-out");
        p.unpack = true;
        // Each phase fits alone; the aggregate does not.
        p.transfer_cap = body_len + body_len / 2;
        let err = fetch(&p).await.expect_err("aggregate over-cap must fail");
        assert!(
            format!("{err:#}").contains("transfer cap"),
            "typed exhaustion names the cap: {err:#}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "aggregate exhaustion is permanent for the candidate — one attempt"
        );
        assert!(!p.output.exists(), "no half-restored tree left behind");
    }

    /// A 404 candidate is attempted exactly ONCE: the closed
    /// permanence enum routes it to `PermanentForCandidate`, which
    /// skips the candidate's remaining attempts (no backoff burned, no
    /// useless re-requests against an answer that cannot change).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_status_hits_candidate_once() {
        use axum::{Router, routing::get};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/gone",
            get(move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::NOT_FOUND
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let out_dir = tempfile::tempdir().unwrap();
        let mut p = params(&format!("http://{addr}/gone"), &[]);
        p.mirrors.clear();
        p.output = out_dir.path().join("gone-out");
        let err = fetch(&p).await.expect_err("404 must fail the fetch");
        assert!(format!("{err:#}").contains("HTTP 404"), "{err:#}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a permanent status must not be re-attempted against the same candidate"
        );
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
        let err = try_fetch_one(&client, &origin(&p.url), &p, roots)
            .await
            .expect_err("https without roots must fail")
            .into_inner();
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

        finalize_output(&tmp, &p, TransferMeter::new("download", p.transfer_cap))
            .await
            .unwrap();
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
