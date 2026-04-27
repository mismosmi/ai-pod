use anyhow::{Context, Result};
use askama::Template;
use colored::Colorize;
use dialoguer;
use std::path::Path;
use std::process::Stdio;

use crate::config::AppConfig;
use crate::runtime::ContainerRuntime;
use crate::workspace::{container_prefix, new_container_name, volume_name as gen_volume_name};

#[derive(Template)]
#[template(path = "container_claude_md.txt")]
struct ContainerClaudeMd<'a> {
    display_name: &'a str,
    host_gateway: &'a str,
}

fn container_claude_md(rt: &ContainerRuntime) -> String {
    let tmpl = ContainerClaudeMd {
        display_name: rt.display_name(),
        host_gateway: rt.host_gateway(),
    };
    tmpl.render()
        .expect("failed to render container CLAUDE.md template")
}

const SKILL_MD: &str = include_str!("../templates/skill.md");

/// Home directory of the `ai-pod` user inside every container image.
/// The Dockerfile template creates this user with this home path, so the
/// runtime does not need to probe the image.
const CONTAINER_HOME: &str = "/home/ai-pod";

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

pub fn volume_exists(rt: &ContainerRuntime, name: &str) -> Result<bool> {
    let status = rt
        .command()
        .args(["volume", "inspect", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to check if volume exists")?;
    Ok(status.success())
}

fn generate_runtime_claude_md(rt: &ContainerRuntime, config: &AppConfig) -> Result<()> {
    let mut content = container_claude_md(rt);

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

    let stop_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": "/usr/local/bin/host-tools daemon stop-all || true; /usr/local/bin/host-tools notify-user \"Task completed\" || true"
        }]
    }]);

    let permission_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": "/usr/local/bin/host-tools notify-user \"Claude needs your approval\" || true"
        }]
    }]);

    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;

    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks.as_object_mut().context("hooks is not an object")?;
    hooks_obj.insert("Stop".to_string(), stop_hook);
    hooks_obj.insert("PermissionRequest".to_string(), permission_hook);

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

fn read_git_global(key: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["config", "--global", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Copy the host user's git identity into the container volume as ~/.gitconfig.
/// This overrides the system-level git config set in the Dockerfile.
fn write_gitconfig_to_volume(
    rt: &ContainerRuntime,
    config: &AppConfig,
    init_container: &str,
) -> Result<()> {
    let name = read_git_global("user.name");
    let email = read_git_global("user.email");
    if name.is_none() && email.is_none() {
        return Ok(());
    }

    let mut lines = vec!["[user]".to_string()];
    if let Some(n) = name {
        lines.push(format!("\tname = {}", n));
    }
    if let Some(e) = email {
        lines.push(format!("\temail = {}", e));
    }

    let tmp = config.config_dir.join("gitconfig.tmp");
    std::fs::write(&tmp, lines.join("\n") + "\n")?;
    let _ = rt
        .command()
        .args([
            "cp",
            tmp.to_str().unwrap(),
            &format!("{}:{}/.gitconfig", init_container, CONTAINER_HOME),
        ])
        .status();
    Ok(())
}

/// Populate a home volume via a temporary stopped container.
/// Handles directory creation, runtime config, skill file, opencode config, and git identity.
/// Set `copy_claude_json` to copy `~/.claude.json` (first-time init only; skipped on reseed).
fn seed_home_volume(
    rt: &ContainerRuntime,
    config: &AppConfig,
    volume_name: &str,
    container_name: &str,
    image: &str,
    copy_claude_json: bool,
) -> Result<()> {
    let init_container = format!("{}-init", container_name);
    let status = rt
        .command()
        .args([
            "create",
            "--name",
            &init_container,
            "-v",
            &format!("{}:{}", volume_name, CONTAINER_HOME),
            image,
            "true",
        ])
        .status()
        .context("Failed to create init container")?;
    if !status.success() {
        anyhow::bail!("Failed to create init container");
    }

    if copy_claude_json {
        let claude_json = config.home_dir.join(".claude.json");
        if claude_json.exists() {
            let _ = rt
                .command()
                .args([
                    "cp",
                    &claude_json.to_string_lossy(),
                    &format!("{}:{}/", init_container, CONTAINER_HOME),
                ])
                .status();
        }
    }

    let _ = rt
        .command()
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:{}:z", volume_name, CONTAINER_HOME),
            image,
            "mkdir",
            "-p",
            &format!("{}/.claude", CONTAINER_HOME),
            &format!("{}/.claude/skills/ai-pod", CONTAINER_HOME),
            &format!("{}/.config", CONTAINER_HOME),
        ])
        .status();

    generate_runtime_claude_md(rt, config)?;
    generate_runtime_settings(config)?;

    let _ = rt
        .command()
        .args([
            "cp",
            &config.runtime_settings.to_string_lossy(),
            &format!("{}:{}/.claude/settings.json", init_container, CONTAINER_HOME),
        ])
        .status();

    let _ = rt
        .command()
        .args([
            "cp",
            &config.runtime_claude_md.to_string_lossy(),
            &format!("{}:{}/.claude/CLAUDE.md", init_container, CONTAINER_HOME),
        ])
        .status();

    let skill_path = config.config_dir.join("skill.md");
    std::fs::write(&skill_path, SKILL_MD)?;
    let _ = rt
        .command()
        .args([
            "cp",
            skill_path.to_str().unwrap(),
            &format!(
                "{}:{}/.claude/skills/ai-pod/SKILL.md",
                init_container, CONTAINER_HOME
            ),
        ])
        .status();

    let opencode_config = config.home_dir.join(".config").join("opencode");
    if opencode_config.exists() {
        let _ = rt
            .command()
            .args([
                "cp",
                &opencode_config.to_string_lossy(),
                &format!("{}:{}/.config/", init_container, CONTAINER_HOME),
            ])
            .status();
    }

    write_gitconfig_to_volume(rt, config, &init_container)?;

    let _ = rt.command().args(["rm", &init_container]).status();

    Ok(())
}

