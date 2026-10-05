use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use atrium_api::app::bsky::feed::post::{RecordData, ReplyRefData};
use atrium_api::com::atproto::repo::strong_ref::MainData;
use atrium_api::types::string::Datetime;
use bsky_sdk::BskyAgent;
use bsky_sdk::agent::config::Config;
use tracing::{error, warn};

use crate::config::BlueskyConfig;

const BLUESKY_CHAR_LIMIT: usize = 300;

pub struct BlueskyPublisher {
    agent: BskyAgent,
    handle: String,
    password: String,
    max_per_run: Option<usize>,
    min_interval_secs: u64,
}

impl BlueskyPublisher {
    pub async fn new(config: &BlueskyConfig) -> Result<Self> {
        let agent_config = Config {
            endpoint: config.pds_url.clone(),
            ..Config::default()
        };
        let agent = BskyAgent::builder()
            .config(agent_config)
            .build()
            .await
            .context("failed to build Bluesky agent")?;
        agent
            .login(&config.handle, &config.password)
            .await
            .context("failed to login to Bluesky")?;
        Ok(Self {
            agent,
            handle: config.handle.clone(),
            password: config.password.clone(),
            max_per_run: config.max_per_run,
            min_interval_secs: config.min_interval_secs,
        })
    }

    /// Log in again if the agent holds no session. When a token refresh is
    /// rejected (e.g. 401 AuthMissing) the SDK drops the session for good, and
    /// every later call fails with "not logged in" until the process restarts:
    /// that stalled publishing for six days on 2026-09-28.
    async fn ensure_session(&self) -> Result<()> {
        if self.agent.get_session().await.is_none() {
            warn!("Bluesky session lost, logging in again");
            self.agent
                .login(&self.handle, &self.password)
                .await
                .context("failed to re-login to Bluesky")?;
        }
        Ok(())
    }

    /// Remove records already created for a thread whose later chunks failed.
    ///
    /// Reverse order so a reader never sees a reply whose parent is gone.
    async fn rollback(&self, uris: &[String]) {
        for uri in uris.iter().rev() {
            match self.agent.delete_record(uri).await {
                Ok(_) => warn!("rolled back partial Bluesky thread post {uri}"),
                Err(e) => {
                    error!("orphaned partial Bluesky post {uri}, delete failed: {e}")
                }
            }
        }
    }
}

impl super::Publisher for BlueskyPublisher {
    fn platform(&self) -> &str {
        "bluesky"
    }

    fn max_per_run(&self) -> Option<usize> {
        self.max_per_run
    }

    fn min_interval_secs(&self) -> u64 {
        self.min_interval_secs
    }

    fn publish(
        &self,
        text: &str,
        source_url: Option<&str>,
        original_timestamp: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let status = super::format_post(text, source_url);
        let timestamp = original_timestamp
            .and_then(|ts| ts.parse::<Datetime>().ok())
            .unwrap_or_else(Datetime::now);
        Box::pin(async move {
            self.ensure_session().await?;
            let chunks = super::split_into_chunks(&status, BLUESKY_CHAR_LIMIT);
            let mut root_ref: Option<(String, atrium_api::types::string::Cid)> = None;
            let mut parent_ref: Option<(String, atrium_api::types::string::Cid)> = None;
            // URIs of chunks already accepted, so a mid-thread failure can be undone.
            let mut posted: Vec<String> = Vec::new();

            for chunk in &chunks {
                let reply = parent_ref.as_ref().map(|(parent_uri, parent_cid)| {
                    let (root_uri, root_cid) = root_ref.as_ref().unwrap();
                    ReplyRefData {
                        root: MainData {
                            uri: root_uri.clone(),
                            cid: root_cid.clone(),
                        }
                        .into(),
                        parent: MainData {
                            uri: parent_uri.clone(),
                            cid: parent_cid.clone(),
                        }
                        .into(),
                    }
                    .into()
                });

                let response = match self
                    .agent
                    .create_record(RecordData {
                        created_at: timestamp.clone(),
                        text: chunk.clone(),
                        embed: None,
                        entities: None,
                        facets: None,
                        labels: None,
                        langs: None,
                        reply,
                        tags: None,
                    })
                    .await
                {
                    Ok(response) => response,
                    Err(e) => {
                        // Half a thread would otherwise sit on the account forever:
                        // the publication is never marked, so the next cycle posts
                        // the whole thread again. Undo what landed, then fail.
                        self.rollback(&posted).await;
                        return Err(anyhow::Error::new(e).context("failed to post to Bluesky"));
                    }
                };

                let uri = response.uri.clone();
                let cid = response.cid.clone();
                posted.push(uri.clone());

                if root_ref.is_none() {
                    root_ref = Some((uri.clone(), cid.clone()));
                }
                parent_ref = Some((uri, cid));
            }

            Ok(())
        })
    }
}
