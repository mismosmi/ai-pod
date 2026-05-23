use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer;
use std::path::Path;
use std::process::Stdio;

use crate::config::{AppConfig, GlobalConfig, MountSpec};
use crate::runtime::ContainerRuntime;
use crate::server::lifecycle::ProjectState;
use crate::workspace::{
    container_name_for, container_prefix, mask_volume_name, new_session_id,
    volume_name as gen_volume_name, workspace_hash,
};

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

/// Create a fresh mask volume and chown its root to the container's `ai-pod` user
/// so the unprivileged in-container user can write under /app/<dir>.
fn seed_mask_volume(rt: &ContainerRuntime, image: &str, vol: &str, dir: &str) -> Result<()> {
    let mount_path = format!("/app/{}", dir);
    let status = rt
        .command()
        .args([
            "run",
            "--rm",
            "--user",
            "0",
            "-v",
            &format!("{}:{}:Z", vol, mount_path),
            "--entrypoint",
            "chown",
            image,
            "ai-pod:ai-pod",
            &mount_path,
        ])
        .status()
        .context("Failed to seed mask volume")?;
    if !status.success() {
        anyhow::bail!("Failed to chown mask volume {}", vol);
    }
    Ok(())
}

/// Ensure a per-mask volume exists (creating + seeding ownership on first use)
/// and return its name. Idempotent.
fn ensure_mask_volume(
    rt: &ContainerRuntime,
    workspace: &Path,
    image: &str,
    dir: &str,
) -> Result<String> {
    let vol = mask_volume_name(workspace, dir);
    if !volume_exists(rt, &vol)? {
        println!("{} {}", "Creating mask volume:".blue().bold(), vol);
        let status = rt
            .command()
            .args(["volume", "create", &vol])
            .status()
            .context("Failed to create mask volume")?;
        if !status.success() {
            anyhow::bail!("Failed to create mask volume {}", vol);
        }
        seed_mask_volume(rt, image, &vol, dir)?;
    }
    Ok(vol)
}

/// Build the additional `-v` arg pairs that shadow-mount each masked top-level
/// directory under /app with its own per-workspace named volume. Returned as a
/// flat list of strings (`-v`, `vol:/app/dir:Z`, ...) ready to splice into the
/// container run command, after the workspace bind so the shadowing is unambiguous.
fn mask_mount_args(
    rt: &ContainerRuntime,
    workspace: &Path,
    image: &str,
    masks: &[String],
) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(masks.len() * 2);
    for dir in masks {
        let vol = ensure_mask_volume(rt, workspace, image, dir)?;
        out.push("-v".to_string());
        out.push(format!("{}:/app/{}:Z", vol, dir));
    }
    Ok(out)
}

/// Resolve the in-container target path for a user-defined mount.
///
/// - If `spec.container` is set, returns it verbatim (already validated at `mount add`).
/// - Otherwise, requires `spec.host` to be under `home_dir` and mirrors the
///   path under `CONTAINER_HOME`. E.g. `~/.claude/skills` → `/home/ai-pod/.claude/skills`.
pub(crate) fn resolve_container_target(spec: &MountSpec, home_dir: &Path) -> Result<String> {
    if let Some(c) = &spec.container {
        return Ok(c.clone());
    }
    let host = Path::new(&spec.host);
    let rel = host.strip_prefix(home_dir).map_err(|_| {
        anyhow::anyhow!(
            "mount {} is outside $HOME; specify an explicit container path with host:container",
            spec.host
        )
    })?;
    Ok(format!("{}/{}", CONTAINER_HOME, rel.display()))
}

