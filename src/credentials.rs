use anyhow::Result;
use colored::Colorize;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::server::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

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
        "⚠  Potential credential files found in workspace:"
            .yellow()
            .bold()
    );

    let mut any_kept = false;
    let mut state_changed = false;

    for path in &pending {
        let rel = path.strip_prefix(workspace).unwrap_or(path);
        println!("\n  {} {}", "•".yellow(), rel.display());

        let choices = &[
            "Keep (allow in container, warn next time)",
            "Ignore (never warn for this file again)",
            "Move to ~/.env-files/<slug>/ and symlink",
        ];
        let selection = dialoguer::Select::new()
            .with_prompt("What would you like to do?")
            .items(choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                any_kept = true;
            }
            1 => {
                state.add_ignored_credential(&rel.to_string_lossy());
                state_changed = true;
            }
            2 => {
                let dst_dir = config.env_files_project_dir(workspace);
                let file_name = path.file_name().unwrap_or_default();
                let dst = dst_dir.join(file_name);
                move_and_symlink(path, &dst)?;
                println!(
                    "  {} Moved to {} and symlinked",
                    "✓".green(),
                    dst.display()
                );
                state_changed = true;
            }
            _ => unreachable!(),
        }
    }

    if state_changed {
        state.save(&state_path)?;
    }

    if any_kept {
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
