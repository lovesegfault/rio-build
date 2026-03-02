//! Ephemeral PostgreSQL bootstrap for integration tests.
//!
//! On first use, [`TestDb::new`] bootstraps a process-global postgres server
//! (via `initdb` + spawning `postgres` as a child process) using binaries
//! provided by the Nix dev shell. Each [`TestDb`] then creates an isolated
//! database on that shared server, runs the given migrations, and drops the
//! database on `Drop`.
//!
//! If `DATABASE_URL` is set, the bootstrap is skipped and that URL is used as
//! the admin connection instead (for debugging against a persistent PG).
//!
//! No silent skips: if postgres can't be found or started, tests **panic**.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use tempfile::TempDir;

/// Process-global postgres server. Lazily initialized on first [`TestDb::new`].
static PG: OnceLock<PgServer> = OnceLock::new();

enum PgServer {
    /// Ephemeral server we bootstrapped ourselves. The child process dies
    /// when the test binary exits (PR_SET_PDEATHSIG on Linux). The tempdir
    /// lives in a static so `TempDir::drop` NEVER runs — cleanup happens
    /// via [`gc_stale_dirs`] on the next bootstrap.
    Ephemeral {
        _tempdir: TempDir,
        _child: Child,
        admin_url: String,
    },
    /// External server via DATABASE_URL override.
    External { admin_url: String },
}

impl PgServer {
    fn get() -> &'static Self {
        PG.get_or_init(|| match std::env::var("DATABASE_URL") {
            Ok(url) => {
                eprintln!("[rio-test-support] using external postgres from DATABASE_URL");
                Self::External { admin_url: url }
            }
            Err(_) => Self::bootstrap(),
        })
    }

    fn admin_url(&self) -> &str {
        match self {
            Self::Ephemeral { admin_url, .. } | Self::External { admin_url } => admin_url,
        }
    }

    fn bootstrap() -> Self {
        // Reclaim /tmp space from dead prior test processes before adding
        // our own dir. Must run before tempdir creation — see gc_stale_dirs
        // for the liveness protocol.
        gc_stale_dirs();

        // Short tempdir path: Unix socket paths are limited to 108 bytes on
        // Linux. In the Nix sandbox $TMPDIR can be long (/build/source/...).
        // Force /tmp explicitly.
        let tempdir = tempfile::Builder::new()
            .prefix("rio-pg-")
            .tempdir_in("/tmp")
            .expect("failed to create tempdir for postgres");

        // Write our PID immediately so concurrent gc_stale_dirs scanners
        // recognize this dir as live (even before initdb finishes).
        std::fs::write(
            tempdir.path().join("owner.pid"),
            std::process::id().to_string(),
        )
        .expect("failed to write owner.pid");

        let pgdata = tempdir.path().join("data");
        let sockdir = tempdir.path().join("sock");
        std::fs::create_dir(&sockdir).expect("failed to create socket dir");

        let pg_bin = find_pg_bin();
        let initdb = pg_bin.join("initdb");
        let postgres = pg_bin.join("postgres");

        // initdb
        let out = Command::new(&initdb)
            .arg("-D")
            .arg(&pgdata)
            .args([
                "--encoding=UTF8",
                "--locale=C",
                "-U",
                "postgres",
                "--auth=trust",
            ])
            .output()
            .unwrap_or_else(|e| {
                panic!(
                    "failed to run initdb at {initdb:?}: {e}.\n\
                     Postgres binaries not found — run inside `nix develop` or set PG_BIN."
                )
            });
        if !out.status.success() {
            panic!(
                "initdb failed ({}):\nstdout:\n{}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // Append test-tuned settings. Unix-socket-only: no TCP listener means
        // no port races and matches the Nix sandbox default.
        let conf_path = pgdata.join("postgresql.conf");
        let mut conf = std::fs::read_to_string(&conf_path).expect("failed to read postgresql.conf");
        writeln!(conf).unwrap();
        writeln!(conf, "listen_addresses = ''").unwrap();
        writeln!(conf, "unix_socket_directories = '{}'", sockdir.display()).unwrap();
        // Durability off — tests don't care, huge speedup.
        writeln!(conf, "fsync = off").unwrap();
        writeln!(conf, "synchronous_commit = off").unwrap();
        writeln!(conf, "full_page_writes = off").unwrap();
        writeln!(conf, "max_connections = 100").unwrap();
        std::fs::write(&conf_path, conf).expect("failed to write postgresql.conf");

        // Spawn postgres as a direct child. On Linux, PR_SET_PDEATHSIG ensures
        // the child gets SIGTERM when the test process exits, so we don't need
        // explicit shutdown machinery.
        let mut cmd = Command::new(&postgres);
        cmd.arg("-D").arg(&pgdata).stdout(Stdio::null());

        // Capture stderr for early-failure diagnostics, but detach it after
        // startup so postgres doesn't block on a full pipe later.
        let log_path = tempdir.path().join("pg.log");
        let log_file = std::fs::File::create(&log_path).expect("failed to create pg.log");
        cmd.stderr(log_file);

        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                nix::sys::prctl::set_pdeathsig(Some(nix::sys::signal::Signal::SIGTERM))
                    .map_err(std::io::Error::from)
            });
        }

        let child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn postgres at {postgres:?}: {e}"));

        // Wait for readiness: poll for the socket file. Postgres writes
        // .s.PGSQL.{port} in unix_socket_directories when accepting connections.
        let sock_file = sockdir.join(".s.PGSQL.5432");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !sock_file.exists() {
            if std::time::Instant::now() > deadline {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                panic!(
                    "postgres failed to start within 10s.\n\
                     Expected socket: {sock_file:?}\n\
                     Log:\n{log}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // sqlx Unix-socket URL format: host query param with socket directory.
        // Port is still needed — postgres encodes it in the socket filename.
        let admin_url = format!(
            "postgres:///postgres?host={}&user=postgres&port=5432",
            sockdir.display()
        );

        eprintln!(
            "[rio-test-support] ephemeral postgres started at {}",
            sockdir.display()
        );

        Self::Ephemeral {
            _tempdir: tempdir,
            _child: child,
            admin_url,
        }
    }
}

/// Find the directory containing `initdb`/`postgres`.
///
/// Order: `PG_BIN` env var, then `PATH` search for `initdb`.
fn find_pg_bin() -> PathBuf {
    if let Ok(p) = std::env::var("PG_BIN") {
        return PathBuf::from(p);
    }

    // Search PATH for initdb
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join("initdb");
            if candidate.is_file() {
                return PathBuf::from(dir);
            }
        }
    }

    panic!(
        "postgres binaries (initdb) not found in PATH and PG_BIN not set.\n\
         Run inside `nix develop` or set PG_BIN=/path/to/postgresql/bin"
    );
}

