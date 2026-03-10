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

fn generate_volume_name(workspace: &Path) -> String {
    let workspace_str = workspace.to_string_lossy();
    let hash = Sha256::digest(workspace_str.as_bytes());
    let short_hash = hex::encode(&hash[..6]);
    format!("claude-{}-home", short_hash)
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

/// Initialize a named home volume for the first time.
/// Creates skeleton dirs, copies host ~/.claude.json and ~/.claude/, and injects runtime config.
fn init_home_volume(
    config: &AppConfig,
    volume_name: &str,
    container_name: &str,
    image: &str,
    port: u16,
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

    // 2. Seed the volume from the image's /home/claude (preserves claude install).
    //    Mount at /mnt/claude-home so the image's /home/claude stays visible, then cp into it.
    let status = Command::new("podman")
        .args([
            "run",
            "--rm",
            "--user",
            "root",
            "--entrypoint",
            "/bin/sh",
            "-v",
            &format!("{}:/mnt/claude-home", volume_name),
            image,
            "-c",
            "cp -a /home/claude/. /mnt/claude-home/ && mkdir -p /mnt/claude-home/.claude && chown -R claude:claude /mnt/claude-home",
        ])
        .status()
        .context("Failed to seed home volume from image")?;
    if !status.success() {
        anyhow::bail!("Failed to seed home volume from image");
    }

    // 3. Create a stopped container for cp operations
    let init_container = format!("{}-init", container_name);
    let status = Command::new("podman")
        .args([
            "create",
            "--name",
            &init_container,
            "-v",
            &format!("{}:/home/claude", volume_name),
            image,
        ])
        .status()
        .context("Failed to create init container")?;
    if !status.success() {
        anyhow::bail!("Failed to create init container");
    }

    // 4. Copy ~/.claude.json (soft error)
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

    // 5. Copy ~/.claude/ (soft error)
    let claude_dir = config.home_dir.join(".claude");
    if claude_dir.exists() {
        let _ = Command::new("podman")
            .args([
                "cp",
                &format!("{}/.", claude_dir.to_string_lossy()),
                &format!("{}:/home/claude/.claude/", init_container),
            ])
            .status();
    }

    // 6. Generate and copy runtime config
    generate_runtime_claude_md(config)?;
    generate_runtime_settings(config, port)?;

    let _ = Command::new("podman")
        .args([
            "cp",
            &config.runtime_claude_md.to_string_lossy(),
            &format!("{}:/home/claude/.claude/CLAUDE.md", init_container),
        ])
        .status();

    let _ = Command::new("podman")
        .args([
            "cp",
            &config.runtime_settings.to_string_lossy(),
            &format!("{}:/home/claude/.claude/settings.json", init_container),
        ])
        .status();

    // 7. Remove init container
    let _ = Command::new("podman")
        .args(["rm", &init_container])
        .status();

    println!("{}", "Home volume initialised.".green());

    Ok(())
}

pub fn launch_container(
    config: &AppConfig,
    workspace: &Path,
    port: u16,
    rebuild: bool,
    image: &str,
) -> Result<()> {
    let container_name = generate_container_name(workspace);
    let volume_name = generate_volume_name(workspace);
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

    // Init home volume if it doesn't exist
    if !volume_exists(&volume_name)? {
        init_home_volume(config, &volume_name, &container_name, image, port)?;
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
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Failed to attach to container")?;
        // Non-zero exits (detach=0, ctrl+c=130) are intentionally ignored
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
                &format!("NOTIFY_URL=http://host.containers.internal:{}/notify", port),
                image,
            ])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .context("Failed to run container")?;
        // Non-zero exits intentionally ignored
    }

    Ok(())
}

pub fn run_in_container(
    config: &AppConfig,
    workspace: &Path,
    port: u16,
    command: &str,
    args: &[String],
) -> Result<()> {
    let container_name = generate_container_name(workspace);
    let volume_name = generate_volume_name(workspace);
    let image = crate::image::image_name(workspace);
    let workspace_str = workspace.to_string_lossy();

    // Init home volume if it doesn't exist
    if !volume_exists(&volume_name)? {
        init_home_volume(config, &volume_name, &container_name, &image, port)?;
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
        format!("{}:/home/claude", volume_name),
        "-v".into(),
        format!("{}:/app:Z", workspace_str),
        "--add-host=host.containers.internal:host-gateway".into(),
        "-e".into(),
        "HOST_GATEWAY=host.containers.internal".into(),
        "-e".into(),
        format!("NOTIFY_URL=http://host.containers.internal:{}/notify", port),
        "--entrypoint".into(),
        command.to_string(),
        image,
    ];
    run_args.extend_from_slice(args);

    let status = Command::new("podman")
        .args(&run_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
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
    let container_name = generate_container_name(workspace);
    let volume_name = generate_volume_name(workspace);

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
        assert_eq!(generate_container_name(path), generate_container_name(path));
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
    fn volume_name_matches_container_hash() {
        let path = Path::new("/home/user/myproject");
        let container = generate_container_name(path);
        let volume = generate_volume_name(path);
        // Volume name should be container name + "-home"
        assert_eq!(volume, format!("{}-home", container));
    }

    #[test]
    fn volume_name_differs_for_different_paths() {
        let a = generate_volume_name(Path::new("/home/user/project-a"));
        let b = generate_volume_name(Path::new("/home/user/project-b"));
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
        // No CLAUDE.md exists — should still succeed
        generate_runtime_claude_md(&config).unwrap();
        assert!(config.runtime_claude_md.exists());
    }
}
