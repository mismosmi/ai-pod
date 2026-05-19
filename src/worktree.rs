//! Git worktree detection.
//!
//! When a workspace's `.git` is a file (rather than a directory), git treats
//! that workspace as a linked worktree. The file contains an absolute path
//! to the per-worktree state inside the main repo's `.git/worktrees/<name>/`,
//! and the per-worktree state's `commondir` points back to the main `.git`.
//!
//! Both of those paths live *outside* the workspace, so a plain `-v
//! <workspace>:/app` bind mount can't reach them and `git` breaks inside the
//! container. `detect()` resolves the main `.git` directory on the host so
//! `launch_container` / `run_in_container` can bind-mount it at the same
//! absolute path inside the container — the `.git` file's absolute pointer
//! then resolves transparently with no rewriting required.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Container-reserved mount prefixes that the worktree mount must not collide
/// with. `/app` is the workspace bind; `/home/ai-pod` is the home volume.
const RESERVED_CONTAINER_PREFIXES: &[&str] = &["/app", "/home/ai-pod"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Canonical host path of the main repo's `.git` directory. Used as both
    /// the source and target of the bind mount inside the container.
    pub main_git_dir: PathBuf,
}

/// Inspect `workspace/.git` and, if the workspace is a linked worktree, return
/// the main repo's `.git` directory so it can be bind-mounted into the
/// container. Returns `Ok(None)` for plain repos, non-repos, and any case we
/// don't fully understand — detection is opportunistic and must not fail a
/// launch on its own.
pub fn detect(workspace: &Path) -> Result<Option<WorktreeInfo>> {
    let dot_git = workspace.join(".git");

    let meta = match std::fs::symlink_metadata(&dot_git) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };

    // Resolve symlinks so we inspect the actual target. A symlinked `.git`
    // pointing at a directory means a plain repo accessed via symlink — no
    // special handling needed.
    let resolved = if meta.file_type().is_symlink() {
        match std::fs::canonicalize(&dot_git) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        }
    } else {
        dot_git.clone()
    };

    let resolved_meta = std::fs::metadata(&resolved).unwrap_or(meta);
    if !resolved_meta.is_file() {
        return Ok(None);
    }

    let contents = match std::fs::read_to_string(&resolved) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    let gitdir_raw = match parse_gitdir_line(&contents) {
        Some(s) => s,
        None => return Ok(None),
    };

    let gitdir = std::fs::canonicalize(PathBuf::from(gitdir_raw))
        .context("failed to canonicalize worktree gitdir path")?;

    let main_git_dir = resolve_main_git_dir(&gitdir)?;

    // Bail out if the sanity check fails — silent skip rather than launch
    // failure for cases we don't recognize as a real git directory.
    if !is_plausible_git_dir(&main_git_dir) {
        return Ok(None);
    }

    // The workspace's bind mount at /app already covers this path.
    let ws_canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    if main_git_dir.starts_with(&ws_canonical) {
        return Ok(None);
    }

    // Reject collisions with reserved container mount points. This only
    // triggers if the host actually stores the main repo under `/app` or
    // `/home/ai-pod`, which would conflict with the workspace bind or the home
    // volume inside the container.
    for reserved in RESERVED_CONTAINER_PREFIXES {
        if main_git_dir.starts_with(reserved) {
            bail!(
                "git worktree main repo at {} conflicts with reserved container path {}",
                main_git_dir.display(),
                reserved
            );
        }
    }

    // Mount specs need exact bytes; reject non-UTF-8 paths early with a clear
    // error rather than letting the runtime do it.
    if main_git_dir.to_str().is_none() {
        bail!(
            "git worktree main repo path is not valid UTF-8: {}",
            main_git_dir.display()
        );
    }

    Ok(Some(WorktreeInfo { main_git_dir }))
}

/// Build the `-v src:dst:Z` mount args for the runtime, mounting the main
/// `.git` directory at the same absolute host path inside the container.
pub fn mount_args(info: &WorktreeInfo) -> Vec<String> {
    let p = info
        .main_git_dir
        .to_str()
        .expect("detect() rejects non-UTF-8 paths");
    vec!["-v".to_string(), format!("{}:{}:Z", p, p)]
}

