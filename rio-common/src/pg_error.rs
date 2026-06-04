//! Shared transient-vs-permanent PostgreSQL error classifier.
//!
//! Every PG consumer used to hand-roll its own retry filter, and the
//! same defect kept reappearing in different shapes: the migrate
//! runner's `Io|PoolTimedOut`-only filter burned a Job pod on a
//! `57P03 cannot_connect_now` FATAL during bitnami startup, while the
//! controller's retry-everything loop warn-retried *permanent* config
//! errors forever with the pod Running/Ready. One classifier, three
//! consumers (migrate runner, scheduler connect, controller connect).
//!
//! Classification:
//! - **Transient** — `Io` (refused/reset/EOF), `PoolTimedOut`, `Tls`,
//!   and `Database` errors in SQLSTATE class `57P*`
//!   (operator_intervention: `57P01` admin_shutdown, `57P02`
//!   crash_shutdown, `57P03` cannot_connect_now — PG restarts,
//!   Aurora Serverless resume) and class `53*`
//!   (insufficient_resources, e.g. `53300` too_many_connections under
//!   Aurora ACU pressure).
//! - **Permanent** — everything else: auth (`28*`), syntax/undefined
//!   object (`42*`), invalid catalog name (`3D000` — see below), …
//!
//! `Tls`-as-transient is a deliberate trade-off: it covers the Aurora
//! Serverless resume RST shape, but it also means a bad sslrootcert
//! path in a password-mode URL retries until the caller's deadline
//! instead of failing fast (visible in warn logs each attempt). IAM
//! mode is unaffected — `TokenSource::new` preflights the rootcert at
//! construction.
//!
//! `3D000` (database does not exist) is transient ONLY in
//! [`is_transient_bounded`] contexts: a fresh k3s install's bitnami
//! init can briefly expose a server whose application database is not
//! yet created, and the migrate runner's bounded poll should ride
//! that out. In an UNBOUNDED loop (controller connect) it is
//! permanent — a database-name typo parses fine, passes the
//! TokenSource preflight, and must not warn-retry forever with the
//! pod Running/Ready.

/// Transient under any retry policy. See the module docs for the
/// class table and the `Tls` trade-off.
pub fn is_transient(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::Tls(_) => true,
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|c| c.starts_with("57P") || c.starts_with("53")),
        _ => false,
    }
}

/// [`is_transient`] plus `3D000` (invalid_catalog_name). ONLY for
/// retry loops with a hard deadline (the migrate runner's bounded
/// poll): the bitnami-init window where the database does not exist
/// yet resolves itself, but a database-name typo never does — bounded
/// callers surface it at deadline exhaustion, unbounded callers would
/// hide it forever.
pub fn is_transient_bounded(e: &sqlx::Error) -> bool {
    is_transient(e)
        || matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("3D000"))
}

/// [`is_transient`] lifted over `anyhow::Error` for connect closures
/// that mix sqlx and token-mint failures. Non-sqlx errors (an RDS IAM
/// token mint hiccup — STS blip, IRSA token rotation race) classify
/// as TRANSIENT: they ride the same backoff as a transient PG error.
pub fn is_transient_anyhow(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<sqlx::Error>() {
        Some(sql) => is_transient(sql),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    /// Minimal `DatabaseError` carrying only a SQLSTATE — the
    /// classifier reads nothing else.
    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "SQLSTATE {}", self.0)
        }
    }
    impl std::error::Error for FakeDbError {}
    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "fake"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    fn db_err(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code)))
    }

    #[test]
    fn classifier_table() {
        // Transient: reachability, PG lifecycle, resource pressure.
        for e in [
            sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            sqlx::Error::PoolTimedOut,
            db_err("57P01"), // admin_shutdown
            db_err("57P03"), // cannot_connect_now (bitnami startup)
            db_err("53300"), // too_many_connections (Aurora ACU pressure)
        ] {
            assert!(is_transient(&e), "{e:?} must be transient");
        }
        // Permanent: auth, syntax, undefined objects, bad catalog.
        for e in [
            db_err("28P01"), // invalid_password
            db_err("28000"), // invalid_authorization_specification
            db_err("42601"), // syntax_error
            db_err("42P01"), // undefined_table
            db_err("3D000"), // invalid_catalog_name — unbounded loops
            sqlx::Error::RowNotFound,
        ] {
            assert!(!is_transient(&e), "{e:?} must be permanent");
        }
    }

    #[test]
    fn bounded_adds_only_3d000() {
        assert!(is_transient_bounded(&db_err("3D000")));
        assert!(is_transient_bounded(&db_err("57P03")));
        assert!(!is_transient_bounded(&db_err("28P01")));
    }

    #[test]
    fn anyhow_lift_treats_non_sqlx_as_transient() {
        let mint = anyhow::anyhow!("STS hiccup");
        assert!(is_transient_anyhow(&mint));

        let perm: anyhow::Error = db_err("28P01").into();
        assert!(!is_transient_anyhow(&perm));
        let tran: anyhow::Error = db_err("57P03").into();
        assert!(is_transient_anyhow(&tran));
    }
}
