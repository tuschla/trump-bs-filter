use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub feed: FeedConfig,
    pub storage: StorageConfig,
    pub prompts: PromptsConfig,
    #[serde(default)]
    pub transform: TransformConfig,
    #[serde(default)]
    pub publishers: PublishersConfig,
}

#[derive(Debug, Deserialize)]
pub struct FeedConfig {
    pub url: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

fn default_poll_interval() -> u64 {
    300
}

#[derive(Debug, Deserialize)]
pub struct StorageConfig {
    pub db_path: String,
}

#[derive(Debug, Deserialize)]
pub struct PromptsConfig {
    #[serde(default = "default_prompts_dir")]
    pub dir: String,
    #[serde(default)]
    pub active: Vec<String>,
}

fn default_prompts_dir() -> String {
    "prompts".to_string()
}

#[derive(Debug, Deserialize)]
pub struct TransformConfig {
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

impl Default for TransformConfig {
    fn default() -> Self {
        Self { concurrency: default_concurrency() }
    }
}

fn default_concurrency() -> usize {
    5
}

#[derive(Debug, Default, Deserialize)]
pub struct PublishersConfig {
    pub mastodon: Option<MastodonConfig>,
    pub bluesky: Option<BlueskyConfig>,
}

#[derive(Debug, Deserialize)]
pub struct MastodonConfig {
    pub instance_url: String,
    pub access_token: String,
}

#[derive(Debug, Deserialize)]
pub struct BlueskyConfig {
    pub handle: String,
    pub password: String,
    #[serde(default = "default_bluesky_url")]
    pub pds_url: String,
}

fn default_bluesky_url() -> String {
    "https://bsky.social".to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        toml::from_str(&content).context("failed to parse config")
    }
}
