use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use scraper::{Html, Selector};
use tracing::info;

use crate::storage::Storage;

pub struct Truth {
    pub id: String,
    pub content: String,
    pub published: Option<DateTime<Utc>>,
    pub url: Option<String>,
}

pub async fn fetch_truths(client: &reqwest::Client, feed_url: &str) -> Result<Vec<Truth>> {
    let body = client
        .get(feed_url)
        .send()
        .await
        .context("failed to fetch RSS feed")?
        .bytes()
        .await
        .context("failed to read RSS response body")?;

    let feed = feed_rs::parser::parse(&body[..]).context("failed to parse RSS feed")?;

    let truths = feed
        .entries
        .into_iter()
        .filter_map(|entry| {
            let content = entry
                .content
                .and_then(|c| c.body)
                .or_else(|| entry.summary.map(|s| s.content))
                .unwrap_or_default();

            let content = html_escape::decode_html_entities(&content).into_owned();
            let content = strip_html_tags(&content);

            if content.is_empty() {
                return None;
            }

            Some(Truth {
                id: entry.id,
                content,
                published: entry.published.map(|d| d.with_timezone(&Utc)),
                url: entry.links.first().map(|l| l.href.clone()),
            })
        })
        .collect();

    Ok(truths)
}

const BASE_URL: &str = "https://trumpstruth.org";

pub async fn backfill(client: &reqwest::Client, storage: &Storage) -> Result<u64> {
    let status_sel = Selector::parse(".status").unwrap();
    let content_sel = Selector::parse(".status__content").unwrap();
    let meta_sel = Selector::parse(".status-info__meta-item").unwrap();
    let link_sel = Selector::parse(".status__external-link").unwrap();

    let mut cursor: Option<String> = None;
    let mut total = 0u64;

    loop {
        let mut url = format!("{BASE_URL}?sort=asc&per_page=50");
        if let Some(ref c) = cursor {
            url.push_str(&format!("&cursor={c}"));
        }

        let html = client
            .get(&url)
            .send()
            .await
            .context("failed to fetch archive page")?
            .text()
            .await
            .context("failed to read archive page")?;

        let doc = Html::parse_document(&html);
        let statuses: Vec<_> = doc.select(&status_sel).collect();

        if statuses.is_empty() {
            break;
        }

        let mut page_new = 0u64;

        for status in &statuses {
            let (id, post_url) = match extract_post_id(status, &meta_sel) {
                Some(v) => v,
                None => continue,
            };

            if storage.truth_exists(&id).await? {
                continue;
            }

            let content = status
                .select(&content_sel)
                .next()
                .map(|el| {
                    let raw = el.inner_html();
                    let decoded = html_escape::decode_html_entities(&raw).into_owned();
                    strip_html_tags(&decoded)
                })
                .unwrap_or_default();

            if content.is_empty() {
                continue;
            }

            let published = extract_date(status, &meta_sel);
            let source_url = status
                .select(&link_sel)
                .next()
                .and_then(|el| el.value().attr("href"))
                .map(|s| s.to_string())
                .or(Some(post_url));

            let published_str = published.map(|d| d.to_rfc3339());
            storage
                .insert_truth(
                    &id,
                    &content,
                    source_url.as_deref(),
                    published_str.as_deref(),
                )
                .await?;
            page_new += 1;
            total += 1;
        }

        info!("backfill: fetched page ({page_new} new), {total} total new truths");

        let next_cursor = extract_next_cursor(&html);
        match next_cursor {
            Some(ref c) if Some(c) != cursor.as_ref() => cursor = next_cursor,
            _ => break,
        }
    }

    Ok(total)
}

