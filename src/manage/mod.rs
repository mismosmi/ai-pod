//! `ai-pod manage` — host-wide TUI for inspecting and driving every running
//! ai-pod agent. Left pane lists agents (with hook-driven status highlights);
//! right pane streams the selected agent's pty through a vt100 emulator.

pub mod agent;
pub mod new_session;
pub mod status;
pub mod terminal;

use std::collections::HashMap;
use std::io::Stdout;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::config::AppConfig;
use crate::runtime::ContainerRuntime;

use self::agent::Agent;
use self::status::AgentStatus;
use self::terminal::AttachedTerm;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Term,
}

enum Screen {
    Main,
    NewSession(new_session::NewSessionState),
}

struct State {
    rt: ContainerRuntime,
    agents_dir: std::path::PathBuf,
    agents: Vec<Agent>,
    list_state: ListState,
    /// Container name → live attached pty.
    attached: HashMap<String, AttachedTerm>,
    focus: Focus,
    last_refresh: Instant,
    last_pane_layout: Option<(Rect, Rect)>,
    screen: Screen,
}

pub async fn run_manage(rt: ContainerRuntime) -> Result<()> {
    let config = AppConfig::new()?;
    let agents_dir = config.agents_dir();
    let _ = std::fs::create_dir_all(&agents_dir);

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alt screen")?;
    // Best-effort: ask the terminal to report modifier-disambiguated key
    // events (kitty keyboard protocol). Without this, Shift+Enter arrives
    // as plain Enter on most terminals. Silently ignored if the host
    // terminal doesn't support it.
    let kbd_enhanced = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )
    .is_ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_loop(&mut terminal, rt, agents_dir).await;

    disable_raw_mode().ok();
    if kbd_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rt: ContainerRuntime,
    agents_dir: std::path::PathBuf,
) -> Result<()> {
    let mut state = State {
        rt,
        agents_dir,
        agents: Vec::new(),
        list_state: ListState::default(),
        attached: HashMap::new(),
        focus: Focus::List,
        last_refresh: Instant::now() - Duration::from_secs(60),
        last_pane_layout: None,
        screen: Screen::Main,
    };

    loop {
        if state.last_refresh.elapsed() > Duration::from_millis(500) {
            refresh_agents(&mut state);
            state.last_refresh = Instant::now();
        }

        // While the new-session modal is open, drain any launch events the
        // background task has produced. On success the task hands us a
        // ready-to-go AttachedTerm — we plug it straight into state.attached,
        // close the modal, and jump to the terminal pane. This means we only
        // touch the channel when the modal is actually open, instead of
        // polling for an outcome every frame.
        let mut handed_term: Option<AttachedTerm> = None;
        if let Screen::NewSession(ref mut s) = state.screen {
            handed_term = new_session::drain_launch_events(s);
        }
        if let Some(term) = handed_term {
            let name = term.container_name.clone();
            state.attached.insert(name.clone(), term);
            state.screen = Screen::Main;
            refresh_agents(&mut state);
            state.last_refresh = Instant::now();
            if let Some(idx) = state
                .agents
                .iter()
                .position(|a| a.container_name == name)
            {
                state.list_state.select(Some(idx));
                state.focus = Focus::Term;
            }
        }

        draw(terminal, &mut state)?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Route keys to the modal when it's open, otherwise to
                    // the main view.
                    let in_modal = matches!(state.screen, Screen::NewSession(_));
                    if in_modal {
                        if let Screen::NewSession(ref mut s) = state.screen {
                            match new_session::handle_key(s, &k, &state.rt) {
                                Ok(true) => {
                                    state.screen = Screen::Main;
                                    state.last_refresh = Instant::now() - Duration::from_secs(60);
                                }
                                Ok(false) => {}
                                Err(_) => { /* surfaced via the log */ }
                            }
                        }
                    } else if handle_key(&mut state, &k) {
                        break;
                    }
                }
                Event::Mouse(m) => {
                    if matches!(state.screen, Screen::Main) {
                        handle_mouse(&mut state, &m);
                    }
                }
                Event::Resize(_, _) => {
                    // Layout recomputes on next draw; per-pty resize happens
                    // inside terminal::render when the inner area changes.
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn refresh_agents(state: &mut State) {
    let snap = match agent::snapshot(&state.rt, &state.agents_dir) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Attach to newly-seen running containers.
    for a in &snap {
        if !a.running {
            continue;
        }
        if state.attached.contains_key(&a.container_name) {
            continue;
        }
        if let Ok(term) = AttachedTerm::attach(&state.rt, &a.container_name) {
            state.attached.insert(a.container_name.clone(), term);
        }
    }

    // Drop attachments for containers that are gone.
    let live: std::collections::HashSet<&str> =
        snap.iter().map(|a| a.container_name.as_str()).collect();
    state.attached.retain(|name, _| live.contains(name.as_str()));

    state.agents = snap;

    // Keep the selection in bounds.
    if state.agents.is_empty() {
        state.list_state.select(None);
    } else {
        let idx = state.list_state.selected().unwrap_or(0);
        state
            .list_state
            .select(Some(idx.min(state.agents.len() - 1)));
    }
}

fn selected_agent<'a>(state: &'a State) -> Option<&'a Agent> {
    state.list_state.selected().and_then(|i| state.agents.get(i))
}

fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, state: &mut State) -> Result<()> {
    let mut pane_layout: Option<(Rect, Rect)> = None;
    terminal.draw(|f| {
        let area = f.area();
        // List pane: capped at 40 cols, but never more than 40% of the width
        // on narrow terminals.
        let list_w = (area.width as u32 * 40 / 100).min(40).max(20) as u16;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let main = chunks[0];
        let footer = chunks[1];

        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(list_w), Constraint::Min(20)])
            .split(main);
        let list_area = panes[0];
        let term_area = panes[1];
        pane_layout = Some((list_area, term_area));

        // ---- list ----
        let items: Vec<ListItem> = state
            .agents
            .iter()
            .map(|a| {
                let (glyph, style) = match a.status {
                    AgentStatus::AwaitingInput => (
                        "! ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    AgentStatus::Finished => ("✓ ", Style::default().fg(Color::Green)),
                    AgentStatus::Idle => ("· ", Style::default().fg(Color::DarkGray)),
                    AgentStatus::Running => ("▶ ", Style::default()),
                };
                let project = if a.project_name.len() > 16 {
                    format!("{}…", &a.project_name[..15])
                } else {
                    a.project_name.clone()
                };
                let line1 = Line::from(vec![
                    Span::styled(glyph, style),
                    Span::styled(project, style),
                ]);
                let detail = if !a.status_line.is_empty() {
                    a.status_line.clone()
                } else {
                    match a.status {
                        AgentStatus::Running => "running".to_string(),
                        AgentStatus::Idle => "idle".to_string(),
                        AgentStatus::AwaitingInput => "needs input".to_string(),
                        AgentStatus::Finished => "finished".to_string(),
                    }
                };
                let detail = if detail.len() > 28 {
                    format!("  {}…", &detail[..27])
                } else {
                    format!("  {}", detail)
                };
                let line2 = Line::from(Span::styled(
                    detail,
                    Style::default().fg(Color::DarkGray),
                ));
                ListItem::new(vec![line1, line2])
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(if state.focus == Focus::List {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    })
                    .title(format!(" agents ({}) ", state.agents.len())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, list_area, &mut state.list_state);

        // ---- terminal pane ----
        // When the new-session modal is open we skip rendering the agent's
        // vt100 grid entirely: the modal only Clears its own rect, so any
        // cells the pane writes outside the modal's bounds would "leak"
        // around the popover.
        let modal_open = matches!(state.screen, Screen::NewSession(_));
        let selected_name = state
            .list_state
            .selected()
            .and_then(|i| state.agents.get(i))
            .map(|a| a.container_name.clone());
        let mut cursor_pos: Option<(u16, u16)> = None;
        if modal_open {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));
            f.render_widget(block, term_area);
        } else if let Some(name) = selected_name.as_deref() {
            if let Some(term) = state.attached.get_mut(name) {
                let title = format!(" {} ", name);
                cursor_pos = term.render(f, term_area, state.focus == Focus::Term, &title);
            } else {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(format!(" {} (not attached) ", name));
                f.render_widget(block, term_area);
            }
        } else {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" no agent selected ");
            f.render_widget(block, term_area);
        }

        // ---- footer ----
        let hint = match state.focus {
            Focus::List => " ↑/↓ select · Enter focus terminal · n new session · q/F10 quit ",
            Focus::Term => " F1 back to list · F10 quit  (other keys → agent) ",
        };
        let footer_p = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
        f.render_widget(footer_p, footer);

        if !modal_open {
            if let Some((cx, cy)) = cursor_pos {
                f.set_cursor_position((cx, cy));
            }
        }

        if let Screen::NewSession(ref s) = state.screen {
            new_session::render(s, f, area);
        }
    })?;
    state.last_pane_layout = pane_layout;
    Ok(())
}

/// Returns true if the loop should exit.
fn handle_key(state: &mut State, key: &KeyEvent) -> bool {
    // Global quit — works from any focus so the user is never stuck inside
    // the terminal pane with keystrokes being forwarded to the agent.
    if key.code == KeyCode::F(10) {
        return true;
    }
    match state.focus {
        Focus::List => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
            KeyCode::Enter | KeyCode::Tab | KeyCode::F(1) => {
                if selected_agent(state).is_some() {
                    state.focus = Focus::Term;
                }
            }
            KeyCode::Char('r') => {
                state.last_refresh = Instant::now() - Duration::from_secs(60);
            }
            KeyCode::Char('n') => {
                state.screen = Screen::NewSession(new_session::NewSessionState::start());
            }
            _ => {}
        },
        Focus::Term => {
            if key.code == KeyCode::F(1) {
                state.focus = Focus::List;
                return false;
            }
            if let Some(agent) = selected_agent(state) {
                let name = agent.container_name.clone();
                if let Some(term) = state.attached.get_mut(&name) {
                    term.send_key(key);
                }
            }
        }
    }
    false
}

fn handle_mouse(state: &mut State, ev: &MouseEvent) {
    let (list_area, term_area) = match state.last_pane_layout {
        Some(l) => l,
        None => return,
    };
    let point = Rect {
        x: ev.column,
        y: ev.row,
        width: 1,
        height: 1,
    };
    let in_list = list_area.intersects(point);
    let in_term = term_area.intersects(point);
    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if in_list {
                let inner_top = list_area.y + 1; // border
                if ev.row >= inner_top {
                    let offset = ((ev.row - inner_top) / 2) as usize;
                    if offset < state.agents.len() {
                        state.list_state.select(Some(offset));
                    }
                }
                state.focus = Focus::List;
            } else if in_term && selected_agent(state).is_some() {
                state.focus = Focus::Term;
            }
        }
        MouseEventKind::ScrollDown if in_list => move_selection(state, 1),
        MouseEventKind::ScrollUp if in_list => move_selection(state, -1),
        _ => {}
    }
}

fn move_selection(state: &mut State, delta: i32) {
    if state.agents.is_empty() {
        state.list_state.select(None);
        return;
    }
    let cur = state.list_state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).clamp(0, state.agents.len() as i32 - 1) as usize;
    state.list_state.select(Some(next));
}
