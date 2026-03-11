use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::AppConfig;
use crate::workspace::workspace_hash;

pub const MCP_PORT: u16 = 7822;

/// Shared server state stored in ~/.ai-pod/server.json
#[derive(Serialize, Deserialize, Default)]
struct ServerState {
    pub pid: Option<u32>,
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
        // Atomic write via temp file
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).context("Failed to write state file")?;
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

    pub fn is_credential_ignored(&self, rel_path: &str) -> bool {
        self.ignored_credential_files.contains(&rel_path.to_string())
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

#[allow(dead_code)]
pub fn state_file_for(config: &AppConfig, workspace: &Path) -> PathBuf {
    let hash = workspace_hash(workspace);
    config.project_state_file(&hash)
}

/// Ensure the shared MCP server is running. Starts it if not alive.
pub fn ensure_shared_server(config: &AppConfig) -> Result<()> {
    let state_path = config.server_state_file();
    let state: ServerState = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if let Some(pid) = state.pid {
        if is_process_alive(pid) {
            return Ok(());
        }
    }

    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let log_path = config.config_dir.join("server.log");
    let log = std::fs::File::create(&log_path).context("Failed to create server log file")?;
    let log_err = log.try_clone()?;

    let child = Command::new(&exe)
        .args(["serve"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .spawn()
        .context("Failed to spawn shared server")?;

    let pid = child.id();
    let new_state = ServerState { pid: Some(pid) };
    let json = serde_json::to_string_pretty(&new_state)?;
    std::fs::write(&state_path, json).context("Failed to write server state")?;

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

/// Register a project with the running shared server.
pub async fn register_project(project_id: &str, api_key: &str, workspace: &Path) -> Result<()> {
    let url = format!("http://127.0.0.1:{}/register", MCP_PORT);
    let body = serde_json::json!({
        "project_id": project_id,
        "api_key": api_key,
        "workspace": workspace.to_string_lossy(),
    });

    reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to register project with shared server")?;

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
