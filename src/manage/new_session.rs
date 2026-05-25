//! New-session flow inside `ai-pod manage`: pick (or create) a workspace
//! directory, optionally create an ai-pod.Dockerfile, and start a detached
//! agent container that the TUI then attaches to.

use std::path::{Path, PathBuf};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::cli::{Agent, BaseImage};
use crate::config::AppConfig;
use crate::runtime::ContainerRuntime;

use super::terminal::AttachedTerm;

/// Events streamed from the background launch task into the modal. Once
/// `Done` or `Error` arrives the task exits and the channel sender is
/// dropped; no polling needed beyond draining whatever is queued each frame.
pub enum LaunchEvent {
    Log(String),
    /// `podman run -d -it` succeeded and we already attached a pty to it.
    /// The main loop inserts the term directly into `state.attached`.
    Done(AttachedTerm),
    Error(String),
}

/// What the new-session modal is currently showing.
pub enum Stage {
    Picking {
        input: String,
        candidates: Vec<PathBuf>,
        list_state: ListState,
        // Known projects from ~/.ai-pod/*.json (used when input is empty).
        known: Vec<PathBuf>,
    },
    PickAgent {
        workspace: PathBuf,
        agents: Vec<Agent>,
        list_state: ListState,
    },
    PickImage {
        workspace: PathBuf,
        agent: Agent,
        images: Vec<BaseImage>,
        list_state: ListState,
    },
    Launching {
        workspace: PathBuf,
        log: Vec<String>,
        rx: UnboundedReceiver<LaunchEvent>,
        /// `None` while the task is still running; `Some(Ok())` on success
        /// (the AttachedTerm has already been handed to the main loop), or
        /// `Some(Err(msg))` if the task failed.
        result: Option<std::result::Result<(), String>>,
    },
}

pub struct NewSessionState {
    pub stage: Stage,
}

impl NewSessionState {
    pub fn start() -> Self {
        let known = known_projects();
        let mut list_state = ListState::default();
        if !known.is_empty() {
            list_state.select(Some(0));
        }
        let candidates = known.clone();
        Self {
            stage: Stage::Picking {
                input: String::new(),
                candidates,
                list_state,
                known,
            },
        }
    }
}

