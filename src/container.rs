use anyhow::{Context, Result};
use colored::Colorize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

use crate::config::AppConfig;

const CONTAINER_CLAUDE_MD: &str = r#"# Container Environment
You are running inside a Podman container. To reach services on the host machine,
use `host.containers.internal` instead of `localhost`.

For example: `curl http://host.containers.internal:3000`

Working directory: /app
"#;

fn generate_container_name(workspace: &Path) -> String {
    let workspace_str = workspace.to_string_lossy();
    let hash = Sha256::digest(workspace_str.as_bytes());
    let short_hash = hex::encode(&hash[..6]);
    format!("claude-{}", short_hash)
}

fn container_exists(name: &str) -> Result<bool> {
    let output = Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{}$", name),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("Failed to check if container exists")?;

    Ok(!output.stdout.is_empty())
}

fn container_is_running(name: &str) -> Result<bool> {
    let output = Command::new("podman")
        .args([
            "ps",
            "--filter",
            &format!("name=^{}$", name),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("Failed to check if container is running")?;

    Ok(!output.stdout.is_empty())
}

fn generate_runtime_claude_md(config: &AppConfig) -> Result<()> {
    let mut content = CONTAINER_CLAUDE_MD.to_string();

    let host_claude_md = config.claude_md_path();
    if host_claude_md.exists() {
        let existing = std::fs::read_to_string(&host_claude_md)
            .context("Failed to read existing CLAUDE.md")?;
        content.push('\n');
        content.push_str(&existing);
    }

    std::fs::write(&config.runtime_claude_md, content)
        .context("Failed to write runtime CLAUDE.md")?;

    Ok(())
}

fn generate_runtime_settings(config: &AppConfig, port: u16) -> Result<()> {
    let mut settings: serde_json::Value = if config.claude_settings_path().exists() {
        let raw = std::fs::read_to_string(config.claude_settings_path())
            .context("Failed to read settings.json")?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hook_command = format!(
        "curl -sf -X POST http://host.containers.internal:{}/notify || true",
        port
    );

    let stop_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": hook_command
        }]
    }]);

    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;

    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks.as_object_mut().context("hooks is not an object")?;
    hooks_obj.insert("Stop".to_string(), stop_hook);

    let output = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&config.runtime_settings, output).context("Failed to write runtime settings")?;

    Ok(())
}

fn create_container(
    config: &AppConfig,
    workspace: &Path,
    container_name: &str,
    port: u16,
) -> Result<()> {
    generate_runtime_claude_md(config)?;
    generate_runtime_settings(config, port)?;

    let workspace_str = workspace.to_string_lossy();
    let volume_name = format!("{}-data", container_name);

    let mut args: Vec<String> = vec![
        "run".into(),
        "-dit".into(),
        "--init".into(),
        "--name".into(),
        container_name.to_string(),
    ];

    // Workspace mount
    args.push("-v".into());
    args.push(format!("{}:/app:Z", workspace_str));

    // Persistent volume for Claude data
    args.push("-v".into());
    args.push(format!("{}:/home/claude/.claude", volume_name));

    // Host gateway
    args.push("--add-host=host.containers.internal:host-gateway".into());

    // Environment variables
    args.push("-e".into());
    args.push("HOST_GATEWAY=host.containers.internal".into());
    args.push("-e".into());
    args.push(format!(
        "NOTIFY_URL=http://host.containers.internal:{}/notify",
        port
    ));

    // Image
    args.push(crate::image::image_name(workspace));

    println!("{} {}", "Creating container:".blue().bold(), container_name);

    let status = Command::new("podman")
        .args(&args)
        .status()
        .context("Failed to create container")?;

    if !status.success() {
        anyhow::bail!("Failed to create container");
    }

    // Copy merged CLAUDE.md
    Command::new("podman")
        .args([
            "cp",
            &config.runtime_claude_md.to_string_lossy(),
            &format!("{}:/home/claude/.claude/CLAUDE.md", container_name),
        ])
        .status()
        .context("Failed to copy CLAUDE.md")?;

    // Copy merged settings.json
    Command::new("podman")
        .args([
            "cp",
            &config.runtime_settings.to_string_lossy(),
            &format!("{}:/home/claude/.claude/settings.json", container_name),
        ])
        .status()
        .context("Failed to copy settings.json")?;

    println!("{}", "Container created successfully.".green());

    Ok(())
}

fn attach_to_container(container_name: &str) -> Result<()> {
    println!(
        "{} {}",
        "Attaching to container:".blue().bold(),
        container_name
    );

    let status = Command::new("podman")
        .args(["attach", container_name])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("Failed to attach to container")?;

    if !status.success() {
        anyhow::bail!("Failed to attach to container");
    }

    Ok(())
}

fn start_container(container_name: &str) -> Result<()> {
    println!("{} {}", "Starting container:".blue().bold(), container_name);

    let status = Command::new("podman")
        .args(["start", container_name])
        .status()
        .context("Failed to start container")?;

    if !status.success() {
        anyhow::bail!("Failed to start container");
    }

    Ok(())
}

