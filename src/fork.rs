use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::AppConfig;

fn generate_name() -> Option<String> {
    use petname::Generator;
    let mut rng = rand::thread_rng();
    petname::Petnames::default().generate(&mut rng, 2, "-")
}

fn validate_fork_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Fork name must not be empty.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("Fork name must contain only lowercase letters, numbers, and hyphens.");
    }
    if name.starts_with('-') {
        bail!("Fork name must not start with a hyphen.");
    }
    Ok(())
}

fn is_git_repo(workspace: &Path) -> bool {
    Command::new("git")
        .args(["-C", &workspace.to_string_lossy(), "rev-parse", "--git-dir"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn branch_exists(workspace: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "-C",
            &workspace.to_string_lossy(),
            "rev-parse",
            "--verify",
            branch,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn create_fork(
    config: &AppConfig,
    workspace: &Path,
    name: Option<&str>,
) -> Result<(String, PathBuf)> {
    if !is_git_repo(workspace) {
        bail!("Current directory is not a git repository. Fork requires git.");
    }

    let fork_name = match name {
        Some(n) => {
            validate_fork_name(n)?;
            n.to_string()
        }
        None => {
            let mut attempts = 0;
            loop {
                let candidate =
                    generate_name().context("Failed to generate a fork name")?;
                let worktree_path = config.worktrees_dir().join(&candidate);
                if !worktree_path.exists() {
                    break candidate;
                }
                attempts += 1;
                if attempts >= 10 {
                    bail!(
                        "Failed to generate a unique fork name after 10 attempts. \
                         Please specify a name explicitly."
                    );
                }
            }
        }
    };

    let worktree_path = config.worktrees_dir().join(&fork_name);
    let branch_name = format!("fork/{}", fork_name);

    if worktree_path.exists() {
        bail!(
            "Fork name '{}' already exists at {}. Choose a different name or remove the existing worktree.",
            fork_name,
            worktree_path.display()
        );
    }

    if branch_exists(workspace, &branch_name) {
        bail!(
            "Branch '{}' already exists. Choose a different fork name.",
            branch_name
        );
    }

    std::fs::create_dir_all(config.worktrees_dir())
        .context("Failed to create worktrees directory")?;

    let output = Command::new("git")
        .args([
            "-C",
            &workspace.to_string_lossy(),
            "worktree",
            "add",
            "-b",
            &branch_name,
            &worktree_path.to_string_lossy(),
        ])
        .output()
        .context("Failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create git worktree: {}", stderr.trim());
    }

    println!(
        "{} {} (branch {})",
        "Fork created:".green().bold(),
        fork_name,
        branch_name.blue()
    );
    println!("{} {}", "Worktree:".blue(), worktree_path.display());

    Ok((fork_name, worktree_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_fork_names() {
        assert!(validate_fork_name("my-fork").is_ok());
        assert!(validate_fork_name("test123").is_ok());
        assert!(validate_fork_name("a").is_ok());
        assert!(validate_fork_name("abc-def-ghi").is_ok());
    }

    #[test]
    fn invalid_fork_names() {
        assert!(validate_fork_name("").is_err());
        assert!(validate_fork_name("-leading").is_err());
        assert!(validate_fork_name("UPPER").is_err());
        assert!(validate_fork_name("has space").is_err());
        assert!(validate_fork_name("has_underscore").is_err());
    }

    #[test]
    fn generate_name_returns_two_words() {
        let name = generate_name().unwrap();
        assert!(name.contains('-'), "expected hyphen in '{}'", name);
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 2, "expected 2 parts in '{}'", name);
    }
}