/// Drive a key event. Returns:
/// - `Ok(true)` → close the modal (return to main view)
/// - `Ok(false)` → stay in the modal
pub fn handle_key(
    state: &mut NewSessionState,
    key: &KeyEvent,
    rt: &ContainerRuntime,
) -> Result<bool> {
    // Esc always closes (except while a launch is in progress; let the user
    // close once it's done).
    if key.code == KeyCode::Esc {
        if matches!(&state.stage, Stage::Launching { result, .. } if result.is_none()) {
            return Ok(false);
        }
        return Ok(true);
    }

    // Some arms transition out of the current stage by calling start_launch /
    // advance_after_pick, which both mutate state.stage. To avoid a double
    // mutable borrow we collect the transition data here and run the
    // transition after the match.
    enum Transition {
        None,
        Pick(PathBuf),
        Launch(PathBuf),
    }
    let mut transition = Transition::None;

    match &mut state.stage {
        Stage::Picking {
            input,
            candidates,
            list_state,
            known,
        } => match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                refresh_candidates(input, candidates, list_state, known);
            }
            KeyCode::Backspace => {
                input.pop();
                refresh_candidates(input, candidates, list_state, known);
            }
            KeyCode::Down => {
                if !candidates.is_empty() {
                    let cur = list_state.selected().unwrap_or(0);
                    list_state.select(Some((cur + 1).min(candidates.len() - 1)));
                }
            }
            KeyCode::Up => {
                let cur = list_state.selected().unwrap_or(0);
                list_state.select(Some(cur.saturating_sub(1)));
            }
            KeyCode::Tab => {
                // Complete to the selected candidate.
                if let Some(idx) = list_state.selected() {
                    if let Some(path) = candidates.get(idx) {
                        *input = path.to_string_lossy().to_string();
                        if !input.ends_with('/') {
                            input.push('/');
                        }
                        refresh_candidates(input, candidates, list_state, known);
                    }
                }
            }
            KeyCode::Enter => {
                let path = resolve_picked_path(input, candidates, list_state)?;
                transition = Transition::Pick(path);
            }
            _ => {}
        },
        Stage::PickAgent {
            workspace,
            agents,
            list_state,
        } => match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let cur = list_state.selected().unwrap_or(0);
                list_state.select(Some((cur + 1).min(agents.len() - 1)));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let cur = list_state.selected().unwrap_or(0);
                list_state.select(Some(cur.saturating_sub(1)));
            }
            KeyCode::Enter => {
                let idx = list_state.selected().unwrap_or(0);
                let agent = agents[idx].clone();
                let images: Vec<BaseImage> = match agent {
                    Agent::Claude => vec![
                        BaseImage::Alpine,
                        BaseImage::Ubuntu,
                        BaseImage::Node,
                        BaseImage::Rust,
                        BaseImage::Python,
                    ],
                    Agent::Opencode => vec![
                        BaseImage::Ubuntu,
                        BaseImage::Node,
                        BaseImage::Rust,
                        BaseImage::Python,
                    ],
                };
                let mut new_state = ListState::default();
                new_state.select(Some(0));
                state.stage = Stage::PickImage {
                    workspace: workspace.clone(),
                    agent,
                    images,
                    list_state: new_state,
                };
            }
            _ => {}
        },
        Stage::PickImage {
            workspace,
            agent,
            images,
            list_state,
        } => match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let cur = list_state.selected().unwrap_or(0);
                list_state.select(Some((cur + 1).min(images.len() - 1)));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let cur = list_state.selected().unwrap_or(0);
                list_state.select(Some(cur.saturating_sub(1)));
            }
            KeyCode::Enter => {
                let idx = list_state.selected().unwrap_or(0);
                let image = images[idx].clone();
                crate::image::write_dockerfile(workspace, agent, &image)?;
                transition = Transition::Launch(workspace.clone());
            }
            _ => {}
        },
        Stage::Launching { result, .. } => {
            // Enter/space closes once we're done. On success the main loop
            // already auto-closed; this is the manual fallback (errors).
            if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) && result.is_some() {
                return Ok(true);
            }
        }
    }

    match transition {
        Transition::None => {}
        Transition::Pick(path) => advance_after_pick(state, path)?,
        Transition::Launch(path) => start_launch(state, rt.clone(), path),
    }
    Ok(false)
}

fn refresh_candidates(
    input: &str,
    candidates: &mut Vec<PathBuf>,
    list_state: &mut ListState,
    known: &[PathBuf],
) {
    if input.is_empty() {
        *candidates = known.to_vec();
    } else {
        *candidates = directory_completions(input);
    }
    if candidates.is_empty() {
        list_state.select(None);
    } else {
        list_state.select(Some(0));
    }
}

