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
    #[serde(default)]
    pub masked_directories: Vec<String>,
    /// Canonical "image with env [KEYS]" strings the user has approved for
    /// `start_service` requests. See `commands::service_approval_key`.
    #[serde(default)]
    pub allowed_services: Vec<String>,
    /// Image names the user has approved for `rebuild_image` requests. See
    /// `commands::rebuild_approval_key`.
    #[serde(default)]
    pub allowed_rebuilds: Vec<String>,
    /// Egress filtering for this workspace (`ai-pod egress`). `None` means
    /// unfiltered network access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<crate::egress::EgressConfig>,
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

    pub fn remove_ignored_credential(&mut self, rel_path: &str) {
        self.ignored_credential_files.retain(|p| p != rel_path);
    }

    pub fn is_masked(&self, dir: &str) -> bool {
        self.masked_directories.iter().any(|d| d == dir)
    }

    pub fn add_masked(&mut self, dir: &str) {
        if !self.is_masked(dir) {
            self.masked_directories.push(dir.to_string());
        }
    }

    pub fn remove_masked(&mut self, dir: &str) {
        self.masked_directories.retain(|d| d != dir);
    }

    pub fn is_service_allowed(&self, key: &str) -> bool {
        self.allowed_services.iter().any(|k| k == key)
    }

    pub fn add_allowed_service(&mut self, key: &str) {
        if !self.is_service_allowed(key) {
            self.allowed_services.push(key.to_string());
        }
    }

    pub fn is_rebuild_allowed(&self, key: &str) -> bool {
        self.allowed_rebuilds.iter().any(|k| k == key)
    }

    pub fn add_allowed_rebuild(&mut self, key: &str) {
        if !self.is_rebuild_allowed(key) {
            self.allowed_rebuilds.push(key.to_string());
        }
    }
}

fn is_process_alive(pid: u32) -> bool {
    if pid <= 1 || unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    // An exited child may remain a zombie until its parent reaps it.
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with("Z "))
        {
            return false;
        }
    }
    true
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

/// Best-effort POST to `/keep-alive` to bump the shared server's inactivity
/// timer. Errors are intentionally swallowed: the caller is re-arming the
/// timer for the next operation, and any real connectivity problem will
/// surface on the subsequent authenticated request.
pub async fn bump_keep_alive() {
    let url = format!("http://127.0.0.1:{}/keep-alive", MCP_PORT);
    let _ = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await;
}

/// Whether the server recorded at `pid` answers `/version`.
///
/// A live PID alone is not enough: it may be a zombie (undetectable on macOS),
/// a recycled PID, or a server that is still starting up. Polls briefly so a
/// concurrently starting server gets a chance to bind the port.
async fn server_responds(pid: u32) -> Result<bool> {
    let client = control_client()?;
    let base = format!("http://127.0.0.1:{MCP_PORT}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if fetch_version(&client, &base).await.is_ok() {
            return Ok(true);
        }
        if !is_process_alive(pid) || tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Ensure the shared server is running. Starts it if not alive.
pub async fn ensure_shared_server(config: &AppConfig) -> Result<()> {
    let state_path = config.server_state_file();
    let state: ServerState = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if let Some(pid) = state.pid
        && is_server_process_alive(pid, state.exe_path.as_deref())
        && server_responds(pid).await?
    {
        // Re-arm the inactivity timer so a freshly-arriving CLI command does
        // not inherit a near-expired timer from the previous run.
        bump_keep_alive().await;
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to get current executable path")?;
    let log_path = config.config_dir.join("server.log");
    let log = create_server_log(&log_path).context("Failed to create server log file")?;
    let log_err = log.try_clone()?;

    let mut child = Command::new(&exe)
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

    // Wait for readiness, including after replacing an old server.
    let client = control_client()?;
    let base = format!("http://127.0.0.1:{MCP_PORT}");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "Shared server exited with {status}; see {}",
                log_path.display()
            );
        }
        if fetch_version(&client, &base)
            .await
            .is_ok_and(|v| v == CLI_VERSION)
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Shared server did not become ready; see {}",
                log_path.display()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Reap the server when it exits. Without this, a long-lived CLI (e.g.
    // `ai-pod run ... acp`) keeps the exited server as a zombie, and its PID
    // passes `kill(pid, 0)` on platforms without the /proc zombie check.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    eprintln!(
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

fn control_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(15))
        .build()?)
}

