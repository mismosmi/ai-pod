use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(ValueEnum, Clone, Debug, PartialEq)]
pub enum Agent {
    Claude,
    Opencode,
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
pub enum BaseImage {
    Alpine,
    Ubuntu,
    Node,
    Rust,
    Python,
}

#[derive(Parser)]
#[command(name = "ai-pod", about = "Run Claude Code inside Podman containers", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Skip credential file scanning
    #[arg(long)]
    pub no_credential_check: bool,

    /// Force image rebuild
    #[arg(long)]
    pub rebuild: bool,

    /// Build image without Docker/Podman layer cache
    #[arg(long)]
    pub no_cache: bool,

    /// Override workspace directory (default: cwd)
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Print podman/docker commands instead of executing them
    #[arg(long)]
    pub dry_run: bool,
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

        /// Agent to start in the container (interactive if omitted)
        #[arg(long, value_enum)]
        agent: Option<Agent>,

        /// Base image for the container (interactive if omitted)
        #[arg(long, value_enum)]
        image: Option<BaseImage>,
    },

    /// Attach to a running ai-pod container session
    Attach,

    /// List all ai-pod containers
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

    /// View and manage host commands for the current workspace
    Commands {
        #[command(subcommand)]
        action: Option<CommandsAction>,
    },

    /// Manage the whitelist of always-allowed commands for a workspace.
    /// Run with no subcommand to open an interactive TUI.
    Allowed {
        #[command(subcommand)]
        action: Option<AllowedAction>,
    },

    /// Manage sensitive files in the workspace — hide them from the AI,
    /// expose them, or silence startup warnings.
    /// Run with no subcommand to open an interactive TUI.
    EnvFiles {
        #[command(subcommand)]
        action: Option<EnvFilesAction>,

        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    /// Shadow-mount a top-level workspace directory with an isolated per-workspace volume.
    /// The masked directory inside the container is backed by a named volume instead of
    /// the host's workspace, so container-only artifacts (e.g. node_modules) don't leak out.
    Mask {
        /// Top-level directory name under /app (e.g. node_modules, target)
        dir: String,
        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    /// Remove a directory from the mask list and delete its shadow volume.
    Unmask {
        /// Top-level directory name to stop masking
        dir: String,
        /// Workspace path (default: cwd)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },

    /// Update ai-pod to the latest release
    Update,
}

#[derive(Subcommand)]
pub enum CommandsAction {
    /// Plain list (one row per command)
    List {
        /// Show commands across all sessions for this workspace.
        #[arg(long)]
        all: bool,
    },
    /// Run a host command (same approval flow as MCP `run_command`)
    Run {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Stop a running command
    Kill {
        command_id: String,
        /// Session id (optional; resolved from list if omitted)
        #[arg(long)]
        session: Option<String>,
    },
    /// Print stdout/stderr/exit for a command
    Logs {
        command_id: String,
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum EnvFilesAction {
    /// Print a list of all detected sensitive files with their status
    List,
    /// Move a workspace file into ~/.env-files/<slug>/ and replace with a symlink
    Hide {
        /// Path relative to the workspace
        path: String,
    },
    /// Restore a hidden file back into the workspace
    Unhide {
        /// Path relative to the workspace
        path: String,
    },
    /// Suppress future startup warnings for a workspace file (keeps it readable by the AI)
    Ignore {
        /// Path relative to the workspace
        path: String,
    },
    /// Re-enable startup warnings for a previously ignored file
    Unignore {
        /// Path relative to the workspace
        path: String,
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
