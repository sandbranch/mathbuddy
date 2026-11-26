//! Pop-ups in the style of crush's dialogs: a rounded box, a gradient title,
//! a filter you type into, grouped items, the chosen row on the primary
//! colour, buttons, and the keys at the bottom. Mouse and keyboard.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use super::text::*;

pub enum Outcome<T> {
    /// Still open.
    Open,
    Closed,
    Chosen(T),
}

/// Centred rectangle of at most `w` x `h` inside `area`.
pub fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(10);
    let h = h.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// A title in crush's Charple-to-Dolly gradient, followed by ╱ to `width`.
fn gradient_title(title: &str, width: usize) -> Line<'static> {
    let (a, b) = ((0x6Bu8, 0x50u8, 0xFFu8), (0xFFu8, 0x60u8, 0xFFu8));
    let fill = width.saturating_sub(title.width() + 1);
    let text: Vec<char> = title
        .chars()
        .chain(std::iter::once(' '))
        .chain(std::iter::repeat_n('╱', fill))
        .collect();
    let n = text.len().max(2) - 1;
    let spans = text
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let t = i as f32 / n as f32;
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            let st = Style::default().fg(Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2)));
            Span::styled(
                c.to_string(),
                if i < title.chars().count() {
                    st.add_modifier(Modifier::BOLD)
                } else {
                    st
                },
            )
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn frame_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PRIMARY))
}

fn keys_line(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, d)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" • ", Style::default().fg(RULE)));
        }
        spans.push(Span::styled(k.to_string(), Style::default().fg(SUBTLE)));
        spans.push(Span::styled(
            format!(" {d}"),
            Style::default().fg(MOST_SUBTLE),
        ));
    }
    Line::from(spans)
}

/// Every typed character appears in order (fuzzy, like crush's filter).
pub fn matches(query: &str, hay: &str) -> bool {
    let hay = hay.to_lowercase();
    let mut it = hay.chars();
    query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|q| it.any(|h| h == q))
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub group: String,
    pub current: bool,
    pub value: usize,
}

/// A filterable, grouped list: models, sessions, kernels, yes/no.
pub struct Picker {
    pub title: String,
    pub placeholder: String,
    pub items: Vec<Item>,
    query: String,
    sel: usize,
    top: usize,
    /// Screen rows of the visible items, for clicks: (row, index into visible).
    hits: Vec<(u16, Rect, usize)>,
}

impl Picker {
    pub fn new(title: &str, placeholder: &str, items: Vec<Item>) -> Self {
        let sel = items.iter().position(|i| i.current).unwrap_or(0);
        Picker {
            title: title.into(),
            placeholder: placeholder.into(),
            items,
            query: String::new(),
            sel,
            top: 0,
            hits: Vec::new(),
        }
    }

    pub fn start_at(mut self, i: usize) -> Self {
        self.sel = i;
        self
    }

