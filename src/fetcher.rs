use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

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
