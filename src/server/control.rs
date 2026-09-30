//! Host-only server lifecycle API. Credentials stay outside container mounts.
use super::{AppState, available_runtimes};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::Notify;

#[derive(Serialize, Deserialize)]
pub(super) struct Credentials {
    pub pid: u32,
    pub token: String,
}

impl Credentials {
    pub fn load(dir: &Path) -> Result<Self> {
        serde_json::from_slice(&std::fs::read(dir.join("server-control.json"))?)
            .context("Invalid server control credentials")
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join("server-control.json");
        let tmp = dir.join("server-control.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Session {
    pub runtime: String,
    pub container: String,
    pub session_id: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Status {
    pub version: String,
    pub pid: u32,
    pub sessions: Vec<Session>,
}

#[derive(Clone)]
struct ControlState {
    app: AppState,
    token: String,
    quit: Arc<Notify>,
}

async fn authorize(
    State(state): State<ControlState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let supplied = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(supplied.as_bytes().ct_eq(state.token.as_bytes())) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

fn parse_sessions(runtime: &str, output: &str) -> Vec<Session> {
    output
        .lines()
        .filter_map(|line| {
            let (name, labels) = line.split_once('\t')?;
            if labels
                .split(',')
                .any(|label| label.trim() == "ai-pod-service=true")
            {
                return None;
            }
            let session_id = crate::workspace::session_id_from_container_name(name)?;
            Some(Session {
                runtime: runtime.into(),
                container: name.into(),
                session_id,
            })
        })
        .collect()
}

async fn status(State(state): State<ControlState>) -> Result<Json<Status>, (StatusCode, String)> {
    let mut sessions = Vec::new();
    for rt in available_runtimes(state.app.runtime.dry_run) {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            rt.async_command()
                .args([
                    "ps",
                    "--filter",
                    "label=managed-by=ai-pod",
                    "--format",
                    "{{.Names}}\t{{.Labels}}",
                ])
                .output(),
        )
        .await;
        match output {
            Ok(Ok(output)) if output.status.success() => {
                sessions.extend(parse_sessions(
                    rt.cmd(),
                    &String::from_utf8_lossy(&output.stdout),
                ));
            }
            _ => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "Could not list {} sessions; server was not stopped",
                        rt.cmd()
                    ),
                ));
            }
        }
    }
    sessions.sort_by(|a, b| (&a.runtime, &a.container).cmp(&(&b.runtime, &b.container)));
    // Give the user time to read the list. The client refreshes this while prompting.
    *state.app.keep_alive_until.lock().await = std::time::Instant::now() + Duration::from_secs(30);
    Ok(Json(Status {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        sessions,
    }))
}

async fn quit(State(state): State<ControlState>) -> &'static str {
    state.quit.notify_one();
    "quitting"
}

pub(super) fn router(app: AppState, token: String, quit_signal: Arc<Notify>) -> Router {
    let state = ControlState {
        app,
        token,
        quit: quit_signal,
    };
    Router::new()
        .route("/server/status", get(status))
        .route("/server/quit", post(quit))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_router(token: &str, quit: Arc<Notify>) -> Router {
        use std::collections::HashMap;
        use tokio::sync::Mutex;
        let state = AppState {
            projects: Arc::new(Mutex::new(HashMap::new())),
            config_dir: std::path::PathBuf::new(),
            approval_lock: Arc::new(Mutex::new(())),
            commands: Arc::new(Mutex::new(HashMap::new())),
            runtime: crate::runtime::ContainerRuntime {
                kind: crate::runtime::RuntimeKind::Docker,
                dry_run: true,
            },
            keep_alive_until: Arc::new(Mutex::new(std::time::Instant::now())),
        };
        router(state, token.into(), quit)
    }

    #[tokio::test]
    async fn lifecycle_endpoints_require_server_credentials() {
        use tower::ServiceExt;
        for (method, path) in [("GET", "/server/status"), ("POST", "/server/quit")] {
            for token in [None, Some("Bearer wrong")] {
                let quit = Arc::new(Notify::new());
                let app = test_router("secret", quit.clone());
                let mut request = axum::http::Request::builder().method(method).uri(path);
                if let Some(token) = token {
                    request = request.header("authorization", token);
                }
                let response = app
                    .oneshot(request.body(axum::body::Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                assert!(
                    tokio::time::timeout(Duration::from_millis(10), quit.notified())
                        .await
                        .is_err()
                );
            }
        }
    }

    #[tokio::test]
    async fn authenticated_quit_signals_shutdown() {
        use tower::ServiceExt;
        let quit = Arc::new(Notify::new());
        let response = test_router("secret", quit.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/server/quit")
                    .header("authorization", "Bearer secret")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_millis(100), quit.notified())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn server_publishes_status_and_releases_port_after_quit() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::AppConfig {
            config_dir: dir.path().to_path_buf(),
            home_dir: dir.path().to_path_buf(),
            runtime_settings: dir.path().join("runtime-settings.json"),
        };
        // Choose an unused port without touching the user's shared server.
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let server = tokio::spawn(super::super::run_server(
            port,
            config,
            crate::runtime::ContainerRuntime {
                kind: crate::runtime::RuntimeKind::Docker,
                dry_run: true,
            },
        ));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let credentials = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(credentials) = Credentials::load(dir.path()) {
                    break credentials;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status: Status = client
            .get(format!("{base}/server/status"))
            .bearer_auth(&credentials.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status.pid, credentials.pid);
        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
        assert!(status.sessions.is_empty());
        client
            .post(format!("{base}/server/quit"))
            .bearer_auth(&credentials.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(7), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // The replacement can bind only once the old listener has shut down.
        let _replacement = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
    }

    #[test]
    fn session_list_excludes_services_and_temporary_containers() {
        let sessions = parse_sessions(
            "docker",
            "ai-pod-abcdef123456-1234abcd\tmanaged-by=ai-pod\nai-pod-abcdef123456-1234abcd-svc-deadbeef\tmanaged-by=ai-pod,ai-pod-service=true\nai-pod-abcdef123456-init\tmanaged-by=ai-pod\n",
        );
        assert_eq!(
            sessions,
            vec![Session {
                runtime: "docker".into(),
                container: "ai-pod-abcdef123456-1234abcd".into(),
                session_id: "1234abcd".into()
            }]
        );
    }

    #[test]
    fn credentials_are_private_and_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Credentials {
            pid: 123,
            token: "secret".into(),
        }
        .save(dir.path())
        .unwrap();
        assert_eq!(Credentials::load(dir.path()).unwrap().token, "secret");
        assert_eq!(
            std::fs::metadata(dir.path().join("server-control.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