/// Decide which path the user picked when they pressed Enter on the picker.
/// Prefers the highlighted candidate if the input matches its parent; falls
/// back to the raw input. The chosen path is then expanded (~ → $HOME) and
/// canonicalised if it already exists.
fn resolve_picked_path(
    input: &str,
    candidates: &[PathBuf],
    list_state: &ListState,
) -> Result<PathBuf> {
    let raw = if input.is_empty() {
        list_state
            .selected()
            .and_then(|i| candidates.get(i))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        input.to_string()
    };
    if raw.is_empty() {
        anyhow::bail!("No path provided");
    }
    Ok(expand_tilde(&raw))
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

/// Read every `~/.ai-pod/{hash}.json` (skip `server.json`) and return their
/// `workspace` paths, deduplicated and sorted.
fn known_projects() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let config = match AppConfig::new() {
        Ok(c) => c,
        Err(_) => return out,
    };
    let dir = match std::fs::read_dir(&config.config_dir) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_stem().and_then(|s| s.to_str()) == Some("server") {
            continue;
        }
        let state = crate::server::lifecycle::ProjectState::load(&path);
        if !state.workspace.is_empty() {
            out.push(PathBuf::from(state.workspace));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Filesystem-based directory completion for the typed path. Splits the input
/// into `(dirname, basename_prefix)` and lists immediate subdirectories of
/// `dirname` whose name starts with the prefix.
fn directory_completions(input: &str) -> Vec<PathBuf> {
    let expanded = expand_tilde(input);
    let (parent, prefix) = if input.ends_with('/') {
        (expanded.clone(), String::new())
    } else {
        let parent = expanded
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let prefix = expanded
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (parent, prefix)
    };
    let parent = if parent.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        parent
    };
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&parent) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name_s = name.to_string_lossy().to_string();
            if name_s.starts_with('.') && !prefix.starts_with('.') {
                continue;
            }
            if !prefix.is_empty() && !name_s.starts_with(&prefix) {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out.sort();
    out.truncate(50);
    out
}

fn advance_after_pick(state: &mut NewSessionState, path: PathBuf) -> Result<()> {
    // Auto-create the directory if it doesn't exist (user-confirmed default).
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }
    let workspace = std::fs::canonicalize(&path).unwrap_or(path);
    let dockerfile = workspace.join(crate::image::DOCKERFILE_NAME);
    if dockerfile.exists() {
        // Skip straight to launch.
        let rt = ContainerRuntime::detect(false)?;
        start_launch(state, rt, workspace);
        return Ok(());
    }
    // No Dockerfile → walk through the init wizard.
    let mut list_state = ListState::default();
    list_state.select(Some(0));
    state.stage = Stage::PickAgent {
        workspace,
        agents: vec![Agent::Claude, Agent::Opencode],
        list_state,
    };
    Ok(())
}

/// Spawn a background tokio task that runs the full launch flow in detached
/// mode and pipes progress into `log`. The TUI watches `done` / `error` to
/// know when to allow closing the modal.
fn start_launch(state: &mut NewSessionState, rt: ContainerRuntime, workspace: PathBuf) {
    let (tx, rx) = unbounded_channel::<LaunchEvent>();
    let workspace_t = workspace.clone();

    tokio::spawn(async move {
        let log = |tx: &UnboundedSender<LaunchEvent>, msg: &str| {
            let _ = tx.send(LaunchEvent::Log(msg.to_string()));
        };
        let result: Result<AttachedTerm> = (async {
            let config = AppConfig::new()?;
            config.init()?;

            log(&tx, "Ensuring shared server is running...");
            crate::server::lifecycle::ensure_shared_server(&config).await?;

            log(&tx, "Building image (this can take a few minutes)...");
            let image_name = crate::image::image_name(&workspace_t);
            let dockerfile = workspace_t.join(crate::image::DOCKERFILE_NAME);
            let rt_b = rt.clone();
            let dockerfile_b = dockerfile.clone();
            let image_b = image_name.clone();
            tokio::task::spawn_blocking(move || {
                crate::image::ensure_image(&rt_b, &dockerfile_b, &image_b, false, false)
            })
            .await
            .map_err(|e| anyhow::anyhow!("build task join: {e}"))??;

            crate::server::lifecycle::bump_keep_alive().await;
            crate::server::lifecycle::check_server_version().await?;

            log(&tx, "Preparing project state...");
            let project_id = crate::workspace::workspace_hash(&workspace_t);
            let proj_state =
                crate::server::lifecycle::get_or_create_project_state(&config, &workspace_t)?;
            crate::server::lifecycle::reload_config().await?;

            log(&tx, "Starting detached container...");
            // Run the synchronous container start + pty attach off the async
            // runtime, then ship the live AttachedTerm back to the main loop.
            let rt_l = rt.clone();
            let workspace_l = workspace_t.clone();
            let config_l = AppConfig::new()?;
            let image_l = image_name.clone();
            let api_key_l = proj_state.api_key.clone();
            let project_id_l = project_id.clone();
            let term = tokio::task::spawn_blocking(move || -> Result<AttachedTerm> {
                let name = crate::container::start_container_detached(
                    &rt_l,
                    &config_l,
                    &workspace_l,
                    &image_l,
                    &project_id_l,
                    &api_key_l,
                    None,
                )?;
                // Immediately attach to the new container's pty so the main
                // loop can take ownership without an extra round-trip
                // through refresh_agents().
                let term = AttachedTerm::attach(&rt_l, &name)?;
                Ok(term)
            })
            .await
            .map_err(|e| anyhow::anyhow!("launch task join: {e}"))??;

            Ok(term)
        })
        .await;

        match result {
            Ok(term) => {
                let _ = tx.send(LaunchEvent::Done(term));
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = tx.send(LaunchEvent::Error(msg));
            }
        }
        // tx drops here → main loop sees the channel close after draining.
    });

    state.stage = Stage::Launching {
        workspace,
        log: Vec::new(),
        rx,
        result: None,
    };
}

