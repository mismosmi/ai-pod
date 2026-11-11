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
        eprintln!("{} {}", "Creating mask volume:".blue().bold(), vol);
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
        out.push(format!(
            "{}:{}:Z",
            vol,
            crate::workspace::container_workdir(workspace)
                .join(dir)
                .display()
        ));
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
    if rel.as_os_str().is_empty() {
        // host == home_dir → "/home/ai-pod/" would mount on top of the seeded
        // home volume root, hiding .claude/settings.json, .claude.json, etc.
        anyhow::bail!(
            "mount {} resolves to the home volume root; mount a sub-path instead",
            spec.host
        );
    }
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
        // Re-validate against the current $HOME and the current rule set so
        // a stale or hand-edited `~/.ai-pod/config.json` can't bypass the
        // checks. Failures warn-and-skip so one bad entry doesn't brick
        // every launch.
        let target = match crate::mount_cli::validate_spec(m, home_dir) {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "{} mount {}: {}; skipping",
                    "warning:".yellow().bold(),
                    m.host,
                    e
                );
                continue;
            }
        };
        // symlink_metadata so a dangling symlink (target temporarily missing)
        // still counts as present — `MountSpec`'s doc comment says symlinks
        // are intentionally not resolved.
        if Path::new(&m.host).symlink_metadata().is_err() {
            eprintln!(
                "{} mount source {} does not exist; skipping",
                "warning:".yellow().bold(),
                m.host
            );
            continue;
        }
        // A mount over a skills directory hides the ai-pod skill ai-pod writes
        // into the home volume, leaving the agent without the host-interaction
        // instructions. Benign enough to allow, noisy enough to say so.
        if let Some(skill) = crate::skill::shadowed_path(CONTAINER_HOME, &target) {
            eprintln!(
                "{} mount {} hides the injected ai-pod skill at {}; the agent will not see it",
                "warning:".yellow().bold(),
                target,
                skill
            );
        }
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
        eprintln!("{} {}", "Removed volume:".red().bold(), vol);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
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

/// Persistent Claude configuration resolves credentials from the main
/// container's environment, so each session needs no home-file rewrite.
fn claude_mcp_entry() -> serde_json::Value {
    serde_json::json!({
        "type": "http",
        "url": "${AI_POD_SERVER_URL}/mcp",
        "headers": {
            "X-Api-Key": "${AI_POD_API_KEY}",
            "X-Ai-Pod-Session-Id": "${AI_POD_SESSION_ID}",
        }
    })
}

/// MCP server entry for the host-side Playwright server, as consumed by Claude
/// Code and OpenCode. The url points at the runtime's host gateway; the
/// spoofed `Host` header satisfies Playwright MCP's host allow-list (see
/// [`crate::playwright`]).
fn playwright_mcp_entry(host_gateway: &str, opencode: bool) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "type": if opencode { "remote" } else { "http" },
        "url": crate::playwright::mcp_url(host_gateway),
        "headers": {
            "Host": crate::playwright::host_header(),
        }
    });
    if opencode {
        entry["enabled"] = serde_json::Value::Bool(true);
    }
    entry
}

/// Full inline config injected into OpenCode via the `OPENCODE_CONFIG_CONTENT`
/// env var. Since the env var is set per-launch, we can bake the literal
/// values in directly — no interpolation needed.
fn opencode_config_content(
    server_url: &str,
    api_key: &str,
    session_id: &str,
    playwright: Option<&str>,
) -> String {
    let mut value = serde_json::json!({
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
    });
    if let Some(host_gateway) = playwright {
        value["mcp"]["playwright"] = playwright_mcp_entry(host_gateway, true);
    }
    serde_json::to_string(&value).expect("serialize opencode config content")
}

/// Merge ai-pod's keys into a Codex `config.toml`, preserving everything else.
///
/// Codex reads `~/.codex/config.toml` (TOML); login/auth lives separately in
/// `~/.codex/auth.json` and is never touched here. We use `toml_edit` so a
/// user's persisted, possibly hand-edited config (model, provider, comments,
/// other `[mcp_servers.*]`) survives byte-for-byte — only the five keys we own
/// are installed on creation/version changes. Credentials resolve from the
/// environment at runtime:
///   - `experimental_use_rmcp_client` (enables native streamable-HTTP MCP)
///   - `[mcp_servers.ai-pod]` (url + env_http_headers)
///   - `approval_policy = "never"` and `sandbox_mode = "danger-full-access"`
///     (the container is the sandbox, matching Claude's `bypassPermissions`
///     and OpenCode's YOLO mode; a user's stricter value is intentionally
///     overridden when versioned defaults are installed)
///   - `notify` (points at the in-container notify helper)
fn codex_config_merge(
    existing: &str,
    server_url: &str,
    host_gateway: &str,
    playwright: bool,
) -> String {
    // Missing OR unparsable config -> start from an empty document.
    let mut doc = existing
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_default();

    doc["experimental_use_rmcp_client"] = toml_edit::value(true);
    doc["approval_policy"] = toml_edit::value("never");
    doc["sandbox_mode"] = toml_edit::value("danger-full-access");

    let mut notify = toml_edit::Array::new();
    notify.push("/usr/local/bin/ai-pod-codex-notify");
    doc["notify"] = toml_edit::value(notify);

    update_codex_connection(&mut doc, server_url, host_gateway, playwright);

    doc.to_string()
}

