//! Host-side `ai-pod commands` subcommand: list, run, kill, logs, plus a
//! minimal ratatui TUI for interactive inspection.

use anyhow::{Context, Result};
use colored::Colorize;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::server::lifecycle::{MCP_PORT, ProjectState};
use crate::workspace::workspace_hash;

const SERVER_BASE: &str = "http://127.0.0.1";

#[derive(Serialize)]
struct RunReq<'a> {
    project_id: &'a str,
    command: &'a str,
    session_id: Option<&'a str>,
}

#[derive(Serialize)]
struct StopReq<'a> {
    project_id: &'a str,
    session_id: &'a str,
    command_id: &'a str,
}

#[derive(Serialize)]
struct ListReq<'a> {
    project_id: &'a str,
    session_id: Option<&'a str>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct CommandSummary {
    pub command_id: String,
    pub session_id: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub started_at: u64,
}

#[derive(Deserialize)]
struct ListResp {
    commands: Vec<CommandSummary>,
}

#[derive(Deserialize)]
struct RunResp {
    command_id: String,
    session_id: String,
    status: String,
    exit_code: Option<i32>,
    stdout_tail: String,
    stderr_tail: String,
}

#[derive(Deserialize)]
struct StopResp {
    stopped: bool,
}

struct Ctx {
    project_id: String,
    api_key: String,
    workspace: PathBuf,
}

fn load_ctx(config: &AppConfig, workspace: &Path) -> Result<Ctx> {
    let project_id = workspace_hash(workspace);
    let state = ProjectState::load(&config.project_state_file(&project_id));
    if state.api_key.is_empty() {
        anyhow::bail!(
            "No project state for this workspace. Launch `ai-pod` first to initialise it."
        );
    }
    Ok(Ctx {
        project_id,
        api_key: state.api_key,
        workspace: workspace.to_path_buf(),
    })
}

fn url(path: &str) -> String {
    format!("{}:{}{}", SERVER_BASE, MCP_PORT, path)
}

async fn fetch_list(ctx: &Ctx, all: bool) -> Result<Vec<CommandSummary>> {
    let client = reqwest::Client::new();
    let session_id = if all {
        None
    } else {
        std::env::var("AI_POD_SESSION_ID").ok()
    };
    let req = ListReq {
        project_id: &ctx.project_id,
        session_id: session_id.as_deref(),
    };
    let resp = client
        .post(url("/commands/list"))
        .header("X-Api-Key", &ctx.api_key)
        .json(&req)
        .send()
        .await
        .context("Failed to reach the ai-pod server. Is it running?")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Server error {status}: {body}");
    }
    let parsed: ListResp = resp.json().await.context("Invalid /commands/list response")?;
    Ok(parsed.commands)
}

pub async fn run_list(config: &AppConfig, workspace: &Path, all: bool) -> Result<()> {
    let ctx = load_ctx(config, workspace)?;
    let cmds = fetch_list(&ctx, all).await?;
    if cmds.is_empty() {
        println!("{}", "No commands found.".yellow());
        return Ok(());
    }
    println!(
        "{:<10} {:<10} {:<10} {:<10} {}",
        "CMD".bold(),
        "SESSION".bold(),
        "STATUS".bold(),
        "EXIT".bold(),
        "COMMAND".bold(),
    );
    for c in cmds {
        let exit = c
            .exit_code
            .map(|e| e.to_string())
            .unwrap_or_else(|| "-".to_string());
        let cmd_short = if c.command.len() > 60 {
            format!("{}…", &c.command[..59])
        } else {
            c.command.clone()
        };
        println!(
            "{:<10} {:<10} {:<10} {:<10} {}",
            c.command_id, c.session_id, c.status, exit, cmd_short
        );
    }
    Ok(())
}

pub async fn run_run(config: &AppConfig, workspace: &Path, command: &str) -> Result<()> {
    let ctx = load_ctx(config, workspace)?;
    let client = reqwest::Client::new();
    let req = RunReq {
        project_id: &ctx.project_id,
        command,
        session_id: None,
    };
    let resp = client
        .post(url("/commands/run"))
        .header("X-Api-Key", &ctx.api_key)
        .json(&req)
        .send()
        .await
        .context("Failed to reach the ai-pod server")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Server error {status}: {body}");
    }
    let r: RunResp = resp.json().await.context("Invalid /commands/run response")?;

    println!(
        "{} {} (session {})",
        "command_id:".blue().bold(),
        r.command_id,
        r.session_id
    );
    println!("{} {}", "status:".blue().bold(), r.status);
    if let Some(code) = r.exit_code {
        println!("{} {}", "exit_code:".blue().bold(), code);
    }
    if !r.stdout_tail.is_empty() {
        println!("{}\n{}", "--- stdout (tail) ---".dimmed(), r.stdout_tail);
    }
    if !r.stderr_tail.is_empty() {
        println!("{}\n{}", "--- stderr (tail) ---".dimmed(), r.stderr_tail);
    }
    if r.status == "running" {
        println!(
            "Command is still running. Re-check with: ai-pod commands logs {}",
            r.command_id
        );
    }
    Ok(())
}

