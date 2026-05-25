//! Per-agent attached terminal: drives a `podman attach` child through a pty,
//! feeds its output into a vt100 emulator, and forwards keystrokes back.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;
use ratatui::style::{Color as TuiColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::runtime::ContainerRuntime;

/// Bookkeeping for a single attached agent terminal.
pub struct AttachedTerm {
    pub container_name: String,
    parser: Arc<Mutex<vt100::Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    pub dirty: Arc<AtomicBool>,
    pub alive: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
}

impl AttachedTerm {
    pub fn attach(rt: &ContainerRuntime, container_name: &str) -> Result<Self> {
        let rows: u16 = 40;
        let cols: u16 = 120;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .with_context(|| format!("openpty for {}", container_name))?;

        let mut cmd = CommandBuilder::new(rt.cmd());
        cmd.arg("attach");
        // Pick a detach sequence the user is extremely unlikely to type.
        // The default ctrl-p,ctrl-q would silently detach the agent's input.
        cmd.arg("--detach-keys=ctrl-^,ctrl-^");
        cmd.arg(container_name);
        // Inherit no env from the parent — `podman` reads its own config.
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn podman attach {}", container_name))?;

        // Slave is now owned by the child process.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let dirty = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));

        // Reader task: blocking read of the pty master, feed bytes into the
        // vt100 parser, flip the dirty flag for the UI tick.
        let parser_reader = parser.clone();
        let dirty_reader = dirty.clone();
        let alive_reader = alive.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut p) = parser_reader.lock() {
                            p.process(&buf[..n]);
                        }
                        dirty_reader.store(true, Ordering::Release);
                    }
                    Err(_) => break,
                }
            }
            alive_reader.store(false, Ordering::Release);
            dirty_reader.store(true, Ordering::Release);
        });

        Ok(Self {
            container_name: container_name.to_string(),
            parser,
            master: pair.master,
            writer,
            _child: child,
            dirty,
            alive,
            rows,
            cols,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Adjust pty + parser to the given pane size (subtracting borders).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        let _ = self.master.resize(PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(self.rows, self.cols);
        }
        self.dirty.store(true, Ordering::Release);
    }

    /// Convert a crossterm KeyEvent into terminal bytes and write to the pty.
    pub fn send_key(&mut self, key: &KeyEvent) {
        let mut buf: Vec<u8> = Vec::with_capacity(8);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) => {
                if ctrl {
                    // Ctrl-A..Ctrl-Z → 0x01..0x1A
                    let lower = c.to_ascii_lowercase();
                    if ('a'..='z').contains(&lower) {
                        buf.push((lower as u8) - b'a' + 1);
                    } else {
                        // Fall back to literal char for unsupported ctrl combos.
                        buf.extend_from_slice(c.to_string().as_bytes());
                    }
                } else {
                    if alt {
                        buf.push(0x1b);
                    }
                    buf.extend_from_slice(c.to_string().as_bytes());
                }
            }
            KeyCode::Enter => buf.push(b'\r'),
            KeyCode::Tab => buf.push(b'\t'),
            KeyCode::BackTab => buf.extend_from_slice(b"\x1b[Z"),
            KeyCode::Backspace => buf.push(0x7f),
            KeyCode::Esc => buf.push(0x1b),
            KeyCode::Up => buf.extend_from_slice(b"\x1b[A"),
            KeyCode::Down => buf.extend_from_slice(b"\x1b[B"),
            KeyCode::Right => buf.extend_from_slice(b"\x1b[C"),
            KeyCode::Left => buf.extend_from_slice(b"\x1b[D"),
            KeyCode::Home => buf.extend_from_slice(b"\x1b[H"),
            KeyCode::End => buf.extend_from_slice(b"\x1b[F"),
            KeyCode::PageUp => buf.extend_from_slice(b"\x1b[5~"),
            KeyCode::PageDown => buf.extend_from_slice(b"\x1b[6~"),
            KeyCode::Delete => buf.extend_from_slice(b"\x1b[3~"),
            KeyCode::Insert => buf.extend_from_slice(b"\x1b[2~"),
            _ => {}
        }
        if buf.is_empty() {
            return;
        }
        let _ = self.writer.write_all(&buf);
        let _ = self.writer.flush();
    }

    /// Render the current vt100 screen into `area`, drawing a titled border
    /// around it. The cursor inside `area` is returned for the caller to
    /// position when this pane has focus.
    pub fn render(
        &mut self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        focused: bool,
        title: &str,
    ) -> Option<(u16, u16)> {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(if focused {
                Style::default().fg(TuiColor::Cyan)
            } else {
                Style::default().fg(TuiColor::DarkGray)
            })
            .title(title.to_string());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Resize the pty to match the inner pane if it changed.
        self.resize(inner.height, inner.width);

        let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);
        let cursor;
        {
            let parser = self.parser.lock().expect("parser lock poisoned");
            let screen = parser.screen();
            let (rows, cols) = (inner.height, inner.width);
            for row in 0..rows {
                let mut spans: Vec<Span> = Vec::new();
                for col in 0..cols {
                    let cell = screen.cell(row, col);
                    let (text, style) = match cell {
                        Some(c) => {
                            let mut style = Style::default();
                            style.fg = vt_color_to_ratatui(c.fgcolor());
                            style.bg = vt_color_to_ratatui(c.bgcolor());
                            let mut mods = Modifier::empty();
                            if c.bold() {
                                mods |= Modifier::BOLD;
                            }
                            if c.italic() {
                                mods |= Modifier::ITALIC;
                            }
                            if c.underline() {
                                mods |= Modifier::UNDERLINED;
                            }
                            if c.inverse() {
                                mods |= Modifier::REVERSED;
                            }
                            style.add_modifier = mods;
                            let s = c.contents().to_string();
                            let s = if s.is_empty() { " ".to_string() } else { s };
                            (s, style)
                        }
                        None => (" ".to_string(), Style::default()),
                    };
                    spans.push(Span::styled(text, style));
                }
                lines.push(Line::from(spans));
            }
            let (cy, cx) = screen.cursor_position();
            cursor = Some((cy, cx));
        }

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);

        self.dirty.store(false, Ordering::Release);

        if focused {
            cursor.and_then(|(cy, cx)| {
                if cy < inner.height && cx < inner.width {
                    Some((inner.x + cx, inner.y + cy))
                } else {
                    None
                }
            })
        } else {
            None
        }
    }
}

fn vt_color_to_ratatui(c: vt100::Color) -> Option<TuiColor> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(match i {
            0 => TuiColor::Black,
            1 => TuiColor::Red,
            2 => TuiColor::Green,
            3 => TuiColor::Yellow,
            4 => TuiColor::Blue,
            5 => TuiColor::Magenta,
            6 => TuiColor::Cyan,
            7 => TuiColor::Gray,
            8 => TuiColor::DarkGray,
            9 => TuiColor::LightRed,
            10 => TuiColor::LightGreen,
            11 => TuiColor::LightYellow,
            12 => TuiColor::LightBlue,
            13 => TuiColor::LightMagenta,
            14 => TuiColor::LightCyan,
            15 => TuiColor::White,
            n => TuiColor::Indexed(n),
        }),
        vt100::Color::Rgb(r, g, b) => Some(TuiColor::Rgb(r, g, b)),
    }
}
