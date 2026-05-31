use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result};
use atrium_api::app::bsky::feed::post::{RecordData, ReplyRefData};
use atrium_api::com::atproto::repo::strong_ref::MainData;
use atrium_api::types::string::Datetime;
use bsky_sdk::BskyAgent;
use bsky_sdk::agent::config::Config;

use crate::config::BlueskyConfig;

const BLUESKY_CHAR_LIMIT: usize = 300;

pub struct BlueskyPublisher {
    agent: BskyAgent,
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
        Ok(Self { agent })
    }
}

impl super::Publisher for BlueskyPublisher {
    fn platform(&self) -> &str {
        "bluesky"
    }

    fn publish(
        &self,
        text: &str,
        source_url: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let status = super::format_post(text, source_url);
        Box::pin(async move {
            let chunks = super::split_into_chunks(&status, BLUESKY_CHAR_LIMIT);
            let mut root_ref: Option<(String, atrium_api::types::string::Cid)> = None;
            let mut parent_ref: Option<(String, atrium_api::types::string::Cid)> = None;

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

                let response = self
                    .agent
                    .create_record(RecordData {
                        created_at: Datetime::now(),
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
                    .context("failed to post to Bluesky")?;

                let uri = response.uri.clone();
                let cid = response.cid.clone();

                if root_ref.is_none() {
                    root_ref = Some((uri.clone(), cid.clone()));
                }
                parent_ref = Some((uri, cid));
            }

            Ok(())
        })
    }
}

