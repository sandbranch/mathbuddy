//! The conversation, and how it turns into wrapped, styled lines.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// The terminal's own colours, so mathbuddy follows whatever theme it has.
pub const PRIMARY: Color = Color::Blue;
pub const SECONDARY: Color = Color::Cyan;
pub const FG: Color = Color::Reset;
pub const SUBTLE: Color = Color::Reset;
pub const MOST_SUBTLE: Color = Color::DarkGray;
pub const RULE: Color = Color::DarkGray;
pub const GOOD: Color = Color::Green;
pub const BAD: Color = Color::Red;
pub const CODE: Color = Color::DarkGray;

/// The chosen row or button: the terminal's colours swapped.
pub fn chosen() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Waiting,
    Approved,
    Always,
    Declined,
}

/// One thing in the conversation.
#[derive(Clone, Debug)]
pub enum Entry {
    User(String),
    Answer(String),
    Thinking(String),
    Tool(String),
    /// run_code's output as the model got it.
    Result(String),
    Run {
        code: String,
        verdict: Verdict,
    },
    Hint {
        count: String,
        text: String,
    },
    Note(String),
    Error(String),
}

/// `text` word-wrapped to `width`, with a first-line and a continuation
/// prefix (same width), each piece in `style`.
pub fn wrap(
    text: &str,
    width: usize,
    first: &str,
    cont: &str,
    style: Style,
    prefix_style: Style,
) -> Vec<Line<'static>> {
    let pw = first.width();
    let avail = width.saturating_sub(pw).max(8);
    let mut out = Vec::new();
    for (n, para) in text.split('\n').enumerate() {
        let mut rows: Vec<String> = vec![String::new()];
        let mut w = 0;
        for word in split_keep_spaces(para) {
            let ww = word.width();
            if w + ww > avail && w > 0 {
                if word.trim().is_empty() {
                    continue; // a space at the break
                }
                let last = rows.last_mut().unwrap();
                last.truncate(last.trim_end().len());
                rows.push(String::new());
                w = 0;
            }
            if ww > avail {
                // a word longer than the line: break it anywhere
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if w + cw > avail {
                        rows.push(String::new());
                        w = 0;
                    }
                    rows.last_mut().unwrap().push(c);
                    w += cw;
                }
                continue;
            }
            rows.last_mut().unwrap().push_str(word);
            w += ww;
        }
        for (i, r) in rows.into_iter().enumerate() {
            let p = if n == 0 && i == 0 { first } else { cont };
            out.push(Line::from(vec![
                Span::styled(p.to_string(), prefix_style),
                Span::styled(r, style),
            ]));
        }
    }
    out
}

fn split_keep_spaces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (i, c) in s.char_indices() {
        let sp = c == ' ';
        if in_space.is_some_and(|x| x != sp) {
            out.push(&s[start..i]);
            start = i;
        }
        in_space = Some(sp);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// An answer: text wrapped, fenced code blocks set off with a gutter and
/// not wrapped by words (code keeps its shape, long lines break anywhere).
fn answer_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut block = 0;
    let mut para = String::new();
    let flush = |para: &mut String, out: &mut Vec<Line<'static>>| {
        if !para.is_empty() {
            for l in para.trim_end_matches('\n').split('\n') {
                out.extend(wrap_styled(&inline_markdown(l), width));
            }
            para.clear();
        }
    };
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            flush(&mut para, &mut out);
            if !in_code {
                block += 1;
                let lang = line.trim().trim_start_matches('`').trim();
                let label = if lang.is_empty() {
                    format!(" [{block}]")
                } else {
                    format!(" [{block}] {lang}")
                };
                out.push(Line::from(Span::styled(
                    label,
                    Style::default().fg(MOST_SUBTLE),
                )));
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.extend(wrap(
                line,
                width,
                " │ ",
                " │ ",
                Style::default().fg(SUBTLE),
                Style::default().fg(CODE),
            ));
        } else {
            para.push_str(line);
            para.push('\n');
        }
    }
    flush(&mut para, &mut out);
    out
}

