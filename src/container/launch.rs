//! Interactive sessions, explicit commands, and their session networks.

use super::agent_config::opencode_config_content;
use super::home::{
    ensure_home_volume, execute_main_container, image_command, image_user, MAIN_BOOTSTRAP,
};
use super::mounts::{build_mount_args, mask_mount_args};
use super::{checked_status, containers_for_prefix};
use crate::config::AppConfig;
use crate::config::GlobalConfig;
use crate::runtime::ContainerRuntime;
use crate::server::lifecycle::ProjectState;
use crate::workspace::{
    container_name_for, container_prefix, new_session_id, volume_name as gen_volume_name,
    workspace_hash,
};
use anyhow::Result;
use colored::Colorize;
use std::path::Path;

/// Preserve terminal capabilities without overriding image defaults when unset.
fn terminal_env_args() -> impl Iterator<Item = &'static str> {
    ["TERM", "COLORTERM", "TERM_PROGRAM"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .flat_map(|name| ["-e", name])
}

fn load_project_state(config: &AppConfig, workspace: &Path) -> ProjectState {
    let hash = workspace_hash(workspace);
    ProjectState::load(&config.project_state_file(&hash))
}

/// Env var holding a comma/space separated list of host env var NAMES to pass
/// into the main container (e.g. `GITHUB_TOKEN,NPM_TOKEN`).
pub const ENV_PASSTHROUGH_VAR: &str = "AI_POD_ENV_PASSTHROUGH";

/// Build `-e NAME` pairs for host env vars listed in `AI_POD_ENV_PASSTHROUGH`.
///
/// Only the NAME is placed on the runtime command line; Podman/Docker read the
/// value from the ai-pod process environment, so secrets never appear in argv,
/// `--dry-run` output or logs. Names that are invalid or unset on the host are
/// skipped (and reported by name on stderr) rather than passed as empty.
pub fn passthrough_env_args() -> Vec<String> {
    let list = std::env::var(ENV_PASSTHROUGH_VAR).unwrap_or_default();
    passthrough_env_args_from(&list, |n| {
        std::env::var_os(n).is_some_and(|v| !v.is_empty())
    })
}

fn passthrough_env_args_from(list: &str, is_set: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in list
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric());
        if !valid {
            eprintln!(
                "{} ignoring invalid env name in {}: {:?}",
                "warning:".yellow().bold(),
                ENV_PASSTHROUGH_VAR,
                name
            );
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        if name.starts_with("SOPS_") {
            eprintln!(
                "{} not passing {} into the container: SOPS key material stays on the host",
                "warning:".yellow().bold(),
                name
            );
            continue;
        }
        if is_set(name) {
            out.push("-e".to_string());
            out.push(name.to_string());
        } else {
            eprintln!(
                "{} {} not set on host; not passed to container",
                "note:".dimmed(),
                name
            );
        }
    }
    out
}

