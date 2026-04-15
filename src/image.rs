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

fn image_exists(rt: &ContainerRuntime, image: &str) -> Result<bool> {
    let status = rt
        .command()
        .args(["image", "exists", image])
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
    println!("{}", "Building container image...".blue().bold());

    let version_arg = format!("AI_POD_VERSION={}", env!("CARGO_PKG_VERSION"));
    let mut cmd = rt.command();
    cmd.arg("build");
    if no_cache {
        cmd.arg("--no-cache");
    }
    cmd.args([
        "--build-arg",
        &version_arg,
        "-t",
        image,
        "-f",
        &dockerfile.to_string_lossy(),
        &dockerfile.parent().unwrap_or(Path::new(".")).to_string_lossy(),
    ]);

    let status = cmd
        .status()
        .context(format!("Failed to run {} build", rt.cmd()))?;

    if !status.success() {
        anyhow::bail!("{} build failed", rt.cmd());
    }

    println!("{}", "Image built successfully.".green().bold());
    Ok(())
}

pub fn ensure_image(rt: &ContainerRuntime, dockerfile: &Path, image: &str, force: bool) -> Result<()> {
    if needs_build(rt, image, force)? {
        build_image(rt, dockerfile, image, force)?;
    } else {
        println!("{}", "Container image is up to date.".green());
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

    #[test]
    fn needs_build_returns_true_when_force() {
        use crate::runtime::{ContainerRuntime, RuntimeKind};
        let rt = ContainerRuntime { kind: RuntimeKind::Podman };
        assert!(needs_build(&rt, "any-image", true).unwrap());
    }
}
