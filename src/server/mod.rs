pub mod lifecycle;
pub mod notify;

use axum::{Router, routing::get, routing::post};
use std::net::SocketAddr;

async fn health_handler() -> &'static str {
    "ok"
}

async fn notify_handler() -> &'static str {
    notify::send_notification("Claude Code", "Task completed.");
    "ok"
}

pub async fn run_server(port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/notify", post(notify_handler));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Notification server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