/// Decrypt the workspace's SOPS-sealed secrets (see `sealed.rs`) and report them by name.
fn load_sealed_secrets(workspace: &Path) -> Result<Option<super::sealed::SealedSecrets>> {
    let sealed = super::sealed::load(workspace)?;
    if let Some(s) = &sealed {
        eprintln!(
            "{} {} from {} (sops), passed by name: {}",
            "Secrets:".blue().bold(),
            s.len(),
            s.file().file_name().unwrap_or_default().to_string_lossy(),
            s.names().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(sealed)
}

pub fn launch_container(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    rebuild: bool,
    image: &str,
    project_id: &str,
    api_key: &str,
    playwright: bool,
    publish: &[String],
) -> Result<()> {
    let prefix = container_prefix(workspace);
    let volume_name = gen_volume_name(workspace);
    let workspace_mounts = crate::workspace::workspace_mount_args(workspace);
    let container_workdir = crate::workspace::container_workdir(workspace).to_string_lossy();
    // Decrypt before creating anything, so a missing key fails the launch cleanly.
    let sealed = load_sealed_secrets(workspace)?;

    rt.warn_if_rootless_userns_mismatch();

    // Rebuild replaces containers; persistent home injections are versioned.
    if rebuild {
        for name in containers_for_prefix(rt, &prefix, false)? {
            checked_status(
                rt.command().args(["rm", "--force", &name]).status()?,
                "remove container for rebuild",
            )?;
        }
    }
    let user = image_user(rt, config, image)?;
    let home = &user.home;

    let session_id = new_session_id();
    let container_name = container_name_for(workspace, &session_id);
    eprintln!("{} {}", "Starting container:".blue().bold(), container_name);

    // Record the runtime for this session before the container starts, so the
    // shared server runs service containers on the same runtime.
    crate::config::SessionState {
        workspace: Some(workspace_hash(workspace)),
        runtime: rt.kind,
        network: None,
    }
    .save(config, &session_id)?;

    ensure_home_volume(rt, &volume_name)?;

    let host_gw_env = format!("HOST_GATEWAY={}", rt.host_gateway());
    let server_url_env = format!("AI_POD_SERVER_URL={}", rt.server_url());
    let opencode_config_env = format!(
        "OPENCODE_CONFIG_CONTENT={}",
        opencode_config_content(
            &rt.server_url(),
            api_key,
            &session_id,
            playwright.then(|| rt.host_gateway()),
        )
    );

    let project_state = load_project_state(config, workspace);
    let mask_args = mask_mount_args(rt, workspace, image, &project_state.masked_directories)?;
    let global = GlobalConfig::load(config);
    let user_mount_args = build_mount_args(&config.home_dir, &global.mounts)?;

    // Create the per-workspace service network up front and attach the main
    // container to it at launch. Lazy attach via `podman network connect` after
    // the fact does not work for rootless podman (slirp4netns containers cannot
    // be added to additional networks), so any later `start_service` call would
    // fail. Doing it here makes service-container requests work on every
    // runtime without restarting the session.
    let net = session_network(
        rt,
        config,
        workspace,
        &session_id,
        project_state.egress.as_ref(),
        playwright,
        publish,
    )?;

    let script_path = format!("{home}/.ai-pod-init-{session_id}.sh");
    let mut run_cmd = rt.command();
    run_cmd.args(["create", "--rm", "-it"]);
    run_cmd.args(terminal_env_args());
    run_cmd.args([
        "--name",
        &container_name,
        "--label",
        "managed-by=ai-pod",
        "--network",
        &net.network,
        "-v",
        &format!("{}:{}:z", volume_name, home),
        "--workdir",
        &container_workdir,
    ]);
    run_cmd.args(&workspace_mounts);
    for arg in &user_mount_args {
        run_cmd.arg(arg);
    }
    for arg in &mask_args {
        run_cmd.arg(arg);
    }
    for env in &net.env {
        run_cmd.args(["-e", env]);
    }
    run_cmd.args([
        &net.add_host_arg,
        "-e",
        &host_gw_env,
        "-e",
        &format!("AI_POD_PROJECT_ID={}", project_id),
        "-e",
        &format!("AI_POD_API_KEY={}", api_key),
        "-e",
        &format!("AI_POD_SESSION_ID={}", session_id),
        "-e",
        &server_url_env,
        "-e",
        &opencode_config_env,
    ]);
    for port in publish {
        run_cmd.args(["--publish", port]);
    }
    let mut env_args = passthrough_env_args();
    if let Some(s) = &sealed {
        env_args = s.without_overridden(env_args);
        env_args.extend(s.apply(&mut run_cmd));
    }
    run_cmd.args(env_args);
    run_cmd.args([
        "--entrypoint",
        "sh",
        image,
        "-c",
        MAIN_BOOTSTRAP,
        "ai-pod-init",
        &script_path,
    ]);
    run_cmd.args(image_command(rt, image)?);
    let run_status = execute_main_container(
        rt,
        run_cmd,
        &container_name,
        config,
        playwright,
        &script_path,
        &user,
    )?;

    // Main container has exited (cleanly or otherwise); tear down anything the
    // agent started for this session. Best-effort: this is also covered by the
    // server's periodic orphan sweep if the CLI was killed.
    crate::service::cleanup_services_for_session(rt, &session_id);
    crate::egress::cleanup_for_session(rt, &session_id);
    let _ = std::fs::remove_file(config.session_state_file(&session_id));
    checked_status(run_status, "run container")
}

pub fn run_in_container(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    image: &str,
    project_id: &str,
    api_key: &str,
    command: &str,
    args: &[String],
    interactive: bool,
    playwright: bool,
    publish: &[String],
) -> Result<()> {
    let session_id = new_session_id();
    let container_name = container_name_for(workspace, &session_id);
    let volume_name = gen_volume_name(workspace);
    let workspace_mounts = crate::workspace::workspace_mount_args(workspace);
    let container_workdir = crate::workspace::container_workdir(workspace).to_string_lossy();
    let sealed = load_sealed_secrets(workspace)?;

    rt.warn_if_rootless_userns_mismatch();

    // Record the runtime for this session before the container starts, so the
    // shared server runs service containers on the same runtime.
    crate::config::SessionState {
        workspace: Some(workspace_hash(workspace)),
        runtime: rt.kind,
        network: None,
    }
    .save(config, &session_id)?;

    let user = image_user(rt, config, image)?;
    let home = &user.home;
    ensure_home_volume(rt, &volume_name)?;

    eprintln!(
        "{} {} {}",
        "Running in container:".blue().bold(),
        container_name,
        command
    );

    let project_state = load_project_state(config, workspace);
    let mask_args = mask_mount_args(rt, workspace, image, &project_state.masked_directories)?;
    let global = GlobalConfig::load(config);
    let user_mount_args = build_mount_args(&config.home_dir, &global.mounts)?;

    // See the matching comment in launch_container — main goes on the
    // per-workspace service network at launch so service containers can be
    // attached later on rootless podman.
    let net = session_network(
        rt,
        config,
        workspace,
        &session_id,
        project_state.egress.as_ref(),
        playwright,
        publish,
    )?;

    // Without a tty on stdin (e.g. an IDE driving ai-pod over stdio for
    // ACP), `-t` would allocate a pseudo-TTY that mangles the JSON-RPC
    // byte stream the agent emits. Keep `-i` so stdin stays attached.
    let stdio_flag = if interactive { "-it" } else { "-i" };
    let script_path = format!("{home}/.ai-pod-init-{session_id}.sh");
    let mut run_args: Vec<String> = vec!["create".into(), "--rm".into(), stdio_flag.into()];
    if interactive {
        run_args.extend(terminal_env_args().map(String::from));
    }
    run_args.extend_from_slice(&[
        "--name".into(),
        container_name.clone(),
        "--label".into(),
        "managed-by=ai-pod".into(),
        "--network".into(),
        net.network,
        "-v".into(),
        format!("{}:{}:z", volume_name, home),
        "--workdir".into(),
        container_workdir.into_owned(),
    ]);
    for port in publish {
        run_args.extend(["--publish".to_string(), port.clone()]);
    }
    let mut env_args = passthrough_env_args();
    if let Some(s) = &sealed {
        env_args = s.without_overridden(env_args);
        env_args.extend(s.names().flat_map(|n| ["-e".to_string(), n.to_string()]));
    }
    run_args.extend(env_args);
    run_args.extend(workspace_mounts);
    run_args.extend(user_mount_args);
    run_args.extend(mask_args);
    for env in net.env {
        run_args.extend(["-e".into(), env]);
    }
    run_args.extend_from_slice(&[
        net.add_host_arg,
        "-e".into(),
        format!("HOST_GATEWAY={}", rt.host_gateway()),
        "-e".into(),
        format!("AI_POD_PROJECT_ID={}", project_id),
        "-e".into(),
        format!("AI_POD_API_KEY={}", api_key),
        "-e".into(),
        format!("AI_POD_SESSION_ID={}", session_id),
        "-e".into(),
        format!("AI_POD_SERVER_URL={}", rt.server_url()),
        "-e".into(),
        format!(
            "OPENCODE_CONFIG_CONTENT={}",
            opencode_config_content(
                &rt.server_url(),
                api_key,
                &session_id,
                playwright.then(|| rt.host_gateway()),
            )
        ),
    ]);
    run_args.extend([
        "--entrypoint".into(),
        "sh".into(),
        image.into(),
        "-c".into(),
        MAIN_BOOTSTRAP.into(),
        "ai-pod-init".into(),
        script_path.clone(),
        command.into(),
    ]);
    run_args.extend_from_slice(args);

    let mut run_cmd = rt.command();
    run_cmd.args(&run_args);
    if let Some(s) = &sealed {
        // Values go on the runtime's environment; run_args already carries the names.
        let _ = s.apply(&mut run_cmd);
    }
    let status = execute_main_container(
        rt,
        run_cmd,
        &container_name,
        config,
        playwright,
        &script_path,
        &user,
    )?;

    crate::service::cleanup_services_for_session(rt, &session_id);
    crate::egress::cleanup_for_session(rt, &session_id);
    let _ = std::fs::remove_file(config.session_state_file(&session_id));

    if !status.success() {
        anyhow::bail!("Command exited with non-zero status");
    }

    Ok(())
}

/// The per-workspace service network by default; with egress filtering
/// configured, the internal filtered network behind a per-session gateway.
fn session_network(
    rt: &ContainerRuntime,
    config: &AppConfig,
    workspace: &Path,
    session_id: &str,
    egress: Option<&crate::egress::EgressConfig>,
    playwright: bool,
    publish: &[String],
) -> Result<crate::egress::EgressSession> {
    let Some(egress) = egress else {
        return Ok(crate::egress::EgressSession {
            network: crate::service::ensure_service_network(rt, workspace)?,
            add_host_arg: rt.add_host_arg(),
            env: Vec::new(),
        });
    };

    eprintln!(
        "{} network access filtered {}",
        "Egress:".blue().bold(),
        egress.describe()
    );
    if !publish.is_empty() {
        eprintln!(
            "{} published ports may not be reachable: the container is on an internal network.",
            "Warning:".yellow().bold()
        );
    }
    let session = crate::egress::start(rt, workspace, session_id, egress, playwright)?;
    // Let the server put this session's service containers on the same
    // internal network.
    crate::config::SessionState {
        workspace: Some(workspace_hash(workspace)),
        runtime: rt.kind,
        network: Some(session.network.clone()),
    }
    .save(config, session_id)?;
    Ok(session)
}

#[cfg(test)]
mod passthrough_tests {
    use super::passthrough_env_args_from;

    fn set(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |n| names.contains(&n)
    }

    #[test]
    fn passes_names_only_for_set_vars() {
        let args = passthrough_env_args_from(
            "GITHUB_TOKEN, NPM_TOKEN MISSING",
            set(&["GITHUB_TOKEN", "NPM_TOKEN"]),
        );
        assert_eq!(args, vec!["-e", "GITHUB_TOKEN", "-e", "NPM_TOKEN"]);
        assert!(
            args.iter().all(|a| !a.contains('=')),
            "values must never reach argv"
        );
    }

    #[test]
    fn never_passes_sops_key_material() {
        let args = passthrough_env_args_from(
            "SOPS_AGE_KEY SOPS_AGE_KEY_FILE GITHUB_TOKEN",
            set(&["SOPS_AGE_KEY", "SOPS_AGE_KEY_FILE", "GITHUB_TOKEN"]),
        );
        assert_eq!(args, vec!["-e", "GITHUB_TOKEN"]);
    }

    #[test]
    fn skips_invalid_duplicate_and_empty_entries() {
        let args = passthrough_env_args_from(",,A=B, 1BAD, OK, OK,", set(&["OK", "A=B", "1BAD"]));
        assert_eq!(args, vec!["-e", "OK"]);
    }

    #[test]
    fn empty_list_yields_nothing() {
        assert!(passthrough_env_args_from("", set(&["X"])).is_empty());
    }
}
