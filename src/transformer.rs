use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::claude;
use crate::storage::PastRewrite;

pub const MEMORY_EXAMPLE_COUNT: i64 = 3;

#[derive(Debug, Deserialize)]
pub struct Pipeline {
    pub name: String,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Deserialize)]
pub struct Stage {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub web_search: bool,
    pub audits: Option<String>,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_max_retries() -> u32 {
    0
}

impl Pipeline {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read pipeline config: {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("failed to parse pipeline config: {}", path.display()))
    }

    pub async fn run(&self, original_post: &str, memory: &[PastRewrite]) -> Result<String> {
        let mut stage_outputs: Vec<(&str, String)> = Vec::new();

        for stage in &self.stages {
            if stage.audits.is_some() {
                let audited_stage_name = stage.audits.as_deref().unwrap();
                let audited_idx = self
                    .stages
                    .iter()
                    .position(|s| s.name == audited_stage_name)
                    .with_context(|| {
                        format!("audit stage '{}' references unknown stage '{}'", stage.name, audited_stage_name)
                    })?;

                let mut retries = 0;
                loop {
                    let user_message = build_message(original_post, &stage_outputs, memory);
                    let audit_result = claude::call(&stage.prompt, &user_message, stage.web_search).await?;

                    if audit_result.trim() == "PASS" {
                        info!("[{}] audit passed for stage '{}'", self.name, audited_stage_name);
                        break;
                    }

                    retries += 1;
                    if retries > stage.max_retries {
                        warn!(
                            "[{}] audit failed after {} retries, using last rewrite",
                            self.name, stage.max_retries
                        );
                        break;
                    }

                    info!(
                        "[{}] audit retry {}/{} for stage '{}'",
                        self.name, retries, stage.max_retries, audited_stage_name
                    );

                    let audited_stage = &self.stages[audited_idx];
                    let retry_message = build_retry_message(
                        original_post,
                        &stage_outputs,
                        &audit_result,
                        memory,
                    );
                    let new_output = claude::call(
                        &audited_stage.prompt,
                        &retry_message,
                        audited_stage.web_search,
                    )
                    .await?;

                    if let Some(entry) = stage_outputs.iter_mut().find(|(name, _)| *name == audited_stage_name) {
                        entry.1 = new_output;
                    }
                }
            } else {
                let user_message = build_message(original_post, &stage_outputs, memory);
                let output = claude::call(&stage.prompt, &user_message, stage.web_search).await?;
                info!("[{}] completed stage '{}'", self.name, stage.name);
                stage_outputs.push((&stage.name, output));
            }
        }

        let final_output = stage_outputs
            .last()
            .map(|(_, output)| output.clone())
            .unwrap_or_default();

        let final_output = validate_urls(&final_output).await;

        Ok(final_output)
    }
}

fn format_memory(memory: &[PastRewrite]) -> String {
    if memory.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\nPREVIOUS REWRITES (for tone/style consistency — match this voice):\n",
    );
    for (i, past) in memory.iter().enumerate() {
        out.push_str(&format!(
            "\n--- Example {} ---\nOriginal: {}\nRewrite: {}\n",
            i + 1,
            past.original,
            past.rewritten
        ));
    }
    out
}

fn build_message(
    original_post: &str,
    previous_outputs: &[(&str, String)],
    memory: &[PastRewrite],
) -> String {
    let mut msg = format!("ORIGINAL POST:\n{original_post}");

    for (name, output) in previous_outputs {
        let label = name.to_uppercase();
        msg.push_str(&format!("\n\n{label}:\n{output}"));
    }

    msg.push_str(&format_memory(memory));
    msg
}

fn build_retry_message(
    original_post: &str,
    stage_outputs: &[(&str, String)],
    audit_feedback: &str,
    memory: &[PastRewrite],
) -> String {
    let mut msg = format!("ORIGINAL POST:\n{original_post}");

    for (name, output) in stage_outputs {
        let label = name.to_uppercase();
        msg.push_str(&format!("\n\n{label}:\n{output}"));
    }

    msg.push_str(&format!(
        "\n\nAUDIT FEEDBACK:\n{audit_feedback}\n\nPlease fix the issues identified in the audit."
    ));

    msg.push_str(&format_memory(memory));
    msg
}

struct SourceLine {
    url: String,
    description: String,
    line: String,
}

fn parse_source_lines(text: &str) -> Vec<SourceLine> {
    let mut results = Vec::new();
    let mut in_sources = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("sources:")
            || trimmed.eq_ignore_ascii_case("sources")
        {
            in_sources = true;
            continue;
        }
        if !in_sources {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('-') && !trimmed.starts_with('•') {
            break;
        }

        if let Some((desc, url)) = parse_markdown_link(trimmed) {
            results.push(SourceLine {
                url,
                description: desc,
                line: line.to_string(),
            });
        } else {
            for word in trimmed.split_whitespace() {
                let cleaned = word.trim_matches(|c: char| "()[]<>,\"'".contains(c));
                if cleaned.starts_with("https://") || cleaned.starts_with("http://") {
                    results.push(SourceLine {
                        url: cleaned.to_string(),
                        description: trimmed.to_string(),
                        line: line.to_string(),
                    });
                    break;
                }
            }
        }
    }
    results
}

fn parse_markdown_link(text: &str) -> Option<(String, String)> {
    let open = text.find('[')?;
    let close = text[open..].find(']')? + open;
    let paren_open = text[close..].find('(')? + close;
    let paren_close = text[paren_open..].find(')')? + paren_open;
    let desc = text[open + 1..close].to_string();
    let url = text[paren_open + 1..paren_close].to_string();
    Some((desc, url))
}

fn extract_keywords(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_lowercase())
        .collect()
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
    output
}

async fn validate_urls(text: &str) -> String {
    let sources = parse_source_lines(text);
    if sources.is_empty() {
        return text.to_string();
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap();

    let mut bad_lines = Vec::new();

    for source in &sources {
        let page_text = match client.get(&source.url).send().await {
            Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                let body = resp.text().await.unwrap_or_default();
                strip_html_tags(&body).to_lowercase()
            }
            _ => {
                warn!("dead URL removed: {}", source.url);
                bad_lines.push(source.line.as_str());
                continue;
            }
        };

        let keywords = extract_keywords(&source.description);
        let matches = keywords.iter().filter(|kw| page_text.contains(kw.as_str())).count();
        let threshold = (keywords.len() / 3).max(2);

        if matches < threshold {
            warn!(
                "irrelevant source removed (matched {}/{} keywords): {}",
                matches,
                keywords.len(),
                source.url
            );
            bad_lines.push(source.line.as_str());
        }
    }

    if bad_lines.is_empty() {
        return text.to_string();
    }

    text.lines()
        .filter(|line| !bad_lines.contains(line))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn load_all(prompts_dir: &Path) -> Result<Vec<Pipeline>> {
    let mut pipelines = Vec::new();

    for entry in std::fs::read_dir(prompts_dir)
        .with_context(|| format!("failed to read prompts directory: {}", prompts_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let pipeline = Pipeline::load(&path)?;
            info!("loaded pipeline: {}", pipeline.name);
            pipelines.push(pipeline);
        }
    }

    if pipelines.is_empty() {
        anyhow::bail!("no pipeline configs found in {}", prompts_dir.display());
    }

    Ok(pipelines)
}