/// Find the first line of the `.git` file matching `gitdir: <path>` and
/// return the path with surrounding whitespace trimmed.
fn parse_gitdir_line(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("gitdir:") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Given the worktree's per-worktree directory (typically
/// `<main>/.git/worktrees/<name>`), resolve the main repo's `.git` directory.
/// Honours an explicit `commondir` file inside the worktree dir; otherwise
/// falls back to `gitdir.parent().parent()` which gets us out of
/// `.git/worktrees/<name>/`.
fn resolve_main_git_dir(gitdir: &Path) -> Result<PathBuf> {
    let commondir_file = gitdir.join("commondir");
    if let Ok(s) = std::fs::read_to_string(&commondir_file) {
        let value = s.trim();
        if !value.is_empty() {
            let candidate = if Path::new(value).is_absolute() {
                PathBuf::from(value)
            } else {
                gitdir.join(value)
            };
            return std::fs::canonicalize(&candidate)
                .context("failed to canonicalize commondir target");
        }
    }
    let parent = gitdir
        .parent()
        .and_then(Path::parent)
        .context("worktree gitdir has no grandparent")?;
    std::fs::canonicalize(parent).context("failed to canonicalize main .git parent")
}

/// Cheap heuristic that a path looks like a real `.git` directory: it has
/// both an `objects/` subdirectory and a `HEAD` file. Good enough to catch
/// pathological setups (e.g. nested worktree-of-a-worktree we don't support).
fn is_plausible_git_dir(p: &Path) -> bool {
    p.join("objects").is_dir() && p.join("HEAD").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_main_git(root: &Path) -> PathBuf {
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::create_dir_all(git_dir.join("refs")).unwrap();
        fs::create_dir_all(git_dir.join("worktrees")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        git_dir
    }

    fn make_worktree_gitdir(main_git: &Path, name: &str) -> PathBuf {
        let dir = main_git.join("worktrees").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        fs::write(dir.join("commondir"), "../..\n").unwrap();
        dir
    }

    fn write_dot_git_file(workspace: &Path, gitdir: &Path) {
        fs::write(
            workspace.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
    }

    #[test]
    fn parse_gitdir_line_strips_prefix_and_whitespace() {
        assert_eq!(
            parse_gitdir_line("gitdir: /foo/bar\n"),
            Some("/foo/bar".to_string())
        );
        assert_eq!(
            parse_gitdir_line("  gitdir:   /foo/bar   \n"),
            Some("/foo/bar".to_string())
        );
        assert_eq!(
            parse_gitdir_line("\n\ngitdir: /foo/bar\n"),
            Some("/foo/bar".to_string())
        );
    }

    #[test]
    fn parse_gitdir_line_returns_none_on_malformed() {
        assert_eq!(parse_gitdir_line(""), None);
        assert_eq!(parse_gitdir_line("not a gitdir\n"), None);
        assert_eq!(parse_gitdir_line("gitdir:\n"), None);
        assert_eq!(parse_gitdir_line("gitdir:    \n"), None);
    }

    #[test]
    fn detect_returns_none_for_missing_dot_git() {
        let ws = TempDir::new().unwrap();
        assert!(detect(ws.path()).unwrap().is_none());
    }

    #[test]
    fn detect_returns_none_for_plain_repo() {
        let root = TempDir::new().unwrap();
        let _git = make_main_git(root.path());
        assert!(detect(root.path()).unwrap().is_none());
    }

    #[test]
    fn detect_returns_none_for_malformed_dot_git_file() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join(".git"), "not a gitdir line\n").unwrap();
        assert!(detect(ws.path()).unwrap().is_none());
    }

    #[test]
    fn detect_worktree_with_relative_commondir() {
        let root = TempDir::new().unwrap();
        let main = root.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let main_git = make_main_git(&main);
        let wt_gitdir = make_worktree_gitdir(&main_git, "feature");

        let wt_dir = root.path().join("wt");
        fs::create_dir_all(&wt_dir).unwrap();
        write_dot_git_file(&wt_dir, &wt_gitdir);

        let info = detect(&wt_dir).unwrap().expect("worktree must be detected");
        assert_eq!(
            std::fs::canonicalize(&info.main_git_dir).unwrap(),
            std::fs::canonicalize(&main_git).unwrap()
        );
    }

    #[test]
    fn detect_worktree_with_absolute_commondir() {
        let root = TempDir::new().unwrap();
        let main = root.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let main_git = make_main_git(&main);
        let wt_gitdir = make_worktree_gitdir(&main_git, "feature");
        // Overwrite commondir with an absolute path
        let abs = std::fs::canonicalize(&main_git).unwrap();
        fs::write(wt_gitdir.join("commondir"), format!("{}\n", abs.display())).unwrap();

        let wt_dir = root.path().join("wt");
        fs::create_dir_all(&wt_dir).unwrap();
        write_dot_git_file(&wt_dir, &wt_gitdir);

        let info = detect(&wt_dir).unwrap().expect("worktree must be detected");
        assert_eq!(
            std::fs::canonicalize(&info.main_git_dir).unwrap(),
            abs
        );
    }

    #[test]
    fn detect_falls_back_to_grandparent_when_no_commondir() {
        let root = TempDir::new().unwrap();
        let main = root.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let main_git = make_main_git(&main);
        let wt_gitdir = make_worktree_gitdir(&main_git, "feature");
        // Remove commondir to force fallback
        fs::remove_file(wt_gitdir.join("commondir")).unwrap();

        let wt_dir = root.path().join("wt");
        fs::create_dir_all(&wt_dir).unwrap();
        write_dot_git_file(&wt_dir, &wt_gitdir);

        let info = detect(&wt_dir).unwrap().expect("worktree must be detected");
        assert_eq!(
            std::fs::canonicalize(&info.main_git_dir).unwrap(),
            std::fs::canonicalize(&main_git).unwrap()
        );
    }

    #[test]
    fn detect_returns_none_when_main_git_dir_is_implausible() {
        let root = TempDir::new().unwrap();
        // Create a gitdir-like target that lacks HEAD and objects in its parent's parent
        let fake_gitdir = root.path().join("not-a-git").join("worktrees").join("x");
        fs::create_dir_all(&fake_gitdir).unwrap();

        let wt_dir = root.path().join("wt");
        fs::create_dir_all(&wt_dir).unwrap();
        write_dot_git_file(&wt_dir, &fake_gitdir);

        assert!(detect(&wt_dir).unwrap().is_none());
    }

    #[test]
    fn detect_returns_none_when_main_git_dir_is_under_workspace() {
        // Workspace contains both the main repo and the worktree's `.git` file.
        // The main `.git` is already reachable via the /app bind, so we should
        // skip the extra mount.
        let root = TempDir::new().unwrap();
        let ws = root.path().join("ws");
        let main = ws.join("main");
        fs::create_dir_all(&main).unwrap();
        let main_git = make_main_git(&main);
        let wt_gitdir = make_worktree_gitdir(&main_git, "feature");

        write_dot_git_file(&ws, &wt_gitdir);

        assert!(detect(&ws).unwrap().is_none());
    }

    #[test]
    fn detect_follows_symlinked_dot_git_file() {
        let root = TempDir::new().unwrap();
        let main = root.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let main_git = make_main_git(&main);
        let wt_gitdir = make_worktree_gitdir(&main_git, "feature");

        let wt_dir = root.path().join("wt");
        fs::create_dir_all(&wt_dir).unwrap();
        let real_dot_git = root.path().join("real-dot-git");
        fs::write(
            &real_dot_git,
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();
        std::os::unix::fs::symlink(&real_dot_git, wt_dir.join(".git")).unwrap();

        let info = detect(&wt_dir).unwrap().expect("worktree must be detected");
        assert_eq!(
            std::fs::canonicalize(&info.main_git_dir).unwrap(),
            std::fs::canonicalize(&main_git).unwrap()
        );
    }

    #[test]
    fn detect_returns_none_when_dot_git_symlinks_to_directory() {
        let root = TempDir::new().unwrap();
        let real_git = root.path().join("elsewhere-git");
        fs::create_dir_all(real_git.join("objects")).unwrap();
        fs::write(real_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let ws = root.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        std::os::unix::fs::symlink(&real_git, ws.join(".git")).unwrap();

        assert!(detect(&ws).unwrap().is_none());
    }

    #[test]
    fn mount_args_returns_same_path_bind_mount() {
        let info = WorktreeInfo {
            main_git_dir: PathBuf::from("/host/repo/.git"),
        };
        let args = mount_args(&info);
        assert_eq!(args, vec!["-v".to_string(), "/host/repo/.git:/host/repo/.git:Z".to_string()]);
    }
}
