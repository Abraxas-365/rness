//! Scrollback viewport: renders a list of styled lines bottom-anchored,
//! with an offset from the bottom. Non-swappable core widget.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

/// Draw `lines` into `area`, pinned to the bottom, scrolled up by
/// `from_bottom` lines. Returns the clamped offset (so callers can sync
/// their scroll state to reality).
pub fn render_bottom_anchored(
    lines: &[Line<'_>],
    from_bottom: u16,
    area: Rect,
    buf: &mut Buffer,
) -> u16 {
    if area.height == 0 || area.width == 0 {
        return 0;
    }
    let height = area.height as usize;
    let max_offset = lines.len().saturating_sub(height);
    let offset = (from_bottom as usize).min(max_offset);

    let end = lines.len() - offset;
    let start = end.saturating_sub(height);
    let visible = &lines[start..end];

    // Bottom-anchor: when content is shorter than the area, draw from top.
    let y0 = area.y + (height - visible.len()) as u16;
    for (i, line) in visible.iter().enumerate() {
        let rect = Rect::new(area.x, y0 + i as u16, area.width, 1);
        line.render(rect, buf);
    }
    offset as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn bottom_anchored_shows_latest() {
        let lines: Vec<Line> = (1..=5).map(|i| Line::raw(format!("line{i}"))).collect();
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        let clamped = render_bottom_anchored(&lines, 0, area, &mut buf);
        assert_eq!(clamped, 0);
        assert_eq!(row(&buf, 0, 10), "line3");
        assert_eq!(row(&buf, 2, 10), "line5");
    }

    #[test]
    fn scroll_offset_moves_window_and_clamps() {
        let lines: Vec<Line> = (1..=5).map(|i| Line::raw(format!("line{i}"))).collect();
        let area = Rect::new(0, 0, 10, 3);

        let mut buf = Buffer::empty(area);
        let clamped = render_bottom_anchored(&lines, 1, area, &mut buf);
        assert_eq!(clamped, 1);
        assert_eq!(row(&buf, 2, 10), "line4");

        // Past the top clamps.
        let mut buf = Buffer::empty(area);
        let clamped = render_bottom_anchored(&lines, 99, area, &mut buf);
        assert_eq!(clamped, 2);
        assert_eq!(row(&buf, 0, 10), "line1");
    }

    #[test]
    fn short_content_renders_from_top() {
        let lines = vec![Line::raw("only")];
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        render_bottom_anchored(&lines, 0, area, &mut buf);
        assert_eq!(row(&buf, 2, 10), "only");
    }
}
