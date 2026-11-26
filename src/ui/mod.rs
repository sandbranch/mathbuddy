//! The full-screen interface: conversation, input, a variables panel,
//! pop-up menus and dialogs, mouse and keyboard. Answers run in their own
//! task, so the screen stays live (scroll, stop with Ctrl-C) meanwhile.

mod dialogs;
mod input;
mod text;

use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use dialogs::{Approval, Decision, Item, Outcome, Picker, Quit, TextBox};
use futures::StreamExt;
use input::Input;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use text::*;
use tokio::sync::{mpsc, oneshot};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    assistant::{Assistant, Confirm, Event},
    core::Core,
    session::{CellDone, Found},
};

const HELP: &str =
    "Type a question and press Enter. Alt-Enter adds a new line; pasting a table or a \
traceback works as is.

The model can look at your variables on its own. Before it runs any code in your session it shows
the code and asks: run it, don't, or always (for this session).

Keys
  F1 help · F2 variables panel · F3 model · F4 session (or click them in the bottom bar)
  Drag over the conversation to copy text (Shift-drag: the terminal's own selection)
  PgUp/PgDn or the mouse wheel scroll the conversation · Ctrl-C stops an answer, or quits
  Up/Down walk your earlier questions

Commands
  /run [n]       run code block n of the last answer (default: the last one) in the session
  /show [n]      show code block n again
  /copy [n]      copy code block n to the clipboard, to paste into a cell
  /vars          list the session's variables (F2 keeps them on screen)
  /cells         show the cells mathbuddy has seen
  /sessions      pick a session to attach to (F4)
  /attach [words]  attach to the session matching words (rstudio, jotter, a notebook)
  /insert [n]    RStudio: put code block n into your open script at the cursor
  /console [n]   RStudio: send code block n to the console, as if you typed it
  /model [name]  pick a model on your Ollama and llama.cpp servers (F3), or name one
  /llamacpp ADDRESS  add a llama-server (one model or a router), saved in the config
  /hints on|off  one-line hints when a cell raises an error
  /clear         forget the conversation (not the cells)
  /help, /quit

mathbuddy remembers the model and the session you last chose. Settings (Ollama's address,
llama-servers, hints...) live in ~/.config/mathbuddy/config.toml; `mathbuddy doctor` shows them.";

/// What the bottom bar's buttons do.
#[derive(Clone, Copy)]
enum Button {
    Help,
    Vars,
    Model,
    Session,
}

enum PickerFor {
    Model(Vec<(usize, String)>),
    Session(Vec<Found>),
    LoadModel(usize, String),
}

enum Overlay {
    Picker(Picker, PickerFor),
    Approval(Approval, oneshot::Sender<bool>),
    Quit(Quit),
    Text(TextBox),
}

/// Results of work done off the UI loop.
enum Job {
    Answer(Assistant, Result<String>, bool),
    Hint(String, Result<String>),
    Ran(Result<(String, String)>),
    Vars(Result<String>),
    Inspected(String, Result<String>),
}

struct Ui {
    core: Core,
    show_thinking: bool,
    entries: Vec<Entry>,
    input: Input,
    /// Rows scrolled up from the bottom (0: follow the newest).
    scroll: usize,
    vars_open: bool,
    vars: Vec<String>,
    overlay: Option<Overlay>,
    answering: Option<Arc<AtomicBool>>,
    hinting: bool,
    always_run: Arc<AtomicBool>,
    events_tx: mpsc::UnboundedSender<Event>,
    confirm_tx: mpsc::UnboundedSender<(String, oneshot::Sender<bool>)>,
    jobs_tx: mpsc::UnboundedSender<Job>,
    buttons: Vec<(Rect, Button)>,
    var_rows: Vec<(u16, String)>,
    conversation: Rect,
    /// The conversation as plain text, a line per row, and the first row
    /// shown: what a mouse selection copies.
    conv_text: Vec<String>,
    conv_start: usize,
    /// Mouse selection in the conversation: anchor and end, as (line,
    /// column), and whether the button is still down.
    select: Option<((usize, usize), (usize, usize))>,
    selecting: bool,
    quit: bool,
    started: Instant,
    seen_cells: usize,
}

pub async fn run(
    core: Core,
    done_rx: mpsc::UnboundedReceiver<CellDone>,
    show_thinking: bool,
) -> Result<()> {
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let r = run_inner(&mut terminal, core, done_rx, show_thinking).await;
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    amux_status("idle", "");
    r
}

fn history_file() -> PathBuf {
    crate::config::state_dir().join("history.jsonl")
}

fn load_history() -> Vec<String> {
    std::fs::read_to_string(history_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<String>(l).ok())
        .collect()
}

fn save_history(h: &[String]) {
    let path = history_file();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let keep = &h[h.len().saturating_sub(500)..];
    let text: String = keep
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_default() + "\n")
        .collect();
    let _ = std::fs::write(path, text);
}