/// Initialize a named home volume for the first time.
fn init_home_volume(
    rt: &ContainerRuntime,
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

    let status = rt
        .command()
        .args(["volume", "create", volume_name])
        .status()
        .context("Failed to create volume")?;
    if !status.success() {
        anyhow::bail!("Failed to create volume {}", volume_name);
    }

    seed_home_volume(rt, config, volume_name, container_name, image, true)?;

    let _ = (project_id, api_key); // used via env vars at runtime

    println!("{}", "Home volume initialised.".green());

    Ok(())
}

/// Re-apply runtime config after a rebuild.
/// Does NOT wipe the volume — auth state is preserved.
fn reseed_home_volume(
    rt: &ContainerRuntime,
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

    seed_home_volume(rt, config, volume_name, container_name, image, false)?;

    let _ = (project_id, api_key); // used via env vars at runtime

    println!("{}", "Home volume reseeded.".green());

    Ok(())
}

pub fn launch_container(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    rebuild: bool,
    image: &str,
    project_id: &str,
    api_key: &str,
) -> Result<()> {
    let prefix = container_prefix(workspace);
    let volume_name = gen_volume_name(workspace);
    let workspace_str = workspace.to_string_lossy();

    // On rebuild: stop all existing containers for this workspace and reseed the volume
    if rebuild {
        for name in containers_for_prefix(rt, &prefix, false)? {
            println!(
                "{} {}",
                "Removing container for rebuild:".blue().bold(),
                name
            );
            let _ = rt.command().args(["rm", "--force", &name]).status();
        }
        if volume_exists(rt, &volume_name)? {
            reseed_home_volume(
                rt,
                config,
                &volume_name,
                &prefix,
                image,
                project_id,
                api_key,
            )?;
        }
    }

    // Init home volume if it doesn't exist
    if !volume_exists(rt, &volume_name)? {
        init_home_volume(
            rt,
            config,
            &volume_name,
            &prefix,
            image,
            project_id,
            api_key,
        )?;
    }

    let container_name = new_container_name(workspace);
    println!("{} {}", "Starting container:".blue().bold(), container_name);

    let add_host = rt.add_host_arg();
    let host_gw_env = format!("HOST_GATEWAY={}", rt.host_gateway());
    let server_url_env = format!("AI_POD_SERVER_URL={}", rt.server_url());

    let mut run_cmd = rt.command();
    run_cmd.args(["run", "--rm", "-it"]);
    run_cmd.args([
        "--name",
        &container_name,
        "--label",
        "managed-by=ai-pod",
        "-v",
        &format!("{}:{}:z", volume_name, CONTAINER_HOME),
        "-v",
        &format!("{}:/app:Z", workspace_str),
        &add_host,
        "-e",
        &host_gw_env,
        "-e",
        &format!("AI_POD_PROJECT_ID={}", project_id),
        "-e",
        &format!("AI_POD_API_KEY={}", api_key),
        "-e",
        &server_url_env,
    ]);
    run_cmd.arg(image);
    run_cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run container")?;

    Ok(())
}

