use anyhow::{Context, Result};
use colored::Colorize;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::server::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvFileStatus {
    /// Symlink in workspace pointing into ~/.env-files/<slug>/ — AI cannot read.
    Hidden,
    /// Regular file in workspace, not in the ignore list — AI can read, warns at startup.
    Exposed,
    /// Regular file in workspace, in the ignore list — AI can read, no warning.
    Ignored,
}

#[derive(Debug, Clone)]
pub struct EnvFileEntry {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub status: EnvFileStatus,
    /// For Hidden entries: where the real file lives.
    /// For other entries: where it would be moved if hidden.
    pub destination: PathBuf,
}

const CREDENTIAL_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.staging",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    ".npmrc",
    ".pypirc",
    ".netrc",
    "credentials.json",
    "service-account.json",
    "terraform.tfstate",
];

const CREDENTIAL_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "tfvars",
];

const CREDENTIAL_DIR_PATTERNS: &[&str] = &[
    ".aws/credentials",
    ".aws/config",
    ".ssh/",
    ".gnupg/",
];

fn is_credential_file(path: &Path) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };

    if CREDENTIAL_PATTERNS.iter().any(|p| file_name == *p) {
        return true;
    }

    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if CREDENTIAL_EXTENSIONS.iter().any(|e| ext == *e) {
            return true;
        }
    }

    let path_str = path.to_string_lossy();
    if CREDENTIAL_DIR_PATTERNS.iter().any(|p| path_str.contains(p)) {
        return true;
    }

    false
}

#[cfg(test)]
mod tests_is_credential_file {
    use super::*;

    #[test]
    fn dot_env_exact_match() {
        assert!(is_credential_file(std::path::Path::new("/project/.env")));
    }

    #[test]
    fn dot_env_local() {
        assert!(is_credential_file(std::path::Path::new("/project/.env.local")));
    }

    #[test]
    fn ssh_private_key() {
        assert!(is_credential_file(std::path::Path::new("/home/user/.ssh/id_rsa")));
    }

    #[test]
    fn pem_extension() {
        assert!(is_credential_file(std::path::Path::new("/certs/server.pem")));
    }

    #[test]
    fn key_extension() {
        assert!(is_credential_file(std::path::Path::new("/keys/private.key")));
    }

    #[test]
    fn p12_extension() {
        assert!(is_credential_file(std::path::Path::new("/certs/bundle.p12")));
    }

    #[test]
    fn aws_credentials_path_pattern() {
        assert!(is_credential_file(std::path::Path::new(
            "/home/user/.aws/credentials"
        )));
    }

    #[test]
    fn gnupg_path_pattern() {
        assert!(is_credential_file(std::path::Path::new(
            "/home/user/.gnupg/secring.gpg"
        )));
    }

    #[test]
    fn normal_rust_file_is_not_credential() {
        assert!(!is_credential_file(std::path::Path::new("/project/src/main.rs")));
    }

    #[test]
    fn normal_json_file_is_not_credential() {
        assert!(!is_credential_file(std::path::Path::new("/project/config.json")));
    }

    #[test]
    fn credentials_json_is_credential() {
        assert!(is_credential_file(std::path::Path::new(
            "/project/credentials.json"
        )));
    }

    #[test]
    fn service_account_json_is_credential() {
        assert!(is_credential_file(std::path::Path::new(
            "/project/service-account.json"
        )));
    }
}

pub fn scan_workspace(workspace: &Path) -> Vec<PathBuf> {
    WalkDir::new(workspace)
        .max_depth(5)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Skip common non-relevant directories
            !matches!(
                name.as_ref(),
                "node_modules" | ".git" | "target" | "__pycache__" | ".venv" | "venv"
            )
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| is_credential_file(e.path()))
        .map(|e| e.into_path())
        .collect()
}

