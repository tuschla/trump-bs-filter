use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Deserialize)]
struct ClaudeJsonOutput {
    result: String,
}

pub async fn call(
    system_prompt: &str,
    user_message: &str,
    web_search: bool,
) -> Result<String> {
    let mut args = vec![
        "-p".to_string(),
        user_message.to_string(),
        "--model".to_string(),
        "claude-opus-4-6".to_string(),
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

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("claude CLI exited with {}: {stderr}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: ClaudeJsonOutput =
        serde_json::from_str(&stdout).context("failed to parse claude JSON output")?;

    Ok(parsed.result.trim().to_string())
}