/// Drain whatever events the launch task has sent so far into the modal
/// state. Returns the AttachedTerm when the task finished successfully so
/// the caller can plug it into `state.attached` and auto-close the modal.
pub fn drain_launch_events(state: &mut NewSessionState) -> Option<AttachedTerm> {
    let Stage::Launching {
        log, rx, result, ..
    } = &mut state.stage
    else {
        return None;
    };
    if result.is_some() {
        return None;
    }
    let mut term_out: Option<AttachedTerm> = None;
    loop {
        match rx.try_recv() {
            Ok(LaunchEvent::Log(msg)) => log.push(msg),
            Ok(LaunchEvent::Done(term)) => {
                log.push(format!("Container started: {}", term.container_name));
                *result = Some(Ok(()));
                term_out = Some(term);
                break;
            }
            Ok(LaunchEvent::Error(msg)) => {
                log.push(format!("ERROR: {msg}"));
                *result = Some(Err(msg));
                break;
            }
            Err(_) => break, // empty or closed
        }
    }
    term_out
}

/// Render the modal centred over `area`. Caller draws the main view first;
/// the modal overlays it.
pub fn render(state: &NewSessionState, frame: &mut Frame<'_>, area: Rect) {
    let modal = centred(area, 80, 24);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" new session ");
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    match &state.stage {
        Stage::Picking {
            input,
            candidates,
            list_state,
            ..
        } => render_picking(frame, inner, input, candidates, list_state),
        Stage::PickAgent {
            workspace,
            agents,
            list_state,
        } => render_agent_pick(frame, inner, workspace, agents, list_state),
        Stage::PickImage {
            workspace,
            agent,
            images,
            list_state,
        } => render_image_pick(frame, inner, workspace, agent, images, list_state),
        Stage::Launching {
            workspace,
            log,
            result,
            ..
        } => render_launching(frame, inner, workspace, log, result),
    }
}

fn render_picking(
    frame: &mut Frame<'_>,
    inner: Rect,
    input: &str,
    candidates: &[PathBuf],
    list_state: &ListState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);

    let title = if input.is_empty() {
        " Path  (type to search dirs; ↑/↓ Enter to pick known project)"
    } else {
        " Path  (Tab complete · Enter use this path)"
    };
    let prompt = Paragraph::new(format!("> {input}_"))
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(prompt, chunks[0]);

    let label = if input.is_empty() {
        " Known projects "
    } else {
        " Completions "
    };
    let items: Vec<ListItem> = candidates
        .iter()
        .map(|p| ListItem::new(p.to_string_lossy().to_string()))
        .collect();
    let mut ls = list_state.clone();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(label))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut ls);

    let hint = Paragraph::new(" Enter pick · Tab complete · Esc cancel ")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, chunks[2]);
}

