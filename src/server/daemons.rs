use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::AppState;

// ──── Types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStatus {
    Running,
    Finished { exit_code: i32 },
    Killed,
}

pub struct DaemonEntry {
    pub id: String,
    pub project_id: String,
    pub command: String,
    pub started_at: std::time::SystemTime,
    pub status: DaemonStatus,
    pub pid: Option<u32>,
    pub log_path: PathBuf,
}

#[derive(Serialize, Clone)]
pub struct DaemonMeta {
    pub id: String,
    pub project_id: String,
    pub command: String,
    pub started_at: u64,
    pub status: DaemonStatus,
    pub log_path: String,
}

impl DaemonMeta {
    fn from_entry(entry: &DaemonEntry) -> Self {
        let started_at = entry
            .started_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id: entry.id.clone(),
            project_id: entry.project_id.clone(),
            command: entry.command.clone(),
            started_at,
            status: entry.status.clone(),
            log_path: entry.log_path.to_string_lossy().to_string(),
        }
    }
}

// Log message format (matches rest.rs Message)
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
enum Message {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

// ──── Request / Response types ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StartDaemonRequest {
    pub project_id: String,
    pub command: String,
}

#[derive(Serialize)]
pub struct StartDaemonResponse {
    pub daemon_id: String,
}

#[derive(Deserialize)]
pub struct StopDaemonRequest {
    pub project_id: String,
    pub daemon_id: String,
}

#[derive(Deserialize)]
pub struct StopAllDaemonsRequest {
    pub project_id: String,
}

#[derive(Serialize)]
pub struct StopAllDaemonsResponse {
    pub stopped: usize,
}

#[derive(Deserialize)]
pub struct ListDaemonsRequest {
    pub project_id: String,
}

#[derive(Serialize)]
pub struct ListDaemonsResponse {
    pub daemons: Vec<DaemonMeta>,
}

#[derive(Deserialize)]
pub struct DaemonStatusRequest {
    pub project_id: String,
    pub daemon_id: String,
}

#[derive(Serialize)]
pub struct DaemonStatusResponse {
    pub daemon: DaemonMeta,
}

#[derive(Deserialize)]
pub struct DaemonOutputRequest {
    pub project_id: String,
    pub daemon_id: String,
}

// ──── Private helpers ────────────────────────────────────────────────────────

fn extract_api_key(headers: &HeaderMap) -> &str {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

async fn authenticate(
    state: &AppState,
    project_id: &str,
    provided_key: &str,
) -> Result<(), (StatusCode, &'static str)> {
    let map = state.projects.lock().await;
    match map.get(project_id) {
        None => Err((StatusCode::NOT_FOUND, "Unknown project")),
        Some(info) if info.api_key != provided_key => {
            Err((StatusCode::UNAUTHORIZED, "Invalid API key"))
        }
        Some(_) => Ok(()),
    }
}


async fn gc_old_daemon_logs(state: AppState) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(7 * 24 * 3600))
        .unwrap_or(std::time::UNIX_EPOCH);

    let mut to_remove: Vec<(String, PathBuf)> = Vec::new();
    {
        let daemons = state.daemons.lock().await;
        for (id, entry) in daemons.iter() {
            if entry.status != DaemonStatus::Running && entry.started_at < cutoff {
                to_remove.push((id.clone(), entry.log_path.clone()));
            }
        }
    }

    for (id, log_path) in &to_remove {
        let _ = tokio::fs::remove_file(log_path).await;
        let mut daemons = state.daemons.lock().await;
        daemons.remove(id);
    }
}

async fn stop_all_daemons_for_project(state: &AppState, project_id: &str) -> usize {
    let mut pids: Vec<u32> = Vec::new();
    let mut count = 0;
    {
        let mut daemons = state.daemons.lock().await;
        for entry in daemons.values_mut() {
            if entry.project_id == project_id && entry.status == DaemonStatus::Running {
                entry.status = DaemonStatus::Killed;
                count += 1;
                if let Some(pid) = entry.pid {
                    pids.push(pid);
                }
            }
        }
    }
    for pid in pids {
        if pid > 0 {
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
        }
    }
    count
}

// ──── Background orphan cleanup ──────────────────────────────────────────────

pub async fn cleanup_orphaned_daemons(state: &AppState) {
    // Collect unique project_ids that have at least one running daemon
    let project_ids: Vec<String> = {
        let daemons = state.daemons.lock().await;
        let mut seen = std::collections::HashSet::new();
        daemons
            .values()
            .filter(|d| d.status == DaemonStatus::Running)
            .filter_map(|d| {
                if seen.insert(d.project_id.clone()) {
                    Some(d.project_id.clone())
                } else {
                    None
                }
            })
            .collect()
    };

    for project_id in project_ids {
        // Container prefix is "claude-{project_id}" (project_id == workspace_hash)
        let filter = format!("name=^claude-{}-", project_id);
        let output = match tokio::process::Command::new("podman")
            .args(["ps", "--filter", &filter, "--filter", "label=managed-by=ai-pod", "--format", "{{.Names}}"])
            .output()
            .await
        {
            Ok(o) => o,
            Err(_) => continue,
        };

        let has_containers = String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|l| !l.is_empty());

        if !has_containers {
            stop_all_daemons_for_project(state, &project_id).await;
        }
    }
}

