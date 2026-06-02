pub mod cli;
pub mod commands_cli;
pub mod config;
pub mod container;
pub mod credentials;
pub mod env_files_cli;
pub mod image;
pub mod mount_cli;
pub mod runtime;
pub mod server;
pub mod service;
pub mod services_cli;
pub mod update;
pub mod workspace;

/// Returns true if stdin is connected to a terminal. When false, ai-pod
/// is being driven by another program (e.g. an IDE speaking ACP over
/// stdio); status output must stay on stderr and the container's stdio
/// must not get a pseudo-TTY allocated.
pub fn is_stdin_tty() -> bool {
    // Safety: isatty just reads the fd's terminal state, no aliasing concerns.
    unsafe { libc::isatty(0) == 1 }
}
