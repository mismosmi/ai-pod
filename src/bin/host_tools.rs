use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};

#[derive(Parser)]
#[command(name = "host-tools", about = "Interact with the host machine from inside an ai-pod container")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install all AI coding agents system-wide (for use in Dockerfiles)
    Install,
    /// Run a shell command on the host machine
    RunCommand {
        /// List previously approved commands
        #[arg(long)]
        list: bool,
        /// The command and arguments to run
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Send a desktop notification to the host user
    NotifyUser {
        /// The notification message
        message: String,
    },
    /// Manage long-running background processes on the host
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start a background daemon process
    Start {
        /// The shell command to run as a daemon
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Stop a running daemon by ID
    Stop {
        /// Daemon ID to stop
        daemon_id: String,
    },
    /// Stop all running daemons for this project
    StopAll,
    /// List all daemons for this project
    List,
    /// Print the log output from a daemon and exit
    Output {
        /// Daemon ID
        daemon_id: String,
    },
    /// Show status of a daemon
    Status {
        /// Daemon ID
        daemon_id: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
enum Message {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

#[derive(Serialize)]
struct RunCommandRequest {
    project_id: String,
    command: String,
}

#[derive(Serialize)]
struct ListCommandsRequest {
    project_id: String,
}

#[derive(Serialize)]
struct NotifyUserRequest {
    project_id: String,
    message: String,
}

#[derive(Serialize)]
struct StartDaemonRequest {
    project_id: String,
    command: String,
}

#[derive(Serialize)]
struct StopDaemonRequest {
    project_id: String,
    daemon_id: String,
}

#[derive(Serialize)]
struct StopAllDaemonsRequest {
    project_id: String,
}

#[derive(Serialize)]
struct ListDaemonsRequest {
    project_id: String,
}

#[derive(Serialize)]
struct DaemonStatusRequest {
    project_id: String,
    daemon_id: String,
}

#[derive(Serialize)]
struct DaemonOutputRequest {
    project_id: String,
    daemon_id: String,
}

fn main() {
    let cli = Cli::parse();

    let project_id = std::env::var("AI_POD_PROJECT_ID").unwrap_or_default();
    let api_key = std::env::var("AI_POD_API_KEY").unwrap_or_default();
    let server_url = std::env::var("AI_POD_SERVER_URL")
        .unwrap_or_else(|_| "http://host.containers.internal:7822".to_string());

    match cli.command {
        Command::Install => {
            install_all_agents();
        }
        Command::RunCommand { list, command } => {
            if list {
                run_list_commands(&project_id, &api_key, &server_url);
            } else {
                let cmd_str = command.join(" ");
                run_command(&project_id, &api_key, &server_url, &cmd_str);
            }
        }
        Command::NotifyUser { message } => {
            notify_user(&project_id, &api_key, &server_url, &message);
        }
        Command::Daemon { action } => match action {
            DaemonAction::Start { command } => {
                let cmd_str = command.join(" ");
                daemon_start(&project_id, &api_key, &server_url, &cmd_str);
            }
            DaemonAction::Stop { daemon_id } => {
                daemon_stop(&project_id, &api_key, &server_url, &daemon_id);
            }
            DaemonAction::StopAll => {
                daemon_stop_all(&project_id, &api_key, &server_url);
            }
            DaemonAction::List => {
                daemon_list(&project_id, &api_key, &server_url);
            }
            DaemonAction::Output { daemon_id } => {
                daemon_output(&project_id, &api_key, &server_url, &daemon_id);
            }
            DaemonAction::Status { daemon_id } => {
                daemon_status(&project_id, &api_key, &server_url, &daemon_id);
            }
        },
    }
}

fn run_command(project_id: &str, api_key: &str, server_url: &str, command: &str) {
    let url = format!("{}/run_command", server_url);
    let body = RunCommandRequest {
        project_id: project_id.to_string(),
        command: command.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let reader = BufReader::new(response);
    let mut exit_code: i32 = 0;
    for line in reader.lines() {
        match line {
            Ok(l) if l.is_empty() => continue,
            Ok(l) => match serde_json::from_str::<Message>(&l) {
                Ok(Message::Stdout(s)) => print!("{}", s),
                Ok(Message::Stderr(s)) => eprint!("{}", s),
                Ok(Message::Exit(code)) => {
                    exit_code = code;
                    break;
                }
                Err(_) => {}
            },
            Err(e) => {
                eprintln!("Error reading stream: {}", e);
                std::process::exit(1);
            }
        }
    }
    // BufReader/Response drops here → graceful TCP FIN instead of RST
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(exit_code);
}

fn run_list_commands(project_id: &str, api_key: &str, server_url: &str) {
    let url = format!("{}/list_allowed_commands", server_url);
    let body = ListCommandsRequest {
        project_id: project_id.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let result: serde_json::Value = match response.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing response: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(commands) = result["commands"].as_array() {
        for cmd in commands {
            println!("{}", cmd.as_str().unwrap_or(""));
        }
    }
}

fn notify_user(project_id: &str, api_key: &str, server_url: &str, message: &str) {
    let url = format!("{}/notify_user", server_url);
    let body = NotifyUserRequest {
        project_id: project_id.to_string(),
        message: message.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }
}

fn daemon_start(project_id: &str, api_key: &str, server_url: &str, command: &str) {
    let url = format!("{}/daemon/start", server_url);
    let body = StartDaemonRequest {
        project_id: project_id.to_string(),
        command: command.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let result: serde_json::Value = match response.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing response: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(id) = result["daemon_id"].as_str() {
        println!("{}", id);
    }
}

fn daemon_stop(project_id: &str, api_key: &str, server_url: &str, daemon_id: &str) {
    let url = format!("{}/daemon/stop", server_url);
    let body = StopDaemonRequest {
        project_id: project_id.to_string(),
        daemon_id: daemon_id.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }
}

fn daemon_stop_all(project_id: &str, api_key: &str, server_url: &str) {
    let url = format!("{}/daemon/stop-all", server_url);
    let body = StopAllDaemonsRequest {
        project_id: project_id.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let result: serde_json::Value = match response.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing response: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(n) = result["stopped"].as_u64() {
        println!("Stopped {} daemon(s)", n);
    }
}

fn daemon_list(project_id: &str, api_key: &str, server_url: &str) {
    let url = format!("{}/daemon/list", server_url);
    let body = ListDaemonsRequest {
        project_id: project_id.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let result: serde_json::Value = match response.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing response: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(daemons) = result["daemons"].as_array() {
        if daemons.is_empty() {
            println!("No daemons");
            return;
        }
        println!("{:<14} {:<12} {:<20} {}", "ID", "STATUS", "STARTED", "COMMAND");
        println!("{}", "-".repeat(80));
        for d in daemons {
            let id = d["id"].as_str().unwrap_or("");
            let status = format_status(&d["status"]);
            let started = d["started_at"].as_u64().unwrap_or(0);
            let started_str = format_unix_time(started);
            let command = d["command"].as_str().unwrap_or("");
            let command_short = if command.len() > 32 {
                format!("{}...", &command[..29])
            } else {
                command.to_string()
            };
            println!("{:<14} {:<12} {:<20} {}", id, status, started_str, command_short);
        }
    }
}

fn daemon_output(project_id: &str, api_key: &str, server_url: &str, daemon_id: &str) {
    let url = format!("{}/daemon/output", server_url);
    let body = DaemonOutputRequest {
        project_id: project_id.to_string(),
        daemon_id: daemon_id.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let reader = BufReader::new(response);
    let mut exit_code: i32 = 0;
    for line in reader.lines() {
        match line {
            Ok(l) if l.is_empty() => continue,
            Ok(l) => match serde_json::from_str::<Message>(&l) {
                Ok(Message::Stdout(s)) => print!("{}", s),
                Ok(Message::Stderr(s)) => eprint!("{}", s),
                Ok(Message::Exit(code)) => {
                    exit_code = code;
                    break;
                }
                Err(_) => {}
            },
            Err(e) => {
                eprintln!("Error reading stream: {}", e);
                std::process::exit(1);
            }
        }
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(exit_code);
}

fn daemon_status(project_id: &str, api_key: &str, server_url: &str, daemon_id: &str) {
    let url = format!("{}/daemon/status", server_url);
    let body = DaemonStatusRequest {
        project_id: project_id.to_string(),
        daemon_id: daemon_id.to_string(),
    };

    let client = reqwest::blocking::Client::new();
    let response = match client
        .post(&url)
        .header("X-Api-Key", api_key)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        eprintln!("Error {}: {}", status, body);
        std::process::exit(1);
    }

    let result: serde_json::Value = match response.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing response: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(d) = result.get("daemon") {
        println!("ID:      {}", d["id"].as_str().unwrap_or(""));
        println!("Command: {}", d["command"].as_str().unwrap_or(""));
        println!("Status:  {}", format_status(&d["status"]));
        println!("Started: {}", format_unix_time(d["started_at"].as_u64().unwrap_or(0)));
        println!("Log:     {}", d["log_path"].as_str().unwrap_or(""));
    }
}

fn format_status(status: &serde_json::Value) -> String {
    if let Some(s) = status.as_str() {
        return s.to_string();
    }
    if let Some(obj) = status.as_object() {
        if obj.contains_key("finished") {
            let code = obj["finished"]["exit_code"].as_i64().unwrap_or(0);
            return format!("finished({})", code);
        }
    }
    // Handle {"type": "finished", "exit_code": N} or similar shapes
    if status["exit_code"].is_number() {
        let code = status["exit_code"].as_i64().unwrap_or(0);
        return format!("finished({})", code);
    }
    serde_json::to_string(status).unwrap_or_else(|_| "unknown".to_string())
}

const CLAUDE_INSTALL_SCRIPT: &[u8] =
    include_bytes!("../install_scripts/claude_install.sh");
const OPENCODE_INSTALL_SCRIPT: &[u8] =
    include_bytes!("../install_scripts/opencode_install.sh");

fn install_all_agents() {
    let agents: &[(&str, &[u8])] = &[
        ("claude", CLAUDE_INSTALL_SCRIPT),
        ("opencode", OPENCODE_INSTALL_SCRIPT),
    ];
    for (name, script) in agents {
        install_stub(name, script);
    }
}

/// Write an install script to /usr/local/bin/<name> that runs the embedded
/// installer on first invocation, then hands off to the real binary.
fn install_stub(name: &str, install_script: &[u8]) {
    // Wrap the embedded installer in a one-shot launcher: run it once, then
    // exec the real binary that the installer placed in ~/.local/bin.
    let inner = std::str::from_utf8(install_script).unwrap_or_default();
    let script = format!(
        "#!/bin/sh\nset -e\n{inner}\nexec \"$HOME/.local/bin/{name}\" \"$@\"\n",
        inner = inner,
        name = name,
    );

    let path = format!("/usr/local/bin/{}", name);
    if let Err(e) = std::fs::write(&path, script.as_bytes()) {
        eprintln!("Failed to write {}: {}", path, e);
        std::process::exit(1);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)) {
            eprintln!("Failed to chmod {}: {}", path, e);
            std::process::exit(1);
        }
    }

    eprintln!("Installed {} at {}", name, path);
}

fn format_unix_time(secs: u64) -> String {
    if secs == 0 {
        return "unknown".to_string();
    }
    // Simple formatting: just show the unix timestamp for now
    // (avoids pulling in a time library)
    use std::time::{Duration, UNIX_EPOCH};
    let _ = UNIX_EPOCH + Duration::from_secs(secs);
    // Print as a rough human-readable relative time
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(secs);
    if age < 60 {
        format!("{}s ago", age)
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else if age < 86400 {
        format!("{}h ago", age / 3600)
    } else {
        format!("{}d ago", age / 86400)
    }
}
