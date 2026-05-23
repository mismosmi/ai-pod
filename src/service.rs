//! Service container management.
//!
//! "Service" containers are auxiliary containers (e.g. postgres) requested by
//! the in-container agent via MCP. They live on a per-workspace bridge network
//! so the agent reaches them by DNS name, are labeled with the requesting
//! session's id for garbage collection, and are removed when that session
//! exits.

use anyhow::{Context, Result};
use serde::Serialize;

use crate::runtime::ContainerRuntime;
use crate::workspace::{service_container_name, service_network_name};

/// Label applied to every service container so we can list/clean them
/// independently of the main container.
pub const SERVICE_LABEL: &str = "ai-pod-service=true";
/// Label key carrying the requesting session id.
pub const PARENT_LABEL_KEY: &str = "ai-pod-parent";

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub name: String,
    pub container_name: String,
    pub image: String,
    pub status: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct StartedService {
    pub host: String,
    pub container_name: String,
}

/// Idempotently create the per-workspace network. Returns its name.
pub fn ensure_service_network(rt: &ContainerRuntime, workspace: &std::path::Path) -> Result<String> {
    let net = service_network_name(workspace);
    let status = rt
        .command()
        .args(["network", "inspect", &net])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to inspect network")?;
    if status.success() {
        return Ok(net);
    }
    let create = rt
        .command()
        .args(["network", "create", &net])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to create network")?;
    if !create.success() {
        anyhow::bail!("failed to create service network {}", net);
    }
    Ok(net)
}

/// Find the running main container for `(workspace, session_id)`. Returns the
/// container name. Errors if no such container is running.
pub fn find_main_container(
    rt: &ContainerRuntime,
    workspace: &std::path::Path,
    session_id: &str,
) -> Result<String> {
    let expected = crate::workspace::container_name_for(workspace, session_id);
    let output = rt
        .command()
        .args([
            "ps",
            "--filter",
            &format!("name=^{}$", expected),
            "--filter",
            "label=managed-by=ai-pod",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("failed to list main container")?;
    let name = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|l| !l.is_empty())
        .map(|l| l.to_string());
    match name {
        Some(n) => Ok(n),
        None => anyhow::bail!(
            "no running main container for session {} (expected {})",
            session_id,
            expected
        ),
    }
}

/// Attach the main container to the workspace network. Idempotent: a second
/// call swallows the "already attached" error.
pub fn connect_main_to_network(
    rt: &ContainerRuntime,
    workspace: &std::path::Path,
    session_id: &str,
) -> Result<()> {
    let net = service_network_name(workspace);
    let main = find_main_container(rt, workspace, session_id)?;
    let output = rt
        .command()
        .args(["network", "connect", &net, &main])
        .output()
        .context("failed to connect main container to service network")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if stderr.contains("already") {
        return Ok(());
    }
    anyhow::bail!(
        "failed to connect main container to network {}: {}",
        net,
        stderr.trim()
    );
}

/// Start a detached service container on the workspace network with a DNS
/// alias matching `name`. Returns the host the agent should use plus the
/// container's full name.
pub fn start_service(
    rt: &ContainerRuntime,
    workspace: &std::path::Path,
    session_id: &str,
    image: &str,
    name: &str,
    env: &[(String, String)],
    command: &[String],
) -> Result<StartedService> {
    let net = ensure_service_network(rt, workspace)?;
    connect_main_to_network(rt, workspace, session_id)?;

    let container_name = service_container_name(workspace, session_id, name);

    // Refuse to silently clobber an existing service of the same name.
    let existing = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{}$", container_name),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("failed to check existing service container")?;
    if !String::from_utf8_lossy(&existing.stdout).trim().is_empty() {
        anyhow::bail!(
            "service '{}' already exists for this session; stop it first or pick a different name",
            name
        );
    }

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--rm".into(),
        "--name".into(),
        container_name.clone(),
        "--label".into(),
        "managed-by=ai-pod".into(),
        "--label".into(),
        SERVICE_LABEL.into(),
        "--label".into(),
        format!("{}={}", PARENT_LABEL_KEY, session_id),
        "--network".into(),
        net,
        "--network-alias".into(),
        name.to_string(),
    ];
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{}={}", k, v));
    }
    args.push(image.to_string());
    for c in command {
        args.push(c.clone());
    }

    let output = rt
        .command()
        .args(&args)
        .output()
        .context("failed to spawn service container")?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to start service container: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(StartedService {
        host: name.to_string(),
        container_name,
    })
}