fn update_codex_connection(
    doc: &mut toml_edit::DocumentMut,
    server_url: &str,
    host_gateway: &str,
    playwright: bool,
) {
    let server = &mut doc["mcp_servers"]["ai-pod"];
    server["url"] = toml_edit::value(format!("{}/mcp", server_url));
    // Migrate literal headers written by older ai-pod versions without
    // discarding unrelated headers a user added.
    if let Some(headers) = server
        .get_mut("http_headers")
        .and_then(|v| v.as_table_like_mut())
    {
        headers.remove("X-Api-Key");
        headers.remove("X-Ai-Pod-Session-Id");
    }
    server["env_http_headers"]["X-Api-Key"] = toml_edit::value("AI_POD_API_KEY");
    server["env_http_headers"]["X-Ai-Pod-Session-Id"] = toml_edit::value("AI_POD_SESSION_ID");
    sync_codex_playwright(doc, host_gateway, playwright);
}

fn managed_playwright_url(url: Option<&str>) -> bool {
    url.is_some_and(|url| {
        ["host.containers.internal", "host.docker.internal"]
            .iter()
            .any(|host| url == crate::playwright::mcp_url(host))
    })
}

fn sync_claude_playwright(
    servers: &mut serde_json::Map<String, serde_json::Value>,
    host: &str,
    enabled: bool,
) {
    if enabled {
        servers.insert("playwright".into(), playwright_mcp_entry(host, false));
    } else if managed_playwright_url(servers.get("playwright").and_then(|v| v["url"].as_str())) {
        servers.remove("playwright");
    }
}

/// Compare owned fields semantically so custom formatting and unrelated settings
/// do not cause a rewrite on every launch.
fn sync_codex_playwright(doc: &mut toml_edit::DocumentMut, host: &str, enabled: bool) -> bool {
    let current = doc.get("mcp_servers").and_then(|v| v.get("playwright"));
    if !enabled {
        if managed_playwright_url(current.and_then(|v| v.get("url")).and_then(|v| v.as_str())) {
            doc.get_mut("mcp_servers")
                .and_then(|v| v.as_table_like_mut())
                .unwrap()
                .remove("playwright");
            return true;
        }
        return false;
    }
    let url = crate::playwright::mcp_url(host);
    let header = crate::playwright::host_header();
    if current.and_then(|v| v.get("url")).and_then(|v| v.as_str()) == Some(url.as_str())
        && current
            .and_then(|v| v.get("http_headers"))
            .and_then(|v| v.get("Host"))
            .and_then(|v| v.as_str())
            == Some(header.as_str())
        && current
            .and_then(|v| v.get("enabled"))
            .map(|v| v.as_bool() == Some(true))
            .unwrap_or(true)
    {
        return false;
    }
    let server = &mut doc["mcp_servers"]["playwright"];
    server["url"] = toml_edit::value(url);
    server["http_headers"]["Host"] = toml_edit::value(header);
    server["enabled"] = toml_edit::value(true);
    true
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

const PLAYWRIGHT_GUIDANCE: &str = "If you need browser access and Playwright MCP is unavailable, ask the user to run `ai-pod --playwright`.";

fn with_playwright_guidance(existing: &str) -> String {
    let mut content = existing.to_string();
    if !content.contains(PLAYWRIGHT_GUIDANCE) {
        if !content.is_empty() {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push('\n');
        }
        content.push_str(PLAYWRIGHT_GUIDANCE);
        content.push('\n');
    }
    content
}

/// Label records the creating CLI; the in-volume marker records the last
/// successfully applied injection version (volume labels are immutable).
pub const HOME_VERSION_LABEL: &str = "ai-pod.created-version";
const HOME_VERSION_FILE: &str = ".ai-pod-version";

fn checked_status(status: std::process::ExitStatus, what: &str) -> Result<()> {
    anyhow::ensure!(status.success(), "{what} failed");
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ImageUser {
    home: String,
    uid: u64,
    gid: u64,
}

/// Cache the image's effective identity by immutable image ID. Legacy HOME-only
/// cache entries are re-probed once to obtain ownership for transferred files.
fn image_user(rt: &ContainerRuntime, config: &AppConfig, image: &str) -> Result<ImageUser> {
    if rt.dry_run {
        return Ok(ImageUser {
            home: CONTAINER_HOME.into(),
            uid: 1000,
            gid: 1000,
        });
    }
    let output = rt.command().args(["image", "inspect", image]).output()?;
    checked_status(output.status, "inspect image")?;
    let images: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let id = images[0]["Id"].as_str().context("Image has no ID")?;
    let cache = config
        .config_dir
        .join("image-homes")
        .join(id.replace(':', "-"));
    if let Ok(data) = std::fs::read(&cache) {
        if let Ok(user) = serde_json::from_slice::<ImageUser>(&data) {
            return Ok(user);
        }
    }
    let output = rt
        .command()
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "test -n \"$HOME\" && printf '%s\\n' \"$HOME\" && id -u && id -g",
        ])
        .output()?;
    checked_status(output.status, "resolve image user")?;
    let text = String::from_utf8(output.stdout)?;
    let mut fields = text.lines();
    let home = fields.next().context("Missing image HOME")?.to_string();
    anyhow::ensure!(
        home.starts_with('/') && home != "/",
        "Invalid image HOME: {home}"
    );
    let user = ImageUser {
        home,
        uid: fields.next().context("Missing image UID")?.parse()?,
        gid: fields.next().context("Missing image GID")?.parse()?,
    };
    std::fs::create_dir_all(cache.parent().unwrap())?;
    std::fs::write(cache, serde_json::to_vec(&user)?)?;
    Ok(user)
}

