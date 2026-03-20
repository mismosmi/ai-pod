use anyhow::{Context, Result};
use colored::Colorize;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

use crate::config::AppConfig;
use crate::server::lifecycle::{MCP_PORT, ProjectState};
use crate::workspace::workspace_hash;

#[derive(Serialize)]
struct ListDaemonsRequest {
    project_id: String,
}

#[derive(Serialize)]
struct DaemonOutputRequest {
    project_id: String,
    daemon_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
enum Message {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

pub async fn run_daemons(config: &AppConfig, workspace: &Path) -> Result<()> {
    let project_id = workspace_hash(workspace);
    let state = ProjectState::load(&config.project_state_file(&project_id));

    if state.api_key.is_empty() {
        anyhow::bail!(
            "No project state found for this workspace.\nLaunch it first with `ai-pod`."
        );
    }

    let base_url = format!("http://localhost:{}", MCP_PORT);
    let client = reqwest::Client::new();

    // Fetch daemon list
    let response = client
        .post(format!("{}/daemon/list", base_url))
        .header("X-Api-Key", &state.api_key)
        .json(&ListDaemonsRequest {
            project_id: project_id.clone(),
        })
        .send()
        .await
        .context("Failed to connect to shared server. Is it running? Start with `ai-pod serve`.")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Server error {}: {}", status, body);
    }

    let result: serde_json::Value = response
        .json()
        .await
        .context("Failed to parse daemon list")?;
    let daemons = result["daemons"]
        .as_array()
        .context("Unexpected response format")?;

    if daemons.is_empty() {
        println!("{}", "No daemons running for this workspace.".yellow());
        return Ok(());
    }

    // Print table
    println!(
        "{:<14} {:<12} {:<20} {}",
        "ID".bold(),
        "STATUS".bold(),
        "STARTED".bold(),
        "COMMAND".bold()
    );
    println!("{}", "-".repeat(80));

    let mut items = Vec::new();
    for d in daemons {
        let id = d["id"].as_str().unwrap_or("");
        let status = format_status(&d["status"]);
        let started = format_unix_time(d["started_at"].as_u64().unwrap_or(0));
        let command = d["command"].as_str().unwrap_or("");
        let command_short = if command.len() > 32 {
            format!("{}...", &command[..29])
        } else {
            command.to_string()
        };
        println!(
            "{:<14} {:<12} {:<20} {}",
            id, status, started, command_short
        );
        items.push((
            id.to_string(),
            format!("{} | {} | {}", id, status, command_short),
        ));
    }

    println!();

    // Interactive selection (blocking dialoguer must run off the async executor)
    let display_items: Vec<String> = items.iter().map(|(_, s)| s.clone()).collect();
    let selection = tokio::task::spawn_blocking(move || {
        dialoguer::Select::new()
            .with_prompt("Select a daemon to view output")
            .items(&display_items)
            .default(0)
            .interact_opt()
    })
    .await??;

    let daemon_id = match selection {
        Some(idx) => &items[idx].0,
        None => return Ok(()),
    };

    println!();
    println!(
        "{} {}",
        "Output for daemon".blue().bold(),
        daemon_id.bold()
    );
    println!("{}", "-".repeat(80));

    // Stream daemon output
    let response = client
        .post(format!("{}/daemon/output", base_url))
        .header("X-Api-Key", &state.api_key)
        .json(&DaemonOutputRequest {
            project_id,
            daemon_id: daemon_id.clone(),
        })
        .send()
        .await
        .context("Failed to fetch daemon output")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Server error {}: {}", status, body);
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading output stream")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = buf.find('\n') {
            let line = buf[..newline_pos].to_string();
            buf = buf[newline_pos + 1..].to_string();

            if line.is_empty() {
                continue;
            }

            match serde_json::from_str::<Message>(&line) {
                Ok(Message::Stdout(s)) => print!("{}", s),
                Ok(Message::Stderr(s)) => eprint!("{}", s),
                Ok(Message::Exit(code)) => {
                    let _ = std::io::stdout().flush();
                    let _ = std::io::stderr().flush();
                    println!();
                    if code == 0 {
                        println!(
                            "{}",
                            format!("Process exited with code {}", code).green()
                        );
                    } else {
                        println!(
                            "{}",
                            format!("Process exited with code {}", code).red()
                        );
                    }
                    return Ok(());
                }
                Err(_) => {}
            }
        }
    }

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    Ok(())
}

fn format_status(status: &serde_json::Value) -> String {
    if let Some(s) = status.as_str() {
        return s.to_string();
    }
    if let Some(obj) = status.as_object() {
        if obj.contains_key("finished") {
            let code = obj["finished"]["exit_code"].as_i64().unwrap_or(0);
            return format!("finished({})", code);
        }
    }
    if status["exit_code"].is_number() {
        let code = status["exit_code"].as_i64().unwrap_or(0);
        return format!("finished({})", code);
    }
    serde_json::to_string(status).unwrap_or_else(|_| "unknown".to_string())
}

fn format_unix_time(secs: u64) -> String {
    if secs == 0 {
        return "unknown".to_string();
    }
    use std::time::{Duration, UNIX_EPOCH};
    let _ = UNIX_EPOCH + Duration::from_secs(secs);
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(secs);
    if age < 60 {
        format!("{}s ago", age)
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}d ago", age / 86400)
    }
}
