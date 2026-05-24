//! Minimal MCP-over-HTTP (Streamable HTTP transport) implementation.
//!
//! Speaks just enough JSON-RPC 2.0 to expose ai-pod's host tools to Claude
//! Code and OpenCode. Each request is unary, so we never need SSE.
//!
//! Auth: standard `X-Api-Key` header (workspace lookup).
//! Session id: `X-Ai-Pod-Session-Id` header — required for tool calls so each
//! container's commands land in `.ai-pod/commands/{session_id}/`.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use super::AppState;
use super::commands;
use super::notify;
use super::runner;
use crate::runtime::ContainerRuntime;
use crate::service;
use crate::workspace::validate_service_name;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn extract_api_key(headers: &HeaderMap) -> &str {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn extract_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-ai-pod-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn tool_text(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn tool_error(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

fn tools_definition(runtime: &ContainerRuntime) -> Value {
    let run_command_description = format!(
        "Run a shell command on the host (outside this container). From inside the container, reach host services via `{}` instead of `localhost`. Waits up to 5 seconds; returns the result inline if finished, otherwise returns a command_id for polling.\n\nOutput goes to `/app/.ai-pod/commands/{{session_id}}/{{command_id}}/{{stdout,stderr,exit}}` — these files live on THIS container's filesystem (the workspace is mounted at `/app`). Read them with your regular file Read tool, not via bash on the host. Re-Read `stdout`/`exit` to poll progress; you do not need to keep calling `command_status`.\n\nDo not start commands with `cd /`. Do not pipe to `| head`/`| tail` on the host — trim output in the container instead. Keep commands as simple as possible.",
        runtime.host_gateway(),
    );
    json!([
        {
            "name": "run_command",
            "description": run_command_description,
            "inputSchema": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
            }
        },
        {
            "name": "command_status",
            "description": "Check the status of a previously started command. Returns running/finished/killed plus the last 10 lines of stdout/stderr. Full streams are at `/app/.ai-pod/commands/{session_id}/{command_id}/{stdout,stderr,exit}` on this container's filesystem — read them with your file Read tool.",
            "inputSchema": {
                "type": "object",
                "properties": { "command_id": { "type": "string" } },
                "required": ["command_id"],
            }
        },
        {
            "name": "stop_command",
            "description": "Stop a running command (SIGTERM, then SIGKILL after 5s).",
            "inputSchema": {
                "type": "object",
                "properties": { "command_id": { "type": "string" } },
                "required": ["command_id"],
            }
        },
        {
            "name": "list_commands",
            "description": "List commands for this session (or the whole workspace with scope=workspace).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["session", "workspace"] }
                }
            }
        },
        {
            "name": "notify_user",
            "description": "Send a desktop notification to the host user.",
            "inputSchema": {
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"],
            }
        },
        {
            "name": "list_allowed_commands",
            "description": "List host commands previously approved by the user for this workspace.",
            "inputSchema": { "type": "object" }
        },
        {
            "name": "start_service",
            "description": "Start an auxiliary service container (e.g. `postgres:16`) on a per-workspace bridge network. The user must approve the image + env-var KEY set once per workspace. Reach the service from this container by the `name` you pass, on the service's standard port — e.g. `name=\"postgres\"` → `postgres:5432`. No host port mapping is created. The service is ephemeral: data lives only for this ai-pod session and is discarded when the session ends. Returns `{ host, container_name }`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": { "type": "string", "description": "Container image, e.g. `postgres:16`." },
                    "name": { "type": "string", "description": "DNS alias used to reach the service from this container. Lowercase [a-z0-9-], 1-30 chars, starts with alphanumeric." },
                    "env": {
                        "type": "object",
                        "description": "Environment variables for the container. Values are passed through unchanged; only the sorted KEY list is shown to the user during approval.",
                        "additionalProperties": { "type": "string" }
                    },
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional command override for the image."
                    }
                },
                "required": ["image", "name"]
            }
        },
        {
            "name": "stop_service",
            "description": "Stop and remove a service container started by this session. No-op if the name doesn't match a service this session owns.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "list_services",
            "description": "List service containers started by this session in this workspace.",
            "inputSchema": { "type": "object" }
        },
        {
            "name": "service_logs",
            "description": "Read the tail of a service container's logs (stdout+stderr). Useful when a service fails to start.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "lines": { "type": "integer", "description": "Number of trailing lines to return (default 50)." }
                },
                "required": ["name"]
            }
        }
    ])
}

