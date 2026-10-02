use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Keep system paths isolated from the container's own filesystem.
fn preserves_host_path(workspace: &Path) -> bool {
    workspace.is_absolute()
        && workspace != Path::new("/")
        && ![
            "/bin",
            "/boot",
            "/dev",
            "/etc",
            "/lib",
            "/lib64",
            "/lost+found",
            "/media",
            "/mnt",
            "/opt",
            "/proc",
            "/root",
            "/run",
            "/sbin",
            "/srv",
            "/sys",
            "/tmp",
            "/usr",
            "/var",
        ]
        .iter()
        .any(|root| workspace.starts_with(root))
}

/// Preserve host paths except beneath Linux system directories.
pub fn container_workdir(workspace: &Path) -> &Path {
    if preserves_host_path(workspace) {
        workspace
    } else {
        Path::new("/app")
    }
}

/// Git lists the main checkout first, followed by its linked worktrees.
/// Linux system directories retain the original workspace isolation.
pub fn main_checkout(workspace: &Path) -> Option<PathBuf> {
    if !preserves_host_path(workspace) {
        return None;
    }
    // A normal checkout (including a subdirectory of one) needs no extra mount.
    let metadata = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    if !metadata.status.success() {
        return None;
    }
    let metadata = std::str::from_utf8(&metadata.stdout).ok()?;
    let mut paths = metadata.lines();
    let git_dir = std::fs::canonicalize(paths.next()?).ok()?;
    let common_dir = std::fs::canonicalize(paths.next()?).ok()?;
    if git_dir == common_dir {
        return None;
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["worktree", "list", "--porcelain", "-z"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let first = output.stdout.split(|b| *b == 0).next()?;
    let path = std::str::from_utf8(first).ok()?.strip_prefix("worktree ")?;
    std::fs::canonicalize(path).ok()
}

/// Bind mounts for the workspace and, for linked worktrees, the main checkout.
/// Nested worktrees are already included in the main checkout mount.
pub fn workspace_mount_args(workspace: &Path) -> Vec<String> {
    let mut roots = vec![workspace.to_path_buf()];
    if let Some(main) = main_checkout(workspace) {
        if workspace.starts_with(&main) {
            roots = vec![main];
        } else if main != workspace {
            roots.push(main);
        }
    }
    roots
        .into_iter()
        .flat_map(|root| {
            let target = if preserves_host_path(workspace) {
                root.as_path()
            } else {
                Path::new("/app")
            };
            // These checkouts can be mounted by multiple concurrent sessions.
            vec![
                "-v".into(),
                format!(
                    "{}:{}:{}",
                    root.display(),
                    target.display(),
                    if preserves_host_path(workspace) {
                        "z"
                    } else {
                        "Z"
                    }
                ),
            ]
        })
        .collect()
}

pub fn workspace_hash(workspace: &Path) -> String {
    let workspace_str = workspace.to_string_lossy();
    let hash = Sha256::digest(workspace_str.as_bytes());
    hex::encode(&hash[..6])
}

/// Stable prefix shared by all containers for this workspace.
pub fn container_prefix(workspace: &Path) -> String {
    format!("ai-pod-{}", workspace_hash(workspace))
}

/// Generate a fresh 8-char session id (the suffix of a new container name).
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string().replace("-", "")[..8].to_string()
}

/// Compose a container name from the workspace prefix and a session id.
pub fn container_name_for(workspace: &Path, session_id: &str) -> String {
    format!("{}-{}", container_prefix(workspace), session_id)
}

/// Unique container name for a new session.
pub fn new_container_name(workspace: &Path) -> String {
    container_name_for(workspace, &new_session_id())
}

/// Extract the trailing session id from a container name, if it matches the
/// `ai-pod-{hash}-{session}` pattern.
pub fn session_id_from_container_name(name: &str) -> Option<String> {
    let suffix = name.rsplit_once('-')?.1;
    if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(suffix.to_string())
    } else {
        None
    }
}

pub fn volume_name(workspace: &Path) -> String {
    let main = main_checkout(workspace);
    format!(
        "ai-pod-{}-home",
        workspace_hash(main.as_deref().unwrap_or(workspace))
    )
}

/// Per-workspace named volume that shadow-mounts /app/{dir} inside the container.
pub fn mask_volume_name(workspace: &Path, dir: &str) -> String {
    format!("ai-pod-{}-mask-{}", workspace_hash(workspace), dir)
}

