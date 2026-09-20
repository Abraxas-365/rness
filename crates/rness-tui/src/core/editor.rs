//! Single/multi-line input editor. Non-swappable core widget.
//!
//! Deliberately small for M3: char-level editing, cursor movement,
//! history is deferred. Newline via Shift+Enter (kitty protocol) or
//! Alt+Enter; Enter submits.

use unicode_width::UnicodeWidthChar;

#[derive(Clone)]
pub struct Editor {
    pastes: Vec<Paste>,
    next_paste: usize,
    /// Image placeholder blocks: same atomic-delete semantics as pastes,
    /// but `text()` strips them instead of expanding.
    images: Vec<ImageBlock>,
    next_image: usize,
    /// Buffer as lines (at least one, possibly empty).
    lines: Vec<String>,
    /// Cursor: (line index, byte offset within line).
    cursor: (usize, usize),
}

/// An image placeholder in the editor text. Behaves like a paste block
/// (atomic backspace, cursor snapping) but is stripped from `text()`.
#[derive(Clone, Debug)]
struct ImageBlock {
    /// Auto-incrementing display number.
    num: usize,
    /// External image ID (e.g. content-addressed hash).
    id: String,
    /// Byte range in the flat buffer.
    start: usize,
    end: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct Paste {
    id: usize,
    start: usize,
    end: usize,
    content: String,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            pastes: Vec::new(),
            next_paste: 1,
            images: Vec::new(),
            next_image: 1,
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }

    pub fn paste(&mut self, text: &str, lines: usize, chars: usize) {
        if text.split('\n').count() <= lines && text.chars().count() <= chars {
            self.insert_str(text);
            return;
        }
        let id = self.next_paste;
        self.next_paste += 1;
        let label = format!(
            "[Paste #{id} · {} lines · {} bytes]",
            text.split('\n').count(),
            text.len()
        );
        let start = self.before_cursor().len();
        self.insert_str(&label);
        self.pastes.push(Paste {
            id,
            start,
            end: start + label.len(),
            content: text.into(),
        });
        self.pastes.sort_by_key(|p| p.start);
    }

    pub fn has_pastes(&self) -> bool {
        !self.pastes.is_empty()
    }

    pub fn selected_paste(&self) -> Option<(usize, &str)> {
        let pos = self.before_cursor().len();
        self.pastes
            .iter()
            .find(|p| pos >= p.start && pos <= p.end)
            .map(|p| (p.id, p.content.as_str()))
    }

    pub fn replace_paste(&mut self, id: usize, text: Option<&str>) {
        let Some(index) = self.pastes.iter().position(|p| p.id == id) else {
            return;
        };
        let paste = self.pastes.remove(index);
        let raw = self.lines.join("\n");
        let replacement = text
            .map(|s| {
                format!(
                    "[Paste #{id} · {} lines · {} bytes]",
                    s.split('\n').count(),
                    s.len()
                )
            })
            .unwrap_or_default();
        let updated = format!(
            "{}{}{}",
            &raw[..paste.start],
            replacement,
            &raw[paste.end..]
        );
        for p in &mut self.pastes {
            if p.start >= paste.end {
                p.start = p.start - (paste.end - paste.start) + replacement.len();
                p.end = p.end - (paste.end - paste.start) + replacement.len();
            }
        }
        for i in &mut self.images {
            if i.start >= paste.end {
                i.start = i.start - (paste.end - paste.start) + replacement.len();
                i.end = i.end - (paste.end - paste.start) + replacement.len();
            }
        }
        self.lines = updated.split('\n').map(str::to_owned).collect();
        self.set_offset(paste.start + replacement.len());
        if let Some(content) = text {
            self.pastes.push(Paste {
                id,
                start: paste.start,
                end: paste.start + replacement.len(),
                content: content.into(),
            });
            self.pastes.sort_by_key(|p| p.start);
        }
    }

    /// Insert an image placeholder at the cursor. The placeholder
    /// behaves like an atomic block: cursor snaps around it, and
    /// backspace removes it entirely. `text()` strips image
    /// placeholders so they don't leak into submitted content.
    /// Returns the display number assigned to this image.
    pub fn insert_image(&mut self, id: &str) -> usize {
        let num = self.next_image;
        self.next_image += 1;
        let label = format!("[Image {num}]");
        let start = self.before_cursor().len();
        self.insert_str(&label);
        self.images.push(ImageBlock {
            num,
            id: id.into(),
            start,
            end: start + label.len(),
        });
        self.images.sort_by_key(|i| i.start);
        num
    }

    /// Remove an image block by index, adjusting offsets of other blocks.
    fn remove_image_block(&mut self, index: usize) {
        let img = self.images.remove(index);
        let raw = self.lines.join("\n");
        let updated = format!("{}{}", &raw[..img.start], &raw[img.end..]);
        let delta = -((img.end - img.start) as isize);
        for p in &mut self.pastes {
            if p.start >= img.end {
                p.start = p.start.saturating_add_signed(delta);
                p.end = p.end.saturating_add_signed(delta);
            }
        }
        for i in &mut self.images {
            if i.start >= img.end {
                i.start = i.start.saturating_add_signed(delta);
                i.end = i.end.saturating_add_signed(delta);
            }
        }
        self.lines = updated.split('\n').map(str::to_owned).collect();
        self.set_offset(img.start);
    }

