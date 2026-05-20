//! Host-side `ai-pod env-files` subcommand: list, hide, unhide, ignore,
//! unignore, plus a ratatui TUI for interactive management of sensitive files
//! in a workspace.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::config::AppConfig;
use crate::credentials::{EnvFileEntry, EnvFileStatus, hide_file, list_env_files, unhide_file};
use crate::server::lifecycle::ProjectState;
use crate::workspace::workspace_hash;

pub fn run_list(config: &AppConfig, workspace: &Path) -> Result<()> {
    let entries = list_env_files(workspace, config);
    if entries.is_empty() {
        println!("{}", "No sensitive files detected.".dimmed());
        return Ok(());
    }
    for e in entries {
        let tag = status_tag(e.status);
        println!("{}  {}", tag, e.rel_path);
    }
    Ok(())
}

pub fn run_hide(config: &AppConfig, workspace: &Path, rel: &str) -> Result<()> {
    let dst = hide_file(workspace, config, rel)?;
    println!(
        "{} {} → {}",
        "Hidden:".green().bold(),
        rel,
        dst.display()
    );
    Ok(())
}

pub fn run_unhide(workspace: &Path, rel: &str) -> Result<()> {
    unhide_file(workspace, rel)?;
    println!("{} {}", "Unhidden:".green().bold(), rel);
    Ok(())
}

pub fn run_ignore(config: &AppConfig, workspace: &Path, rel: &str) -> Result<()> {
    let hash = workspace_hash(workspace);
    let state_path = config.project_state_file(&hash);
    let mut state = ProjectState::load(&state_path);
    if state.is_credential_ignored(rel) {
        println!("{} {}", "Already ignored:".yellow(), rel);
        return Ok(());
    }
    state.add_ignored_credential(rel);
    state.save(&state_path)?;
    println!("{} {}", "Ignored:".green().bold(), rel);
    Ok(())
}

pub fn run_unignore(config: &AppConfig, workspace: &Path, rel: &str) -> Result<()> {
    let hash = workspace_hash(workspace);
    let state_path = config.project_state_file(&hash);
    let mut state = ProjectState::load(&state_path);
    if !state.is_credential_ignored(rel) {
        println!("{} {}", "Not ignored:".yellow(), rel);
        return Ok(());
    }
    state.remove_ignored_credential(rel);
    state.save(&state_path)?;
    println!("{} {}", "Unignored:".green().bold(), rel);
    Ok(())
}

fn status_tag(status: EnvFileStatus) -> String {
    match status {
        EnvFileStatus::Hidden => "[hidden] ".to_string(),
        EnvFileStatus::Exposed => "[exposed]".to_string(),
        EnvFileStatus::Ignored => "[ignored]".to_string(),
    }
}

// ---------------- TUI ----------------