pub async fn run_kill(
    config: &AppConfig,
    workspace: &Path,
    session_id: Option<&str>,
    command_id: &str,
) -> Result<()> {
    let ctx = load_ctx(config, workspace)?;
    let cmds = fetch_list(&ctx, true).await?;
    let sid = match session_id {
        Some(s) => s.to_string(),
        None => match cmds.iter().find(|c| c.command_id == command_id) {
            Some(c) => c.session_id.clone(),
            None => anyhow::bail!("Unknown command_id: {command_id}"),
        },
    };
    let client = reqwest::Client::new();
    let req = StopReq {
        project_id: &ctx.project_id,
        session_id: &sid,
        command_id,
    };
    let resp = client
        .post(url("/commands/stop"))
        .header("X-Api-Key", &ctx.api_key)
        .json(&req)
        .send()
        .await
        .context("Failed to reach the ai-pod server")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Server error {status}: {body}");
    }
    let r: StopResp = resp.json().await.context("Invalid /commands/stop response")?;
    if r.stopped {
        println!("{} {}", "Stopped:".green(), command_id);
    } else {
        println!("{} {} (already finished?)", "Not running:".yellow(), command_id);
    }
    Ok(())
}

pub async fn run_logs(
    config: &AppConfig,
    workspace: &Path,
    session_id: Option<&str>,
    command_id: &str,
) -> Result<()> {
    let ctx = load_ctx(config, workspace)?;
    let sid = match session_id {
        Some(s) => s.to_string(),
        None => {
            let cmds = fetch_list(&ctx, true).await?;
            cmds.iter()
                .find(|c| c.command_id == command_id)
                .ok_or_else(|| anyhow::anyhow!("Unknown command_id: {command_id}"))?
                .session_id
                .clone()
        }
    };
    let dir = ctx
        .workspace
        .join(".ai-pod")
        .join("commands")
        .join(&sid)
        .join(command_id);
    if !dir.exists() {
        anyhow::bail!("No output directory at {}", dir.display());
    }
    println!("{} {}", "session_id:".blue().bold(), sid);
    println!("{} {}", "command_id:".blue().bold(), command_id);
    if let Ok(cmd) = std::fs::read_to_string(dir.join("command")) {
        println!("{} {}", "command:".blue().bold(), cmd.trim());
    }
    if let Ok(exit) = std::fs::read_to_string(dir.join("exit")) {
        println!("{} {}", "exit:".blue().bold(), exit.trim());
    } else {
        println!("{} running", "exit:".blue().bold());
    }
    println!("{}", "--- stdout ---".dimmed());
    let _ = std::io::copy(
        &mut std::fs::File::open(dir.join("stdout")).unwrap_or_else(|_| {
            std::fs::File::open("/dev/null").expect("open /dev/null")
        }),
        &mut std::io::stdout(),
    );
    println!("{}", "--- stderr ---".dimmed());
    let _ = std::io::copy(
        &mut std::fs::File::open(dir.join("stderr")).unwrap_or_else(|_| {
            std::fs::File::open("/dev/null").expect("open /dev/null")
        }),
        &mut std::io::stdout(),
    );
    Ok(())
}

// ---------------- TUI ----------------

