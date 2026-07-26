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
    /// "truthsocial" polls Truth Social's API directly (near-zero mirror lag);
    /// anything else falls back to the trumpstruth.org RSS feed at `url`.
    #[serde(default = "default_source")]
    pub source: String,
    /// Truth Social account id to poll when source = "truthsocial".
    #[serde(default)]
    pub truthsocial_account_id: Option<String>,
    /// Only ingest posts strictly newer than this RFC3339 instant. Guards the
    /// source switchover: posts already in the DB under trumpstruth ids must not
    /// re-enter under Truth Social ids and double-publish.
    #[serde(default)]
    pub cutover_rfc3339: Option<String>,
}

fn default_poll_interval() -> u64 {
    300
}

fn default_source() -> String {
    "trumpstruth".to_string()
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
    /// Max posts per daemon cycle. Mastodon can't backdate, so the historical
    /// backlog must be drip-fed instead of dumped. Defaults to 1.
    #[serde(default = "default_mastodon_max_per_run")]
    pub max_per_run: usize,
    /// Minimum seconds between Mastodon posts, independent of poll interval.
    /// 0 = no interval gate (rate limited by max_per_run + poll cadence only).
    #[serde(default)]
    pub min_interval_secs: u64,
}

fn default_mastodon_max_per_run() -> usize {
    1
}

#[derive(Debug, Deserialize)]
pub struct BlueskyConfig {
    pub handle: String,
    pub password: String,
    #[serde(default = "default_bluesky_url")]
    pub pds_url: String,
    /// Max posts per daemon cycle. Bluesky backdates correctly, so the timeline
    /// is fine at any rate — but a fresh account creating thousands of records
    /// fast trips spam heuristics and hits create rate limits. None = unlimited.
    #[serde(default)]
    pub max_per_run: Option<usize>,
    /// Minimum seconds between posts. 0 = no interval gate.
    #[serde(default)]
    pub min_interval_secs: u64,
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
