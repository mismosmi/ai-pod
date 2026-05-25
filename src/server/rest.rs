use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use subtle::ConstantTimeEq;

use super::AppState;
use super::commands;
use super::notify;
use super::runner;

#[derive(Deserialize)]
pub struct RunCommandRequest {
    pub project_id: String,
    pub command: String,
    /// Required for MCP-style runs that produce per-session output dirs.
    /// Host-side `ai-pod commands run` may omit this; the workspace project_id
    /// is used as a fallback session id ("host" namespace).
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
pub struct StopCommandRequest {
    pub project_id: String,
    pub session_id: String,
    pub command_id: String,
}

#[derive(Serialize)]
pub struct StopCommandResponse {
    pub stopped: bool,
}

#[derive(Deserialize)]
pub struct CommandStatusRequest {
    pub project_id: String,
    pub session_id: String,
    pub command_id: String,
}

#[derive(Deserialize)]
pub struct ListCommandsRequest2 {
    pub project_id: String,
    /// `None` → list all sessions for the workspace.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Serialize)]
pub struct ListCommandsResponse2 {
    pub commands: Vec<runner::CommandSummary>,
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
pub struct ListAllowedCommandsRequest {
    pub project_id: String,
}

#[derive(Serialize)]
pub struct ListAllowedCommandsResponse {
    pub commands: Vec<String>,
}

#[derive(Deserialize)]
pub struct AgentStatusRequest {
    pub project_id: String,
    pub session_id: String,
    pub status: String,
    #[serde(default)]
    pub status_line: Option<String>,
}

#[derive(Serialize)]
pub struct AgentStatusResponse {
    pub ok: bool,
}

fn extract_api_key(headers: &HeaderMap) -> &str {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

pub(crate) async fn authenticate(
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

    let session_id = req
        .session_id
        .clone()
        .or_else(|| {
            headers
                .get("x-ai-pod-session-id")
                .and_then(|v| v.to_str().ok().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| "host".to_string());

    match commands::run_host_command(&state, &req.command, &workspace).await {
        commands::ApprovalOutcome::Rejected => {
            let pattern = commands::COMMAND_REJECT_RE.as_str();
            let body = serde_json::json!({
                "error": format!("Command rejected — it matches the forbidden pattern: {pattern}. Do not use `cd /` or `| head` / `| tail`."),
            });
            return (StatusCode::BAD_REQUEST, body.to_string()).into_response();
        }
        commands::ApprovalOutcome::Denied(reason) => {
            let body = serde_json::json!({
                "error": reason.message(),
                "reason": reason.slug(),
            });
            return (StatusCode::BAD_REQUEST, body.to_string()).into_response();
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

    match runner::spawn_and_wait(&state, &workspace, &session_id, &req.command).await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to run command: {e}"),
        )
            .into_response(),
    }
}

pub async fn stop_command_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StopCommandRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();
    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }
    let stopped = runner::stop(&state, &req.session_id, &req.command_id).await;
    Json(StopCommandResponse { stopped }).into_response()
}

pub async fn command_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CommandStatusRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();
    let workspace = match authenticate(&state, &req.project_id, &provided_key).await {
        Ok(w) => w,
        Err((status, msg)) => return (status, msg.to_string()).into_response(),
    };
    match runner::status_for(&state, &workspace, &req.session_id, &req.command_id).await {
        Some(o) => Json(o).into_response(),
        None => (StatusCode::NOT_FOUND, "Unknown command").into_response(),
    }
}

pub async fn list_commands_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ListCommandsRequest2>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();
    let workspace = match authenticate(&state, &req.project_id, &provided_key).await {
        Ok(w) => w,
        Err((status, msg)) => return (status, msg.to_string()).into_response(),
    };
    let cmds = runner::list(&state, &workspace, req.session_id.as_deref()).await;
    Json(ListCommandsResponse2 { commands: cmds }).into_response()
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

pub async fn agent_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AgentStatusRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();
    if let Err((status, msg)) = authenticate(&state, &req.project_id, &provided_key).await {
        return (status, msg.to_string()).into_response();
    }

    // Whitelist the allowed status values.
    let normalised = match req.status.as_str() {
        "Running" | "Idle" | "AwaitingInput" | "Finished" => req.status.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "status must be one of Running, Idle, AwaitingInput, Finished",
            )
                .into_response();
        }
    };

    // Reject session ids that aren't 8 hex chars to keep the filename safe.
    if req.session_id.len() != 8 || !req.session_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return (StatusCode::BAD_REQUEST, "Invalid session_id").into_response();
    }

    let agents_dir = {
        let home = match dirs::data_local_dir().or_else(dirs::data_dir) {
            Some(p) => p,
            None => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "no data dir").into_response();
            }
        };
        home.join("ai-pod").join("agents")
    };
    if let Err(e) = std::fs::create_dir_all(&agents_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create agents_dir: {e}"),
        )
            .into_response();
    }

    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let payload = serde_json::json!({
        "session_id": req.session_id,
        "project_id": req.project_id,
        "status": normalised,
        "status_line": req.status_line.clone().unwrap_or_default(),
        "updated_at": updated_at,
    });

    let final_path = agents_dir.join(format!("{}.json", req.session_id));
    let tmp_path = agents_dir.join(format!(".{}.json.tmp", req.session_id));
    if let Err(e) = std::fs::write(&tmp_path, payload.to_string()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write status: {e}"),
        )
            .into_response();
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("commit status: {e}"),
        )
            .into_response();
    }

    Json(AgentStatusResponse { ok: true }).into_response()
}

pub async fn list_allowed_commands_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ListAllowedCommandsRequest>,
) -> impl IntoResponse {
    let provided_key = extract_api_key(&headers).to_string();

    let workspace = match authenticate(&state, &req.project_id, &provided_key).await {
        Ok(w) => w,
        Err((status, msg)) => return (status, msg.to_string()).into_response(),
    };

    let cmds = commands::get_allowed_commands(&state, &workspace);
    Json(ListAllowedCommandsResponse { commands: cmds }).into_response()
}