/// A startup script can contain large configuration/login files. Transfer it
/// to the stopped main container, rather than putting it in a command argument
/// (Linux limits each individual argument to roughly 128 KiB).
fn execute_main_container(
    rt: &ContainerRuntime,
    mut command: std::process::Command,
    name: &str,
    config: &AppConfig,
    playwright: bool,
    script_path: &str,
    user: &ImageUser,
) -> Result<std::process::ExitStatus> {
    checked_status(command.status()?, "create main container")?;
    let result = (|| -> Result<std::process::ExitStatus> {
        if let Some(script) = prepare_home_volume(rt, config, name, &user.home, playwright)? {
            let mut archive = tempfile::tempfile()?;
            {
                let mut builder = tar::Builder::new(&mut archive);
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o600);
                header.set_uid(user.uid);
                header.set_gid(user.gid);
                let content = format!("set -eu\nrm -f -- \"$0\"\n{script}");
                header.set_size(content.len() as u64);
                header.set_mtime(0);
                header.set_cksum();
                let basename = Path::new(script_path)
                    .file_name()
                    .context("Invalid init script path")?;
                builder.append_data(&mut header, basename, content.as_bytes())?;
                builder.finish()?;
            }
            use std::io::Seek;
            archive.rewind()?;
            checked_status(
                rt.command()
                    .args(["cp", "--archive", "-", &format!("{name}:{}/", user.home)])
                    .stdin(Stdio::from(archive))
                    .status()?,
                "copy private startup script",
            )?;
        }
        rt.command()
            .args(["start", "--attach", "--interactive", name])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to start container")
    })();
    if result.is_err() {
        let _ = rt.command().args(["rm", "--force", name]).status();
    }
    result
}

fn image_command(rt: &ContainerRuntime, image: &str) -> Result<Vec<String>> {
    if rt.dry_run {
        return Ok(vec!["<image-default-command>".into()]);
    }
    let output = rt.command().args(["image", "inspect", image]).output()?;
    checked_status(output.status, "inspect image command")?;
    let images: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut args = Vec::new();
    for key in ["Entrypoint", "Cmd"] {
        if let Some(values) = images[0]["Config"][key].as_array() {
            args.extend(values.iter().filter_map(|v| v.as_str()).map(str::to_owned));
        }
    }
    anyhow::ensure!(!args.is_empty(), "Image has no entrypoint or command");
    Ok(args)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Append an atomic write performed by the main container as its configured USER.
fn home_write(script: &mut String, path: &str, content: &str) {
    let path = shell_quote(path);
    script.push_str(&format!("mkdir -p \"$(dirname {path})\"\nai_pod_tmp=$(mktemp \"$(dirname {path})/.ai-pod-write.XXXXXX\")\nprintf %s {} > \"$ai_pod_tmp\"\nmv -f \"$ai_pod_tmp\" {path}\n", shell_quote(content)));
}

fn read_home_file(
    rt: &ContainerRuntime,
    container: &str,
    home: &str,
    path: &str,
    tmp: &Path,
) -> Result<Option<String>> {
    let target = tmp.join("read");
    let _ = std::fs::remove_file(&target);
    let output = rt
        .command()
        .args([
            "cp",
            &format!("{container}:{home}/{path}"),
            &target.to_string_lossy(),
        ])
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        anyhow::ensure!(
            error.contains("no such")
                || error.contains("not found")
                || error.contains("does not exist"),
            "Failed reading {path}: {error}"
        );
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(target)?))
}

fn ensure_home_volume(rt: &ContainerRuntime, volume: &str) -> Result<()> {
    if !volume_exists(rt, volume)? {
        checked_status(
            rt.command()
                .args([
                    "volume",
                    "create",
                    "--label",
                    "managed-by=ai-pod",
                    "--label",
                    &format!("{HOME_VERSION_LABEL}={}", env!("CARGO_PKG_VERSION")),
                    volume,
                ])
                .status()?,
            "create home volume",
        )?;
    }
    Ok(())
}

const MAIN_BOOTSTRAP: &str = r#"init=$1
shift
if [ -f "$init" ]; then exec sh "$init" "$@"; fi
exec "$@"
"#;

