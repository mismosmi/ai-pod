//! File-based command execution.
//!
//! All host commands (whether triggered by MCP or REST) write their output to
//! `{workspace}/.ai-pod/commands/{session_id}/{command_id}/{stdout,stderr,exit,command}`.
//! The 5-second wait window is implemented here so MCP and REST callers share
//! identical semantics.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use super::AppState;

/// In-memory record of a running or recently finished command.
#[derive(Clone)]
pub struct CommandHandle {
    pub session_id: String,
    pub command_id: String,
    pub command: String,
    pub started_at: u64,
    /// Live process group leader pid; 0 once the child has been reaped.
    pub pid: Arc<AtomicI32>,
    /// Set to true when stop_command was issued for this command.
    pub killed: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Running,
    Finished,
    Killed,
}

#[derive(Serialize)]
pub struct CommandSummary {
    pub command_id: String,
    pub session_id: String,
    pub command: String,
    pub status: CommandStatus,
    pub exit_code: Option<i32>,
    pub started_at: u64,
}

#[derive(Serialize)]
pub struct RunCommandOutcome {
    pub command_id: String,
    pub session_id: String,
    pub status: CommandStatus,
    pub exit_code: Option<i32>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_path: String,
    pub stderr_path: String,
    pub exit_path: String,
}

const TAIL_LINES: usize = 10;
const RUN_WAIT: Duration = Duration::from_secs(5);

pub fn commands_root(workspace: &Path) -> PathBuf {
    workspace.join(".ai-pod").join("commands")
}

pub fn command_dir(workspace: &Path, session_id: &str, command_id: &str) -> PathBuf {
    commands_root(workspace).join(session_id).join(command_id)
}