    fn visible(&self) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|it| {
                matches(
                    &self.query,
                    &format!("{} {} {}", it.label, it.detail, it.group),
                )
            })
            .collect()
    }

    fn rows(&self) -> Vec<(bool, usize, String)> {
        // (is heading, index into visible, text)
        let mut rows = Vec::new();
        let mut last = None;
        for (i, it) in self.visible().iter().enumerate() {
            if !it.group.is_empty() && last != Some(it.group.clone()) {
                rows.push((true, i, it.group.clone()));
                last = Some(it.group.clone());
            }
            rows.push((false, i, it.label.clone()));
        }
        rows
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        const MAX_ROWS: usize = 12;
        let vis_len = self.visible().len();
        self.sel = self.sel.min(vis_len.saturating_sub(1));
        let rows = self.rows();
        let shown = rows.len().clamp(1, MAX_ROWS);
        let r = centred(area, 74, shown as u16 + 8);
        f.render_widget(Clear, r);
        f.render_widget(frame_block(), r);
        let inner = Rect {
            x: r.x + 2,
            y: r.y + 1,
            width: r.width.saturating_sub(4),
            height: r.height.saturating_sub(2),
        };
        let w = inner.width as usize;
        f.render_widget(
            Paragraph::new(gradient_title(&self.title, w)),
            Rect { height: 1, ..inner },
        );
        let input = if self.query.is_empty() {
            Line::from(vec![
                Span::styled(
                    "> ",
                    Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.placeholder.clone(), Style::default().fg(MOST_SUBTLE)),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    "> ",
                    Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.query.clone(), Style::default().fg(FG)),
            ])
        };
        f.render_widget(
            Paragraph::new(input),
            Rect {
                y: inner.y + 1,
                height: 1,
                ..inner
            },
        );
        f.set_cursor_position(Position {
            x: inner.x + 2 + self.query.width() as u16,
            y: inner.y + 1,
        });
        // keep the selection in view
        let sel_row = rows
            .iter()
            .position(|(h, i, _)| !h && *i == self.sel)
            .unwrap_or(0);
        if sel_row < self.top {
            self.top = if sel_row > 0 && rows[sel_row - 1].0 {
                sel_row - 1
            } else {
                sel_row
            };
        }
        if sel_row >= self.top + MAX_ROWS {
            self.top = sel_row + 1 - MAX_ROWS;
        }
        let mut hits = Vec::new();
        let list_y = inner.y + 3;
        let vis = self.visible();
        if vis.is_empty() {
            let p = Paragraph::new(Line::styled(
                "nothing matches",
                Style::default()
                    .fg(MOST_SUBTLE)
                    .add_modifier(Modifier::ITALIC),
            ));
            f.render_widget(
                p,
                Rect {
                    y: list_y,
                    height: 1,
                    ..inner
                },
            );
        }
        for (n, (heading, i, text)) in rows.iter().skip(self.top).take(MAX_ROWS).enumerate() {
            let y = list_y + n as u16;
            let row = Rect {
                y,
                height: 1,
                ..inner
            };
            if *heading {
                let rule = "─".repeat(w.saturating_sub(text.width() + 1));
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(format!("{text} "), Style::default().fg(SUBTLE)),
                        Span::styled(rule, Style::default().fg(RULE)),
                    ])),
                    row,
                );
                continue;
            }
            let it = vis[*i];
            let mark = if it.current { "✓ " } else { "  " };
            let gap = w
                .saturating_sub(2 + mark.width() + text.width() + it.detail.width())
                .max(1);
            let line = if *i == self.sel {
                let st = Style::default().bg(PRIMARY).fg(ON_PRIMARY);
                Line::from(vec![
                    Span::styled(format!(" {mark}{text}{}", " ".repeat(gap)), st),
                    Span::styled(format!("{} ", it.detail), st.add_modifier(Modifier::ITALIC)),
                ])
            } else {
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(mark, Style::default().fg(GOOD)),
                    Span::styled(text.clone(), Style::default().fg(FG)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(it.detail.clone(), Style::default().fg(MOST_SUBTLE)),
                ])
            };
            f.render_widget(Paragraph::new(line), row);
            hits.push((y, row, *i));
        }
        drop(vis);
        self.hits = hits;
        let more = rows.len().saturating_sub(self.top + MAX_ROWS);
        let foot_y = r.y + r.height - 2;
        if more > 0 {
            f.render_widget(
                Paragraph::new(Line::styled(
                    format!("  ↓ {more} more"),
                    Style::default().fg(MOST_SUBTLE),
                )),
                Rect {
                    y: foot_y - 1,
                    height: 1,
                    ..inner
                },
            );
        }
        f.render_widget(
            Paragraph::new(keys_line(&[
                ("↑↓", "choose"),
                ("enter", "confirm"),
                ("click", "pick"),
                ("esc", "cancel"),
            ])),
            Rect {
                y: foot_y,
                height: 1,
                ..inner
            },
        );
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome<usize> {
        let n = self.visible().len();
        match k.code {
            KeyCode::Esc => return Outcome::Closed,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                return Outcome::Closed;
            }
            KeyCode::Enter => {
                return self
                    .visible()
                    .get(self.sel)
                    .map(|i| Outcome::Chosen(i.value))
                    .unwrap_or(Outcome::Closed);
            }
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sel = self.sel.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Tab => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Backspace => {
                self.query.pop();
                self.sel = 0;
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.sel = 0;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.sel = 0;
            }
            _ => {}
        }
        Outcome::Open
    }

    pub fn mouse(&mut self, m: MouseEvent) -> Outcome<usize> {
        let n = self.visible().len();
        match m.kind {
            MouseEventKind::ScrollUp => self.sel = self.sel.saturating_sub(1),
            MouseEventKind::ScrollDown => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, _, i)) = self
                    .hits
                    .iter()
                    .find(|(y, r, _)| *y == m.row && m.column >= r.x && m.column < r.x + r.width)
                {
                    let i = *i;
                    return self
                        .visible()
                        .get(i)
                        .map(|it| Outcome::Chosen(it.value))
                        .unwrap_or(Outcome::Open);
                }
            }
            _ => {}
        }
        Outcome::Open
    }
}

