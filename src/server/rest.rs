use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use subtle::ConstantTimeEq;

use super::AppState;
use super::commands;
use super::notify;

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
    let workspace = {
        let map = state.projects.lock().await;
        match map.get(project_id) {
            None => return Err((StatusCode::NOT_FOUND, "Unknown project")),
            Some(info) => {
                if !bool::from(info.api_key.as_bytes().ct_eq(provided_key.as_bytes())) {
                    return Err((StatusCode::UNAUTHORIZED, "Invalid API key"));
                }
                info.workspace.clone()
            }
        }
    };
    *state.keep_alive_until.lock().await =
        std::time::Instant::now() + std::time::Duration::from_secs(30);
    Ok(workspace)
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

    match commands::run_host_command(&state, &req.command, &workspace).await {
        commands::ApprovalOutcome::Rejected => {
            let pattern = commands::COMMAND_REJECT_RE.as_str();
            let body = serde_json::json!({
                "error": format!("Command rejected — it matches the forbidden pattern: {pattern}. Do not use `cd /` or `| head` / `| tail` in daemon commands."),
            });
            return (StatusCode::BAD_REQUEST, body.to_string()).into_response();
        }
        commands::ApprovalOutcome::Denied => {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"error":"Command denied by user"}"#,
            )
                .into_response();
        }
        commands::ApprovalOutcome::Timeout => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                r#"{"error":"Permission request timed out after 60 seconds. Stop your current work and ask the user if they would like to try again."}"#,
            )
                .into_response();
        }
        commands::ApprovalOutcome::Approved | commands::ApprovalOutcome::AlwaysAllow => {}
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

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let (tx, rx) = mpsc::channel::<String>(64);
    let (stdout_done_tx, stdout_done_rx) = oneshot::channel::<()>();
    let (stderr_done_tx, stderr_done_rx) = oneshot::channel::<()>();

    let tx_stdout = tx.clone();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let msg = serde_json::to_string(&Message::Stdout(line.clone())).unwrap() + "\n";
                    let _ = tx_stdout.send(msg).await;
                }
                Err(_) => break,
            }
        }
        let _ = stdout_done_tx.send(());
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
                    let msg = serde_json::to_string(&Message::Stderr(line.clone())).unwrap() + "\n";
                    let _ = tx_stderr.send(msg).await;
                }
                Err(_) => break,
            }
        }
        let _ = stderr_done_tx.send(());
    });

    tokio::spawn(async move {
        // Own `child` throughout. The kernel cannot recycle the PID
        // until `Child::wait()` returns, so `child.id()` in the
        // client-disconnect branch is guaranteed to still reference our
        // process group. This eliminates the PID-reuse TOCTOU that
        // motivated issue #29.
        let exit_code = tokio::select! {
            res = child.wait() => {
                match res {
                    Ok(status) => status.code().unwrap_or(-1),
                    Err(_) => -1,
                }
            }
            // Resolves when the axum response body (and hence the mpsc
            // Receiver held by ReceiverStream) is dropped — client
            // disconnect or early cancel. `closed()` is cancel-safe.
            _ = tx.closed() => {
                if let Some(pid) = child.id()
                    && pid > 0
                {
                    // Kill the whole process group (see
                    // `.process_group(0)` on spawn) so children of
                    // `sh` are caught too.
                    let ret = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
                    debug_assert_eq!(ret, 0, "kill({}, SIGTERM) failed: {}", pid, std::io::Error::last_os_error());
                    // Give the process group a grace period, then escalate to SIGKILL
                    // to handle processes that ignore SIGTERM.
                    tokio::select! {
                        res = child.wait() => match res {
                            Ok(status) => status.code().unwrap_or(-1),
                            Err(_) => -1,
                        },
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                            let _ = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
                            match child.wait().await {
                                Ok(status) => status.code().unwrap_or(-1),
                                Err(_) => -1,
                            }
                        }
                    }
                } else {
                    match child.wait().await {
                        Ok(status) => status.code().unwrap_or(-1),
                        Err(_) => -1,
                    }
                }
            }
        };
        // Wait for both reader tasks to finish before sending Exit,
        // ensuring Exit is always the last message in the channel.
        let _ = stdout_done_rx.await;
        let _ = stderr_done_rx.await;
        let msg = serde_json::to_string(&Message::Exit(exit_code)).unwrap() + "\n";
        // On the client-disconnect path, tx.send will Err and we move on.
        let _ = tx.send(msg).await;
    });

    let stream = ReceiverStream::new(rx).map(|s| Ok::<_, std::convert::Infallible>(s));
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