// ──── Handlers ───────────────────────────────────────────────────────────────

pub async fn start_daemon_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StartDaemonRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    // Look up workspace for this project (guaranteed to exist after authentication)
    let workspace = {
        let projects = state.projects.lock().await;
        projects
            .get(&req.project_id)
            .map(|info| info.workspace.clone())
            .unwrap_or_default()
    };

    match super::commands::run_host_command(&state, &req.command, &workspace).await {
        super::commands::ApprovalOutcome::PipeRejected => {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":"Command must not end with | head or | tail"}"#,
            )
                .into_response();
        }
        super::commands::ApprovalOutcome::Denied => {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":"Command denied by user"}"#,
            )
                .into_response();
        }
        super::commands::ApprovalOutcome::Timeout => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                r#"{"error":"Permission request timed out after 60 seconds."}"#,
            )
                .into_response();
        }
        super::commands::ApprovalOutcome::Approved | super::commands::ApprovalOutcome::AlwaysAllow => {}
    }

    // Generate daemon_id: first 12 chars of UUID v4 without dashes
    let raw = uuid::Uuid::new_v4().to_string().replace('-', "");
    let daemon_id = raw[..12].to_string();

    // Compute log path (project_id is already the workspace hash)
    let log_dir = state
        .config_dir
        .join("daemon-logs")
        .join(&req.project_id);
    let log_path = log_dir.join(format!("{}.log", daemon_id));

    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create log dir: {}", e),
        )
            .into_response();
    }

    // Spawn background GC (best-effort)
    let gc_state = state.clone();
    tokio::spawn(async move { gc_old_daemon_logs(gc_state).await });

    // Spawn the process
    let mut child = match tokio::process::Command::new("sh")
        .args(["-c", &req.command])
        .current_dir(&workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to spawn daemon: {}", e),
            )
                .into_response();
        }
    };

    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Insert entry before spawning reaper
    {
        let mut daemons = state.daemons.lock().await;
        daemons.insert(
            daemon_id.clone(),
            DaemonEntry {
                id: daemon_id.clone(),
                project_id: req.project_id.clone(),
                command: req.command.clone(),
                started_at: std::time::SystemTime::now(),
                status: DaemonStatus::Running,
                pid,
                log_path: log_path.clone(),
            },
        );
    }

    // Spawn reaper task: pumps stdout/stderr to log, writes Exit, updates status
    let daemon_id_clone = daemon_id.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        let log_file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => f,
            Err(_) => return,
        };

        let (log_tx, mut log_rx) = mpsc::channel::<String>(64);
        let (stdout_done_tx, stdout_done_rx) = tokio::sync::oneshot::channel::<()>();
        let (stderr_done_tx, stderr_done_rx) = tokio::sync::oneshot::channel::<()>();

        // Pump stdout → channel
        let tx1 = log_tx.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let msg =
                            serde_json::to_string(&Message::Stdout(line.clone())).unwrap() + "\n";
                        let _ = tx1.send(msg).await;
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });

        // Pump stderr → channel
        let tx2 = log_tx.clone();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let msg =
                            serde_json::to_string(&Message::Stderr(line.clone())).unwrap() + "\n";
                        let _ = tx2.send(msg).await;
                    }
                }
            }
            let _ = stderr_done_tx.send(());
        });

        // Writer task: drains channel into log file
        let write_handle = tokio::spawn(async move {
            let mut writer = tokio::io::BufWriter::new(log_file);
            while let Some(msg) = log_rx.recv().await {
                let _ = writer.write_all(msg.as_bytes()).await;
            }
            let _ = writer.flush().await;
            writer
        });

        // Wait for child to exit
        let exit_code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };

        // Wait for both I/O pumps to finish
        let _ = stdout_done_rx.await;
        let _ = stderr_done_rx.await;

        // Close the log channel so the writer task drains and exits
        drop(log_tx);

        // Wait for writer task to finish, then write Exit message
        if let Ok(mut writer) = write_handle.await {
            let exit_msg = serde_json::to_string(&Message::Exit(exit_code)).unwrap() + "\n";
            let _ = writer.write_all(exit_msg.as_bytes()).await;
            let _ = writer.flush().await;
        }

        // Update daemon status
        let mut daemons = state_clone.daemons.lock().await;
        if let Some(entry) = daemons.get_mut(&daemon_id_clone) {
            if entry.status == DaemonStatus::Running {
                entry.status = DaemonStatus::Finished { exit_code };
            }
        }
    });

    Json(StartDaemonResponse { daemon_id }).into_response()
}

pub async fn stop_daemon_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StopDaemonRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    let pid = {
        let mut daemons = state.daemons.lock().await;
        match daemons.get_mut(&req.daemon_id) {
            None => return (StatusCode::NOT_FOUND, "Unknown daemon").into_response(),
            Some(entry) if entry.project_id != req.project_id => {
                return (StatusCode::NOT_FOUND, "Unknown daemon").into_response();
            }
            Some(entry) if entry.status != DaemonStatus::Running => {
                return Json(serde_json::json!({"ok": true})).into_response();
            }
            Some(entry) => {
                entry.status = DaemonStatus::Killed;
                entry.pid
            }
        }
    };

    if let Some(pid) = pid {
        if pid > 0 {
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
            }
        }
    }

    Json(serde_json::json!({"ok": true})).into_response()
}

