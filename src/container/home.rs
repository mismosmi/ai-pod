//! Persistent home initialization and startup payloads written as the image user.

use super::agent_config::{
    claude_mcp_entry, codex_config_merge, generate_runtime_settings, sync_claude_playwright,
    sync_codex_playwright, with_playwright_guidance,
};
use super::{checked_status, volume_exists, CONTAINER_HOME};
use crate::config::AppConfig;
use crate::runtime::ContainerRuntime;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;

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

/// Label records the creating CLI; the in-volume marker records the last
/// successfully applied injection version (volume labels are immutable).
pub const HOME_VERSION_LABEL: &str = "ai-pod.created-version";
const HOME_VERSION_FILE: &str = ".ai-pod-version";

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct ImageUser {
    pub(super) home: String,
    uid: u64,
    gid: u64,
}

/// Cache the image's effective identity by immutable image ID. Legacy HOME-only
/// cache entries are re-probed once to obtain ownership for transferred files.
pub(super) fn image_user(
    rt: &ContainerRuntime,
    config: &AppConfig,
    image: &str,
) -> Result<ImageUser> {
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
pub(super) fn execute_main_container(
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

pub(super) fn image_command(rt: &ContainerRuntime, image: &str) -> Result<Vec<String>> {
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

pub(super) fn ensure_home_volume(rt: &ContainerRuntime, volume: &str) -> Result<()> {
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

pub(super) const MAIN_BOOTSTRAP: &str = r#"init=$1
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
