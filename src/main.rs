use ai_pod::{
    cli, commands_cli, config, container, credentials, env_files_cli, image, mount_cli, runtime,
    server, update, workspace,
};

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::Path;

use cli::{AllowedAction, Cli, Command, CommandsAction, EnvFilesAction, MountAction};
use config::AppConfig;
use runtime::ContainerRuntime;

/// Validate a mask target: must be a single top-level directory name, not a
/// hidden dir or path traversal, and must only contain characters that are
/// already legal in a podman/docker volume name suffix.
fn validate_mask_dir(dir: &str) -> Result<()> {
    if dir.is_empty() {
        anyhow::bail!("Directory name must not be empty");
    }
    if dir == "." || dir == ".." {
        anyhow::bail!("Directory name must not be '.' or '..'");
    }
    if dir.starts_with('.') {
        anyhow::bail!("Directory name must not start with '.'");
    }
    if dir.contains('/') || dir.contains('\\') {
        anyhow::bail!("Directory name must be a single top-level segment (no slashes)");
    }
    if !dir
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        anyhow::bail!(
            "Directory name may only contain ASCII letters, digits, '_', '-' or '.'"
        );
    }
    Ok(())
}

fn resolve_workspace(workdir: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match workdir {
        Some(p) => std::fs::canonicalize(p).context("Invalid workspace path"),
        None => std::env::current_dir().context("Failed to get current directory"),
    }
}

fn resolve_agent(agent: Option<cli::Agent>) -> Result<cli::Agent> {
    match agent {
        Some(a) => Ok(a),
        None => {
            let items = &["Claude", "OpenCode"];
            let sel = dialoguer::Select::new()
                .with_prompt("Select agent")
                .items(items)
                .default(0)
                .interact()
                .context("Selection cancelled")?;
            Ok(match sel {
                0 => cli::Agent::Claude,
                _ => cli::Agent::Opencode,
            })
        }
    }
}

fn resolve_base_image(agent: &cli::Agent, image: Option<cli::BaseImage>) -> Result<cli::BaseImage> {
    if let Some(ref i) = image {
        if matches!(agent, cli::Agent::Opencode) && matches!(i, cli::BaseImage::Alpine) {
            anyhow::bail!("opencode is not supported on Alpine (glibc-linked binary incompatible with musl). Use ubuntu, node, rust, or python.");
        }
        return Ok(image.unwrap());
    }

    let (items, variants): (&[&str], &[cli::BaseImage]) = match agent {
        cli::Agent::Opencode => (&["Ubuntu", "Node", "Rust", "Python"], &[
            cli::BaseImage::Ubuntu,
            cli::BaseImage::Node,
            cli::BaseImage::Rust,
            cli::BaseImage::Python,
        ]),
        cli::Agent::Claude => (&["Alpine", "Ubuntu", "Node", "Rust", "Python"], &[
            cli::BaseImage::Alpine,
            cli::BaseImage::Ubuntu,
            cli::BaseImage::Node,
            cli::BaseImage::Rust,
            cli::BaseImage::Python,
        ]),
    };

    let sel = dialoguer::Select::new()
        .with_prompt("Select base image")
        .items(items)
        .default(0)
        .interact()
        .context("Selection cancelled")?;

    Ok(variants[sel].clone())
}

struct BaseImageConfig {
    from: &'static str,
    install_packages: &'static str,
    create_user: &'static str,
}

