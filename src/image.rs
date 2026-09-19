use anyhow::{Context, Result};
use colored::Colorize;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::runtime::ContainerRuntime;

pub const DOCKERFILE_NAME: &str = "ai-pod.Dockerfile";

/// Derives a stable, human-readable image name from the workspace path.
/// Format: `{dirname}-{6-char hash}`, e.g. `myproject-12aef3`.
pub fn image_name(workspace: &Path) -> String {
    // Sanitise the last path component: lowercase, only [a-z0-9._-], trim dashes.
    let label = workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    let label = label.trim_matches('-');
    // Docker/Podman tags must start with an alphanumeric character (e.g. temp dirs
    // like `.tmpXXX` would otherwise produce an invalid tag).
    let label = label.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let label = if label.is_empty() { "project" } else { label };

    let hash = Sha256::digest(workspace.to_string_lossy().as_bytes());
    let short_hash = hex::encode(&hash[..3]); // 6 hex chars

    format!("{}-{}", label, short_hash)
}

/// Quote a string for safe inclusion in a `sh -c` command line.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The shell form of the image build, for callers that run it through the
/// file-based command runner instead of inheriting stdio (see the
/// `rebuild_image` MCP tool). Mirrors the arguments used by [`build_image`].
fn build_command_string(
    rt: &ContainerRuntime,
    dockerfile: &Path,
    image: &str,
    no_cache: bool,
) -> String {
    let context = dockerfile.parent().unwrap_or(Path::new("."));
    let mut parts = vec![rt.cmd().to_string(), "build".to_string()];
    if no_cache {
        parts.push("--no-cache".to_string());
    }
    // Docker build containers don't get the host gateway for free.
    if rt.kind == crate::runtime::RuntimeKind::Docker {
        parts.push(sh_quote(&rt.add_host_arg()));
    }
    parts.extend([
        "--build-arg".to_string(),
        sh_quote(&format!("AI_POD_VERSION={}", env!("CARGO_PKG_VERSION"))),
        "--build-arg".to_string(),
        sh_quote(&format!("HOST_GATEWAY={}", rt.host_gateway())),
        "-t".to_string(),
        sh_quote(image),
        "-f".to_string(),
        sh_quote(&dockerfile.to_string_lossy()),
        sh_quote(&context.to_string_lossy()),
    ]);
    parts.join(" ")
}

/// Build the image, then — only if the build succeeded — run `test_command` in
/// a throwaway container of the freshly built image so the caller can verify
/// the tools it asked for are actually installed. The workspace is not
/// mounted: this checks the *image*, not the project.
pub fn rebuild_and_test_command(
    rt: &ContainerRuntime,
    dockerfile: &Path,
    image: &str,
    no_cache: bool,
    test_command: &str,
) -> String {
    let build = build_command_string(rt, dockerfile, image, no_cache);
    let test = format!(
        "{} run --rm {} --entrypoint sh {} -c {}",
        rt.cmd(),
        sh_quote(&rt.add_host_arg()),
        sh_quote(image),
        sh_quote(test_command),
    );
    format!("{build} && {test}")
}

fn image_exists(rt: &ContainerRuntime, image: &str) -> Result<bool> {
    let status = rt
        .command()
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context(format!("Failed to run {}", rt.cmd()))?;
    Ok(status.success())
}

pub fn needs_build(rt: &ContainerRuntime, image: &str, force: bool) -> Result<bool> {
    if force {
        return Ok(true);
    }
    Ok(!image_exists(rt, image)?)
}

pub fn build_image(rt: &ContainerRuntime, dockerfile: &Path, image: &str, no_cache: bool) -> Result<()> {
    eprintln!("{}", "Building container image...".blue().bold());

    let version_arg = format!("AI_POD_VERSION={}", env!("CARGO_PKG_VERSION"));
    let gateway_arg = format!("HOST_GATEWAY={}", rt.host_gateway());
    let mut cmd = rt.command();
    cmd.arg("build");
    if no_cache {
        cmd.arg("--no-cache");
    }
    // For Docker, host.docker.internal is not automatically available in build
    // containers — we need to inject it explicitly.
    if rt.kind == crate::runtime::RuntimeKind::Docker {
        cmd.args(["--add-host", &format!("{}:host-gateway", rt.host_gateway())]);
    }
    cmd.args([
        "--build-arg",
        &version_arg,
        "--build-arg",
        &gateway_arg,
        "-t",
        image,
        "-f",
        &dockerfile.to_string_lossy(),
        &dockerfile.parent().unwrap_or(Path::new(".")).to_string_lossy(),
    ]);

    // Keep the shared server alive during the build. The server auto-shuts-down
    // after 30 s of inactivity with no containers running. POST /keep-alive
    // immediately so the timer is bumped before the build's first long step,
    // then re-bump every 10 s for safety margin.
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let keepalive_thread = std::thread::spawn(move || {
        let client = reqwest::blocking::Client::new();
        let url = format!(
            "http://127.0.0.1:{}/keep-alive",
            crate::server::lifecycle::MCP_PORT
        );
        let _ = client.post(&url).send();
        loop {
            match stop_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let _ = client.post(&url).send();
                }
            }
        }
    });

    let status = cmd
        .status()
        .context(format!("Failed to run {} build", rt.cmd()));

    let _ = stop_tx.send(());
    let _ = keepalive_thread.join();

    if !status?.success() {
        anyhow::bail!("{} build failed", rt.cmd());
    }

    eprintln!("{}", "Image built successfully.".green().bold());
    Ok(())
}