/// A row of buttons; remembers where they are for clicks.
struct Buttons {
    labels: Vec<&'static str>,
    sel: usize,
    hits: Vec<Rect>,
}

impl Buttons {
    fn draw(&mut self, f: &mut Frame, row: Rect) {
        let total: usize = self.labels.iter().map(|l| l.width() + 6).sum::<usize>()
            + self.labels.len().saturating_sub(1);
        let mut x = row.x + (row.width.saturating_sub(total as u16)) / 2;
        self.hits.clear();
        for (i, l) in self.labels.iter().enumerate() {
            let text = format!("   {l}   ");
            let w = text.width() as u16;
            let st = if i == self.sel {
                Style::default().bg(SECONDARY).fg(ON_PRIMARY)
            } else {
                Style::default().bg(RULE).fg(FG)
            };
            let r = Rect {
                x,
                y: row.y,
                width: w.min(row.right().saturating_sub(x)),
                height: 1,
            };
            f.render_widget(Paragraph::new(Span::styled(text, st)), r);
            self.hits.push(r);
            x += w + 1;
        }
    }

    fn hit(&self, m: &MouseEvent) -> Option<usize> {
        self.hits
            .iter()
            .position(|r| m.row == r.y && m.column >= r.x && m.column < r.x + r.width)
    }
}

/// What to do with code the model wants to run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    Run,
    Skip,
    Always,
}

pub struct Approval {
    pub code: String,
    scroll: usize,
    buttons: Buttons,
}