async fn run_inner(
    terminal: &mut DefaultTerminal,
    core: Core,
    mut done_rx: mpsc::UnboundedReceiver<CellDone>,
    show_thinking: bool,
) -> Result<()> {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (confirm_tx, mut confirm_rx) = mpsc::unbounded_channel();
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel();
    let always = core.always_run;
    let mut ui = Ui {
        core,
        show_thinking,
        entries: vec![Entry::Note(format!(
            "{}: /help or F1 for commands",
            crate::setup::version_string()
        ))],
        input: Input::with_history(load_history()),
        scroll: 0,
        vars_open: false,
        vars: Vec::new(),
        overlay: None,
        answering: None,
        hinting: false,
        always_run: Arc::new(AtomicBool::new(always)),
        events_tx,
        confirm_tx,
        jobs_tx,
        buttons: Vec::new(),
        var_rows: Vec::new(),
        conversation: Rect::default(),
        conv_text: Vec::new(),
        conv_start: 0,
        select: None,
        selecting: false,
        quit: false,
        started: Instant::now(),
        seen_cells: 0,
    };
    terminal.draw(|f| ui.draw(f))?;
    ui.core.choose_model().await;
    let target = ui.core.settings.session.clone();
    if ui.core.attach(target.as_deref(), false).await && target.is_some() {
        ui.core.remember_session();
    }
    ui.take_notes();
    amux_status("idle", "");

    let mut term_events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut frame = tokio::time::interval(Duration::from_millis(120));
    while !ui.quit {
        terminal.draw(|f| ui.draw(f))?;
        tokio::select! {
            Some(Ok(ev)) = term_events.next() => ui.on_terminal(ev).await,
            Some(ev) = events_rx.recv() => {
                ui.on_event(ev);
                while let Ok(ev) = events_rx.try_recv() {
                    ui.on_event(ev);
                }
            }
            Some((code, reply)) = confirm_rx.recv() => ui.on_confirm(code, reply),
            Some(job) = jobs_rx.recv() => {
                // the answer's last words may still be queued: show them first
                while let Ok(ev) = events_rx.try_recv() {
                    ui.on_event(ev);
                }
                ui.on_job(job);
            }
            Some(done) = done_rx.recv() => ui.on_cell_done(done),
            _ = tick.tick() => {
                if ui.answering.is_none() {
                    ui.core.tick().await;
                    ui.take_notes();
                }
                ui.refresh_vars_if_changed();
            }
            _ = frame.tick(), if ui.answering.is_some() || ui.hinting => {}
        }
    }
    save_history(ui.input.history());
    Ok(())
}

fn amux_status(state: &str, msg: &str) {
    if std::env::var("AMUX_ENV").as_deref() != Ok("1") {
        return;
    }
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(msg);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]7501;state={state}:msg={b}\x1b\\");
    let _ = out.flush();
}

/// A selection's two ends in reading order.
fn ordered((a, b): ((usize, usize), (usize, usize))) -> ((usize, usize), (usize, usize)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The text between two (line, column) positions, both ends included,
/// columns counted in terminal cells; trailing blanks dropped per line.
fn selected_text(lines: &[String], (a, b): ((usize, usize), (usize, usize))) -> String {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate().take(b.0 + 1).skip(a.0) {
        let from = if i == a.0 { a.1 } else { 0 };
        let to = if i == b.0 { b.1 + 1 } else { usize::MAX };
        let mut col = 0;
        let mut s = String::new();
        for ch in line.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if col >= from && col < to {
                s.push(ch);
            }
            col += w.max(1);
        }
        out.push(s.trim_end().to_string());
    }
    out.join("\n")
}

/// `line` with the cells in columns [from, to) shown reversed.
fn highlight(line: Line<'static>, from: usize, to: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut col = 0;
    let pad = line.spans.iter().map(|s| s.content.width()).sum::<usize>();
    for s in line.spans {
        for ch in s.content.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(0)
                .max(1);
            let style = if col >= from && col < to {
                s.style.add_modifier(Modifier::REVERSED)
            } else {
                s.style
            };
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(ch),
                _ => spans.push(Span::styled(ch.to_string(), style)),
            }
            col += w;
        }
    }
    // a selection running past the end of a line shows one cell there
    if from <= pad && to > pad {
        spans.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    Line::from(spans).style(line.style)
}

fn copy_to_clipboard(text: &str) -> String {
    for cmd in [
        vec!["wl-copy"],
        vec!["xclip", "-selection", "clipboard"],
        vec!["xsel", "-ib"],
    ] {
        if crate::setup::which(cmd[0]).is_none() {
            continue;
        }
        let child = std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut c) = child {
            let ok = c
                .stdin
                .take()
                .map(|mut s| s.write_all(text.as_bytes()).is_ok())
                .unwrap_or(false);
            if ok && c.wait().is_ok_and(|s| s.success()) {
                return cmd[0].to_string();
            }
        }
    }
    // OSC 52: the terminal (and tmux with set-clipboard on) takes it from here
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(text);
    let mut seq = format!("\x1b]52;c;{b}\x07");
    if std::env::var("TMUX").is_ok() {
        seq = format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"));
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
    "OSC 52".into()
}

impl Ui {
    fn take_notes(&mut self) {
        for n in self.core.notes.drain(..) {
            self.entries.push(Entry::Note(n));
        }
    }