pub async fn stop_all_daemons_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StopAllDaemonsRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    let stopped = stop_all_daemons_for_project(&state, &req.project_id).await;

    Json(StopAllDaemonsResponse { stopped }).into_response()
}

pub async fn list_daemons_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ListDaemonsRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    let daemons_list: Vec<DaemonMeta> = {
        let daemons = state.daemons.lock().await;
        daemons
            .values()
            .filter(|e| e.project_id == req.project_id)
            .map(DaemonMeta::from_entry)
            .collect()
    };

    Json(ListDaemonsResponse {
        daemons: daemons_list,
    })
    .into_response()
}

pub async fn daemon_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DaemonStatusRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    let meta = {
        let daemons = state.daemons.lock().await;
        match daemons.get(&req.daemon_id) {
            None => return (StatusCode::NOT_FOUND, "Unknown daemon").into_response(),
            Some(entry) if entry.project_id != req.project_id => {
                return (StatusCode::NOT_FOUND, "Unknown daemon").into_response();
            }
            Some(entry) => DaemonMeta::from_entry(entry),
        }
    };

    Json(DaemonStatusResponse { daemon: meta }).into_response()
}

pub async fn daemon_output_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DaemonOutputRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    let log_path = {
        let daemons = state.daemons.lock().await;
        match daemons.get(&req.daemon_id) {
            None => return (StatusCode::NOT_FOUND, "Unknown daemon").into_response(),
            Some(entry) if entry.project_id != req.project_id => {
                return (StatusCode::NOT_FOUND, "Unknown daemon").into_response();
            }
            Some(entry) => entry.log_path.clone(),
        }
    };

    let (tx, rx) = mpsc::channel::<String>(64);
    let daemon_id = req.daemon_id.clone();

    tokio::spawn(async move {
        let file = match tokio::fs::File::open(&log_path).await {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut reader = tokio::io::BufReader::new(file);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF – check if daemon is still running
                    let status = {
                        let daemons = state.daemons.lock().await;
                        daemons
                            .get(&daemon_id)
                            .map(|e| e.status.clone())
                            .unwrap_or(DaemonStatus::Killed)
                    };
                    match status {
                        DaemonStatus::Running => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        _ => break, // Daemon finished; reaper wrote Exit message to log already
                    }
                }
                Ok(_) => {
                    if tx.send(line.clone()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(|s| Ok::<_, std::convert::Infallible>(s));
    axum::body::Body::from_stream(stream).into_response()
}

// ──── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::commands::ends_with_pipe_to_head_or_tail;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    // ── Shared helpers ────────────────────────────────────────────────────────

    fn make_entry(id: &str, project_id: &str, status: DaemonStatus, log_path: PathBuf) -> DaemonEntry {
        DaemonEntry {
            id: id.to_string(),
            project_id: project_id.to_string(),
            command: "test-command".to_string(),
            started_at: std::time::SystemTime::now(),
            status,
            pid: None,
            log_path,
        }
    }

    /// Build an AppState with "proj1" (key "key1") and "proj2" (key "key2") registered.
    fn make_state(config_dir: &std::path::Path) -> AppState {
        let mut projects = HashMap::new();
        projects.insert(
            "proj1".to_string(),
            crate::server::ProjectInfo {
                workspace: config_dir.to_path_buf(),
                api_key: "key1".to_string(),
            },
        );
        projects.insert(
            "proj2".to_string(),
            crate::server::ProjectInfo {
                workspace: config_dir.to_path_buf(),
                api_key: "key2".to_string(),
            },
        );
        AppState {
            projects: Arc::new(Mutex::new(projects)),
            config_dir: config_dir.to_path_buf(),
            approval_lock: Arc::new(Mutex::new(())),
            daemons: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // ── ends_with_pipe_to_head_or_tail ────────────────────────────────────────

    #[test]
    fn pipe_to_head_is_rejected() {
        assert!(ends_with_pipe_to_head_or_tail("ls | head"));
        assert!(ends_with_pipe_to_head_or_tail("ls | head -n 10"));
        assert!(ends_with_pipe_to_head_or_tail("ls | tail"));
        assert!(ends_with_pipe_to_head_or_tail("ls | tail -5"));
    }

    #[test]
    fn normal_commands_not_rejected() {
        assert!(!ends_with_pipe_to_head_or_tail("ls"));
        assert!(!ends_with_pipe_to_head_or_tail("cat file | grep foo"));
        assert!(!ends_with_pipe_to_head_or_tail("echo hello"));
    }

    #[test]
    fn pipe_head_with_extra_whitespace() {
        assert!(ends_with_pipe_to_head_or_tail("ls |  head"));
        assert!(ends_with_pipe_to_head_or_tail("ls |  tail -n 5"));
    }

    #[test]
    fn pipe_to_head_in_middle_of_pipeline_is_allowed() {
        assert!(!ends_with_pipe_to_head_or_tail("cat file | head | cat"));
        assert!(!ends_with_pipe_to_head_or_tail("ls | head | wc -l"));
    }

    #[test]
    fn empty_command_not_rejected() {
        assert!(!ends_with_pipe_to_head_or_tail(""));
    }

    #[test]
    fn command_with_word_starting_with_head_or_tail_not_rejected() {
        assert!(!ends_with_pipe_to_head_or_tail("ls | headroom"));
        assert!(!ends_with_pipe_to_head_or_tail("ls | tailored"));
        assert!(!ends_with_pipe_to_head_or_tail("ls | heading"));
    }

    #[test]
    fn trailing_whitespace_after_head_is_rejected() {
        assert!(ends_with_pipe_to_head_or_tail("ls | head   "));
    }

    // ── DaemonMeta::from_entry ────────────────────────────────────────────────

    #[test]
    fn daemon_meta_from_entry_sets_started_at() {
        let entry = DaemonEntry {
            id: "abc123".to_string(),
            project_id: "proj1".to_string(),
            command: "sleep 10".to_string(),
            started_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000),
            status: DaemonStatus::Running,
            pid: Some(1234),
            log_path: PathBuf::from("/tmp/test.log"),
        };
        let meta = DaemonMeta::from_entry(&entry);
        assert_eq!(meta.started_at, 1000);
        assert_eq!(meta.id, "abc123");
    }

    #[test]
    fn daemon_meta_preserves_running_status() {
        let entry = make_entry("d1", "p1", DaemonStatus::Running, PathBuf::from("/tmp/d1.log"));
        let meta = DaemonMeta::from_entry(&entry);
        assert_eq!(meta.status, DaemonStatus::Running);
    }

    #[test]
    fn daemon_meta_preserves_finished_status() {
        let entry = make_entry(
            "d1",
            "p1",
            DaemonStatus::Finished { exit_code: 42 },
            PathBuf::from("/tmp/d1.log"),
        );
        let meta = DaemonMeta::from_entry(&entry);
        assert_eq!(meta.status, DaemonStatus::Finished { exit_code: 42 });
    }

    #[test]
    fn daemon_meta_preserves_killed_status() {
        let entry = make_entry("d1", "p1", DaemonStatus::Killed, PathBuf::from("/tmp/d1.log"));
        let meta = DaemonMeta::from_entry(&entry);
        assert_eq!(meta.status, DaemonStatus::Killed);
    }

    #[test]
    fn daemon_meta_log_path_string_conversion() {
        let path = PathBuf::from("/tmp/daemon logs/my daemon.log");
        let entry = make_entry("d1", "p1", DaemonStatus::Running, path.clone());
        let meta = DaemonMeta::from_entry(&entry);
        assert_eq!(meta.log_path, path.to_string_lossy());
    }

    #[test]
    fn daemon_meta_epoch_zero_maps_to_zero() {
        let entry = DaemonEntry {
            id: "d1".to_string(),
            project_id: "p1".to_string(),
            command: "cmd".to_string(),
            started_at: std::time::UNIX_EPOCH,
            status: DaemonStatus::Running,
            pid: None,
            log_path: PathBuf::from("/tmp/d1.log"),
        };
        let meta = DaemonMeta::from_entry(&entry);
        assert_eq!(meta.started_at, 0);
    }

    // ── DaemonStatus serde ────────────────────────────────────────────────────

    #[test]
    fn daemon_status_serde_running_is_snake_case() {
        let s = serde_json::to_string(&DaemonStatus::Running).unwrap();
        assert_eq!(s, "\"running\"");
    }

    #[test]
    fn daemon_status_serde_killed_roundtrip() {
        let json = serde_json::to_string(&DaemonStatus::Killed).unwrap();
        let back: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, DaemonStatus::Killed);
    }

    #[test]
    fn daemon_status_serde_finished_roundtrip() {
        let orig = DaemonStatus::Finished { exit_code: 7 };
        let json = serde_json::to_string(&orig).unwrap();
        let back: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, orig);
    }

    #[test]
    fn daemon_status_deserialise_running_from_str() {
        let s: DaemonStatus = serde_json::from_str("\"running\"").unwrap();
        assert_eq!(s, DaemonStatus::Running);
    }

    // ── gc_old_daemon_logs ────────────────────────────────────────────────────

    #[tokio::test]
    async fn gc_removes_old_finished_daemon() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("abc123.log");
        std::fs::write(&log_path, b"").unwrap();

        let state = make_state(dir.path());
        state.daemons.lock().await.insert(
            "abc123".to_string(),
            DaemonEntry {
                id: "abc123".to_string(),
                project_id: "proj1".to_string(),
                command: "true".to_string(),
                started_at: std::time::UNIX_EPOCH, // ancient
                status: DaemonStatus::Finished { exit_code: 0 },
                pid: None,
                log_path: log_path.clone(),
            },
        );

        gc_old_daemon_logs(state.clone()).await;

        assert!(state.daemons.lock().await.is_empty(), "daemon should be removed");
        assert!(!log_path.exists(), "log file should be deleted");
    }

    #[tokio::test]
    async fn gc_removes_old_killed_daemon() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("killed1.log");
        std::fs::write(&log_path, b"").unwrap();

        let state = make_state(dir.path());
        state.daemons.lock().await.insert(
            "killed1".to_string(),
            DaemonEntry {
                id: "killed1".to_string(),
                project_id: "proj1".to_string(),
                command: "sleep 9999".to_string(),
                started_at: std::time::UNIX_EPOCH,
                status: DaemonStatus::Killed,
                pid: None,
                log_path: log_path.clone(),
            },
        );

        gc_old_daemon_logs(state.clone()).await;

        assert!(state.daemons.lock().await.is_empty());
        assert!(!log_path.exists());
    }

    #[tokio::test]
    async fn gc_leaves_running_daemon_alone() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("running1.log");
        std::fs::write(&log_path, b"").unwrap();

        let state = make_state(dir.path());
        state.daemons.lock().await.insert(
            "running1".to_string(),
            make_entry("running1", "proj1", DaemonStatus::Running, log_path.clone()),
        );

        gc_old_daemon_logs(state.clone()).await;

        assert_eq!(state.daemons.lock().await.len(), 1, "running daemon must not be removed");
        assert!(log_path.exists());
    }

    #[tokio::test]
    async fn gc_leaves_recent_finished_daemon() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("recent1.log");
        std::fs::write(&log_path, b"").unwrap();

        let state = make_state(dir.path());
        state.daemons.lock().await.insert(
            "recent1".to_string(),
            DaemonEntry {
                id: "recent1".to_string(),
                project_id: "proj1".to_string(),
                command: "echo hi".to_string(),
                started_at: std::time::SystemTime::now(), // just started
                status: DaemonStatus::Finished { exit_code: 0 },
                pid: None,
                log_path: log_path.clone(),
            },
        );

        gc_old_daemon_logs(state.clone()).await;

        assert_eq!(state.daemons.lock().await.len(), 1, "recent daemon must not be removed");
    }

    // ── HTTP integration tests ────────────────────────────────────────────────

    mod http {
        use super::*;
        use axum::{Router, body::Body, http::Request, routing::post};
        use tower::ServiceExt;

        fn make_router(state: AppState) -> Router {
            Router::new()
                .route("/daemon/start", post(start_daemon_handler))
                .route("/daemon/stop", post(stop_daemon_handler))
                .route("/daemon/stop-all", post(stop_all_daemons_handler))
                .route("/daemon/list", post(list_daemons_handler))
                .route("/daemon/status", post(daemon_status_handler))
                .route("/daemon/output", post(daemon_output_handler))
                .with_state(state)
        }

        fn json_req(uri: &str, api_key: &str, body: serde_json::Value) -> Request<Body> {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("x-api-key", api_key)
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        }

        async fn to_json(resp: axum::response::Response) -> serde_json::Value {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}))
        }

        async fn to_text(resp: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            String::from_utf8_lossy(&bytes).to_string()
        }

        // Poll the in-memory daemons map until the daemon is no longer Running.
        async fn wait_until_done(state: &AppState, daemon_id: &str) {
            for _ in 0..200 {
                let is_running = {
                    let daemons = state.daemons.lock().await;
                    daemons
                        .get(daemon_id)
                        .map(|e| e.status == DaemonStatus::Running)
                        .unwrap_or(false)
                };
                if !is_running {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!("daemon '{}' did not finish within 5s", daemon_id);
        }

        /// Pre-approve a command in the project state file so tests bypass the
        /// interactive approval prompt.
        fn pre_allow(config_dir: &std::path::Path, workspace: &std::path::Path, cmd: &str) {
            use crate::server::lifecycle::ProjectState;
            use crate::workspace::workspace_hash;
            let hash = workspace_hash(workspace);
            let state_path = config_dir.join(format!("{}.json", hash));
            let mut state = ProjectState::load(&state_path);
            state.add_allowed(cmd);
            state.save(&state_path).unwrap();
        }

        // ── Authentication ────────────────────────────────────────────────────

        #[tokio::test]
        async fn auth_unknown_project_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/list",
                    "any-key",
                    serde_json::json!({"project_id": "does-not-exist"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn auth_wrong_api_key_returns_401() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/list",
                    "wrong-key",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
        }

        // ── POST /daemon/start ────────────────────────────────────────────────

        #[tokio::test]
        async fn start_daemon_returns_12_char_id() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "echo hello");
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "echo hello"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = to_json(resp).await;
            let id = body["daemon_id"].as_str().unwrap();
            assert_eq!(id.len(), 12, "daemon_id must be 12 characters");
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "daemon_id must be alphanumeric");
        }

        #[tokio::test]
        async fn start_daemon_is_inserted_into_state() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "echo hello");
            let router = make_router(state.clone());

            let resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "echo hello"}),
                ))
                .await
                .unwrap();

            let body = to_json(resp).await;
            let id = body["daemon_id"].as_str().unwrap().to_string();

            let daemons = state.daemons.lock().await;
            assert!(daemons.contains_key(&id), "daemon must be in state map");
            assert_eq!(daemons[&id].project_id, "proj1");
        }

        #[tokio::test]
        async fn start_daemon_creates_log_dir() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "true");
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "true"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let log_dir = dir.path().join("daemon-logs").join("proj1");
            assert!(log_dir.exists(), "daemon log directory must be created");
        }

        #[tokio::test]
        async fn start_two_daemons_produce_different_ids() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "echo 1");
            pre_allow(dir.path(), dir.path(), "echo 2");

            let r1 = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "echo 1"}),
                ))
                .await
                .unwrap();
            let r2 = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "echo 2"}),
                ))
                .await
                .unwrap();

            let id1 = to_json(r1).await["daemon_id"].as_str().unwrap().to_string();
            let id2 = to_json(r2).await["daemon_id"].as_str().unwrap().to_string();
            assert_ne!(id1, id2, "each start must produce a unique daemon_id");
        }

        #[tokio::test]
        async fn start_daemon_rejects_pipe_to_head() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "ls | head -n 5"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
            let text = to_text(resp).await;
            assert!(text.contains("must not end with"), "error should mention the restriction");
        }

        #[tokio::test]
        async fn start_daemon_rejects_pipe_to_tail() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "cat log | tail -20"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn start_daemon_runs_in_project_workspace() {
            let config_dir = TempDir::new().unwrap();
            let workspace1 = TempDir::new().unwrap();
            let workspace2 = TempDir::new().unwrap();

            // Canonicalize to resolve any symlinks (e.g. /var -> /private/var on macOS)
            let ws1 = workspace1.path().canonicalize().unwrap();
            let ws2 = workspace2.path().canonicalize().unwrap();

            let mut projects = std::collections::HashMap::new();
            projects.insert(
                "proj1".to_string(),
                crate::server::ProjectInfo {
                    workspace: ws1.clone(),
                    api_key: "key1".to_string(),
                },
            );
            projects.insert(
                "proj2".to_string(),
                crate::server::ProjectInfo {
                    workspace: ws2.clone(),
                    api_key: "key2".to_string(),
                },
            );
            let state = AppState {
                projects: Arc::new(Mutex::new(projects)),
                config_dir: config_dir.path().to_path_buf(),
                approval_lock: Arc::new(Mutex::new(())),
                daemons: Arc::new(Mutex::new(std::collections::HashMap::new())),
            };

            pre_allow(config_dir.path(), &ws1, "pwd");
            pre_allow(config_dir.path(), &ws2, "pwd");

            // Start `pwd` in proj1
            let resp1 = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "pwd"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp1.status(), axum::http::StatusCode::OK);
            let id1 = to_json(resp1).await["daemon_id"].as_str().unwrap().to_string();

            // Start `pwd` in proj2
            let resp2 = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/start",
                    "key2",
                    serde_json::json!({"project_id": "proj2", "command": "pwd"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp2.status(), axum::http::StatusCode::OK);
            let id2 = to_json(resp2).await["daemon_id"].as_str().unwrap().to_string();

            wait_until_done(&state, &id1).await;
            wait_until_done(&state, &id2).await;

            // Read output for proj1
            let out1 = to_text(
                make_router(state.clone())
                    .oneshot(json_req(
                        "/daemon/output",
                        "key1",
                        serde_json::json!({"project_id": "proj1", "daemon_id": id1}),
                    ))
                    .await
                    .unwrap(),
            )
            .await;

            // Read output for proj2
            let out2 = to_text(
                make_router(state.clone())
                    .oneshot(json_req(
                        "/daemon/output",
                        "key2",
                        serde_json::json!({"project_id": "proj2", "daemon_id": id2}),
                    ))
                    .await
                    .unwrap(),
            )
            .await;

            #[derive(Deserialize)]
            #[serde(tag = "type", content = "data")]
            enum Msg { Stdout(String), Stderr(String), Exit(i32) }

            let stdout1: String = out1
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str::<Msg>(l).ok())
                .filter_map(|m| if let Msg::Stdout(s) = m { Some(s) } else { None })
                .collect::<Vec<_>>()
                .join("");

            let stdout2: String = out2
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str::<Msg>(l).ok())
                .filter_map(|m| if let Msg::Stdout(s) = m { Some(s) } else { None })
                .collect::<Vec<_>>()
                .join("");

            assert!(
                stdout1.trim() == ws1.to_string_lossy(),
                "proj1 daemon must run in its workspace: expected {:?}, got {:?}",
                ws1,
                stdout1.trim()
            );
            assert!(
                stdout2.trim() == ws2.to_string_lossy(),
                "proj2 daemon must run in its workspace: expected {:?}, got {:?}",
                ws2,
                stdout2.trim()
            );
        }

        // ── POST /daemon/stop ─────────────────────────────────────────────────

        #[tokio::test]
        async fn stop_unknown_daemon_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/stop",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": "nonexistent"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn stop_daemon_wrong_project_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                make_entry("d1", "proj1", DaemonStatus::Running, dir.path().join("d1.log")),
            );
            let router = make_router(state);

            // proj2's key is correct, but the daemon belongs to proj1
            let resp = router
                .oneshot(json_req(
                    "/daemon/stop",
                    "key2",
                    serde_json::json!({"project_id": "proj2", "daemon_id": "d1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn stop_already_finished_daemon_returns_ok() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                make_entry(
                    "d1",
                    "proj1",
                    DaemonStatus::Finished { exit_code: 0 },
                    dir.path().join("d1.log"),
                ),
            );
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/stop",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": "d1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = to_json(resp).await;
            assert_eq!(body["ok"], true);
        }

        #[tokio::test]
        async fn stop_running_daemon_sets_status_killed() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "sleep 60");
            let router = make_router(state.clone());

            // Start a long-running process
            let start_resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "sleep 60"}),
                ))
                .await
                .unwrap();
            let daemon_id = to_json(start_resp).await["daemon_id"].as_str().unwrap().to_string();

            // Stop it
            let stop_resp = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/stop",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": daemon_id}),
                ))
                .await
                .unwrap();

            assert_eq!(stop_resp.status(), axum::http::StatusCode::OK);

            // Status must be Killed in the map (set synchronously before SIGTERM)
            let daemons = state.daemons.lock().await;
            assert_eq!(daemons[&daemon_id].status, DaemonStatus::Killed);
        }

        // ── POST /daemon/stop-all ─────────────────────────────────────────────

        #[tokio::test]
        async fn stop_all_with_no_daemons_returns_zero() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/stop-all",
                    "key1",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = to_json(resp).await;
            assert_eq!(body["stopped"], 0);
        }

        #[tokio::test]
        async fn stop_all_returns_correct_count() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            {
                let mut daemons = state.daemons.lock().await;
                for i in 0..3 {
                    let id = format!("d{}", i);
                    daemons.insert(
                        id.clone(),
                        make_entry(&id, "proj1", DaemonStatus::Running, dir.path().join(format!("{}.log", id))),
                    );
                }
                // One already-finished daemon — should not be counted
                daemons.insert(
                    "done1".to_string(),
                    make_entry(
                        "done1",
                        "proj1",
                        DaemonStatus::Finished { exit_code: 0 },
                        dir.path().join("done1.log"),
                    ),
                );
            }
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/stop-all",
                    "key1",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            let body = to_json(resp).await;
            assert_eq!(body["stopped"], 3, "only Running daemons should be counted");
        }

        #[tokio::test]
        async fn stop_all_only_affects_requesting_project() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            {
                let mut daemons = state.daemons.lock().await;
                daemons.insert(
                    "p1d1".to_string(),
                    make_entry("p1d1", "proj1", DaemonStatus::Running, dir.path().join("p1d1.log")),
                );
                daemons.insert(
                    "p2d1".to_string(),
                    make_entry("p2d1", "proj2", DaemonStatus::Running, dir.path().join("p2d1.log")),
                );
            }
            let router = make_router(state.clone());

            let resp = router
                .oneshot(json_req(
                    "/daemon/stop-all",
                    "key1",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            let body = to_json(resp).await;
            assert_eq!(body["stopped"], 1);

            let daemons = state.daemons.lock().await;
            assert_eq!(daemons["p1d1"].status, DaemonStatus::Killed);
            assert_eq!(daemons["p2d1"].status, DaemonStatus::Running, "other project untouched");
        }

        // ── POST /daemon/list ─────────────────────────────────────────────────

        #[tokio::test]
        async fn list_returns_empty_array_for_no_daemons() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/list",
                    "key1",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = to_json(resp).await;
            assert_eq!(body["daemons"], serde_json::json!([]));
        }

        #[tokio::test]
        async fn list_filters_by_project() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            {
                let mut daemons = state.daemons.lock().await;
                daemons.insert(
                    "a".to_string(),
                    make_entry("a", "proj1", DaemonStatus::Running, dir.path().join("a.log")),
                );
                daemons.insert(
                    "b".to_string(),
                    make_entry("b", "proj2", DaemonStatus::Running, dir.path().join("b.log")),
                );
            }
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/list",
                    "key1",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            let body = to_json(resp).await;
            let daemons = body["daemons"].as_array().unwrap();
            assert_eq!(daemons.len(), 1);
            assert_eq!(daemons[0]["id"], "a");
            assert_eq!(daemons[0]["project_id"], "proj1");
        }

        #[tokio::test]
        async fn list_response_has_required_fields() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                make_entry("d1", "proj1", DaemonStatus::Running, dir.path().join("d1.log")),
            );
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/list",
                    "key1",
                    serde_json::json!({"project_id": "proj1"}),
                ))
                .await
                .unwrap();

            let body = to_json(resp).await;
            let d = &body["daemons"][0];
            assert!(d["id"].is_string(), "id must be present");
            assert!(d["project_id"].is_string(), "project_id must be present");
            assert!(d["command"].is_string(), "command must be present");
            assert!(d["started_at"].is_number(), "started_at must be a number");
            assert!(!d["status"].is_null(), "status must be present");
            assert!(d["log_path"].is_string(), "log_path must be present");
        }

        // ── POST /daemon/status ───────────────────────────────────────────────

        #[tokio::test]
        async fn daemon_status_unknown_id_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/status",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": "nope"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn daemon_status_wrong_project_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                make_entry("d1", "proj1", DaemonStatus::Running, dir.path().join("d1.log")),
            );
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/status",
                    "key2",
                    serde_json::json!({"project_id": "proj2", "daemon_id": "d1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn daemon_status_returns_daemon_meta() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                DaemonEntry {
                    id: "d1".to_string(),
                    project_id: "proj1".to_string(),
                    command: "my-command".to_string(),
                    started_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(5000),
                    status: DaemonStatus::Finished { exit_code: 2 },
                    pid: None,
                    log_path: dir.path().join("d1.log"),
                },
            );
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/status",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": "d1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body = to_json(resp).await;
            assert_eq!(body["daemon"]["id"], "d1");
            assert_eq!(body["daemon"]["command"], "my-command");
            assert_eq!(body["daemon"]["started_at"], 5000);
            // Finished with exit_code 2
            let status = &body["daemon"]["status"];
            let exit_code = status["finished"]["exit_code"]
                .as_i64()
                .expect("status should be finished with exit_code");
            assert_eq!(exit_code, 2);
        }

        // ── POST /daemon/output ───────────────────────────────────────────────

        #[tokio::test]
        async fn output_unknown_daemon_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/output",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": "nope"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn output_wrong_project_returns_404() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                make_entry("d1", "proj1", DaemonStatus::Running, dir.path().join("d1.log")),
            );
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/output",
                    "key2",
                    serde_json::json!({"project_id": "proj2", "daemon_id": "d1"}),
                ))
                .await
                .unwrap();

            assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn output_missing_log_file_returns_empty_stream() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            state.daemons.lock().await.insert(
                "d1".to_string(),
                make_entry(
                    "d1",
                    "proj1",
                    DaemonStatus::Finished { exit_code: 0 },
                    dir.path().join("nonexistent.log"), // file does not exist
                ),
            );
            let router = make_router(state);

            let resp = router
                .oneshot(json_req(
                    "/daemon/output",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": "d1"}),
                ))
                .await
                .unwrap();

            // Must return 200 (not 500); body will be empty since the file open fails silently
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let text = to_text(resp).await;
            assert!(text.is_empty(), "body should be empty when log file is missing");
        }

        #[tokio::test]
        async fn output_streams_stdout_and_exit_for_finished_daemon() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "printf 'line1\\nline2\\n'");
            let router = make_router(state.clone());

            // Start daemon
            let start_resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({
                        "project_id": "proj1",
                        "command": "printf 'line1\\nline2\\n'"
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(start_resp.status(), axum::http::StatusCode::OK);
            let daemon_id = to_json(start_resp).await["daemon_id"].as_str().unwrap().to_string();

            // Wait for the reaper to mark it finished
            wait_until_done(&state, &daemon_id).await;

            // Stream output
            let out_resp = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/output",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": daemon_id}),
                ))
                .await
                .unwrap();

            assert_eq!(out_resp.status(), axum::http::StatusCode::OK);
            let text = to_text(out_resp).await;

            // Parse line-delimited JSON messages
            #[derive(Deserialize)]
            #[serde(tag = "type", content = "data")]
            enum Msg { Stdout(String), Stderr(String), Exit(i32) }

            let messages: Vec<Msg> = text
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str::<Msg>(l).expect("each line must be valid Message JSON"))
                .collect();

            // Must have at least two Stdout messages and a final Exit(0)
            let stdout_content: Vec<&str> = messages
                .iter()
                .filter_map(|m| if let Msg::Stdout(s) = m { Some(s.as_str()) } else { None })
                .collect();
            assert!(
                stdout_content.iter().any(|s| s.contains("line1")),
                "output must contain 'line1'"
            );
            assert!(
                stdout_content.iter().any(|s| s.contains("line2")),
                "output must contain 'line2'"
            );

            let last = messages.last().expect("must have at least one message");
            assert!(matches!(last, Msg::Exit(0)), "last message must be Exit(0)");
        }

        #[tokio::test]
        async fn output_exit_code_reflects_command_failure() {
            let dir = TempDir::new().unwrap();
            let state = make_state(dir.path());
            pre_allow(dir.path(), dir.path(), "exit 3");
            let router = make_router(state.clone());

            let start_resp = router
                .oneshot(json_req(
                    "/daemon/start",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "command": "exit 3"}),
                ))
                .await
                .unwrap();
            let daemon_id = to_json(start_resp).await["daemon_id"].as_str().unwrap().to_string();

            wait_until_done(&state, &daemon_id).await;

            let out_resp = make_router(state.clone())
                .oneshot(json_req(
                    "/daemon/output",
                    "key1",
                    serde_json::json!({"project_id": "proj1", "daemon_id": daemon_id}),
                ))
                .await
                .unwrap();

            let text = to_text(out_resp).await;

            #[derive(Deserialize)]
            #[serde(tag = "type", content = "data")]
            enum Msg { Stdout(String), Stderr(String), Exit(i32) }

            let messages: Vec<Msg> = text
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str::<Msg>(l).unwrap())
                .collect();

            let last = messages.last().expect("must have Exit message");
            assert!(matches!(last, Msg::Exit(3)), "exit code must be 3");
        }
    }
}
