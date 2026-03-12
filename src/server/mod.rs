pub mod commands;
pub mod daemons;
pub mod lifecycle;
pub mod notify;
pub mod rest;

use axum::{Json, Router, extract::State, routing::{get, post}};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::AppConfig;
use daemons::DaemonEntry;
use lifecycle::ProjectState;

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
    pub daemons: Arc<Mutex<HashMap<String, DaemonEntry>>>,
}

async fn health_handler() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct RegisterRequest {
    project_id: String,
    api_key: String,
    workspace: String,
}

async fn register_handler(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> &'static str {
    let mut projects = state.projects.lock().await;
    projects.insert(
        req.project_id,
        ProjectInfo {
            workspace: PathBuf::from(req.workspace),
            api_key: req.api_key,
        },
    );
    "registered"
}

pub async fn run_server(port: u16, config: AppConfig) -> anyhow::Result<()> {
    // Scan existing project state files to pre-populate the projects map
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
            // Skip server.json
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
        daemons: Arc::new(Mutex::new(HashMap::new())),
    };

    // Background task: kill daemons when their project has no running containers
    let bg_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            daemons::cleanup_orphaned_daemons(&bg_state).await;
        }
    });

    // Background task: shut down server when all claude-* containers have stopped
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        // Grace period: allow container to start before first check
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            let output = tokio::process::Command::new("podman")
                .args(["ps", "--filter", "label=managed-by=ai-pod", "--format", "{{.Names}}"])
                .output()
                .await;
            let has_containers = output
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| !l.is_empty()))
                .unwrap_or(true); // on error, stay alive
            if !has_containers {
                let _ = shutdown_tx.send(());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/register", post(register_handler))
        .route("/run_command", post(rest::run_command_handler))
        .route("/notify_user", post(rest::notify_user_handler))
        .route("/list_allowed_commands", post(rest::list_allowed_commands_handler))
        .route("/daemon/start", post(daemons::start_daemon_handler))
        .route("/daemon/stop", post(daemons::stop_daemon_handler))
        .route("/daemon/stop-all", post(daemons::stop_all_daemons_handler))
        .route("/daemon/list", post(daemons::list_daemons_handler))
        .route("/daemon/status", post(daemons::daemon_status_handler))
        .route("/daemon/output", post(daemons::daemon_output_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Shared server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async { shutdown_rx.await.ok(); })
        .await?;

    Ok(())
}
