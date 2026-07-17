pub mod bluesky;
pub mod mastodon;

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

pub trait Publisher: Send + Sync {
    fn platform(&self) -> &str;
    /// Max posts per daemon cycle. None = unlimited (safe for backdating platforms).
    fn max_per_run(&self) -> Option<usize> {
        None
    }
    /// Minimum seconds between posts. 0 = no interval gate.
    fn min_interval_secs(&self) -> u64 {
        0
    }
    fn publish(
        &self,
        text: &str,
        source_url: Option<&str>,
        original_timestamp: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

pub fn format_post(text: &str, source_url: Option<&str>) -> String {
    match source_url {
        Some(url) => format!("{text}\n\nSource: {url}"),
        None => text.to_string(),
    }
}

/// Split text into chunks that fit within char_limit.
/// Prefers splitting at paragraph boundaries (\n\n), then single newlines, then whitespace.
pub fn split_into_chunks(text: &str, char_limit: usize) -> Vec<String> {
    if text.chars().count() <= char_limit {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= char_limit {
            chunks.push(remaining.trim().to_string());
            break;
        }

        let byte_pos: usize = remaining
            .char_indices()
            .nth(char_limit)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let candidate = &remaining[..byte_pos];

        let split_at = candidate
            .rfind("\n\n")
            .or_else(|| candidate.rfind('\n'))
            .or_else(|| candidate.rfind(|c: char| c.is_whitespace()))
            .unwrap_or(byte_pos);

        let chunk = remaining[..split_at].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        remaining = remaining[split_at..].trim_start();
    }

    chunks
}
