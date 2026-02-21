mod cli;
mod config;
mod container;
mod credentials;
mod image;
mod server;
mod update;

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::Path;

use cli::{Cli, Command};
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

fn launch_flow(cli: &Cli) -> Result<()> {
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
        if !credentials::check_credentials(&workspace)? {
            println!("{}", "Aborted.".red());
            return Ok(());
        }
    }

    // 4. Build image if needed
    let image = image::image_name(&workspace);
    image::ensure_image(&config, &dockerfile, &image, cli.rebuild)?;

    // 5. Ensure notification server
    server::lifecycle::ensure_server(&config.pid_file, &config.log_file, cli.notify_port)?;

    // 6. Generate settings + launch container
    container::launch_container(&config, &workspace, cli.notify_port)?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Skip update check for internal/daemon commands
    if !matches!(&cli.command, Some(Command::ServeNotifications)) {
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
        Some(Command::ServeNotifications) => {
            server::run_server(cli.notify_port).await?;
        }
        Some(Command::StopServer) => {
            let config = AppConfig::new()?;
            server::lifecycle::stop_server(&config.pid_file)?;
        }
        Some(Command::ServerStatus) => {
            let config = AppConfig::new()?;
            server::lifecycle::print_status(&config.pid_file, cli.notify_port);
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
                if !credentials::check_credentials(&workspace)? {
                    println!("{}", "Aborted.".red());
                    return Ok(());
                }
            }
            let image = image::image_name(&workspace);
            image::ensure_image(&config, &dockerfile, &image, cli.rebuild)?;
            server::lifecycle::ensure_server(&config.pid_file, &config.log_file, cli.notify_port)?;
            container::run_in_container(&config, &workspace, cli.notify_port, command, args)?;
        }
        None => {
            launch_flow(&cli)?;
        }
    }

    Ok(())
}
