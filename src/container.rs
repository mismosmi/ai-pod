use anyhow::{Context, Result};
use colored::Colorize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::AppConfig;
use crate::workspace::{container_name as gen_container_name, volume_name as gen_volume_name};

const CONTAINER_CLAUDE_MD: &str = r#"# Container Environment
You are running inside a Podman container. To reach services on the host machine,
use `host.containers.internal` instead of `localhost`.

For example: `curl http://host.containers.internal:3000`

Working directory: /app
"#;

/// Setup script: installs Claude Code.
const SETUP_SCRIPT: &str = r#"#!/bin/sh
set -e
export PATH="$HOME/.local/bin:$PATH"
curl -fsSL https://claude.ai/install.sh | bash
"#;

const SKILL_MD: &str = r#"---
name: ai-pod
description: This skill should be used when the user asks to run a command on the host machine, open an application on the host, send a desktop notification to the user, or list previously approved host commands. Provides the host-tools binary at /home/claude/.local/bin/host-tools.
version: 0.1.0
---
# host-tools — Host Interaction

`/home/claude/.local/bin/host-tools` interacts with the host machine from inside this container.

## run-command

Run a shell command on the host. The host user is prompted to approve commands not previously allowed. Output streams back in real time.

    host-tools run-command <shell command and args>

Examples:
- `host-tools run-command ls ~/Desktop`
- `host-tools run-command open https://example.com`

List previously approved commands:

    host-tools run-command --list

Use only for tasks that require host-side effects. Prefer doing things inside the container.

## notify-user

Send a desktop notification to the host user. The notification title is set automatically to the project name.

    host-tools notify-user "<message>"

Example: `host-tools notify-user "Build finished successfully"`

A Stop hook already calls this automatically when the session ends.
"#;

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

fn volume_exists(name: &str) -> Result<bool> {
    let status = Command::new("podman")
        .args(["volume", "exists", name])
        .status()
        .context("Failed to check if volume exists")?;
    Ok(status.success())
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

fn generate_runtime_settings(config: &AppConfig) -> Result<()> {
    let mut settings: serde_json::Value = if config.claude_settings_path().exists() {
        let raw = std::fs::read_to_string(config.claude_settings_path())
            .context("Failed to read settings.json")?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let hook_command =
        r#"/home/claude/.local/bin/host-tools notify-user "Task completed" || true"#.to_string();

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

    // Set default permission mode — no per-tool prompts in TUI
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let perms_obj = permissions
        .as_object_mut()
        .context("permissions is not an object")?;
    perms_obj.insert(
        "defaultMode".to_string(),
        serde_json::Value::String("bypassPermissions".to_string()),
    );

    let output = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&config.runtime_settings, output).context("Failed to write runtime settings")?;

    Ok(())
}

async fn ensure_host_tools_binary(config: &AppConfig) -> Result<PathBuf> {
    let cache_path = config
        .config_dir
        .join(format!("host-tools-v{}", env!("CARGO_PKG_VERSION")));
    if cache_path.exists() {
        return Ok(cache_path);
    }

    #[cfg(target_arch = "x86_64")]
    let arch = "x86_64";
    #[cfg(target_arch = "aarch64")]
    let arch = "aarch64";

    let url = format!(
        "https://github.com/mismosmi/ai-pod/releases/download/v{}/host-tools-linux-{}",
        env!("CARGO_PKG_VERSION"),
        arch
    );

    let response = reqwest::get(&url)
        .await
        .context("Failed to download host-tools binary")?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to download host-tools: HTTP {}", response.status());
    }

    let bytes = response
        .bytes()
        .await
        .context("Failed to read host-tools binary")?;
    std::fs::write(&cache_path, &bytes).context("Failed to write host-tools binary")?;

    // chmod 755
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cache_path, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(cache_path)
}