/// Build `-v` arg pairs for global host bind-mounts. Returned as a flat list
/// (`-v`, `host:target:opts`, ...) ready to splice into the container run
/// command after the workspace bind. Missing host paths are skipped with a
/// stderr warning so a temporarily-absent host directory doesn't brick every
/// project.
///
/// Uses the `:z` SELinux label (shared) to match the home-volume mount, so the
/// host user retains access to e.g. `~/.claude/skills` after the container
/// touches it. Read-only mounts get `:z,ro`.
pub(crate) fn build_mount_args(home_dir: &Path, mounts: &[MountSpec]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(mounts.len() * 2);
    for m in mounts {
        let host = Path::new(&m.host);
        if !host.exists() {
            eprintln!(
                "{} mount source {} does not exist; skipping",
                "warning:".yellow().bold(),
                m.host
            );
            continue;
        }
        let target = resolve_container_target(m, home_dir)?;
        let opts = if m.writable { "z" } else { "z,ro" };
        out.push("-v".to_string());
        out.push(format!("{}:{}:{}", m.host, target, opts));
    }
    Ok(out)
}

/// Best-effort removal of a single mask volume. Prints a message on success and
/// a warning if the volume is in use (e.g. another container still mounts it).
pub fn remove_mask_volume(rt: &ContainerRuntime, workspace: &Path, dir: &str) -> Result<()> {
    let vol = mask_volume_name(workspace, dir);
    if !volume_exists(rt, &vol)? {
        return Ok(());
    }
    let output = rt
        .command()
        .args(["volume", "rm", &vol])
        .output()
        .context("Failed to remove mask volume")?;
    if output.status.success() {
        println!("{} {}", "Removed volume:".red().bold(), vol);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!(
            "{} could not remove {} ({})",
            "Warning:".yellow().bold(),
            vol,
            stderr.trim()
        );
    }
    Ok(())
}

fn load_project_state(config: &AppConfig, workspace: &Path) -> ProjectState {
    let hash = workspace_hash(workspace);
    ProjectState::load(&config.project_state_file(&hash))
}

fn generate_runtime_settings(config: &AppConfig) -> Result<()> {
    let mut settings: serde_json::Value = if config.claude_settings_path().exists() {
        let raw = std::fs::read_to_string(config.claude_settings_path())
            .context("Failed to read settings.json")?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let notify_curl = |msg: &str| {
        format!(
            "curl -fsS -X POST -H \"X-Api-Key: $AI_POD_API_KEY\" -H 'Content-Type: application/json' -d '{{\"project_id\":\"'\"$AI_POD_PROJECT_ID\"'\",\"message\":\"{}\"}}' \"$AI_POD_SERVER_URL/notify_user\" >/dev/null || true",
            msg
        )
    };

    let stop_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": notify_curl("Task completed"),
        }]
    }]);

    let permission_hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": notify_curl("Claude needs your approval"),
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

/// MCP server entry consumed by Claude Code. Lives under `mcpServers.ai-pod`
/// inside `~/.claude.json`. We bake the api key and session id in as literals
/// (rather than `${VAR}` placeholders) because `claude doctor` eagerly
/// validates referenced env vars and warns if any context can't see them.
fn claude_mcp_entry(server_url: &str, api_key: &str, session_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "http",
        "url": format!("{}/mcp", server_url),
        "headers": {
            "X-Api-Key": api_key,
            "X-Ai-Pod-Session-Id": session_id,
        }
    })
}

