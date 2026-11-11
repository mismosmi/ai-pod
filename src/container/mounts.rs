//! Host bind mounts and per-workspace mask volumes.

use super::{volume_exists, CONTAINER_HOME};
use crate::config::MountSpec;
use crate::runtime::ContainerRuntime;
use crate::workspace::mask_volume_name;
use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;

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
pub(super) fn mask_mount_args(
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        assert!(
            args.is_empty(),
            "stored invalid host should be warn-skipped"
        );
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