/// Run the setup script inside a temporary container with the home volume mounted.
/// Installs Claude Code.
fn run_setup_script(volume_name: &str, image: &str) -> Result<()> {
    println!(
        "{}",
        "Running setup script (installing Claude Code)...".blue()
    );

    let mut child = Command::new("podman")
        .args([
            "run",
            "--rm",
            "--user",
            "claude",
            "-v",
            &format!("{}:/home/claude:z", volume_name),
            "--add-host=host.containers.internal:host-gateway",
            "-i",
            image,
            "sh",
            "-s",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to spawn setup script container")?;

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(SETUP_SCRIPT.as_bytes())
        .context("Failed to write setup script")?;

    let status = child.wait().context("Setup script container failed")?;
    if !status.success() {
        anyhow::bail!("Setup script exited with non-zero status");
    }

    println!("{}", "Setup complete.".green());
    Ok(())
}

/// Initialize a named home volume for the first time.
async fn init_home_volume(
    config: &AppConfig,
    volume_name: &str,
    container_name: &str,
    image: &str,
    project_id: &str,
    api_key: &str,
) -> Result<()> {
    println!(
        "{} {}",
        "Initialising home volume:".blue().bold(),
        volume_name
    );

    // 1. Create the volume
    let status = Command::new("podman")
        .args(["volume", "create", volume_name])
        .status()
        .context("Failed to create volume")?;
    if !status.success() {
        anyhow::bail!("Failed to create volume {}", volume_name);
    }

    // 2. Create a stopped container for cp operations
    let init_container = format!("{}-init", container_name);
    let status = Command::new("podman")
        .args([
            "create",
            "--name",
            &init_container,
            "-v",
            &format!("{}:/home/claude", volume_name),
            image,
            "claude",
        ])
        .status()
        .context("Failed to create init container")?;
    if !status.success() {
        anyhow::bail!("Failed to create init container");
    }

    // 3. Copy ~/.claude.json (soft error — auth state)
    let claude_json = config.home_dir.join(".claude.json");
    if claude_json.exists() {
        let _ = Command::new("podman")
            .args([
                "cp",
                &claude_json.to_string_lossy(),
                &format!("{}:/home/claude/", init_container),
            ])
            .status();
    }

    // 3b. Ensure required directories exist in the volume
    let _ = Command::new("podman")
        .args([
            "run",
            "--rm",
            "--user",
            "claude",
            "-v",
            &format!("{}:/home/claude:z", volume_name),
            image,
            "mkdir",
            "-p",
            "/home/claude/.claude",
            "/home/claude/.claude/skills/ai-pod",
            "/home/claude/.local/bin",
        ])
        .status();

    // 4. Generate and copy runtime config
    generate_runtime_claude_md(config)?;
    generate_runtime_settings(config)?;

    let _ = Command::new("podman")
        .args([
            "cp",
            &config.runtime_settings.to_string_lossy(),
            &format!("{}:/home/claude/.claude/settings.json", init_container),
        ])
        .status();

    let _ = Command::new("podman")
        .args([
            "cp",
            &config.runtime_claude_md.to_string_lossy(),
            &format!("{}:/home/claude/.claude/CLAUDE.md", init_container),
        ])
        .status();

    // 5. Copy host-tools binary and skill
    if let Ok(host_tools) = ensure_host_tools_binary(config).await {
        let _ = Command::new("podman")
            .args([
                "cp",
                host_tools.to_str().unwrap(),
                &format!("{}:/home/claude/.local/bin/host-tools", init_container),
            ])
            .status();
    }

    let skill_path = config.config_dir.join("skill.md");
    std::fs::write(&skill_path, SKILL_MD)?;
    let _ = Command::new("podman")
        .args([
            "cp",
            skill_path.to_str().unwrap(),
            &format!(
                "{}:/home/claude/.claude/skills/ai-pod/SKILL.md",
                init_container
            ),
        ])
        .status();

    // 6. Remove init container
    let _ = Command::new("podman")
        .args(["rm", &init_container])
        .status();

    // 7. Run setup script — installs Claude
    run_setup_script(volume_name, image)?;

    let _ = (project_id, api_key); // used via env vars at runtime

    println!("{}", "Home volume initialised.".green());

    Ok(())
}

/// Re-apply runtime config and re-run setup after a rebuild.
/// Does NOT wipe the volume — auth state is preserved.
async fn reseed_home_volume(
    config: &AppConfig,
    volume_name: &str,
    container_name: &str,
    image: &str,
    project_id: &str,
    api_key: &str,
) -> Result<()> {
    println!(
        "{} {}",
        "Refreshing home volume config:".blue().bold(),
        volume_name
    );

    // 1. Create a stopped container for cp operations
    let init_container = format!("{}-init", container_name);
    let status = Command::new("podman")
        .args([
            "create",
            "--name",
            &init_container,
            "-v",
            &format!("{}:/home/claude", volume_name),
            image,
            "claude",
        ])
        .status()
        .context("Failed to create init container for reseed")?;
    if !status.success() {
        anyhow::bail!("Failed to create init container for reseed");
    }

    // 2. Ensure required directories exist in the volume
    let _ = Command::new("podman")
        .args([
            "run",
            "--rm",
            "--user",
            "claude",
            "-v",
            &format!("{}:/home/claude:z", volume_name),
            image,
            "mkdir",
            "-p",
            "/home/claude/.claude",
            "/home/claude/.claude/skills/ai-pod",
            "/home/claude/.local/bin",
        ])
        .status();

    // 2b. Regenerate and copy runtime config (refreshes hooks + permissions)
    generate_runtime_claude_md(config)?;
    generate_runtime_settings(config)?;

    let _ = Command::new("podman")
        .args([
            "cp",
            &config.runtime_settings.to_string_lossy(),
            &format!("{}:/home/claude/.claude/settings.json", init_container),
        ])
        .status();

    let _ = Command::new("podman")
        .args([
            "cp",
            &config.runtime_claude_md.to_string_lossy(),
            &format!("{}:/home/claude/.claude/CLAUDE.md", init_container),
        ])
        .status();

    // 3. Copy host-tools binary and skill
    if let Ok(host_tools) = ensure_host_tools_binary(config).await {
        let _ = Command::new("podman")
            .args([
                "cp",
                host_tools.to_str().unwrap(),
                &format!("{}:/home/claude/.local/bin/host-tools", init_container),
            ])
            .status();
    }

    let skill_path = config.config_dir.join("skill.md");
    std::fs::write(&skill_path, SKILL_MD)?;
    let _ = Command::new("podman")
        .args([
            "cp",
            skill_path.to_str().unwrap(),
            &format!(
                "{}:/home/claude/.claude/skills/ai-pod/SKILL.md",
                init_container
            ),
        ])
        .status();

    // 4. Remove init container
    let _ = Command::new("podman")
        .args(["rm", &init_container])
        .status();

    // 5. Run setup script — updates Claude
    run_setup_script(volume_name, image)?;

    let _ = (project_id, api_key); // used via env vars at runtime

    println!("{}", "Home volume reseeded.".green());

    Ok(())
}

pub async fn launch_container(
    config: &AppConfig,
    workspace: &Path,
    rebuild: bool,
    image: &str,
    project_id: &str,
    api_key: &str,
) -> Result<()> {
    let container_name = gen_container_name(workspace);
    let volume_name = gen_volume_name(workspace);
    let workspace_str = workspace.to_string_lossy();

    // Handle rebuild: remove the container (but keep volume)
    if rebuild && container_exists(&container_name)? {
        println!(
            "{} {}",
            "Removing container for rebuild:".blue().bold(),
            container_name
        );
        let _ = Command::new("podman")
            .args(["rm", "--force", &container_name])
            .status();
    }

    // On rebuild, reseed the volume from the new image and regenerate runtime settings
    if rebuild && volume_exists(&volume_name)? {
        reseed_home_volume(
            config,
            &volume_name,
            &container_name,
            image,
            project_id,
            api_key,
        )
        .await?;
    }

    // Init home volume if it doesn't exist
    if !volume_exists(&volume_name)? {
        init_home_volume(
            config,
            &volume_name,
            &container_name,
            image,
            project_id,
            api_key,
        )
        .await?;
    }

    if container_is_running(&container_name)? {
        // Reconnect to existing running container
        println!(
            "{} {}",
            "Attaching to running container:".green(),
            container_name
        );
        Command::new("podman")
            .args(["attach", &container_name])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to attach to container")?;
    } else {
        // Clean up stale stopped container if one exists
        if container_exists(&container_name)? {
            let _ = Command::new("podman")
                .args(["rm", &container_name])
                .status();
        }

        println!("{} {}", "Starting container:".blue().bold(), container_name);

        Command::new("podman")
            .args([
                "run",
                "--rm",
                "-it",
                "--name",
                &container_name,
                "-v",
                &format!("{}:/home/claude:z", volume_name),
                "-v",
                &format!("{}:/app:Z", workspace_str),
                "--add-host=host.containers.internal:host-gateway",
                "-e",
                "HOST_GATEWAY=host.containers.internal",
                "-e",
                &format!("AI_POD_PROJECT_ID={}", project_id),
                "-e",
                &format!("AI_POD_API_KEY={}", api_key),
                "-e",
                "AI_POD_SERVER_URL=http://host.containers.internal:7822",
                image,
                "claude",
            ])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to run container")?;
    }

    Ok(())
}

pub async fn run_in_container(
    config: &AppConfig,
    workspace: &Path,
    image: &str,
    project_id: &str,
    api_key: &str,
    command: &str,
    args: &[String],
) -> Result<()> {
    let container_name = gen_container_name(workspace);
    let volume_name = gen_volume_name(workspace);
    let workspace_str = workspace.to_string_lossy();

    // Init home volume if it doesn't exist
    if !volume_exists(&volume_name)? {
        init_home_volume(
            config,
            &volume_name,
            &container_name,
            image,
            project_id,
            api_key,
        )
        .await?;
    }

    println!(
        "{} {} {}",
        "Running in container:".blue().bold(),
        container_name,
        command
    );

    let mut run_args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-it".into(),
        "-v".into(),
        format!("{}:/home/claude:z", volume_name),
        "-v".into(),
        format!("{}:/app:Z", workspace_str),
        "--add-host=host.containers.internal:host-gateway".into(),
        "-e".into(),
        "HOST_GATEWAY=host.containers.internal".into(),
        "-e".into(),
        format!("AI_POD_PROJECT_ID={}", project_id),
        "-e".into(),
        format!("AI_POD_API_KEY={}", api_key),
        "-e".into(),
        "AI_POD_SERVER_URL=http://host.containers.internal:7822".into(),
        "--entrypoint".into(),
        command.to_string(),
        image.to_string(),
    ];
    run_args.extend_from_slice(args);

    let status = Command::new("podman")
        .args(&run_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run command in container")?;

    if !status.success() {
        anyhow::bail!("Command exited with non-zero status");
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

pub fn clean_container(workspace: &Path) -> Result<()> {
    let container_name = gen_container_name(workspace);
    let volume_name = gen_volume_name(workspace);

    let container_existed = container_exists(&container_name)?;

    if container_existed {
        println!("{} {}", "Removing container:".red().bold(), container_name);

        if container_is_running(&container_name)? {
            Command::new("podman")
                .args(["stop", &container_name])
                .status()
                .context("Failed to stop container")?;
        }

        Command::new("podman")
            .args(["rm", &container_name])
            .status()
            .context("Failed to remove container")?;

        println!("{}", "Container removed.".green());
    } else {
        println!(
            "{} {}",
            "Container does not exist:".yellow(),
            container_name
        );
    }

    // Remove named home volume
    if volume_exists(&volume_name)? {
        println!("{} {}", "Removing volume:".red().bold(), volume_name);
        let status = Command::new("podman")
            .args(["volume", "rm", &volume_name])
            .status()
            .context("Failed to remove volume")?;
        if status.success() {
            println!("{}", "Volume removed.".green());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{container_name, volume_name};
    use std::path::Path;
    use tempfile::TempDir;

    fn make_test_config(dir: &TempDir) -> AppConfig {
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            runtime_claude_md: config_dir.join("runtime-CLAUDE.md"),
            config_dir,
            home_dir: home,
        }
    }

    #[test]
    fn container_name_is_deterministic() {
        let path = Path::new("/home/user/myproject");
        assert_eq!(container_name(path), container_name(path));
    }

    #[test]
    fn container_name_starts_with_claude() {
        let name = container_name(Path::new("/home/user/myproject"));
        assert!(name.starts_with("claude-"));
    }

    #[test]
    fn container_name_has_expected_length() {
        let name = container_name(Path::new("/home/user/myproject"));
        assert_eq!(name.len(), 19);
    }

    #[test]
    fn container_name_differs_for_different_paths() {
        let a = container_name(Path::new("/home/user/project-a"));
        let b = container_name(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn volume_name_matches_container_hash() {
        let path = Path::new("/home/user/myproject");
        let cname = container_name(path);
        let vname = volume_name(path);
        assert_eq!(vname, format!("{}-home", cname));
    }

    #[test]
    fn volume_name_differs_for_different_paths() {
        let a = volume_name(Path::new("/home/user/project-a"));
        let b = volume_name(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn runtime_settings_contains_stop_hook() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();

        let stop = &json["hooks"]["Stop"];
        assert!(stop.is_array(), "hooks.Stop should be an array");
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("host-tools"));
        assert!(cmd.contains("notify-user"));
    }

    #[test]
    fn runtime_settings_stop_hook_calls_host_tools() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let cmd = json["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("host-tools"));
    }

    #[test]
    fn runtime_settings_contains_default_mode_bypass_permissions() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["permissions"]["defaultMode"], "bypassPermissions");
    }

    #[test]
    fn runtime_settings_does_not_contain_mcp_servers() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(json["mcpServers"].is_null(), "mcpServers should not be set");
    }

    #[test]
    fn runtime_settings_preserves_existing_keys() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);

        let claude_dir = config.home_dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let existing = serde_json::json!({"theme": "dark", "verbosity": "verbose"});
        std::fs::write(
            config.claude_settings_path(),
            serde_json::to_string(&existing).unwrap(),
        )
        .unwrap();

        generate_runtime_settings(&config).unwrap();

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
        std::fs::write(config.claude_md_path(), "# My Rules\nAlways use Rust.\n").unwrap();

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
        generate_runtime_claude_md(&config).unwrap();
        assert!(config.runtime_claude_md.exists());
    }
}