pub fn ensure_image(rt: &ContainerRuntime, dockerfile: &Path, image: &str, force: bool, no_cache: bool) -> Result<()> {
    if needs_build(rt, image, force)? {
        build_image(rt, dockerfile, image, no_cache)?;
    } else {
        eprintln!("{}", "Container image is up to date.".green());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn image_name_uses_last_path_component() {
        let name = image_name(Path::new("/home/user/myproject"));
        assert!(name.starts_with("myproject-"));
    }

    #[test]
    fn image_name_is_lowercase() {
        let name = image_name(Path::new("/home/user/MyProject"));
        assert!(name.starts_with("myproject-"));
    }

    #[test]
    fn image_name_sanitises_special_chars() {
        let name = image_name(Path::new("/home/user/my project!"));
        // spaces and ! become dashes, trimmed
        assert!(name.starts_with("my-project--") || name.starts_with("my-project-"));
        assert!(!name.contains(' '));
        assert!(!name.contains('!'));
    }

    #[test]
    fn image_name_short_hash_is_6_hex_chars() {
        let name = image_name(Path::new("/home/user/myproject"));
        let hash_part = name.split('-').last().unwrap();
        assert_eq!(hash_part.len(), 6);
        assert!(hash_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn image_name_strips_leading_dot() {
        // Temp dirs like /tmp/.tmpXXX must produce a valid (non-dot-prefixed) tag.
        let name = image_name(Path::new("/tmp/.tmpfoo123"));
        assert!(!name.starts_with('.'), "tag must not start with dot: {name}");
        assert!(name.chars().next().map_or(false, |c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn image_name_is_deterministic() {
        let path = Path::new("/home/user/myproject");
        assert_eq!(image_name(path), image_name(path));
    }

    #[test]
    fn image_name_differs_for_different_paths() {
        let a = image_name(Path::new("/home/user/project-a"));
        let b = image_name(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn image_name_differs_for_same_dirname_different_parent() {
        let a = image_name(Path::new("/alice/code/myproject"));
        let b = image_name(Path::new("/bob/code/myproject"));
        assert_ne!(a, b);
    }

    fn test_rt(kind: crate::runtime::RuntimeKind) -> ContainerRuntime {
        ContainerRuntime {
            kind,
            dry_run: false,
        }
    }

    #[test]
    fn sh_quote_escapes_embedded_single_quotes() {
        assert_eq!(sh_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(sh_quote("plain"), "'plain'");
    }

    #[test]
    fn rebuild_command_builds_then_tests_the_fresh_image() {
        use crate::runtime::RuntimeKind;
        let cmd = rebuild_and_test_command(
            &test_rt(RuntimeKind::Podman),
            Path::new("/ws/ai-pod.Dockerfile"),
            "ws-abc123",
            false,
            "node --version",
        );
        let (build, test) = cmd.split_once(" && ").expect("build must gate the test");
        assert!(build.starts_with("podman build "));
        assert!(build.contains("-t 'ws-abc123'"));
        assert!(build.contains("-f '/ws/ai-pod.Dockerfile'"));
        // Build context is the Dockerfile's directory.
        assert!(
            build.ends_with(" '/ws'"),
            "unexpected build context: {build}"
        );
        assert!(build.contains("--build-arg 'HOST_GATEWAY=host.containers.internal'"));
        assert!(build.contains(&format!(
            "--build-arg 'AI_POD_VERSION={}'",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(!build.contains("--no-cache"));
        // Throwaway container, no workspace mount, test command quoted as one arg.
        assert!(test.starts_with("podman run --rm "));
        assert!(test.contains("--entrypoint sh 'ws-abc123' -c 'node --version'"));
        assert!(!test.contains("-v "));
    }

    #[test]
    fn rebuild_command_honours_no_cache() {
        use crate::runtime::RuntimeKind;
        let cmd = rebuild_and_test_command(
            &test_rt(RuntimeKind::Podman),
            Path::new("/ws/ai-pod.Dockerfile"),
            "img",
            true,
            "true",
        );
        assert!(cmd.contains("podman build --no-cache "));
    }

    #[test]
    fn rebuild_command_adds_host_gateway_for_docker_builds() {
        use crate::runtime::RuntimeKind;
        let cmd = rebuild_and_test_command(
            &test_rt(RuntimeKind::Docker),
            Path::new("/ws/ai-pod.Dockerfile"),
            "img",
            false,
            "true",
        );
        assert!(cmd.contains("docker build '--add-host=host.docker.internal:host-gateway'"));
    }

    #[test]
    fn rebuild_command_quotes_a_hostile_test_command() {
        use crate::runtime::RuntimeKind;
        let cmd = rebuild_and_test_command(
            &test_rt(RuntimeKind::Podman),
            Path::new("/ws/ai-pod.Dockerfile"),
            "img",
            false,
            "echo hi'; rm -rf /tmp/x; echo '",
        );
        // The injected shell metacharacters stay inside a single quoted argument.
        assert!(
            cmd.ends_with(r#"-c 'echo hi'\''; rm -rf /tmp/x; echo '\'''"#),
            "got: {cmd}"
        );
    }

    #[test]
    fn needs_build_returns_true_when_force() {
        use crate::runtime::{ContainerRuntime, RuntimeKind};
        let rt = ContainerRuntime { kind: RuntimeKind::Podman, dry_run: false };
        assert!(needs_build(&rt, "any-image", true).unwrap());
    }
}
