//! Single/multi-line input editor. Non-swappable core widget.
//!
//! Deliberately small for M3: char-level editing, cursor movement,
//! history is deferred. Newline via Shift+Enter (kitty protocol) or
//! Alt+Enter; Enter submits.

use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct Editor {
    /// Buffer as lines (at least one, possibly empty).
    lines: Vec<String>,
    /// Cursor: (line index, byte offset within line).
    cursor: (usize, usize),
}

impl Editor {
    pub fn new() -> Self {
        Self { lines: vec![String::new()], cursor: (0, 0) }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn before_cursor(&self) -> String {
        let (row, col) = self.cursor;
        let mut parts = self.lines[..row].to_vec();
        parts.push(self.lines[row][..col].to_owned());
        parts.join("\n")
    }

    pub fn replace_before_cursor(&mut self, bytes: usize, replacement: &str) {
        let chars = self.before_cursor()[self.before_cursor().len() - bytes..].chars().count();
        for _ in 0..chars { self.backspace(); }
        self.insert_str(replacement);
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Take the buffer, resetting the editor.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        text
    }

    pub fn insert_char(&mut self, c: char) {
        let (row, col) = self.cursor;
        self.lines[row].insert(col, c);
        self.cursor.1 += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.insert_newline();
            } else {
                self.insert_char(c);
            }
        }
    }

    pub fn insert_newline(&mut self) {
        let (row, col) = self.cursor;
        let rest = self.lines[row].split_off(col);
        self.lines.insert(row + 1, rest);
        self.cursor = (row + 1, 0);
    }

    pub fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            let prev = prev_char_boundary(&self.lines[row], col);
            self.lines[row].remove(prev);
            self.cursor.1 = prev;
        } else if row > 0 {
            let removed = self.lines.remove(row);
            let prev_len = self.lines[row - 1].len();
            self.lines[row - 1].push_str(&removed);
            self.cursor = (row - 1, prev_len);
        }
    }

    pub fn move_left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.cursor.1 = prev_char_boundary(&self.lines[row], col);
        } else if row > 0 {
            self.cursor = (row - 1, self.lines[row - 1].len());
        }
    }

    pub fn move_right(&mut self) {
        let (row, col) = self.cursor;
        let line = &self.lines[row];
        if col < line.len() {
            let c = line[col..].chars().next().expect("cursor on boundary");
            self.cursor.1 = col + c.len_utf8();
        } else if row + 1 < self.lines.len() {
            self.cursor = (row + 1, 0);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor.1 = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor.1 = self.lines[self.cursor.0].len();
    }

    pub fn move_up(&mut self) {
        if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
            self.snap_to_boundary();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor.0 + 1 < self.lines.len() {
            self.cursor.0 += 1;
            self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
            self.snap_to_boundary();
        }
    }

    fn snap_to_boundary(&mut self) {
        let line = &self.lines[self.cursor.0];
        while self.cursor.1 > 0 && !line.is_char_boundary(self.cursor.1) {
            self.cursor.1 -= 1;
        }
    }

    /// Lines for display.
    pub fn display_lines(&self) -> &[String] {
        &self.lines
    }

    /// Cursor position for display: (row, column cells).
    pub fn cursor_cells(&self) -> (usize, usize) {
        let (row, col) = self.cursor;
        let cells = self.lines[row][..col].chars().map(|c| c.width().unwrap_or(0)).sum();
        (row, cells)
    }

    /// Soft-wrap every logical line to `width` cells (char-level, the
    /// dsh composer contract translated to cells). Every logical line
    /// yields at least one row, so an empty buffer still renders one
    /// empty row. Byte ranges index into the logical line.
    pub fn wrapped_rows(&self, width: usize) -> Vec<WrapRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for (li, line) in self.lines.iter().enumerate() {
            let mut start = 0;
            let mut cells = 0;
            for (bi, c) in line.char_indices() {
                let w = c.width().unwrap_or(0);
                if cells + w > width {
                    rows.push(WrapRow { line: li, start, end: bi });
                    start = bi;
                    cells = 0;
                }
                cells += w;
            }
            rows.push(WrapRow { line: li, start, end: line.len() });
        }
        rows
    }

    /// Cursor in wrapped-row space: (row index into `wrapped_rows`,
    /// column cells within that row).
    pub fn cursor_wrapped(&self, width: usize) -> (usize, usize) {
        let (crow, ccol) = self.cursor;
        let rows = self.wrapped_rows(width);
        for (ri, r) in rows.iter().enumerate() {
            if r.line != crow {
                continue;
            }
            // Cursor sits in this row when its byte offset is inside the
            // range; end-of-line lands on the line's LAST row.
            let last_of_line = ri + 1 == rows.len() || rows[ri + 1].line != crow;
            if ccol < r.end || (last_of_line && ccol == r.end) {
                let cells = self.lines[crow][r.start..ccol]
                    .chars()
                    .map(|c| c.width().unwrap_or(0))
                    .sum();
                return (ri, cells);
            }
        }
        (0, 0) // unreachable: every line has rows
    }
}

