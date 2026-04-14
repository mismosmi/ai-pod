//! End-to-end tests that exercise production code against a real container
//! runtime (Docker or Podman).
//!
//! Automatically **skipped** when no runtime daemon is available.

use ai_pod::config::AppConfig;
use ai_pod::container;
use ai_pod::image;
use ai_pod::runtime::ContainerRuntime;
use ai_pod::server;
use ai_pod::workspace;

use lazy_static::lazy_static;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

lazy_static! {
    /// Shared container runtime, lazily detected on first access. Wrapped in a
    /// Mutex so every test serialises *all* runtime operations — not just
    /// `build`. Parallel podman commands sharing the overlay cache corrupt
    /// each other ("layer not known", missing `merged` dir), which is why
    /// builds previously had to pass --no-cache. Locking the whole runtime
    /// lets us keep the layer cache enabled so ubuntu and apt aren't
    /// re-fetched for every test.
    ///
    /// `None` means no runtime is available on this machine; tests that
    /// require one skip via `try_runtime()`.
    static ref RT: Option<Mutex<ContainerRuntime>> = {
        let rt = ContainerRuntime::detect().ok()?;
        // detect() only checks `--version`; probe `info` to confirm the
        // daemon is actually running.
        let ok = rt
            .command()
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        ok.then(|| Mutex::new(rt))
    };
}

fn build_image(rt: &ContainerRuntime, config: &AppConfig, dockerfile: &Path, tag: &str) {
    let status = rt
        .command()
        .args([
            "build",
            "-t",
            tag,
            "-f",
            &dockerfile.to_string_lossy(),
            &config.config_dir.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn container build");
    assert!(status.success(), "{} build failed for tag {tag}", rt.cmd());
}

fn ensure_image(
    rt: &ContainerRuntime,
    config: &AppConfig,
    dockerfile: &Path,
    tag: &str,
    force: bool,
) {
    if force || image::needs_build(rt, tag, false).unwrap() {
        build_image(rt, config, dockerfile, tag);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `None` (skip) when no container runtime is available. On success,
/// the returned guard holds the global runtime lock for its lifetime, so each
/// test sees exclusive access to the container runtime.
fn try_runtime() -> Option<MutexGuard<'static, ContainerRuntime>> {
    // Recover from a poisoned lock so one panicking test doesn't cascade into
    // PoisonError failures for every subsequent test.
    Some(
        RT.as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    )
}

macro_rules! require_runtime {
    () => {
        match try_runtime() {
            Some(rt) => rt,
            None => {
                eprintln!("SKIPPED: no container runtime (docker/podman) available");
                return;
            }
        }
    };
}

/// Create a temporary `AppConfig` suitable for image builds. The returned
/// `TempDir` must be kept alive for the duration of the test (drop = cleanup).
fn make_test_config() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().to_path_buf();
    let config_dir = home.join(".ai-pod");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = AppConfig {
        runtime_settings: config_dir.join("runtime-settings.json"),
        runtime_claude_md: config_dir.join("runtime-CLAUDE.md"),
        config_dir,
        home_dir: home,
    };
    (dir, config)
}

/// Create a minimal test Dockerfile (no agent install, no host-tools download).
/// Returns (TempDir, dockerfile_path).
fn make_test_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let ws = tempfile::TempDir::new().unwrap();
    let dst = ws.path().join(image::DOCKERFILE_NAME);
    std::fs::write(&dst, r#"FROM ubuntu:latest
RUN apt-get update && apt-get install -y curl git vim
WORKDIR /app
RUN useradd -ms /bin/bash claude && chown -R claude /app
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"
USER claude
ENV PATH="/home/claude/.local/bin:${PATH}"
ENV EDITOR=vim
"#).expect("failed to write test Dockerfile");
    (ws, dst)
}

fn cleanup_image(rt: &ContainerRuntime, tag: &str) {
    let _ = rt.command().args(["rmi", "-f", tag]).output();
}

fn cleanup_container(rt: &ContainerRuntime, name: &str) {
    let _ = rt.command().args(["rm", "-f", name]).output();
}

fn cleanup_volume(rt: &ContainerRuntime, name: &str) {
    let _ = rt.command().args(["volume", "rm", "-f", name]).output();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `ContainerRuntime::detect()` finds a working runtime.
#[test]
fn e2e_runtime_detect() {
    let rt = require_runtime!();
    assert!(
        rt.cmd() == "podman" || rt.cmd() == "docker",
        "unexpected runtime: {}",
        rt.cmd()
    );
}

/// `image::build_image()` builds from the project's Dockerfile.
#[test]
fn e2e_build_image() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-build:test";

    build_image(&rt, &config, &dockerfile, tag);

    // Verify image exists via the runtime
    let status = rt
        .command()
        .args(["image", "inspect", tag])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "built image not found");

    cleanup_image(&rt, tag);
}

/// After a successful build, `needs_build()` returns false.
#[test]
fn e2e_needs_build_false_after_build() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-needs:test";

    // Before build: needs_build should be true
    assert!(image::needs_build(&rt, tag, false).unwrap());

    build_image(&rt, &config, &dockerfile, tag);

    // After build: needs_build should be false
    assert!(!image::needs_build(&rt, tag, false).unwrap());

    // With force=true it should always return true
    assert!(image::needs_build(&rt, tag, true).unwrap());

    cleanup_image(&rt, tag);
}