pub fn check_credentials(workspace: &Path, config: &AppConfig) -> Result<bool> {
    // Canonicalize so WalkDir paths and strip_prefix share the same base.
    let workspace_buf = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let workspace = workspace_buf.as_path();

    let found = scan_workspace(workspace);
    if found.is_empty() {
        return Ok(true);
    }

    let hash = workspace_hash(workspace);
    let state_path = config.project_state_file(&hash);
    let mut state = ProjectState::load(&state_path);

    let pending: Vec<PathBuf> = found
        .into_iter()
        .filter(|path| {
            let rel = path.strip_prefix(workspace).unwrap_or(path);
            !state.is_credential_ignored(&rel.to_string_lossy())
        })
        .collect();

    if pending.is_empty() {
        return Ok(true);
    }

    println!(
        "\n{}",
        "⚠  Potentially sensitive files detected in workspace:"
            .yellow()
            .bold()
    );
    println!(
        "  {}",
        "The workspace is mounted into the AI container, so these files will be readable by the AI."
            .dimmed()
    );

    let mut any_exposed = false;
    let mut state_changed = false;

    for path in &pending {
        let rel = path.strip_prefix(workspace).unwrap_or(path);
        println!("\n  {} {}", "•".yellow(), rel.display());

        let choices = &[
            "Hide from AI  (move to home directory, keep symlink for host tools)",
            "Expose to AI  (keep in workspace, suppress future warnings)",
            "Expose to AI  (keep in workspace, warn again next time)",
        ];
        let selection = dialoguer::Select::new()
            .with_prompt("This file will be readable inside the container — what would you like to do?")
            .items(choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                let dst_dir = config.env_files_project_dir(workspace);
                let file_name = path.file_name().unwrap_or_default();
                let dst = dst_dir.join(file_name);
                move_and_symlink(path, &dst)?;
                println!(
                    "  {} Hidden: moved to {} and replaced with a symlink",
                    "✓".green(),
                    dst.display()
                );
                state_changed = true;
            }
            1 => {
                state.add_ignored_credential(&rel.to_string_lossy());
                state_changed = true;
                any_exposed = true;
            }
            2 => {
                any_exposed = true;
            }
            _ => unreachable!(),
        }
    }

    if state_changed {
        state.save(&state_path)?;
    }

    if any_exposed {
        let proceed = dialoguer::Confirm::new()
            .with_prompt("Continue anyway?")
            .default(false)
            .interact()?;
        return Ok(proceed);
    }

    Ok(true)
}

fn move_and_symlink(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(_) = std::fs::rename(src, dst) {
        // Cross-device move fallback
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)?;
    }
    std::os::unix::fs::symlink(dst, src)?;
    Ok(())
}

/// Enumerate every sensitive file in the workspace, classified by management
/// status. Symlinks pointing into `~/.env-files/<slug>/` are reported as
/// `Hidden`; regular files are reported as `Exposed` or `Ignored` depending on
/// the project state's ignore list. The result is sorted by relative path.
pub fn list_env_files(workspace: &Path, config: &AppConfig) -> Vec<EnvFileEntry> {
    let workspace_buf =
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let workspace = workspace_buf.as_path();
    let env_dir = config.env_files_project_dir(workspace);
    let hash = workspace_hash(workspace);
    let state = ProjectState::load(&config.project_state_file(&hash));

    let mut entries = Vec::new();

    for ent in WalkDir::new(workspace)
        .max_depth(5)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                "node_modules" | ".git" | "target" | "__pycache__" | ".venv" | "venv"
            )
        })
        .filter_map(|e| e.ok())
    {
        let path = ent.path();
        if !is_credential_file(path) {
            continue;
        }
        let ft = ent.file_type();
        let rel = path
            .strip_prefix(workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let file_name = path.file_name().unwrap_or_default();

        if ft.is_symlink() {
            let Ok(target) = std::fs::read_link(path) else {
                continue;
            };
            let resolved = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(workspace).join(&target)
            };
            if resolved.starts_with(&env_dir) {
                entries.push(EnvFileEntry {
                    rel_path: rel,
                    abs_path: path.to_path_buf(),
                    status: EnvFileStatus::Hidden,
                    destination: resolved,
                });
            }
        } else if ft.is_file() {
            let status = if state.is_credential_ignored(&rel) {
                EnvFileStatus::Ignored
            } else {
                EnvFileStatus::Exposed
            };
            entries.push(EnvFileEntry {
                rel_path: rel,
                abs_path: path.to_path_buf(),
                status,
                destination: env_dir.join(file_name),
            });
        }
    }

    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    entries
}