pub async fn mcp_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = match body.get("method").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => return (StatusCode::BAD_REQUEST, "Missing method").into_response(),
    };
    let params = body.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) — return empty 200.
    let is_notification = body.get("id").is_none();

    match method.as_str() {
        "initialize" => Json(rpc_result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ai-pod", "version": env!("CARGO_PKG_VERSION") }
            }),
        ))
        .into_response(),
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => Json(rpc_result(
            id,
            json!({ "tools": tools_definition(&state.runtime) }),
        ))
        .into_response(),
        "tools/call" => {
            // Auth required for tool calls.
            let api_key = extract_api_key(&headers).to_string();
            let project_id = match resolve_project_id(&state, &api_key).await {
                Some(pid) => pid,
                None => {
                    return Json(rpc_error(id, -32001, "Unauthorized")).into_response();
                }
            };
            let workspace = {
                let map = state.projects.lock().await;
                match map.get(&project_id) {
                    Some(info) => info.workspace.clone(),
                    None => {
                        return Json(rpc_error(id, -32001, "Unknown project")).into_response();
                    }
                }
            };
            let session_id = extract_session_id(&headers).unwrap_or_else(|| "host".to_string());

            let result = handle_tool_call(&state, &workspace, &session_id, &params).await;
            Json(rpc_result(id, result)).into_response()
        }
        _ => {
            if is_notification {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(rpc_error(id, -32601, &format!("Unknown method: {method}"))).into_response()
            }
        }
    }
}

async fn handle_start_service(
    state: &AppState,
    workspace: &std::path::Path,
    session_id: &str,
    args: &Value,
) -> Value {
    let image = match args.get("image").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return tool_error("Missing `image`".into()),
    };
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return tool_error("Missing `name`".into()),
    };
    if let Err(e) = validate_service_name(name) {
        return tool_error(e);
    }
    let name = name.to_string();

    let env_pairs: Vec<(String, String)> = match args.get("env") {
        Some(Value::Null) | None => Vec::new(),
        Some(Value::Object(map)) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                let val = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return tool_error(format!(
                            "env value for `{}` must be a string, number, or boolean",
                            k
                        ));
                    }
                };
                out.push((k.clone(), val));
            }
            out
        }
        _ => return tool_error("`env` must be an object".into()),
    };
    let env_keys: Vec<String> = env_pairs.iter().map(|(k, _)| k.clone()).collect();

    let command: Vec<String> = match args.get("command") {
        Some(Value::Null) | None => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return tool_error("`command` entries must be strings".into()),
                }
            }
            out
        }
        _ => return tool_error("`command` must be an array of strings".into()),
    };

    match commands::run_service_request(state, &image, &env_keys, workspace).await {
        commands::ApprovalOutcome::Denied(reason) => return tool_error(reason.message().into()),
        commands::ApprovalOutcome::Timeout => {
            return tool_error("Permission request timed out after 60 seconds.".into());
        }
        commands::ApprovalOutcome::Rejected => {
            return tool_error("Service start rejected".into());
        }
        commands::ApprovalOutcome::Approved | commands::ApprovalOutcome::AlwaysAllow => {}
    }

    let rt = state.runtime.clone();
    let workspace_owned = workspace.to_path_buf();
    let session_owned = session_id.to_string();
    let join = tokio::task::spawn_blocking(move || {
        service::start_service(
            &rt,
            &workspace_owned,
            &session_owned,
            &image,
            &name,
            &env_pairs,
            &command,
        )
    })
    .await;
    match join {
        Err(_) => tool_error("internal error spawning service".into()),
        Ok(Err(e)) => tool_error(format!("Failed to start service: {e}")),
        Ok(Ok(started)) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&started).unwrap_or_default()
            }],
            "isError": false,
            "structuredContent": started,
        }),
    }
}