/// Full inline config injected into OpenCode via the `OPENCODE_CONFIG_CONTENT`
/// env var. Since the env var is set per-launch, we can bake the literal
/// values in directly — no interpolation needed.
fn opencode_config_content(server_url: &str, api_key: &str, session_id: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "mcp": {
            "ai-pod": {
                "type": "remote",
                "url": format!("{}/mcp", server_url),
                "enabled": true,
                "headers": {
                    "X-Api-Key": api_key,
                    "X-Ai-Pod-Session-Id": session_id,
                }
            }
        }
    }))
    .expect("serialize opencode config content")
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
///
/// Note: the `mcpServers.ai-pod` entry is *not* written here — `refresh_claude_mcp_in_volume`
/// handles that on every launch with the current session id baked in.
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
        let host_claude_json = config.home_dir.join(".claude.json");
        if host_claude_json.exists() {
            let _ = rt
                .command()
                .args([
                    "cp",
                    &host_claude_json.to_string_lossy(),
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
            &format!("{}/.config", CONTAINER_HOME),
            &format!("{}/.config/opencode/plugins", CONTAINER_HOME),
        ])
        .status();

    generate_runtime_settings(config)?;

    let _ = rt
        .command()
        .args([
            "cp",
            &config.runtime_settings.to_string_lossy(),
            &format!("{}:{}/.claude/settings.json", init_container, CONTAINER_HOME),
        ])
        .status();

    // Copy the host's personal CLAUDE.md into the container (no ai-pod preamble)
    let host_claude_md = config.claude_md_path();
    if host_claude_md.exists() {
        let _ = rt
            .command()
            .args([
                "cp",
                &host_claude_md.to_string_lossy(),
                &format!("{}:{}/.claude/CLAUDE.md", init_container, CONTAINER_HOME),
            ])
            .status();
    }

    let opencode_plugin = config.config_dir.join("opencode-plugin.js");
    if opencode_plugin.exists() {
        let _ = rt
            .command()
            .args([
                "cp",
                &opencode_plugin.to_string_lossy(),
                &format!(
                    "{}:{}/.config/opencode/plugins/ai-pod.js",
                    init_container, CONTAINER_HOME
                ),
            ])
            .status();
    }

    write_gitconfig_to_volume(rt, config, &init_container)?;

    let _ = rt.command().args(["rm", &init_container]).status();

    Ok(())
}

