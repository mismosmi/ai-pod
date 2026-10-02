//! The `ai-pod` agent skill injected into every container.
//!
//! Rather than spending tokens on a permanently-loaded preamble, ai-pod ships
//! a single skill file that agents load on demand. It teaches the in-container
//! agent how to use the MCP host bridge (and how *not* to: no output piping,
//! no `pkill` on the host) and how to change the image via `ai-pod.Dockerfile`.

use crate::image::DOCKERFILE_NAME;
use crate::runtime::ContainerRuntime;

const SKILL_TEMPLATE: &str = include_str!("../templates/ai-pod-skill.md");

/// Skill directory name — the agent sees the skill as `ai-pod`.
pub const SKILL_NAME: &str = "ai-pod";

/// Directories (relative to the container home) that agents scan for global
/// skills. Claude Code and OpenCode both read `~/.claude/skills`; Codex reads
/// `~/.codex/skills`. One small file in each is cheaper than detecting which
/// agent the workspace's Dockerfile ends up running.
pub const SKILL_DIRS: &[&str] = &[".claude/skills", ".codex/skills"];

/// Absolute in-container paths of the skill file, one per agent skill dir.
pub fn skill_paths(container_home: &str) -> Vec<String> {
    SKILL_DIRS
        .iter()
        .map(|dir| format!("{container_home}/{dir}/{SKILL_NAME}/SKILL.md"))
        .collect()
}

/// In-container directories that must exist before the skill file is copied in.
pub fn skill_dirs(container_home: &str) -> Vec<String> {
    SKILL_DIRS
        .iter()
        .map(|dir| format!("{container_home}/{dir}/{SKILL_NAME}"))
        .collect()
}

/// The skill path a bind-mount at `target` would hide, if any.
///
/// Mounting the host's own `~/.claude/skills` into the container is an
/// advertised use case, and it shadows the copy ai-pod writes into the home
/// volume — the agent then has no idea how to talk to the host. Callers use
/// this to warn instead of failing silently.
pub fn shadowed_path(container_home: &str, target: &str) -> Option<String> {
    let target = target.trim_end_matches('/');
    skill_paths(container_home)
        .into_iter()
        .find(|p| p == target || p.starts_with(&format!("{target}/")))
}

/// Render the skill for a given runtime (the host gateway hostname differs
/// between podman and docker).
pub fn render(rt: &ContainerRuntime) -> String {
    SKILL_TEMPLATE
        .replace("{{HOST_GATEWAY}}", rt.host_gateway())
        .replace("{{DOCKERFILE}}", DOCKERFILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeKind;

    fn rt(kind: RuntimeKind) -> ContainerRuntime {
        ContainerRuntime {
            kind,
            dry_run: false,
        }
    }

    #[test]
    fn render_substitutes_every_placeholder() {
        let s = render(&rt(RuntimeKind::Podman));
        assert!(
            !s.contains("{{"),
            "unsubstituted placeholder left in skill: {s}"
        );
    }

    #[test]
    fn render_uses_the_runtimes_host_gateway() {
        assert!(render(&rt(RuntimeKind::Podman)).contains("host.containers.internal"));
        assert!(render(&rt(RuntimeKind::Docker)).contains("host.docker.internal"));
    }

    #[test]
    fn skill_starts_with_frontmatter_naming_the_skill() {
        let s = render(&rt(RuntimeKind::Podman));
        assert!(s.starts_with("---\n"), "skill must open with frontmatter");
        let end = s[4..].find("\n---").expect("frontmatter must be closed");
        let frontmatter = &s[4..4 + end];
        assert!(frontmatter.contains("name: ai-pod"));
        assert!(frontmatter.contains("description:"));
    }

    #[test]
    fn skill_teaches_the_host_command_constraints() {
        let s = render(&rt(RuntimeKind::Podman));
        // Simple commands, no output shaping — output is already in files.
        assert!(s.contains("| head"), "should call out piping to head/tail");
        assert!(s.contains("./.ai-pod/commands/"));
        // Stopping goes through the MCP tool, not host signals.
        assert!(s.contains("stop_command"));
        assert!(s.contains("pkill"));
    }

    #[test]
    fn skill_documents_dockerfile_edits_and_rebuild() {
        let s = render(&rt(RuntimeKind::Podman));
        assert!(s.contains(DOCKERFILE_NAME));
        assert!(s.contains("rebuild_image"));
        assert!(s.contains("test_command"));
    }

    #[test]
    fn skill_paths_cover_claude_and_codex() {
        let paths = skill_paths("/home/ai-pod");
        assert!(paths.contains(&"/home/ai-pod/.claude/skills/ai-pod/SKILL.md".to_string()));
        assert!(paths.contains(&"/home/ai-pod/.codex/skills/ai-pod/SKILL.md".to_string()));
    }

    #[test]
    fn shadowed_path_flags_mounts_that_hide_the_skill() {
        // The advertised `~/.claude/skills` mount hides the injected skill.
        assert_eq!(
            shadowed_path("/home/ai-pod", "/home/ai-pod/.claude/skills"),
            Some("/home/ai-pod/.claude/skills/ai-pod/SKILL.md".to_string())
        );
        // Trailing slash, the skill's own directory, and the file itself too.
        assert!(shadowed_path("/home/ai-pod", "/home/ai-pod/.claude/skills/").is_some());
        assert!(shadowed_path("/home/ai-pod", "/home/ai-pod/.claude/skills/ai-pod").is_some());
        assert!(
            shadowed_path(
                "/home/ai-pod",
                "/home/ai-pod/.claude/skills/ai-pod/SKILL.md"
            )
            .is_some()
        );
    }

    #[test]
    fn shadowed_path_ignores_unrelated_mounts() {
        assert_eq!(
            shadowed_path("/home/ai-pod", "/home/ai-pod/.claude/agents"),
            None
        );
        // A sibling skill directory is not the ai-pod skill.
        assert_eq!(
            shadowed_path("/home/ai-pod", "/home/ai-pod/.claude/skills/other"),
            None
        );
        // Prefix-of-a-path-segment must not count as a parent directory.
        assert_eq!(
            shadowed_path("/home/ai-pod", "/home/ai-pod/.claude/skill"),
            None
        );
    }

    #[test]
    fn skill_dirs_are_the_parents_of_skill_paths() {
        let dirs = skill_dirs("/home/ai-pod");
        for path in skill_paths("/home/ai-pod") {
            let parent = path.trim_end_matches("/SKILL.md").to_string();
            assert!(dirs.contains(&parent), "missing mkdir target for {path}");
        }
    }
}
