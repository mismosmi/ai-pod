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
}

#[derive(Subcommand)]
pub enum Command {
    /// Build the container image only
    Build,

    /// Start the shared MCP server on port 7822
    Serve,

    /// Create ai-pod.Dockerfile in the workspace for editing
    Init {
        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    /// Attach to a running claude container session
    Attach,

    /// List all claude containers
    List,

    /// Remove the container for current/specified workspace
    Clean {
        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    /// Run a command in the container, overriding the default
    Run {
        /// Command to run (e.g. bash, claude)
        command: String,

        /// Arguments to pass to the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Manage the whitelist of always-allowed commands for a workspace
    Allowed {
        #[command(subcommand)]
        action: AllowedAction,
    },
}

#[derive(Subcommand)]
pub enum AllowedAction {
    /// List whitelisted commands
    List {
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Add a command to the whitelist
    Add {
        command: String,
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Remove a command from the whitelist
    Remove {
        command: String,
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
}
