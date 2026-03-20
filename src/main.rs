mod cli;
mod config;
mod container;
mod credentials;
mod daemons;
mod image;
mod server;
mod update;
mod workspace;

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::Path;

use cli::{AllowedAction, Cli, Command};
use config::AppConfig;

fn resolve_workspace(workdir: &Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
    match workdir {
        Some(p) => std::fs::canonicalize(p).context("Invalid workspace path"),
        None => std::env::current_dir().context("Failed to get current directory"),
    }
}

fn init_project(workspace: &Path) -> Result<()> {
    let dockerfile = workspace.join(image::DOCKERFILE_NAME);

    if dockerfile.exists() {
        println!(
            "{} {}",
            "Already exists:".yellow(),
            dockerfile.display()
        );
        return Ok(());
    }

    let default = include_str!("../claude.Dockerfile");
    std::fs::write(&dockerfile, default).context("Failed to write ai-pod.Dockerfile")?;

    println!("{} {}", "Created:".green().bold(), dockerfile.display());
    println!("Edit this file to customise your Claude container, then run `ai-pod` to launch.");

    Ok(())
}

async fn launch_flow(cli: &Cli) -> Result<()> {
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

    // 4. Build image if needed
    let image = image::image_name(&workspace);
    image::ensure_image(&config, &dockerfile, &image, cli.rebuild)?;

    // 5. Ensure shared server is running
    server::lifecycle::ensure_shared_server(&config)?;

    // 6. Check server version compatibility
    server::lifecycle::check_server_version().await?;

    // 7. Get or create project state (stable api_key)
    let project_id = workspace::workspace_hash(&workspace);
    let state = server::lifecycle::get_or_create_project_state(&config, &workspace)?;

    // 8. Reload server config so it picks up the updated project file
    server::lifecycle::reload_config().await?;

    // 9. Launch container
    container::launch_container(
        &config,
        &workspace,
        cli.rebuild,
        &image,
        &project_id,
        &state.api_key,
    )
    .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Skip update check for internal/daemon commands
    if !matches!(&cli.command, Some(Command::Serve)) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            update::check_for_update(),
        )
        .await;
    }

    match &cli.command {
        Some(Command::Init { workdir }) => {
            let workspace = resolve_workspace(workdir)?;
            init_project(&workspace)?;
        }
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
            let image = image::image_name(&workspace);
            image::ensure_image(&config, &dockerfile, &image, cli.rebuild)?;
        }
        Some(Command::Serve) => {
            let config = AppConfig::new()?;
            config.init()?;
            server::run_server(server::lifecycle::MCP_PORT, config).await?;
        }
        Some(Command::Attach) => {
            container::attach_container()?;
        }
        Some(Command::List) => {
            container::list_containers()?;
        }
        Some(Command::Clean { workdir }) => {
            let workspace = resolve_workspace(workdir)?;
            container::clean_container(&workspace)?;
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
            let image = image::image_name(&workspace);
            image::ensure_image(&config, &dockerfile, &image, cli.rebuild)?;

            server::lifecycle::ensure_shared_server(&config)?;
            server::lifecycle::check_server_version().await?;
            let project_id = workspace::workspace_hash(&workspace);
            let state = server::lifecycle::get_or_create_project_state(&config, &workspace)?;
            server::lifecycle::reload_config().await?;

            container::run_in_container(
                &config,
                &workspace,
                &image,
                &project_id,
                &state.api_key,
                command,
                args,
            )
            .await?;
        }
        Some(Command::Daemons) => {
            let config = AppConfig::new()?;
            let workspace = resolve_workspace(&cli.workdir)?;
            daemons::run_daemons(&config, &workspace).await?;
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
        }
        None => {
            launch_flow(&cli).await?;
        }
    }

    Ok(())
}
