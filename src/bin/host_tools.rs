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

fn main() {
    let cli = Cli::parse();

    let project_id = std::env::var("AI_POD_PROJECT_ID").unwrap_or_default();
    let api_key = std::env::var("AI_POD_API_KEY").unwrap_or_default();
    let server_url = std::env::var("AI_POD_SERVER_URL")
        .unwrap_or_else(|_| "http://host.containers.internal:7822".to_string());

    match cli.command {
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
