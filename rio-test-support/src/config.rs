//! `figment::Jail` config-test macros shared across all 5 binaries.
//!
//! Every binary's main.rs (or config.rs for rio-builder) carries the
//! same pair of standing-guard tests:
//!
//! - `all_subconfigs_roundtrip_toml`: write a TOML with every known
//!   sub-config table, load via `rio_common::config::load`, assert
//!   each field round-tripped.
//! - `all_subconfigs_default_when_absent`: near-empty TOML → every
//!   sub-config at its `Default` impl. Catches "new required field
//!   breaks existing deployments" (figment missing-field error).
//!
//! These catch the P0219 failure mode: a builder `with_X()` method
//! exists but `Config` has no corresponding field, so TOML-driven
//! deployments silently get the hardcoded default.
//! `config_defaults_are_stable` (the pre-existing per-crate test) is
//! STRUCTURALLY BLIND to that — it only checks fields that ARE on
//! Config, not fields that SHOULD be.
//!
//! The scaffolding (`figment::Jail::expect_with`, TOML file write,
//! config load, `Ok(())`) was 5×-duplicated. These macros keep the
//! per-field asserts at the call site — the **ADD IT HERE** edit
//! point is the entire purpose — and extract only the boilerplate
//! around them.
//!
//! ## Known limitation: ADD-IT-HERE is advisory, not enforced
//!
//! The standing-guard pair asserts that KNOWN sub-config tables
//! roundtrip; it does NOT catch a NEW sub-config you forgot to add to
//! the test. A build.rs grep-vs-TOML-literal cross-check was
//! considered and rejected (overkill for 5 crates; would itself
//! drift). When adding a `Config.newfield` of non-primitive type
//! (sub-table), you MUST update the `jail_roundtrip!` +
//! `jail_defaults!` bodies in that crate. The doc-comment is the
//! enforcement.
//!
//! # Call-site requirements
//!
//! `#[macro_export]` macros resolve paths at the CALL SITE, not the
//! definition site. The caller's crate must have these as (dev-)deps:
//! - `figment` (with `test` feature) — for `figment::Jail`
//! - `rio-common` — for `rio_common::config::load`
//!
//! All 5 binaries already satisfy this (they were using
//! `figment::Jail::expect_with` directly before this extraction).

/// Standing guard: TOML → Config roundtrip for EVERY sub-config
/// table via the REAL `rio_common::config::load` path (not raw
/// figment). Jail changes cwd to a temp dir; `./{component}.toml`
/// there is picked up by load()'s `{component}.toml` layer.
///
/// When you add `Config.newfield`: ADD AN ASSERT to the `|$cfg|`
/// block or this macro's doc-comment is a lie. The companion
/// [`jail_defaults!`](crate::jail_defaults) catches "new required
/// field breaks existing deployments" (figment missing-field error).
///
/// `#[allow(result_large_err)]`: figment::Error is 208B,
/// API-fixed — the closure signature is set by the library.
///
/// ```ignore
/// jail_roundtrip!("gateway", r#"
///     [tls]
///     cert_path = "/etc/tls/cert.pem"
/// "#, |cfg: Config| {
///     assert_eq!(cfg.common.tls.cert_path.as_deref(),
///                Some(Path::new("/etc/tls/cert.pem")));
/// });
/// ```
#[macro_export]
macro_rules! jail_roundtrip {
    ($component:expr, $toml:expr, |$cfg:ident: $cfg_ty:ty| $asserts:block) => {
        #[test]
        #[allow(clippy::result_large_err)]
        fn all_subconfigs_roundtrip_toml() {
            figment::Jail::expect_with(|jail| {
                jail.create_file(concat!($component, ".toml"), $toml)?;
                let $cfg: $cfg_ty =
                    rio_common::config::load($component, <CliArgs as Default>::default()).unwrap();
                $asserts
                Ok(())
            });
        }
    };
}

/// Near-empty `{component}.toml` → every sub-config at its Default
/// impl. If `Config.foo` is added WITHOUT `#[serde(default)]` AND the
/// sub-struct lacks `impl Default`, this fails with a figment
/// missing-field error.
///
/// `$sentinel` is a single TOML line that proves the file IS loaded
/// (a truly empty file would be indistinguishable from a missing one
/// in terms of sub-config defaults). Pick any scalar field.
///
/// ```ignore
/// jail_defaults!("gateway", "drain_grace_secs = 6", |cfg: Config| {
///     assert!(!cfg.common.tls.is_configured());
///     assert!(cfg.rate_limit.is_none());
/// });
/// ```
#[macro_export]
macro_rules! jail_defaults {
    ($component:expr, $sentinel:expr, |$cfg:ident: $cfg_ty:ty| $asserts:block) => {
        #[test]
        #[allow(clippy::result_large_err)]
        fn all_subconfigs_default_when_absent() {
            figment::Jail::expect_with(|jail| {
                jail.create_file(concat!($component, ".toml"), $sentinel)?;
                let $cfg: $cfg_ty =
                    rio_common::config::load($component, <CliArgs as Default>::default()).unwrap();
                $asserts
                Ok(())
            });
        }
    };
}