/// Remove stale `/tmp/rio-pg-*` directories left by dead test processes.
///
/// Because [`PG`] is a static, Rust never runs its `Drop` — the `TempDir`
/// inside is leaked on every process exit. This scanner reclaims those dirs
/// on the *next* bootstrap, bounding the leak to O(concurrent test processes).
///
/// **Liveness protocol:** each bootstrap writes its process PID to
/// `<dir>/owner.pid` immediately after creating the tempdir. A dir is stale
/// iff that PID is dead. A dir with no owner.pid is treated as an in-flight
/// bootstrap (another test process is between mkdtemp and write) and left
/// alone — reaping it races with the bootstrap's initdb and causes spurious
/// ENOENT failures. In the Nix sandbox /tmp is fresh per build, so there
/// are no pre-fix legacy dirs to worry about.
///
/// Races with concurrent scanners are benign: `remove_dir_all` on an
/// already-removed dir just returns ENOENT, which we ignore.
fn gc_stale_dirs() {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rio-pg-"))
        {
            continue;
        }
        if is_dir_stale(&path) {
            // Best-effort. May race with another scanner; ignore errors.
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// A dir is stale only when we can positively identify its owner PID AND
/// that PID is dead. Missing or unparseable owner.pid means another process
/// is mid-bootstrap (between mkdtemp and the owner.pid write completing —
/// `std::fs::write` is open(CREAT|TRUNC) then write(), so a concurrent
/// reader may see an empty file). Reaping a mid-bootstrap dir races with
/// initdb/postgres: the non-atomic remove_dir_all can delete sock/ or data/
/// while they are being populated, causing ENOENT failures.
///
/// In the Nix sandbox /tmp is fresh per build, so the only way owner.pid
/// is absent or empty is a live concurrent bootstrap.
fn is_dir_stale(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("owner.pid")) {
        Ok(s) => match s.trim().parse::<i32>() {
            Ok(pid) => !pid_alive(pid),
            // Empty or non-numeric: open()/write() race window. Leave alone.
            Err(_) => false,
        },
        // Missing: mkdtemp/write race window. Leave alone.
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: i32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // Signal 0: probe existence without actually signalling.
    // ESRCH = no such process; EPERM = exists but not ours (still alive).
    match kill(Pid::from_raw(pid), None) {
        Ok(_) | Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: i32) -> bool {
    // Conservative: assume alive on non-Linux. We never GC, but PG tests
    // only run on Linux anyway (initdb/postgres from Nix dev shell).
    true
}

/// Build a URL for the given database name from an admin URL.
///
/// Handles both TCP URLs (`postgres://host:port/old` → `.../new`) and
/// Unix-socket URLs (`postgres:///old?host=/sock` → `postgres:///new?host=/sock`).
fn replace_db_name(url: &str, new_db: &str) -> String {
    // Split off query string first so we don't mangle ?host=... paths.
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let new_base = match base.rfind('/') {
        Some(idx) => format!("{}/{}", &base[..idx], new_db),
        None => format!("{base}/{new_db}"),
    };
    match query {
        Some(q) => format!("{new_base}?{q}"),
        None => new_base,
    }
}

/// Per-test isolated database handle.
///
/// Each instance creates a fresh database on the shared [`PgServer`], runs the
/// provided migrations, and drops the database on `Drop`.
///
/// # Panics
///
/// `new()` panics if the postgres server cannot be started or the database
/// cannot be created — tests must not silently skip.
///
/// # Fault-injection contract
///
/// `Drop` opens a fresh admin connection (independent of `self.pool`), so
/// tests that close `pool` to simulate DB unavailability will still have their
/// database cleaned up. See `test_db_failure_during_completion_logged` in
/// rio-scheduler for an example.
pub struct TestDb {
    pub pool: PgPool,
    db_name: String,
    admin_url: String,
}

impl TestDb {
    /// Create an isolated test database and run migrations on it.
    pub async fn new(migrator: &Migrator) -> Self {
        let server = PgServer::get();
        let admin_url = server.admin_url().to_string();

        let db_name = format!(
            "rio_test_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        // Create the database via an admin connection.
        let admin_pool = PgPool::connect(&admin_url)
            .await
            .expect("failed to connect to test postgres admin database");
        sqlx::query(&format!(r#"CREATE DATABASE "{db_name}""#))
            .execute(&admin_pool)
            .await
            .expect("failed to create test database");
        admin_pool.close().await;

        // Connect to the new database and migrate.
        let test_url = replace_db_name(&admin_url, &db_name);
        let pool = PgPool::connect(&test_url)
            .await
            .expect("failed to connect to test database");
        migrator
            .run(&pool)
            .await
            .expect("migrations failed on test database");

        Self {
            pool,
            db_name,
            admin_url,
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Drop the database via a fresh admin connection. Runs on a separate
        // thread with its own runtime since Drop is sync.
        //
        // Intentionally does NOT use self.pool: tests may have closed it to
        // simulate DB failure. Best-effort cleanup — don't panic in Drop.
        let db_name = self.db_name.clone();
        let admin_url = self.admin_url.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let Ok(admin_pool) = PgPool::connect(&admin_url).await else {
                    return;
                };
                // Kick any lingering connections, then drop.
                let _ = sqlx::query(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
                ))
                .execute(&admin_pool)
                .await;
                let _ = sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{db_name}""#))
                    .execute(&admin_pool)
                    .await;
                admin_pool.close().await;
            });
        });
        // Best-effort: wait for cleanup so test output isn't interleaved, but
        // don't block forever.
        let _ = handle.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_db_name_tcp() {
        assert_eq!(
            replace_db_name("postgres://u:p@host:5432/old", "new"),
            "postgres://u:p@host:5432/new"
        );
    }

    #[test]
    fn test_replace_db_name_unix_socket() {
        assert_eq!(
            replace_db_name("postgres:///old?host=/tmp/sock&port=5432", "new"),
            "postgres:///new?host=/tmp/sock&port=5432"
        );
    }

    #[test]
    fn test_replace_db_name_no_query() {
        assert_eq!(
            replace_db_name("postgres://host/old", "new"),
            "postgres://host/new"
        );
    }
}
