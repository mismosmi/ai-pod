use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

struct KillOnDrop(Option<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            if pid > 0 {
                // Kill the entire process group to catch children of sh
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
                }
            }
        }
    }
}

struct GuardedStream<S: Unpin> {
    inner: S,
    _guard: KillOnDrop,
}

impl<S: Stream + Unpin> Stream for GuardedStream<S> {
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

use super::commands;
use super::notify;
use super::AppState;

#[derive(Deserialize)]
pub struct RunCommandRequest {
    pub project_id: String,
    pub command: String,
}

#[derive(Deserialize)]
pub struct NotifyUserRequest {
    pub project_id: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct NotifyUserResponse {
    pub ok: bool,
}

#[derive(Deserialize)]
pub struct ListCommandsRequest {
    pub project_id: String,
}

#[derive(Serialize)]
pub struct ListCommandsResponse {
    pub commands: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "data")]
enum Message {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

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
) -> Result<PathBuf, (StatusCode, &'static str)> {
    let map = state.projects.lock().await;
    match map.get(project_id) {
        None => Err((StatusCode::NOT_FOUND, "Unknown project")),
        Some(info) if info.api_key != provided_key => {
            Err((StatusCode::UNAUTHORIZED, "Invalid API key"))
        }
        Some(info) => Ok(info.workspace.clone()),
    }
}

pub async fn run_command_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RunCommandRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    let workspace = match authenticate(&state, &req.project_id, &provided_key).await {
        Ok(w) => w,
        Err((status, msg)) => return (status, msg.to_string()).into_response(),
    };

    match commands::check_approval(&state, &req.command, &workspace).await {
        commands::CheckResult::Denied => {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":"Command denied by user"}"#,
            )
                .into_response();
        }
        commands::CheckResult::AlwaysAllow => {
            // Save command to project state file
            use crate::workspace::workspace_hash;
            use super::lifecycle::ProjectState;
            let hash = workspace_hash(&workspace);
            let state_file = state.config_dir.join(format!("{}.json", hash));
            let mut ps = ProjectState::load(&state_file);
            ps.add_allowed(&req.command);
            let _ = ps.save(&state_file);
        }
        commands::CheckResult::PreApproved => {}
    }

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
                format!("Failed to spawn command: {}", e),
            )
                .into_response();
        }
    };

    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let (tx, rx) = mpsc::channel::<String>(64);

    let tx_stdout = tx.clone();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let msg = serde_json::to_string(&Message::Stdout(line.clone())).unwrap()
                        + "\n";
                    let _ = tx_stdout.send(msg).await;
                }
                Err(_) => break,
            }
        }
    });

    let tx_stderr = tx.clone();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let msg = serde_json::to_string(&Message::Stderr(line.clone())).unwrap()
                        + "\n";
                    let _ = tx_stderr.send(msg).await;
                }
                Err(_) => break,
            }
        }
    });

    tokio::spawn(async move {
        let exit_code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };
        let msg = serde_json::to_string(&Message::Exit(exit_code)).unwrap() + "\n";
        let _ = tx.send(msg).await;
    });

    let stream = GuardedStream {
        inner: ReceiverStream::new(rx).map(|s| Ok::<_, std::convert::Infallible>(s)),
        _guard: KillOnDrop(pid),
    };
    axum::body::Body::from_stream(stream).into_response()
}

pub async fn notify_user_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NotifyUserRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    let workspace = match authenticate(&state, &req.project_id, &provided_key).await {
        Ok(w) => w,
        Err((status, msg)) => return (status, msg.to_string()).into_response(),
    };

    let project_name = workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    notify::send_notification(&format!("ai-pod {}", project_name), &req.message);

    Json(NotifyUserResponse { ok: true }).into_response()
}

pub async fn list_allowed_commands_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ListCommandsRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    let workspace = match authenticate(&state, &req.project_id, &provided_key).await {
        Ok(w) => w,
        Err((status, msg)) => return (status, msg.to_string()).into_response(),
    };

    let cmds = commands::get_allowed_commands(&state, &workspace);
    Json(ListCommandsResponse { commands: cmds }).into_response()
}
