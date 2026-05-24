//! Host-side `ai-pod services` subcommand: list, logs, stop, plus a minimal
//! ratatui TUI that mirrors `ai-pod commands` for visual parity.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
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

use crate::runtime::ContainerRuntime;
use crate::service::{self, ServiceInfo};

/// Resolve a `name` to a `session_id` for a workspace. If `explicit` is
/// supplied, it wins. Otherwise we look at every service in the workspace and
/// only succeed when exactly one matches.
fn resolve_session(
    services: &[ServiceInfo],
    name: &str,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    let matches: Vec<&ServiceInfo> = services.iter().filter(|s| s.name == name).collect();
    match matches.len() {
        0 => anyhow::bail!("No service named '{}' in this workspace.", name),
        1 => Ok(matches[0].session_id.clone()),
        n => {
            let ids: Vec<&str> = matches.iter().map(|s| s.session_id.as_str()).collect();
            anyhow::bail!(
                "{} services named '{}' across sessions [{}]; pass --session to disambiguate.",
                n,
                name,
                ids.join(", ")
            )
        }
    }
}

pub fn run_list(rt: &ContainerRuntime, workspace: &Path) -> Result<()> {
    let services = service::list_services_for_workspace(rt, workspace)?;
    if services.is_empty() {
        println!("{}", "No services found.".yellow());
        return Ok(());
    }
    println!(
        "{:<16} {:<10} {:<30} {}",
        "NAME".bold(),
        "SESSION".bold(),
        "IMAGE".bold(),
        "STATUS".bold(),
    );
    for s in &services {
        let img_short = if s.image.len() > 28 {
            format!("{}…", &s.image[..27])
        } else {
            s.image.clone()
        };
        println!(
            "{:<16} {:<10} {:<30} {}",
            s.name, s.session_id, img_short, s.status,
        );
    }
    Ok(())
}

pub fn run_logs(
    rt: &ContainerRuntime,
    workspace: &Path,
    name: &str,
    session: Option<&str>,
    lines: usize,
) -> Result<()> {
    let services = service::list_services_for_workspace(rt, workspace)?;
    let session_id = resolve_session(&services, name, session)?;
    let logs = service::service_logs(rt, workspace, &session_id, name, lines)
        .context("failed to read service logs")?;
    println!("{} {}", "session:".blue().bold(), session_id);
    println!("{} {}", "service:".blue().bold(), name);
    println!("{}", "--- logs ---".dimmed());
    print!("{}", logs);
    if !logs.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub fn run_stop(
    rt: &ContainerRuntime,
    workspace: &Path,
    name: &str,
    session: Option<&str>,
) -> Result<()> {
    let services = service::list_services_for_workspace(rt, workspace)?;
    let session_id = resolve_session(&services, name, session)?;
    let stopped = service::stop_service(rt, workspace, &session_id, name)?;
    if stopped {
        println!("{} {} (session {})", "Stopped:".green(), name, session_id);
    } else {
        println!(
            "{} {} (session {}) — already gone?",
            "Not running:".yellow(),
            name,
            session_id
        );
    }
    Ok(())
}

// ---------------- TUI ----------------

pub fn run_tui(rt: &ContainerRuntime, workspace: &Path) -> Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = tui_loop(&mut terminal, rt, workspace);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

struct TuiState {
    items: Vec<ServiceInfo>,
    list_state: ListState,
    last_refresh: Instant,
    log_buf: String,
}

fn fetch_logs(rt: &ContainerRuntime, workspace: &Path, s: &TuiState) -> String {
    let idx = match s.list_state.selected() {
        Some(i) => i,
        None => return String::new(),
    };
    let svc = match s.items.get(idx) {
        Some(s) => s,
        None => return String::new(),
    };
    service::service_logs(rt, workspace, &svc.session_id, &svc.name, 200)
        .unwrap_or_default()
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    rt: &ContainerRuntime,
    workspace: &Path,
) -> Result<()> {
    let mut s = TuiState {
        items: Vec::new(),
        list_state: ListState::default(),
        last_refresh: Instant::now() - Duration::from_secs(60),
        log_buf: String::new(),
    };

    loop {
        if s.last_refresh.elapsed() > Duration::from_millis(750) {
            if let Ok(items) = service::list_services_for_workspace(rt, workspace) {
                s.items = items;
                if s.list_state.selected().is_none() && !s.items.is_empty() {
                    s.list_state.select(Some(0));
                }
                if let Some(idx) = s.list_state.selected() {
                    if idx >= s.items.len() && !s.items.is_empty() {
                        s.list_state.select(Some(s.items.len() - 1));
                    }
                }
                s.log_buf = fetch_logs(rt, workspace, &s);
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
                .map(|svc| {
                    let icon = if svc.status.starts_with("Up") || svc.status.starts_with("running")
                    {
                        "▶"
                    } else {
                        "⏹"
                    };
                    let img_short = if svc.image.len() > 24 {
                        format!("{}…", &svc.image[..23])
                    } else {
                        svc.image.clone()
                    };
                    ListItem::new(format!(
                        "{} {} {} {}",
                        icon, svc.name, svc.session_id, img_short
                    ))
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("services  (↑/↓ select, k kill, r refresh, q quit)"),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(list, chunks[0], &mut s.list_state);

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
            .block(Block::default().borders(Borders::ALL).title("logs"));
            f.render_widget(para, chunks[1]);
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(KeyEvent { code, kind, .. }) = event::read()? {
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
                            s.log_buf = fetch_logs(rt, workspace, &s);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('K') => {
                        let cur = s.list_state.selected().unwrap_or(0);
                        s.list_state.select(Some(cur.saturating_sub(1)));
                        s.log_buf = fetch_logs(rt, workspace, &s);
                    }
                    KeyCode::Char('k') => {
                        if let Some(idx) = s.list_state.selected() {
                            if let Some(svc) = s.items.get(idx).cloned() {
                                let _ =
                                    service::stop_service(rt, workspace, &svc.session_id, &svc.name);
                                s.last_refresh = Instant::now() - Duration::from_secs(60);
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