/// In-container view of the stdout/stderr/exit files. The workspace is bind-mounted
/// at `/app` (see `src/container.rs`), so this is the path the agent inside the
/// container sees and can pass to its file Read tool.
pub fn container_paths(session_id: &str, command_id: &str) -> (String, String, String) {
    let base = format!("/app/.ai-pod/commands/{session_id}/{command_id}");
    (
        format!("{base}/stdout"),
        format!("{base}/stderr"),
        format!("{base}/exit"),
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_tail(path: &Path, max_lines: usize) -> String {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    // Read the whole file — command output is usually small. For very large
    // outputs we could seek from the end, but keep it simple.
    let mut buf = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.read_to_string(&mut buf);
    let lines: Vec<&str> = buf.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn read_exit_file(path: &Path) -> Option<ExitInfo> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed == "killed" {
        Some(ExitInfo {
            killed: true,
            exit_code: None,
        })
    } else {
        Some(ExitInfo {
            killed: false,
            exit_code: trimmed.parse().ok(),
        })
    }
}

struct ExitInfo {
    killed: bool,
    exit_code: Option<i32>,
}

/// Spawn a command, wait up to 5s for it to finish, and return the outcome.
/// Caller must already have approved the command.
pub async fn spawn_and_wait(
    state: &AppState,
    workspace: &Path,
    session_id: &str,
    command: &str,
) -> Result<RunCommandOutcome> {
    let command_id = uuid::Uuid::new_v4().to_string().replace("-", "")[..8].to_string();
    let dir = command_dir(workspace, session_id, &command_id);
    std::fs::create_dir_all(&dir).context("Failed to create command output directory")?;
    std::fs::write(dir.join("command"), command).ok();

    let stdout_path = dir.join("stdout");
    let stderr_path = dir.join("stderr");
    let exit_path = dir.join("exit");

    let stdout_file = File::create(&stdout_path).context("Failed to create stdout file")?;
    let stderr_file = File::create(&stderr_path).context("Failed to create stderr file")?;

    let mut child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .process_group(0)
        .spawn()
        .context("Failed to spawn command")?;

    let pid = child.id().unwrap_or(0) as i32;
    let started_at = now_secs();
    let pid_atomic = Arc::new(AtomicI32::new(pid));
    let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let handle = CommandHandle {
        session_id: session_id.to_string(),
        command_id: command_id.clone(),
        command: command.to_string(),
        started_at,
        pid: pid_atomic.clone(),
        killed: killed.clone(),
    };
    {
        let mut map = state.commands.lock().await;
        map.insert((session_id.to_string(), command_id.clone()), handle);
    }

    // Try to finish within RUN_WAIT.
    let wait_res = tokio::time::timeout(RUN_WAIT, child.wait()).await;

    match wait_res {
        Ok(Ok(status)) => {
            let exit_code = status.code().unwrap_or(-1);
            let was_killed = killed.load(Ordering::SeqCst);
            let exit_text = if was_killed {
                "killed".to_string()
            } else {
                exit_code.to_string()
            };
            let _ = std::fs::write(&exit_path, exit_text);
            pid_atomic.store(0, Ordering::SeqCst);
            remove_handle(state, session_id, &command_id).await;
            let cstatus = if was_killed {
                CommandStatus::Killed
            } else {
                CommandStatus::Finished
            };
            Ok(RunCommandOutcome {
                command_id,
                session_id: session_id.to_string(),
                status: cstatus,
                exit_code: if was_killed { None } else { Some(exit_code) },
                stdout_tail: read_tail(&stdout_path, TAIL_LINES),
                stderr_tail: read_tail(&stderr_path, TAIL_LINES),
                stdout_path: stdout_path.to_string_lossy().into(),
                stderr_path: stderr_path.to_string_lossy().into(),
                exit_path: exit_path.to_string_lossy().into(),
            })
        }
        Ok(Err(_)) => {
            let _ = std::fs::write(&exit_path, "-1");
            pid_atomic.store(0, Ordering::SeqCst);
            remove_handle(state, session_id, &command_id).await;
            Ok(RunCommandOutcome {
                command_id,
                session_id: session_id.to_string(),
                status: CommandStatus::Finished,
                exit_code: Some(-1),
                stdout_tail: read_tail(&stdout_path, TAIL_LINES),
                stderr_tail: read_tail(&stderr_path, TAIL_LINES),
                stdout_path: stdout_path.to_string_lossy().into(),
                stderr_path: stderr_path.to_string_lossy().into(),
                exit_path: exit_path.to_string_lossy().into(),
            })
        }
        Err(_) => {
            // Timed out → keep running; spawn a watcher to write exit when it
            // eventually finishes.
            let exit_path_watch = exit_path.clone();
            let pid_watch = pid_atomic.clone();
            let killed_watch = killed.clone();
            let state_watch = state.clone();
            let sid_watch = session_id.to_string();
            let cid_watch = command_id.clone();
            tokio::spawn(async move {
                let result = child.wait().await;
                let exit_code = result.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1);
                let was_killed = killed_watch.load(Ordering::SeqCst);
                let exit_text = if was_killed {
                    "killed".to_string()
                } else {
                    exit_code.to_string()
                };
                let _ = std::fs::write(&exit_path_watch, exit_text);
                pid_watch.store(0, Ordering::SeqCst);
                remove_handle(&state_watch, &sid_watch, &cid_watch).await;
            });
            Ok(RunCommandOutcome {
                command_id,
                session_id: session_id.to_string(),
                status: CommandStatus::Running,
                exit_code: None,
                stdout_tail: read_tail(&stdout_path, TAIL_LINES),
                stderr_tail: read_tail(&stderr_path, TAIL_LINES),
                stdout_path: stdout_path.to_string_lossy().into(),
                stderr_path: stderr_path.to_string_lossy().into(),
                exit_path: exit_path.to_string_lossy().into(),
            })
        }
    }
}

async fn remove_handle(state: &AppState, session_id: &str, command_id: &str) {
    let mut map = state.commands.lock().await;
    map.remove(&(session_id.to_string(), command_id.to_string()));
}

/// Look up the on-disk and in-memory state for a command.
pub async fn status_for(
    state: &AppState,
    workspace: &Path,
    session_id: &str,
    command_id: &str,
) -> Option<RunCommandOutcome> {
    let dir = command_dir(workspace, session_id, command_id);
    if !dir.exists() {
        return None;
    }
    let stdout_path = dir.join("stdout");
    let stderr_path = dir.join("stderr");
    let exit_path = dir.join("exit");

    let exit_info = read_exit_file(&exit_path);
    let in_flight = {
        let map = state.commands.lock().await;
        map.contains_key(&(session_id.to_string(), command_id.to_string()))
    };

    let (status, exit_code) = match exit_info {
        Some(ExitInfo {
            killed: true, ..
        }) => (CommandStatus::Killed, None),
        Some(ExitInfo {
            killed: false,
            exit_code,
        }) => (CommandStatus::Finished, exit_code),
        None if in_flight => (CommandStatus::Running, None),
        None => (CommandStatus::Finished, None),
    };

    Some(RunCommandOutcome {
        command_id: command_id.to_string(),
        session_id: session_id.to_string(),
        status,
        exit_code,
        stdout_tail: read_tail(&stdout_path, TAIL_LINES),
        stderr_tail: read_tail(&stderr_path, TAIL_LINES),
        stdout_path: stdout_path.to_string_lossy().into(),
        stderr_path: stderr_path.to_string_lossy().into(),
        exit_path: exit_path.to_string_lossy().into(),
    })
}

/// Stop a running command. SIGTERM, 5s grace, SIGKILL.
pub async fn stop(state: &AppState, session_id: &str, command_id: &str) -> bool {
    let pid = {
        let map = state.commands.lock().await;
        match map.get(&(session_id.to_string(), command_id.to_string())) {
            Some(h) => {
                h.killed.store(true, Ordering::SeqCst);
                h.pid.load(Ordering::SeqCst)
            }
            None => return false,
        }
    };
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let state_clone = state.clone();
    let sid = session_id.to_string();
    let cid = command_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let still_alive = {
            let map = state_clone.commands.lock().await;
            map.get(&(sid, cid))
                .map(|h| h.pid.load(Ordering::SeqCst))
                .unwrap_or(0)
        };
        if still_alive > 0 {
            unsafe { libc::kill(-still_alive, libc::SIGKILL) };
        }
    });
    true
}