/// Persistent settings/auth/plugins/skills are applied only on creation or a
/// CLI version change. Session values arrive through environment variables.
/// Read the mounted home through the stopped main container; it performs any
/// required writes as its configured USER when started.
fn prepare_home_volume(
    rt: &ContainerRuntime,
    config: &AppConfig,
    container: &str,
    home: &str,
    playwright: bool,
) -> Result<Option<String>> {
    if rt.dry_run {
        return Ok(None);
    }
    // One-time migration for volumes seeded by the earlier literal-header
    // format at this same development version; subsequent launches skip writes.
    let version = format!("{}:env-playwright-v2", env!("CARGO_PKG_VERSION"));
    let tmp = tempfile::tempdir()?;
    let previous = read_home_file(rt, container, home, HOME_VERSION_FILE, tmp.path())?;
    let inject = previous.as_deref() != Some(version.as_str());
    let mut script =
        "set -eu\nai_pod_start_dir=$PWD\nai_pod_umask=$(umask)\numask 077\ncd \"$HOME\"\n"
            .to_string();
    if inject {
        generate_runtime_settings(config)?;
        home_write(
            &mut script,
            ".claude/settings.json",
            &std::fs::read_to_string(&config.runtime_settings)?,
        );
        for (source, target) in [
            (config.claude_md_path(), ".claude/CLAUDE.md"),
            (config.codex_dir().join("AGENTS.md"), ".codex/AGENTS.md"),
        ] {
            let existing = if source.exists() {
                std::fs::read_to_string(source)?
            } else {
                read_home_file(rt, container, home, target, tmp.path())?.unwrap_or_default()
            };
            home_write(&mut script, target, &with_playwright_guidance(&existing));
        }
        let plugin = config.config_dir.join("opencode-plugin.js");
        if plugin.exists() {
            home_write(
                &mut script,
                ".config/opencode/plugins/ai-pod.js",
                &std::fs::read_to_string(plugin)?,
            );
        }
        // Existing auth and user preferences survive upgrades.
        if previous.is_none() {
            for (source, target) in [
                (config.home_dir.join(".claude.json"), ".claude.json"),
                (config.codex_auth_path(), ".codex/auth.json"),
                (config.codex_config_path(), ".codex/config.toml"),
            ] {
                if source.exists() {
                    script.push_str(&format!("if [ ! -e {} ]; then\n", shell_quote(target)));
                    home_write(&mut script, target, &std::fs::read_to_string(source)?);
                    script.push_str("fi\n");
                }
            }
        }
        if let Some(auth) = read_home_file(rt, container, home, ".codex/auth.json", tmp.path())? {
            // Re-home legacy root-owned copies without replacing the login.
            home_write(&mut script, ".codex/auth.json", &auth);
        }
        let mut git = String::from("[user]\n");
        for key in ["name", "email"] {
            if let Some(value) = read_git_global(&format!("user.{key}")) {
                git.push_str(&format!("\t{key} = {value}\n"));
            }
        }
        if git != "[user]\n" {
            home_write(&mut script, ".gitconfig", &git);
        }
        for path in crate::skill::skill_paths("") {
            home_write(
                &mut script,
                path.trim_start_matches('/'),
                &crate::skill::render(rt),
            );
        }
    }
    let claude = read_home_file(rt, container, home, ".claude.json", tmp.path())?.or_else(|| {
        previous
            .is_none()
            .then(|| std::fs::read_to_string(config.home_dir.join(".claude.json")).ok())
            .flatten()
    });
    let mut value: serde_json::Value = match claude.as_deref() {
        Some(text) => serde_json::from_str(text).context("Invalid Claude configuration")?,
        None => serde_json::json!({}),
    };
    let original_claude = value.clone();
    let obj = value
        .as_object_mut()
        .context("claude.json root must be an object")?;
    if inject || playwright || obj.contains_key("mcpServers") {
        let servers = obj
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}));
        let servers = servers
            .as_object_mut()
            .context("mcpServers must be an object")?;
        if inject {
            servers.insert("ai-pod".into(), claude_mcp_entry());
        }
        sync_claude_playwright(servers, rt.host_gateway(), playwright);
    }
    let claude_changed = value != original_claude;
    if inject || claude_changed {
        home_write(
            &mut script,
            ".claude.json",
            &serde_json::to_string_pretty(&value)?,
        );
    }
    let codex = read_home_file(rt, container, home, ".codex/config.toml", tmp.path())?
        .or_else(|| {
            previous
                .is_none()
                .then(|| std::fs::read_to_string(config.codex_config_path()).ok())
                .flatten()
        })
        .unwrap_or_default();
    let mut codex_doc = codex
        .parse::<toml_edit::DocumentMut>()
        .context("Invalid Codex configuration")?;
    let codex_changed = if inject {
        codex_doc = codex_config_merge(&codex, &rt.server_url(), rt.host_gateway(), playwright)
            .parse::<toml_edit::DocumentMut>()?;
        true
    } else {
        sync_codex_playwright(&mut codex_doc, rt.host_gateway(), playwright)
    };
    if codex_changed {
        home_write(&mut script, ".codex/config.toml", &codex_doc.to_string());
    }
    if !inject && !claude_changed && !codex_changed {
        return Ok(None);
    }
    // Only mark success once all writes have completed.
    if inject {
        home_write(&mut script, HOME_VERSION_FILE, &version);
    }
    script.push_str("cd \"$ai_pod_start_dir\"\numask \"$ai_pod_umask\"\n");
    script.push_str("exec \"$@\"\n");
    Ok(Some(script))
}