/// Update the `mcpServers.ai-pod` entry in the volume's `~/.claude.json`
/// with literal api_key + session_id values. Runs on every launch so the
/// in-volume config matches the env the agent will see.
fn refresh_claude_mcp_in_volume(
    rt: &ContainerRuntime,
    config: &AppConfig,
    volume_name: &str,
    container_name: &str,
    image: &str,
    server_url: &str,
    api_key: &str,
    session_id: &str,
) -> Result<()> {
    let init_container = format!("{}-mcp", container_name);
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
        .context("Failed to create mcp-refresh container")?;
    if !status.success() {
        anyhow::bail!("Failed to create mcp-refresh container");
    }

    // Pull the existing .claude.json out of the volume (may not exist yet).
    let tmp_in = config.config_dir.join("claude-in.json");
    let _ = std::fs::remove_file(&tmp_in);
    let _ = rt
        .command()
        .args([
            "cp",
            &format!("{}:{}/.claude.json", init_container, CONTAINER_HOME),
            tmp_in.to_str().unwrap(),
        ])
        .status();

    let mut value: serde_json::Value = std::fs::read_to_string(&tmp_in)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let obj = value
        .as_object_mut()
        .expect("claude.json root must be an object");
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    servers
        .as_object_mut()
        .expect("mcpServers must be an object")
        .insert(
            "ai-pod".to_string(),
            claude_mcp_entry(server_url, api_key, session_id),
        );

    let tmp_out = config.config_dir.join("claude-out.json");
    std::fs::write(&tmp_out, serde_json::to_string_pretty(&value)?)?;
    let _ = rt
        .command()
        .args([
            "cp",
            tmp_out.to_str().unwrap(),
            &format!("{}:{}/.claude.json", init_container, CONTAINER_HOME),
        ])
        .status();

    let _ = rt.command().args(["rm", &init_container]).status();
    let _ = std::fs::remove_file(&tmp_in);
    let _ = std::fs::remove_file(&tmp_out);
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

    let session_id = new_session_id();
    let container_name = container_name_for(workspace, &session_id);
    println!("{} {}", "Starting container:".blue().bold(), container_name);

    refresh_claude_mcp_in_volume(
        rt,
        config,
        &volume_name,
        &prefix,
        image,
        &rt.server_url(),
        api_key,
        &session_id,
    )?;

    let add_host = rt.add_host_arg();
    let host_gw_env = format!("HOST_GATEWAY={}", rt.host_gateway());
    let server_url_env = format!("AI_POD_SERVER_URL={}", rt.server_url());
    let opencode_config_env = format!(
        "OPENCODE_CONFIG_CONTENT={}",
        opencode_config_content(&rt.server_url(), api_key, &session_id)
    );

    let project_state = load_project_state(config, workspace);
    let mask_args = mask_mount_args(rt, workspace, image, &project_state.masked_directories)?;
    let global = GlobalConfig::load(config);
    let user_mount_args = build_mount_args(&config.home_dir, &global.mounts)?;

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
    ]);
    for arg in &user_mount_args {
        run_cmd.arg(arg);
    }
    for arg in &mask_args {
        run_cmd.arg(arg);
    }
    run_cmd.args([
        &add_host,
        "-e",
        &host_gw_env,
        "-e",
        &format!("AI_POD_PROJECT_ID={}", project_id),
        "-e",
        &format!("AI_POD_API_KEY={}", api_key),
        "-e",
        &format!("AI_POD_SESSION_ID={}", session_id),
        "-e",
        &server_url_env,
        "-e",
        &opencode_config_env,
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
    let session_id = new_session_id();
    let container_name = container_name_for(workspace, &session_id);
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

    refresh_claude_mcp_in_volume(
        rt,
        config,
        &volume_name,
        &container_name,
        image,
        &rt.server_url(),
        api_key,
        &session_id,
    )?;

    println!(
        "{} {} {}",
        "Running in container:".blue().bold(),
        container_name,
        command
    );

    let project_state = load_project_state(config, workspace);
    let mask_args = mask_mount_args(rt, workspace, image, &project_state.masked_directories)?;
    let global = GlobalConfig::load(config);
    let user_mount_args = build_mount_args(&config.home_dir, &global.mounts)?;

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
    ]);
    run_args.extend(user_mount_args);
    run_args.extend(mask_args);
    run_args.extend_from_slice(&[
        rt.add_host_arg(),
        "-e".into(),
        format!("HOST_GATEWAY={}", rt.host_gateway()),
        "-e".into(),
        format!("AI_POD_PROJECT_ID={}", project_id),
        "-e".into(),
        format!("AI_POD_API_KEY={}", api_key),
        "-e".into(),
        format!("AI_POD_SESSION_ID={}", session_id),
        "-e".into(),
        format!("AI_POD_SERVER_URL={}", rt.server_url()),
        "-e".into(),
        format!(
            "OPENCODE_CONFIG_CONTENT={}",
            opencode_config_content(&rt.server_url(), api_key, &session_id)
        ),
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

pub fn clean_container(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
) -> Result<()> {
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

    // Remove per-mask volumes recorded in this workspace's state. Config (the
    // list of masked dirs) is preserved so the volumes are re-created on next
    // launch — `clean` resets runtime state, not user config.
    let state = load_project_state(config, workspace);
    for dir in &state.masked_directories {
        let _ = remove_mask_volume(rt, workspace, dir);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{container_prefix, new_container_name, volume_name};
    use std::path::Path;
    use tempfile::TempDir;

    fn make_test_config(dir: &TempDir) -> AppConfig {
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
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
        assert!(cmd.contains("notify_user"));
        assert!(cmd.contains("$AI_POD_SERVER_URL"));
    }

    #[test]
    fn runtime_settings_stop_hook_uses_curl() {
        let dir = TempDir::new().unwrap();
        let config = make_test_config(&dir);
        generate_runtime_settings(&config).unwrap();

        let content = std::fs::read_to_string(&config.runtime_settings).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let cmd = json["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.starts_with("curl"));
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
    fn claude_mcp_entry_bakes_literal_values() {
        let entry = claude_mcp_entry("http://host.containers.internal:7822", "k1", "s2");
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://host.containers.internal:7822/mcp");
        assert_eq!(entry["headers"]["X-Api-Key"], "k1");
        assert_eq!(entry["headers"]["X-Ai-Pod-Session-Id"], "s2");
    }

    #[test]
    fn opencode_config_content_bakes_literal_values() {
        let s = opencode_config_content("http://host.containers.internal:7822", "k1", "s2");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mcp"]["ai-pod"]["type"], "remote");
        assert_eq!(
            v["mcp"]["ai-pod"]["url"],
            "http://host.containers.internal:7822/mcp"
        );
        assert_eq!(v["mcp"]["ai-pod"]["enabled"], true);
        assert_eq!(v["mcp"]["ai-pod"]["headers"]["X-Api-Key"], "k1");
        assert_eq!(v["mcp"]["ai-pod"]["headers"]["X-Ai-Pod-Session-Id"], "s2");
    }

    #[test]
    fn resolve_container_target_uses_explicit_path() {
        let spec = MountSpec {
            host: "/whatever".into(),
            container: Some("/run/secrets/key".into()),
            writable: false,
        };
        let t = resolve_container_target(&spec, Path::new("/home/user")).unwrap();
        assert_eq!(t, "/run/secrets/key");
    }

    #[test]
    fn resolve_container_target_mirrors_home_paths() {
        let spec = MountSpec {
            host: "/home/user/.claude/skills".into(),
            container: None,
            writable: false,
        };
        let t = resolve_container_target(&spec, Path::new("/home/user")).unwrap();
        assert_eq!(t, "/home/ai-pod/.claude/skills");
    }

    #[test]
    fn resolve_container_target_errors_for_paths_outside_home() {
        let spec = MountSpec {
            host: "/etc/foo".into(),
            container: None,
            writable: false,
        };
        let err = resolve_container_target(&spec, Path::new("/home/user")).unwrap_err();
        assert!(err.to_string().contains("outside $HOME"), "got: {err}");
    }

    #[test]
    fn build_mount_args_emits_readonly_by_default() {
        let dir = TempDir::new().unwrap();
        let host_path = dir.path().join("skills");
        std::fs::create_dir(&host_path).unwrap();
        let host_str = host_path.to_string_lossy().to_string();
        let mounts = vec![MountSpec {
            host: host_str.clone(),
            container: Some("/home/ai-pod/.claude/skills".into()),
            writable: false,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert_eq!(
            args,
            vec![
                "-v".to_string(),
                format!("{}:/home/ai-pod/.claude/skills:z,ro", host_str),
            ]
        );
    }

    #[test]
    fn build_mount_args_emits_writable_when_flagged() {
        let dir = TempDir::new().unwrap();
        let host_path = dir.path().join("skills");
        std::fs::create_dir(&host_path).unwrap();
        let host_str = host_path.to_string_lossy().to_string();
        let mounts = vec![MountSpec {
            host: host_str.clone(),
            container: Some("/home/ai-pod/.claude/skills".into()),
            writable: true,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert_eq!(
            args,
            vec![
                "-v".to_string(),
                format!("{}:/home/ai-pod/.claude/skills:z", host_str),
            ]
        );
    }

    #[test]
    fn build_mount_args_resolves_home_relative_target() {
        let dir = TempDir::new().unwrap();
        let host_path = dir.path().join(".claude").join("skills");
        std::fs::create_dir_all(&host_path).unwrap();
        let host_str = host_path.to_string_lossy().to_string();
        let mounts = vec![MountSpec {
            host: host_str.clone(),
            container: None,
            writable: false,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert_eq!(
            args,
            vec![
                "-v".to_string(),
                format!("{}:/home/ai-pod/.claude/skills:z,ro", host_str),
            ]
        );
    }

    #[test]
    fn build_mount_args_skips_missing_host_paths() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        let mounts = vec![MountSpec {
            host: missing.to_string_lossy().to_string(),
            container: Some("/home/ai-pod/x".into()),
            writable: false,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert!(args.is_empty(), "missing host path should be skipped");
    }
}