/// Enumerate commands for a workspace. If `session_id` is `Some`, only that
/// session's directory is scanned. Reflects on-disk state plus in-memory
/// running state.
pub async fn list(
    state: &AppState,
    workspace: &Path,
    session_id: Option<&str>,
) -> Vec<CommandSummary> {
    let root = commands_root(workspace);
    let mut sessions: Vec<String> = Vec::new();
    if let Some(s) = session_id {
        sessions.push(s.to_string());
    } else if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                sessions.push(name.to_string());
            }
        }
    }

    let in_flight: std::collections::HashSet<(String, String)> = {
        let map = state.commands.lock().await;
        map.keys().cloned().collect()
    };

    let mut out = Vec::new();
    for sid in sessions {
        let sdir = root.join(&sid);
        let entries = match std::fs::read_dir(&sdir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let cdir = entry.path();
            let cid = match entry.file_name().to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            let command = std::fs::read_to_string(cdir.join("command"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let started_at = std::fs::metadata(&cdir)
                .ok()
                .and_then(|m| m.created().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let exit_info = read_exit_file(&cdir.join("exit"));
            let running = in_flight.contains(&(sid.clone(), cid.clone()));
            let (status, exit_code) = match exit_info {
                Some(ExitInfo { killed: true, .. }) => (CommandStatus::Killed, None),
                Some(ExitInfo {
                    killed: false,
                    exit_code,
                }) => (CommandStatus::Finished, exit_code),
                None if running => (CommandStatus::Running, None),
                None => (CommandStatus::Finished, None),
            };
            out.push(CommandSummary {
                command_id: cid,
                session_id: sid.clone(),
                command,
                status,
                exit_code,
                started_at,
            });
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    out
}

/// Remove session subdirectories whose 8-char id is not in `live_sessions`.
pub fn clean_stale_sessions(workspace: &Path, live_sessions: &[String]) -> Result<usize> {
    let root = commands_root(workspace);
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        if live_sessions.iter().any(|s| s == &name) {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

pub type CommandsMap = Mutex<std::collections::HashMap<(String, String), CommandHandle>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ContainerRuntime, RuntimeKind};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_state(dir: &TempDir) -> AppState {
        AppState {
            projects: Arc::new(Mutex::new(HashMap::new())),
            config_dir: dir.path().to_path_buf(),
            approval_lock: Arc::new(Mutex::new(())),
            commands: Arc::new(Mutex::new(HashMap::new())),
            runtime: ContainerRuntime {
                kind: RuntimeKind::Podman,
                dry_run: false,
            },
            keep_alive_until: Arc::new(Mutex::new(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
            )),
        }
    }

    #[tokio::test]
    async fn quick_command_returns_finished_with_exit_code() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);
        let outcome = spawn_and_wait(&state, dir.path(), "abcd1234", "echo hi")
            .await
            .unwrap();
        assert_eq!(outcome.status, CommandStatus::Finished);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout_tail.contains("hi"));
        // exit file written
        let exit = std::fs::read_to_string(&outcome.exit_path).unwrap();
        assert_eq!(exit.trim(), "0");
    }

    #[tokio::test]
    async fn long_command_returns_running() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);
        let outcome = spawn_and_wait(&state, dir.path(), "deadbeef", "sleep 30")
            .await
            .unwrap();
        assert_eq!(outcome.status, CommandStatus::Running);
        assert!(outcome.exit_code.is_none());
        // stop the leftover process so the test directory can be cleaned up
        let _ = stop(&state, "deadbeef", &outcome.command_id).await;
    }

    #[test]
    fn container_paths_uses_app_mount() {
        let (out, err, exit) = container_paths("abcd1234", "efgh5678");
        assert_eq!(out, "/app/.ai-pod/commands/abcd1234/efgh5678/stdout");
        assert_eq!(err, "/app/.ai-pod/commands/abcd1234/efgh5678/stderr");
        assert_eq!(exit, "/app/.ai-pod/commands/abcd1234/efgh5678/exit");
    }

    #[test]
    fn clean_stale_sessions_removes_orphans() {
        let dir = TempDir::new().unwrap();
        let root = commands_root(dir.path());
        std::fs::create_dir_all(root.join("alive001/cmd1")).unwrap();
        std::fs::create_dir_all(root.join("dead0001/cmd2")).unwrap();
        let removed =
            clean_stale_sessions(dir.path(), &["alive001".to_string()]).unwrap();
        assert_eq!(removed, 1);
        assert!(root.join("alive001").exists());
        assert!(!root.join("dead0001").exists());
    }
}
