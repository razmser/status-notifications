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
#[serde(deny_unknown_fields)]
pub struct Feed {
    pub name: String,
    pub url: String,
}

/// The `doh` config value: either a bool toggle (`doh = true` → default
/// endpoint) or an explicit DoH-JSON endpoint URL (`doh = "https://.../dns-query"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Doh {
    Enabled(bool),
    Endpoint(String),
}

/// Daemon configuration loaded from `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_age_minutes")]
    pub max_age_minutes: i64,
    /// Optional DNS-over-HTTPS resolution for feed hostnames. `doh = true` uses
    /// the default endpoint; `doh = "https://host/dns-query"` overrides it.
    /// Useful when local DNS is hijacked (e.g. a fake-IP VPN). Omit to use the
    /// system resolver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doh: Option<Doh>,
    #[serde(default)]
    pub feeds: Vec<Feed>,
}

impl Config {
    /// Resolve the configured DoH endpoint URL, if DoH is enabled.
    ///
    /// - absent or `doh = false` → `None` (use the system resolver)
    /// - `doh = true` → the default endpoint
    /// - `doh = "<url>"` → that endpoint
    pub fn doh_endpoint(&self) -> Option<String> {
        match &self.doh {
            None | Some(Doh::Enabled(false)) => None,
            Some(Doh::Enabled(true)) => Some(crate::doh::DEFAULT_DOH_ENDPOINT.to_string()),
            Some(Doh::Endpoint(url)) => Some(url.clone()),
        }
    }
}

/// The built-in default configuration (three default feeds).
pub fn default_config() -> Config {
    Config {
        poll_interval_secs: default_poll_interval_secs(),
        max_age_minutes: default_max_age_minutes(),
        doh: None,
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

/// Upper bound for `max_age_minutes`: one year (525_600 minutes). Far larger
/// than any sane age window, and small enough that `chrono::Duration::minutes`
/// and the `now - duration` arithmetic at the call sites can never overflow.
const MAX_AGE_MINUTES_CAP: i64 = 525_600;

/// Validate a loaded (user-authored) config (consistent with the
/// malformed-config-exits design).
///
/// - `poll_interval_secs` of 0 would busy-loop the poll loop, hammering feeds.
/// - `max_age_minutes < 1` would make every normal entry ineligible (the daemon
///   would run but never notify), and an extreme value would overflow
///   `chrono::Duration::minutes` at the call sites, so it must stay in
///   `1..=MAX_AGE_MINUTES_CAP`.
fn validate(config: &Config) -> Result<()> {
    if config.poll_interval_secs == 0 {
        anyhow::bail!(
            "poll_interval_secs must be >= 1 (got {})",
            config.poll_interval_secs
        );
    }
    if config.max_age_minutes < 1 || config.max_age_minutes > MAX_AGE_MINUTES_CAP {
        anyhow::bail!(
            "max_age_minutes must be between 1 and {MAX_AGE_MINUTES_CAP} (got {})",
            config.max_age_minutes
        );
    }
    if let Some(Doh::Endpoint(url)) = &config.doh
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        anyhow::bail!(
            "doh endpoint must be an http(s) URL (got {url:?}); use `doh = true` for the default"
        );
    }
    Ok(())
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
        validate(&config).with_context(|| format!("invalid config file: {}", path.display()))?;
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

    #[test]
    fn unknown_key_returns_error() {
        // A typo in a known key (missing trailing 's') must NOT be silently
        // ignored and fall back to the default — it must fail to parse loudly.
        let toml_str = r#"
            max_age_minute = 5
            [[feeds]]
            name = "OpenAI"
            url = "https://status.openai.com/feed.atom"
        "#;
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown config key must cause a parse error, not a silent default"
        );
    }

    #[test]
    fn unknown_feed_key_returns_error() {
        let toml_str = r#"
            [[feeds]]
            name = "OpenAI"
            url = "https://status.openai.com/feed.atom"
            unexpected = "x"
        "#;
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown feed key must cause a parse error, not be silently ignored"
        );
    }

    #[test]
    fn validate_rejects_zero_poll_interval() {
        let mut config = default_config();
        config.poll_interval_secs = 0;
        assert!(
            validate(&config).is_err(),
            "poll_interval_secs == 0 must be rejected (would busy-loop)"
        );
    }

    #[test]
    fn validate_accepts_default_config() {
        // The default interval (60) and max_age_minutes (10) must remain valid.
        assert!(validate(&default_config()).is_ok());
    }

    #[test]
    fn validate_rejects_zero_max_age_minutes() {
        let mut config = default_config();
        config.max_age_minutes = 0;
        assert!(
            validate(&config).is_err(),
            "max_age_minutes == 0 must be rejected (would never notify)"
        );
    }

    #[test]
    fn validate_rejects_negative_max_age_minutes() {
        let mut config = default_config();
        config.max_age_minutes = -1;
        assert!(
            validate(&config).is_err(),
            "negative max_age_minutes must be rejected (would never notify)"
        );
    }

    #[test]
    fn validate_rejects_max_age_minutes_above_cap() {
        let mut config = default_config();
        config.max_age_minutes = MAX_AGE_MINUTES_CAP + 1;
        assert!(
            validate(&config).is_err(),
            "max_age_minutes above the cap must be rejected (overflow guard)"
        );
    }

    #[test]
    fn doh_defaults_to_none_when_absent() {
        let toml_str = r#"
            [[feeds]]
            name = "OpenAI"
            url = "https://status.openai.com/feed.atom"
        "#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.doh, None);
        assert_eq!(config.doh_endpoint(), None);
    }

    #[test]
    fn doh_bool_true_uses_default_endpoint() {
        let toml_str = r#"
            doh = true
            [[feeds]]
            name = "OpenAI"
            url = "https://status.openai.com/feed.atom"
        "#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(config.doh, Some(Doh::Enabled(true)));
        assert_eq!(
            config.doh_endpoint().as_deref(),
            Some(crate::doh::DEFAULT_DOH_ENDPOINT)
        );
    }

    #[test]
    fn doh_bool_false_disables() {
        let mut config = default_config();
        config.doh = Some(Doh::Enabled(false));
        assert_eq!(config.doh_endpoint(), None);
    }

    #[test]
    fn doh_string_overrides_endpoint() {
        let toml_str = r#"
            doh = "https://8.8.8.8/resolve"
            [[feeds]]
            name = "OpenAI"
            url = "https://status.openai.com/feed.atom"
        "#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(
            config.doh,
            Some(Doh::Endpoint("https://8.8.8.8/resolve".to_string()))
        );
        assert_eq!(
            config.doh_endpoint().as_deref(),
            Some("https://8.8.8.8/resolve")
        );
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_rejects_non_url_doh_endpoint() {
        let mut config = default_config();
        config.doh = Some(Doh::Endpoint("1.1.1.1".to_string()));
        assert!(
            validate(&config).is_err(),
            "a doh endpoint without an http(s) scheme must be rejected"
        );
    }

    #[test]
    fn doh_round_trips_through_toml() {
        let mut config = default_config();
        config.doh = Some(Doh::Endpoint("https://1.1.1.1/dns-query".to_string()));
        let serialized = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&serialized).expect("parse");
        assert_eq!(config, parsed);
    }
}
