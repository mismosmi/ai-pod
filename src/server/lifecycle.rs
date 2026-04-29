use anyhow::{Context, Result};
use colored::Colorize;

const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::AppConfig;
use crate::workspace::workspace_hash;

pub const MCP_PORT: u16 = 7822;

/// Shared server state stored in ~/.ai-pod/server.json
#[derive(Serialize, Deserialize, Default)]
struct ServerState {
    pub pid: Option<u32>,
    /// Path of the executable that was spawned for this server.
    /// Used to verify (on Linux via /proc/<pid>/exe) that the PID we
    /// loaded from disk still references our binary and has not been
    /// recycled by the kernel to an unrelated process. Optional for
    /// backwards compatibility with server state files written by
    /// prior versions.
    #[serde(default)]
    pub exe_path: Option<String>,
}

/// Per-project state stored in ~/.ai-pod/{hash}.json
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ProjectState {
    pub workspace: String,
    pub allowed_commands: Vec<String>,
    pub api_key: String,
    #[serde(default)]
    pub ignored_credential_files: Vec<String>,
}

impl ProjectState {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        // Atomic write via temp file with restrictive permissions (owner read/write only)
        let tmp = path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .context("Failed to write state file")?;
        file.write_all(json.as_bytes())
            .context("Failed to write state file contents")?;
        std::fs::rename(&tmp, path).context("Failed to rename state file")?;
        Ok(())
    }

    pub fn is_allowed(&self, cmd: &str) -> bool {
        self.allowed_commands.contains(&cmd.to_string())
    }

    pub fn add_allowed(&mut self, cmd: &str) {
        if !self.is_allowed(cmd) {
            self.allowed_commands.push(cmd.to_string());
        }
    }

    pub fn remove_allowed(&mut self, cmd: &str) {
        self.allowed_commands.retain(|c| c != cmd);
    }

    pub fn is_credential_ignored(&self, rel_path: &str) -> bool {
        self.ignored_credential_files
            .contains(&rel_path.to_string())
    }

    pub fn add_ignored_credential(&mut self, rel_path: &str) {
        if !self.is_credential_ignored(rel_path) {
            self.ignored_credential_files.push(rel_path.to_string());
        }
    }
}

fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Verify that the process at `pid` is still the same binary we spawned.
///
/// Returns true if the process is alive AND (on Linux) its `/proc/<pid>/exe`
/// symlink target matches `expected_exe`. On non-Linux platforms, or when
/// `expected_exe` is `None` (e.g. loaded from a server.json file written by
/// a prior CLI version), falls back to a plain liveness check.
///
/// This closes a PID-reuse correctness gap in `ensure_shared_server`: after
/// the shared server exits, a stale PID in `server.json` could otherwise
/// pass `kill(pid, 0)` if the kernel recycled the PID to an unrelated
/// process, causing us to skip the restart. No signals are sent here.
fn is_server_process_alive(pid: u32, expected_exe: Option<&str>) -> bool {
    if !is_process_alive(pid) {
        return false;
    }
    let expected = match expected_exe {
        Some(p) => p,
        None => return true, // backwards-compat: no identity info stored
    };

    #[cfg(target_os = "linux")]
    {
        match std::fs::read_link(format!("/proc/{}/exe", pid)) {
            Ok(target) => {
                let target_str = target.to_string_lossy();
                // When the running binary is replaced on disk (e.g. `cargo install`
                // over the same path), the kernel appends " (deleted)" to the symlink
                // target. Strip it before comparing so we don't falsely kill a still-
                // valid server process.
                let target_str = target_str.strip_suffix(" (deleted)").unwrap_or(&target_str);
                target_str == expected
            }
            Err(_) => false, // /proc entry gone → process is dead or inaccessible
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = expected;
        // macOS has no /proc. Fall back to liveness-only; this is a
        // correctness gap, not a security one, since no signal is sent.
        true
    }
}

/// Create the shared server log file with owner-only permissions (0o600).
/// Truncates any existing file, matching `File::create` semantics, so each
/// shared-server start gets a fresh log.
fn create_server_log(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[allow(dead_code)]
pub fn state_file_for(config: &AppConfig, workspace: &Path) -> PathBuf {
    let hash = workspace_hash(workspace);
    config.project_state_file(&hash)
}

/// Best-effort blocking POST to `/keep-alive` to bump the shared server's
/// inactivity timer. Errors are intentionally swallowed: the caller is
/// re-arming the timer for the next operation, and any real connectivity
/// problem will surface on the subsequent authenticated request.
pub fn bump_keep_alive() {
    let url = format!("http://127.0.0.1:{}/keep-alive", MCP_PORT);
    let _ = reqwest::blocking::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send();
}

/// Ensure the shared server is running. Starts it if not alive.
pub fn ensure_shared_server(config: &AppConfig) -> Result<()> {
    let state_path = config.server_state_file();
    let state: ServerState = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if let Some(pid) = state.pid
        && is_server_process_alive(pid, state.exe_path.as_deref())
    {
        // Re-arm the inactivity timer so a freshly-arriving CLI command does
        // not inherit a near-expired timer from the previous run.
        bump_keep_alive();
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let log_path = config.config_dir.join("server.log");
    let log = create_server_log(&log_path).context("Failed to create server log file")?;
    let log_err = log.try_clone()?;

    let child = Command::new(&exe)
        .args(["serve"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .spawn()
        .context("Failed to spawn shared server")?;

    let pid = child.id();
    let new_state = ServerState {
        pid: Some(pid),
        exe_path: Some(exe.to_string_lossy().to_string()),
    };
    let json = serde_json::to_string_pretty(&new_state)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&state_path)
        .context("Failed to write server state")?;
    file.write_all(json.as_bytes())
        .context("Failed to write server state contents")?;

    // Wait briefly for server to start
    std::thread::sleep(std::time::Duration::from_millis(500));

    println!(
        "{} (PID {}, port {})",
        "Shared server started.".green(),
        pid,
        MCP_PORT,
    );

    Ok(())
}

/// Load or create per-project state (generates api_key on first use).
pub fn get_or_create_project_state(config: &AppConfig, workspace: &Path) -> Result<ProjectState> {
    let hash = workspace_hash(workspace);
    let state_path = config.project_state_file(&hash);
    let mut state = ProjectState::load(&state_path);

    let changed = if state.api_key.is_empty() {
        state.api_key = uuid::Uuid::new_v4().to_string().replace('-', "");
        true
    } else {
        false
    };

    let workspace_str = workspace.to_string_lossy().to_string();
    let changed = changed || state.workspace != workspace_str;
    state.workspace = workspace_str;

    if changed {
        state.save(&state_path)?;
    }

    Ok(state)
}

/// Tell the running shared server to rescan config files.
pub async fn reload_config() -> Result<()> {
    let url = format!("http://127.0.0.1:{}/reload", MCP_PORT);
    reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .context("Failed to reload server config")?;
    Ok(())
}

fn is_newer_version(server: &str, cli: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let mut parts = v.splitn(3, '.');
        Some((
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        ))
    };
    match (parse(cli), parse(server)) {
        (Some(c), Some(s)) => c > s,
        _ => false,
    }
}

/// Check that the running server version matches the CLI. Returns Err if CLI is newer.
pub async fn check_server_version() -> Result<()> {
    let url = format!("http://127.0.0.1:{}/version", MCP_PORT);
    let resp: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("Failed to reach server /version")?
        .json()
        .await
        .context("Invalid JSON from server /version")?;

    let server_version = resp["version"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing version field in server response"))?;

    if is_newer_version(server_version, CLI_VERSION) {
        println!(
            "{} Server is v{}, CLI is v{}. Finish active ai-pod sessions so a new server can start.",
            "Version mismatch:".yellow().bold(),
            server_version,
            CLI_VERSION,
        );
        anyhow::bail!("Server version mismatch");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_config(dir: &TempDir) -> AppConfig {
        let home = dir.path().to_path_buf();
        let config_dir = home.join(".ai-pod");
        std::fs::create_dir_all(&config_dir).unwrap();
        AppConfig {
            runtime_settings: config_dir.join("runtime-settings.json"),
            runtime_claude_md: config_dir.join("runtime-CLAUDE.md"),
            config_dir,
            home_dir: home,
        }
    }

    #[test]
    fn project_state_default_has_no_api_key() {
        let state = ProjectState::default();
        assert!(state.api_key.is_empty());
        assert!(state.allowed_commands.is_empty());
    }

    #[test]
    fn project_state_save_sets_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");
        let state = ProjectState {
            workspace: "/home/user/project".into(),
            allowed_commands: vec![],
            api_key: "secret".into(),
            ignored_credential_files: vec![],
        };
        state.save(&path).unwrap();
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "state file must be owner read/write only (0600)"
        );
    }

    #[test]
    fn server_log_file_has_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("server.log");
        let _file = create_server_log(&path).unwrap();
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "server log must be owner read/write only (0600)"
        );
    }

    #[test]
    fn project_state_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.json");
        let state = ProjectState {
            workspace: "/home/user/project".into(),
            allowed_commands: vec!["make build".into()],
            api_key: "deadbeef1234567890abcdef12345678".into(),
            ignored_credential_files: vec![],
        };
        state.save(&path).unwrap();
        let loaded = ProjectState::load(&path);
        assert_eq!(loaded.workspace, "/home/user/project");
        assert_eq!(loaded.allowed_commands, vec!["make build"]);
        assert_eq!(loaded.api_key, "deadbeef1234567890abcdef12345678");
    }

    #[test]
    fn project_state_load_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");
        let state = ProjectState::load(&path);
        assert!(state.api_key.is_empty());
    }

    #[test]
    fn is_allowed_checks_exact_match() {
        let mut state = ProjectState::default();
        state.add_allowed("make build");
        assert!(state.is_allowed("make build"));
        assert!(!state.is_allowed("make test"));
    }

    #[test]
    fn add_allowed_is_idempotent() {
        let mut state = ProjectState::default();
        state.add_allowed("npm test");
        state.add_allowed("npm test");
        assert_eq!(state.allowed_commands.len(), 1);
    }

    #[test]
    fn state_file_is_under_config_dir() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let path = state_file_for(&config, Path::new("/home/user/myproject"));
        assert!(path.starts_with(&config.config_dir));
        assert!(path.extension().unwrap() == "json");
    }

    #[test]
    fn get_or_create_generates_api_key() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let workspace = Path::new("/home/user/myproject");
        let state = get_or_create_project_state(&config, workspace).unwrap();
        assert!(!state.api_key.is_empty());
        assert_eq!(state.api_key.len(), 32);
    }

    #[test]
    fn get_or_create_is_stable() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        let workspace = Path::new("/home/user/myproject");
        let state1 = get_or_create_project_state(&config, workspace).unwrap();
        let state2 = get_or_create_project_state(&config, workspace).unwrap();
        assert_eq!(state1.api_key, state2.api_key);
    }
}
