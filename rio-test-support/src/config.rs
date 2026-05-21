//! [`crate::Jail`] config-test macros shared across all 5 binaries.
//!
//! Every binary's main.rs (or config.rs for rio-builder) carries the
//! same pair of standing-guard tests:
//!
//! - `all_subconfigs_roundtrip_toml`: write a TOML with every known
//!   sub-config table, load via `rio_common::config::load`, assert
//!   each field round-tripped.
//! - `all_subconfigs_default_when_absent`: near-empty TOML → every
//!   sub-config at its `Default` impl. Catches "new required field
//!   breaks existing deployments" (missing-field error).
//!
//! These catch the P0219 failure mode: a builder `with_X()` method
//! exists but `Config` has no corresponding field, so TOML-driven
//! deployments silently get the hardcoded default.
//! `config_defaults_are_stable` (the pre-existing per-crate test) is
//! STRUCTURALLY BLIND to that — it only checks fields that ARE on
//! Config, not fields that SHOULD be.
//!
//! The scaffolding (`$crate::Jail::expect_with`, TOML file write,
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
//! definition site. The caller's crate must have this as a (dev-)dep:
//! - `rio-common` — for `rio_common::config::load`
//!
//! The jail itself resolves through `$crate` ([`crate::Jail`]), so
//! callers no longer need a figment dev-dependency.
//!
//! All 5 binaries already satisfy this.

/// Standing guard: TOML → Config roundtrip for EVERY sub-config
/// table via the REAL `rio_common::config::load` path. The jail
/// ([`crate::Jail`]) changes cwd to a temp dir; `./{component}.toml`
/// there is picked up by load()'s `{component}.toml` layer.
///
/// When you add `Config.newfield`: ADD AN ASSERT to the `|$cfg|`
/// block or this macro's doc-comment is a lie. The companion
/// [`jail_defaults!`](crate::jail_defaults) catches "new required
/// field breaks existing deployments" (missing-field error).
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
        fn all_subconfigs_roundtrip_toml() {
            $crate::Jail::expect_with(|jail| {
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
/// sub-struct lacks `impl Default`, this fails with a missing-field
/// error.
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
        fn all_subconfigs_default_when_absent() {
            $crate::Jail::expect_with(|jail| {
                jail.create_file(concat!($component, ".toml"), $sentinel)?;
                let $cfg: $cfg_ty =
                    rio_common::config::load($component, <CliArgs as Default>::default()).unwrap();
                $asserts
                Ok(())
            });
        }
    };
}

/// Snapshot guard: assert the committed `tests/fixtures/config-schema.json`
/// matches the live `schema_for!($ty)` + `<$ty>::default()`.
///
/// `xtask regen docs-data` reads the committed fixture (NOT the crate
/// itself) to flatten into `docs/gen/config.json` rows — keeping the
/// 5 binary crates out of xtask's dependency graph. This test is the
/// enforcement that the fixture and `Config` stay in lockstep, the
/// same role `migration_checksums_frozen` plays for shipped `.sql`.
///
/// Regenerate with:
/// ```text
/// BLESS=1 cargo nextest run -E 'test(config_schema_frozen)'
/// cargo xtask regen docs-data
/// ```
/// and commit BOTH the per-crate fixture(s) AND `docs/gen/config.json`.
///
/// Self-contained — references `::schemars` / `::serde_json` from the
/// **caller's** crate root (`#[macro_export]` macros resolve at the
/// call site). All 5 binary crates carry both as direct deps.
///
/// Read/write via runtime `CARGO_MANIFEST_DIR` (not `env!`): nextest's
/// `--workspace-remap` rewrites it to the writable workspace copy that
/// has the per-member `tests/fixtures/`; the compile-time value points
/// at the buildRustCrate source store path. `BLESS` is never set in the
/// Nix sandbox, so the write branch never runs against a read-only path.
///
/// Comparison is `serde_json::Value::==` after parsing both sides —
/// `Value::Object` is BTreeMap-backed (the workspace doesn't unify
/// `preserve_order`), so source key order never causes a false mismatch.
#[macro_export]
macro_rules! config_schema_frozen {
    ($ty:ty) => {
        #[test]
        fn config_schema_frozen() {
            let path = ::std::path::PathBuf::from(
                ::std::env::var("CARGO_MANIFEST_DIR")
                    .expect("CARGO_MANIFEST_DIR not set; run via cargo/nextest"),
            )
            .join("tests/fixtures/config-schema.json");
            let live = ::serde_json::json!({
                "schema": ::schemars::schema_for!($ty),
                "defaults": ::serde_json::to_value(<$ty>::default()).unwrap(),
            });
            if ::std::env::var_os("BLESS").is_some() {
                ::std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                ::std::fs::write(&path, ::serde_json::to_string_pretty(&live).unwrap() + "\n")
                    .unwrap();
                return;
            }
            let committed: ::serde_json::Value = ::serde_json::from_str(
                &::std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    panic!(
                        "{} missing ({e}).\nGenerate it: BLESS=1 cargo nextest run \
                         -E 'test(config_schema_frozen)'",
                        path.display()
                    )
                }),
            )
            .unwrap();
            assert_eq!(
                committed,
                live,
                "\n\nconfig schema for `{}` drifted from {}.\n\
                 The fixture is the source `xtask regen docs-data` reads — regenerate BOTH:\n  \
                 BLESS=1 cargo nextest run -E 'test(config_schema_frozen)'\n  \
                 cargo xtask regen docs-data\n\
                 then commit the fixture(s) AND docs/gen/config.json.\n",
                stringify!($ty),
                path.display(),
            );
        }
    };
}