/// `ensure_image()` is idempotent — second call is a no-op.
#[test]
fn e2e_ensure_image_is_idempotent() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-ensure:test";

    // First call builds
    ensure_image(&rt, &config, &dockerfile, tag, false);
    assert!(!image::needs_build(&rt, tag, false).unwrap());

    // Second call should succeed without rebuilding
    ensure_image(&rt, &config, &dockerfile, tag, false);

    cleanup_image(&rt, tag);
}

/// `image::image_name()` produces a tag the runtime accepts for a real build.
#[test]
fn e2e_image_name_produces_valid_tag() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();

    let ws_path = _ws.path();
    let tag = image::image_name(ws_path);

    build_image(&rt, &config, &dockerfile, &tag);
    assert!(!image::needs_build(&rt, &tag, false).unwrap());

    cleanup_image(&rt, &tag);
}

/// `container::volume_exists()` correctly reports volume state.
#[test]
fn e2e_volume_exists_lifecycle() {
    let rt = require_runtime!();
    let vol = "ai-pod-e2e-volexist";

    cleanup_volume(&rt, vol);

    // Should not exist yet
    assert!(!container::volume_exists(&rt, vol).unwrap());

    // Create via runtime
    let status = rt
        .command()
        .args(["volume", "create", vol])
        .status()
        .unwrap();
    assert!(status.success());

    // Now should exist
    assert!(container::volume_exists(&rt, vol).unwrap());

    // Remove
    cleanup_volume(&rt, vol);

    // Should not exist again
    assert!(!container::volume_exists(&rt, vol).unwrap());
}

/// `workspace::volume_name()` and `container_prefix()` produce names the
/// runtime accepts for volume and container operations.
#[test]
fn e2e_workspace_naming_works_with_runtime() {
    let rt = require_runtime!();
    let ws = tempfile::TempDir::new().unwrap();

    let vol = workspace::volume_name(ws.path());
    let prefix = workspace::container_prefix(ws.path());
    let name = workspace::new_container_name(ws.path());

    // Volume name should be usable
    cleanup_volume(&rt, &vol);
    let status = rt
        .command()
        .args(["volume", "create", &vol])
        .status()
        .unwrap();
    assert!(status.success(), "runtime rejected volume name: {}", vol);
    assert!(container::volume_exists(&rt, &vol).unwrap());

    // Container name should be usable (and starts with prefix)
    assert!(name.starts_with(&prefix));

    cleanup_volume(&rt, &vol);
}

