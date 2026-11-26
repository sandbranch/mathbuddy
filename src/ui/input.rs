//! The input box: a small multi-line editor with history.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Input {
    pub text: Vec<char>,
    pub cursor: usize,
    history: Vec<String>,
    /// Position while walking the history (None: editing a new line).
    walk: Option<usize>,
    draft: Vec<char>,
}

impl Input {
    pub fn with_history(history: Vec<String>) -> Self {
        Input {
            history,
            ..Default::default()
        }
    }

    pub fn value(&self) -> String {
        self.text.iter().collect()
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.chars().collect();
        self.cursor = self.text.len();
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += 1;
        self.walk = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| *c != '\r') {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.len());
    }

    pub fn home(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub fn end(&mut self) {
        while self.cursor < self.text.len() && self.text[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    /// Delete the word before the cursor (Ctrl-W).
    pub fn delete_word(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
        while self.cursor > 0 && !self.text[self.cursor - 1].is_whitespace() {
            self.backspace();
        }
    }

    /// Delete to the start of the line (Ctrl-U).
    pub fn kill_line(&mut self) {
        while self.cursor > 0 && self.text[self.cursor - 1] != '\n' {
            self.backspace();
        }
    }

    fn line_of_cursor(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.iter().filter(|c| **c == '\n').count();
        let col = before.iter().rev().take_while(|c| **c != '\n').count();
        (line, col)
    }

    /// Up: the line above, or the previous history entry on the first line.
    pub fn up(&mut self) {
        let (line, col) = self.line_of_cursor();
        if line > 0 {
            self.move_to_line(line - 1, col);
            return;
        }
        if self.history.is_empty() {
            return;
        }
        let i = match self.walk {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.walk = Some(i);
        let h = self.history[i].clone();
        self.set(&h);
    }

    /// Down: the line below, or the next history entry on the last line.
    pub fn down(&mut self) {
        let (line, col) = self.line_of_cursor();
        let lines = self.text.iter().filter(|c| **c == '\n').count();
        if line < lines {
            self.move_to_line(line + 1, col);
            return;
        }
        match self.walk {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.walk = Some(i + 1);
                let h = self.history[i + 1].clone();
                self.set(&h);
            }
            Some(_) => {
                self.walk = None;
                self.text = std::mem::take(&mut self.draft);
                self.cursor = self.text.len();
            }
        }
    }

    fn move_to_line(&mut self, target: usize, col: usize) {
        let mut line = 0;
        let mut i = 0;
        while i < self.text.len() && line < target {
            if self.text[i] == '\n' {
                line += 1;
            }
            i += 1;
        }
        let mut c = 0;
        while i < self.text.len() && self.text[i] != '\n' && c < col {
            i += 1;
            c += 1;
        }
        self.cursor = i;
    }

    /// Take the text to send; remember it in the history.
    pub fn take(&mut self) -> String {
        let s = self.value();
        if !s.trim().is_empty() && self.history.last() != Some(&s) {
            self.history.push(s.clone());
        }
        self.text.clear();
        self.cursor = 0;
        self.walk = None;
        s
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// The text wrapped to `width` columns after a prompt of `prompt_w`
    /// columns, and where the cursor lands (row, column).
    pub fn layout(&self, width: usize, prompt_w: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(prompt_w + 2);
        let mut rows = vec![String::new()];
        let mut col = prompt_w;
        let mut cur = (0, prompt_w);
        for (i, &c) in self.text.iter().enumerate() {
            if i == self.cursor {
                cur = (rows.len() - 1, col);
            }
            if c == '\n' {
                rows.push(String::new());
                col = prompt_w;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                rows.push(String::new());
                col = prompt_w;
            }
            rows.last_mut().unwrap().push(c);
            col += w;
        }
        if self.cursor == self.text.len() {
            if col >= width {
                rows.push(String::new());
                col = prompt_w;
            }
            cur = (rows.len() - 1, col);
        }
        (rows, cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_history() {
        let mut i = Input::default();
        i.insert_str("mean of x");
        i.home();
        i.insert_str("the ");
        assert_eq!(i.value(), "the mean of x");
        assert_eq!(i.take(), "the mean of x");
        i.insert_str("draft");
        i.up();
        assert_eq!(i.value(), "the mean of x");
        i.down();
        assert_eq!(i.value(), "draft");
        i.delete_word();
        assert_eq!(i.value(), "");
    }

    #[test]
    fn multi_line_moves_and_layout() {
        let mut i = Input::default();
        i.insert_str("ab\ncdef");
        i.up();
        assert_eq!(i.cursor, 2); // end of "ab"
        i.down();
        assert_eq!(i.cursor, 5);
        let (rows, cur) = i.layout(10, 2);
        assert_eq!(rows, vec!["ab".to_string(), "cdef".to_string()]);
        assert_eq!(cur, (1, 4)); // before the e of cdef
        let mut long = Input::default();
        long.insert_str("abcdefghij");
        let (rows, _) = long.layout(6, 2);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }
}
