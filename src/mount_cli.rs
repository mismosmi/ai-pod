//! Host-side `ai-pod mount` subcommand: add, remove, list bind-mounts that
//! are applied to every ai-pod container launch.

use anyhow::Result;
use colored::Colorize;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::config::{AppConfig, GlobalConfig, MountSpec};

const CONTAINER_HOME: &str = "/home/ai-pod";

/// Container path prefixes that would break the container if shadowed by a
/// user mount.
const RESERVED_CONTAINER_PREFIXES: &[&str] =
    &["/proc", "/sys", "/dev", "/etc", "/tmp", "/run", "/var/run"];

/// Parse a user-provided `host[:container]` spec. The host portion is
/// tilde-expanded against `home_dir`.
pub(crate) fn parse_spec(s: &str, writable: bool, home_dir: &Path) -> Result<MountSpec> {
    let (host_raw, container) = match s.split_once(':') {
        Some((h, c)) => (h, Some(c.to_string())),
        None => (s, None),
    };
    let host = expand_tilde(host_raw, home_dir);
    validate_host_path(&host)?;
    if let Some(c) = &container {
        validate_container_path(c)?;
    }
    Ok(MountSpec {
        host,
        container,
        writable,
    })
}

pub(crate) fn expand_tilde(s: &str, home_dir: &Path) -> String {
    if s == "~" {
        return home_dir.display().to_string();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir.join(rest).display().to_string();
    }
    s.to_string()
}

pub(crate) fn validate_host_path(p: &str) -> Result<()> {
    if p.is_empty() {
        anyhow::bail!("Host path must not be empty");
    }
    if p.contains('\0') {
        anyhow::bail!("Host path must not contain null bytes");
    }
    let path = Path::new(p);
    if !path.is_absolute() {
        anyhow::bail!(
            "Host path must be absolute (got {}). Use ~/path or /absolute/path.",
            p
        );
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        anyhow::bail!("Host path must not contain '..' segments");
    }
    Ok(())
}

pub(crate) fn validate_container_path(p: &str) -> Result<()> {
    if p.is_empty() {
        anyhow::bail!("Container path must not be empty");
    }
    if p.contains('\0') {
        anyhow::bail!("Container path must not contain null bytes");
    }
    let path = Path::new(p);
    if !path.is_absolute() {
        anyhow::bail!("Container path must be absolute (got {})", p);
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        anyhow::bail!("Container path must not contain '..' segments");
    }
    if p == "/" {
        anyhow::bail!("Container path '/' is not a valid mount target");
    }
    if p == CONTAINER_HOME {
        anyhow::bail!(
            "Container target {} would shadow the entire home volume; pick a sub-path",
            CONTAINER_HOME
        );
    }
    if p == "/app" || p.starts_with("/app/") {
        anyhow::bail!("Container target must not be under /app (workspace bind)");
    }
    for reserved in RESERVED_CONTAINER_PREFIXES {
        if p == *reserved || p.starts_with(&format!("{}/", reserved)) {
            anyhow::bail!("Container target {} is reserved", p);
        }
    }
    Ok(())
}

pub fn run_add(config: &AppConfig, spec_str: &str, writable: bool) -> Result<()> {
    let spec = parse_spec(spec_str, writable, &config.home_dir)?;

    // If no explicit container path is given, ensure host is under $HOME so
    // the launch-time resolver won't fail later.
    if spec.container.is_none() {
        let host = Path::new(&spec.host);
        if host.strip_prefix(&config.home_dir).is_err() {
            anyhow::bail!(
                "Host path {} is outside $HOME. Supply an explicit container path: \
                 ai-pod mount add {}:<container-path>",
                spec.host,
                spec.host
            );
        }
    }

    let mut gc = GlobalConfig::load(config);
    if !gc.add(spec.clone()) {
        println!("{} {}", "Already mounted:".yellow(), spec.host);
        return Ok(());
    }
    gc.save(config)?;

    let target = crate::container::resolve_container_target(&spec, &config.home_dir)
        .unwrap_or_else(|_| "(invalid)".to_string());
    println!(
        "{} {} → {} ({})",
        "Mounted:".green().bold(),
        spec.host,
        target,
        if spec.writable { "rw" } else { "ro" }
    );

    warn_if_unreadable(&spec)?;
    Ok(())
}

pub fn run_remove(config: &AppConfig, host: &str) -> Result<()> {
    let expanded = expand_tilde(host, &config.home_dir);
    let mut gc = GlobalConfig::load(config);
    if !gc.remove(&expanded) {
        println!("{} {}", "Not mounted:".yellow(), expanded);
        return Ok(());
    }
    gc.save(config)?;
    println!("{} {}", "Unmounted:".green().bold(), expanded);
    Ok(())
}

