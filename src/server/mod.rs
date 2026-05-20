pub mod commands;
pub mod lifecycle;
pub mod mcp;
pub mod notify;
pub mod rest;
pub mod runner;

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

use crate::config::AppConfig;
use crate::runtime::ContainerRuntime;
use lifecycle::ProjectState;
use runner::CommandHandle;

#[derive(Clone)]
pub struct ProjectInfo {
    pub workspace: PathBuf,
    pub api_key: String,
}

#[derive(Clone)]
pub struct AppState {
    pub projects: Arc<Mutex<HashMap<String, ProjectInfo>>>,
    pub config_dir: PathBuf,
    pub approval_lock: Arc<Mutex<()>>,
    pub commands: Arc<Mutex<HashMap<(String, String), CommandHandle>>>,
    pub runtime: ContainerRuntime,
    pub keep_alive_until: Arc<Mutex<Instant>>,
}

async fn health_handler() -> &'static str {
    "ok"
}

async fn keep_alive_handler(State(state): State<AppState>) -> &'static str {
    *state.keep_alive_until.lock().await = Instant::now() + Duration::from_secs(30);
    "ok"
}

async fn version_handler() -> Json<serde_json::Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

const INSTALL_CLAUDE_SH: &str = include_str!("../../templates/install-claude.sh");
const INSTALL_OPENCODE_SH: &str = include_str!("../../templates/install-opencode.sh");

/// Stub returned when an outdated `ai-pod.Dockerfile` still tries to fetch
/// `/host-tools`. The bundled `host-tools` binary was removed in 0.11.0 in
/// favour of inline install scripts. Old Dockerfiles will write this script
/// to /usr/local/bin/host-tools and then invoke `host-tools install <agent>`,
/// which will print the upgrade instructions below and fail the build.
const HOST_TOOLS_DEPRECATED_STUB: &str = r#"#!/bin/sh
cat >&2 <<'EOF'

================================================================================
  ai-pod: your ai-pod.Dockerfile is out of date
================================================================================

  The `host-tools` helper binary was removed in ai-pod 0.11.0. The server no
  longer ships it, so your Dockerfile cannot build.

  To upgrade, replace your project's ai-pod.Dockerfile with the current
  template. For a default claude pod it looks like this:

    FROM rust:latest

    ARG HOST_GATEWAY
    RUN curl -fsSL "http://${HOST_GATEWAY}:7822/install/claude.sh" | bash

    WORKDIR /app
    RUN useradd -ms /bin/bash ai-pod
    RUN chown -R ai-pod /app

    RUN git config --system user.email "ai-pod@ai-pod" && \
        git config --system user.name "ai-pod"

    USER ai-pod
    ENV PATH="/home/ai-pod/.local/bin:${PATH}"

    CMD ["claude"]

  Swap `claude` for `opencode` if that is the agent you use. See the project
  README for the full template.

================================================================================
EOF
exit 1
"#;

async fn host_tools_deprecated_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        HOST_TOOLS_DEPRECATED_STUB,
    )
        .into_response()
}

async fn install_script_handler(AxumPath(name): AxumPath<String>) -> Response {
    let body = match name.as_str() {
        "claude.sh" => INSTALL_CLAUDE_SH,
        "opencode.sh" => INSTALL_OPENCODE_SH,
        _ => {
            return (StatusCode::NOT_FOUND, "Unknown install script").into_response();
        }
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Middleware that translates tower_governor's non-standard
/// `x-ratelimit-after` header into the standard `Retry-After` header on 429
/// responses, per RFC 7231 §7.1.3.
async fn add_retry_after_header(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if response.status() == StatusCode::TOO_MANY_REQUESTS
        && !response.headers().contains_key(header::RETRY_AFTER)
    {
        if let Some(wait) = response.headers().get("x-ratelimit-after").cloned() {
            response.headers_mut().insert(header::RETRY_AFTER, wait);
        }
    }
    response
}

pub fn build_app(state: AppState) -> Router {
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(1)
            .burst_size(50)
            .finish()
            .expect("valid governor config"),
    );

    let governor_limiter = governor_conf.limiter().clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            governor_limiter.retain_recent();
        }
    });

    let rate_limited = Router::new()
        .route("/health", get(health_handler))
        .route("/version", get(version_handler))
        .route("/keep-alive", post(keep_alive_handler))
        .route("/reload", post(reload_handler))
        .route("/notify_user", post(rest::notify_user_handler))
        .route("/list_allowed_commands", post(rest::list_allowed_commands_handler))
        .route("/commands/run", post(rest::run_command_handler))
        .route("/commands/stop", post(rest::stop_command_handler))
        .route("/commands/status", post(rest::command_status_handler))
        .route("/commands/list", post(rest::list_commands_handler))
        .route("/mcp", post(mcp::mcp_handler))
        .layer(GovernorLayer::new(governor_conf))
        .layer(middleware::from_fn(add_retry_after_header));

    // Unthrottled: install scripts (fetched at image build time, idempotent)
    Router::new()
        .route("/install/{name}", get(install_script_handler))
        .route("/host-tools", get(host_tools_deprecated_handler))
        .merge(rate_limited)
        .with_state(state)
}

async fn reload_handler(State(state): State<AppState>) -> &'static str {
    let mut projects = state.projects.lock().await;
    if let Ok(entries) = std::fs::read_dir(&state.config_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if stem == "server" {
                continue;
            }
            let ps = ProjectState::load(&path);
            if !ps.api_key.is_empty() && !ps.workspace.is_empty() {
                projects.insert(
                    stem,
                    ProjectInfo {
                        workspace: PathBuf::from(&ps.workspace),
                        api_key: ps.api_key,
                    },
                );
            }
        }
    }
    "reloaded"
}

pub async fn run_server(port: u16, config: AppConfig, rt: ContainerRuntime) -> anyhow::Result<()> {
    let mut projects: HashMap<String, ProjectInfo> = HashMap::new();

    if let Ok(entries) = std::fs::read_dir(&config.config_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if stem == "server" {
                continue;
            }
            let state = ProjectState::load(&path);
            if !state.api_key.is_empty() && !state.workspace.is_empty() {
                projects.insert(
                    stem,
                    ProjectInfo {
                        workspace: PathBuf::from(&state.workspace),
                        api_key: state.api_key,
                    },
                );
            }
        }
    }

    let state = AppState {
        projects: Arc::new(Mutex::new(projects)),
        config_dir: config.config_dir.clone(),
        approval_lock: Arc::new(Mutex::new(())),
        commands: Arc::new(Mutex::new(HashMap::new())),
        runtime: rt,
        keep_alive_until: Arc::new(Mutex::new(Instant::now() + Duration::from_secs(30))),
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_rt = state.runtime.clone();
    let shutdown_keep_alive = state.keep_alive_until.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            if Instant::now() < *shutdown_keep_alive.lock().await {
                continue;
            }
            let output = shutdown_rt
                .async_command()
                .args(["ps", "--filter", "label=managed-by=ai-pod", "--format", "{{.Names}}"])
                .output()
                .await;
            let has_containers = output
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| !l.is_empty()))
                .unwrap_or(true);
            if !has_containers {
                let _ = shutdown_tx.send(());
                break;
            }
        }
    });

    let app = build_app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Shared server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async { shutdown_rx.await.ok(); })
    .await?;

    Ok(())
}