/// Move a workspace file into `~/.env-files/<slug>/` and replace it with a
/// symlink pointing at the new location. Errors if the workspace path is
/// already a symlink (i.e. the file is already hidden).
pub fn hide_file(workspace: &Path, config: &AppConfig, rel_path: &str) -> Result<PathBuf> {
    let src = workspace.join(rel_path);
    let md = std::fs::symlink_metadata(&src)
        .with_context(|| format!("File not found: {}", rel_path))?;
    if md.file_type().is_symlink() {
        anyhow::bail!("{} is already hidden", rel_path);
    }
    if !md.file_type().is_file() {
        anyhow::bail!("{} is not a regular file", rel_path);
    }
    let dst_dir = config.env_files_project_dir(workspace);
    let file_name = src.file_name().unwrap_or_default();
    let dst = dst_dir.join(file_name);
    move_and_symlink(&src, &dst)?;
    Ok(dst)
}

/// Reverse of `hide_file`: resolve the symlink, move the real file back into
/// the workspace, and remove the symlink. Errors if `rel_path` is not a
/// symlink.
pub fn unhide_file(workspace: &Path, rel_path: &str) -> Result<()> {
    let src = workspace.join(rel_path);
    let md = std::fs::symlink_metadata(&src)
        .with_context(|| format!("File not found: {}", rel_path))?;
    if !md.file_type().is_symlink() {
        anyhow::bail!("{} is not hidden", rel_path);
    }
    let target = std::fs::read_link(&src)?;
    let target = if target.is_absolute() {
        target
    } else {
        src.parent().unwrap_or(workspace).join(target)
    };
    if !target.exists() {
        anyhow::bail!(
            "Symlink target is missing: {}. Refusing to remove the dangling symlink.",
            target.display()
        );
    }
    std::fs::remove_file(&src).context("Failed to remove workspace symlink")?;
    if std::fs::rename(&target, &src).is_err() {
        // Cross-device move fallback
        std::fs::copy(&target, &src).context("Failed to copy file back into workspace")?;
        std::fs::remove_file(&target).ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests_ignore_list {
    use super::*;
    use crate::server::lifecycle::ProjectState;
    use tempfile::TempDir;

    #[test]
    fn ignored_credential_file_is_filtered_out() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=123").unwrap();

        let found = scan_workspace(dir.path());
        assert_eq!(found.len(), 1);

        let rel = found[0]
            .strip_prefix(dir.path())
            .expect("strip_prefix should succeed")
            .to_string_lossy()
            .to_string();
        let mut state = ProjectState::default();
        state.add_ignored_credential(&rel);

        let pending: Vec<PathBuf> = scan_workspace(dir.path())
            .into_iter()
            .filter(|path| {
                let r = path.strip_prefix(dir.path()).unwrap_or(path);
                !state.is_credential_ignored(&r.to_string_lossy())
            })
            .collect();

        assert!(
            pending.is_empty(),
            "ignored file should be filtered out, got: {:?}",
            pending
        );
    }

    #[test]
    fn ignored_credential_file_is_filtered_when_workspace_is_canonicalized() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=123").unwrap();

        let canonical = std::fs::canonicalize(dir.path()).unwrap();

        let mut state = ProjectState::default();
        state.add_ignored_credential(".env");

        let pending: Vec<PathBuf> = scan_workspace(&canonical)
            .into_iter()
            .filter(|path| {
                let r = path.strip_prefix(&canonical).unwrap_or(path);
                !state.is_credential_ignored(&r.to_string_lossy())
            })
            .collect();

        assert!(
            pending.is_empty(),
            "ignored file should be filtered out after canonicalization"
        );
    }
}

#[cfg(test)]
mod tests_scan {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_workspace_returns_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(scan_workspace(dir.path()).is_empty());
    }

    #[test]
    fn finds_dot_env_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=123").unwrap();
        let found = scan_workspace(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with(".env"));
    }

    #[test]
    fn finds_multiple_credential_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1").unwrap();
        std::fs::write(dir.path().join("id_rsa"), "key").unwrap();
        std::fs::write(dir.path().join("cert.pem"), "cert").unwrap();
        let found = scan_workspace(dir.path());
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn ignores_normal_source_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("README.md"), "# readme").unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(scan_workspace(dir.path()).is_empty());
    }

    #[test]
    fn skips_node_modules() {
        let dir = TempDir::new().unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir(&nm).unwrap();
        std::fs::write(nm.join(".env"), "SECRET=123").unwrap();
        assert!(scan_workspace(dir.path()).is_empty());
    }

    #[test]
    fn skips_git_directory() {
        let dir = TempDir::new().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("id_rsa"), "key").unwrap();
        assert!(scan_workspace(dir.path()).is_empty());
    }

    #[test]
    fn skips_target_directory() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join(".env"), "SECRET=123").unwrap();
        assert!(scan_workspace(dir.path()).is_empty());
    }

    #[test]
    fn finds_credentials_in_subdirectory() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("config");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("service-account.json"), r#"{}"#).unwrap();
        let found = scan_workspace(dir.path());
        assert_eq!(found.len(), 1);
    }
}