impl Approval {
    pub fn new(code: String) -> Self {
        Approval {
            code,
            scroll: 0,
            buttons: Buttons {
                labels: vec!["Run it", "Don't run", "Always"],
                sel: 0,
                hits: Vec::new(),
            },
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let lines: Vec<&str> = self.code.lines().collect();
        let code_h = lines.len().clamp(1, 16) as u16;
        let r = centred(area, 84, code_h + 9);
        f.render_widget(Clear, r);
        f.render_widget(frame_block(), r);
        let inner = Rect {
            x: r.x + 2,
            y: r.y + 1,
            width: r.width.saturating_sub(4),
            height: r.height.saturating_sub(2),
        };
        f.render_widget(
            Paragraph::new(gradient_title(
                "Run this in your session?",
                inner.width as usize,
            )),
            Rect { height: 1, ..inner },
        );
        let code_area = Rect {
            y: inner.y + 2,
            height: code_h.min(inner.height.saturating_sub(6)),
            ..inner
        };
        self.scroll = self
            .scroll
            .min(lines.len().saturating_sub(code_area.height as usize));
        let shown: Vec<Line> = lines
            .iter()
            .skip(self.scroll)
            .take(code_area.height as usize)
            .map(|l| {
                Line::from(vec![
                    Span::styled("│ ", Style::default().fg(SECONDARY)),
                    Span::styled(l.to_string(), Style::default().fg(FG)),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(shown), code_area);
        let more = lines
            .len()
            .saturating_sub(self.scroll + code_area.height as usize);
        let note = if more > 0 {
            format!("↓ {more} more lines (wheel or PgDn)")
        } else {
            String::new()
        };
        f.render_widget(
            Paragraph::new(Line::styled(note, Style::default().fg(MOST_SUBTLE))),
            Rect {
                y: code_area.bottom(),
                height: 1,
                ..inner
            },
        );
        self.buttons.draw(
            f,
            Rect {
                y: inner.bottom().saturating_sub(3),
                height: 1,
                ..inner
            },
        );
        f.render_widget(
            Paragraph::new(keys_line(&[
                ("y", "run"),
                ("n", "don't"),
                ("a", "always this session"),
                ("←→", "choose"),
                ("enter", "confirm"),
            ])),
            Rect {
                y: inner.bottom().saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }

    fn pick(i: usize) -> Decision {
        [Decision::Run, Decision::Skip, Decision::Always][i.min(2)]
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome<Decision> {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Chosen(Decision::Run),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Outcome::Chosen(Decision::Skip)
            }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                Outcome::Chosen(Decision::Skip)
            }
            KeyCode::Char('a') | KeyCode::Char('A') => Outcome::Chosen(Decision::Always),
            KeyCode::Left | KeyCode::BackTab => {
                self.buttons.sel = self.buttons.sel.saturating_sub(1);
                Outcome::Open
            }
            KeyCode::Right | KeyCode::Tab => {
                self.buttons.sel = (self.buttons.sel + 1).min(2);
                Outcome::Open
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                Outcome::Open
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.scroll += if k.code == KeyCode::Down { 1 } else { 10 };
                Outcome::Open
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Outcome::Open
            }
            KeyCode::Enter | KeyCode::Char(' ') => Outcome::Chosen(Self::pick(self.buttons.sel)),
            _ => Outcome::Open,
        }
    }

    pub fn mouse(&mut self, m: MouseEvent) -> Outcome<Decision> {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(1),
            MouseEventKind::ScrollDown => self.scroll += 1,
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = self.buttons.hit(&m) {
                    return Outcome::Chosen(Self::pick(i));
                }
            }
            _ => {}
        }
        Outcome::Open
    }
}

/// crush's quit dialog: Yep! / Nope, Yep! chosen. Ctrl-C again (or y) quits.
pub struct Quit {
    buttons: Buttons,
}

impl Default for Quit {
    fn default() -> Self {
        Quit {
            buttons: Buttons {
                labels: vec!["Yep!", "Nope"],
                sel: 0,
                hits: Vec::new(),
            },
        }
    }
}

impl Quit {
    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let r = centred(area, 38, 11);
        f.render_widget(Clear, r);
        f.render_widget(frame_block(), r);
        let inner = Rect {
            x: r.x + 1,
            y: r.y + 1,
            width: r.width.saturating_sub(2),
            height: r.height.saturating_sub(2),
        };
        let centre = |s: &str, st: Style| {
            Paragraph::new(Line::styled(s.to_string(), st))
                .alignment(ratatui::layout::Alignment::Center)
        };
        f.render_widget(
            centre("Are you sure you want to quit?", Style::default().fg(FG)),
            Rect {
                y: inner.y + 1,
                height: 1,
                ..inner
            },
        );
        self.buttons.draw(
            f,
            Rect {
                y: inner.y + 3,
                height: 1,
                ..inner
            },
        );
        let hint = Style::default().fg(MOST_SUBTLE);
        f.render_widget(
            centre("To quit without confirmation", hint),
            Rect {
                y: inner.y + 5,
                height: 1,
                ..inner
            },
        );
        f.render_widget(
            centre("press ctrl+c twice.", hint),
            Rect {
                y: inner.y + 6,
                height: 1,
                ..inner
            },
        );
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome<bool> {
        match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                Outcome::Chosen(true)
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Chosen(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Outcome::Chosen(false),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.buttons.sel = 1 - self.buttons.sel;
                Outcome::Open
            }
            KeyCode::Enter | KeyCode::Char(' ') => Outcome::Chosen(self.buttons.sel == 0),
            _ => Outcome::Open,
        }
    }

    pub fn mouse(&mut self, m: MouseEvent) -> Outcome<bool> {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind
            && let Some(i) = self.buttons.hit(&m)
        {
            return Outcome::Chosen(i == 0);
        }
        Outcome::Open
    }
}

/// A scrollable text pop-up (help, a variable's description, /cells).
pub struct TextBox {
    pub title: String,
    pub lines: Vec<String>,
    scroll: usize,
}

impl TextBox {
    pub fn new(title: &str, text: &str) -> Self {
        TextBox {
            title: title.into(),
            lines: text.lines().map(String::from).collect(),
            scroll: 0,
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let h = (self.lines.len() as u16 + 5).min(area.height.saturating_sub(2));
        let r = centred(area, 92, h);
        f.render_widget(Clear, r);
        f.render_widget(frame_block(), r);
        let inner = Rect {
            x: r.x + 2,
            y: r.y + 1,
            width: r.width.saturating_sub(4),
            height: r.height.saturating_sub(2),
        };
        f.render_widget(
            Paragraph::new(gradient_title(&self.title, inner.width as usize)),
            Rect { height: 1, ..inner },
        );
        let body = Rect {
            y: inner.y + 2,
            height: inner.height.saturating_sub(3),
            ..inner
        };
        self.scroll = self
            .scroll
            .min(self.lines.len().saturating_sub(body.height as usize));
        let shown: Vec<Line> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(body.height as usize)
            .map(|l| Line::styled(l.clone(), Style::default().fg(FG)))
            .collect();
        f.render_widget(Paragraph::new(shown), body);
        f.render_widget(
            Paragraph::new(keys_line(&[("↑↓", "scroll"), ("esc", "close")])),
            Rect {
                y: inner.bottom().saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome<()> {
        match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Outcome::Closed,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Outcome::Closed,
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                Outcome::Open
            }
            KeyCode::Down => {
                self.scroll += 1;
                Outcome::Open
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Outcome::Open
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll += 10;
                Outcome::Open
            }
            _ => Outcome::Open,
        }
    }

    pub fn mouse(&mut self, m: MouseEvent) -> Outcome<()> {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(2),
            MouseEventKind::ScrollDown => self.scroll += 2,
            _ => {}
        }
        Outcome::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_filter() {
        assert!(matches("qw35", "qwen3.5:latest loaded ollama"));
        assert!(matches("", "anything"));
        assert!(!matches("53qw", "qwen3.5:latest"));
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn picker_filters_and_chooses() {
        let items = vec![
            Item {
                label: "alpha".into(),
                detail: String::new(),
                group: "g1".into(),
                current: false,
                value: 10,
            },
            Item {
                label: "beta".into(),
                detail: String::new(),
                group: "g1".into(),
                current: true,
                value: 20,
            },
            Item {
                label: "gamma".into(),
                detail: String::new(),
                group: "g2".into(),
                current: false,
                value: 30,
            },
        ];
        let mut p = Picker::new("t", "filter", items.clone());
        assert!(
            matches!(p.key(key(KeyCode::Enter)), Outcome::Chosen(20)),
            "starts on the current one"
        );
        let mut p = Picker::new("t", "filter", items.clone());
        for _ in 0..5 {
            p.key(key(KeyCode::Down));
        }
        assert!(
            matches!(p.key(key(KeyCode::Enter)), Outcome::Chosen(30)),
            "stops at the end, no wrap"
        );
        let mut p = Picker::new("t", "filter", items);
        for c in "gam".chars() {
            p.key(key(KeyCode::Char(c)));
        }
        assert!(matches!(p.key(key(KeyCode::Enter)), Outcome::Chosen(30)));
        assert!(matches!(p.key(key(KeyCode::Esc)), Outcome::Closed));
    }

    #[test]
    fn approval_and_quit_keys() {
        let mut a = Approval::new("x = 1".into());
        assert!(matches!(
            a.key(key(KeyCode::Char('a'))),
            Outcome::Chosen(Decision::Always)
        ));
        assert!(matches!(
            a.key(key(KeyCode::Esc)),
            Outcome::Chosen(Decision::Skip)
        ));
        a.key(key(KeyCode::Right));
        assert!(matches!(
            a.key(key(KeyCode::Enter)),
            Outcome::Chosen(Decision::Skip)
        ));
        let mut q = Quit::default();
        assert!(matches!(q.key(key(KeyCode::Enter)), Outcome::Chosen(true)));
        q.key(key(KeyCode::Right));
        assert!(matches!(q.key(key(KeyCode::Enter)), Outcome::Chosen(false)));
        assert!(matches!(
            q.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Outcome::Chosen(true)
        ));
    }
}