    fn note(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Note(s.into()));
    }

    // -- drawing ----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let status_h = 1;
        let panel_w = if self.vars_open && area.width > 70 {
            38
        } else {
            0
        };
        let main = Rect {
            width: area.width - panel_w,
            height: area.height - status_h,
            ..area
        };
        let (rows, cursor) = self.input.layout(main.width as usize, 2);
        let input_h = (rows.len() as u16).clamp(1, 8) + 1;
        let conv = Rect {
            height: main.height.saturating_sub(input_h),
            ..main
        };
        let input_area = Rect {
            y: conv.bottom(),
            height: input_h,
            ..main
        };
        self.conversation = conv;
        self.draw_conversation(f, conv);
        self.draw_input(f, input_area, &rows, cursor);
        if panel_w > 0 {
            self.draw_vars(
                f,
                Rect {
                    x: main.right(),
                    width: panel_w,
                    height: main.height,
                    y: area.y,
                },
            );
        }
        self.draw_status(
            f,
            Rect {
                y: area.bottom() - 1,
                height: 1,
                ..area
            },
        );
        match &mut self.overlay {
            Some(Overlay::Picker(p, _)) => p.draw(f, area),
            Some(Overlay::Approval(a, _)) => a.draw(f, area),
            Some(Overlay::Quit(q)) => q.draw(f, area),
            Some(Overlay::Text(t)) => t.draw(f, area),
            None => {}
        }
    }

    fn draw_conversation(&mut self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();
        for e in &self.entries {
            lines.extend(e.lines(width, self.show_thinking));
        }
        if self.answering.is_some() {
            let dots =
                ["·  ", "·· ", "···"][(self.started.elapsed().as_millis() / 300 % 3) as usize];
            lines.push(Line::styled(
                dots.to_string(),
                Style::default().fg(SECONDARY),
            ));
        }
        let h = area.height as usize;
        let max_scroll = lines.len().saturating_sub(h);
        self.scroll = self.scroll.min(max_scroll);
        let start = lines.len().saturating_sub(h + self.scroll);
        self.conv_text = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        self.conv_start = start;
        let sel = self.select.map(ordered);
        let shown: Vec<Line> = lines
            .into_iter()
            .enumerate()
            .skip(start)
            .take(h)
            .map(|(i, l)| match sel {
                Some((a, b)) if i >= a.0 && i <= b.0 => {
                    let from = if i == a.0 { a.1 } else { 0 };
                    let to = if i == b.0 { b.1 + 1 } else { usize::MAX };
                    highlight(l, from, to)
                }
                _ => l,
            })
            .collect();
        let inner = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..area
        };
        f.render_widget(Paragraph::new(shown), inner);
        if self.scroll > 0 {
            let tag = format!(" ↓ {} more (PgDn) ", self.scroll);
            let w = tag.width() as u16;
            f.render_widget(
                Paragraph::new(Span::styled(tag, chosen())),
                Rect {
                    x: area.right().saturating_sub(w + 1),
                    y: area.bottom().saturating_sub(1),
                    width: w,
                    height: 1,
                },
            );
        }
    }

    fn draw_input(&self, f: &mut Frame, area: Rect, rows: &[String], cursor: (usize, usize)) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(RULE));
        f.render_widget(block, area);
        let body = Rect {
            y: area.y + 1,
            height: area.height - 1,
            ..area
        };
        let first_shown = (cursor.0 + 1).saturating_sub(body.height as usize);
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(first_shown)
            .take(body.height as usize)
            .map(|(i, r)| {
                let p = if i == 0 { "» " } else { "  " };
                Line::from(vec![
                    Span::styled(p, Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)),
                    Span::styled(r.clone(), Style::default().fg(FG)),
                ])
            })
            .collect();
        if self.input.text.is_empty() {
            let hint = if self.answering.is_some() {
                "answering... (Ctrl-C stops it)"
            } else {
                "ask about your session, or /help"
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "» ",
                        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(hint, Style::default().fg(MOST_SUBTLE)),
                ])),
                body,
            );
        } else {
            f.render_widget(Paragraph::new(lines), body);
        }
        if self.overlay.is_none() {
            let y = body.y + (cursor.0 - first_shown) as u16;
            f.set_cursor_position(Position {
                x: body.x + cursor.1 as u16,
                y,
            });
        }
    }

    fn draw_vars(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(RULE))
            .title(Span::styled(" variables ", Style::default().fg(SUBTLE)));
        f.render_widget(block, area);
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(3),
            height: area.height.saturating_sub(1),
        };
        self.var_rows.clear();
        let w = inner.width as usize;
        let mut y = inner.y;
        if self.vars.is_empty() {
            let t = if self.core.conn.is_none() {
                "(no session)"
            } else {
                "(none yet)"
            };
            f.render_widget(
                Paragraph::new(Span::styled(t, Style::default().fg(MOST_SUBTLE))),
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
            );
            return;
        }
        for v in &self.vars {
            if y >= inner.bottom().saturating_sub(1) {
                break;
            }
            let (name, rest) = v.split_once(':').unwrap_or((v.as_str(), ""));
            let rest = rest.trim();
            let mut spans = vec![Span::styled(
                name.to_string(),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )];
            let room = w.saturating_sub(name.width() + 1);
            if room > 2 && !rest.is_empty() {
                let r: String = rest.chars().take(room).collect();
                spans.push(Span::styled(
                    format!(" {r}"),
                    Style::default().fg(MOST_SUBTLE),
                ));
            }
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
            );
            self.var_rows.push((y, name.trim().to_string()));
            y += 1;
        }
        f.render_widget(
            Paragraph::new(Span::styled(
                "click one to look at it",
                Style::default().fg(MOST_SUBTLE),
            )),
            Rect {
                y: inner.bottom().saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }

    fn draw_status(&mut self, f: &mut Frame, area: Rect) {
        let left = format!(
            " mathbuddy · {} · {} · hints {} ",
            self.core.models.label(),
            self.core.session_label(),
            if self.core.hints { "on" } else { "off" }
        );
        // the buttons always show; the status text gives way to them
        let full = [
            (" F1 help ", Button::Help),
            (" F2 vars ", Button::Vars),
            (" F3 model ", Button::Model),
            (" F4 session ", Button::Session),
        ];
        let short = [
            (" F1 ", Button::Help),
            (" F2 ", Button::Vars),
            (" F3 ", Button::Model),
            (" F4 ", Button::Session),
        ];
        let width =
            |ls: &[(&str, Button)]| -> u16 { ls.iter().map(|(l, _)| l.width() as u16 + 1).sum() };
        let labels = if area.width >= width(&full) + 20 {
            full
        } else {
            short
        };
        let total = width(&labels).min(area.width);
        let room = (area.width - total) as usize;
        let text = fit(&left, room);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{text:<w$}", w = area.width as usize),
                chosen(),
            )),
            area,
        );
        self.buttons.clear();
        let mut x = area.right() - total;
        for (l, b) in labels {
            let w = l.width() as u16;
            if x + w > area.right() {
                break;
            }
            let on = matches!(b, Button::Vars) && self.vars_open;
            let bst = if on {
                Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let r = Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            };
            f.render_widget(Paragraph::new(Span::styled(l, bst)), r);
            self.buttons.push((r, b));
            x += w + 1;
        }
    }

    // -- input ----------------------------------------------------------------

    async fn on_terminal(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k).await,
            TermEvent::Mouse(m) => self.on_mouse(m).await,
            TermEvent::Paste(s) if self.overlay.is_none() => self.input.insert_str(&s),
            _ => {}
        }
    }

    async fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        self.select = None;
        if let Some(ov) = self.overlay.take() {
            self.overlay_key(ov, k).await;
            return;
        }
        match k.code {
            KeyCode::Char('c') if ctrl => {
                if let Some(cancel) = &self.answering {
                    cancel.store(true, Ordering::Relaxed);
                    self.note("(stopping...)");
                } else if !self.input.text.is_empty() {
                    self.input.take();
                } else {
                    self.overlay = Some(Overlay::Quit(Quit::default()));
                }
            }
            KeyCode::Char('d') if ctrl && self.input.text.is_empty() => self.quit = true,
            KeyCode::Enter
                if k.modifiers.contains(KeyModifiers::ALT)
                    || k.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.input.insert('\n')
            }
            KeyCode::Enter => {
                let line = self.input.take();
                let line = line.trim().to_string();
                if !line.is_empty() {
                    self.scroll = 0;
                    self.submit(line).await;
                }
            }
            KeyCode::F(1) => self.press(Button::Help).await,
            KeyCode::F(2) => self.press(Button::Vars).await,
            KeyCode::F(3) => self.press(Button::Model).await,
            KeyCode::F(4) => self.press(Button::Session).await,
            KeyCode::PageUp => self.scroll += (self.conversation.height as usize / 2).max(1),
            KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_sub((self.conversation.height as usize / 2).max(1))
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up => self.input.up(),
            KeyCode::Down => self.input.down(),
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('u') if ctrl => self.input.kill_line(),
            KeyCode::Char('l') if ctrl => self.scroll = 0,
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            _ => {}
        }
    }

    /// Where a mouse position falls in the conversation text: (line,
    /// column), clamped to it.
    fn text_pos(&self, column: u16, row: u16) -> (usize, usize) {
        let c = self.conversation;
        let row = row.clamp(c.y, c.bottom().saturating_sub(1)) - c.y;
        let line = (self.conv_start + row as usize).min(self.conv_text.len().saturating_sub(1));
        (line, column.saturating_sub(c.x + 1) as usize)
    }

    async fn on_mouse(&mut self, m: MouseEvent) {
        if let Some(ov) = self.overlay.take() {
            self.overlay_mouse(ov, m).await;
            return;
        }
        let inside =
            |r: Rect| m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom();
        // selecting text in the conversation: press, drag, let go to copy
        match m.kind {
            MouseEventKind::Down(MouseButton::Left)
                if inside(self.conversation) && !self.conv_text.is_empty() =>
            {
                let p = self.text_pos(m.column, m.row);
                self.select = Some((p, p));
                self.selecting = true;
                return;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                // past the edge: scroll, so a selection can be longer than the screen
                if m.row < self.conversation.y {
                    self.scroll += 1;
                } else if m.row >= self.conversation.bottom() {
                    self.scroll = self.scroll.saturating_sub(1);
                }
                if let Some((a, _)) = self.select {
                    self.select = Some((a, self.text_pos(m.column, m.row)));
                }
                return;
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting => {
                self.selecting = false;
                match self.select {
                    Some((a, b)) if a != b => {
                        let text = selected_text(&self.conv_text, ordered((a, b)));
                        if !text.trim().is_empty() {
                            let how = copy_to_clipboard(&text);
                            self.core.say(format!(
                                "(copied {} characters, {how})",
                                text.chars().count()
                            ));
                            self.take_notes();
                        }
                    }
                    _ => self.select = None,
                }
                return;
            }
            _ => {}
        }
        match m.kind {
            MouseEventKind::ScrollUp if inside(self.conversation) => self.scroll += 3,
            MouseEventKind::ScrollDown if inside(self.conversation) => {
                self.scroll = self.scroll.saturating_sub(3)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, b)) = self.buttons.iter().find(|(r, _)| inside(*r)).copied() {
                    self.press(b).await;
                } else if let Some((_, name)) = self
                    .var_rows
                    .iter()
                    .find(|(y, _)| *y == m.row && m.column > self.conversation.right())
                    .cloned()
                {
                    self.inspect(name);
                }
            }
            _ => {}
        }
    }

    async fn press(&mut self, b: Button) {
        match b {
            Button::Help => self.overlay = Some(Overlay::Text(TextBox::new("Help", HELP))),
            Button::Vars => {
                self.vars_open = !self.vars_open;
                if self.vars_open {
                    self.refresh_vars();
                }
            }
            Button::Model => self.open_model_picker().await,
            Button::Session => self.open_session_picker().await,
        }
    }

    async fn overlay_key(&mut self, ov: Overlay, k: KeyEvent) {
        match ov {
            Overlay::Picker(mut p, kind) => match p.key(k) {
                Outcome::Open => self.overlay = Some(Overlay::Picker(p, kind)),
                Outcome::Closed => {}
                Outcome::Chosen(v) => self.picked(kind, v).await,
            },
            Overlay::Approval(mut a, reply) => match a.key(k) {
                Outcome::Chosen(d) => self.decide(d, reply),
                _ => self.overlay = Some(Overlay::Approval(a, reply)),
            },
            Overlay::Quit(mut q) => match q.key(k) {
                Outcome::Chosen(yes) => self.quit = yes,
                _ => self.overlay = Some(Overlay::Quit(q)),
            },
            Overlay::Text(mut t) => {
                if let Outcome::Open = t.key(k) {
                    self.overlay = Some(Overlay::Text(t))
                }
            }
        }
    }

    async fn overlay_mouse(&mut self, ov: Overlay, m: MouseEvent) {
        match ov {
            Overlay::Picker(mut p, kind) => match p.mouse(m) {
                Outcome::Chosen(v) => self.picked(kind, v).await,
                _ => self.overlay = Some(Overlay::Picker(p, kind)),
            },
            Overlay::Approval(mut a, reply) => match a.mouse(m) {
                Outcome::Chosen(d) => self.decide(d, reply),
                _ => self.overlay = Some(Overlay::Approval(a, reply)),
            },
            Overlay::Quit(mut q) => match q.mouse(m) {
                Outcome::Chosen(yes) => self.quit = yes,
                _ => self.overlay = Some(Overlay::Quit(q)),
            },
            Overlay::Text(mut t) => {
                t.mouse(m);
                self.overlay = Some(Overlay::Text(t));
            }
        }
    }

    // -- menus ----------------------------------------------------------------

    async fn open_model_picker(&mut self) {
        let mut items = Vec::new();
        let mut map = Vec::new();
        for (i, cat) in self.core.catalog().await {
            let s = &self.core.models.servers[i];
            let group = format!(
                "{} · {}",
                s.kind.name(),
                s.base.split("://").nth(1).unwrap_or(&s.base)
            );
            match cat {
                Err(e) => items.push(Item {
                    label: "not reachable".into(),
                    detail: e.chars().take(40).collect(),
                    group,
                    current: false,
                    value: usize::MAX,
                }),
                Ok(models) => {
                    for (name, loaded) in models {
                        let current = i == self.core.models.current
                            && name == self.core.models.server().model;
                        items.push(Item {
                            label: name.clone(),
                            detail: if loaded {
                                "loaded".into()
                            } else {
                                String::new()
                            },
                            group: group.clone(),
                            current,
                            value: map.len(),
                        });
                        map.push((i, name));
                    }
                }
            }
        }
        self.overlay = Some(Overlay::Picker(
            Picker::new("Switch model", "Type to filter models", items),
            PickerFor::Model(map),
        ));
    }

    async fn open_session_picker(&mut self) {
        let found = self.core.find_sessions(true).await;
        self.core.known.extend(found.iter().map(|s| s.path.clone()));
        if found.is_empty() {
            self.note("(no sessions found: start jotter, or R/RStudio with the bridge loaded; `mathbuddy setup-r` sets that up)");
            return;
        }
        let cur = self.core.conn.as_ref().map(|c| c.path().to_path_buf());
        let items = found
            .iter()
            .enumerate()
            .map(|(i, s)| Item {
                label: s.title(),
                detail: if s.score > 0 {
                    s.reason.clone()
                } else {
                    String::new()
                },
                group: s.language.clone(),
                current: Some(&s.path) == cur.as_ref(),
                value: i,
            })
            .collect();
        self.overlay = Some(Overlay::Picker(
            Picker::new(
                "Attach to a session",
                "Type to filter: rstudio, jotter, a notebook",
                items,
            ),
            PickerFor::Session(found),
        ));
    }

    async fn picked(&mut self, kind: PickerFor, v: usize) {
        match kind {
            PickerFor::Model(map) => {
                let Some((server, name)) = map.get(v).cloned() else {
                    return;
                };
                if let Some(warning) = self.core.llamacpp_swap_warning(server, &name).await {
                    self.note(warning);
                    let items = vec![
                        Item {
                            label: format!("Yes, switch to {name}"),
                            detail: String::new(),
                            group: String::new(),
                            current: false,
                            value: 1,
                        },
                        Item {
                            label: format!("No, stay with {}", self.core.models.server().model),
                            detail: String::new(),
                            group: String::new(),
                            current: false,
                            value: 0,
                        },
                    ];
                    let p = Picker::new(&format!("Load {name}?"), "Choose", items).start_at(1);
                    self.overlay = Some(Overlay::Picker(p, PickerFor::LoadModel(server, name)));
                    return;
                }
                self.core.switch_model(server, &name);
                self.take_notes();
            }
            PickerFor::LoadModel(server, name) => {
                if v == 1 {
                    self.core.switch_model(server, &name);
                } else {
                    let label = self.core.models.label();
                    self.core.say(format!("(staying with {label})"));
                }
                self.take_notes();
            }
            PickerFor::Session(found) => {
                if let Some(s) = found.get(v).cloned() {
                    self.core.listing = found;
                    if self.core.attach_to(&s, false).await {
                        self.core.remember_session();
                    }
                    self.take_notes();
                    self.refresh_vars();
                }
            }
        }
    }

    // -- answering -----------------------------------------------------------

    async fn submit(&mut self, line: String) {
        if let Some(cmd) = line.strip_prefix('/') {
            self.command(cmd).await;
            return;
        }
        if self.answering.is_some() {
            self.note("(still answering; Ctrl-C stops it)");
            return;
        }
        self.entries.push(Entry::User(line.clone()));
        let cancel = Arc::new(AtomicBool::new(false));
        self.answering = Some(cancel.clone());
        amux_status("working", &line.chars().take(60).collect::<String>());
        let mut assistant = std::mem::take(&mut self.core.assistant);
        let conn = self.core.conn.clone();
        let server = self.core.models.server().clone();
        let (events, confirm_tx, jobs) = (
            self.events_tx.clone(),
            self.confirm_tx.clone(),
            self.jobs_tx.clone(),
        );
        tokio::spawn(async move {
            let confirm: Confirm = Box::new(move |code: String| {
                let confirm_tx = confirm_tx.clone();
                Box::pin(async move {
                    let (tx, rx) = oneshot::channel();
                    if confirm_tx.send((code, tx)).is_err() {
                        return false;
                    }
                    rx.await.unwrap_or(false)
                })
            });
            let r = assistant
                .ask(
                    &server,
                    conn.as_deref(),
                    &line,
                    events,
                    confirm,
                    cancel.clone(),
                )
                .await;
            let _ = jobs.send(Job::Answer(assistant, r, cancel.load(Ordering::Relaxed)));
        });
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Token(t) => match self.entries.last_mut() {
                Some(Entry::Answer(a)) => a.push_str(&t),
                _ => self.entries.push(Entry::Answer(t)),
            },
            Event::Thinking(t) => match self.entries.last_mut() {
                Some(Entry::Thinking(a)) => a.push_str(&t),
                _ => self.entries.push(Entry::Thinking(t)),
            },
            Event::Tool { name, arg } => {
                if name != "run_code" {
                    // the approval shows run_code's code
                    let a = if arg.is_empty() {
                        String::new()
                    } else {
                        format!("({arg})")
                    };
                    self.entries.push(Entry::Tool(format!("[{name}{a}]")));
                }
            }
            Event::ToolResult(t) => {
                if t.starts_with("status:") {
                    self.entries.push(Entry::Result(t)); // run_code output: the real numbers
                }
            }
        }
    }

    fn on_confirm(&mut self, code: String, reply: oneshot::Sender<bool>) {
        if self.always_run.load(Ordering::Relaxed) {
            self.entries.push(Entry::Run {
                code,
                verdict: Verdict::Always,
            });
            let _ = reply.send(true);
            return;
        }
        self.entries.push(Entry::Run {
            code: code.clone(),
            verdict: Verdict::Waiting,
        });
        self.scroll = 0;
        self.overlay = Some(Overlay::Approval(Approval::new(code), reply));
    }

    fn decide(&mut self, d: Decision, reply: oneshot::Sender<bool>) {
        let verdict = match d {
            Decision::Run => Verdict::Approved,
            Decision::Skip => Verdict::Declined,
            Decision::Always => {
                self.always_run.store(true, Ordering::Relaxed);
                Verdict::Always
            }
        };
        if let Some(Entry::Run { verdict: v, .. }) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Run { .. }))
        {
            *v = verdict.clone();
        }
        let _ = reply.send(verdict != Verdict::Declined);
    }

    fn on_job(&mut self, job: Job) {
        match job {
            Job::Answer(assistant, r, cancelled) => {
                self.core.assistant = assistant;
                self.answering = None;
                amux_status("idle", "");
                match r {
                    Ok(_) if cancelled => self.note("(stopped)"),
                    Ok(answer) => {
                        if let Some(h) = self.core.take_blocks(&answer) {
                            self.note(h);
                        }
                    }
                    Err(e) => self.entries.push(Entry::Error(format!("(error: {e})"))),
                }
                self.refresh_vars();
            }
            Job::Hint(count, r) => {
                self.hinting = false;
                amux_status("idle", "");
                match r {
                    Ok(text) if !text.is_empty() => self.entries.push(Entry::Hint { count, text }),
                    Ok(_) => {}
                    Err(e) => self.note(format!("(hint failed: {e})")),
                }
            }
            Job::Ran(r) => match r {
                Ok((status, text)) => {
                    let t = text.trim_end().to_string();
                    self.note(if t.is_empty() {
                        format!("(done, status {status}, no output)")
                    } else {
                        t
                    });
                    self.refresh_vars();
                }
                Err(e) => self
                    .entries
                    .push(Entry::Error(format!("(run failed: {e})"))),
            },
            Job::Vars(r) => {
                self.vars = match r {
                    Ok(t) => t
                        .lines()
                        .filter(|l| !l.trim().is_empty() && !l.starts_with('('))
                        .map(String::from)
                        .collect(),
                    Err(_) => Vec::new(),
                };
            }
            Job::Inspected(expr, r) => {
                let text = r.unwrap_or_else(|e| format!("(could not look at it: {e})"));
                self.overlay = Some(Overlay::Text(TextBox::new(&expr, &text)));
            }
        }
    }

    fn on_cell_done(&mut self, done: CellDone) {
        if !self.core.hints || self.answering.is_some() || self.hinting {
            return; // mid-answer: don't pile on
        }
        self.hinting = true;
        amux_status("working", "hint");
        let conn = self.core.conn.clone();
        let server = self.core.models.server().clone();
        let jobs = self.jobs_tx.clone();
        let count = done
            .cell
            .count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        tokio::spawn(async move {
            let r = Assistant::default()
                .hint(&server, conn.as_deref(), &done.cell)
                .await;
            let _ = jobs.send(Job::Hint(count, r));
        });
    }

    fn refresh_vars(&mut self) {
        if !self.vars_open {
            return;
        }
        let Some(conn) = self.core.conn.clone() else {
            self.vars.clear();
            return;
        };
        let jobs = self.jobs_tx.clone();
        tokio::spawn(async move {
            let _ = jobs.send(Job::Vars(conn.list_variables().await));
        });
    }

    /// New cells since the last look (the user ran something): refresh.
    fn refresh_vars_if_changed(&mut self) {
        let n = self
            .core
            .conn
            .as_ref()
            .map(|c| {
                c.recent(1000).len() + c.recent(1).first().map(|c| c.done as usize).unwrap_or(0)
            })
            .unwrap_or(0);
        if n != self.seen_cells {
            self.seen_cells = n;
            self.refresh_vars();
        }
    }

    fn inspect(&mut self, name: String) {
        let Some(conn) = self.core.conn.clone() else {
            return;
        };
        let jobs = self.jobs_tx.clone();
        tokio::spawn(async move {
            let r = conn.inspect(&name).await;
            let _ = jobs.send(Job::Inspected(name, r));
        });
    }

    // -- commands ---------------------------------------------------------------

    fn need_session(&mut self) -> Option<Arc<crate::session::Connection>> {
        if self.core.conn.is_none() {
            self.note("(no session attached yet; /sessions lists them)");
        }
        self.core.conn.clone()
    }

    async fn command(&mut self, line: &str) {
        let (cmd, arg) = line
            .split_once(' ')
            .map(|(c, a)| (c, a.trim()))
            .unwrap_or((line, ""));
        match cmd {
            "q" | "quit" | "exit" => self.quit = true,
            "h" | "help" | "?" => self.overlay = Some(Overlay::Text(TextBox::new("Help", HELP))),
            "show" => {
                if let Some(code) = self.core.pick_block(arg) {
                    self.entries
                        .push(Entry::Answer(format!("```\n{code}\n```")));
                }
                self.take_notes();
            }
            "copy" => {
                if let Some(code) = self.core.pick_block(arg) {
                    let via = copy_to_clipboard(&code);
                    self.note(format!("(copied via {via})"));
                }
                self.take_notes();
            }
            "run" => {
                let code = self.core.pick_block(arg);
                self.take_notes();
                if let (Some(code), Some(conn)) = (code, self.need_session()) {
                    let keep = if conn.rstudio_api() {
                        "/insert puts it in your script"
                    } else {
                        "/copy it to keep it"
                    };
                    self.note(format!(
                        "(running in your session: it changes your variables, but it isn't saved in your notebook or script; {keep})"
                    ));
                    let jobs = self.jobs_tx.clone();
                    tokio::spawn(async move {
                        let _ = jobs.send(Job::Ran(conn.run(&code).await));
                    });
                }
            }
            "vars" => {
                if let Some(conn) = self.need_session() {
                    match conn.list_variables().await {
                        Ok(v) => self.overlay = Some(Overlay::Text(TextBox::new("Variables", &v))),
                        Err(e) => self.note(format!("(could not list them: {e})")),
                    }
                }
            }
            "cells" => {
                if let Some(conn) = self.need_session() {
                    let cells: Vec<String> =
                        conn.recent(20).iter().map(|c| c.render(400, 300)).collect();
                    let text = if cells.is_empty() {
                        "(none yet)".to_string()
                    } else {
                        cells.join("\n\n")
                    };
                    self.overlay = Some(Overlay::Text(TextBox::new("Cells seen", &text)));
                }
            }
            "sessions" | "session" => self.open_session_picker().await,
            "attach" => {
                let target = if arg.is_empty() { None } else { Some(arg) };
                if self.core.attach(target, false).await && target.is_some() {
                    self.core.remember_session();
                }
                self.take_notes();
                self.refresh_vars();
            }
            "insert" | "console" => {
                let code = self.core.pick_block(arg);
                self.take_notes();
                if let (Some(code), Some(conn)) = (code, self.need_session()) {
                    match conn.to_editor(cmd == "console", &code).await {
                        Ok(t) => self.note(format!("({t})")),
                        Err(e) => self.note(format!("(/{cmd}: {e})")),
                    }
                }
            }
            "model" | "models" => {
                if arg.is_empty() {
                    self.open_model_picker().await;
                } else {
                    match self.core.locate(arg).await {
                        Some((server, name)) => {
                            self.picked(PickerFor::Model(vec![(server, name)]), 0).await
                        }
                        None => {
                            self.note(format!("(no model {arg:?} on your servers; F3 lists them)"))
                        }
                    }
                }
            }
            "llamacpp" => {
                if arg.is_empty() {
                    let hosts: Vec<String> = self
                        .core
                        .models
                        .servers
                        .iter()
                        .filter(|s| s.kind == crate::llm::ServerKind::LlamaCpp)
                        .map(|s| s.base.clone())
                        .collect();
                    if hosts.is_empty() {
                        self.note("(no llama-server yet: /llamacpp ADDRESS:PORT adds one)");
                    } else {
                        self.note(format!("(llama-servers: {})", hosts.join(", ")));
                    }
                } else {
                    self.core.add_llamacpp(arg).await;
                    self.take_notes();
                }
            }
            "hints" => {
                self.core.hints = arg != "off";
                let s = if self.core.hints { "on" } else { "off" };
                self.note(format!("(hints {s})"));
            }
            "clear" => {
                if self.answering.is_some() {
                    self.note("(still answering; Ctrl-C stops it first)");
                } else {
                    self.core.assistant.clear();
                    self.entries.clear();
                    self.note("(conversation cleared)");
                }
            }
            other => self.note(format!("(unknown command /{other}; /help lists them)")),
        }
    }
}