pub fn launch_container(config: &AppConfig, workspace: &Path, port: u16) -> Result<()> {
    let container_name = generate_container_name(workspace);

    if container_exists(&container_name)? {
        println!("{} {}", "Found existing container:".green(), container_name);

        if !container_is_running(&container_name)? {
            start_container(&container_name)?;
        }

        attach_to_container(&container_name)?;
    } else {
        create_container(config, workspace, &container_name, port)?;
        attach_to_container(&container_name)?;
    }

    Ok(())
}

pub fn list_containers() -> Result<()> {
    let output = Command::new("podman")
        .args([
            "ps",
            "-a",
            "--filter",
            "name=^claude-",
            "--format",
            "{{.Names}}\t{{.Status}}\t{{.CreatedAt}}",
        ])
        .output()
        .context("Failed to list containers")?;

    if output.stdout.is_empty() {
        println!("{}", "No claude containers found.".yellow());
    } else {
        println!("{}", "Claude containers:".blue().bold());
        println!("{:<20} {:<30} {}", "NAME", "STATUS", "CREATED");
        println!("{}", "-".repeat(80));
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_config(dir: &TempDir) -> AppConfig {
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            pid_file: config_dir.join("server.pid"),
            log_file: config_dir.join("server.log"),
            runtime_settings: config_dir.join("runtime-settings.json"),
            runtime_claude_md: config_dir.join("runtime-CLAUDE.md"),
            config_dir,
            home_dir: home,
        }
    }

    #[test]
    fn container_name_is_deterministic() {
        let path = Path::new("/home/user/myproject");
        assert_eq!(
            generate_container_name(path),
            generate_container_name(path)
        );
    }

    #[test]
    fn container_name_starts_with_claude() {
        let name = generate_container_name(Path::new("/home/user/myproject"));
        assert!(name.starts_with("claude-"));
    }

    #[test]
    fn container_name_has_expected_length() {
        // "claude-" (7) + hex of 6 bytes (12 chars) = 19
        let name = generate_container_name(Path::new("/home/user/myproject"));
        assert_eq!(name.len(), 19);
    }

    #[test]
    fn container_name_differs_for_different_paths() {
        let a = generate_container_name(Path::new("/home/user/project-a"));
        let b = generate_container_name(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn runtime_settings_contains_stop_hook() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config, 9876).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();

        let stop = &json["hooks"]["Stop"];
        assert!(stop.is_array(), "hooks.Stop should be an array");
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("9876"));
        assert!(cmd.contains("host.containers.internal"));
    }

    #[test]
    fn runtime_settings_uses_correct_port() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config, 1234).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        assert!(content.contains("1234"));
        assert!(!content.contains("9876"));
    }

    #[test]
    fn runtime_settings_preserves_existing_keys() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);

        // Write existing settings with a custom key
        let claude_dir = config.home_dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let existing = serde_json::json!({"theme": "dark", "verbosity": "verbose"});
        std::fs::write(
            config.claude_settings_path(),
            serde_json::to_string(&existing).unwrap(),
        )
        .unwrap();

        generate_runtime_settings(&config, 9876).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["theme"], "dark");
        assert_eq!(json["verbosity"], "verbose");
    }

    #[test]
    fn runtime_claude_md_contains_container_preamble() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_claude_md(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_claude_md).unwrap();
        assert!(content.contains("host.containers.internal"));
        assert!(content.contains("Podman container"));
    }

    #[test]
    fn runtime_claude_md_appends_existing_claude_md() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);

        let claude_dir = config.home_dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            config.claude_md_path(),
            "# My Rules\nAlways use Rust.\n",
        )
        .unwrap();

        generate_runtime_claude_md(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_claude_md).unwrap();
        assert!(content.contains("host.containers.internal"));
        assert!(content.contains("My Rules"));
        assert!(content.contains("Always use Rust."));
    }

    #[test]
    fn runtime_claude_md_without_existing_file_does_not_error() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        // No CLAUDE.md exists — should still succeed
        generate_runtime_claude_md(&config).unwrap();
        assert!(config.runtime_claude_md.exists());
    }
}

pub fn clean_container(workspace: &Path) -> Result<()> {
    let container_name = generate_container_name(workspace);

    if !container_exists(&container_name)? {
        println!(
            "{} {}",
            "Container does not exist:".yellow(),
            container_name
        );
        return Ok(());
    }

    println!("{} {}", "Removing container:".red().bold(), container_name);

    // Stop if running
    if container_is_running(&container_name)? {
        Command::new("podman")
            .args(["stop", &container_name])
            .status()
            .context("Failed to stop container")?;
    }

    // Remove container
    Command::new("podman")
        .args(["rm", &container_name])
        .status()
        .context("Failed to remove container")?;

    // Remove associated volume
    let volume_name = format!("{}-data", container_name);
    let _ = Command::new("podman")
        .args(["volume", "rm", &volume_name])
        .status();

    println!("{}", "Container removed.".green());

    Ok(())
}