async fn handle_stop_service(
    state: &AppState,
    workspace: &std::path::Path,
    session_id: &str,
    args: &Value,
) -> Value {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return tool_error("Missing `name`".into()),
    };
    if let Err(e) = validate_service_name(&name) {
        return tool_error(e);
    }
    let rt = state.runtime.clone();
    let workspace_owned = workspace.to_path_buf();
    let session_owned = session_id.to_string();
    let join = tokio::task::spawn_blocking(move || {
        service::stop_service(&rt, &workspace_owned, &session_owned, &name)
    })
    .await;
    match join {
        Err(_) => tool_error("internal error stopping service".into()),
        Ok(Err(e)) => tool_error(format!("Failed to stop service: {e}")),
        Ok(Ok(stopped)) => json!({
            "content": [{ "type": "text", "text": format!("stopped: {stopped}") }],
            "isError": false,
            "structuredContent": { "stopped": stopped },
        }),
    }
}

async fn handle_list_services(
    state: &AppState,
    workspace: &std::path::Path,
    session_id: &str,
) -> Value {
    let rt = state.runtime.clone();
    let workspace_owned = workspace.to_path_buf();
    let session_owned = session_id.to_string();
    let join = tokio::task::spawn_blocking(move || {
        service::list_services(&rt, &workspace_owned, &session_owned)
    })
    .await;
    match join {
        Err(_) => tool_error("internal error listing services".into()),
        Ok(Err(e)) => tool_error(format!("Failed to list services: {e}")),
        Ok(Ok(list)) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&list).unwrap_or_default()
            }],
            "isError": false,
            "structuredContent": { "services": list },
        }),
    }
}

async fn handle_service_logs(
    state: &AppState,
    workspace: &std::path::Path,
    session_id: &str,
    args: &Value,
) -> Value {
    let name = match args.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return tool_error("Missing `name`".into()),
    };
    if let Err(e) = validate_service_name(&name) {
        return tool_error(e);
    }
    let lines = args
        .get("lines")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(50);
    let rt = state.runtime.clone();
    let workspace_owned = workspace.to_path_buf();
    let session_owned = session_id.to_string();
    let join = tokio::task::spawn_blocking(move || {
        service::service_logs(&rt, &workspace_owned, &session_owned, &name, lines)
    })
    .await;
    match join {
        Err(_) => tool_error("internal error reading service logs".into()),
        Ok(Err(e)) => tool_error(format!("Failed to read service logs: {e}")),
        Ok(Ok(text)) => tool_text(text),
    }
}

async fn resolve_project_id(state: &AppState, api_key: &str) -> Option<String> {
    let map = state.projects.lock().await;
    for (pid, info) in map.iter() {
        if bool::from(info.api_key.as_bytes().ct_eq(api_key.as_bytes())) {
            return Some(pid.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeKind;

    fn test_runtime(kind: RuntimeKind) -> ContainerRuntime {
        ContainerRuntime {
            kind,
            dry_run: false,
        }
    }

    #[test]
    fn tools_definition_lists_all_expected_tools() {
        let v = tools_definition(&test_runtime(RuntimeKind::Podman));
        let names: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"command_status"));
        assert!(names.contains(&"stop_command"));
        assert!(names.contains(&"list_commands"));
        assert!(names.contains(&"notify_user"));
        assert!(names.contains(&"list_allowed_commands"));
    }

    #[test]
    fn run_command_description_includes_podman_host_gateway() {
        let v = tools_definition(&test_runtime(RuntimeKind::Podman));
        let desc = v[0]["description"].as_str().unwrap();
        assert!(desc.contains("host.containers.internal"));
    }

    #[test]
    fn run_command_description_includes_docker_host_gateway() {
        let v = tools_definition(&test_runtime(RuntimeKind::Docker));
        let desc = v[0]["description"].as_str().unwrap();
        assert!(desc.contains("host.docker.internal"));
    }

    #[test]
    fn run_command_description_points_at_in_container_log_path() {
        let v = tools_definition(&test_runtime(RuntimeKind::Podman));
        let desc = v[0]["description"].as_str().unwrap();
        assert!(
            desc.contains("/app/.ai-pod/commands/"),
            "description should reference the in-container log path, got: {desc}"
        );
        assert!(
            desc.contains("Read tool"),
            "description should tell the agent to use its Read tool, got: {desc}"
        );
    }

    #[test]
    fn tools_definition_includes_service_tools() {
        let v = tools_definition(&test_runtime(RuntimeKind::Podman));
        let names: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"start_service"));
        assert!(names.contains(&"stop_service"));
        assert!(names.contains(&"list_services"));
        assert!(names.contains(&"service_logs"));
    }

    #[test]
    fn start_service_description_mentions_lifetime_and_reachability() {
        let v = tools_definition(&test_runtime(RuntimeKind::Podman));
        let desc = v
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "start_service")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            desc.contains("session"),
            "start_service description should mention per-session lifetime, got: {desc}"
        );
        assert!(
            desc.contains("postgres:5432") || desc.to_lowercase().contains("standard port"),
            "start_service description should explain DNS-name reachability, got: {desc}"
        );
    }

    #[test]
    fn command_status_description_points_at_in_container_log_path() {
        let v = tools_definition(&test_runtime(RuntimeKind::Podman));
        let desc = v
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "command_status")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            desc.contains("/app/.ai-pod/commands/"),
            "description should reference the in-container log path, got: {desc}"
        );
        assert!(
            desc.contains("Read tool"),
            "description should tell the agent to use its Read tool, got: {desc}"
        );
    }
}

