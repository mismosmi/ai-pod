use sha2::{Digest, Sha256};
use std::path::Path;

pub fn workspace_hash(workspace: &Path) -> String {
    let workspace_str = workspace.to_string_lossy();
    let hash = Sha256::digest(workspace_str.as_bytes());
    hex::encode(&hash[..6])
}

/// Stable prefix shared by all containers for this workspace.
pub fn container_prefix(workspace: &Path) -> String {
    format!("claude-{}", workspace_hash(workspace))
}

/// Unique container name for a new session.
pub fn new_container_name(workspace: &Path) -> String {
    let suffix = &uuid::Uuid::new_v4().to_string().replace("-", "")[..8];
    format!("{}-{}", container_prefix(workspace), suffix)
}

pub fn volume_name(workspace: &Path) -> String {
    format!("claude-{}-home", workspace_hash(workspace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
    fn container_prefix_starts_with_claude() {
        let prefix = container_prefix(Path::new("/home/user/myproject"));
        assert!(prefix.starts_with("claude-"));
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
        assert_eq!(volume_name(p), format!("claude-{}-home", workspace_hash(p)));
    }

    #[test]
    fn names_differ_for_different_paths() {
        let a = container_prefix(Path::new("/home/user/project-a"));
        let b = container_prefix(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }
}
