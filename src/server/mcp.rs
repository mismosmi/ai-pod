use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, sse::{Event, Sse}},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::AppState;
use super::commands::{list_allowed_commands, run_host_command};
use super::notify::send_notification;

#[derive(Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl JsonRpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<Value>, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(json!({ "code": code, "message": message })),
        }
    }
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "send_notification",
                "description": "Send a desktop notification to the host user.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Notification title" },
                        "message": { "type": "string", "description": "Notification body" }
                    },
                    "required": ["title", "message"]
                }
            },
            {
                "name": "run_host_command",
                "description": "Run a shell command on the host machine. Requires user approval for new commands.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to run" }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "list_allowed_commands",
                "description": "List commands that have been permanently allowed for this project.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
}

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
}

pub async fn mcp_handler(
    Path(project_id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    let provided_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let workspace = {
        let map = state.projects.lock().await;
        match map.get(&project_id) {
            None => {
                return (StatusCode::NOT_FOUND, "Unknown project").into_response();
            }
            Some(info) if info.api_key != provided_key => {
                return (StatusCode::UNAUTHORIZED, "Invalid API key").into_response();
            }
            Some(info) => info.workspace.clone(),
        }
    };

    let id = req.id.clone();
    let use_sse = wants_sse(&headers);

    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "ai-pod", "version": env!("CARGO_PKG_VERSION") }
            });
            let resp = JsonRpcResponse::ok(id, result);
            if use_sse {
                single_sse_event(resp)
            } else {
                Json(resp).into_response()
            }
        }

        "notifications/initialized" => axum::http::StatusCode::NO_CONTENT.into_response(),

        "tools/list" => {
            let resp = JsonRpcResponse::ok(id, tools_list());
            if use_sse {
                single_sse_event(resp)
            } else {
                Json(resp).into_response()
            }
        }

        "tools/call" => {
            let params = req.params.unwrap_or_default();
            let tool_name = params["name"].as_str().unwrap_or("").to_string();
            let arguments = params["arguments"].clone();

            if use_sse {
                stream_tool_call(state, id, tool_name, arguments, workspace)
            } else {
                let result = dispatch_tool(&state, &tool_name, &arguments, &workspace).await;
                let resp = JsonRpcResponse::ok(id, result);
                Json(resp).into_response()
            }
        }

        _ => {
            let resp = JsonRpcResponse::err(id, -32601, "Method not found");
            Json(resp).into_response()
        }
    }
}

async fn dispatch_tool(state: &AppState, name: &str, args: &Value, workspace: &PathBuf) -> Value {
    match name {
        "send_notification" => {
            let title = args["title"].as_str().unwrap_or("ai-pod");
            let message = args["message"].as_str().unwrap_or("");
            send_notification(title, message);
            json!({ "content": [{ "type": "text", "text": "Notification sent." }] })
        }
        "run_host_command" => {
            let command = args["command"].as_str().unwrap_or("");
            if command.is_empty() {
                return json!({
                    "content": [{ "type": "text", "text": "Missing required argument: command" }],
                    "isError": true
                });
            }
            run_host_command(state, command, workspace).await
        }
        "list_allowed_commands" => list_allowed_commands(state, workspace).await,
        _ => json!({
            "content": [{ "type": "text", "text": format!("Unknown tool: {}", name) }],
            "isError": true
        }),
    }
}

fn single_sse_event(resp: JsonRpcResponse) -> Response {
    let data = serde_json::to_string(&resp).unwrap_or_default();
    let stream = futures_util::stream::once(async move {
        Ok::<Event, std::convert::Infallible>(Event::default().event("message").data(data))
    });
    Sse::new(stream).into_response()
}

fn stream_tool_call(
    state: AppState,
    id: Option<Value>,
    tool_name: String,
    args: Value,
    workspace: PathBuf,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(32);

    tokio::spawn(async move {
        let result = dispatch_tool(&state, &tool_name, &args, &workspace).await;
        let resp = JsonRpcResponse::ok(id, result);
        let data = serde_json::to_string(&resp).unwrap_or_default();
        let event = Event::default().event("message").data(data);
        let _ = tx.send(Ok(event)).await;
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}