/// Explicit skill refresh used by tools/tests. Writes as the image user.
pub fn refresh_skill_in_volume(
    rt: &ContainerRuntime,
    _config: &AppConfig,
    volume: &str,
    _prefix: &str,
    image: &str,
) -> Result<()> {
    use std::io::Write;
    let mut script = "set -eu\numask 077\ncd \"$HOME\"\n".to_string();
    for path in crate::skill::skill_paths("") {
        home_write(
            &mut script,
            path.trim_start_matches('/'),
            &crate::skill::render(rt),
        );
    }
    let mut child = rt
        .command()
        .args([
            "run",
            "--rm",
            "-i",
            "-v",
            &format!("{volume}:{CONTAINER_HOME}:z"),
            "--entrypoint",
            "sh",
            image,
            "-s",
        ])
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("helper stdin missing")?
        .write_all(script.as_bytes())?;
    checked_status(child.wait()?, "write skill")
}

pub fn launch_container(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    rebuild: bool,
    image: &str,
    project_id: &str,
    api_key: &str,
    playwright: bool,
    publish: &[String],
) -> Result<()> {
    let prefix = container_prefix(workspace);
    let volume_name = gen_volume_name(workspace);
    let workspace_mounts = crate::workspace::workspace_mount_args(workspace);
    let container_workdir = crate::workspace::container_workdir(workspace).to_string_lossy();

    rt.warn_if_rootless_userns_mismatch();

    // Rebuild replaces containers; persistent home injections are versioned.
    if rebuild {
        for name in containers_for_prefix(rt, &prefix, false)? {
            checked_status(rt.command().args(["rm", "--force", &name]).status()?, "remove container for rebuild")?;
        }
    }
    let user = image_user(rt, config, image)?;
    let home = &user.home;

    let session_id = new_session_id();
    let container_name = container_name_for(workspace, &session_id);
    eprintln!("{} {}", "Starting container:".blue().bold(), container_name);

    // Record the runtime for this session before the container starts, so the
    // shared server runs service containers on the same runtime.
    crate::config::SessionState {
        workspace: Some(workspace_hash(workspace)),
        runtime: rt.kind,
        network: None,
    }
    .save(config, &session_id)?;

    ensure_home_volume(rt, &volume_name)?;

    let host_gw_env = format!("HOST_GATEWAY={}", rt.host_gateway());
    let server_url_env = format!("AI_POD_SERVER_URL={}", rt.server_url());
    let opencode_config_env = format!(
        "OPENCODE_CONFIG_CONTENT={}",
        opencode_config_content(
            &rt.server_url(),
            api_key,
            &session_id,
            playwright.then(|| rt.host_gateway()),
        )
    );

    let project_state = load_project_state(config, workspace);
    let mask_args = mask_mount_args(rt, workspace, image, &project_state.masked_directories)?;
    let global = GlobalConfig::load(config);
    let user_mount_args = build_mount_args(&config.home_dir, &global.mounts)?;

    // Create the per-workspace service network up front and attach the main
    // container to it at launch. Lazy attach via `podman network connect` after
    // the fact does not work for rootless podman (slirp4netns containers cannot
    // be added to additional networks), so any later `start_service` call would
    // fail. Doing it here makes service-container requests work on every
    // runtime without restarting the session.
    let net = session_network(
        rt,
        config,
        workspace,
        &session_id,
        project_state.egress.as_ref(),
        playwright,
        publish,
    )?;

    let script_path = format!("{home}/.ai-pod-init-{session_id}.sh");
    let mut run_cmd = rt.command();
    run_cmd.args(["create", "--rm", "-it"]);
    run_cmd.args([
        "--name",
        &container_name,
        "--label",
        "managed-by=ai-pod",
        "--network",
        &net.network,
        "-v",
        &format!("{}:{}:z", volume_name, home),
        "--workdir",
        &container_workdir,
    ]);
    run_cmd.args(&workspace_mounts);
    for arg in &user_mount_args {
        run_cmd.arg(arg);
    }
    for arg in &mask_args {
        run_cmd.arg(arg);
    }
    for env in &net.env {
        run_cmd.args(["-e", env]);
    }
    run_cmd.args([
        &net.add_host_arg,
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
    for port in publish {
        run_cmd.args(["--publish", port]);
    }
    run_cmd.args(["--entrypoint", "sh", image, "-c", MAIN_BOOTSTRAP, "ai-pod-init", &script_path]);
    run_cmd.args(image_command(rt, image)?);
    let run_status = execute_main_container(rt, run_cmd, &container_name, config, playwright, &script_path, &user)?;

    // Main container has exited (cleanly or otherwise); tear down anything the
    // agent started for this session. Best-effort: this is also covered by the
    // server's periodic orphan sweep if the CLI was killed.
    crate::service::cleanup_services_for_session(rt, &session_id);
    crate::egress::cleanup_for_session(rt, &session_id);
    let _ = std::fs::remove_file(config.session_state_file(&session_id));
    checked_status(run_status, "run container")
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
    interactive: bool,
    playwright: bool,
    publish: &[String],
) -> Result<()> {
    let session_id = new_session_id();
    let container_name = container_name_for(workspace, &session_id);
    let volume_name = gen_volume_name(workspace);
    let workspace_mounts = crate::workspace::workspace_mount_args(workspace);
    let container_workdir = crate::workspace::container_workdir(workspace).to_string_lossy();

    rt.warn_if_rootless_userns_mismatch();

    // Record the runtime for this session before the container starts, so the
    // shared server runs service containers on the same runtime.
    crate::config::SessionState {
        workspace: Some(workspace_hash(workspace)),
        runtime: rt.kind,
        network: None,
    }
    .save(config, &session_id)?;

    let user = image_user(rt, config, image)?;
    let home = &user.home;
    ensure_home_volume(rt, &volume_name)?;

    eprintln!(
        "{} {} {}",
        "Running in container:".blue().bold(),
        container_name,
        command
    );

    let project_state = load_project_state(config, workspace);
    let mask_args = mask_mount_args(rt, workspace, image, &project_state.masked_directories)?;
    let global = GlobalConfig::load(config);
    let user_mount_args = build_mount_args(&config.home_dir, &global.mounts)?;

    // See the matching comment in launch_container — main goes on the
    // per-workspace service network at launch so service containers can be
    // attached later on rootless podman.
    let net = session_network(
        rt,
        config,
        workspace,
        &session_id,
        project_state.egress.as_ref(),
        playwright,
        publish,
    )?;

    // Without a tty on stdin (e.g. an IDE driving ai-pod over stdio for
    // ACP), `-t` would allocate a pseudo-TTY that mangles the JSON-RPC
    // byte stream the agent emits. Keep `-i` so stdin stays attached.
    let stdio_flag = if interactive { "-it" } else { "-i" };
    let script_path = format!("{home}/.ai-pod-init-{session_id}.sh");
    let mut run_args: Vec<String> = vec![
        "create".into(),
        "--rm".into(),
        stdio_flag.into(),
    ];
    run_args.extend_from_slice(&[
        "--name".into(),
        container_name.clone(),
        "--label".into(),
        "managed-by=ai-pod".into(),
        "--network".into(),
        net.network,
        "-v".into(),
        format!("{}:{}:z", volume_name, home),
        "--workdir".into(),
        container_workdir.into_owned(),
    ]);
    for port in publish {
        run_args.extend(["--publish".to_string(), port.clone()]);
    }
    run_args.extend(workspace_mounts);
    run_args.extend(user_mount_args);
    run_args.extend(mask_args);
    for env in net.env {
        run_args.extend(["-e".into(), env]);
    }
    run_args.extend_from_slice(&[
        net.add_host_arg,
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
            opencode_config_content(
            &rt.server_url(),
            api_key,
            &session_id,
            playwright.then(|| rt.host_gateway()),
        )
        ),
    ]);
    run_args.extend([
        "--entrypoint".into(), "sh".into(), image.into(), "-c".into(), MAIN_BOOTSTRAP.into(),
        "ai-pod-init".into(), script_path.clone(), command.into(),
    ]);
    run_args.extend_from_slice(args);

    let mut run_cmd = rt.command();
    run_cmd.args(&run_args);
    let status = execute_main_container(rt, run_cmd, &container_name, config, playwright, &script_path, &user)?;

    crate::service::cleanup_services_for_session(rt, &session_id);
    crate::egress::cleanup_for_session(rt, &session_id);
    let _ = std::fs::remove_file(config.session_state_file(&session_id));

    if !status.success() {
        anyhow::bail!("Command exited with non-zero status");
    }

    Ok(())
}

/// The per-workspace service network by default; with egress filtering
/// configured, the internal filtered network behind a per-session gateway.
fn session_network(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    session_id: &str,
    egress: Option<&crate::egress::EgressConfig>,
    playwright: bool,
    publish: &[String],
) -> Result<crate::egress::EgressSession> {
    let Some(egress) = egress else {
        return Ok(crate::egress::EgressSession {
            network: crate::service::ensure_service_network(rt, workspace)?,
            add_host_arg: rt.add_host_arg(),
            env: Vec::new(),
        });
    };

    eprintln!(
        "{} network access filtered {}",
        "Egress:".blue().bold(),
        egress.describe()
    );
    if !publish.is_empty() {
        eprintln!(
            "{} published ports may not be reachable: the container is on an internal network.",
            "Warning:".yellow().bold()
        );
    }
    let session = crate::egress::start(rt, workspace, session_id, egress, playwright)?;
    // Let the server put this session's service containers on the same
    // internal network.
    crate::config::SessionState {
        workspace: Some(workspace_hash(workspace)),
        runtime: rt.kind,
        network: Some(session.network.clone()),
    }
    .save(config, session_id)?;
    Ok(session)
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn claude_mcp_entry_uses_environment() {
        let entry = claude_mcp_entry();
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "${AI_POD_SERVER_URL}/mcp");
        assert_eq!(entry["headers"]["X-Api-Key"], "${AI_POD_API_KEY}");
        assert_eq!(
            entry["headers"]["X-Ai-Pod-Session-Id"],
            "${AI_POD_SESSION_ID}"
        );
    }

    #[test]
    fn opencode_config_content_bakes_literal_values() {
        let s = opencode_config_content(
            "http://host.containers.internal:7822",
            "k1",
            "s2",
            None,
        );
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
    fn opencode_config_includes_playwright_when_enabled() {
        let s = opencode_config_content(
            "http://host.containers.internal:7822",
            "k1",
            "s2",
            Some("host.containers.internal"),
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mcp"]["playwright"]["type"], "remote");
        assert_eq!(v["mcp"]["playwright"]["enabled"], true);
        assert_eq!(
            v["mcp"]["playwright"]["url"],
            "http://host.containers.internal:8931/mcp"
        );
        assert_eq!(v["mcp"]["playwright"]["headers"]["Host"], "localhost:8931");
    }

    #[test]
    fn claude_playwright_entry_is_http_config() {
        let entry = playwright_mcp_entry("host.containers.internal", false);
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://host.containers.internal:8931/mcp");
        assert_eq!(entry["headers"]["Host"], "localhost:8931");
        assert!(entry["enabled"].is_null());
    }

    #[test]
    fn codex_config_enables_playwright_when_requested() {
        let existing = "[mcp_servers.playwright]\nurl = \"http://old-host/mcp\"\nenabled = false\n";
        let out = codex_config_merge(
            existing,
            "http://host.containers.internal:7822",
            "host.containers.internal", true);
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        let server = &doc["mcp_servers"]["playwright"];
        assert_eq!(
            server["url"].as_str(),
            Some("http://host.containers.internal:8931/mcp")
        );
        assert_eq!(
            server["http_headers"]["Host"].as_str(),
            Some("localhost:8931")
        );
        assert_eq!(server["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn playwright_config_is_omitted_and_removed_when_disabled() {
        let opencode: serde_json::Value = serde_json::from_str(&opencode_config_content("http://host", "key", "session", None)).unwrap();
        assert!(opencode["mcp"]["playwright"].is_null());
        let mut servers = serde_json::Map::new();
        servers.insert("other".into(), serde_json::json!({"url":"http://other/mcp"}));
        sync_claude_playwright(&mut servers, "host.containers.internal", true);
        sync_claude_playwright(&mut servers, "host.containers.internal", false);
        assert!(!servers.contains_key("playwright"));
        assert!(servers.contains_key("other"));
        let mut doc = codex_config_merge("model = \"mine\"\n", "http://host", "host.containers.internal", true).parse::<toml_edit::DocumentMut>().unwrap();
        assert!(sync_codex_playwright(&mut doc, "host.containers.internal", false));
        assert!(doc["mcp_servers"].get("playwright").is_none());
        assert_eq!(doc["model"].as_str(), Some("mine"));
        assert!(!sync_codex_playwright(&mut doc, "host.containers.internal", false));
    }

    #[test]
    fn matching_playwright_config_keeps_custom_formatting_and_headers() {
        let input = "# keep this comment\n[mcp_servers.playwright]\nurl='http://host.containers.internal:8931/mcp'\nenabled=true\n[mcp_servers.playwright.http_headers]\nHost='localhost:8931'\nOther='keep'\n";
        let mut doc = input.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(!sync_codex_playwright(&mut doc, "host.containers.internal", true));
        assert_eq!(doc.to_string(), input);
        let implicit = input.replace("enabled=true\n", "");
        let mut doc = implicit.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(!sync_codex_playwright(&mut doc, "host.containers.internal", true));
        assert_eq!(doc.to_string(), implicit);
        let mut servers = serde_json::Map::new();
        servers.insert("playwright".into(), serde_json::json!({"url":"http://personal/mcp"}));
        sync_claude_playwright(&mut servers, "host.containers.internal", false);
        assert!(servers.contains_key("playwright"));
    }

    #[test]
    fn playwright_guidance_preserves_instructions_without_duplication() {
        let existing = "# Personal instructions\nKeep my preferred style.\n";
        let updated = with_playwright_guidance(existing);
        assert!(updated.starts_with(existing));
        assert!(updated.contains(PLAYWRIGHT_GUIDANCE));
        assert_eq!(with_playwright_guidance(&updated), updated);
        assert_eq!(
            with_playwright_guidance(""),
            format!("{PLAYWRIGHT_GUIDANCE}\n")
        );
    }

    #[test]
    fn codex_config_merge_into_empty() {
        let out = codex_config_merge(
            "",
            "http://host.containers.internal:7822",
            "host.containers.internal", true);
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["experimental_use_rmcp_client"].as_bool(), Some(true));
        assert_eq!(doc["approval_policy"].as_str(), Some("never"));
        assert_eq!(doc["sandbox_mode"].as_str(), Some("danger-full-access"));
        assert_eq!(
            doc["notify"].as_array().unwrap().get(0).unwrap().as_str(),
            Some("/usr/local/bin/ai-pod-codex-notify")
        );
        let server = &doc["mcp_servers"]["ai-pod"];
        assert_eq!(
            server["url"].as_str(),
            Some("http://host.containers.internal:7822/mcp")
        );
        assert_eq!(
            server["env_http_headers"]["X-Api-Key"].as_str(),
            Some("AI_POD_API_KEY")
        );
        assert_eq!(
            server["env_http_headers"]["X-Ai-Pod-Session-Id"].as_str(),
            Some("AI_POD_SESSION_ID")
        );
    }

    #[test]
    fn codex_config_merge_preserves_existing_keys() {
        let existing = "model = \"gpt-5\"\n# my comment\n";
        let out = codex_config_merge(
            existing,
            "http://host.containers.internal:7822",
            "host.containers.internal", true);
        // The user's key and comment survive (toml_edit, not a typed rewrite).
        assert!(
            out.contains("# my comment"),
            "comment should survive: {out}"
        );
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        // ...and ai-pod's keys are still merged in.
        assert_eq!(
            doc["mcp_servers"]["ai-pod"]["env_http_headers"]["X-Api-Key"].as_str(),
            Some("AI_POD_API_KEY")
        );
    }

    #[test]
    fn codex_config_merge_url_uses_runtime_gateway() {
        // On Docker the gateway is host.docker.internal — the url must reflect
        // whatever server_url is passed, not a hardcoded host.
        let out = codex_config_merge(
            "",
            "http://host.docker.internal:7822",
            "host.docker.internal", true);
        let doc = out.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            doc["mcp_servers"]["ai-pod"]["url"].as_str(),
            Some("http://host.docker.internal:7822/mcp")
        );
    }

    #[test]
    fn codex_config_migrates_literal_credentials() {
        let existing = "[mcp_servers.ai-pod.http_headers]\nX-Api-Key = \"old-key\"\nX-Ai-Pod-Session-Id = \"old-session\"\nOther = \"preserved\"\n";
        let output = codex_config_merge(existing, "http://h:7822", "host.containers.internal", true);
        let doc = output.parse::<toml_edit::DocumentMut>().unwrap();
        let server = &doc["mcp_servers"]["ai-pod"];
        assert_eq!(server["http_headers"]["Other"].as_str(), Some("preserved"));
        assert_eq!(
            server["env_http_headers"]["X-Api-Key"].as_str(),
            Some("AI_POD_API_KEY")
        );
        assert!(!output.contains("old-key"));
        assert!(!output.contains("old-session"));
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

    #[test]
    fn resolve_container_target_rejects_home_root() {
        // mount add ~ → empty rel → would produce "/home/ai-pod/" and
        // shadow the seeded volume.
        let spec = MountSpec {
            host: "/home/user".into(),
            container: None,
            writable: false,
        };
        let err = resolve_container_target(&spec, Path::new("/home/user")).unwrap_err();
        assert!(err.to_string().contains("home volume root"), "got: {err}");
    }

    #[test]
    fn build_mount_args_warn_skips_outside_home_when_no_container() {
        // Regression for the "stored mount bricks every launch" issue:
        // a hand-synced config.json where `container == None` and `host`
        // falls outside the *current* $HOME must not error — it should
        // warn-skip like a missing host.
        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("..").join("outside");
        let mounts = vec![MountSpec {
            host: outside.display().to_string(),
            container: None,
            writable: false,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert!(args.is_empty(), "invalid stored mount should be skipped");
    }

    #[test]
    fn build_mount_args_warn_skips_invalid_stored_spec() {
        // A spec that would have been rejected at `mount add` (here: `/` as
        // host) but got past validation via hand-edited config.json must be
        // dropped at launch.
        let dir = TempDir::new().unwrap();
        let mounts = vec![MountSpec {
            host: "/".into(),
            container: Some("/home/ai-pod/exploit".into()),
            writable: false,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert!(args.is_empty(), "stored invalid host should be warn-skipped");
    }

    #[test]
    fn build_mount_args_keeps_dangling_symlinks() {
        // MountSpec doc explicitly says symlinks are not resolved, so a
        // symlink whose target is temporarily missing should still mount.
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("link-to-nothing");
        symlink(dir.path().join("does-not-exist"), &link).unwrap();
        let host_str = link.to_string_lossy().to_string();
        let mounts = vec![MountSpec {
            host: host_str.clone(),
            container: Some("/home/ai-pod/.claude/skills".into()),
            writable: false,
        }];
        let args = build_mount_args(dir.path(), &mounts).unwrap();
        assert_eq!(args.len(), 2, "dangling symlink should still mount");
        assert!(args[1].starts_with(&host_str));
    }
}
