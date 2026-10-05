use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::Event;
use scraper::{Html, Selector};
use tracing::{info, warn};

use crate::storage::Storage;

pub struct Truth {
    pub id: String,
    pub content: String,
    pub published: Option<DateTime<Utc>>,
    pub url: Option<String>,
}

/// Poll Truth Social's own API for an account's recent statuses. This bypasses
/// the trumpstruth.org mirror (~160s of lag) and reports the real post time, so
/// end-to-end latency drops to poll + transform + publish. The statuses endpoint
/// 403s without a browser-like Referer; these headers are what unlock it.
pub async fn fetch_truths_ts(client: &reqwest::Client, account_id: &str) -> Result<Vec<Truth>> {
    use reqwest::header::{ACCEPT, REFERER, USER_AGENT};

    let url = format!(
        "https://truthsocial.com/api/v1/accounts/{account_id}/statuses?exclude_replies=true&limit=40"
    );
    let items: Vec<serde_json::Value> = client
        .get(&url)
        .header(USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
        .header(ACCEPT, "application/json, text/plain, */*")
        .header(REFERER, "https://truthsocial.com/@realDonaldTrump")
        .send()
        .await
        .context("failed to fetch Truth Social statuses")?
        .error_for_status()
        .context("Truth Social statuses request failed")?
        .json()
        .await
        .context("failed to parse Truth Social statuses JSON")?;

    let truths = items
        .into_iter()
        .filter_map(|s| {
            // Canonical permalink doubles as the stable primary key and source URL.
            let id = s.get("url").and_then(|v| v.as_str())?.to_string();

            let raw = s
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let content = strip_html_tags(&html_escape::decode_html_entities(raw));
            if content.is_empty() {
                return None;
            }

            let published = s
                .get("created_at")
                .and_then(|v| v.as_str())
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|d| d.with_timezone(&Utc));

            Some(Truth {
                id: id.clone(),
                content,
                published,
                url: Some(id),
            })
        })
        .collect();

    Ok(truths)
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

    let originals = extract_original_urls(&body);
    let feed = feed_rs::parser::parse(&body[..]).context("failed to parse RSS feed")?;

    if !feed.entries.is_empty() && originals.is_empty() {
        warn!(
            "mirror feed carried no truth:originalUrl elements; falling back to mirror ids, \
             which cannot be reconciled with Truth Social ids and may double-publish"
        );
    }

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

            // Prefer the canonical Truth Social permalink. Both ingest paths then
            // share one primary key, so a post already stored from the API is not
            // re-inserted (and re-published) under a mirror id.
            let id = originals
                .get(&entry.id)
                .cloned()
                .unwrap_or_else(|| entry.id.clone());

            Some(Truth {
                content,
                published: entry.published.map(|d| d.with_timezone(&Utc)),
                url: Some(id.clone()),
                id,
            })
        })
        .collect();

    Ok(truths)
}

/// Which element's text we are currently accumulating inside an `<item>`.
enum ItemField {
    Link,
    Guid,
    Original,
}

/// Map a mirror item's own link/guid to the canonical Truth Social permalink that
/// trumpstruth.org publishes as `<truth:originalUrl>`.
///
/// feed-rs 2.x drops namespaced extension elements entirely, so the raw XML is
/// scanned separately rather than replacing a parser that already handles the
/// feed's date and CDATA quirks. Both link and guid are keyed because feed-rs
/// derives `entry.id` from whichever the feed supplies.
fn extract_original_urls(xml: &[u8]) -> HashMap<String, String> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut map = HashMap::new();

    let mut link: Option<String> = None;
    let mut guid: Option<String> = None;
    let mut original: Option<String> = None;
    let mut field: Option<ItemField> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                field = match e.name().as_ref() {
                    // Entering an item discards any channel-level link already seen.
                    b"item" => {
                        link = None;
                        guid = None;
                        original = None;
                        None
                    }
                    b"link" => Some(ItemField::Link),
                    b"guid" => Some(ItemField::Guid),
                    b"truth:originalUrl" => Some(ItemField::Original),
                    _ => None,
                };
            }
            Ok(Event::Text(t)) => {
                if let Some(f) = &field {
                    let text = t.unescape().unwrap_or_default().trim().to_string();
                    if !text.is_empty() {
                        match f {
                            ItemField::Link => link = Some(text),
                            ItemField::Guid => guid = Some(text),
                            ItemField::Original => original = Some(text),
                        }
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(f) = &field {
                    let text = String::from_utf8_lossy(&t).trim().to_string();
                    if !text.is_empty() {
                        match f {
                            ItemField::Link => link = Some(text),
                            ItemField::Guid => guid = Some(text),
                            ItemField::Original => original = Some(text),
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"item"
                    && let Some(canonical) = original.take()
                {
                    for key in [link.take(), guid.take()].into_iter().flatten() {
                        map.insert(key, canonical.clone());
                    }
                }
                field = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                warn!("mirror feed XML scan stopped early: {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    map
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
        if recovered.is_multiple_of(25) {
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

fn extract_post_id(status: &scraper::ElementRef, meta_sel: &Selector) -> Option<(String, String)> {
    for el in status.select(meta_sel) {
        if let Some(href) = el.value().attr("href")
            && href.contains("/statuses/")
        {
            let url = if href.starts_with("http") {
                href.to_string()
            } else {
                format!("{BASE_URL}{href}")
            };
            return Some((url.clone(), url));
        }
    }
    None
}

fn extract_date(status: &scraper::ElementRef, meta_sel: &Selector) -> Option<DateTime<Utc>> {
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
        let end = segment.find(['"', '&', '\'']).unwrap_or(segment.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The mirror keys posts by its own status id while the API keys them by the
    /// canonical Truth Social permalink. If this mapping regresses, a fallback
    /// fetch re-inserts posts already stored from the API and publishes them a
    /// second time to a live account, so each branch is pinned.
    #[test]
    fn maps_mirror_ids_to_canonical_truth_social_urls() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:truth="https://truthsocial.com/ns">
  <channel>
    <link><![CDATA[https://trumpstruth.org/feed]]></link>
    <item>
      <link>https://trumpstruth.org/statuses/41657</link>
      <guid>https://trumpstruth.org/statuses/41657</guid>
      <pubDate>Wed, 09 Sep 2026 13:41:33 +0000</pubDate>
      <truth:originalUrl>https://truthsocial.com/@realDonaldTrump/117241367309466443</truth:originalUrl>
      <truth:originalId>117241367309466443</truth:originalId>
    </item>
    <item>
      <link>https://trumpstruth.org/statuses/41000</link>
      <guid>https://trumpstruth.org/statuses/41000</guid>
    </item>
  </channel>
</rss>"#;

        let map = extract_original_urls(xml);

        assert_eq!(
            map.get("https://trumpstruth.org/statuses/41657")
                .map(String::as_str),
            Some("https://truthsocial.com/@realDonaldTrump/117241367309466443"),
            "item link must resolve to the canonical permalink"
        );

        // An item lacking the extension must be absent rather than mapped to
        // something wrong: callers then keep the mirror id, which is merely
        // degraded, not a duplicate under a second identity.
        assert!(!map.contains_key("https://trumpstruth.org/statuses/41000"));

        // The channel's own <link> precedes the items; it must never be treated
        // as an item and inherit the first item's permalink.
        assert!(!map.contains_key("https://trumpstruth.org/feed"));
    }
}
