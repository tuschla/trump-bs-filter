mod claude;
mod config;
mod fetcher;
mod publisher;
mod storage;
mod transformer;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, TryStreamExt};
use tracing::{error, info, warn};

use publisher::Publisher;
use transformer::Pipeline;

const AUTH_BACKOFF_MIN: Duration = Duration::from_secs(60);
const AUTH_BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

#[derive(Parser)]
#[command(
    name = "non-violent-trump",
    about = "Rewrite Trump's truths in kinder language"
)]
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
    // rustls, not the default native-tls: Truth Social's edge rejects the OpenSSL
    // ClientHello fingerprint with a 403 no matter what headers are sent, while
    // the rustls handshake negotiated up to h2 is accepted.
    let http = reqwest::Client::builder()
        .use_rustls_tls()
        .build()
        .context("failed to build HTTP client")?;

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
            // Fetch and publish keep running while transforms wait out an auth
            // outage; the pause doubles so a dead login is probed, not hammered.
            let mut auth_backoff = AUTH_BACKOFF_MIN;
            let mut transforms_paused_until: Option<tokio::time::Instant> = None;
            loop {
                if let Err(e) = fetch(&http, &config, &storage).await {
                    error!("fetch failed: {e:#}");
                }
                if transforms_paused_until.is_none_or(|t| tokio::time::Instant::now() >= t) {
                    match transform(&config, &storage, &pipelines).await {
                        Ok(()) => {
                            transforms_paused_until = None;
                            auth_backoff = AUTH_BACKOFF_MIN;
                        }
                        Err(e) if e.downcast_ref::<claude::AuthError>().is_some() => {
                            error!(
                                "{e:#}; run `claude auth login` or set CLAUDE_CODE_OAUTH_TOKEN. \
                                 Pausing transforms for {}s",
                                auth_backoff.as_secs()
                            );
                            transforms_paused_until =
                                Some(tokio::time::Instant::now() + auth_backoff);
                            auth_backoff = (auth_backoff * 2).min(AUTH_BACKOFF_MAX);
                        }
                        Err(e) => error!("transform failed: {e:#}"),
                    }
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
    let truths = if config.feed.source == "truthsocial" {
        let account_id = config
            .feed
            .truthsocial_account_id
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!("source=truthsocial requires feed.truthsocial_account_id")
            })?;
        // The API is the low-latency path but is edge-blocked without warning.
        // Degrade to the mirror (~160s lag) rather than going dark; both paths key
        // posts by the canonical Truth Social permalink, so this cannot duplicate.
        match fetcher::fetch_truths_ts(http, account_id).await {
            Ok(truths) => truths,
            Err(e) => {
                warn!(
                    "Truth Social API fetch failed, falling back to {}: {e:#}",
                    config.feed.url
                );
                fetcher::fetch_truths(http, &config.feed.url).await?
            }
        }
    } else {
        fetcher::fetch_truths(http, &config.feed.url).await?
    };
    info!("fetched {} truths from feed", truths.len());

    // Cutover guard: skip anything at/before the switchover instant so posts
    // already stored under trumpstruth ids don't re-publish under Truth Social ids.
    let cutover = config
        .feed
        .cutover_rfc3339
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));

    for truth in &truths {
        if let (Some(cut), Some(published)) = (cutover, truth.published)
            && published <= cut
        {
            continue;
        }
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

        let (rewritable, skipped): (Vec<_>, Vec<_>) = untransformed
            .iter()
            .partition(|t| transformer::content_is_rewritable(&t.content));
        for t in &skipped {
            // Persist the verdict so this post is not re-read and re-logged on every
            // subsequent poll; on failure it simply gets reconsidered next cycle.
            match storage.mark_transform_skipped(&t.id, &pipeline.name).await {
                Ok(()) => info!("[{}] skipping no-content post: {}", pipeline.name, t.id),
                Err(e) => error!(
                    "[{}] failed to record skip for {}: {e:#}",
                    pipeline.name, t.id
                ),
            }
        }

        // Per-post failures are logged and left for the next cycle; only an auth
        // failure (which every remaining post would repeat) aborts the batch.
        stream::iter(rewritable.iter().map(Ok::<_, anyhow::Error>))
            .try_for_each_concurrent(concurrency, |truth| {
                let pipeline_name = &pipeline.name;
                async move {
                    let memory = match storage
                        .get_recent_rewrites(pipeline_name, transformer::MEMORY_EXAMPLE_COUNT)
                        .await
                    {
                        Ok(m) => m,
                        Err(e) => {
                            error!("[{pipeline_name}] failed to get memory for {}: {e:#}", truth.id);
                            return Ok(());
                        }
                    };

                    match pipeline.run(&truth.content, &memory).await {
                        Ok(rewritten) => {
                            if transformer::looks_like_refusal(&rewritten) {
                                warn!(
                                    "[{pipeline_name}] discarding out-of-character refusal for {}: {}",
                                    truth.id,
                                    rewritten.chars().take(80).collect::<String>()
                                );
                                return Ok(());
                            }
                            if let Err(e) = storage
                                .insert_rewrite(&truth.id, pipeline_name, &rewritten)
                                .await
                            {
                                error!("[{pipeline_name}] failed to store rewrite for {}: {e:#}", truth.id);
                            } else {
                                info!("transformed [{pipeline_name}]: {}", truth.id);
                            }
                        }
                        Err(e) if e.downcast_ref::<claude::AuthError>().is_some() => return Err(e),
                        Err(e) => {
                            error!("[{pipeline_name}] transform failed for {}: {e:#}", truth.id);
                        }
                    }
                    Ok(())
                }
            })
            .await?;
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
    for p in publishers {
        let cap = p.max_per_run();
        let interval = p.min_interval_secs() as i64;
        let mut posted = 0usize;
        // Newest publish time for this platform, so rate limiting survives restarts.
        let mut last = storage.last_published_at(p.platform()).await?;

        'platform: for pipeline in pipelines {
            let unpublished = storage
                .get_unpublished(&pipeline.name, p.platform())
                .await?;
            for rewrite in &unpublished {
                if let Some(c) = cap
                    && posted >= c
                {
                    break 'platform;
                }
                if interval > 0
                    && let Some(ref last_ts) = last
                    && secs_since(last_ts) < interval
                {
                    // Too soon; try again next cycle.
                    break 'platform;
                }

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
                        posted += 1;
                        last = Some(now_utc_string());
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

fn now_utc_string() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Seconds since a "YYYY-MM-DD HH:MM:SS" UTC timestamp. Unparseable → i64::MAX
/// (treat as long ago, so a bad row never blocks publishing forever).
fn secs_since(ts: &str) -> i64 {
    match chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
        Ok(dt) => (chrono::Utc::now().naive_utc() - dt).num_seconds(),
        Err(_) => i64::MAX,
    }
}