fn render_agent_pick(
    frame: &mut Frame<'_>,
    inner: Rect,
    workspace: &Path,
    agents: &[Agent],
    list_state: &ListState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let header = Paragraph::new(vec![
        Line::from(format!("No ai-pod.Dockerfile in {}", workspace.display())),
        Line::from(Span::styled(
            "Pick an agent to bootstrap one.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title(" init "));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = agents
        .iter()
        .map(|a| {
            ListItem::new(match a {
                Agent::Claude => "Claude",
                Agent::Opencode => "OpenCode",
            })
        })
        .collect();
    let mut ls = list_state.clone();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" agent "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut ls);

    let hint = Paragraph::new(" ↑/↓ select · Enter pick · Esc cancel ")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, chunks[2]);
}

fn render_image_pick(
    frame: &mut Frame<'_>,
    inner: Rect,
    workspace: &Path,
    agent: &Agent,
    images: &[BaseImage],
    list_state: &ListState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let header = Paragraph::new(vec![
        Line::from(format!("Workspace: {}", workspace.display())),
        Line::from(format!(
            "Agent: {}",
            match agent {
                Agent::Claude => "Claude",
                Agent::Opencode => "OpenCode",
            }
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title(" init "));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = images
        .iter()
        .map(|i| {
            ListItem::new(match i {
                BaseImage::Alpine => "Alpine",
                BaseImage::Ubuntu => "Ubuntu",
                BaseImage::Node => "Node",
                BaseImage::Rust => "Rust",
                BaseImage::Python => "Python",
            })
        })
        .collect();
    let mut ls = list_state.clone();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" base image "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut ls);

    let hint = Paragraph::new(" ↑/↓ select · Enter create + launch · Esc cancel ")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, chunks[2]);
}

fn render_launching(
    frame: &mut Frame<'_>,
    inner: Rect,
    workspace: &Path,
    log: &[String],
    result: &Option<std::result::Result<(), String>>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    let workspace_str = workspace.display().to_string();
    // Inner area of the header block is chunks[0].width - 2 (left + right borders).
    let header_text = truncate_to(
        &format!("Launching agent for {workspace_str}"),
        chunks[0].width.saturating_sub(2) as usize,
    );
    let header = Paragraph::new(header_text)
        .block(Block::default().borders(Borders::ALL).title(" launch "));
    frame.render_widget(header, chunks[0]);

    // Log body. Long lines are truncated with an ellipsis so they can never
    // spill past the modal — image builds in particular like to emit very
    // long lines (full layer ids, urls, etc.).
    let inner_w = chunks[1].width.saturating_sub(2) as usize;
    let inner_h = chunks[1].height.saturating_sub(2) as usize;
    let lines: Vec<Line> = log
        .iter()
        .rev()
        .take(inner_h)
        .rev()
        .map(|s| {
            let trimmed = truncate_to(s, inner_w);
            if s.starts_with("ERROR") {
                Line::from(Span::styled(trimmed, Style::default().fg(Color::Red)))
            } else {
                Line::from(trimmed)
            }
        })
        .collect();
    let body = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" log "));
    frame.render_widget(body, chunks[1]);

    let hint = match result {
        None => " building...",
        Some(Ok(())) => " done · Enter to close ",
        Some(Err(_)) => " failed · Esc/Enter to close ",
    };
    let p = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(p, chunks[2]);
}

/// Truncate a string to a max display-column width, appending `…` if
/// shortened. Strips control characters (including embedded \n, \r, and
/// ANSI escapes) and uses Unicode display width so wide glyphs (emoji,
/// CJK) are counted as the 2 columns they actually occupy on screen.
/// Without this, build-output lines with control codes or wide chars
/// would render past the modal border.
fn truncate_to(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max == 0 {
        return String::new();
    }
    let limit = max.saturating_sub(1); // reserve one cell for the ellipsis
    let mut width = 0usize;
    let mut out = String::new();
    let mut truncated = false;
    for c in s.chars() {
        // Drop anything that could cause the terminal to do something other
        // than print a glyph in its own cell.
        if c.is_control() && c != ' ' {
            continue;
        }
        let cw = c.width().unwrap_or(0);
        if cw == 0 {
            continue;
        }
        if width + cw > limit {
            truncated = true;
            break;
        }
        width += cw;
        out.push(c);
    }
    if truncated {
        out.push('…');
    }
    out
}

fn centred(area: Rect, max_w: u16, max_h: u16) -> Rect {
    let w = max_w.min(area.width.saturating_sub(2));
    let h = max_h.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
