//! Configuration file utilities for Rio Build CLI

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

/// Get default config path (~/.config/rio/config.toml)
pub fn default_config_path() -> Result<Utf8PathBuf> {
    let config_dir = dirs::config_dir().context("Failed to determine config directory")?;

    let config_dir = Utf8PathBuf::try_from(config_dir)
        .map_err(|e| anyhow::anyhow!("Config directory path is not valid UTF-8: {:?}", e))?;

    Ok(config_dir.join("rio").join("config.toml"))
}

/// Load config file contents as string (returns None if file doesn't exist)
pub fn load_config_file(path: &Utf8Path) -> Result<Option<String>> {
    if !path.exists() {
        tracing::debug!("No config file found at: {}", path);
        return Ok(None);
    }

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    Ok(Some(contents))
}

/// Create default config file at ~/.config/rio/config.toml
pub fn create_default_config() -> Result<Utf8PathBuf> {
    let config_path = default_config_path()?;
    let config_dir = config_path
        .parent()
        .context("Config path has no parent directory")?;

    // Create directory
    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("Failed to create config directory: {}", config_dir))?;

    // Default config content
    let default_toml = r#"# Rio Build Configuration
#
# Seed agents for cluster discovery
# The CLI will connect to these agents to discover the cluster and find the leader
seed_agents = [
    "http://localhost:50051",
]
"#;

    std::fs::write(&config_path, default_toml)
        .with_context(|| format!("Failed to write config file: {}", config_path))?;

    tracing::info!("Created default config at: {}", config_path);

    Ok(config_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn test_load_existing_file() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;
        let config_file = temp_path.join("config.toml");

        std::fs::write(&config_file, "test content")?;

        let contents = load_config_file(&config_file)?;
        assert_eq!(contents, Some("test content".to_string()));
        Ok(())
    }

    #[test]
    fn test_load_missing_file_returns_none() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;
        let nonexistent = temp_path.join("missing.toml");

        let result = load_config_file(&nonexistent)?;
        assert_eq!(result, None);
        Ok(())
    }

    #[test]
    fn test_create_default_config() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        // set_var is unsafe in Rust 2024
        unsafe {
            std::env::set_var("HOME", temp_path.as_str());
        }

        let config_path = create_default_config()?;
        assert!(config_path.exists());

        let contents = std::fs::read_to_string(&config_path)?;
        assert!(contents.contains("seed_agents"));
        Ok(())
    }

    #[test]
    fn test_default_config_path() -> anyhow::Result<()> {
        let path = default_config_path()?;
        assert!(path.ends_with("rio/config.toml"));
        Ok(())
    }
}
