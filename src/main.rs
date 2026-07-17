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
use futures::stream::{self, StreamExt};
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
    /// Fetch latest truths from RSS into DB
    Fetch,
    /// Scrape full archive into DB
    Backfill,
    /// Fetch individual status pages to fill gaps backfill missed
    Recover,
    /// Strip em dashes and URLs from existing rewrites (no model calls)
    Sanitize,
    /// Transform all untransformed truths
    Transform,
    /// Publish all unpublished rewrites
    Publish,
    /// Fetch + transform + publish once
    Once,
    /// Run continuously
    Daemon,
    /// Fetch + transform without publishing
    DryRun,
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
        Command::Fetch => {
            fetch(&http, &config, &storage).await?;
        }
        Command::Backfill => {
            let count = fetcher::backfill(&http, &storage).await?;
            info!("backfill complete: {count} new truths");
        }
        Command::Recover => {
            let count = fetcher::recover_missing(&http, &storage, 5000).await?;
            info!("recover complete: {count} new truths");
        }
        Command::Sanitize => {
            let mut total = 0u64;
            for p in &all_pipelines {
                if p.keep_urls {
                    continue;
                }
                for (id, content) in storage.get_rewrites_by_style(&p.name).await? {
                    let clean = transformer::sanitize_output(&content);
                    if clean != content {
                        storage.update_rewrite_content(id, &clean).await?;
                        total += 1;
                    }
                }
            }
            info!("sanitized {total} rewrites");
        }
        Command::Transform => {
            transform(&config, &storage, &pipelines).await?;
        }
        Command::Publish => {
            let publishers = create_publishers(&config).await?;
            publish_pending(&storage, &pipelines, &publishers).await?;
        }
        Command::Once => {
            fetch(&http, &config, &storage).await?;
            transform(&config, &storage, &pipelines).await?;
            let publishers = create_publishers(&config).await?;
            publish_pending(&storage, &pipelines, &publishers).await?;
        }
        Command::Daemon => {
            let interval = Duration::from_secs(config.feed.poll_interval_secs);
            let publishers = create_publishers(&config).await?;
            info!("starting daemon, polling every {}s", interval.as_secs());
            loop {
                if let Err(e) = fetch(&http, &config, &storage).await {
                    error!("fetch failed: {e:#}");
                }
                if let Err(e) = transform(&config, &storage, &pipelines).await {
                    error!("transform failed: {e:#}");
                }
                if let Err(e) = publish_pending(&storage, &pipelines, &publishers).await {
                    error!("publish failed: {e:#}");
                }
                tokio::time::sleep(interval).await;
            }
        }
        Command::DryRun => {
            fetch(&http, &config, &storage).await?;
            transform(&config, &storage, &pipelines).await?;
        }
    }

    Ok(())
}

async fn fetch(
    http: &reqwest::Client,
    config: &config::Config,
    storage: &storage::Storage,
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
    }

    Ok(())
}

async fn transform(
    config: &config::Config,
    storage: &storage::Storage,
    pipelines: &[&Pipeline],
) -> Result<()> {
    let concurrency = config.transform.concurrency;

    for pipeline in pipelines {
        let untransformed = storage.get_untransformed(&pipeline.name).await?;
        if untransformed.is_empty() {
            info!("[{}] nothing to transform", pipeline.name);
            continue;
        }

        info!(
            "[{}] transforming {} truths (concurrency: {})",
            pipeline.name,
            untransformed.len(),
            concurrency
        );

        let results: Vec<_> = stream::iter(untransformed.iter().map(|truth| {
            let pipeline_name = &pipeline.name;
            async move {
                let memory = storage
                    .get_recent_rewrites(pipeline_name, transformer::MEMORY_EXAMPLE_COUNT)
                    .await;
                let memory = match memory {
                    Ok(m) => m,
                    Err(e) => {
                        error!("[{pipeline_name}] failed to get memory for {}: {e:#}", truth.id);
                        return;
                    }
                };

                match pipeline.run(&truth.content, &memory).await {
                    Ok(rewritten) => {
                        if let Err(e) = storage
                            .insert_rewrite(&truth.id, pipeline_name, &rewritten)
                            .await
                        {
                            error!("[{pipeline_name}] failed to store rewrite for {}: {e:#}", truth.id);
                        } else {
                            info!("transformed [{pipeline_name}]: {}", truth.id);
                        }
                    }
                    Err(e) => {
                        error!("[{pipeline_name}] transform failed for {}: {e:#}", truth.id);
                    }
                }
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;

        let _ = results;
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
                    .publish(
                        &rewrite.content,
                        rewrite.source_url.as_deref(),
                        rewrite.original_published.as_deref(),
                    )
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
