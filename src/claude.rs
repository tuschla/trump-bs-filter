use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveTime, TimeDelta};
use serde::Deserialize;
use tokio::process::Command;
use tracing::info;

#[derive(Deserialize)]
struct ClaudeJsonOutput {
    result: String,
    #[serde(default)]
    api_error_status: Option<u16>,
}

/// The CLI has no usable credentials. Every call fails the same way until a
/// human re-authenticates, so callers abort the batch instead of retrying per post.
#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "claude CLI is not authenticated: {}", self.0)
    }
}

impl std::error::Error for AuthError {}

fn is_auth_failure(parsed: &ClaudeJsonOutput) -> bool {
    parsed.api_error_status == Some(401)
        || [
            "Failed to authenticate",
            "Invalid API key",
            "Please run /login",
        ]
        .iter()
        .any(|m| parsed.result.contains(m))
}

pub async fn call(
    system_prompt: &str,
    user_message: &str,
    web_search: bool,
    model: &str,
    effort: &str,
) -> Result<String> {
    loop {
        let result = call_once(system_prompt, user_message, web_search, model, effort).await;
        match result {
            Ok(output) => return Ok(output),
            Err(e) => {
                let msg = format!("{e:#}");
                if let Some(wait) = parse_rate_limit_wait(&msg) {
                    info!("rate limited, sleeping {}s until reset", wait.as_secs());
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

async fn call_once(
    system_prompt: &str,
    user_message: &str,
    web_search: bool,
    model: &str,
    effort: &str,
) -> Result<String> {
    let mut args = vec![
        "-p".to_string(),
        user_message.to_string(),
        "--model".to_string(),
        model.to_string(),
        "--effort".to_string(),
        effort.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        // One-shot rewrites are never resumed; persisting them wrote a session
        // transcript under ~/.claude/projects for every single call.
        "--no-session-persistence".to_string(),
        // Skip user settings: the interactive setup's plugins and hooks (e.g. a
        // SessionStart hook injecting a "caveman" style ruleset) otherwise ran on
        // every rewrite, leaked into its context, and left a ~/.claude/session-env
        // dir behind per call. Auth is unaffected; it is not a settings source.
        "--setting-sources".to_string(),
        "project".to_string(),
    ];

    if !system_prompt.is_empty() {
        args.push("--system-prompt".to_string());
        args.push(system_prompt.to_string());
    }

    if web_search {
        args.push("--allowedTools".to_string());
        args.push("WebSearch".to_string());
    }

    // Rewriting a post needs no filesystem, shell, or task tools. Their schemas
    // are still shipped in the prompt otherwise: measured 30514 cache-write
    // tokens per call with them versus 25025 without, an 18% cut for free.
    // WebSearch is kept whenever the pipeline actually asked for it.
    let mut denied = vec![
        "Bash",
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "WebFetch",
        "Task",
        "TodoWrite",
        "NotebookEdit",
        "BashOutput",
        "KillShell",
        "SlashCommand",
    ];
    if !web_search {
        denied.push("WebSearch");
    }
    args.push("--disallowedTools".to_string());
    args.push(denied.join(" "));

    let output = Command::new("claude")
        .args(&args)
        .output()
        .await
        .context("failed to run claude CLI — is it installed and in PATH?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        if let Ok(parsed) = serde_json::from_str::<ClaudeJsonOutput>(&stdout) {
            if parsed.api_error_status == Some(429) {
                bail!("rate_limit: {}", parsed.result);
            }
            if is_auth_failure(&parsed) {
                return Err(AuthError(parsed.result).into());
            }
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if !stderr.is_empty() { &stderr } else { &stdout };
        bail!(
            "claude CLI exited with {}: {}",
            output.status,
            detail.chars().take(500).collect::<String>()
        );
    }

    let parsed: ClaudeJsonOutput =
        serde_json::from_str(&stdout).context("failed to parse claude JSON output")?;

    Ok(parsed.result.trim().to_string())
}

fn parse_rate_limit_wait(msg: &str) -> Option<std::time::Duration> {
    if !msg.contains("rate_limit") {
        return None;
    }

    // "resets 11pm", "resets 6:20am", "resets 12:30pm"
    let marker = "resets ";
    let rest = msg.split(marker).nth(1)?;
    let time_str: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == ':')
        .collect();

    let (time_part, is_pm) = if time_str.ends_with("pm") {
        (&time_str[..time_str.len() - 2], true)
    } else if time_str.ends_with("am") {
        (&time_str[..time_str.len() - 2], false)
    } else {
        return None;
    };

    let (hour, minute) = if let Some((h, m)) = time_part.split_once(':') {
        (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)
    } else {
        (time_part.parse::<u32>().ok()?, 0)
    };

    let hour_24 = match (hour, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, true) => h + 12,
        (h, false) => h,
    };

    let reset_time = NaiveTime::from_hms_opt(hour_24, minute, 0)?;
    let now = Local::now().time();

    let wait = if reset_time > now {
        reset_time - now
    } else {
        (reset_time + TimeDelta::hours(24)) - now
    };

    let secs = wait.num_seconds().max(60) as u64;
    Some(std::time::Duration::from_secs(secs + 60))
}