fn base_image_config(image: &cli::BaseImage) -> BaseImageConfig {
    match image {
        cli::BaseImage::Alpine => BaseImageConfig {
            from: "alpine:latest",
            install_packages: "RUN apk add --no-cache curl git vim bash",
            create_user: "RUN adduser -D -h /home/ai-pod ai-pod && chown -R ai-pod /app",
        },
        cli::BaseImage::Ubuntu => BaseImageConfig {
            from: "ubuntu:latest",
            install_packages: "RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git vim && rm -rf /var/lib/apt/lists/*",
            create_user: "RUN useradd -ms /bin/bash ai-pod && chown -R ai-pod /app",
        },
        cli::BaseImage::Node => BaseImageConfig {
            from: "node:lts",
            install_packages: "RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git vim && rm -rf /var/lib/apt/lists/*",
            create_user: "RUN useradd -ms /bin/bash ai-pod && chown -R ai-pod /app",
        },
        cli::BaseImage::Rust => BaseImageConfig {
            from: "rust:latest",
            install_packages: "RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git vim && rm -rf /var/lib/apt/lists/*",
            create_user: "RUN useradd -ms /bin/bash ai-pod && chown -R ai-pod /app",
        },
        cli::BaseImage::Python => BaseImageConfig {
            from: "python:latest",
            install_packages: "RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git vim && rm -rf /var/lib/apt/lists/*",
            create_user: "RUN useradd -ms /bin/bash ai-pod && chown -R ai-pod /app",
        },
    }
}

fn init_project(
    workspace: &Path,
    agent: Option<cli::Agent>,
    image: Option<cli::BaseImage>,
) -> Result<()> {
    let dockerfile = workspace.join(image::DOCKERFILE_NAME);

    if dockerfile.exists() {
        println!(
            "{} {}",
            "Already exists:".yellow(),
            dockerfile.display()
        );
        return Ok(());
    }

    let agent = resolve_agent(agent)?;
    let image = resolve_base_image(&agent, image)?;

    let agent_str = match agent {
        cli::Agent::Claude => "claude",
        cli::Agent::Opencode => "opencode",
    };

    let cfg = base_image_config(&image);
    let extra_commands = if agent == cli::Agent::Opencode { "ENV OPENCODE_YOLO=1" } else { "" };
    let content = include_str!("../templates/Dockerfile")
        .replace("{{BASE_IMAGE}}", cfg.from)
        .replace("{{INSTALL_PACKAGES}}", cfg.install_packages)
        .replace("{{EXTRA_COMMANDS}}", extra_commands)
        .replace("{{CREATE_USER}}", cfg.create_user)
        .replace("{{AGENT}}", agent_str);

    std::fs::write(&dockerfile, content).context("Failed to write ai-pod.Dockerfile")?;

    println!("{} {}", "Created:".green().bold(), dockerfile.display());
    println!("Edit this file to customise your container, then run `ai-pod` to launch.");

    maybe_prompt_gitignore(workspace)?;

    Ok(())
}

