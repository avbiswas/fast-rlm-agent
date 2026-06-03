//! A small multiline text editor for the input area.
//!
//! Stores text as a `Vec<char>` (so cursor math is in characters, not bytes)
//! plus a cursor index. Supports the editing keys people expect from a real
//! terminal input: word motion/deletion, line motion, kill-to-end/start, and
//! multiline editing.

/// A char counts as part of a "word" for word-wise motion/deletion.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[derive(Default)]
pub struct Composer {
    chars: Vec<char>,
    /// Cursor position as an index into `chars` (0..=len).
    cursor: usize,
}

impl Composer {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.iter().all(|c| c.is_whitespace())
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    // ---- editing ---------------------------------------------------------

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete_forward(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete_word_back(&mut self) {
        let target = self.prev_word_boundary();
        self.chars.drain(target..self.cursor);
        self.cursor = target;
    }

    pub fn delete_word_forward(&mut self) {
        let target = self.next_word_boundary();
        self.chars.drain(self.cursor..target);
    }

    /// Ctrl+K: delete from cursor to end of the current line.
    pub fn kill_to_line_end(&mut self) {
        let (_, end) = self.current_line_range();
        self.chars.drain(self.cursor..end);
    }

    /// Ctrl+U: delete from start of the current line to the cursor.
    pub fn kill_to_line_start(&mut self) {
        let (start, _) = self.current_line_range();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    // ---- motion ----------------------------------------------------------

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn word_left(&mut self) {
        self.cursor = self.prev_word_boundary();
    }

    pub fn word_right(&mut self) {
        self.cursor = self.next_word_boundary();
    }

    pub fn line_start(&mut self) {
        self.cursor = self.current_line_range().0;
    }

    pub fn line_end(&mut self) {
        self.cursor = self.current_line_range().1;
    }

    pub fn up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            return;
        }
        self.move_to(row - 1, col);
    }

    pub fn down(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row + 1 >= self.line_ranges().len() {
            return;
        }
        self.move_to(row + 1, col);
    }

    // ---- view helpers ----------------------------------------------------

    /// (row, column) of the cursor, both 0-based, in character cells.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let ranges = self.line_ranges();
        for (row, &(start, end)) in ranges.iter().enumerate() {
            if self.cursor <= end {
                return (row, self.cursor - start);
            }
        }
        let last = ranges.len().saturating_sub(1);
        (last, 0)
    }

    pub fn line_count(&self) -> usize {
        self.line_ranges().len()
    }

    // ---- internals -------------------------------------------------------

    /// Char ranges `(start, end)` for each logical line (end excludes '\n').
    fn line_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut start = 0;
        for (i, &c) in self.chars.iter().enumerate() {
            if c == '\n' {
                ranges.push((start, i));
                start = i + 1;
            }
        }
        ranges.push((start, self.chars.len()));
        ranges
    }

    fn current_line_range(&self) -> (usize, usize) {
        let ranges = self.line_ranges();
        for &(start, end) in &ranges {
            if self.cursor <= end {
                return (start, end);
            }
        }
        *ranges.last().unwrap()
    }

    fn move_to(&mut self, row: usize, col: usize) {
        let ranges = self.line_ranges();
        if let Some(&(start, end)) = ranges.get(row) {
            self.cursor = (start + col).min(end);
        }
    }

    fn prev_word_boundary(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && !is_word(self.chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(self.chars[i - 1]) {
            i -= 1;
        }
        i
    }

    fn next_word_boundary(&self) -> usize {
        let len = self.chars.len();
        let mut i = self.cursor;
        while i < len && !is_word(self.chars[i]) {
            i += 1;
        }
        while i < len && is_word(self.chars[i]) {
            i += 1;
        }
        i
    }
}