/// One soft-wrapped display row: byte range `start..end` of logical
/// line `line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

fn prev_char_boundary(s: &str, from: usize) -> usize {
    let mut i = from - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_take() {
        let mut e = Editor::new();
        e.insert_str("hola");
        assert_eq!(e.text(), "hola");
        assert_eq!(e.take(), "hola");
        assert!(e.is_empty());
    }

    #[test]
    fn multiline_editing() {
        let mut e = Editor::new();
        e.insert_str("ab\ncd");
        assert_eq!(e.line_count(), 2);
        e.backspace(); // "ab\nc"
        e.backspace(); // "ab\n"
        e.backspace(); // "ab"
        assert_eq!(e.text(), "ab");
        assert_eq!(e.line_count(), 1);
    }

    #[test]
    fn unicode_boundaries() {
        let mut e = Editor::new();
        e.insert_str("añó");
        e.backspace();
        assert_eq!(e.text(), "añ");
        e.move_left();
        e.move_right();
        assert_eq!(e.text(), "añ");
        let (_, cells) = e.cursor_cells();
        assert_eq!(cells, 2);
    }

    #[test]
    fn cursor_navigation_between_lines() {
        let mut e = Editor::new();
        e.insert_str("first\nsecond");
        e.move_home();
        e.move_left(); // wraps to end of first line
        let (row, _) = e.cursor_cells();
        assert_eq!(row, 0);
        e.move_down();
        let (row, _) = e.cursor_cells();
        assert_eq!(row, 1);
    }

    #[test]
    fn soft_wrap_splits_long_lines() {
        let mut e = Editor::new();
        e.insert_str("abcdefghij"); // 10 chars, width 4 -> 3 rows
        let rows = e.wrapped_rows(4);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].start, rows[0].end), (0, 4));
        assert_eq!((rows[1].start, rows[1].end), (4, 8));
        assert_eq!((rows[2].start, rows[2].end), (8, 10));
        // Cursor at end of text lands on the last row, col 2.
        assert_eq!(e.cursor_wrapped(4), (2, 2));
    }

    #[test]
    fn soft_wrap_empty_and_multiline() {
        let e = Editor::new();
        assert_eq!(e.wrapped_rows(10).len(), 1); // empty buffer = one row
        let mut e = Editor::new();
        e.insert_str("abcdef\nxy");
        let rows = e.wrapped_rows(4); // "abcd","ef" + "xy"
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].line, 1);
        assert_eq!(e.cursor_wrapped(4), (2, 2));
    }

    #[test]
    fn soft_wrap_wide_chars_never_split() {
        let mut e = Editor::new();
        e.insert_str("ａｂｃ"); // 2 cells each, width 5 -> "ａｂ" + "ｃ"
        let rows = e.wrapped_rows(5);
        assert_eq!(rows.len(), 2);
        let line = &e.display_lines()[0];
        assert_eq!(&line[rows[0].start..rows[0].end], "ａｂ");
        assert_eq!(&line[rows[1].start..rows[1].end], "ｃ");
    }

    #[test]
    fn cursor_wrapped_at_exact_boundary() {
        let mut e = Editor::new();
        e.insert_str("abcd"); // width 4: cursor at byte 4 = end
        // End-of-line at an exact wrap boundary stays on the line's
        // last row (col == width), not a phantom next row.
        assert_eq!(e.cursor_wrapped(4), (0, 4));
        e.insert_char('e'); // now 2 rows, cursor after 'e'
        assert_eq!(e.cursor_wrapped(4), (1, 1));
    }
}
