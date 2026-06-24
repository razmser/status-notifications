use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

fn default_poll_interval_secs() -> u64 {
    60
}

fn default_max_age_minutes() -> i64 {
    10
}

/// A single status-page feed to poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub name: String,
    pub url: String,
}

/// Daemon configuration loaded from `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_age_minutes")]
    pub max_age_minutes: i64,
    #[serde(default)]
    pub feeds: Vec<Feed>,
}

/// The built-in default configuration (three default feeds).
pub fn default_config() -> Config {
    Config {
        poll_interval_secs: default_poll_interval_secs(),
        max_age_minutes: default_max_age_minutes(),
        feeds: vec![
            Feed {
                name: "OpenAI".to_string(),
                url: "https://status.openai.com/feed.atom".to_string(),
            },
            Feed {
                name: "Claude".to_string(),
                url: "https://status.claude.com/history.atom".to_string(),
            },
            Feed {
                name: "DeepSeek".to_string(),
                url: "https://status.deepseek.com/feed.atom".to_string(),
            },
        ],
    }
}

/// Resolve the application's config directory under macOS Application Support.
fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "status-notifications")
        .context("could not determine config directory")
}

/// Path to the config directory (`~/Library/Application Support/status-notifications/`).
pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// Path to `config.toml`.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Path to `seen.json` (the seen-store state file).
pub fn seen_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("seen.json"))
}

/// Load the config from `config.toml`, creating it with defaults if missing.
///
/// - Missing file: create the config dir, write serialized defaults, return defaults.
/// - Present but malformed: return an error (the caller logs and exits non-zero).
pub fn load_or_create() -> Result<Config> {
    let path = config_path()?;
    if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        Ok(config)
    } else {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create config dir: {}", dir.display()))?;
        let config = default_config();
        let serialized =
            toml::to_string_pretty(&config).context("failed to serialize default config")?;
        std::fs::write(&path, serialized)
            .with_context(|| format!("failed to write default config: {}", path.display()))?;
        log::info!("created default config at {}", path.display());
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let config = default_config();
        let serialized = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&serialized).expect("parse");
        assert_eq!(config, parsed);
    }

    #[test]
    fn minimal_toml_applies_defaults() {
        let toml_str = r#"
            [[feeds]]
            name = "OpenAI"
            url = "https://status.openai.com/feed.atom"
        "#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(config.max_age_minutes, 10);
        assert_eq!(config.feeds.len(), 1);
        assert_eq!(config.feeds[0].name, "OpenAI");
    }

    #[test]
    fn malformed_toml_returns_error() {
        let toml_str = "this is = = not valid toml [[[";
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }
}