async fn handle_tool_call(
    state: &AppState,
    workspace: &std::path::Path,
    session_id: &str,
    params: &Value,
) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "run_command" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.is_empty() {
                return tool_error("Missing `command`".into());
            }
            match commands::run_host_command(state, cmd, workspace).await {
                commands::ApprovalOutcome::Rejected => tool_error(format!(
                    "Command rejected — matches forbidden pattern. Do not use `cd /` or `| head`/`| tail` on the host."
                )),
                commands::ApprovalOutcome::Denied(reason) => tool_error(reason.message().into()),
                commands::ApprovalOutcome::Timeout => {
                    tool_error("Permission request timed out after 60 seconds.".into())
                }
                commands::ApprovalOutcome::Approved | commands::ApprovalOutcome::AlwaysAllow => {
                    match runner::spawn_and_wait(state, workspace, session_id, cmd).await {
                        Ok(mut outcome) => {
                            let (s, e, x) = runner::container_paths(
                                &outcome.session_id,
                                &outcome.command_id,
                            );
                            outcome.stdout_path = s;
                            outcome.stderr_path = e;
                            outcome.exit_path = x;
                            json!({
                                "content": [{
                                    "type": "text",
                                    "text": serde_json::to_string_pretty(&outcome).unwrap_or_default()
                                }],
                                "isError": false,
                                "structuredContent": outcome,
                            })
                        }
                        Err(e) => tool_error(format!("Failed to run command: {e}")),
                    }
                }
            }
        }
        "command_status" => {
            let cid = args
                .get("command_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match runner::status_for(state, workspace, session_id, cid).await {
                Some(mut o) => {
                    let (s, e, x) = runner::container_paths(&o.session_id, &o.command_id);
                    o.stdout_path = s;
                    o.stderr_path = e;
                    o.exit_path = x;
                    json!({
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string_pretty(&o).unwrap_or_default()
                        }],
                        "isError": false,
                        "structuredContent": o,
                    })
                }
                None => tool_error(format!("Unknown command_id: {cid}")),
            }
        }
        "stop_command" => {
            let cid = args
                .get("command_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let stopped = runner::stop(state, session_id, cid).await;
            tool_text(format!("stopped: {stopped}"))
        }
        "list_commands" => {
            let scope = args
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("session");
            let sid = if scope == "workspace" {
                None
            } else {
                Some(session_id)
            };
            let list = runner::list(state, workspace, sid).await;
            json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&list).unwrap_or_default()
                }],
                "isError": false,
                "structuredContent": { "commands": list },
            })
        }
        "notify_user" => {
            let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let project_name = workspace
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            notify::send_notification(&format!("ai-pod {}", project_name), msg);
            tool_text("ok".into())
        }
        "list_allowed_commands" => {
            let cmds = commands::get_allowed_commands(state, workspace);
            json!({
                "content": [{
                    "type": "text",
                    "text": cmds.join("\n")
                }],
                "isError": false,
                "structuredContent": { "commands": cmds },
            })
        }
        "start_service" => handle_start_service(state, workspace, session_id, &args).await,
        "stop_service" => handle_stop_service(state, workspace, session_id, &args).await,
        "list_services" => handle_list_services(state, workspace, session_id).await,
        "service_logs" => handle_service_logs(state, workspace, session_id, &args).await,
        other => tool_error(format!("Unknown tool: {other}")),
    }
}
