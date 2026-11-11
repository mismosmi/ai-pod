//! Container discovery, attachment, and complete workspace runtime cleanup.

use super::{checked_status, volume_exists};
use crate::config::AppConfig;
use crate::runtime::ContainerRuntime;
use crate::workspace::{container_prefix, volume_name as gen_volume_name, workspace_hash};
use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
use std::process::Stdio;

pub fn containers_for_prefix(
    rt: &ContainerRuntime,
    prefix: &str,
    running_only: bool,
) -> Result<Vec<String>> {
    let filter = format!("name=^{}-", prefix);
    let mut cmd = rt.command();
    cmd.arg("ps");
    if !running_only {
        cmd.arg("-a");
    }
    cmd.args([
        "--filter",
        &filter,
        "--filter",
        "label=managed-by=ai-pod",
        "--format",
        "{{.Names}}",
    ]);
    let output = cmd.output().context("Failed to list containers")?;
    let names = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    Ok(names)
}

pub fn list_containers(rt: &ContainerRuntime) -> Result<()> {
    let output = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            "label=managed-by=ai-pod",
            "--format",
            "{{.Names}}\t{{.Status}}\t{{.CreatedAt}}",
        ])
        .output()
        .context("Failed to list containers")?;

    if output.stdout.is_empty() {
        println!("{}", "No ai-pod containers found.".yellow());
    } else {
        println!("{}", "ai-pod containers:".blue().bold());
        println!("{:<20} {:<30} {}", "NAME", "STATUS", "CREATED");
        println!("{}", "-".repeat(80));
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }

    Ok(())
}

pub fn attach_container(rt: &ContainerRuntime) -> Result<()> {
    // List all running ai-pod containers with their start times
    let output = rt
        .command()
        .args([
            "ps",
            "--filter",
            "label=managed-by=ai-pod",
            "--format",
            "{{.Names}}\t{{.CreatedAt}}",
        ])
        .output()
        .context("Failed to list running containers")?;

    let entries: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, '\t');
            let name = parts.next().unwrap_or("").to_string();
            let created = parts.next().unwrap_or("").to_string();
            (name, created)
        })
        .collect();

    if entries.is_empty() {
        println!("{}", "No running ai-pod containers found.".yellow());
        return Ok(());
    }

    let container_name = if entries.len() == 1 {
        entries[0].0.clone()
    } else {
        let items: Vec<String> = entries
            .iter()
            .map(|(name, created)| format!("{:<32} started {}", name, created))
            .collect();
        let selection = dialoguer::Select::new()
            .with_prompt("Select session to attach")
            .items(&items)
            .default(0)
            .interact()
            .context("Selection cancelled")?;
        entries[selection].0.clone()
    };

    println!("{} {}", "Attaching to:".green(), container_name);
    rt.command()
        .args(["attach", "--detach-keys=ctrl-p,ctrl-q", &container_name])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to attach to container")?;

    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn clean_container(rt: &ContainerRuntime, config: &AppConfig, workspace: &Path) -> Result<()> {
    let prefix = container_prefix(workspace);
    // Include stopped initialization helpers, services, and egress containers,
    // which do not all carry the main container's managed-by label.
    let output = rt
        .command()
        .args([
            "ps",
            "-a",
            "--format",
            "{{.Names}}",
            "--filter",
            &format!("name=^{prefix}-"),
        ])
        .output()?;
    checked_status(output.status, "list workspace containers")?;
    for name in String::from_utf8(output.stdout)?
        .lines()
        .filter(|n| n.starts_with(&format!("{prefix}-")))
    {
        let inspection = rt.command().args(["inspect", name]).output()?;
        checked_status(inspection.status, "inspect workspace container")?;
        let data: serde_json::Value = serde_json::from_slice(&inspection.stdout)?;
        let labels = &data[0]["Config"]["Labels"];
        if let Some(session) = labels[crate::service::PARENT_LABEL_KEY].as_str() {
            remove_file_if_exists(&config.session_state_file(session))?;
        }
        if let Some(env) = data[0]["Config"]["Env"].as_array() {
            for value in env.iter().filter_map(|v| v.as_str()) {
                if let Some(session) = value.strip_prefix("AI_POD_SESSION_ID=") {
                    remove_file_if_exists(&config.session_state_file(session))?;
                }
            }
        }
        checked_status(
            rt.command()
                .args(["rm", "--force", "--volumes", name])
                .status()?,
            "remove workspace container",
        )?;
    }

    let output = rt
        .command()
        .args([
            "volume",
            "ls",
            "--format",
            "{{.Name}}",
            "--filter",
            &format!("name=^{prefix}-"),
        ])
        .output()?;
    checked_status(output.status, "list workspace volumes")?;
    let home = gen_volume_name(workspace);
    let mut volumes: Vec<String> = String::from_utf8(output.stdout)?
        .lines()
        .filter(|n| n.starts_with(&format!("{prefix}-")))
        .map(str::to_owned)
        .collect();
    if volume_exists(rt, &home)? && !volumes.contains(&home) {
        volumes.push(home);
    }
    for volume in volumes {
        checked_status(
            rt.command().args(["volume", "rm", &volume]).status()?,
            "remove workspace volume",
        )?;
    }

    for network in [
        crate::workspace::service_network_name(workspace),
        crate::egress::filtered_network_name(workspace),
    ] {
        let status = rt
            .command()
            .args(["network", "inspect", &network])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            checked_status(
                rt.command().args(["network", "rm", &network]).status()?,
                "remove workspace network",
            )?;
        }
    }
    let image = crate::image::image_name(workspace);
    if !crate::image::needs_build(rt, &image, false)? {
        checked_status(
            rt.command().args(["image", "rm", &image]).status()?,
            "remove workspace image",
        )?;
    }
    if rt.dry_run {
        return Ok(());
    }
    if config.sessions_dir().exists() {
        for entry in std::fs::read_dir(config.sessions_dir())? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)?;
            if let Ok(state) = serde_json::from_str::<crate::config::SessionState>(&raw) {
                if state.workspace.as_deref() == Some(workspace_hash(workspace).as_str())
                    || state.network.as_deref()
                        == Some(crate::egress::filtered_network_name(workspace).as_str())
                {
                    remove_file_if_exists(&path)?;
                }
            }
        }
    }
    remove_file_if_exists(&config.project_state_file(&workspace_hash(workspace)))?;
    let commands = crate::server::runner::commands_root(workspace);
    if commands.exists() {
        std::fs::remove_dir_all(commands)?;
    }
    // Preserve source files, including ai-pod.Dockerfile and linked worktrees.
    // The next build must ignore cached layers, even if base layers remain
    // shared with other workspaces.
    let marker = crate::image::clean_build_marker(workspace);
    std::fs::create_dir_all(marker.parent().unwrap())?;
    std::fs::write(marker, "")?;
    println!(
        "{}",
        "Workspace runtime data removed; next launch will build from scratch.".green()
    );

    Ok(())
}