async fn fetch_version(client: &reqwest::Client, base: &str) -> Result<String> {
    let resp: serde_json::Value = client
        .get(format!("{base}/version"))
        .send()
        .await
        .context("Failed to reach server /version")?
        .error_for_status()?
        .json()
        .await
        .context("Invalid JSON from server /version")?;
    resp["version"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Missing version field in server response"))
}

/// Negotiate an explicit restart; never stop a server without confirmation.
/// Returns the PID to wait for, or None when no upgrade is needed.
async fn request_restart(
    config: &AppConfig,
    client: &reqwest::Client,
    base: &str,
    confirm: impl FnOnce(&super::control::Status) -> Result<bool> + Send + 'static,
) -> Result<Option<u32>> {
    let version = fetch_version(client, base).await?;
    if !is_newer_version(&version, CLI_VERSION) {
        return Ok(None);
    }
    eprintln!(
        "{} Server is v{}, CLI is v{}.",
        "Version mismatch:".yellow().bold(),
        version,
        CLI_VERSION
    );
    let credentials = super::control::Credentials::load(&config.config_dir).context(
        "This server predates interactive restarts. Finish all active ai-pod sessions and allow the old server to exit, then retry.")?;
    let response = client
        .get(format!("{base}/server/status"))
        .bearer_auth(&credentials.token)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!(
            "This server predates interactive restarts. Finish all active ai-pod sessions and allow the old server to exit, then retry."
        );
    }
    if !response.status().is_success() {
        anyhow::bail!("Could not list active sessions: {}", response.text().await?);
    }
    let status: super::control::Status = response.json().await?;
    if status.pid <= 1 || status.pid != credentials.pid || status.version != version {
        anyhow::bail!("Server changed while checking its version; retry the command");
    }

    // Keep an idle server alive while the user reads and answers the prompt.
    let keep_alive_client = client.clone();
    let keep_alive_url = format!("{base}/keep-alive");
    let keep_alive = tokio::spawn(async move {
        loop {
            let _ = keep_alive_client.post(&keep_alive_url).send().await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });
    let pid = status.pid;
    let accepted = tokio::task::spawn_blocking(move || confirm(&status)).await;
    keep_alive.abort();
    if !accepted?? {
        anyhow::bail!("Server restart declined; existing sessions are still running");
    }
    client
        .post(format!("{base}/server/quit"))
        .bearer_auth(&credentials.token)
        .send()
        .await?
        .error_for_status()
        .context("Server refused to quit")?;
    Ok(Some(pid))
}

/// Check compatibility and offer to replace an older shared server.
pub async fn check_server_version(config: &AppConfig) -> Result<()> {
    let client = control_client()?;
    let base = format!("http://127.0.0.1:{MCP_PORT}");
    let pid = request_restart(config, &client, &base, |status| {
        if status.sessions.is_empty() {
            eprintln!("No active ai-pod sessions.");
        } else {
            eprintln!("Active ai-pod sessions:");
            for session in &status.sessions {
                eprintln!("  {}  {}  ({})", session.session_id, session.container, session.runtime);
            }
        }
        eprintln!("Restarting leaves containers running, briefly interrupts server access, and loses tracking of running host commands.");
        if !crate::is_stdin_tty() {
            anyhow::bail!("Server restart requires confirmation. Run ai-pod in an interactive terminal, or finish active sessions and retry after the server exits.");
        }
        Ok(dialoguer::Confirm::new()
            .with_prompt("Restart the server anyway?")
            .default(false)
            .interact()?)
    }).await?;
    let Some(pid) = pid else { return Ok(()) };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while is_process_alive(pid) {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("Old server did not exit within 15 seconds; no replacement was started");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    ensure_shared_server(config).await?;
    let version = fetch_version(&client, &base).await?;
    if version != CLI_VERSION {
        anyhow::bail!("Replacement server is v{version}, expected v{CLI_VERSION}");
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
            config_dir,
            home_dir: home,
        }
    }

    async fn mock_server(
        version: &str,
        status_code: axum::http::StatusCode,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let version = version.to_string();
        let status_version = version.clone();
        let quits = Arc::new(AtomicUsize::new(0));
        let count = quits.clone();
        let app = Router::new()
            .route(
                "/version",
                get(move || async move { Json(serde_json::json!({"version": version})) }),
            )
            .route("/keep-alive", post(|| async { "ok" }))
            .route(
                "/server/status",
                get(move || async move {
                    (
                        status_code,
                        Json(super::super::control::Status {
                            pid: 12345,
                            version: status_version,
                            sessions: vec![super::super::control::Session {
                                runtime: "docker".into(),
                                container: "ai-pod-abcdef123456-1234abcd".into(),
                                session_id: "1234abcd".into(),
                            }],
                        }),
                    )
                }),
            )
            .route(
                "/server/quit",
                post(move |headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer secret");
                    count.fetch_add(1, Ordering::SeqCst);
                    "quitting"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, quits, task)
    }

    #[tokio::test]
    async fn old_server_quits_only_after_session_list_is_confirmed() {
        use std::sync::atomic::Ordering;
        for accepted in [false, true] {
            let dir = TempDir::new().unwrap();
            let config = temp_config(&dir);
            super::super::control::Credentials {
                pid: 12345,
                token: "secret".into(),
            }
            .save(&config.config_dir)
            .unwrap();
            let (base, quits, task) = mock_server("0.0.1", axum::http::StatusCode::OK).await;
            let observed_quits = quits.clone();
            let result =
                request_restart(&config, &control_client().unwrap(), &base, move |status| {
                    assert_eq!(status.sessions.len(), 1);
                    assert_eq!(status.sessions[0].session_id, "1234abcd");
                    assert_eq!(observed_quits.load(Ordering::SeqCst), 0);
                    Ok(accepted)
                })
                .await;
            if accepted {
                assert_eq!(result.unwrap(), Some(12345));
            } else {
                assert!(result.unwrap_err().to_string().contains("declined"));
            }
            assert_eq!(quits.load(Ordering::SeqCst), usize::from(accepted));
            task.abort();
        }
    }

    #[tokio::test]
    async fn compatible_server_does_not_prompt_or_quit() {
        for version in [CLI_VERSION, "999.0.0"] {
            let dir = TempDir::new().unwrap();
            let config = temp_config(&dir);
            let (base, quits, task) = mock_server(version, axum::http::StatusCode::OK).await;
            let result = request_restart(&config, &control_client().unwrap(), &base, |_| {
                panic!("unexpected prompt")
            })
            .await
            .unwrap();
            assert_eq!(result, None);
            assert_eq!(quits.load(std::sync::atomic::Ordering::SeqCst), 0);
            task.abort();
        }
    }

    #[tokio::test]
    async fn unavailable_session_list_never_prompts_or_quits() {
        for status_code in [
            axum::http::StatusCode::NOT_FOUND,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let dir = TempDir::new().unwrap();
            let config = temp_config(&dir);
            super::super::control::Credentials {
                pid: 12345,
                token: "secret".into(),
            }
            .save(&config.config_dir)
            .unwrap();
            let (base, quits, task) = mock_server("0.0.1", status_code).await;
            assert!(
                request_restart(&config, &control_client().unwrap(), &base, |_| panic!(
                    "unexpected prompt"
                ))
                .await
                .is_err()
            );
            assert_eq!(quits.load(std::sync::atomic::Ordering::SeqCst), 0);
            task.abort();
        }
    }

    #[tokio::test]
    async fn failed_confirmation_never_quits() {
        let dir = TempDir::new().unwrap();
        let config = temp_config(&dir);
        super::super::control::Credentials {
            pid: 12345,
            token: "secret".into(),
        }
        .save(&config.config_dir)
        .unwrap();
        let (base, quits, task) = mock_server("0.0.1", axum::http::StatusCode::OK).await;
        assert!(
            request_restart(
                &config,
                &control_client().unwrap(),
                &base,
                |_| anyhow::bail!("noninteractive input")
            )
            .await
            .is_err()
        );
        assert_eq!(quits.load(std::sync::atomic::Ordering::SeqCst), 0);
        task.abort();
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
            masked_directories: vec![],
            allowed_services: vec![],
            allowed_rebuilds: vec![],
            egress: None,
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
            masked_directories: vec![],
            allowed_services: vec![],
            allowed_rebuilds: vec![],
            egress: None,
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
    fn service_helpers_round_trip() {
        let mut state = ProjectState::default();
        assert!(!state.is_service_allowed("postgres:16"));
        state.add_allowed_service("postgres:16");
        state.add_allowed_service("postgres:16");
        state.add_allowed_service("redis:7");
        assert!(state.is_service_allowed("postgres:16"));
        assert!(state.is_service_allowed("redis:7"));
        assert_eq!(state.allowed_services.len(), 2);
    }

    #[test]
    fn allowed_services_default_loads_when_field_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.json");
        // Simulate a state file written by an older version (no allowed_services).
        std::fs::write(
            &path,
            r#"{
              "workspace": "/home/user/proj",
              "allowed_commands": [],
              "api_key": "abc",
              "ignored_credential_files": [],
              "masked_directories": []
            }"#,
        )
        .unwrap();
        let loaded = ProjectState::load(&path);
        assert_eq!(loaded.workspace, "/home/user/proj");
        assert!(loaded.allowed_services.is_empty());
    }

    #[test]
    fn mask_helpers_round_trip() {
        let mut state = ProjectState::default();
        assert!(!state.is_masked("node_modules"));
        state.add_masked("node_modules");
        state.add_masked("node_modules");
        state.add_masked("target");
        assert!(state.is_masked("node_modules"));
        assert!(state.is_masked("target"));
        assert_eq!(state.masked_directories.len(), 2);
        state.remove_masked("node_modules");
        assert!(!state.is_masked("node_modules"));
        assert!(state.is_masked("target"));
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