/// Per-workspace bridge network used to wire service containers to the
/// running main container so the agent can reach them by name.
pub fn service_network_name(workspace: &Path) -> String {
    format!("ai-pod-{}-net", workspace_hash(workspace))
}

/// Container name for a service requested by the given main-container session.
/// Embedding the session id keeps two concurrent ai-pod sessions on the same
/// workspace from colliding on the same service name.
pub fn service_container_name(workspace: &Path, session_id: &str, name: &str) -> String {
    format!(
        "ai-pod-{}-{}-svc-{}",
        workspace_hash(workspace),
        session_id,
        name
    )
}

/// Validate a user-supplied service name. Returns the trimmed name on success.
///
/// Rules: 1..=30 ASCII chars, lowercase alphanumeric or `-`, must start with an
/// alphanumeric. Doubles as DNS alias on the workspace network, so we keep it
/// to a strict subset of RFC 1123 hostname rules. The 30-char cap keeps the
/// derived container name under podman/docker's 63-char limit.
pub fn validate_service_name(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("service name must not be empty".into());
    }
    if name.len() > 30 {
        return Err(format!(
            "service name '{}' too long ({} chars, max 30)",
            name,
            name.len()
        ));
    }
    let first = name.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!(
            "service name '{}' must start with a lowercase letter or digit",
            name
        ));
    }
    for c in name.chars() {
        let ok =
            c.is_ascii_digit() || (c.is_ascii_alphabetic() && c.is_ascii_lowercase()) || c == '-';
        if !ok {
            return Err(format!(
                "service name '{}' contains invalid character '{}' (only [a-z0-9-] allowed)",
                name, c
            ));
        }
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn host_paths_exclude_linux_system_directories() {
        for path in [
            "/home/user/project",
            "/Users/user/project",
            "/projects/repo",
            "/homework/project",
        ] {
            assert_eq!(container_workdir(Path::new(path)), Path::new(path));
        }
        for path in [
            "/",
            "/etc/project",
            "/usr/local/project",
            "/var/project",
            "/tmp/project",
            "/proc",
            "/root/project",
            "/opt/project",
            "/mnt/project",
            "relative",
        ] {
            assert_eq!(container_workdir(Path::new(path)), Path::new("/app"));
        }
    }

    #[test]
    fn home_worktrees_share_volume_and_preserve_paths() {
        let home = dirs::home_dir().unwrap();
        if !preserves_host_path(&home) {
            return; // System directories deliberately use the isolated /app mount.
        }
        let temp = tempfile::tempdir_in(home).unwrap();
        let main = temp.path().join("main checkout");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let nested = main.join(".ai-pod/worktrees/nested");
        let sibling = temp.path().join("sibling worktree");
        git(
            &main,
            &["worktree", "add", "-b", "nested", nested.to_str().unwrap()],
        );
        git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "sibling",
                sibling.to_str().unwrap(),
            ],
        );
        assert_eq!(main_checkout(&nested), Some(main.clone()));
        assert_eq!(main_checkout(&sibling), Some(main.clone()));
        assert_eq!(volume_name(&main), volume_name(&nested));
        assert_eq!(volume_name(&main), volume_name(&sibling));
        assert_eq!(container_workdir(&nested), nested);
        assert_eq!(workspace_mount_args(&nested), workspace_mount_args(&main));
        assert_eq!(
            workspace_mount_args(&sibling),
            vec![
                "-v".to_string(),
                format!("{0}:{0}:z", sibling.display()),
                "-v".to_string(),
                format!("{0}:{0}:z", main.display())
            ]
        );
        assert_eq!(workspace_mount_args(&main).len(), 2);
        let subdir = main.join("ordinary-subdirectory");
        std::fs::create_dir(&subdir).unwrap();
        assert_eq!(main_checkout(&subdir), None);
        assert_eq!(
            workspace_mount_args(&subdir),
            vec!["-v".to_string(), format!("{0}:{0}:z", subdir.display())]
        );
    }

    #[test]
    fn system_directory_keeps_app_mount_and_independent_volume() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        git(temp.path(), &["init"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let linked = temp.path().join("linked");
        git(
            temp.path(),
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        assert_eq!(main_checkout(&linked), None);
        assert_eq!(container_workdir(&linked), Path::new("/app"));
        assert_ne!(volume_name(temp.path()), volume_name(&linked));
        assert_eq!(
            workspace_mount_args(&linked),
            vec!["-v".to_string(), format!("{}:/app:Z", linked.display())]
        );
        assert_eq!(
            container_workdir(Path::new("/homework/project")),
            Path::new("/homework/project")
        );
    }

    #[test]
    fn hash_is_deterministic() {
        let p = Path::new("/home/user/myproject");
        assert_eq!(workspace_hash(p), workspace_hash(p));
    }

    #[test]
    fn hash_is_12_chars() {
        let h = workspace_hash(Path::new("/home/user/myproject"));
        assert_eq!(h.len(), 12);
    }

    #[test]
    fn container_prefix_starts_with_ai_pod() {
        let prefix = container_prefix(Path::new("/home/user/myproject"));
        assert!(prefix.starts_with("ai-pod-"));
    }

    #[test]
    fn new_container_name_starts_with_prefix() {
        let p = Path::new("/home/user/myproject");
        let name = new_container_name(p);
        assert!(name.starts_with(&container_prefix(p)));
    }

    #[test]
    fn new_container_name_is_unique() {
        let p = Path::new("/home/user/myproject");
        assert_ne!(new_container_name(p), new_container_name(p));
    }

    #[test]
    fn volume_name_uses_workspace_hash() {
        let p = Path::new("/home/user/myproject");
        assert_eq!(volume_name(p), format!("ai-pod-{}-home", workspace_hash(p)));
    }

    #[test]
    fn mask_volume_name_includes_hash_and_dir() {
        let p = Path::new("/home/user/myproject");
        let name = mask_volume_name(p, "node_modules");
        assert_eq!(
            name,
            format!("ai-pod-{}-mask-node_modules", workspace_hash(p))
        );
    }

    #[test]
    fn mask_volume_names_differ_per_workspace_and_dir() {
        let a = Path::new("/home/user/project-a");
        let b = Path::new("/home/user/project-b");
        assert_ne!(mask_volume_name(a, "target"), mask_volume_name(b, "target"));
        assert_ne!(mask_volume_name(a, "target"), mask_volume_name(a, "dist"));
    }

    #[test]
    fn names_differ_for_different_paths() {
        let a = container_prefix(Path::new("/home/user/project-a"));
        let b = container_prefix(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }

    #[test]
    fn service_network_name_is_per_workspace() {
        let p = Path::new("/home/user/myproject");
        assert_eq!(
            service_network_name(p),
            format!("ai-pod-{}-net", workspace_hash(p))
        );
        let other = Path::new("/home/user/other");
        assert_ne!(service_network_name(p), service_network_name(other));
    }

    #[test]
    fn service_container_name_embeds_workspace_and_session() {
        let p = Path::new("/home/user/myproject");
        assert_eq!(
            service_container_name(p, "abcd1234", "postgres"),
            format!("ai-pod-{}-abcd1234-svc-postgres", workspace_hash(p))
        );
    }

    #[test]
    fn service_container_name_differs_between_sessions() {
        let p = Path::new("/home/user/myproject");
        assert_ne!(
            service_container_name(p, "aaaa1111", "postgres"),
            service_container_name(p, "bbbb2222", "postgres"),
        );
    }

    #[test]
    fn service_container_name_stays_under_docker_limit() {
        let p = Path::new("/home/user/myproject");
        let name = service_container_name(p, "abcd1234", &"x".repeat(30));
        assert!(name.len() <= 63, "got {} chars: {}", name.len(), name);
    }

    #[test]
    fn validate_service_name_accepts_typical_names() {
        assert!(validate_service_name("postgres").is_ok());
        assert!(validate_service_name("redis").is_ok());
        assert!(validate_service_name("redis-7").is_ok());
        assert!(validate_service_name("db1").is_ok());
        assert!(validate_service_name("a").is_ok());
    }

    #[test]
    fn validate_service_name_rejects_uppercase_and_punctuation() {
        assert!(validate_service_name("Postgres").is_err());
        assert!(validate_service_name("pg/x").is_err());
        assert!(validate_service_name("pg_x").is_err());
        assert!(validate_service_name("pg.x").is_err());
    }

    #[test]
    fn validate_service_name_uppercase_first_char_message_blames_first_char() {
        let err = validate_service_name("Postgres").unwrap_err();
        assert!(
            err.contains("must start with a lowercase letter or digit"),
            "expected first-char error for uppercase first letter, got: {err}"
        );
    }

    #[test]
    fn validate_service_name_rejects_empty_and_too_long() {
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name(&"a".repeat(31)).is_err());
    }

    #[test]
    fn validate_service_name_rejects_leading_dash() {
        assert!(validate_service_name("-postgres").is_err());
    }
}
