use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "claude-container", about = "Run Claude Code inside Podman containers")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Skip credential file scanning
    #[arg(long)]
    pub no_credential_check: bool,

    /// Force image rebuild
    #[arg(long)]
    pub rebuild: bool,

    /// Override workspace directory (default: cwd)
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Notification server port
    #[arg(long, default_value = "9876")]
    pub notify_port: u16,
}

#[derive(Subcommand)]
pub enum Command {
    /// Build the container image only
    Build,

    /// Run the notification server (internal use)
    ServeNotifications,

    /// Stop the notification daemon
    StopServer,

    /// Show notification daemon status
    ServerStatus,

    /// Create ai-pod.Dockerfile in the workspace for editing
    Init {
        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    /// List all claude containers
    List,

    /// Remove the container for current/specified workspace
    Clean {
        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
}