pub fn run_list(config: &AppConfig) -> Result<()> {
    let gc = GlobalConfig::load(config);
    if gc.mounts.is_empty() {
        println!(
            "{}",
            "No global mounts configured. Use `ai-pod mount add <host>[:<container>]`."
                .dimmed()
        );
        return Ok(());
    }
    for m in &gc.mounts {
        let target = crate::container::resolve_container_target(m, &config.home_dir)
            .unwrap_or_else(|_| "(invalid)".to_string());
        let mode = if m.writable { "rw" } else { "ro" };
        let exists = if Path::new(&m.host).exists() {
            ""
        } else {
            "  (missing — will be skipped at launch)"
        };
        println!("{:<50} → {:<40} [{}]{}", m.host, target, mode, exists);
    }
    Ok(())
}

/// One-line warning for `mount add` when the host file is mode-restricted in a
/// way that the in-container `ai-pod` user is unlikely to be able to read it.
/// Best-effort; silent on any error.
fn warn_if_unreadable(spec: &MountSpec) -> Result<()> {
    let path = Path::new(&spec.host);
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if !meta.is_file() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o004 != 0 {
        return Ok(());
    }
    eprintln!(
        "{} {} has mode {:o}; the container user may not be able to read it under \
         rootless podman. Consider `chmod o+r {}` or rely on docker / rootful podman.",
        "warning:".yellow().bold(),
        spec.host,
        mode,
        spec.host
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_spec_host_only() {
        let dir = TempDir::new().unwrap();
        let spec = parse_spec("/abs/path", false, dir.path()).unwrap();
        assert_eq!(spec.host, "/abs/path");
        assert_eq!(spec.container, None);
        assert!(!spec.writable);
    }

    #[test]
    fn parse_spec_host_and_container() {
        let dir = TempDir::new().unwrap();
        let spec = parse_spec("/abs/a:/abs/b", true, dir.path()).unwrap();
        assert_eq!(spec.host, "/abs/a");
        assert_eq!(spec.container.as_deref(), Some("/abs/b"));
        assert!(spec.writable);
    }

    #[test]
    fn parse_spec_expands_tilde_in_host() {
        let dir = TempDir::new().unwrap();
        let home = dir.path();
        let spec = parse_spec("~/.claude/skills", false, home).unwrap();
        assert_eq!(spec.host, home.join(".claude/skills").display().to_string());
    }

    #[test]
    fn parse_spec_rejects_relative_host() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("relative/path", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn parse_spec_rejects_traversal_in_host() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/a/../b", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn parse_spec_rejects_app_target() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/app/foo", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("/app"));
    }

    #[test]
    fn parse_spec_rejects_app_root_target() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/app", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("/app"));
    }

    #[test]
    fn parse_spec_rejects_reserved_target() {
        let dir = TempDir::new().unwrap();
        for r in ["/proc", "/proc/sys", "/sys", "/dev/null", "/etc/passwd"] {
            let err = parse_spec(&format!("/host:{}", r), false, dir.path()).unwrap_err();
            assert!(err.to_string().contains("reserved"), "target {} should be reserved", r);
        }
    }

    #[test]
    fn parse_spec_rejects_container_home_exactly() {
        let dir = TempDir::new().unwrap();
        let err = parse_spec("/host:/home/ai-pod", false, dir.path()).unwrap_err();
        assert!(err.to_string().contains("home volume"));
    }

    #[test]
    fn parse_spec_accepts_container_home_subpath() {
        let dir = TempDir::new().unwrap();
        let spec = parse_spec("/host:/home/ai-pod/.claude/skills", false, dir.path()).unwrap();
        assert_eq!(spec.container.as_deref(), Some("/home/ai-pod/.claude/skills"));
    }

    #[test]
    fn expand_tilde_only_at_start() {
        let home = Path::new("/H");
        assert_eq!(expand_tilde("~", home), "/H");
        assert_eq!(expand_tilde("~/foo", home), "/H/foo");
        assert_eq!(expand_tilde("/abs/~/foo", home), "/abs/~/foo");
        assert_eq!(expand_tilde("/abs", home), "/abs");
    }

    #[test]
    fn run_add_rejects_no_container_outside_home() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home,
        };
        let err = run_add(&config, "/etc/foo", false).unwrap_err();
        assert!(err.to_string().contains("outside $HOME"));
    }

    #[test]
    fn run_add_and_remove_round_trip() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home.clone(),
        };
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();

        run_add(&config, "~/.claude/skills", false).unwrap();
        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1);
        assert_eq!(
            gc.mounts[0].host,
            home.join(".claude/skills").display().to_string()
        );

        run_remove(
            &config,
            &home.join(".claude/skills").display().to_string(),
        )
        .unwrap();
        let gc = GlobalConfig::load(&config);
        assert!(gc.mounts.is_empty());
    }

    #[test]
    fn run_add_dedups_by_host() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home.clone(),
        };
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();

        run_add(&config, "~/.claude/skills", false).unwrap();
        run_add(&config, "~/.claude/skills", true).unwrap();
        let gc = GlobalConfig::load(&config);
        assert_eq!(gc.mounts.len(), 1, "duplicate host should not be added");
        assert!(!gc.mounts[0].writable, "first add wins");
    }
}
