//! End-to-end tests that exercise a real container runtime (Docker or Podman).
//!
//! These tests are automatically **skipped** when neither `docker` nor `podman`
//! is available on the host, so `cargo test` is safe to run anywhere.

use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Detect an available container runtime **with a running daemon**.
/// `--version` succeeds even without a daemon, so we probe with `info`.
/// Returns `None` when neither docker nor podman is usable, causing every
/// test to skip gracefully.
fn detect_runtime() -> Option<String> {
    for cmd in &["podman", "docker"] {
        if Command::new(cmd)
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Some(cmd.to_string());
        }
    }
    None
}

macro_rules! require_runtime {
    () => {
        match detect_runtime() {
            Some(rt) => rt,
            None => {
                eprintln!("SKIPPED: no container runtime (docker/podman) available");
                return;
            }
        }
    };
}

/// Build a minimal Alpine image and return its tag. Panics on failure.
fn build_alpine_image(rt: &str, tag: &str) {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("Dockerfile"), "FROM alpine:latest\n").unwrap();
    let status = Command::new(rt)
        .args(["build", "-t", tag, "-f", "Dockerfile", "."])
        .current_dir(tmp.path())
        .status()
        .expect("failed to start image build");
    assert!(status.success(), "alpine image build failed");
}

fn cleanup_image(rt: &str, image: &str) {
    let _ = Command::new(rt).args(["rmi", "-f", image]).output();
}

fn cleanup_container(rt: &str, name: &str) {
    let _ = Command::new(rt).args(["rm", "-f", name]).output();
}

fn cleanup_volume(rt: &str, name: &str) {
    let _ = Command::new(rt).args(["volume", "rm", "-f", name]).output();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_runtime_is_responsive() {
    let rt = require_runtime!();
    let output = Command::new(&rt)
        .arg("info")
        .output()
        .expect("failed to run runtime");
    assert!(
        output.status.success(),
        "{} info failed: {}",
        rt,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn e2e_build_project_base_image() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-base:test";

    let status = Command::new(&rt)
        .args(["build", "-t", tag, "-f", "claude.Dockerfile", "."])
        .status()
        .expect("failed to start build");
    assert!(status.success(), "build from claude.Dockerfile failed");

    // Verify image exists via inspect
    let output = Command::new(&rt)
        .args(["image", "inspect", tag])
        .output()
        .unwrap();
    assert!(output.status.success(), "built image not found");

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_run_echo_in_container() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-echo:test";
    build_alpine_image(&rt, tag);

    let output = Command::new(&rt)
        .args(["run", "--rm", tag, "echo", "hello-from-ai-pod"])
        .output()
        .unwrap();

    assert!(output.status.success(), "container run failed");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("hello-from-ai-pod"),
        "expected output not found"
    );

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_container_user_is_claude() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-user:test";

    let status = Command::new(&rt)
        .args(["build", "-t", tag, "-f", "claude.Dockerfile", "."])
        .status()
        .unwrap();
    assert!(status.success(), "image build failed");

    let output = Command::new(&rt)
        .args(["run", "--rm", tag, "whoami"])
        .output()
        .unwrap();

    assert!(output.status.success(), "whoami failed");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "claude",
        "default user should be claude"
    );

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_container_has_git() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-git:test";

    let status = Command::new(&rt)
        .args(["build", "-t", tag, "-f", "claude.Dockerfile", "."])
        .status()
        .unwrap();
    assert!(status.success());

    let output = Command::new(&rt)
        .args(["run", "--rm", tag, "git", "--version"])
        .output()
        .unwrap();

    assert!(output.status.success(), "git not found in container");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("git version"),
        "unexpected git output"
    );

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_container_workdir_is_app() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-workdir:test";

    let status = Command::new(&rt)
        .args(["build", "-t", tag, "-f", "claude.Dockerfile", "."])
        .status()
        .unwrap();
    assert!(status.success());

    let output = Command::new(&rt)
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

#[test]
fn e2e_workspace_bind_mount() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-mount:test";
    build_alpine_image(&rt, tag);

    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("marker.txt"), "e2e-test-content").unwrap();

    let mount_arg = format!("{}:/app", workspace.path().display());
    let output = Command::new(&rt)
        .args(["run", "--rm", "-v", &mount_arg, tag, "cat", "/app/marker.txt"])
        .output()
        .unwrap();

    assert!(output.status.success(), "bind mount read failed");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("e2e-test-content"),
        "marker file content mismatch"
    );

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_volume_create_inspect_remove() {
    let rt = require_runtime!();
    let vol = "ai-pod-e2e-vol-lifecycle";

    // Ensure clean state
    cleanup_volume(&rt, vol);

    // Create
    let status = Command::new(&rt)
        .args(["volume", "create", vol])
        .status()
        .unwrap();
    assert!(status.success(), "volume create failed");

    // Inspect
    let status = Command::new(&rt)
        .args(["volume", "inspect", vol])
        .status()
        .unwrap();
    assert!(status.success(), "volume inspect failed after create");

    // Remove
    let status = Command::new(&rt)
        .args(["volume", "rm", vol])
        .status()
        .unwrap();
    assert!(status.success(), "volume rm failed");

    // Verify removed
    let status = Command::new(&rt)
        .args(["volume", "inspect", vol])
        .status()
        .unwrap();
    assert!(!status.success(), "volume should not exist after removal");
}

