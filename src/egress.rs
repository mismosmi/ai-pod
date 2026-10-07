//! Egress filtering (`ai-pod egress`).
//!
//! When enabled for a workspace, the main container (and every service
//! container it starts) is placed on an `--internal` network with no route to
//! the outside world. A small per-session gateway container sits on both that
//! network and the regular per-workspace network and forwards exactly three
//! ports with socat:
//!
//!   * 7822 → the ai-pod server on the host (MCP, approvals, notifications)
//!   * 8931 → host-side Playwright MCP (only with `--playwright`)
//!   * 3128 → the configured upstream HTTP proxy
//!
//! Inside the main container the host-gateway name
//! (`host.containers.internal` / `host.docker.internal`) is pointed at the
//! gateway's IP, so every existing URL keeps working unchanged, and
//! `HTTP(S)_PROXY` points at port 3128 on it. The gateway doesn't filter
//! anything itself; the actual policy lives in the upstream proxy, which is
//! either an existing proxy the user runs (e.g. squid on the host) or a proxy
//! image ai-pod starts per session. Tools that ignore the proxy variables
//! simply can't connect, so the filter fails closed.
//!
//! When egress filtering is not configured nothing in this module runs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;

use crate::runtime::ContainerRuntime;
use crate::workspace::workspace_hash;

/// Label applied to the gateway and proxy containers so they can be cleaned up
/// independently of the main container and of agent-started services.
pub const EGRESS_LABEL: &str = "ai-pod-egress=true";

/// Port the in-container proxy variables point at (on the gateway).
pub const PROXY_PORT: u16 = 3128;

/// Image used for the gateway. Tiny (alpine + socat) and independent of the
/// user's base image.
pub const GATEWAY_IMAGE: &str = "docker.io/alpine/socat:latest";

/// Per-workspace egress configuration, stored in the project state file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum EgressConfig {
    /// Forward to a proxy that is already running somewhere reachable from
    /// the host network (e.g. squid on the host, a corporate proxy).
    Proxy {
        /// `host:port`. `localhost` / `127.0.0.1` refer to the host machine.
        address: String,
    },
    /// Start this image as the proxy for every session.
    Image {
        image: String,
        /// Port the proxy listens on inside its container.
        port: u16,
        /// Extra `-v` arguments for the proxy container (e.g. an allow-list).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        volumes: Vec<String>,
    },
}

impl EgressConfig {
    /// One-line human description for `ai-pod egress status` and launch output.
    pub fn describe(&self) -> String {
        match self {
            EgressConfig::Proxy { address } => format!("via proxy {}", address),
            EgressConfig::Image { image, port, .. } => {
                format!("via proxy container {} (port {})", image, port)
            }
        }
    }
}

/// Validate a `host:port` proxy address. The value ends up in the gateway's
/// shell command, so only hostname/IP characters are accepted.
pub fn validate_address(address: &str) -> Result<()> {
    let (host, port) = address
        .rsplit_once(':')
        .context("Proxy address must be host:port (e.g. localhost:3128)")?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == ':')
    {
        anyhow::bail!("Invalid proxy host '{}'", host);
    }
    port.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .with_context(|| format!("Invalid proxy port '{}'", port))?;
    Ok(())
}

/// Rewrite loopback hosts to the runtime's host-gateway name: inside the
/// gateway container, `localhost` would be the gateway itself.
fn upstream_for_gateway(address: &str, host_gateway: &str) -> String {
    let (host, port) = address.rsplit_once(':').unwrap_or((address, ""));
    match host {
        "localhost" | "127.0.0.1" | "[::1]" => format!("{}:{}", host_gateway, port),
        _ => address.to_string(),
    }
}

/// The per-workspace `--internal` network that filtered sessions join.
pub fn filtered_network_name(workspace: &Path) -> String {
    format!("ai-pod-{}-filtered-net", workspace_hash(workspace))
}

pub fn gateway_container_name(workspace: &Path, session_id: &str) -> String {
    format!("ai-pod-{}-{}-egress-gw", workspace_hash(workspace), session_id)
}

pub fn proxy_container_name(workspace: &Path, session_id: &str) -> String {
    format!("ai-pod-{}-{}-egress-proxy", workspace_hash(workspace), session_id)
}

/// Idempotently create the per-workspace internal network.
fn ensure_filtered_network(rt: &ContainerRuntime, workspace: &Path) -> Result<String> {
    let net = filtered_network_name(workspace);
    let exists = rt
        .command()
        .args(["network", "inspect", &net])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to inspect network")?;
    if exists.success() {
        return Ok(net);
    }
    let create = rt
        .command()
        .args(["network", "create", "--internal", &net])
        .output()
        .context("failed to create network")?;
    if create.status.success() {
        return Ok(net);
    }
    let stderr = String::from_utf8_lossy(&create.stderr).to_lowercase();
    if stderr.contains("already exists") || stderr.contains("already in use") {
        return Ok(net);
    }
    anyhow::bail!(
        "failed to create filtered network {}: {}",
        net,
        stderr.trim()
    );
}

