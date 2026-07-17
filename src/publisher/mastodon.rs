use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use megalodon::megalodon::{PostStatusInputOptions, PostStatusOutput};
use megalodon::{self, Megalodon};

use crate::config::MastodonConfig;

pub struct MastodonPublisher {
    client: Box<dyn Megalodon + Send + Sync>,
    char_limit: usize,
    max_per_run: usize,
    min_interval_secs: u64,
}

impl MastodonPublisher {
    pub async fn new(config: &MastodonConfig) -> Result<Self> {
        let client = megalodon::generator(
            megalodon::SNS::Mastodon,
            config.instance_url.clone(),
            Some(config.access_token.clone()),
            None,
        )
        .context("failed to create Mastodon client")?;

        let instance = client
            .get_instance()
            .await
            .context("failed to query Mastodon instance info")?;

        let char_limit = instance.json.configuration.statuses.max_characters as usize;

        Ok(Self {
            client,
            char_limit,
            max_per_run: config.max_per_run,
            min_interval_secs: config.min_interval_secs,
        })
    }
}

impl super::Publisher for MastodonPublisher {
    fn platform(&self) -> &str {
        "mastodon"
    }

    fn max_per_run(&self) -> Option<usize> {
        Some(self.max_per_run)
    }

    fn min_interval_secs(&self) -> u64 {
        self.min_interval_secs
    }

    fn publish(
        &self,
        text: &str,
        source_url: Option<&str>,
        _original_timestamp: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let full_text = super::format_post(text, source_url);
        let char_limit = self.char_limit;

        Box::pin(async move {
            let (spoiler, body) = parse_cw(&full_text);

            let chunks = super::split_into_chunks(&body, char_limit);
            let mut reply_to: Option<String> = None;

            for chunk in &chunks {
                let mut options = PostStatusInputOptions {
                    spoiler_text: spoiler.clone(),
                    in_reply_to_id: reply_to.clone(),
                    ..Default::default()
                };
                if reply_to.is_some() {
                    options.spoiler_text = None;
                }

                let response = self
                    .client
                    .post_status(chunk.clone(), Some(&options))
                    .await
                    .context("failed to post to Mastodon")?;

                let post_id = match response.json {
                    PostStatusOutput::Status(status) => status.id,
                    PostStatusOutput::ScheduledStatus(scheduled) => scheduled.id,
                };
                reply_to = Some(post_id);
            }

            Ok(())
        })
    }
}

fn parse_cw(text: &str) -> (Option<String>, String) {
    if let Some(rest) = text.strip_prefix("CW: ") {
        if let Some(newline_pos) = rest.find('\n') {
            let spoiler = rest[..newline_pos].trim().to_string();
            let body = rest[newline_pos + 1..].trim().to_string();
            return (Some(spoiler), body);
        }
    }
    (None, text.to_string())
}