/// Stop a service container belonging to `session_id`. Returns true on
/// successful removal, false if no matching container existed.
pub fn stop_service(
    rt: &ContainerRuntime,
    workspace: &std::path::Path,
    session_id: &str,
    name: &str,
) -> Result<bool> {
    let container_name = service_container_name(workspace, session_id, name);
    // Verify ownership: filter by both the expected name and the parent label.
    let listed = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{}$", container_name),
            "--filter",
            &format!("label={}={}", PARENT_LABEL_KEY, session_id),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("failed to list service container")?;
    if String::from_utf8_lossy(&listed.stdout).trim().is_empty() {
        return Ok(false);
    }
    let status = rt
        .command()
        .args(["rm", "--force", &container_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to remove service container")?;
    Ok(status.success())
}

/// List service containers owned by `session_id` in `workspace`.
pub fn list_services(
    rt: &ContainerRuntime,
    workspace: &std::path::Path,
    session_id: &str,
) -> Result<Vec<ServiceInfo>> {
    let prefix = format!(
        "ai-pod-{}-{}-svc-",
        crate::workspace::workspace_hash(workspace),
        session_id
    );
    let output = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{}", prefix),
            "--filter",
            &format!("label={}={}", PARENT_LABEL_KEY, session_id),
            "--format",
            "{{.Names}}\t{{.Image}}\t{{.Status}}",
        ])
        .output()
        .context("failed to list service containers")?;
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let container_name = parts.next().unwrap_or("").to_string();
        let image = parts.next().unwrap_or("").to_string();
        let status = parts.next().unwrap_or("").to_string();
        let name = container_name
            .strip_prefix(&prefix)
            .unwrap_or(&container_name)
            .to_string();
        out.push(ServiceInfo {
            name,
            container_name,
            image,
            status,
        });
    }
    Ok(out)
}

/// Fetch the last `lines` lines of a service container's stdout+stderr (podman
/// merges them in `podman logs --tail`).
pub fn service_logs(
    rt: &ContainerRuntime,
    workspace: &std::path::Path,
    session_id: &str,
    name: &str,
    lines: usize,
) -> Result<String> {
    let container_name = service_container_name(workspace, session_id, name);
    let listed = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name=^{}$", container_name),
            "--filter",
            &format!("label={}={}", PARENT_LABEL_KEY, session_id),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("failed to list service container")?;
    if String::from_utf8_lossy(&listed.stdout).trim().is_empty() {
        anyhow::bail!("no service named '{}' for this session", name);
    }
    let output = rt
        .command()
        .args(["logs", "--tail", &lines.to_string(), &container_name])
        .output()
        .context("failed to read service logs")?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err);
    }
    Ok(text)
}

/// Best-effort removal of every service container labeled with this session
/// id. Used by the CLI right after the main container exits and by the
/// server's periodic sweep when the main container is gone.
pub fn cleanup_services_for_session(rt: &ContainerRuntime, session_id: &str) {
    let output = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={}={}", PARENT_LABEL_KEY, session_id),
            "--filter",
            &format!("label={}", SERVICE_LABEL),
            "--format",
            "{{.Names}}",
        ])
        .output();
    let names = match output {
        Ok(o) => o.stdout,
        Err(_) => return,
    };
    for name in String::from_utf8_lossy(&names).lines() {
        if name.is_empty() {
            continue;
        }
        let _ = rt
            .command()
            .args(["rm", "--force", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Async sibling of `cleanup_services_for_session` for use from inside the
/// shared server's tokio runtime.
pub async fn cleanup_services_for_session_async(rt: &ContainerRuntime, session_id: &str) {
    let output = rt
        .async_command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={}={}", PARENT_LABEL_KEY, session_id),
            "--filter",
            &format!("label={}", SERVICE_LABEL),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await;
    let names = match output {
        Ok(o) => o.stdout,
        Err(_) => return,
    };
    for name in String::from_utf8_lossy(&names).lines() {
        if name.is_empty() {
            continue;
        }
        let _ = rt
            .async_command()
            .args(["rm", "--force", name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
}

/// Remove the per-workspace service network. Best-effort: ignores "not found"
/// or "in use" errors so callers don't have to special-case the first run.
pub fn remove_service_network(rt: &ContainerRuntime, workspace: &std::path::Path) {
    let net = service_network_name(workspace);
    let _ = rt
        .command()
        .args(["network", "rm", &net])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_label_constant_matches_filter_form() {
        assert_eq!(SERVICE_LABEL, "ai-pod-service=true");
    }

    #[test]
    fn parent_label_key_matches_documented_value() {
        assert_eq!(PARENT_LABEL_KEY, "ai-pod-parent");
    }
}
