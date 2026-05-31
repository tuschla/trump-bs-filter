mod claude;
mod config;
mod fetcher;
mod publisher;
mod storage;
mod transformer;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info};

use publisher::Publisher;
use transformer::Pipeline;

#[derive(Parser)]
#[command(name = "non-violent-trump", about = "Rewrite Trump's truths in kinder language")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Fetch, transform, and publish once
    Once,
    /// Run continuously, polling at the configured interval
    Daemon,
    /// Fetch and transform once without publishing
    DryRun,
    /// Publish all unpublished rewrites
    Publish,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "non_violent_trump=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let config = config::Config::load(&cli.config)?;
    let storage = storage::Storage::open(&config.storage.db_path).await?;
    let http = reqwest::Client::new();

    let prompts_dir = Path::new(&config.prompts.dir);
    let all_pipelines = transformer::load_all(prompts_dir)?;

    let pipelines: Vec<&Pipeline> = if config.prompts.active.is_empty() {
        all_pipelines.iter().collect()
    } else {
        all_pipelines
            .iter()
            .filter(|p| config.prompts.active.contains(&p.name))
            .collect()
    };

    info!(
        "active pipelines: {:?}",
        pipelines.iter().map(|p| &p.name).collect::<Vec<_>>()
    );

    match cli.command {
        Command::Once => {
            fetch_and_transform(&http, &config, &storage, &pipelines).await?;
            let publishers = create_publishers(&config).await?;
            publish_pending(&storage, &pipelines, &publishers).await?;
        }
        Command::Daemon => {
            let interval = Duration::from_secs(config.feed.poll_interval_secs);
            let publishers = create_publishers(&config).await?;
            info!("starting daemon, polling every {}s", interval.as_secs());
            loop {
                if let Err(e) = fetch_and_transform(&http, &config, &storage, &pipelines).await {
                    error!("fetch/transform cycle failed: {e:#}");
                }
                if let Err(e) = publish_pending(&storage, &pipelines, &publishers).await {
                    error!("publish cycle failed: {e:#}");
                }
                tokio::time::sleep(interval).await;
            }
        }
        Command::DryRun => {
            fetch_and_transform(&http, &config, &storage, &pipelines).await?;
        }
        Command::Publish => {
            let publishers = create_publishers(&config).await?;
            publish_pending(&storage, &pipelines, &publishers).await?;
        }
    }

    Ok(())
}

async fn fetch_and_transform(
    http: &reqwest::Client,
    config: &config::Config,
    storage: &storage::Storage,
    pipelines: &[&Pipeline],
) -> Result<()> {
    let truths = fetcher::fetch_truths(http, &config.feed.url).await?;
    info!("fetched {} truths from feed", truths.len());

    for truth in &truths {
        if storage.truth_exists(&truth.id).await? {
            continue;
        }

        let published = truth.published.map(|d| d.to_rfc3339());
        storage
            .insert_truth(
                &truth.id,
                &truth.content,
                truth.url.as_deref(),
                published.as_deref(),
            )
            .await?;
        info!("stored new truth: {}", truth.id);

        for pipeline in pipelines {
            let memory = storage
                .get_recent_rewrites(&pipeline.name, transformer::MEMORY_EXAMPLE_COUNT)
                .await?;
            match pipeline.run(&truth.content, &memory).await {
                Ok(rewritten) => {
                    storage
                        .insert_rewrite(&truth.id, &pipeline.name, &rewritten)
                        .await?;
                    info!("transformed [{}]: {}", pipeline.name, truth.id);
                }
                Err(e) => {
                    error!(
                        "transform [{}] failed for {}: {e:#}",
                        pipeline.name, truth.id
                    );
                }
            }
        }
    }

    Ok(())
}

async fn create_publishers(config: &config::Config) -> Result<Vec<Box<dyn Publisher>>> {
    let mut publishers: Vec<Box<dyn Publisher>> = Vec::new();

    if let Some(ref mastodon_config) = config.publishers.mastodon {
        publishers.push(Box::new(
            publisher::mastodon::MastodonPublisher::new(mastodon_config).await?,
        ));
    }

    if let Some(ref bluesky_config) = config.publishers.bluesky {
        publishers.push(Box::new(
            publisher::bluesky::BlueskyPublisher::new(bluesky_config).await?,
        ));
    }

    if publishers.is_empty() {
        info!("no publishers configured, skipping");
    }

    Ok(publishers)
}

async fn publish_pending(
    storage: &storage::Storage,
    pipelines: &[&Pipeline],
    publishers: &[Box<dyn Publisher>],
) -> Result<()> {
    for pipeline in pipelines {
        for p in publishers {
            let unpublished = storage
                .get_unpublished(&pipeline.name, p.platform())
                .await?;
            for rewrite in &unpublished {
                match p
                    .publish(&rewrite.content, rewrite.source_url.as_deref())
                    .await
                {
                    Ok(()) => {
                        storage
                            .mark_published(&rewrite.truth_id, &pipeline.name, p.platform())
                            .await?;
                        info!(
                            "published [{}] to {}: {}",
                            pipeline.name,
                            p.platform(),
                            rewrite.truth_id
                        );
                    }
                    Err(e) => {
                        error!(
                            "publish [{}] to {} failed for {}: {e:#}",
                            pipeline.name,
                            p.platform(),
                            rewrite.truth_id
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