    /// Returns the external IDs of images still present in the editor.
    pub fn image_ids(&self) -> Vec<&str> {
        self.images.iter().map(|i| i.id.as_str()).collect()
    }

    /// Remove all image blocks whose external ID is not in `keep`.
    pub fn clear_images(&mut self) {
        // Remove from last to first to keep offsets valid.
        let mut to_remove: Vec<usize> = (0..self.images.len()).collect();
        to_remove.reverse();
        for idx in to_remove {
            self.remove_image_block(idx);
        }
    }

    /// Whether the editor contains any image placeholders.
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// Remove the first image block with the given external ID.
    pub fn remove_image_by_id(&mut self, id: &str) {
        if let Some(idx) = self.images.iter().position(|i| i.id == id) {
            self.remove_image_block(idx);
        }
    }

    fn set_offset(&mut self, mut offset: usize) {
        for (row, line) in self.lines.iter().enumerate() {
            if offset <= line.len() {
                self.cursor = (row, offset);
                return;
            }
            offset -= line.len() + 1;
        }
    }

    fn snap_paste(&mut self, forward: bool) {
        let pos = self.before_cursor().len();
        let block = self
            .pastes
            .iter()
            .map(|p| (p.start, p.end))
            .chain(self.images.iter().map(|i| (i.start, i.end)))
            .find(|&(s, e)| pos > s && pos < e);
        if let Some((start, end)) = block {
            self.set_offset(if forward { end } else { start });
        }
    }