/// If `workspace` is a git repo and `.gitignore` doesn't yet exclude `.ai-pod`,
/// ask the user whether to add it. Best-effort: failure to read or write the
/// file does not abort init.
fn clean_stale_sessions(rt: &ContainerRuntime, workspace: &Path) {
    let prefix = workspace::container_prefix(workspace);
    let live = match container::containers_for_prefix(rt, &prefix, true) {
        Ok(names) => names
            .into_iter()
            .filter_map(|n| workspace::session_id_from_container_name(&n))
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    let _ = ai_pod::server::runner::clean_stale_sessions(workspace, &live);
}

fn maybe_prompt_gitignore(workspace: &Path) -> Result<()> {
    if !workspace.join(".git").exists() {
        return Ok(());
    }
    let gitignore = workspace.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    let already_ignored = existing
        .lines()
        .any(|l| matches!(l.trim(), ".ai-pod" | ".ai-pod/"));
    if already_ignored {
        return Ok(());
    }
    let confirm = dialoguer::Confirm::new()
        .with_prompt("Add .ai-pod/ to .gitignore?")
        .default(true)
        .interact()
        .unwrap_or(false);
    if !confirm {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(".ai-pod/\n");
    std::fs::write(&gitignore, content).context("Failed to write .gitignore")?;
    println!("{} .ai-pod/ to {}", "Added:".green().bold(), gitignore.display());
    Ok(())
}

async fn launch_flow(cli: &Cli, rt: &ContainerRuntime) -> Result<()> {
    let config = AppConfig::new()?;
    config.init()?;

    // 1. Resolve workspace
    let workspace = resolve_workspace(&cli.workdir)?;
    println!("{} {}", "Workspace:".blue(), workspace.display());

    // 2. Locate Dockerfile
    let dockerfile = workspace.join(image::DOCKERFILE_NAME);
    if !dockerfile.exists() {
        anyhow::bail!(
            "No {} found in {}.\nRun `ai-pod init` to create one.",
            image::DOCKERFILE_NAME,
            workspace.display()
        );
    }

    // 3. Credential scan
    if !cli.no_credential_check {
        if !credentials::check_credentials(&workspace, &config)? {
            println!("{}", "Aborted.".red());
            return Ok(());
        }
    }

    // 4. Ensure shared server is running (must be up before image build so the
    //    Dockerfile can fetch /install/{agent}.sh from http://{gateway}:7822)
    server::lifecycle::ensure_shared_server(&config).await?;

    // Prune .ai-pod/commands/ entries for sessions whose container is gone.
    clean_stale_sessions(rt, &workspace);

    // 5. Build image if needed
    let image = image::image_name(&workspace);
    image::ensure_image(rt, &dockerfile, &image, cli.rebuild, cli.no_cache)?;

    // Bridge the gap between build completion and the first authenticated
    // request: re-arm the inactivity timer so the server doesn't shut down
    // while we set up state and launch the container.
    server::lifecycle::bump_keep_alive().await;

    // 6. Check server version compatibility
    server::lifecycle::check_server_version().await?;

    // 7. Get or create project state (stable api_key)
    let project_id = workspace::workspace_hash(&workspace);
    let state = server::lifecycle::get_or_create_project_state(&config, &workspace)?;

    // 8. Reload server config so it picks up the updated project file
    server::lifecycle::reload_config().await?;

    // 9. Launch container
    container::launch_container(
        rt,
        &config,
        &workspace,
        cli.rebuild,
        &image,
        &project_id,
        &state.api_key,
    )?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Skip update check for internal/daemon commands
    if !matches!(&cli.command, Some(Command::Serve) | Some(Command::Update)) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            update::check_for_update(),
        )
        .await;
    }

    // Commands that don't need a container runtime
    match &cli.command {
        Some(Command::Init { workdir, agent, image }) => {
            let workspace = resolve_workspace(workdir)?;
            init_project(&workspace, agent.clone(), image.clone())?;
            return Ok(());
        }
        Some(Command::Update) => {
            update::run_update().await?;
            return Ok(());
        }
        Some(Command::EnvFiles { action, workdir }) => {
            let config = AppConfig::new()?;
            config.init()?;
            let ws = workdir.clone().or_else(|| cli.workdir.clone());
            let workspace = resolve_workspace(&ws)?;
            match action {
                None => env_files_cli::run_tui(&config, &workspace)?,
                Some(EnvFilesAction::List) => env_files_cli::run_list(&config, &workspace)?,
                Some(EnvFilesAction::Hide { path }) => {
                    env_files_cli::run_hide(&config, &workspace, path)?
                }
                Some(EnvFilesAction::Unhide { path }) => {
                    env_files_cli::run_unhide(&workspace, path)?
                }
                Some(EnvFilesAction::Ignore { path }) => {
                    env_files_cli::run_ignore(&config, &workspace, path)?
                }
                Some(EnvFilesAction::Unignore { path }) => {
                    env_files_cli::run_unignore(&config, &workspace, path)?
                }
            }
            return Ok(());
        }
        Some(Command::Mount { action }) => {
            let config = AppConfig::new()?;
            config.init()?;
            match action {
                MountAction::List => mount_cli::run_list(&config)?,
                MountAction::Add { spec, writable } => {
                    mount_cli::run_add(&config, spec, *writable)?
                }
                MountAction::Remove { host } => mount_cli::run_remove(&config, host)?,
            }
            return Ok(());
        }
        Some(Command::Allowed { action }) => {
            let config = AppConfig::new()?;
            let action = match action {
                Some(a) => a,
                None => {
                    let workspace = resolve_workspace(&cli.workdir)?;
                    commands_cli::run_allowed_tui(&config, &workspace)?;
                    return Ok(());
                }
            };
            match action {
                AllowedAction::List { workdir } => {
                    let workspace = resolve_workspace(workdir)?;
                    let hash = workspace::workspace_hash(&workspace);
                    let state = server::lifecycle::ProjectState::load(
                        &config.project_state_file(&hash),
                    );
                    for cmd in &state.allowed_commands {
                        println!("{}", cmd);
                    }
                }
                AllowedAction::Add { command, workdir } => {
                    let workspace = resolve_workspace(workdir)?;
                    let hash = workspace::workspace_hash(&workspace);
                    let state_path = config.project_state_file(&hash);
                    let mut state = server::lifecycle::ProjectState::load(&state_path);
                    state.add_allowed(command);
                    state.save(&state_path)?;
                    println!("Added: {}", command);
                }
                AllowedAction::Remove { command, workdir } => {
                    let workspace = resolve_workspace(workdir)?;
                    let hash = workspace::workspace_hash(&workspace);
                    let state_path = config.project_state_file(&hash);
                    let mut state = server::lifecycle::ProjectState::load(&state_path);
                    state.remove_allowed(command);
                    state.save(&state_path)?;
                    println!("Removed: {}", command);
                }
            }
            return Ok(());
        }
        _ => {}
    }

    // Detect container runtime (podman preferred, docker fallback)
    let rt = ContainerRuntime::detect(cli.dry_run)?;

    match &cli.command {
        Some(Command::Build) => {
            let config = AppConfig::new()?;
            config.init()?;
            let workspace = resolve_workspace(&cli.workdir)?;
            let dockerfile = workspace.join(image::DOCKERFILE_NAME);
            if !dockerfile.exists() {
                anyhow::bail!(
                    "No {} found in {}.\nRun `ai-pod init` to create one.",
                    image::DOCKERFILE_NAME,
                    workspace.display()
                );
            }
            server::lifecycle::ensure_shared_server(&config).await?;
            let image = image::image_name(&workspace);
            image::ensure_image(&rt, &dockerfile, &image, cli.rebuild, cli.no_cache)?;
        }
        Some(Command::Serve) => {
            let config = AppConfig::new()?;
            config.init()?;
            server::run_server(server::lifecycle::MCP_PORT, config, rt).await?;
        }
        Some(Command::Attach) => {
            container::attach_container(&rt)?;
        }
        Some(Command::List) => {
            container::list_containers(&rt)?;
        }
        Some(Command::Clean { workdir }) => {
            let config = AppConfig::new()?;
            let ws = workdir.clone().or_else(|| cli.workdir.clone());
            let workspace = resolve_workspace(&ws)?;
            container::clean_container(&rt, &config, &workspace)?;
        }
        Some(Command::Mask { dir, workdir }) => {
            let config = AppConfig::new()?;
            config.init()?;
            let ws = workdir.clone().or_else(|| cli.workdir.clone());
            let workspace = resolve_workspace(&ws)?;
            validate_mask_dir(dir)?;

            let hash = workspace::workspace_hash(&workspace);
            let state_path = config.project_state_file(&hash);
            let mut state = server::lifecycle::ProjectState::load(&state_path);
            if state.is_masked(dir) {
                println!("Already masked: {}", dir);
                return Ok(());
            }
            state.add_masked(dir);
            state.save(&state_path)?;
            println!("{} {}", "Masked:".green().bold(), dir);

            let prefix = workspace::container_prefix(&workspace);
            if !container::containers_for_prefix(&rt, &prefix, true)?.is_empty() {
                println!(
                    "{} a container is running for this workspace; the new mount applies on next launch.",
                    "Note:".yellow().bold()
                );
            }
        }
        Some(Command::Unmask { dir, workdir }) => {
            let config = AppConfig::new()?;
            config.init()?;
            let ws = workdir.clone().or_else(|| cli.workdir.clone());
            let workspace = resolve_workspace(&ws)?;
            validate_mask_dir(dir)?;

            let hash = workspace::workspace_hash(&workspace);
            let state_path = config.project_state_file(&hash);
            let mut state = server::lifecycle::ProjectState::load(&state_path);
            if !state.is_masked(dir) {
                println!("Not masked: {}", dir);
                return Ok(());
            }
            state.remove_masked(dir);
            state.save(&state_path)?;

            let prefix = workspace::container_prefix(&workspace);
            let container_running =
                !container::containers_for_prefix(&rt, &prefix, true)?.is_empty();
            if container_running {
                println!(
                    "{} a container is running for this workspace; the volume will be left in place. Stop the container and re-run `ai-pod unmask {}` (or `ai-pod clean`) to delete its data.",
                    "Note:".yellow().bold(),
                    dir
                );
            } else {
                container::remove_mask_volume(&rt, &workspace, dir)?;
            }
            println!("{} {}", "Unmasked:".green().bold(), dir);
        }
        Some(Command::Run { command, args }) => {
            let config = AppConfig::new()?;
            config.init()?;
            let workspace = resolve_workspace(&cli.workdir)?;
            let dockerfile = workspace.join(image::DOCKERFILE_NAME);
            if !dockerfile.exists() {
                anyhow::bail!(
                    "No {} found in {}.\nRun `ai-pod init` to create one.",
                    image::DOCKERFILE_NAME,
                    workspace.display()
                );
            }
            if !cli.no_credential_check {
                if !credentials::check_credentials(&workspace, &config)? {
                    println!("{}", "Aborted.".red());
                    return Ok(());
                }
            }
            server::lifecycle::ensure_shared_server(&config).await?;
            let image = image::image_name(&workspace);
            image::ensure_image(&rt, &dockerfile, &image, cli.rebuild, cli.no_cache)?;
            server::lifecycle::bump_keep_alive().await;
            server::lifecycle::check_server_version().await?;
            let project_id = workspace::workspace_hash(&workspace);
            let state = server::lifecycle::get_or_create_project_state(&config, &workspace)?;
            server::lifecycle::reload_config().await?;

            container::run_in_container(
                &rt,
                &config,
                &workspace,
                &image,
                &project_id,
                &state.api_key,
                command,
                args,
            )?;
        }
        Some(Command::Commands { action }) => {
            let config = AppConfig::new()?;
            let workspace = resolve_workspace(&cli.workdir)?;
            match action {
                None => commands_cli::run_tui(&config, &workspace).await?,
                Some(CommandsAction::List { all }) => {
                    commands_cli::run_list(&config, &workspace, *all).await?
                }
                Some(CommandsAction::Run { command }) => {
                    if command.is_empty() {
                        anyhow::bail!("ai-pod commands run <shell command>");
                    }
                    let cmd = command.join(" ");
                    commands_cli::run_run(&config, &workspace, &cmd).await?;
                }
                Some(CommandsAction::Kill { command_id, session }) => {
                    commands_cli::run_kill(&config, &workspace, session.as_deref(), command_id)
                        .await?;
                }
                Some(CommandsAction::Logs { command_id, session }) => {
                    commands_cli::run_logs(&config, &workspace, session.as_deref(), command_id)
                        .await?;
                }
            }
        }
        None => {
            launch_flow(&cli, &rt).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_mask_dir;

    #[test]
    fn accepts_typical_top_level_names() {
        for ok in ["node_modules", "target", "dist", "build", "out", "vendor"] {
            assert!(validate_mask_dir(ok).is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn rejects_empty_and_dots() {
        assert!(validate_mask_dir("").is_err());
        assert!(validate_mask_dir(".").is_err());
        assert!(validate_mask_dir("..").is_err());
    }

    #[test]
    fn rejects_hidden_dirs() {
        assert!(validate_mask_dir(".git").is_err());
        assert!(validate_mask_dir(".cache").is_err());
    }

    #[test]
    fn rejects_path_traversal_and_slashes() {
        assert!(validate_mask_dir("../etc").is_err());
        assert!(validate_mask_dir("foo/bar").is_err());
        assert!(validate_mask_dir("foo\\bar").is_err());
    }

    #[test]
    fn rejects_disallowed_characters() {
        assert!(validate_mask_dir("foo bar").is_err());
        assert!(validate_mask_dir("foo:bar").is_err());
        assert!(validate_mask_dir("foo*bar").is_err());
    }
}
