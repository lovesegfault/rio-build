//! Create a new SQL migration and pin its checksum.
//!
//! Replaces the 4-step manual dance: create .sql → run test → copy
//! hex-SHA from panic → edit PINNED. `--repin` re-hashes an existing
//! migration after you've edited it.

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha384};
use tracing::info;

use crate::sh::repo_root;
use crate::ui;

const PINNED_FILE: &str = "rio-store/tests/migrations.rs";

#[derive(Args)]
pub struct MigrationArgs {
    /// Name of the new migration (snake_case, no number prefix).
    name: Option<String>,
    /// Re-hash an existing migration N and update its PINNED entry.
    #[arg(long, value_name = "N")]
    repin: Option<i64>,
}

pub fn run(args: MigrationArgs) -> Result<()> {
    match (args.name, args.repin) {
        (None, Some(n)) => repin(n),
        (Some(name), None) => create(&name),
        (None, None) => {
            let name = ui::text("Migration name (snake_case)?", |s| {
                if !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    Ok(())
                } else {
                    Err("must be non-empty snake_case (lowercase, digits, underscores)".into())
                }
            })?
            .ok_or_else(|| anyhow::anyhow!("specify either <name> or --repin <N>"))?;
            create(&name)
        }
        (Some(_), Some(_)) => bail!("specify either <name> or --repin <N>, not both"),
    }
}

fn create(name: &str) -> Result<()> {
    let mig_dir = repo_root().join("migrations");
    let next = std::fs::read_dir(&mig_dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|f| f.split('_').next()?.parse::<i64>().ok())
        .max()
        .unwrap_or(0)
        + 1;

    let path = mig_dir.join(format!("{next:03}_{name}.sql"));
    std::fs::write(&path, "-- TODO: write migration\n")?;
    info!("created {}", path.display());

    let checksum = hash(&path)?;
    insert_pin(next, &checksum)?;
    info!("pinned migration {next}");
    info!(
        "edit {}, then run `cargo xtask new-migration --repin {next}`",
        path.display()
    );
    Ok(())
}

fn repin(n: i64) -> Result<()> {
    let mig_dir = repo_root().join("migrations");
    let path = std::fs::read_dir(&mig_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with(&format!("{n:03}_")))
        })
        .with_context(|| format!("no migration file for {n:03}_*"))?;

    let checksum = hash(&path)?;
    update_pin(n, &checksum)?;
    info!("repinned migration {n} from {}", path.display());
    Ok(())
}

fn hash(path: &std::path::Path) -> Result<String> {
    let body = std::fs::read(path)?;
    Ok(hex::encode(Sha384::digest(&body)))
}

fn insert_pin(n: i64, checksum: &str) -> Result<()> {
    let path = repo_root().join(PINNED_FILE);
    let src = std::fs::read_to_string(&path)?;
    // PINNED is #[rustfmt::skip], so the `    ];` closer is stable.
    let needle = "\n    ];\n";
    let idx = src
        .find(needle)
        .context("PINNED table closer `];` not found")?;
    let line = format!("        ({n}, \"{checksum}\"),\n");
    let out = format!("{}{}{}", &src[..idx + 1], line, &src[idx + 1..]);
    std::fs::write(&path, out)?;
    Ok(())
}

fn update_pin(n: i64, checksum: &str) -> Result<()> {
    let path = repo_root().join(PINNED_FILE);
    let src = std::fs::read_to_string(&path)?;
    let re = regex::Regex::new(&format!(r#"\(\s*{n}\s*,\s*"[a-f0-9]+"\)"#)).unwrap();
    if !re.is_match(&src) {
        bail!("no PINNED entry for migration {n} — use `new-migration <name>` first");
    }
    let out = re.replace(&src, format!(r#"({n}, "{checksum}")"#));
    std::fs::write(&path, out.as_ref())?;
    Ok(())
}