    fn shift_pastes(&mut self, at: usize, bytes: isize) {
        for p in &mut self.pastes {
            if p.start >= at {
                p.start = p.start.saturating_add_signed(bytes);
                p.end = p.end.saturating_add_signed(bytes);
            }
        }
        for i in &mut self.images {
            if i.start >= at {
                i.start = i.start.saturating_add_signed(bytes);
                i.end = i.end.saturating_add_signed(bytes);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        // Collect all blocks to process: pastes expand, images strip.
        struct Block {
            start: usize,
            end: usize,
            replacement: String,
        }
        let mut blocks: Vec<Block> = Vec::new();
        for p in &self.pastes {
            blocks.push(Block {
                start: p.start,
                end: p.end,
                replacement: p.content.clone(),
            });
        }
        for i in &self.images {
            blocks.push(Block {
                start: i.start,
                end: i.end,
                replacement: String::new(),
            });
        }
        blocks.sort_by(|a, b| b.start.cmp(&a.start));
        let mut text = self.lines.join("\n");
        for block in blocks {
            text.replace_range(block.start..block.end, &block.replacement);
        }
        text
    }

    pub fn before_cursor(&self) -> String {
        let (row, col) = self.cursor;
        let mut parts = self.lines[..row].to_vec();
        parts.push(self.lines[row][..col].to_owned());
        parts.join("\n")
    }

    pub fn replace_before_cursor(&mut self, bytes: usize, replacement: &str) {
        let chars = self.before_cursor()[self.before_cursor().len() - bytes..]
            .chars()
            .count();
        for _ in 0..chars {
            self.backspace();
        }
        self.insert_str(replacement);
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Take the buffer, resetting the editor.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.lines = vec![String::new()];
        self.pastes.clear();
        self.images.clear();
        self.cursor = (0, 0);
        text
    }

    pub fn insert_char(&mut self, c: char) {
        self.snap_paste(true);
        self.shift_pastes(self.before_cursor().len(), c.len_utf8() as isize);
        let (row, col) = self.cursor;
        self.lines[row].insert(col, c);
        self.cursor.1 += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.snap_paste(true);
        let pos = self.before_cursor().len();
        self.shift_pastes(pos, s.len() as isize);
        let (row, col) = self.cursor;
        let suffix = self.lines[row].split_off(col);
        let mut parts = s.split('\n');
        self.lines[row].push_str(parts.next().unwrap_or_default());
        let remaining: Vec<String> = parts.map(str::to_owned).collect();
        let count = remaining.len();
        self.lines.splice(row + 1..row + 1, remaining);
        self.cursor = (row + count, self.lines[row + count].len());
        self.lines[row + count].push_str(&suffix);
    }

    pub fn insert_newline(&mut self) {
        self.snap_paste(true);
        self.shift_pastes(self.before_cursor().len(), 1);
        let (row, col) = self.cursor;
        let rest = self.lines[row].split_off(col);
        self.lines.insert(row + 1, rest);
        self.cursor = (row + 1, 0);
    }

    pub fn backspace(&mut self) {
        let pos = self.before_cursor().len();
        if let Some(id) = self
            .pastes
            .iter()
            .find(|p| pos > p.start && pos <= p.end)
            .map(|p| p.id)
        {
            self.replace_paste(id, None);
            return;
        }
        if let Some(idx) = self
            .images
            .iter()
            .position(|i| pos > i.start && pos <= i.end)
        {
            self.remove_image_block(idx);
            return;
        }
        let (row, col) = self.cursor;
        if col > 0 {
            let prev = prev_char_boundary(&self.lines[row], col);
            self.shift_pastes(pos, -((col - prev) as isize));
            self.lines[row].remove(prev);
            self.cursor.1 = prev;
        } else if row > 0 {
            self.shift_pastes(pos, -1);
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
        self.snap_paste(false);
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
        self.snap_paste(true);
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
            self.snap_paste(false);
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor.0 + 1 < self.lines.len() {
            self.cursor.0 += 1;
            self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
            self.snap_to_boundary();
            self.snap_paste(false);
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
        let cells = self.lines[row][..col]
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum();
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
                    rows.push(WrapRow {
                        line: li,
                        start,
                        end: bi,
                    });
                    start = bi;
                    cells = 0;
                }
                cells += w;
            }
            rows.push(WrapRow {
                line: li,
                start,
                end: line.len(),
            });
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
mod paste_tests {
    use super::*;

    #[test]
    fn blocks_preserve_bytes_and_shift_with_edits() {
        let mut e = Editor::new();
        e.insert_str("before ");
        let content = "\tfn 日本語() {\r\n  x();\n}\n";
        e.paste(content, 1, 5);
        assert_eq!(e.selected_paste().unwrap().1, content);
        e.insert_str(" after");
        assert_eq!(e.text(), format!("before {content} after"));
        e.move_home();
        e.insert_str("prefix ");
        e.move_end();
        for _ in 0..6 {
            e.move_left();
        }
        let id = e.selected_paste().unwrap().0;
        e.replace_paste(id, Some("edited\n"));
        assert_eq!(e.text(), "prefix before edited\n after");
        e.backspace();
        assert_eq!(e.take(), "prefix before  after");
        assert!(e.selected_paste().is_none());
    }

    #[test]
    fn multiple_blocks_are_not_placeholder_substitutions() {
        let mut e = Editor::new();
        e.paste("first\n", 0, 0);
        e.insert_str("[Paste #1] between ");
        e.paste("second\n", 0, 0);
        assert_eq!(e.text(), "first\n[Paste #1] between second\n");
        e.move_left();
        e.insert_str("inserted ");
        assert_eq!(e.text(), "first\n[Paste #1] between inserted second\n");
    }
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

#[cfg(test)]
mod image_tests {
    use super::*;

    #[test]
    fn image_placeholder_is_displayed_but_stripped_from_text() {
        let mut e = Editor::new();
        e.insert_str("hello ");
        e.insert_image("img_abc");
        let display = e.display_lines().join("\n");
        assert!(display.contains("[Image 1]"), "display: {display}");
        assert_eq!(e.text(), "hello ");
    }

    #[test]
    fn backspace_removes_image_block_atomically() {
        let mut e = Editor::new();
        e.insert_str("before ");
        e.insert_image("img_1");
        e.insert_str(" after");
        assert_eq!(e.text(), "before  after");
        assert_eq!(e.image_ids(), vec!["img_1"]);
        // Cursor is after " after"; move left 6 chars to reach end of image block.
        for _ in 0..6 {
            e.move_left();
        }
        // Backspace should delete the entire [Image 1] block.
        e.backspace();
        assert!(e.image_ids().is_empty());
        assert_eq!(e.text(), "before  after");
        assert_eq!(e.display_lines().join(""), "before  after");
    }

    #[test]
    fn multiple_images_track_independently() {
        let mut e = Editor::new();
        e.insert_image("img_a");
        e.insert_image("img_b");
        assert_eq!(e.image_ids().len(), 2);
        assert_eq!(e.text(), "");
        // Backspace removes img_b (last).
        e.backspace();
        assert_eq!(e.image_ids(), vec!["img_a"]);
        e.backspace();
        assert!(e.image_ids().is_empty());
    }

    #[test]
    fn remove_image_by_id_strips_placeholder() {
        let mut e = Editor::new();
        e.insert_str("start ");
        e.insert_image("x");
        e.insert_str(" end");
        assert!(e.display_lines().join("").contains("[Image 1]"));
        e.remove_image_by_id("x");
        assert!(e.image_ids().is_empty());
        assert!(!e.display_lines().join("").contains("[Image"));
        assert_eq!(e.text(), "start  end");
    }

    #[test]
    fn images_and_pastes_coexist() {
        let mut e = Editor::new();
        e.paste("long paste\ncontent\n", 0, 0);
        e.insert_image("photo");
        assert_eq!(e.image_ids(), vec!["photo"]);
        assert_eq!(e.text(), "long paste\ncontent\n");
        // Backspace removes image, not paste.
        e.backspace();
        assert!(e.image_ids().is_empty());
        assert!(e.has_pastes());
        assert_eq!(e.text(), "long paste\ncontent\n");
    }
}