pub fn run_tui(config: &AppConfig, workspace: &Path) -> Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = tui_loop(&mut terminal, config, workspace);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    config: &AppConfig,
    workspace: &Path,
) -> Result<()> {
    let mut entries = list_env_files(workspace, config);
    let mut list_state = ListState::default();
    if !entries.is_empty() {
        list_state.select(Some(0));
    }
    let mut message: Option<(String, Color)> = None;

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(3)])
                .split(f.area());

            let items: Vec<ListItem> = entries
                .iter()
                .map(|e| {
                    let (tag, color) = match e.status {
                        EnvFileStatus::Hidden => ("[hidden] ", Color::Green),
                        EnvFileStatus::Exposed => ("[exposed]", Color::Red),
                        EnvFileStatus::Ignored => ("[ignored]", Color::Yellow),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(tag, Style::default().fg(color)),
                        Span::raw("  "),
                        Span::raw(e.rel_path.clone()),
                    ]))
                })
                .collect();

            let title =
                "sensitive files  (↑/↓ select · h hide · u unhide · i ignore · r unignore · q quit)";
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(list, chunks[0], &mut list_state);

            let footer_text = if let Some((msg, color)) = &message {
                Line::from(Span::styled(msg.clone(), Style::default().fg(*color)))
            } else {
                match list_state.selected().and_then(|i| entries.get(i)) {
                    Some(e) => {
                        let label = match e.status {
                            EnvFileStatus::Hidden => "Stored at",
                            EnvFileStatus::Exposed | EnvFileStatus::Ignored => "Would hide to",
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{}: ", label),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(e.destination.display().to_string()),
                        ])
                    }
                    None => Line::from(Span::styled(
                        "No sensitive files detected in this workspace.",
                        Style::default().fg(Color::DarkGray),
                    )),
                }
            };
            let footer = Paragraph::new(footer_text)
                .block(Block::default().borders(Borders::ALL).title("details"));
            f.render_widget(footer, chunks[1]);
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(KeyEvent { code, kind, .. }) = event::read()? {
                if kind != KeyEventKind::Press {
                    continue;
                }
                message = None;
                match code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !entries.is_empty() {
                            let cur = list_state.selected().unwrap_or(0);
                            let next = (cur + 1).min(entries.len() - 1);
                            list_state.select(Some(next));
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let cur = list_state.selected().unwrap_or(0);
                        list_state.select(Some(cur.saturating_sub(1)));
                    }
                    KeyCode::Char('h') => {
                        if let Some(entry) = selected_entry(&entries, &list_state).cloned() {
                            message = Some(apply_action(
                                config,
                                workspace,
                                &entry,
                                EnvFileAction::Hide,
                            ));
                            entries = list_env_files(workspace, config);
                            reselect(&entries, &mut list_state, &entry.rel_path);
                        }
                    }
                    KeyCode::Char('u') => {
                        if let Some(entry) = selected_entry(&entries, &list_state).cloned() {
                            message = Some(apply_action(
                                config,
                                workspace,
                                &entry,
                                EnvFileAction::Unhide,
                            ));
                            entries = list_env_files(workspace, config);
                            reselect(&entries, &mut list_state, &entry.rel_path);
                        }
                    }
                    KeyCode::Char('i') => {
                        if let Some(entry) = selected_entry(&entries, &list_state).cloned() {
                            message = Some(apply_action(
                                config,
                                workspace,
                                &entry,
                                EnvFileAction::Ignore,
                            ));
                            entries = list_env_files(workspace, config);
                            reselect(&entries, &mut list_state, &entry.rel_path);
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Some(entry) = selected_entry(&entries, &list_state).cloned() {
                            message = Some(apply_action(
                                config,
                                workspace,
                                &entry,
                                EnvFileAction::Unignore,
                            ));
                            entries = list_env_files(workspace, config);
                            reselect(&entries, &mut list_state, &entry.rel_path);
                        }
                    }
                    KeyCode::Char('R') => {
                        entries = list_env_files(workspace, config);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum EnvFileAction {
    Hide,
    Unhide,
    Ignore,
    Unignore,
}

fn apply_action(
    config: &AppConfig,
    workspace: &Path,
    entry: &EnvFileEntry,
    action: EnvFileAction,
) -> (String, Color) {
    let run = || -> Result<Option<String>> {
        match action {
            EnvFileAction::Hide => {
                if entry.status == EnvFileStatus::Hidden {
                    return Ok(None);
                }
                let hash = workspace_hash(workspace);
                let state_path = config.project_state_file(&hash);
                let mut state = ProjectState::load(&state_path);
                if state.is_credential_ignored(&entry.rel_path) {
                    state.remove_ignored_credential(&entry.rel_path);
                    state.save(&state_path)?;
                }
                let dst = hide_file(workspace, config, &entry.rel_path)?;
                Ok(Some(format!("Hidden: {} → {}", entry.rel_path, dst.display())))
            }
            EnvFileAction::Unhide => {
                if entry.status != EnvFileStatus::Hidden {
                    return Ok(None);
                }
                unhide_file(workspace, &entry.rel_path)?;
                Ok(Some(format!("Unhidden: {}", entry.rel_path)))
            }
            EnvFileAction::Ignore => {
                if entry.status != EnvFileStatus::Exposed {
                    return Ok(None);
                }
                let hash = workspace_hash(workspace);
                let state_path = config.project_state_file(&hash);
                let mut state = ProjectState::load(&state_path);
                state.add_ignored_credential(&entry.rel_path);
                state.save(&state_path)?;
                Ok(Some(format!("Ignored: {}", entry.rel_path)))
            }
            EnvFileAction::Unignore => {
                if entry.status != EnvFileStatus::Ignored {
                    return Ok(None);
                }
                let hash = workspace_hash(workspace);
                let state_path = config.project_state_file(&hash);
                let mut state = ProjectState::load(&state_path);
                state.remove_ignored_credential(&entry.rel_path);
                state.save(&state_path)?;
                Ok(Some(format!("Unignored: {}", entry.rel_path)))
            }
        }
    };
    match run() {
        Ok(Some(msg)) => (msg, Color::Green),
        Ok(None) => (
            format!(
                "{} — no action: {}",
                entry.rel_path,
                action_not_applicable_reason(action, entry.status)
            ),
            Color::Yellow,
        ),
        Err(e) => (e.to_string(), Color::Red),
    }
}

fn action_not_applicable_reason(action: EnvFileAction, status: EnvFileStatus) -> &'static str {
    match (action, status) {
        (EnvFileAction::Hide, EnvFileStatus::Hidden) => "already hidden",
        (EnvFileAction::Unhide, _) => "not hidden",
        (EnvFileAction::Ignore, EnvFileStatus::Hidden) => "hidden files don't need ignoring",
        (EnvFileAction::Ignore, EnvFileStatus::Ignored) => "already ignored",
        (EnvFileAction::Unignore, _) => "not ignored",
        _ => "no change required",
    }
}

fn selected_entry<'a>(
    entries: &'a [EnvFileEntry],
    list_state: &ListState,
) -> Option<&'a EnvFileEntry> {
    list_state.selected().and_then(|i| entries.get(i))
}

fn reselect(entries: &[EnvFileEntry], list_state: &mut ListState, target_rel: &str) {
    if entries.is_empty() {
        list_state.select(None);
        return;
    }
    let idx = entries
        .iter()
        .position(|e| e.rel_path == target_rel)
        .unwrap_or_else(|| list_state.selected().unwrap_or(0).min(entries.len() - 1));
    list_state.select(Some(idx));
}
