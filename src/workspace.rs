use sha2::{Digest, Sha256};
use std::path::Path;

pub fn workspace_hash(workspace: &Path) -> String {
    let workspace_str = workspace.to_string_lossy();
    let hash = Sha256::digest(workspace_str.as_bytes());
    hex::encode(&hash[..6])
}

pub fn container_name(workspace: &Path) -> String {
    format!("claude-{}", workspace_hash(workspace))
}

pub fn volume_name(workspace: &Path) -> String {
    format!("claude-{}-home", workspace_hash(workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn container_name_starts_with_claude() {
        let name = container_name(Path::new("/home/user/myproject"));
        assert!(name.starts_with("claude-"));
    }

    #[test]
    fn container_name_has_expected_length() {
        // "claude-" (7) + 12 hex chars = 19
        let name = container_name(Path::new("/home/user/myproject"));
        assert_eq!(name.len(), 19);
    }

    #[test]
    fn volume_name_is_container_name_plus_home() {
        let p = Path::new("/home/user/myproject");
        assert_eq!(volume_name(p), format!("{}-home", container_name(p)));
    }

    #[test]
    fn names_differ_for_different_paths() {
        let a = container_name(Path::new("/home/user/project-a"));
        let b = container_name(Path::new("/home/user/project-b"));
        assert_ne!(a, b);
    }
}