/// One line of an answer as styled pieces: **bold**, `code`, # heading.
pub fn inline_markdown(line: &str) -> Vec<(String, Style)> {
    let base = Style::default().fg(FG);
    let trimmed = line.trim_start();
    if let Some(h) = trimmed
        .strip_prefix("### ")
        .or(trimmed.strip_prefix("## "))
        .or(trimmed.strip_prefix("# "))
    {
        return vec![(
            h.to_string(),
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )];
    }
    let mut out: Vec<(String, Style)> = Vec::new();
    let (mut bold, mut code) = (false, false);
    let mut cur = String::new();
    let style = |bold: bool, code: bool| {
        if code {
            Style::default().fg(SECONDARY)
        } else if bold {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            out.push((std::mem::take(&mut cur), style(bold, code)));
            code = !code;
            i += 1;
            continue;
        }
        if !code && chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            out.push((std::mem::take(&mut cur), style(bold, code)));
            bold = !bold;
            i += 2;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    out.push((cur, style(bold, code)));
    out.retain(|(t, _)| !t.is_empty());
    out
}

/// Styled pieces word-wrapped to `width`, keeping each piece's style.
pub fn wrap_styled(pieces: &[(String, Style)], width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut w = 0;
    for (text, st) in pieces {
        for word in split_keep_spaces(text) {
            let ww = word.width();
            if w + ww > width && w > 0 {
                if word.trim().is_empty() {
                    continue;
                }
                if let Some(last) = rows.last_mut().unwrap().last_mut() {
                    let t = last.content.trim_end().to_string();
                    last.content = t.into();
                }
                rows.push(Vec::new());
                w = 0;
            }
            match rows.last_mut().unwrap().last_mut() {
                Some(sp) if sp.style == *st => sp.content.to_mut().push_str(word),
                _ => rows
                    .last_mut()
                    .unwrap()
                    .push(Span::styled(word.to_string(), *st)),
            }
            w += ww;
        }
    }
    rows.into_iter().map(Line::from).collect()
}

impl Entry {
    pub fn lines(&self, width: usize, show_thinking: bool) -> Vec<Line<'static>> {
        let dim = Style::default().fg(MOST_SUBTLE);
        match self {
            Entry::User(t) => {
                let mut v = vec![Line::raw("")];
                v.extend(wrap(
                    t,
                    width,
                    "» ",
                    "  ",
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                    Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
                ));
                v
            }
            Entry::Answer(t) => answer_lines(t, width),
            Entry::Thinking(t) if show_thinking => {
                wrap(t, width, "", "", dim.add_modifier(Modifier::ITALIC), dim)
            }
            Entry::Thinking(_) => vec![Line::styled(
                "(thinking...)",
                dim.add_modifier(Modifier::ITALIC),
            )],
            Entry::Tool(t) => vec![Line::styled(t.clone(), dim)],
            Entry::Result(text) => {
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                let shown: Vec<&str> = if lines.first() == Some(&"status: error") && lines.len() > 8
                {
                    // a traceback's point is its end
                    let mut v = vec![lines[0], "..."];
                    v.extend(&lines[lines.len() - 6..]);
                    v
                } else {
                    let mut v: Vec<&str> = lines.iter().take(14).copied().collect();
                    if lines.len() > 14 {
                        v.push("...");
                    }
                    v
                };
                shown
                    .iter()
                    .flat_map(|l| wrap(l, width, "  → ", "    ", dim, dim))
                    .collect()
            }
            Entry::Run { code, verdict } => {
                let mut v = vec![Line::from(Span::styled(
                    "mathbuddy wants to run this in your session:",
                    Style::default().fg(FG).add_modifier(Modifier::BOLD),
                ))];
                for l in code.lines() {
                    v.extend(wrap(
                        l,
                        width,
                        "  │ ",
                        "  │ ",
                        Style::default().fg(SUBTLE),
                        Style::default().fg(SECONDARY),
                    ));
                }
                let (txt, col) = match verdict {
                    Verdict::Waiting => ("(waiting for your answer)", SECONDARY),
                    Verdict::Approved => ("(approved)", GOOD),
                    Verdict::Always => ("(approved: always for this session)", GOOD),
                    Verdict::Declined => ("(declined)", BAD),
                };
                v.push(Line::styled(txt, Style::default().fg(col)));
                v
            }
            Entry::Hint { count, text } => wrap(
                text,
                width,
                &format!("hint [{count}]: "),
                &" ".repeat(format!("hint [{count}]: ").width()),
                Style::default().fg(FG),
                Style::default().fg(SECONDARY),
            ),
            Entry::Note(t) => wrap(
                t,
                width,
                "",
                "",
                Style::default().fg(SUBTLE),
                Style::default(),
            ),
            Entry::Error(t) => wrap(t, width, "", "", Style::default().fg(BAD), Style::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wraps_words_and_long_words() {
        let l = wrap(
            "the mean of the values is 2.5",
            12,
            "",
            "",
            Style::default(),
            Style::default(),
        );
        assert_eq!(plain(&l), vec!["the mean of", "the values", "is 2.5"]);
        let l = wrap(
            "abcdefghijklmnopqrst",
            8,
            "",
            "",
            Style::default(),
            Style::default(),
        );
        assert_eq!(plain(&l), vec!["abcdefgh", "ijklmnop", "qrst"]);
    }

    #[test]
    fn inline_bold_and_code() {
        let p = inline_markdown("The mean of `xs` is **2.5**.");
        let texts: Vec<&str> = p.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, vec!["The mean of ", "xs", " is ", "2.5", "."]);
        assert_eq!(p[1].1.fg, Some(SECONDARY));
        assert!(p[3].1.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn code_blocks_numbered_and_guttered() {
        let lines = answer_lines("Try:\n```python\nx = 1\n```\ndone", 40);
        let p = plain(&lines);
        assert_eq!(p, vec!["Try:", " [1] python", " │ x = 1", "done"]);
    }
}