#[cfg(test)]
mod tests_env_files_management {
    use super::*;
    use crate::server::lifecycle::ProjectState;
    use tempfile::TempDir;

    fn temp_config(home: &Path) -> AppConfig {
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            config_dir,
            home_dir: home.to_path_buf(),
        }
    }

    #[test]
    fn remove_ignored_credential_removes_entry() {
        let mut state = ProjectState::default();
        state.add_ignored_credential(".env");
        state.add_ignored_credential("id_rsa");
        assert!(state.is_credential_ignored(".env"));
        state.remove_ignored_credential(".env");
        assert!(!state.is_credential_ignored(".env"));
        assert!(state.is_credential_ignored("id_rsa"));
    }

    #[test]
    fn hide_file_moves_and_symlinks() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config = temp_config(home.path());

        let src = workspace.path().join(".env");
        std::fs::write(&src, "SECRET=123").unwrap();

        let dst = hide_file(workspace.path(), &config, ".env").unwrap();

        assert!(dst.exists(), "destination file should exist");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "SECRET=123");

        let md = std::fs::symlink_metadata(&src).unwrap();
        assert!(md.file_type().is_symlink(), "workspace entry should be a symlink");
        assert_eq!(std::fs::read_link(&src).unwrap(), dst);
    }

    #[test]
    fn hide_file_refuses_already_hidden_file() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config = temp_config(home.path());

        let src = workspace.path().join(".env");
        std::fs::write(&src, "SECRET=123").unwrap();
        hide_file(workspace.path(), &config, ".env").unwrap();

        let err = hide_file(workspace.path(), &config, ".env").unwrap_err();
        assert!(err.to_string().contains("already hidden"));
    }

    #[test]
    fn unhide_file_restores_real_file() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config = temp_config(home.path());

        let src = workspace.path().join(".env");
        std::fs::write(&src, "SECRET=123").unwrap();
        hide_file(workspace.path(), &config, ".env").unwrap();

        unhide_file(workspace.path(), ".env").unwrap();

        let md = std::fs::symlink_metadata(&src).unwrap();
        assert!(md.file_type().is_file(), "workspace entry should be a regular file again");
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "SECRET=123");
        let hidden = config.env_files_project_dir(workspace.path()).join(".env");
        assert!(!hidden.exists(), "hidden file should be removed from env-files dir");
    }

    #[test]
    fn unhide_file_refuses_regular_file() {
        let workspace = TempDir::new().unwrap();
        std::fs::write(workspace.path().join(".env"), "SECRET=123").unwrap();
        let err = unhide_file(workspace.path(), ".env").unwrap_err();
        assert!(err.to_string().contains("not hidden"));
    }

    #[test]
    fn list_env_files_classifies_all_three_states() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config = temp_config(home.path());

        // Exposed file
        std::fs::write(workspace.path().join(".env"), "A=1").unwrap();
        // Ignored file
        std::fs::write(workspace.path().join("id_rsa"), "key").unwrap();
        // Hidden file
        std::fs::write(workspace.path().join(".env.local"), "B=2").unwrap();
        hide_file(workspace.path(), &config, ".env.local").unwrap();

        // Mark id_rsa as ignored
        let hash = workspace_hash(workspace.path());
        let state_path = config.project_state_file(&hash);
        let mut state = ProjectState::load(&state_path);
        state.add_ignored_credential("id_rsa");
        state.save(&state_path).unwrap();

        let entries = list_env_files(workspace.path(), &config);
        assert_eq!(entries.len(), 3);

        let by_path: std::collections::HashMap<_, _> = entries
            .iter()
            .map(|e| (e.rel_path.clone(), e.status))
            .collect();
        assert_eq!(by_path[".env"], EnvFileStatus::Exposed);
        assert_eq!(by_path["id_rsa"], EnvFileStatus::Ignored);
        assert_eq!(by_path[".env.local"], EnvFileStatus::Hidden);
    }
}