pub async fn run_tui(config: &AppConfig, workspace: &Path) -> Result<()> {
    let ctx = load_ctx(config, workspace)?;

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = tui_loop(&mut terminal, &ctx).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

struct TuiState {
    items: Vec<CommandSummary>,
    list_state: ListState,
    show_stderr: bool,
    last_refresh: Instant,
    log_buf: String,
}

async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ctx: &Ctx,
) -> Result<()> {
    let mut s = TuiState {
        items: Vec::new(),
        list_state: ListState::default(),
        show_stderr: false,
        last_refresh: Instant::now() - Duration::from_secs(60),
        log_buf: String::new(),
    };

    loop {
        if s.last_refresh.elapsed() > Duration::from_millis(500) {
            if let Ok(items) = fetch_list(ctx, true).await {
                s.items = items;
                if s.list_state.selected().is_none() && !s.items.is_empty() {
                    s.list_state.select(Some(0));
                }
                if let Some(idx) = s.list_state.selected() {
                    if idx >= s.items.len() && !s.items.is_empty() {
                        s.list_state.select(Some(s.items.len() - 1));
                    }
                }
                s.log_buf = current_log(ctx, &s);
            }
            s.last_refresh = Instant::now();
        }

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                .split(f.area());

            let items: Vec<ListItem> = s
                .items
                .iter()
                .map(|c| {
                    let icon = match c.status.as_str() {
                        "running" => "▶",
                        "killed" => "⏹",
                        "finished" => match c.exit_code {
                            Some(0) => "✓",
                            _ => "✗",
                        },
                        _ => "?",
                    };
                    let cmd = if c.command.len() > 30 {
                        format!("{}…", &c.command[..29])
                    } else {
                        c.command.clone()
                    };
                    ListItem::new(format!(
                        "{} {} {} {}",
                        icon, c.command_id, c.session_id, cmd
                    ))
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("commands  (↑/↓ select, k kill, Tab err/out, q quit)"),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(list, chunks[0], &mut s.list_state);

            let title = if s.show_stderr { "stderr" } else { "stdout" };
            let para = Paragraph::new(
                s.log_buf
                    .lines()
                    .rev()
                    .take(chunks[1].height as usize)
                    .map(|l| Line::from(l.to_string()))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>(),
            )
            .block(Block::default().borders(Borders::ALL).title(title));
            f.render_widget(para, chunks[1]);
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(KeyEvent {
                code, kind, ..
            }) = event::read()?
            {
                if kind != KeyEventKind::Press {
                    continue;
                }
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !s.items.is_empty() {
                            let cur = s.list_state.selected().unwrap_or(0);
                            let next = (cur + 1).min(s.items.len().saturating_sub(1));
                            s.list_state.select(Some(next));
                            s.log_buf = current_log(ctx, &s);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('K') => {
                        let cur = s.list_state.selected().unwrap_or(0);
                        s.list_state.select(Some(cur.saturating_sub(1)));
                        s.log_buf = current_log(ctx, &s);
                    }
                    KeyCode::Tab => {
                        s.show_stderr = !s.show_stderr;
                        s.log_buf = current_log(ctx, &s);
                    }
                    KeyCode::Char('k') => {
                        if let Some(idx) = s.list_state.selected() {
                            if let Some(c) = s.items.get(idx).cloned() {
                                if c.status == "running" {
                                    let client = reqwest::Client::new();
                                    let _ = client
                                        .post(url("/commands/stop"))
                                        .header("X-Api-Key", &ctx.api_key)
                                        .json(&StopReq {
                                            project_id: &ctx.project_id,
                                            session_id: &c.session_id,
                                            command_id: &c.command_id,
                                        })
                                        .send()
                                        .await;
                                    s.last_refresh = Instant::now() - Duration::from_secs(60);
                                }
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        s.last_refresh = Instant::now() - Duration::from_secs(60);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

// ---------------- Allow-list TUI ----------------

pub fn run_allowed_tui(config: &AppConfig, workspace: &Path) -> Result<()> {
    let hash = workspace_hash(workspace);
    let state_path = config.project_state_file(&hash);

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = allowed_tui_loop(&mut terminal, &state_path);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn allowed_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state_path: &Path,
) -> Result<()> {
    let mut state = ProjectState::load(state_path);
    let mut list_state = ListState::default();
    if !state.allowed_commands.is_empty() {
        list_state.select(Some(0));
    }
    let mut confirm_delete: Option<usize> = None;

    loop {
        terminal.draw(|f| {
            let items: Vec<ListItem> = state
                .allowed_commands
                .iter()
                .map(|c| ListItem::new(c.clone()))
                .collect();
            let title = match confirm_delete {
                Some(_) => "allowed commands  (y confirm delete, n cancel)".to_string(),
                None => "allowed commands  (↑/↓ select, d delete, q quit)".to_string(),
            };
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(list, f.area(), &mut list_state);
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(KeyEvent { code, kind, .. }) = event::read()? {
                if kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(idx) = confirm_delete {
                    match code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            if idx < state.allowed_commands.len() {
                                let cmd = state.allowed_commands[idx].clone();
                                state.remove_allowed(&cmd);
                                state.save(state_path)?;
                                let new_len = state.allowed_commands.len();
                                if new_len == 0 {
                                    list_state.select(None);
                                } else {
                                    list_state.select(Some(idx.min(new_len - 1)));
                                }
                            }
                            confirm_delete = None;
                        }
                        _ => confirm_delete = None,
                    }
                    continue;
                }
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !state.allowed_commands.is_empty() {
                            let cur = list_state.selected().unwrap_or(0);
                            let next = (cur + 1).min(state.allowed_commands.len() - 1);
                            list_state.select(Some(next));
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let cur = list_state.selected().unwrap_or(0);
                        list_state.select(Some(cur.saturating_sub(1)));
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(idx) = list_state.selected() {
                            if idx < state.allowed_commands.len() {
                                confirm_delete = Some(idx);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn current_log(ctx: &Ctx, s: &TuiState) -> String {
    let idx = match s.list_state.selected() {
        Some(i) => i,
        None => return String::new(),
    };
    let c = match s.items.get(idx) {
        Some(c) => c,
        None => return String::new(),
    };
    let dir = ctx
        .workspace
        .join(".ai-pod")
        .join("commands")
        .join(&c.session_id)
        .join(&c.command_id);
    let path = dir.join(if s.show_stderr { "stderr" } else { "stdout" });
    let mut buf = String::new();
    if let Ok(mut f) = std::fs::File::open(&path) {
        // Read the last 64 KiB to keep the TUI responsive.
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(64 * 1024);
        let _ = f.seek(SeekFrom::Start(start));
        let _ = f.read_to_string(&mut buf);
    }
    buf
}