#[test]
fn e2e_volume_persists_data_across_containers() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-persist:test";
    let vol = "ai-pod-e2e-persist-vol";

    build_alpine_image(&rt, tag);
    cleanup_volume(&rt, vol);

    Command::new(&rt)
        .args(["volume", "create", vol])
        .status()
        .unwrap();

    // Write in first container
    let mount = format!("{}:/data", vol);
    let status = Command::new(&rt)
        .args([
            "run", "--rm", "-v", &mount, tag, "sh", "-c",
            "echo persisted-data > /data/test.txt",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "write to volume failed");

    // Read in second container
    let output = Command::new(&rt)
        .args(["run", "--rm", "-v", &mount, tag, "cat", "/data/test.txt"])
        .output()
        .unwrap();
    assert!(output.status.success(), "read from volume failed");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("persisted-data"),
        "data did not persist across containers"
    );

    cleanup_volume(&rt, vol);
    cleanup_image(&rt, tag);
}

#[test]
fn e2e_managed_by_label_filter() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-label:test";
    let name = "ai-pod-e2e-labeled";

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("Dockerfile"),
        "FROM alpine:latest\nCMD [\"sleep\", \"300\"]\n",
    )
    .unwrap();
    Command::new(&rt)
        .args(["build", "-t", tag, "-f", "Dockerfile", "."])
        .current_dir(tmp.path())
        .status()
        .unwrap();

    // Ensure clean
    cleanup_container(&rt, name);

    // Start detached with the same label ai-pod uses
    let status = Command::new(&rt)
        .args([
            "run", "-d", "--name", name, "--label", "managed-by=ai-pod", tag,
        ])
        .status()
        .unwrap();
    assert!(status.success(), "failed to start labeled container");

    // Filter should find it
    let output = Command::new(&rt)
        .args([
            "ps",
            "--filter",
            "label=managed-by=ai-pod",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let names = String::from_utf8_lossy(&output.stdout);
    assert!(
        names.contains(name),
        "labeled container not found in filtered ps output"
    );

    // Cleanup
    cleanup_container(&rt, name);

    // Verify it's gone
    let output = Command::new(&rt)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{}$", name),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(name),
        "container should be removed"
    );

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_container_env_vars_are_passed() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-env:test";
    build_alpine_image(&rt, tag);

    let output = Command::new(&rt)
        .args([
            "run",
            "--rm",
            "-e",
            "AI_POD_PROJECT_ID=test-project-123",
            "-e",
            "AI_POD_API_KEY=test-key-456",
            tag,
            "sh",
            "-c",
            "echo $AI_POD_PROJECT_ID $AI_POD_API_KEY",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test-project-123"), "project ID env var missing");
    assert!(stdout.contains("test-key-456"), "API key env var missing");

    cleanup_image(&rt, tag);
}

#[test]
fn e2e_home_volume_with_claude_user() {
    let rt = require_runtime!();
    let tag = "ai-pod-e2e-homevol:test";
    let vol = "ai-pod-e2e-home-vol";

    let status = Command::new(&rt)
        .args(["build", "-t", tag, "-f", "claude.Dockerfile", "."])
        .status()
        .unwrap();
    assert!(status.success());

    cleanup_volume(&rt, vol);
    Command::new(&rt)
        .args(["volume", "create", vol])
        .status()
        .unwrap();

    // Write a file as claude user into the home volume
    let mount = format!("{}:/home/claude", vol);
    let status = Command::new(&rt)
        .args([
            "run", "--rm", "-v", &mount, tag, "sh", "-c",
            "mkdir -p /home/claude/.claude && echo ok > /home/claude/.claude/test",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "writing to home volume as claude failed");

    // Verify file persists in a new container
    let output = Command::new(&rt)
        .args([
            "run", "--rm", "-v", &mount, tag, "cat", "/home/claude/.claude/test",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");

    cleanup_volume(&rt, vol);
    cleanup_image(&rt, tag);
}