/// `s` cut to `max` columns, ending in an ellipsis when cut.
fn fit(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + c.width().unwrap_or(0) + 2 > max {
            break;
        }
        out.push(c);
    }
    if max >= 2 {
        out.push_str("… ");
    }
    out
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    #[test]
    fn selected_text_spans_lines_in_cells() {
        let lines = vec![
            "solve(x^2+2*x+1=0, x)".to_string(),
            "list[-1]  ".into(),
            "x · y".into(),
        ];
        // one line, both ends included
        assert_eq!(selected_text(&lines, ((0, 0), (0, 4))), "solve");
        // across lines, trailing blanks dropped
        assert_eq!(selected_text(&lines, ((0, 19), (1, 20))), "x)\nlist[-1]");
        // dragged backwards: the same text
        assert_eq!(
            selected_text(&lines, ordered(((2, 4), (2, 0)))),
            selected_text(&lines, ((2, 0), (2, 4)))
        );
        assert_eq!(selected_text(&lines, ((2, 0), (2, 4))), "x · y");
    }

    #[test]
    fn highlight_reverses_only_the_selection() {
        let l = Line::from(vec![
            Span::raw("ab"),
            Span::styled("cd", Style::default().fg(ratatui::style::Color::Red)),
        ]);
        let h = highlight(l, 1, 3);
        let rev: String = h
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(rev, "bc");
        let text: String = h.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abcd");
    }
}