pub fn remove_filtered_network(rt: &ContainerRuntime, workspace: &Path) {
    let _ = rt
        .command()
        .args(["network", "rm", &filtered_network_name(workspace)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Shell script run by the gateway: one socat forwarder per port.
fn gateway_script(host_gateway: &str, upstream: &str, playwright: bool) -> String {
    let mut ports = vec![(
        crate::server::lifecycle::MCP_PORT,
        format!("{}:{}", host_gateway, crate::server::lifecycle::MCP_PORT),
    )];
    if playwright {
        ports.push((
            crate::playwright::PORT,
            format!("{}:{}", host_gateway, crate::playwright::PORT),
        ));
    }
    ports.push((PROXY_PORT, upstream.to_string()));
    let mut script = String::new();
    for (port, target) in ports {
        script.push_str(&format!(
            "socat TCP-LISTEN:{},fork,reuseaddr TCP:{} & ",
            port, target
        ));
    }
    script.push_str("wait");
    script
}

/// Network attachment for a session's main container.
pub struct EgressSession {
    /// Network the main container joins.
    pub network: String,
    /// `--add-host` argument mapping the host-gateway name.
    pub add_host_arg: String,
    /// Extra `-e` values (the proxy environment when filtered).
    pub env: Vec<String>,
}

/// Proxy environment for the main container. Lower- and upper-case variants
/// are both set since tools disagree on which one they read.
fn proxy_env(host_gateway: &str) -> Vec<String> {
    let proxy = format!("http://{}:{}", host_gateway, PROXY_PORT);
    let no_proxy = format!("localhost,127.0.0.1,::1,{}", host_gateway);
    let mut env = Vec::new();
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        env.push(format!("{}={}", key, proxy));
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.push(format!("{}={}", key, no_proxy));
    }
    env
}

fn run_checked(rt: &ContainerRuntime, args: &[String], what: &str) -> Result<String> {
    let mut cmd = rt.command();
    // Proxy/gateway images expect the default user mapping, same as services.
    cmd.env_remove("PODMAN_USERNS");
    let output = cmd
        .args(args)
        .output()
        .with_context(|| format!("failed to {}", what))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to {}: {}",
            what,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn labels(session_id: &str) -> Vec<String> {
    vec![
        "--label".into(),
        EGRESS_LABEL.into(),
        "--label".into(),
        format!("{}={}", crate::service::PARENT_LABEL_KEY, session_id),
    ]
}

/// Start the gateway (and, in image mode, the proxy container) for a session
/// and return what the main container needs. On failure everything started
/// so far is removed again.
pub fn start(
    rt: &ContainerRuntime,
    workspace: &Path,
    session_id: &str,
    egress: &EgressConfig,
    playwright: bool,
) -> Result<EgressSession> {
    let result = start_inner(rt, workspace, session_id, egress, playwright);
    if result.is_err() {
        cleanup_for_session(rt, session_id);
    }
    result
}

fn start_inner(
    rt: &ContainerRuntime,
    workspace: &Path,
    session_id: &str,
    egress: &EgressConfig,
    playwright: bool,
) -> Result<EgressSession> {
    let host_gateway = rt.host_gateway();
    let filtered = ensure_filtered_network(rt, workspace)?;
    // The regular per-workspace network has normal egress; gateway and proxy
    // container use it to reach the host and the internet.
    let outside = crate::service::ensure_service_network(rt, workspace)?;

    let upstream = match egress {
        EgressConfig::Proxy { address } => upstream_for_gateway(address, host_gateway),
        EgressConfig::Image {
            image,
            port,
            volumes,
        } => {
            let name = proxy_container_name(workspace, session_id);
            let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone()];
            args.extend(labels(session_id));
            args.extend(["--network".into(), outside.clone(), rt.add_host_arg()]);
            for v in volumes {
                args.extend(["-v".into(), v.clone()]);
            }
            args.push(image.clone());
            run_checked(rt, &args, "start egress proxy container")?;
            format!("{}:{}", name, port)
        }
    };

    // create + network connect + start rather than repeated `--network`
    // flags, which older Docker releases don't accept on `run`.
    let gw = gateway_container_name(workspace, session_id);
    let mut create: Vec<String> = vec!["create".into(), "--name".into(), gw.clone()];
    create.extend(labels(session_id));
    create.extend([
        "--network".into(),
        outside,
        rt.add_host_arg(),
        "--entrypoint".into(),
        "sh".into(),
        GATEWAY_IMAGE.into(),
        "-c".into(),
        gateway_script(host_gateway, &upstream, playwright),
    ]);
    run_checked(rt, &create, "create egress gateway")?;
    run_checked(
        rt,
        &["network".into(), "connect".into(), filtered.clone(), gw.clone()],
        "attach egress gateway to filtered network",
    )?;
    run_checked(rt, &["start".into(), gw.clone()], "start egress gateway")?;

    let ip = if rt.dry_run {
        "<gateway-ip>".to_string()
    } else {
        let format = format!(
            "{{{{(index .NetworkSettings.Networks \"{}\").IPAddress}}}}",
            filtered
        );
        let ip = run_checked(
            rt,
            &["inspect".into(), "--format".into(), format, gw],
            "look up egress gateway address",
        )?;
        if ip.is_empty() {
            anyhow::bail!("egress gateway has no address on {}", filtered);
        }
        ip
    };

    Ok(EgressSession {
        network: filtered,
        add_host_arg: format!("--add-host={}:{}", host_gateway, ip),
        env: proxy_env(host_gateway),
    })
}

/// Remove the gateway and proxy containers belonging to `session_id`.
pub fn cleanup_for_session(rt: &ContainerRuntime, session_id: &str) {
    let output = rt
        .command()
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={}={}", crate::service::PARENT_LABEL_KEY, session_id),
            "--filter",
            &format!("label={}", EGRESS_LABEL),
            "--format",
            "{{.Names}}",
        ])
        .output();
    let Ok(output) = output else { return };
    for name in String::from_utf8_lossy(&output.stdout).lines() {
        if name.is_empty() {
            continue;
        }
        let _ = rt
            .command()
            .args(["rm", "--force", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_address_accepts_host_port() {
        for ok in [
            "localhost:3128",
            "127.0.0.1:3128",
            "proxy.corp.example:8080",
            "host.containers.internal:3128",
            "[::1]:3128",
        ] {
            assert!(validate_address(ok).is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn validate_address_rejects_bad_input() {
        for bad in [
            "localhost",
            ":3128",
            "localhost:",
            "localhost:0",
            "localhost:99999",
            "evil;rm -rf /:3128",
            "a b:3128",
            "$(id):3128",
            "http://proxy:3128",
        ] {
            assert!(validate_address(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn loopback_upstream_is_rewritten_to_host_gateway() {
        let gw = "host.containers.internal";
        assert_eq!(upstream_for_gateway("localhost:3128", gw), "host.containers.internal:3128");
        assert_eq!(upstream_for_gateway("127.0.0.1:8080", gw), "host.containers.internal:8080");
        assert_eq!(upstream_for_gateway("proxy.corp:3128", gw), "proxy.corp:3128");
    }

    #[test]
    fn gateway_script_forwards_server_and_proxy() {
        let s = gateway_script("host.docker.internal", "up:3128", false);
        assert!(s.contains("TCP-LISTEN:7822,fork,reuseaddr TCP:host.docker.internal:7822"));
        assert!(s.contains("TCP-LISTEN:3128,fork,reuseaddr TCP:up:3128"));
        assert!(!s.contains("8931"));
        assert!(s.ends_with("wait"));
    }

    #[test]
    fn gateway_script_forwards_playwright_when_enabled() {
        let s = gateway_script("host.containers.internal", "up:3128", true);
        assert!(s.contains("TCP-LISTEN:8931,fork,reuseaddr TCP:host.containers.internal:8931"));
    }

    #[test]
    fn proxy_env_sets_both_cases_and_bypasses_host_gateway() {
        let env = proxy_env("host.containers.internal");
        assert!(env.contains(&"HTTPS_PROXY=http://host.containers.internal:3128".to_string()));
        assert!(env.contains(&"http_proxy=http://host.containers.internal:3128".to_string()));
        assert!(env
            .iter()
            .any(|e| e.starts_with("NO_PROXY=") && e.contains("host.containers.internal")));
    }

    #[test]
    fn config_round_trips_through_json() {
        for cfg in [
            EgressConfig::Proxy {
                address: "localhost:3128".into(),
            },
            EgressConfig::Image {
                image: "ubuntu/squid".into(),
                port: 3128,
                volumes: vec!["/home/u/squid.conf:/etc/squid/squid.conf:ro".into()],
            },
        ] {
            let json = serde_json::to_string(&cfg).unwrap();
            assert_eq!(serde_json::from_str::<EgressConfig>(&json).unwrap(), cfg);
        }
        let json = serde_json::to_value(EgressConfig::Proxy {
            address: "x:1".into(),
        })
        .unwrap();
        assert_eq!(json["mode"], "proxy");
    }

    #[test]
    fn container_names_are_per_session_and_not_main_containers() {
        let ws = Path::new("/home/user/project");
        let gw = gateway_container_name(ws, "abcd1234");
        let proxy = proxy_container_name(ws, "abcd1234");
        assert_ne!(gw, proxy);
        // Must not look like a main container to the orphan sweep.
        assert!(crate::workspace::session_id_from_container_name(&gw).is_none());
        assert!(crate::workspace::session_id_from_container_name(&proxy).is_none());
    }
}
