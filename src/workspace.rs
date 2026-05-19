use sha2::{Digest, Sha256};
use std::path::Path;

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
    format!("ai-pod-{}-home", workspace_hash(workspace))
}

/// Per-workspace named volume that shadow-mounts /app/{dir} inside the container.
pub fn mask_volume_name(workspace: &Path, dir: &str) -> String {
    format!("ai-pod-{}-mask-{}", workspace_hash(workspace), dir)
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
}
