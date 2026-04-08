//! End-to-end tests that exercise production code against a real container
//! runtime (Docker or Podman).
//!
//! Automatically **skipped** when no runtime daemon is available.

use ai_pod::config::AppConfig;
use ai_pod::container;
use ai_pod::image;
use ai_pod::runtime::ContainerRuntime;
use ai_pod::workspace;

use std::path::Path;
use std::sync::Mutex;

/// Global lock to prevent concurrent `podman/docker build` calls.
///
/// Multiple parallel builds sharing the same overlay layer cache cause
/// storage corruption errors (e.g. "layer not known", "image not known").
/// Serialising builds avoids this while still letting non-build tests run
/// in parallel.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

fn build_image(rt: &ContainerRuntime, config: &AppConfig, dockerfile: &Path, tag: &str) {
    let _guard = BUILD_LOCK.lock().unwrap();
    image::build_image(rt, config, dockerfile, tag).unwrap();
}

fn ensure_image(rt: &ContainerRuntime, config: &AppConfig, dockerfile: &Path, tag: &str, force: bool) {
    let _guard = BUILD_LOCK.lock().unwrap();
    image::ensure_image(rt, config, dockerfile, tag, force).unwrap();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Try to detect a runtime via the production `ContainerRuntime::detect()`.
/// Returns `None` (skip) when neither daemon is reachable.
fn try_runtime() -> Option<ContainerRuntime> {
    // detect() checks `--version`, but the daemon may still be down.
    // Probe with `info` to confirm the daemon is actually running.
    let rt = ContainerRuntime::detect().ok()?;
    let ok = rt
        .command()
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok { Some(rt) } else { None }
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

/// Copy the project's `claude.Dockerfile` into a temp workspace as
/// `ai-pod.Dockerfile` so that `image::build_image` can find it.
/// Returns (TempDir, dockerfile_path).
fn make_test_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let ws = tempfile::TempDir::new().unwrap();
    let src = Path::new("claude.Dockerfile");
    let dst = ws.path().join(image::DOCKERFILE_NAME);
    std::fs::copy(src, &dst).expect("failed to copy claude.Dockerfile into test workspace");
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
    let status = rt.command().args(["volume", "create", vol]).status().unwrap();
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
    let status = rt.command().args(["volume", "create", &vol]).status().unwrap();
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
            "run", "-d", "--name", &name,
            "--label", "managed-by=ai-pod",
            tag, "sleep", "300",
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
    let status = rt.command().args(["volume", "create", &vol]).status().unwrap();
    assert!(status.success());

    // Start a labeled container
    let status = rt
        .command()
        .args([
            "run", "-d", "--name", &name,
            "--label", "managed-by=ai-pod",
            tag, "sleep", "300",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    // Verify both exist
    assert!(container::volume_exists(&rt, &vol).unwrap());
    assert!(!container::containers_for_prefix(&rt, &prefix, false).unwrap().is_empty());

    // Production clean_container should remove both
    container::clean_container(&rt, ws.path()).unwrap();

    assert!(!container::volume_exists(&rt, &vol).unwrap(), "volume should be removed");
    assert!(
        container::containers_for_prefix(&rt, &prefix, false).unwrap().is_empty(),
        "containers should be removed"
    );

    cleanup_image(&rt, tag);
}

/// Image built from `claude.Dockerfile` has `claude` as the default user.
#[test]
fn e2e_container_user_is_claude() {
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
        "claude",
        "default user should be claude"
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
