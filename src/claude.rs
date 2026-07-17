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

pub async fn call(
    system_prompt: &str,
    user_message: &str,
    web_search: bool,
    model: &str,
) -> Result<String> {
    loop {
        let result = call_once(system_prompt, user_message, web_search, model).await;
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
) -> Result<String> {
    let mut args = vec![
        "-p".to_string(),
        user_message.to_string(),
        "--model".to_string(),
        model.to_string(),
        "--effort".to_string(),
        "high".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ];

    if !system_prompt.is_empty() {
        args.push("--system-prompt".to_string());
        args.push(system_prompt.to_string());
    }

    if web_search {
        args.push("--allowedTools".to_string());
        args.push("WebSearch".to_string());
    }

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
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if !stderr.is_empty() { &stderr } else { &stdout };
        bail!("claude CLI exited with {}: {}", output.status, detail.chars().take(500).collect::<String>());
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
    let time_str: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == ':').collect();

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