/// `container::containers_for_prefix()` finds labeled containers.
#[test]
fn e2e_containers_for_prefix() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-prefix:test";

    build_image(&rt, &config, &dockerfile, tag);

    let ws = tempfile::TempDir::new().unwrap();
    let prefix = workspace::container_prefix(ws.path());
    let name = workspace::new_container_name(ws.path());

    // Start a detached container with the ai-pod label
    cleanup_container(&rt, &name);
    let status = rt
        .command()
        .args([
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            "managed-by=ai-pod",
            tag,
            "sleep",
            "300",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "failed to start test container");

    // Production code should find it
    let found = container::containers_for_prefix(&rt, &prefix, true).unwrap();
    assert!(
        found.contains(&name),
        "containers_for_prefix did not find {}: {:?}",
        name,
        found
    );

    // Also find via non-running filter (all)
    let found_all = container::containers_for_prefix(&rt, &prefix, false).unwrap();
    assert!(found_all.contains(&name));

    cleanup_container(&rt, &name);
    cleanup_image(&rt, tag);
}

/// `container::clean_container()` removes containers and volumes for a workspace.
#[test]
fn e2e_clean_container_removes_all() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-clean:test";

    build_image(&rt, &config, &dockerfile, tag);

    let ws = tempfile::TempDir::new().unwrap();
    let prefix = workspace::container_prefix(ws.path());
    let vol = workspace::volume_name(ws.path());
    let name = workspace::new_container_name(ws.path());

    // Create volume
    let status = rt
        .command()
        .args(["volume", "create", &vol])
        .status()
        .unwrap();
    assert!(status.success());

    // Start a labeled container
    let status = rt
        .command()
        .args([
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            "managed-by=ai-pod",
            tag,
            "sleep",
            "300",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    // Verify both exist
    assert!(container::volume_exists(&rt, &vol).unwrap());
    assert!(
        !container::containers_for_prefix(&rt, &prefix, false)
            .unwrap()
            .is_empty()
    );

    // Production clean_container should remove both
    container::clean_container(&rt, ws.path()).unwrap();

    assert!(
        !container::volume_exists(&rt, &vol).unwrap(),
        "volume should be removed"
    );
    assert!(
        container::containers_for_prefix(&rt, &prefix, false)
            .unwrap()
            .is_empty(),
        "containers should be removed"
    );

    cleanup_image(&rt, tag);
}

/// Image built from `claude.Dockerfile` has `ubuntu` as the default user.
#[test]
fn e2e_container_default_user() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-user:test";

    build_image(&rt, &config, &dockerfile, tag);

    let output = rt
        .command()
        .args(["run", "--rm", tag, "whoami"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ubuntu",
        "default user should be ubuntu"
    );

    cleanup_image(&rt, tag);
}

/// Image built from `claude.Dockerfile` has `/app` as WORKDIR.
#[test]
fn e2e_container_workdir_is_app() {
    let rt = require_runtime!();
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-workdir:test";

    build_image(&rt, &config, &dockerfile, tag);

    let output = rt
        .command()
        .args(["run", "--rm", tag, "pwd"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "/app",
        "WORKDIR should be /app"
    );

    cleanup_image(&rt, tag);
}

// ---------------------------------------------------------------------------
// Async helpers
// ---------------------------------------------------------------------------

/// Bind to port 0, let the OS assign a free port, return it.
/// The listener is dropped immediately so the server can bind to it.
fn find_free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---------------------------------------------------------------------------
// Async tests (server)
// ---------------------------------------------------------------------------

/// The production HTTP server is reachable from inside a container via the
/// host gateway (`host.docker.internal` / `host.containers.internal`).
///
/// Exercises: `server::run_server()`, `rt.add_host_arg()`, `rt.host_gateway()`.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_server_reachable_from_container() {
    let rt = match try_runtime() {
        Some(rt) => rt,
        None => {
            eprintln!("SKIPPED: no container runtime (docker/podman) available");
            return;
        }
    };

    // Build image (claude.Dockerfile has curl)
    let (_ws, dockerfile) = make_test_workspace();
    let (_dir, config) = make_test_config();
    let tag = "ai-pod-e2e-server:test";
    build_image(&rt, &config, &dockerfile, tag);

    // Start the production server on a free port
    let port = find_free_port();
    let server_rt = rt.clone();
    let (_server_dir, server_config) = make_test_config();
    let server_handle = tokio::spawn(async move {
        let _ = server::run_server(port, server_config, server_rt).await;
    });

    // Wait for the server to become ready
    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{}/health", port);
    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if client.get(&health_url).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(ready.is_ok(), "server did not become ready within 5s");

    // --- /health from inside the container ---
    let add_host = rt.add_host_arg();
    let container_health_url = format!("http://{}:{}/health", rt.host_gateway(), port);

    let output = rt
        .command()
        .args([
            "run",
            "--rm",
            &add_host,
            tag,
            "curl",
            "-sf",
            &container_health_url,
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "curl /health from container failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ok",
        "/health should return 'ok'"
    );

    // --- /version from inside the container ---
    let container_version_url = format!("http://{}:{}/version", rt.host_gateway(), port);

    let output = rt
        .command()
        .args([
            "run",
            "--rm",
            &add_host,
            tag,
            "curl",
            "-sf",
            &container_version_url,
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "curl /version from container failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = String::from_utf8_lossy(&output.stdout);
    assert!(
        body.contains("version"),
        "/version response should contain 'version': got {}",
        body
    );

    // Cleanup
    server_handle.abort();
    cleanup_image(&rt, tag);
}

/// Build an image with `host-tools install` (multi-stage, builds
/// host-tools from source) and verify that `claude doctor` succeeds.
///
/// This is the equivalent of `ai-pod init && ai-pod run claude doctor`.
///
/// Ignored by default because it is slow (compiles host-tools inside the
/// container and downloads Claude Code on first run).
#[test]
#[ignore]
fn e2e_claude_doctor() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-doctor:test";
    let vol = "ai-pod-e2e-doctor-home";

    // Clean up any leftover state from a previous run
    cleanup_image(&rt, tag);
    cleanup_volume(&rt, vol);

    // Write a multi-stage Dockerfile that builds host-tools from source,
    // installs the claude stub, and sets up the container exactly like
    // the production claude.Dockerfile would.
    let ws = tempfile::TempDir::new().unwrap();
    let dockerfile = ws.path().join("Dockerfile");
    std::fs::write(&dockerfile, r#"FROM rust:latest AS builder
COPY . /build
WORKDIR /build
RUN cargo build --release --bin host-tools

FROM ubuntu:latest
RUN apt-get update && apt-get install -y curl git vim
COPY --from=builder /build/target/release/host-tools /usr/local/bin/host-tools
RUN host-tools install
WORKDIR /app
RUN useradd -ms /bin/bash claude && chown -R claude /app
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"
USER claude
ENV PATH="/home/claude/.local/bin:${PATH}"
ENV EDITOR=vim
CMD ["claude"]
"#).unwrap();

    // Build with project root as context (so COPY . /build gets the source)
    let project_root = std::env::current_dir().unwrap();
    let status = rt
        .command()
        .args([
            "build",
            "--no-cache",
            "-t",
            tag,
            "-f",
            &dockerfile.to_string_lossy(),
            &project_root.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn image build");
    assert!(status.success(), "image build failed");

    // Create a volume to simulate persistent home directory
    let status = rt
        .command()
        .args(["volume", "create", vol])
        .status()
        .unwrap();
    assert!(status.success(), "volume create failed");

    // Run `claude doctor` — first run triggers the lazy install stub
    let output = rt
        .command()
        .args([
            "run",
            "--rm",
            "-v",
            &format!("{}:/home/claude:z", vol),
            tag,
            "claude",
            "doctor",
        ])
        .output()
        .expect("failed to run claude doctor");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "claude doctor failed (exit {}).\nstdout: {}\nstderr: {}",
        output.status,
        stdout,
        stderr,
    );

    // Cleanup
    cleanup_volume(&rt, vol);
    cleanup_image(&rt, tag);
}