pub fn run_in_container(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    image: &str,
    project_id: &str,
    api_key: &str,
    command: &str,
    args: &[String],
) -> Result<()> {
    let container_name = new_container_name(workspace);
    let volume_name = gen_volume_name(workspace);
    let workspace_str = workspace.to_string_lossy();

    // Init home volume if it doesn't exist
    if !volume_exists(rt, &volume_name)? {
        init_home_volume(
            rt,
            config,
            &volume_name,
            &container_name,
            image,
            project_id,
            api_key,
        )?;
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
    ];
    run_args.extend_from_slice(&[
        "--label".into(),
        "managed-by=ai-pod".into(),
        "-v".into(),
        format!("{}:{}:z", volume_name, CONTAINER_HOME),
        "-v".into(),
        format!("{}:/app:Z", workspace_str),
        rt.add_host_arg(),
        "-e".into(),
        format!("HOST_GATEWAY={}", rt.host_gateway()),
        "-e".into(),
        format!("AI_POD_PROJECT_ID={}", project_id),
        "-e".into(),
        format!("AI_POD_API_KEY={}", api_key),
        "-e".into(),
        format!("AI_POD_SERVER_URL={}", rt.server_url()),
        "--entrypoint".into(),
        command.to_string(),
        image.to_string(),
    ]);
    run_args.extend_from_slice(args);

    let status = rt
        .command()
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

pub fn clean_container(rt: &ContainerRuntime, workspace: &Path) -> Result<()> {
    let prefix = container_prefix(workspace);
    let volume_name = gen_volume_name(workspace);

    let containers = containers_for_prefix(rt, &prefix, false)?;

    if containers.is_empty() {
        println!("{}", "No containers found for this workspace.".yellow());
    } else {
        for name in &containers {
            println!("{} {}", "Removing container:".red().bold(), name);
            let _ = rt.command().args(["rm", "--force", name]).status();
        }
        println!("{}", "Containers removed.".green());
    }

    // Remove named home volume
    if volume_exists(rt, &volume_name)? {
        println!("{} {}", "Removing volume:".red().bold(), volume_name);
        let status = rt
            .command()
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
    use crate::runtime::RuntimeKind;
    use crate::workspace::{container_prefix, new_container_name, volume_name};
    use std::path::Path;
    use tempfile::TempDir;

    fn test_runtime() -> ContainerRuntime {
        ContainerRuntime {
            kind: RuntimeKind::Podman,
            dry_run: false,
        }
    }

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
    fn container_prefix_is_deterministic() {
        let path = Path::new("/home/user/myproject");
        assert_eq!(container_prefix(path), container_prefix(path));
    }

    #[test]
    fn container_prefix_starts_with_ai_pod() {
        let name = container_prefix(Path::new("/home/user/myproject"));
        assert!(name.starts_with("ai-pod-"));
    }

    #[test]
    fn new_container_name_starts_with_prefix() {
        let path = Path::new("/home/user/myproject");
        assert!(new_container_name(path).starts_with(&container_prefix(path)));
    }

    #[test]
    fn new_container_name_is_unique() {
        let path = Path::new("/home/user/myproject");
        assert_ne!(new_container_name(path), new_container_name(path));
    }

    #[test]
    fn container_prefix_differs_for_different_paths() {
        let a = container_prefix(Path::new("/home/user/project-a"));
        let b = container_prefix(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn volume_name_matches_container_prefix() {
        let path = Path::new("/home/user/myproject");
        let vname = volume_name(path);
        assert_eq!(vname, format!("{}-home", container_prefix(path)));
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
        let rt = test_runtime();
        generate_runtime_claude_md(&rt, &config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_claude_md).unwrap();
        assert!(content.contains("host.containers.internal"));
        assert!(content.contains("Podman container"));
    }

    #[test]
    fn runtime_claude_md_contains_docker_preamble() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        let rt = ContainerRuntime {
            kind: RuntimeKind::Docker,
            dry_run: false,
        };
        generate_runtime_claude_md(&rt, &config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_claude_md).unwrap();
        assert!(content.contains("host.docker.internal"));
        assert!(content.contains("Docker container"));
    }

    #[test]
    fn runtime_claude_md_appends_existing_claude_md() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        let rt = test_runtime();

        let claude_dir = config.home_dir.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(config.claude_md_path(), "# My Rules\nAlways use Rust.\n").unwrap();

        generate_runtime_claude_md(&rt, &config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_claude_md).unwrap();
        assert!(content.contains("host.containers.internal"));
        assert!(content.contains("My Rules"));
        assert!(content.contains("Always use Rust."));
    }

    #[test]
    fn runtime_claude_md_without_existing_file_does_not_error() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        let rt = test_runtime();
        generate_runtime_claude_md(&rt, &config).unwrap();
        assert!(config.runtime_claude_md.exists());
    }
}