/// Recover truths missing from the DB by fetching individual status pages.
/// Backfill scrapes listing pages and silently drops some posts (e.g. retruths
/// whose body renders empty on the listing). This walks the recent id range and
/// fetches each missing status page directly, which always carries the content.
pub async fn recover_missing(
    client: &reqwest::Client,
    storage: &Storage,
    window: i64,
) -> Result<u64> {
    let content_sel = Selector::parse(".status__content").unwrap();
    let meta_sel = Selector::parse(".status-info__meta-item").unwrap();
    let link_sel = Selector::parse(".status__external-link").unwrap();

    let listing = client
        .get(format!("{BASE_URL}/?sort=desc&per_page=20"))
        .send()
        .await
        .context("failed to fetch listing for newest id")?
        .text()
        .await
        .context("failed to read listing body")?;
    let newest = extract_max_status_id(&listing)
        .context("could not determine newest status id from listing")?;
    let lo = (newest - window).max(1);
    info!("recover: scanning statuses {lo}..={newest}");

    let mut recovered = 0u64;
    for n in lo..=newest {
        let id = format!("{BASE_URL}/statuses/{n}");
        if storage.truth_exists(&id).await? {
            continue;
        }

        let html = match client.get(&id).send().await {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            _ => continue,
        };
        let doc = Html::parse_document(&html);

        let mut content = doc
            .select(&content_sel)
            .next()
            .map(|el| {
                let decoded = html_escape::decode_html_entities(&el.inner_html()).into_owned();
                strip_html_tags(&decoded)
            })
            .unwrap_or_default();

        let link = doc
            .select(&link_sel)
            .next()
            .and_then(|el| el.value().attr("href"))
            .map(|s| s.to_string());

        if content.is_empty() {
            match &link {
                Some(l) => content = l.clone(),
                None => continue,
            }
        }

        let published = extract_date_doc(&doc, &meta_sel).map(|d| d.to_rfc3339());
        let source_url = link.or_else(|| Some(id.clone()));

        storage
            .insert_truth(&id, &content, source_url.as_deref(), published.as_deref())
            .await?;
        recovered += 1;
        if recovered % 25 == 0 {
            info!("recover: {recovered} new so far (at id {n})");
        }
    }

    info!("recover complete: {recovered} new truths");
    Ok(recovered)
}

fn extract_max_status_id(html: &str) -> Option<i64> {
    let mut max: Option<i64> = None;
    for seg in html.split("/statuses/").skip(1) {
        let num: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = num.parse::<i64>() {
            max = Some(max.map_or(n, |m| m.max(n)));
        }
    }
    max
}

fn extract_date_doc(doc: &Html, meta_sel: &Selector) -> Option<DateTime<Utc>> {
    for el in doc.select(meta_sel) {
        let text = el.text().collect::<String>();
        if let Ok(dt) = NaiveDateTime::parse_from_str(text.trim(), "%B %e, %Y, %l:%M %p") {
            return Some(dt.and_utc());
        }
    }
    None
}

fn extract_post_id(
    status: &scraper::ElementRef,
    meta_sel: &Selector,
) -> Option<(String, String)> {
    for el in status.select(meta_sel) {
        if let Some(href) = el.value().attr("href") {
            if href.contains("/statuses/") {
                let url = if href.starts_with("http") {
                    href.to_string()
                } else {
                    format!("{BASE_URL}{href}")
                };
                return Some((url.clone(), url));
            }
        }
    }
    None
}

fn extract_date(
    status: &scraper::ElementRef,
    meta_sel: &Selector,
) -> Option<DateTime<Utc>> {
    for el in status.select(meta_sel) {
        let text = el.text().collect::<String>();
        if let Ok(dt) = NaiveDateTime::parse_from_str(text.trim(), "%B %e, %Y, %l:%M %p") {
            return Some(dt.and_utc());
        }
    }
    None
}

fn extract_next_cursor(html: &str) -> Option<String> {
    let marker = "cursor=";
    let mut last_cursor = None;
    for segment in html.split(marker).skip(1) {
        let end = segment.find(|c: char| c == '"' || c == '&' || c == '\'').unwrap_or(segment.len());
        let cursor = &segment[..end];
        if !cursor.is_empty() {
            last_cursor = Some(cursor.to_string());
        }
    }
    last_cursor
}

fn strip_html_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut inside_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => output.push(ch),
            _ => {}
        }
    }
    output.trim().to_string()
}
