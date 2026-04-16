use ai_pod::{cli, config, container, credentials, daemons, image, runtime, server, update, workspace};

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::Path;

use cli::{AllowedAction, Cli, Command};
use config::AppConfig;
use runtime::ContainerRuntime;

fn resolve_workspace(workdir: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match workdir {
        Some(p) => std::fs::canonicalize(p).context("Invalid workspace path"),
        None => std::env::current_dir().context("Failed to get current directory"),
    }
}

fn init_project(workspace: &Path, agent: &str) -> Result<()> {
    let dockerfile = workspace.join(image::DOCKERFILE_NAME);

    if dockerfile.exists() {
        println!(
            "{} {}",
            "Already exists:".yellow(),
            dockerfile.display()
        );
        return Ok(());
    }

    let default = include_str!("../templates/Dockerfile").replace("{{AGENT}}", agent);
    std::fs::write(&dockerfile, default).context("Failed to write ai-pod.Dockerfile")?;

    println!("{} {}", "Created:".green().bold(), dockerfile.display());
    println!("Edit this file to customise your Claude container, then run `ai-pod` to launch.");

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
    //    Dockerfile can fetch host-tools from http://{gateway}:7822/host-tools)
    server::lifecycle::ensure_shared_server(&config)?;

    // 5. Build image if needed
    let image = image::image_name(&workspace);
    image::ensure_image(rt, &dockerfile, &image, cli.rebuild, cli.no_cache)?;

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
        &cli.userns,
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
        Some(Command::Init { workdir, agent }) => {
            let workspace = resolve_workspace(workdir)?;
            let agent_str = match agent {
                cli::Agent::Claude => "claude",
                cli::Agent::Opencode => "opencode",
            };
            init_project(&workspace, agent_str)?;
            return Ok(());
        }
        Some(Command::Update) => {
            update::run_update().await?;
            return Ok(());
        }
        Some(Command::Allowed { action }) => {
            let config = AppConfig::new()?;
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
            server::lifecycle::ensure_shared_server(&config)?;
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
            let workspace = resolve_workspace(workdir)?;
            container::clean_container(&rt, &workspace)?;
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
            server::lifecycle::ensure_shared_server(&config)?;
            let image = image::image_name(&workspace);
            image::ensure_image(&rt, &dockerfile, &image, cli.rebuild, cli.no_cache)?;
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
                &cli.userns,
            )?;
        }
        Some(Command::Daemons) => {
            let config = AppConfig::new()?;
            let workspace = resolve_workspace(&cli.workdir)?;
            daemons::run_daemons(&config, &workspace).await?;
        }
        None => {
            launch_flow(&cli, &rt).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}
